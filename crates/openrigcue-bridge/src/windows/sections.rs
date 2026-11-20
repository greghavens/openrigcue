//! Named file mappings: opening and copying the game's sections, holding the sections OpenRigCue's listeners own,
//! publishing exports.

use std::collections::HashMap;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr;

use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, FALSE, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP, FILE_MAP_READ, FILE_MAP_WRITE,
    MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READONLY, PAGE_READWRITE,
};

use super::wide;
use crate::control::SectionHolder;
use crate::export::SectionPublisher;
use crate::mirror::{Mirror, MirrorDir, SectionReader};
use crate::plan::MirrorTarget;
use crate::section_mapping::{self, ReadOnlyApi};

/// Take ownership of a handle returned by an API that reports failure as NULL.
pub(super) fn owned(handle: HANDLE) -> io::Result<OwnedHandle> {
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `handle` is a valid handle just returned to us, and nothing else owns it.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

/// `OpenFileMappingW(access, FALSE, name)`.
fn open_section(name: &str, access: FILE_MAP) -> io::Result<OwnedHandle> {
    let name = wide(name);
    // SAFETY: `name` is a NUL-terminated UTF-16 string that outlives the call.
    owned(unsafe { OpenFileMappingW(access, FALSE, name.as_ptr()) })
}

/// A section handle and whether this call created the section (rather than opening an existing one).
struct CreatedOrOpened {
    section: OwnedHandle,
    created: bool,
}

/// `CreateFileMappingW(INVALID_HANDLE_VALUE, NULL, PAGE_READWRITE, size, name)`, which opens the section when it
/// already exists.
fn create_or_open_section(name: &str, size: usize) -> io::Result<CreatedOrOpened> {
    create_with_protection(name, size, PAGE_READWRITE)
}

fn create_with_protection(name: &str, size: usize, protection: u32) -> io::Result<CreatedOrOpened> {
    let name = wide(name);
    let size = size as u64;
    // SAFETY: pagefile-backed mapping; `name` is NUL-terminated and outlives the call; no security attributes.
    let section = owned(unsafe {
        CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            ptr::null(),
            protection,
            (size >> 32) as u32,
            size as u32,
            name.as_ptr(),
        )
    })?;
    // SAFETY: reads the calling thread's last error, set by the successful call above.
    let created = unsafe { GetLastError() } != ERROR_ALREADY_EXISTS;
    Ok(CreatedOrOpened { section, created })
}

/// A mapped view of `len` bytes, unmapped on drop.
struct View {
    address: MEMORY_MAPPED_VIEW_ADDRESS,
    len: usize,
}

// SAFETY: a Win32 mapping's address is process-wide, not thread-local. Moving
// its single owning View does not unmap it; Drop still unmaps it once. The
// bridge's Shared mutex serializes access to retained views across its workers.
unsafe impl Send for View {}

impl View {
    /// `MapViewOfFile(section, access, 0, 0, len)`; fails when the section is smaller than `len`.
    fn map(section: &OwnedHandle, access: FILE_MAP, len: usize) -> io::Result<View> {
        // SAFETY: `section` is a live section handle; the returned address is checked before use.
        let address = unsafe { MapViewOfFile(section.as_raw_handle(), access, 0, 0, len) };
        if address.Value.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(View { address, len })
    }

    /// Copy the whole view into `buf`. The game may be writing concurrently;
    /// the copy is not synchronised with it.
    fn copy_into(&self, buf: &mut Vec<u8>) {
        buf.resize(self.len, 0);
        // SAFETY: the view is mapped readable for `len` bytes and `buf` holds `len` bytes; they cannot overlap.
        unsafe { ptr::copy_nonoverlapping(self.address.Value as *const u8, buf.as_mut_ptr(), self.len) };
    }

    /// Overwrite the start of a writable view with `bytes` (at most `len` bytes).
    fn copy_from(&mut self, bytes: &[u8]) {
        let n = bytes.len().min(self.len);
        // SAFETY: the view is mapped writable for `len` >= `n` bytes; `bytes` is a separate allocation.
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), self.address.Value as *mut u8, n) };
    }
}

impl Drop for View {
    fn drop(&mut self) {
        // SAFETY: `address` came from a successful MapViewOfFile and is unmapped exactly once.
        unsafe { UnmapViewOfFile(self.address) };
    }
}

/// Map the largest of `sizes` that `section` allows and copy it into `buf`.
fn copy_largest_view(section: &OwnedHandle, sizes: &[usize], buf: &mut Vec<u8>) -> bool {
    match sizes.iter().find_map(|&size| View::map(section, FILE_MAP_READ, size).ok()) {
        Some(view) => {
            view.copy_into(buf);
            true
        }
        None => false,
    }
}

/// A section the bridge holds for OpenRigCue, with the size OpenRigCue's object maps.
struct Held {
    // A kept read-only view is unmapped before the section is closed.
    read_only_view: Option<View>,
    section: OwnedHandle,
    size: usize,
}

/// The game's sections as the mirror reads them, and the sections the bridge holds for OpenRigCue (the
/// application's own mapping objects). A section nothing holds is opened, mapped, copied and closed on every read,
/// so it disappears with the game; a held one is copied through the handle kept for it.
#[derive(Default)]
pub struct LiveSections {
    held: HashMap<String, Held>,
}

impl SectionReader for LiveSections {
    fn read(&mut self, target: &MirrorTarget, buf: &mut Vec<u8>) -> bool {
        if let Some(held) = self.held.get(&target.name) {
            if let Some(view) = &held.read_only_view {
                view.copy_into(buf);
                return true;
            }
            let mut sizes = target.sizes.clone();
            if !sizes.contains(&held.size) {
                sizes.push(held.size);
                sizes.sort_unstable_by(|a, b| b.cmp(a));
            }
            return copy_largest_view(&held.section, &sizes, buf);
        }
        match open_section(&target.name, FILE_MAP_READ) {
            Ok(section) => copy_largest_view(&section, &target.sizes, buf),
            Err(_) => false,
        }
    }

