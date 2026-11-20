//! The static table of 418 `tm_string_hash` objects of the Aerofly SDK
//! (a vector with a static initialiser and an exit handler). Update never
//! reads it;
//! what it does is allocate and fill one heap block at load and release it
//! at unload.
//!
//! Entry (0x110 bytes): `u32 length` (strlen), `char text[0x100]` (zeroed,
//! then the name), 4 padding bytes, `u64 hash` at +0x108.
//! The vector is built in one go: exactly 418 elements, one
//! allocation of 418 * 0x110 bytes (an over-aligned "big" block,
//! HeapAlloc 0x1bc47).

use crate::hash::tm_string_hash;

pub const ENTRIES: usize = 418;
pub const ENTRY_SIZE: usize = 0x110;
pub const TEXT_OFFSET: usize = 4;
pub const TEXT_SIZE: usize = 0x100;
pub const HASH_OFFSET: usize = 0x108;

/// One table entry; bytes 0x104..0x108 are left 0 (see module doc).
pub const fn entry(name: &[u8]) -> [u8; ENTRY_SIZE] {
    let mut e = [0u8; ENTRY_SIZE];
    let n = name.len() as u32;
    let nb = n.to_le_bytes();
    e[0] = nb[0];
    e[1] = nb[1];
    e[2] = nb[2];
    e[3] = nb[3];
    let mut i = 0;
    while i < name.len() {
        e[TEXT_OFFSET + i] = name[i];
        i += 1;
    }
    let h = tm_string_hash(name).to_le_bytes();
    let mut j = 0;
    while j < 8 {
        e[HASH_OFFSET + j] = h[j];
        j += 1;
    }
    e
}

#[cfg(windows)]
pub use self::win::{construct, destroy};

#[cfg(windows)]
mod win {
    use super::*;
    use crate::msvc_heap::{allocate, deallocate};
    use core::ptr::null_mut;

    #[repr(C)]
    pub struct TableVector {
        first: *mut u8,
        last: *mut u8,
        end: *mut u8,
    }

    /// The table's begin / end / capacity pointers.
    pub static mut TABLE: TableVector = TableVector { first: null_mut(), last: null_mut(), end: null_mut() };

    /// The static initialiser.
    /// All 418 entries are first built as stack temporaries
    /// (a frame of about 0x1bc58 bytes, touched page by page by the stack
    /// probe), then the three pointers are zeroed, the block is allocated
    /// (begin and capacity set at once) and the temporaries are copied in
    /// one by one, the end pointer advancing by one element per copy.
    /// Kept out of line so only this initialiser carries the big frame.
    #[inline(never)]
    pub unsafe fn construct() {
        let mut temps = [[0u8; ENTRY_SIZE]; ENTRIES];
        for (t, name) in temps.iter_mut().zip(NAMES.iter()) {
            *t = entry(name.as_bytes());
        }
        let temps = core::hint::black_box(&temps);
        let bytes = ENTRIES * ENTRY_SIZE;
        let t = core::ptr::addr_of_mut!(TABLE);
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*t).end), null_mut());
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*t).first), null_mut());
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*t).last), null_mut());
        let p = allocate(bytes);
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*t).last), p);
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*t).end), p.add(bytes));
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*t).first), p);
        for e in temps.iter() {
            let last = core::ptr::read_volatile(core::ptr::addr_of!((*t).last));
            core::ptr::copy_nonoverlapping(e.as_ptr(), last, ENTRY_SIZE);
            core::ptr::write_volatile(core::ptr::addr_of_mut!((*t).last), last.add(ENTRY_SIZE));
        }
    }

    /// The exit handler: frees `((cap - begin) / 0x110) * 0x110` bytes and
    /// zeroes the three pointers.
    pub unsafe fn destroy() {
        let t = &mut *core::ptr::addr_of_mut!(TABLE);
        if !t.first.is_null() {
            let bytes = ((t.end as usize - t.first as usize) / ENTRY_SIZE) * ENTRY_SIZE;
            deallocate(t.first, bytes);
            t.first = null_mut();
            t.last = null_mut();
            t.end = null_mut();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::msvc_heap::{request, Request};

    #[test]
    fn one_big_allocation() {
        assert_eq!(request(ENTRIES * ENTRY_SIZE), Request::Big(0x1bc47));
    }
}
