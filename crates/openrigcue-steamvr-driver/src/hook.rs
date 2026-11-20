//! Redirecting one virtual function of a C++ class by rewriting its vtable slot.
//!
//! Method (as used on Linux by OpenVR-SpaceCalibrator's driver; reimplemented here): a vtable is
//! shared by every object of a class, so replacing slot `n` of the vtable that one object points
//! at redirects that virtual call for every object of the class, in every driver in the process.
//! `vrserver` keeps vtables in `.data.rel.ro`, which is read-only after relocation (GNU_RELRO), so
//! the page holding the slot is made writable for the duration of one atomic store and its
//! original protection (read from `/proc/self/maps`) is restored immediately.
//!
//! Deliberate properties of this implementation:
//! - only the page holding the slot is unprotected, and its exact previous protection is restored;
//! - the slot is swapped with a compare-and-swap so a concurrent hooker is never overwritten;
//! - the replaced pointer is kept per vtable and the detour forwards to it, which chains with any
//!   other tool that hooked the slot first; unhooking restores the slot only if it still holds our
//!   detour, so a tool that hooked after us keeps working;
//! - the hook entry is published before the slot is switched, so the detour can always find the
//!   function to forward to.

use std::ffi::c_void;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookError {
    /// The slot address is not inside any mapping of this process.
    NotMapped(usize),
    /// `mprotect` refused (for example a sealed mapping); nothing was changed.
    Protect { address: usize, errno: i32 },
    /// The table of hooked vtables is full.
    TableFull,
    /// `/proc/self/maps` could not be read.
    Maps(String),
}

impl fmt::Display for HookError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HookError::NotMapped(a) => write!(f, "address {a:#x} is not mapped"),
            HookError::Protect { address, errno } => {
                write!(f, "mprotect of the page at {address:#x} failed (errno {errno})")
            }
            HookError::TableFull => write!(f, "too many hooked vtables"),
            HookError::Maps(e) => write!(f, "cannot read /proc/self/maps: {e}"),
        }
    }
}

impl std::error::Error for HookError {}

/// What [`HookTable::install`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Installed {
    /// The slot now points at the detour; `previous` is what it held before.
    Hooked { vtable: usize, previous: usize },
    /// This vtable was already hooked by this table.
    AlreadyHooked { vtable: usize },
}

/// What [`HookTable::uninstall_all`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Uninstalled {
    pub restored: usize,
    /// Slots that another tool re-hooked after us; left alone so its chain stays intact.
    pub left_chained: usize,
    pub failed: usize,
}

const CAPACITY: usize = 8;

struct Entry {
    vtable: AtomicUsize,
    slot_address: AtomicUsize,
    original: AtomicUsize,
    /// True while our detour is reachable through this vtable (directly, or under another tool's
    /// hook that chained onto ours); false after the slot was restored.
    in_chain: AtomicBool,
}

/// The hooks one detour function serves: one entry per distinct vtable it was installed into.
pub struct HookTable {
    slot: usize,
    entries: [Entry; CAPACITY],
    install_lock: Mutex<()>,
}

impl HookTable {
    pub const fn new(slot: usize) -> Self {
        HookTable {
            slot,
            entries: [const {
                Entry {
                    vtable: AtomicUsize::new(0),
                    slot_address: AtomicUsize::new(0),
                    original: AtomicUsize::new(0),
                    in_chain: AtomicBool::new(false),
                }
            }; CAPACITY],
            install_lock: Mutex::new(()),
        }
    }

    pub fn slot(&self) -> usize {
        self.slot
    }

    /// The function a detour must forward to for a call on `object`. Lock-free, allocation-free.
    ///
    /// # Safety
    /// `object` must be a live C++ object (its first word must be readable).
    pub unsafe fn original_for(&self, object: *mut c_void) -> Option<usize> {
        // SAFETY: the caller guarantees `object` is a live object, whose first word is its vptr.
        let vtable = unsafe { *(object as *const usize) };
        self.original_for_vtable(vtable)
    }