    fn is_held(&self, name: &str) -> bool {
        self.held.contains_key(name)
    }
}

impl LiveSections {
    /// Create (when `create`) or open `name` and keep the handle. A section this call creates starts with `seed`
    /// (the frame OpenRigCue's object kept while no prefix ran), otherwise zero-filled.
    fn hold_section(&mut self, name: &str, size: usize, create: bool, seed: Option<&[u8]>) -> bool {
        let section = if create {
            match create_or_open_section(name, size) {
                Ok(CreatedOrOpened { section, created }) => {
                    if let (true, Some(seed)) = (created, seed) {
                        match View::map(&section, FILE_MAP_WRITE, size) {
                            Ok(mut view) => view.copy_from(seed),
                            Err(error) => tracing::warn!(section = name, %error, "cannot fill the created section"),
                        }
                    }
                    tracing::debug!(section = name, bytes = size, created, "holding section for OpenRigCue");
                    section
                }
                Err(error) => {
                    tracing::warn!(section = name, %error, "cannot create section");
                    return false;
                }
            }
        } else {
            match open_section(name, FILE_MAP_READ) {
                Ok(section) => {
                    tracing::debug!(section = name, "holding section for OpenRigCue");
                    section
                }
                Err(_) => return false,
            }
        };
        self.held.insert(name.to_owned(), Held { section, size, read_only_view: None });
        true
    }
}

/// What the control connection acts on: the sections and their mirrors.
pub struct Prefix<'a> {
    pub sections: &'a mut LiveSections,
    pub mirror: &'a mut Mirror,
    pub dir: &'a mut MirrorDir,
}

impl SectionHolder for Prefix<'_> {
    fn hold_read_only(&mut self, name: &str, size: usize) -> io::Result<Vec<u8>> {
        let mapping = section_mapping::open(&mut WindowsReadOnly, name, size)?;
        let mut bytes = Vec::new();
        mapping.view.copy_into(&mut bytes);
        self.sections.held.insert(
            name.to_owned(),
            Held { read_only_view: Some(mapping.view), section: mapping.section.section, size },
        );
        self.mirror.ensure(name, size);
        self.mirror.refresh(name, self.sections, self.dir);
        Ok(bytes)
    }

    fn copy_read_only(&mut self, name: &str, size: usize) -> io::Result<Vec<u8>> {
        // The temporary mapping/view remains alive during the full copy and is
        // released before the control reply. Never seed PAGE_READWRITE here.
        section_mapping::copy_once(&mut WindowsReadOnly, name, size)
    }
    fn hold(&mut self, name: &str, size: usize, create: bool, seed: Option<&[u8]>) -> bool {
        if !self.sections.hold_section(name, size, create, seed) {
            return false;
        }
        self.mirror.ensure(name, size);
        self.mirror.refresh(name, self.sections, self.dir);
        true
    }

    fn release(&mut self, name: &str) {
        if self.sections.held.remove(name).is_some() {
            tracing::debug!(section = name, "section released by OpenRigCue");
        }
        // The section is gone unless the game still holds it: the next copy finds out.
        self.mirror.refresh(name, self.sections, self.dir);
    }

    /// A per-poll snapshot: create-or-open the section, copy it, and drop the mapping right after (so
    /// a section created here goes again at once unless the game opened it meanwhile). OpenRigCue reads the zeros
    /// of a created section itself; the mirror follows the section as it is afterwards.
    fn create_once(&mut self, name: &str, size: usize) {
        self.mirror.ensure(name, size);
        match create_or_open_section(name, size) {
            Ok(CreatedOrOpened { section, .. }) => drop(section),
            Err(error) => tracing::warn!(section = name, %error, "cannot create section"),
        }
        self.mirror.refresh(name, self.sections, self.dir);
    }
}

struct WindowsReadOnly;

impl ReadOnlyApi for WindowsReadOnly {
    type Section = CreatedOrOpened;
    type View = View;

    fn create_read_only(&mut self, name: &str, size: usize) -> io::Result<CreatedOrOpened> {
        create_with_protection(name, size, PAGE_READONLY)
    }
    fn map_read(&mut self, section: &CreatedOrOpened, size: usize) -> io::Result<View> {
        View::map(&section.section, FILE_MAP_READ, size)
    }
    fn copy(&mut self, view: &View) -> Vec<u8> {
        let mut bytes = Vec::new();
        view.copy_into(&mut bytes);
        bytes
    }
}

/// Sections the bridge publishes from openrigcue's files, each held with a writable view.
#[derive(Default)]
pub struct OwnedSections {
    held: HashMap<&'static str, (View, OwnedHandle)>,
}

impl SectionPublisher for OwnedSections {
    fn publish(&mut self, name: &'static str, bytes: &[u8]) -> io::Result<()> {
        if self.held.get(name).is_some_and(|(view, _)| view.len != bytes.len()) {
            self.held.remove(name);
        }
        let (view, _) = match self.held.entry(name) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let CreatedOrOpened { section, .. } = create_or_open_section(name, bytes.len())?;
                let view = View::map(&section, FILE_MAP_READ | FILE_MAP_WRITE, bytes.len())?;
                entry.insert((view, section))
            }
        };
        view.copy_from(bytes);
        Ok(())
    }

    fn withdraw(&mut self, name: &'static str) {
        self.held.remove(name);
    }
}
