//! The six kernel32 calls and the `operator new` the plugin makes for its
//! mapping, behind a trait so host tests can observe the exact call sequence
//! and arguments.

use core::ffi::c_void;

pub type Handle = *mut c_void;

pub trait Os {
    /// `operator new(size)` (MSVCR110 `??2@YAPEAX_K@Z`, i.e. malloc); NULL on failure.
    unsafe fn operator_new(&self, size: usize) -> *mut u8;
    /// `CreateFileMappingW(INVALID_HANDLE_VALUE, NULL, protect, size_high, size_low, name)`.
    unsafe fn create_file_mapping(&self, protect: u32, size_high: u32, size_low: u32, name: *const u16) -> Handle;
    /// `GetLastError()`.
    fn last_error(&self) -> u32;
    /// `OpenFileMappingW(access, inherit, name)`.
    unsafe fn open_file_mapping(&self, access: u32, inherit: bool, name: *const u16) -> Handle;
    /// `MapViewOfFile(handle, access, offset_high, offset_low, bytes)`.
    unsafe fn map_view(&self, handle: Handle, access: u32, offset_high: u32, offset_low: u32, bytes: usize) -> *mut u8;
    /// `UnmapViewOfFile(view)`.
    unsafe fn unmap_view(&self, view: *mut u8);
    /// `CloseHandle(handle)`.
    unsafe fn close_handle(&self, handle: Handle);
}

/// Declares an import that is called through its IAT slot (`call [__imp_X]`,
/// or `jmp [__imp_X]` as a tail call). rustc never
/// marks functions dllimport on windows-gnu, so a plain `extern fn` would be
/// called through a `jmp [__imp_X]` thunk, which exists only for
/// the imports in src/imports_*.s that carry one. A static, however, is
/// marked dllimport: its address is loaded from `__imp_X`, and that address
/// is the function. The macro wraps this in a function of the import's own
/// name and signature. `$x64` / `$x86` are the link names (LLVM adds the
/// i386 `_`; `\u{1}` suppresses it).
#[cfg(windows)]
macro_rules! iat_import {
    ($lib:literal, $abi:tt, $vis:vis fn $name:ident($($arg:ident: $ty:ty),* $(,)?) $(-> $ret:ty)?, $x64:literal, $x86:literal) => {
        #[allow(non_snake_case)]
        #[inline(always)]
        $vis unsafe fn $name($($arg: $ty),*) $(-> $ret)? {
            #[link(name = $lib)]
            extern "C" {
                #[cfg_attr(target_arch = "x86_64", link_name = $x64)]
                #[cfg_attr(target_arch = "x86", link_name = $x86)]
                static IMPORT: u8;
            }
            let f: unsafe extern $abi fn($($ty),*) $(-> $ret)? = core::mem::transmute(core::ptr::addr_of!(IMPORT));
            f($($arg),*)
        }
    };
}
#[cfg(windows)]
pub(crate) use iat_import;

#[cfg(windows)]
mod native {
    use super::{Handle, Os};
    use core::ffi::c_void;

    // MSVCR110 `operator new(size_t)`: x64 `??2@YAPEAX_K@Z`,
    // i386 `??2@YAPAXI@Z`.
    iat_import!("msvcr110", "C", fn operator_new(size: usize) -> *mut c_void, "??2@YAPEAX_K@Z", "\u{1}??2@YAPAXI@Z");
    // KERNEL32.
    iat_import!(
        "kernel32",
        "system",
        fn CreateFileMappingW(
            file: Handle,
            attributes: *mut c_void,
            protect: u32,
            size_high: u32,
            size_low: u32,
            name: *const u16,
        ) -> Handle,
        "CreateFileMappingW",
        "CreateFileMappingW@24"
    );
    iat_import!(
        "kernel32",
        "system",
        fn OpenFileMappingW(access: u32, inherit: i32, name: *const u16) -> Handle,
        "OpenFileMappingW",
        "OpenFileMappingW@12"
    );
    iat_import!(
        "kernel32",
        "system",
        fn MapViewOfFile(handle: Handle, access: u32, offset_high: u32, offset_low: u32, bytes: usize) -> *mut c_void,
        "MapViewOfFile",
        "MapViewOfFile@20"
    );
    iat_import!("kernel32", "system", fn UnmapViewOfFile(view: *const c_void) -> i32, "UnmapViewOfFile", "UnmapViewOfFile@4");
    iat_import!("kernel32", "system", fn CloseHandle(handle: Handle) -> i32, "CloseHandle", "CloseHandle@4");
    iat_import!("kernel32", "system", fn GetLastError() -> u32, "GetLastError", "GetLastError@0");

    const INVALID_HANDLE_VALUE: Handle = usize::MAX as Handle;

    pub struct Kernel32;

    impl Os for Kernel32 {
        unsafe fn operator_new(&self, size: usize) -> *mut u8 {
            operator_new(size) as *mut u8
        }
        unsafe fn create_file_mapping(&self, protect: u32, size_high: u32, size_low: u32, name: *const u16) -> Handle {
            CreateFileMappingW(INVALID_HANDLE_VALUE, core::ptr::null_mut(), protect, size_high, size_low, name)
        }
        fn last_error(&self) -> u32 {
            unsafe { GetLastError() }
        }
        unsafe fn open_file_mapping(&self, access: u32, inherit: bool, name: *const u16) -> Handle {
            OpenFileMappingW(access, inherit as i32, name)
        }
        unsafe fn map_view(&self, handle: Handle, access: u32, offset_high: u32, offset_low: u32, bytes: usize) -> *mut u8 {
            MapViewOfFile(handle, access, offset_high, offset_low, bytes) as *mut u8
        }
        unsafe fn unmap_view(&self, view: *mut u8) {
            UnmapViewOfFile(view as *const c_void);
        }
        unsafe fn close_handle(&self, handle: Handle) {
            CloseHandle(handle);
        }
    }

    pub static KERNEL32: Kernel32 = Kernel32;
}

#[cfg(windows)]
pub fn native() -> &'static dyn Os {
    &native::KERNEL32
}

/// UTF-16, NUL-terminated `Local\SimTelemetryETS2`.
pub static MAP_NAME_W: [u16; 23] = {
    let s = crate::layout::MAP_NAME.as_bytes();
    let mut w = [0u16; 23];
    let mut i = 0;
    while i < s.len() {
        w[i] = s[i] as u16;
        i += 1;
    }
    w
};
const _: () = assert!(crate::layout::MAP_NAME.len() == 22);