    pub fn original_for_vtable(&self, vtable: usize) -> Option<usize> {
        self.entries.iter().find_map(|e| {
            if e.vtable.load(Ordering::Acquire) == vtable && vtable != 0 {
                Some(e.original.load(Ordering::Acquire))
            } else {
                None
            }
        })
    }

    /// Vtables through which our detour is currently reachable.
    pub fn hooked_vtables(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.vtable.load(Ordering::Acquire) != 0 && e.in_chain.load(Ordering::Acquire))
            .count()
    }

    /// Points slot [`HookTable::slot`] of `object`'s vtable at `detour`.
    ///
    /// # Safety
    /// `object` must be a live C++ object whose vtable has more than `slot` entries, and `detour`
    /// must be an `extern "C"` function with the signature of that virtual function that forwards
    /// through [`HookTable::original_for`]. The library containing `detour` must never be unloaded
    /// while the hook may be reached (the driver pins itself with `RTLD_NODELETE`).
    pub unsafe fn install(&self, object: *mut c_void, detour: usize) -> Result<Installed, HookError> {
        let _guard = self.install_lock.lock().unwrap_or_else(|p| p.into_inner());
        // SAFETY: caller contract; reading the vptr and computing the slot address.
        let (vtable, slot_address) = unsafe {
            let vtable = *(object as *const usize);
            (vtable, crate::openvr::vtable_slot_address(object, self.slot) as usize)
        };
        let known = self.entries.iter().find(|e| e.vtable.load(Ordering::Acquire) == vtable);
        if known.is_some_and(|e| e.in_chain.load(Ordering::Acquire)) {
            // Hooking again would make our detour forward to itself (directly or via a chain).
            return Ok(Installed::AlreadyHooked { vtable });
        }
        // A vtable restored by `uninstall_all` reuses its entry; otherwise take a free one.
        let entry = match known {
            Some(e) => e,
            None => self.entries.iter().find(|e| e.vtable.load(Ordering::Acquire) == 0).ok_or(HookError::TableFull)?,
        };

        let mut previous = load_slot(slot_address);
        loop {
            // Publish where to forward before any caller can reach the detour.
            entry.slot_address.store(slot_address, Ordering::Relaxed);
            entry.original.store(previous, Ordering::Release);
            entry.vtable.store(vtable, Ordering::Release);
            // SAFETY: `slot_address` is a word-aligned vtable slot of a live class (caller contract).
            match unsafe { swap_slot(slot_address, previous, detour) } {
                Ok(()) => {
                    entry.in_chain.store(true, Ordering::Release);
                    return Ok(Installed::Hooked { vtable, previous });
                }
                Err(SwapError::Changed(now)) => previous = now,
                Err(SwapError::Hook(e)) => {
                    if known.is_none() {
                        entry.vtable.store(0, Ordering::Release);
                    }
                    return Err(e);
                }
            }
        }
    }

    /// Restores every slot that still holds `detour`. Entries stay registered, so a detour that
    /// is still reached through another tool's chain keeps forwarding correctly.
    ///
    /// # Safety
    /// The hooked vtables must still be mapped (true for the lifetime of `vrserver`).
    pub unsafe fn uninstall_all(&self, detour: usize) -> Uninstalled {
        let _guard = self.install_lock.lock().unwrap_or_else(|p| p.into_inner());
        let mut result = Uninstalled::default();
        for e in &self.entries {
            if e.vtable.load(Ordering::Acquire) == 0 || !e.in_chain.load(Ordering::Acquire) {
                continue;
            }
            let slot_address = e.slot_address.load(Ordering::Relaxed);
            let original = e.original.load(Ordering::Acquire);
            if load_slot(slot_address) != detour {
                result.left_chained += 1;
                continue;
            }
            // SAFETY: the slot was hooked by us and is still mapped (caller contract).
            match unsafe { swap_slot(slot_address, detour, original) } {
                Ok(()) => {
                    e.in_chain.store(false, Ordering::Release);
                    result.restored += 1;
                }
                Err(SwapError::Changed(_)) => result.left_chained += 1,
                Err(SwapError::Hook(_)) => result.failed += 1,
            }
        }
        result
    }
}

