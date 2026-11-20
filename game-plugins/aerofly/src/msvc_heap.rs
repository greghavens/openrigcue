//! The plugin's heap path:
//!
//! * `operator new`: `malloc` =
//!   `HeapAlloc(crt_heap, 0, size ? size : 1)` for sizes up to
//!   0xffffffffffffffe0 (larger: NULL without calling HeapAlloc). On NULL
//!   (no new handler is ever installed) `std::bad_array_new_length` is
//!   thrown when size == SIZE_MAX, `std::bad_alloc` otherwise.
//! * the container allocator:
//!   blocks of 0x1000 bytes or more are over-allocated by 0x27, the user
//!   pointer is `(raw + 0x27) & ~0x1f` and `raw` is stored just below it;
//!   `size + 0x27` overflowing throws bad_array_new_length.
//! * deallocation: for big
//!   blocks `raw = user[-1]`; if `user - raw - 8 > 0x1f` the
//!   invalid-parameter failure ends the process;
//!   then `free` = `HeapFree(crt_heap, 0, p)` (NULL is
//!   skipped).
//! * `crt_heap` is set once at load
//!   (`GetProcessHeap()`, start-up fails when it is NULL);
//!   no allocation or free calls GetProcessHeap.
//!
//! The arithmetic is here as plain functions (host-tested); the Windows half
//! performs it on the process heap.

/// Size from which a block is over-aligned.
pub const BIG_ALLOCATION_THRESHOLD: usize = 0x1000;
/// Extra bytes of an over-aligned block: sizeof(void*) + 32 - 1
pub const NON_USER_SIZE: usize = 0x27;
/// Alignment of an over-aligned block.
pub const BIG_ALLOCATION_ALIGNMENT: usize = 0x20;
/// The largest request: `malloc` refuses anything above it.
pub const HEAP_MAXREQ: usize = 0xFFFF_FFFF_FFFF_FFE0;

/// What the container allocator asks `operator new` for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    /// 0 bytes: no call, NULL pointer.
    Null,
    /// below the threshold: `operator new(bytes)`, pointer used as is.
    Plain(usize),
    /// `operator new(bytes + 0x27)`, pointer over-aligned.
    Big(usize),
    /// `bytes + 0x27` overflows: bad_array_new_length.
    Overflow,
}

pub const fn request(bytes: usize) -> Request {
    if bytes < BIG_ALLOCATION_THRESHOLD {
        if bytes == 0 {
            Request::Null
        } else {
            Request::Plain(bytes)
        }
    } else {
        match bytes.checked_add(NON_USER_SIZE) {
            // overflow test of size + 0x27
            Some(n) => Request::Big(n),
            None => Request::Overflow,
        }
    }
}

/// User pointer of a big block: `(raw + 0x27) & ~0x1f`.
pub const fn align_big(raw: usize) -> usize {
    raw.wrapping_add(NON_USER_SIZE) & !(BIG_ALLOCATION_ALIGNMENT - 1)
}

/// The check before freeing a big block: `(user - raw) - 8 <= 0x1f`.
pub const fn big_block_ok(user: usize, raw: usize) -> bool {
    user.wrapping_sub(raw).wrapping_sub(8) <= 0x1f
}

/// Size `malloc` passes to HeapAlloc, or None when it returns NULL
/// without calling it.
pub const fn heap_alloc_size(size: usize) -> Option<usize> {
    if size > HEAP_MAXREQ {
        None
    } else if size == 0 {
        Some(1)
    } else {
        Some(size)
    }
}

/// Which exception `operator new` throws when the allocation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NewFailure {
    BadArrayNewLength,
    BadAlloc,
}

pub const fn new_failure(size: usize) -> NewFailure {
    if size == usize::MAX {
        NewFailure::BadArrayNewLength
    } else {
        NewFailure::BadAlloc
    }
}

#[cfg(windows)]
pub use self::win::*;

#[cfg(windows)]
mod win {
    use super::*;
    use crate::crt::{invalid_parameter_noinfo_noreturn, GetProcessHeap, HeapAlloc, HeapFree};
    use crate::cxx_eh::{throw_bad_alloc, throw_bad_array_new_length};
    use core::ffi::c_void;

    /// The heap handle.
    static mut CRT_HEAP: crate::crt::HANDLE = core::ptr::null_mut();

