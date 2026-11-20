//! The KERNEL32 calls the plugin code makes, behind a trait so host tests
//! can script them. `Win` is the real implementation (i686 DLL only).

use core::ffi::c_void;

pub type Handle = *mut c_void;

pub trait Os {
    fn create_file_mapping_a(&self, file: Handle, prot: u32, size_hi: u32, size_lo: u32, name: *const u8) -> Handle;
    fn get_last_error(&self) -> u32;
    fn open_file_mapping_a(&self, access: u32, inherit: i32, name: *const u8) -> Handle;
    fn map_view_of_file(&self, h: Handle, access: u32, off_hi: u32, off_lo: u32, size: usize) -> *mut u8;
    fn unmap_view_of_file(&self, p: *const u8) -> i32;
    fn close_handle(&self, h: Handle) -> i32;
    /// QueryPerformanceCounter; None when it fails.
    fn qpc(&self) -> Option<i64>;
    /// QueryPerformanceFrequency; None when it fails.
    fn qpf(&self) -> Option<i64>;
}

#[cfg(all(windows, target_arch = "x86"))]
pub mod win {
    use super::*;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateFileMappingA(h: Handle, sa: *mut c_void, prot: u32, hi: u32, lo: u32, name: *const u8) -> Handle;
        fn GetLastError() -> u32;
        fn OpenFileMappingA(access: u32, inherit: i32, name: *const u8) -> Handle;
        fn MapViewOfFile(h: Handle, access: u32, hi: u32, lo: u32, size: usize) -> *mut u8;
        fn UnmapViewOfFile(p: *const u8) -> i32;
        fn CloseHandle(h: Handle) -> i32;
        fn QueryPerformanceCounter(v: *mut i64) -> i32;
        fn QueryPerformanceFrequency(v: *mut i64) -> i32;
    }

    pub struct Win;
    pub static WIN: Win = Win;

    impl Os for Win {
        fn create_file_mapping_a(&self, file: Handle, prot: u32, hi: u32, lo: u32, name: *const u8) -> Handle {
            unsafe { CreateFileMappingA(file, core::ptr::null_mut(), prot, hi, lo, name) }
        }
        fn get_last_error(&self) -> u32 {
            unsafe { GetLastError() }
        }
        fn open_file_mapping_a(&self, access: u32, inherit: i32, name: *const u8) -> Handle {
            unsafe { OpenFileMappingA(access, inherit, name) }
        }
        fn map_view_of_file(&self, h: Handle, access: u32, hi: u32, lo: u32, size: usize) -> *mut u8 {
            unsafe { MapViewOfFile(h, access, hi, lo, size) }
        }
        fn unmap_view_of_file(&self, p: *const u8) -> i32 {
            unsafe { UnmapViewOfFile(p) }
        }
        fn close_handle(&self, h: Handle) -> i32 {
            unsafe { CloseHandle(h) }
        }
        fn qpc(&self) -> Option<i64> {
            let mut v = 0i64;
            if unsafe { QueryPerformanceCounter(&mut v) } != 0 { Some(v) } else { None }
        }
        fn qpf(&self) -> Option<i64> {
            let mut v = 0i64;
            if unsafe { QueryPerformanceFrequency(&mut v) } != 0 { Some(v) } else { None }
        }
    }
}
