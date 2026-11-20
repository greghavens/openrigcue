//! The operator new retry loop and C++ allocation exceptions.

use crate::sys::Sys;
use core::ffi::c_void;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    BadAlloc,
    BadArrayNewLength,
}

#[cfg(windows)]
pub(crate) type Body = unsafe extern "C" fn(*mut c_void) -> *mut u8;
#[cfg(not(windows))]
pub(crate) type Body = unsafe extern "C-unwind" fn(*mut c_void) -> *mut u8;

/// `operator new`: malloc, then the new-handler, repeating only if it returns nonzero.
pub unsafe fn operator_new(sys: &dyn Sys, size: usize) -> *mut u8 {
    loop {
        let p = sys.malloc(size);
        if !p.is_null() {
            return p;
        }
        if !sys.call_new_handler(size) {
            sys.allocation_failure(if size == usize::MAX { Failure::BadArrayNewLength } else { Failure::BadAlloc });
        }
    }
}

/// A finally frame that also runs cleanup when a shared new-handler throws.
pub(crate) unsafe fn with_cleanup(
    context: *mut c_void,
    body: Body,
    cleanup: unsafe extern "C" fn(*mut c_void),
) -> *mut u8 {
    #[cfg(windows)]
    {
        windows::with_cleanup(context, body, cleanup)
    }
    #[cfg(not(windows))]
    {
        // Host fake failures unwind as Rust panics; Windows uses the SEH frame below.
        struct Cleanup(*mut c_void, unsafe extern "C" fn(*mut c_void));
        impl Drop for Cleanup {
            fn drop(&mut self) {
                unsafe { (self.1)(self.0) }
            }
        }
        let _guard = Cleanup(context, cleanup);
        body(context)
    }
}

#[cfg(windows)]
pub(crate) mod windows;