    /// Takes the process heap; run by the start-up
    /// before anything else at load.
    pub unsafe fn initialize_heap() -> bool {
        let h = GetProcessHeap();
        core::ptr::write_volatile(core::ptr::addr_of_mut!(CRT_HEAP), h);
        !h.is_null()
    }

    #[inline(always)]
    unsafe fn crt_heap() -> crate::crt::HANDLE {
        core::ptr::read_volatile(core::ptr::addr_of!(CRT_HEAP))
    }

    /// `malloc` on the process heap.
    pub unsafe fn malloc(size: usize) -> *mut u8 {
        match heap_alloc_size(size) {
            // no new handler exists: one attempt only
            Some(n) => HeapAlloc(crt_heap(), 0, n) as *mut u8,
            None => core::ptr::null_mut(),
        }
    }

    /// `free` on the process heap.
    pub unsafe fn free(p: *mut u8) {
        if !p.is_null() {
            HeapFree(crt_heap(), 0, p as *mut c_void);
        }
    }

    /// `operator new`
    pub unsafe fn operator_new(size: usize) -> *mut u8 {
        let p = malloc(size);
        if !p.is_null() {
            return p;
        }
        match new_failure(size) {
            NewFailure::BadArrayNewLength => throw_bad_array_new_length(),
            NewFailure::BadAlloc => throw_bad_alloc(),
        }
    }

    /// The container allocation.
    pub unsafe fn allocate(bytes: usize) -> *mut u8 {
        match request(bytes) {
            Request::Null => core::ptr::null_mut(),
            Request::Plain(n) => operator_new(n),
            Request::Overflow => throw_bad_array_new_length(),
            Request::Big(n) => {
                let raw = operator_new(n);
                if raw.is_null() {
                    invalid_parameter_noinfo_noreturn();
                }
                let user = align_big(raw as usize) as *mut u8;
                *(user as *mut usize).sub(1) = raw as usize;
                user
            }
        }
    }

    /// The container deallocation, ending in `free`.
    pub unsafe fn deallocate(user: *mut u8, bytes: usize) {
        let mut p = user;
        if bytes >= BIG_ALLOCATION_THRESHOLD {
            let raw = *(user as *const usize).sub(1);
            if !big_block_ok(user as usize, raw) {
                invalid_parameter_noinfo_noreturn();
            }
            p = raw as *mut u8;
        }
        free(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_sizes_match_original_trace() {
        // Expected HeapAlloc sizes
        assert_eq!(request(0x80), Request::Plain(0x80));
        assert_eq!(request(0xe00), Request::Plain(0xe00));
        assert_eq!(request(42 * 0x80), Request::Big(0x1527));
        assert_eq!(request(63 * 0x80), Request::Big(0x1fa7));
        assert_eq!(request(418 * 0x110), Request::Big(0x1bc47));
        assert_eq!(request(0), Request::Null);
        assert_eq!(request(0xfff), Request::Plain(0xfff));
        assert_eq!(request(0x1000), Request::Big(0x1027));
        assert_eq!(request(usize::MAX - 0x26), Request::Overflow);
        assert_eq!(request(usize::MAX - 0x27), Request::Big(usize::MAX));
    }

    #[test]
    fn big_block_alignment() {
        // a user pointer 0x10 above a 16-aligned raw block
        assert_eq!(align_big(0x3000_0010), 0x3000_0020);
        assert_eq!(align_big(0x3000_0020), 0x3000_0040);
        assert!(big_block_ok(0x3000_0020, 0x3000_0010));
        assert!(big_block_ok(0x3000_0040, 0x3000_0020));
        assert!(!big_block_ok(0x3000_0040, 0x3000_0040)); // 0 - 8 wraps
        assert!(!big_block_ok(0x3000_0068, 0x3000_0040));
        for raw in (0x1000..0x1100).step_by(16) {
            let u = align_big(raw);
            assert_eq!(u % 32, 0);
            assert!(big_block_ok(u, raw));
        }
    }

    #[test]
    fn malloc_base_sizes() {
        assert_eq!(heap_alloc_size(0), Some(1));
        assert_eq!(heap_alloc_size(0x80), Some(0x80));
        assert_eq!(heap_alloc_size(HEAP_MAXREQ), Some(HEAP_MAXREQ));
        assert_eq!(heap_alloc_size(HEAP_MAXREQ + 1), None);
        assert_eq!(new_failure(usize::MAX), NewFailure::BadArrayNewLength);
        assert_eq!(new_failure(0x80), NewFailure::BadAlloc);
    }
}