fn load_slot(slot_address: usize) -> usize {
    // SAFETY: only called with addresses of vtable slots obtained from live objects, which are
    // word-aligned and readable; read atomically because another hooker may be writing.
    unsafe { AtomicUsize::from_ptr(slot_address as *mut usize).load(Ordering::Acquire) }
}

enum SwapError {
    /// The slot no longer held the expected value; it now holds this.
    Changed(usize),
    Hook(HookError),
}

/// Compare-and-swap of one word in a possibly read-only page.
///
/// # Safety
/// `address` must be a word-aligned address of a word that may be rewritten (a vtable slot).
unsafe fn swap_slot(address: usize, expected: usize, new: usize) -> Result<(), SwapError> {
    let protection = mapping_protection(address).map_err(SwapError::Hook)?;
    let page_size = page_size();
    let page = address & !(page_size - 1);
    let must_unprotect = protection & libc::PROT_WRITE == 0;
    if must_unprotect {
        // SAFETY: `page` is the start of a page of an existing mapping; we only add write access.
        let rc = unsafe { libc::mprotect(page as *mut c_void, page_size, protection | libc::PROT_WRITE) };
        if rc != 0 {
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            return Err(SwapError::Hook(HookError::Protect { address: page, errno }));
        }
    }
    // SAFETY: the page is writable now; the word is aligned (caller contract).
    let swapped = unsafe { AtomicUsize::from_ptr(address as *mut usize) }.compare_exchange(
        expected,
        new,
        Ordering::AcqRel,
        Ordering::Acquire,
    );
    if must_unprotect {
        // SAFETY: restores exactly the protection the mapping had before.
        // A failure leaves the page writable, which is harmless for correctness.
        unsafe { libc::mprotect(page as *mut c_void, page_size, protection) };
    }
    swapped.map(|_| ()).map_err(SwapError::Changed)
}

fn page_size() -> usize {
    // SAFETY: sysconf has no preconditions.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if size > 0 {
        size as usize
    } else {
        4096
    }
}

/// The `PROT_*` flags of the mapping that contains `address`.
pub fn mapping_protection(address: usize) -> Result<i32, HookError> {
    let maps = std::fs::read_to_string("/proc/self/maps").map_err(|e| HookError::Maps(e.to_string()))?;
    protection_in_maps(&maps, address).ok_or(HookError::NotMapped(address))
}

/// Parses `/proc/<pid>/maps` text: `start-end perms offset dev inode [path]`.
pub fn protection_in_maps(maps: &str, address: usize) -> Option<i32> {
    maps.lines().find_map(|line| {
        let mut fields = line.split_ascii_whitespace();
        let (start, end) = fields.next()?.split_once('-')?;
        let start = usize::from_str_radix(start, 16).ok()?;
        let end = usize::from_str_radix(end, 16).ok()?;
        if !(start..end).contains(&address) {
            return None;
        }
        let perms = fields.next()?.as_bytes();
        let mut prot = libc::PROT_NONE;
        for (byte, flag) in [(b'r', libc::PROT_READ), (b'w', libc::PROT_WRITE), (b'x', libc::PROT_EXEC)] {
            if perms.contains(&byte) {
                prot |= flag;
            }
        }
        Some(prot)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_maps_lines() {
        let maps = "55ccc0000000-55ccc0001000 r--p 00000000 00:1f 123 /opt/vrserver\n\
                    55ccc0001000-55ccc0003000 rw-p 00001000 00:1f 123 /opt/vrserver\n\
                    7fff00000000-7fff00001000 r-xp 00000000 00:00 0\n";
        assert_eq!(protection_in_maps(maps, 0x55ccc0000800), Some(libc::PROT_READ));
        assert_eq!(protection_in_maps(maps, 0x55ccc0002fff), Some(libc::PROT_READ | libc::PROT_WRITE));
        assert_eq!(protection_in_maps(maps, 0x7fff00000000), Some(libc::PROT_READ | libc::PROT_EXEC));
        assert_eq!(protection_in_maps(maps, 0x55ccc0003000), None);
    }
}
