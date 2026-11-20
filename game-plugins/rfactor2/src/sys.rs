//! Every operating-system call the plugin makes (KERNEL32 / ADVAPI32 and
//! the file primitives under its stdio calls), behind one trait so the
//! host tests can script and observe them. The Windows implementation is in
//! `win`.

use core::ffi::c_void;

pub type Handle = *mut c_void;

/// `SYSTEMTIME` fields the trace prefix prints.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LocalTime {
    pub hour: u16,
    pub minute: u16,
    pub second: u16,
    pub milliseconds: u16,
}

/// How a CRT stream opens its file (the mode strings the plugin uses).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenMode {
    /// `"a"` (`fopen`, `_fsopen(.., _SH_DENYNO)`): OPEN_ALWAYS, writes at end of file,
    /// share read|write.
    Append,
    /// `"w"` (`fopen`): CREATE_ALWAYS, share read|write.
    Write,
    /// `"rb"` (`fopen_s`, i.e. `_SH_SECURE`): OPEN_EXISTING, read, share read.
    ReadSecure,
}

pub trait Sys {
    // --- memory (operator new / free of the VC runtime) ---
    unsafe fn malloc(&self, size: usize) -> *mut u8;
    unsafe fn free(&self, p: *mut u8);
    /// The shared CRT new-handler; true asks operator new to retry malloc.
    fn call_new_handler(&self, _size: usize) -> bool {
        false
    }
    /// The current thread's shared CRT error; host fakes retain a compatibility value.
    fn crt_errno(&self, fallback: i32) -> i32 {
        fallback
    }
    fn set_crt_errno(&self, _value: i32) {}
    /// The NUL-terminated text uses the application's CRT conversion without clearing errno.
    fn crt_atol(&self, text: &[u8], fallback: &mut i32) -> i32 {
        crate::crt::atol(text, fallback)
    }
    /// The C++ allocation exception, distinct from invalid-parameter failure.
    fn allocation_failure(&self, failure: crate::allocation::Failure) -> ! {
        #[cfg(windows)]
        unsafe {
            crate::allocation::windows::throw(failure)
        }
        #[cfg(not(windows))]
        panic!("native allocation failure: {:?}", failure)
    }

    // --- file mappings ---
    /// `ConvertStringSecurityDescriptorToSecurityDescriptorA(sddl, SDDL_REVISION_1, &sd, NULL)`;
    /// NULL on failure (last error set).
    unsafe fn convert_sddl(&self, sddl: &[u8]) -> *mut c_void;
    /// `LocalFree(p)`.
    unsafe fn local_free(&self, p: *mut c_void);
    /// `CreateFileMappingA(INVALID_HANDLE_VALUE, sd ? {sizeof, sd, FALSE} : NULL,
    /// PAGE_READWRITE, 0, size, name)`.
    unsafe fn create_file_mapping(&self, sd: Option<*mut c_void>, size: u32, name: &[u8]) -> Handle;
    /// `MapViewOfFile(h, FILE_MAP_ALL_ACCESS, 0, 0, size)`.
    unsafe fn map_view(&self, h: Handle, size: usize) -> *mut u8;
    /// `UnmapViewOfFile(p) != 0`.
    unsafe fn unmap_view(&self, p: *mut u8) -> bool;
    /// `CloseHandle(h) != 0`.
    unsafe fn close_handle(&self, h: Handle) -> bool;
    fn get_last_error(&self) -> u32;
    /// `FormatMessageA(FORMAT_MESSAGE_ALLOCATE_BUFFER | FROM_SYSTEM | IGNORE_INSERTS,
    /// NULL, err, LANG_USER_DEFAULT (0x400), &buf, 0, NULL)`: (chars, buf).
    unsafe fn format_message(&self, err: u32) -> (u32, *mut u8);

    // --- process / module ---
    /// `GetModuleFileNameA(NULL, buf, 0x400)`.
    fn module_file_name(&self, buf: &mut [u8; 0x400]);
    /// `GetCurrentDirectoryA(0x104, buf)`.
    fn current_directory(&self, buf: &mut [u8]);
    fn current_process_id(&self) -> u32;
    fn current_thread_id(&self) -> u32;
    /// `LoadLibraryExA(path, NULL, 0)`.
    unsafe fn load_library(&self, path: &[u8]) -> Handle;
    /// `GetProcAddress(h, name)`.
    unsafe fn get_proc_address(&self, h: Handle, name: &[u8]) -> *const c_void;
    /// `FreeLibrary(h)` (callers test the BOOL against 1).
    unsafe fn free_library(&self, h: Handle) -> i32;

    // --- time ---
    fn query_performance_frequency(&self) -> i64;
    fn query_performance_counter(&self) -> i64;
    fn tick_count64(&self) -> u64;
    fn local_time(&self) -> LocalTime;
    /// The CRT's `atan2`.
    fn atan2(&self, y: f64, x: f64) -> f64;

    // --- files (the lowio layer under the CRT streams) ---
    /// Open `path` (NUL-terminated); Err(win32 error).
    unsafe fn file_open(&self, path: &[u8], mode: OpenMode) -> Result<Handle, u32>;
    /// Write all bytes (Append handles write at end of file); false on error.
    unsafe fn file_write(&self, h: Handle, data: &[u8]) -> bool;
    /// Read up to buf.len() bytes; Ok(0) at end of file.
    unsafe fn file_read(&self, h: Handle, buf: &mut [u8]) -> Result<usize, u32>;
    /// Move the file pointer (0 = begin, 2 = end); new position.
    unsafe fn file_seek(&self, h: Handle, off: i64, whence: u32) -> Result<i64, u32>;
    unsafe fn file_close(&self, h: Handle) -> bool;
    /// `DeleteFileA` (the CRT's `remove`).
    unsafe fn file_delete(&self, path: &[u8]) -> Result<(), u32>;

    /// The C runtime's default invalid-parameter handler
    /// (`__fastfail(FAST_FAIL_INVALID_ARG)`): the process ends.
    fn invalid_parameter(&self) -> !;

    /// A failed stack-cookie check
    /// (`__fastfail(FAST_FAIL_STACK_COOKIE_CHECK_FAILURE = 2)`): the process ends.
    fn report_gs_failure(&self) -> !;
}

#[cfg(windows)]
pub mod win {
    use super::{Handle, LocalTime, OpenMode, Sys};
    use core::ffi::c_void;

    #[repr(C)]
    struct SecurityAttributes {
        n_length: u32,
        lp_security_descriptor: *mut c_void,
        b_inherit_handle: i32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct SystemTime {
        year: u16,
        month: u16,
        day_of_week: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        milliseconds: u16,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateFileMappingA(h: Handle, sa: *mut SecurityAttributes, protect: u32, hi: u32, lo: u32, name: *const u8) -> Handle;
        fn MapViewOfFile(h: Handle, access: u32, hi: u32, lo: u32, bytes: usize) -> *mut c_void;
        fn UnmapViewOfFile(p: *const c_void) -> i32;
        fn CloseHandle(h: Handle) -> i32;
        fn GetLastError() -> u32;
        fn FormatMessageA(flags: u32, src: *const c_void, id: u32, lang: u32, buf: *mut u8, size: u32, args: *mut c_void) -> u32;
        fn LocalFree(p: *mut c_void) -> *mut c_void;
        fn GetModuleFileNameA(h: Handle, buf: *mut u8, size: u32) -> u32;
        fn GetCurrentDirectoryA(size: u32, buf: *mut u8) -> u32;
        fn GetCurrentProcessId() -> u32;
        fn GetCurrentThreadId() -> u32;
        fn LoadLibraryExA(path: *const u8, file: Handle, flags: u32) -> Handle;
        fn GetProcAddress(h: Handle, name: *const u8) -> *const c_void;
        fn FreeLibrary(h: Handle) -> i32;
        fn QueryPerformanceFrequency(f: *mut i64) -> i32;
        fn QueryPerformanceCounter(c: *mut i64) -> i32;
        fn GetTickCount64() -> u64;
        fn GetLocalTime(t: *mut SystemTime);
        fn CreateFileA(name: *const u8, access: u32, share: u32, sa: *mut SecurityAttributes, disp: u32, flags: u32, tmpl: Handle) -> Handle;
        fn WriteFile(h: Handle, buf: *const u8, n: u32, written: *mut u32, ov: *mut c_void) -> i32;
        fn ReadFile(h: Handle, buf: *mut u8, n: u32, read: *mut u32, ov: *mut c_void) -> i32;
        fn SetFilePointerEx(h: Handle, dist: i64, newpos: *mut i64, method: u32) -> i32;
        fn DeleteFileA(name: *const u8) -> i32;
    }

    #[link(name = "advapi32")]
    extern "system" {
        fn ConvertStringSecurityDescriptorToSecurityDescriptorA(s: *const u8, rev: u32, sd: *mut *mut c_void, size: *mut u32) -> i32;
    }

    #[link(name = "api-ms-win-crt-math-l1-1-0", kind = "raw-dylib")]
    extern "C" {
        fn atan2(y: f64, x: f64) -> f64;
    }

    #[link(name = "api-ms-win-crt-heap-l1-1-0", kind = "raw-dylib")]
    extern "C" {
        fn malloc(n: usize) -> *mut c_void;
        fn free(p: *mut c_void);
        fn _callnewh(n: usize) -> i32;
    }

    #[link(name = "api-ms-win-crt-runtime-l1-1-0", kind = "raw-dylib")]
    extern "C" {
        fn _errno() -> *mut i32;
    }

    #[link(name = "api-ms-win-crt-convert-l1-1-0", kind = "raw-dylib")]
    extern "C" {
        fn atol(text: *const u8) -> i32;
    }

    const INVALID_HANDLE_VALUE: Handle = usize::MAX as Handle;
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_SHARE_READ: u32 = 1;
    const FILE_SHARE_WRITE: u32 = 2;
    const CREATE_ALWAYS: u32 = 2;
    const OPEN_EXISTING: u32 = 3;
    const OPEN_ALWAYS: u32 = 4;
    const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;

    pub struct Win;

    impl Sys for Win {
        unsafe fn malloc(&self, size: usize) -> *mut u8 {
            malloc(size) as *mut u8
        }
        unsafe fn free(&self, p: *mut u8) {
            free(p as *mut c_void)
        }
        fn call_new_handler(&self, size: usize) -> bool {
            unsafe { _callnewh(size) != 0 }
        }
        fn crt_errno(&self, _fallback: i32) -> i32 {
            unsafe { *_errno() }
        }
        fn set_crt_errno(&self, value: i32) {
            unsafe { *_errno() = value }
        }
        fn crt_atol(&self, text: &[u8], _fallback: &mut i32) -> i32 {
            debug_assert_eq!(text.last(), Some(&0));
            unsafe { atol(text.as_ptr()) }
        }
        unsafe fn convert_sddl(&self, sddl: &[u8]) -> *mut c_void {
            let mut sd: *mut c_void = core::ptr::null_mut();
            if ConvertStringSecurityDescriptorToSecurityDescriptorA(sddl.as_ptr(), 1, &mut sd, core::ptr::null_mut()) == 0 {
                return core::ptr::null_mut();
            }
            sd
        }
        unsafe fn local_free(&self, p: *mut c_void) {
            LocalFree(p);
        }
        unsafe fn create_file_mapping(&self, sd: Option<*mut c_void>, size: u32, name: &[u8]) -> Handle {
            match sd {
                Some(sd) => {
                    let mut sa = SecurityAttributes {
                        n_length: core::mem::size_of::<SecurityAttributes>() as u32,
                        lp_security_descriptor: sd,
                        b_inherit_handle: 0,
                    };
                    CreateFileMappingA(INVALID_HANDLE_VALUE, &mut sa, 4, 0, size, name.as_ptr())
                }
                None => CreateFileMappingA(INVALID_HANDLE_VALUE, core::ptr::null_mut(), 4, 0, size, name.as_ptr()),
            }
        }
        unsafe fn map_view(&self, h: Handle, size: usize) -> *mut u8 {
            MapViewOfFile(h, 0xf001f, 0, 0, size) as *mut u8
        }
        unsafe fn unmap_view(&self, p: *mut u8) -> bool {
            UnmapViewOfFile(p as *const c_void) != 0
        }
        unsafe fn close_handle(&self, h: Handle) -> bool {
            CloseHandle(h) != 0
        }
        fn get_last_error(&self) -> u32 {
            unsafe { GetLastError() }
        }
        unsafe fn format_message(&self, err: u32) -> (u32, *mut u8) {
            let mut buf: *mut u8 = core::ptr::null_mut();
            let n = FormatMessageA(0x1300, core::ptr::null(), err, 0x400, &mut buf as *mut *mut u8 as *mut u8, 0, core::ptr::null_mut());
            (n, buf)
        }
        fn module_file_name(&self, buf: &mut [u8; 0x400]) {
            unsafe {
                GetModuleFileNameA(core::ptr::null_mut(), buf.as_mut_ptr(), 0x400);
            }
        }
        fn current_directory(&self, buf: &mut [u8]) {
            unsafe {
                GetCurrentDirectoryA(buf.len() as u32, buf.as_mut_ptr());
            }
        }
        fn current_process_id(&self) -> u32 {
            unsafe { GetCurrentProcessId() }
        }
        fn current_thread_id(&self) -> u32 {
            unsafe { GetCurrentThreadId() }
        }
        unsafe fn load_library(&self, path: &[u8]) -> Handle {
            LoadLibraryExA(path.as_ptr(), core::ptr::null_mut(), 0)
        }
        unsafe fn get_proc_address(&self, h: Handle, name: &[u8]) -> *const c_void {
            GetProcAddress(h, name.as_ptr())
        }
        unsafe fn free_library(&self, h: Handle) -> i32 {
            FreeLibrary(h)
        }
        fn query_performance_frequency(&self) -> i64 {
            let mut f = 0;
            unsafe { QueryPerformanceFrequency(&mut f) };
            f
        }
        fn query_performance_counter(&self) -> i64 {
            let mut c = 0;
            unsafe { QueryPerformanceCounter(&mut c) };
            c
        }
        fn tick_count64(&self) -> u64 {
            unsafe { GetTickCount64() }
        }
        fn atan2(&self, y: f64, x: f64) -> f64 {
            unsafe { atan2(y, x) }
        }
        fn local_time(&self) -> LocalTime {
            let mut t = SystemTime::default();
            unsafe { GetLocalTime(&mut t) };
            LocalTime { hour: t.hour, minute: t.minute, second: t.second, milliseconds: t.milliseconds }
        }
        unsafe fn file_open(&self, path: &[u8], mode: OpenMode) -> Result<Handle, u32> {
            let (access, share, disp) = match mode {
                OpenMode::Append => (GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE, OPEN_ALWAYS),
                OpenMode::Write => (GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE, CREATE_ALWAYS),
                OpenMode::ReadSecure => (GENERIC_READ, FILE_SHARE_READ, OPEN_EXISTING),
            };
            // The CRT opens its streams inheritable (lpSecurityAttributes with
            // bInheritHandle = TRUE unless 'N' is in the mode).
            let mut sa = SecurityAttributes {
                n_length: core::mem::size_of::<SecurityAttributes>() as u32,
                lp_security_descriptor: core::ptr::null_mut(),
                b_inherit_handle: 1,
            };
            let h = CreateFileA(path.as_ptr(), access, share, &mut sa, disp, FILE_ATTRIBUTE_NORMAL, core::ptr::null_mut());
            if h == INVALID_HANDLE_VALUE {
                Err(GetLastError())
            } else {
                Ok(h)
            }
        }
        unsafe fn file_write(&self, h: Handle, data: &[u8]) -> bool {
            // lowio _write in append mode seeks to the end before each write.
            let mut pos = 0i64;
            SetFilePointerEx(h, 0, &mut pos, 2);
            let mut off = 0;
            while off < data.len() {
                let mut n = 0u32;
                let chunk = (data.len() - off).min(0x4000_0000) as u32;
                if WriteFile(h, data.as_ptr().add(off), chunk, &mut n, core::ptr::null_mut()) == 0 {
                    self.set_crt_errno(crate::crt::errno_from_win32(GetLastError()));
                    return false;
                }
                if n == 0 {
                    return false;
                }
                off += n as usize;
            }
            true
        }
        unsafe fn file_read(&self, h: Handle, buf: &mut [u8]) -> Result<usize, u32> {
            let mut n = 0u32;
            if ReadFile(h, buf.as_mut_ptr(), buf.len().min(0x4000_0000) as u32, &mut n, core::ptr::null_mut()) == 0 {
                let error = GetLastError();
                self.set_crt_errno(crate::crt::errno_from_win32(error));
                return Err(error);
            }
            Ok(n as usize)
        }
        unsafe fn file_seek(&self, h: Handle, off: i64, whence: u32) -> Result<i64, u32> {
            let mut pos = 0i64;
            if SetFilePointerEx(h, off, &mut pos, whence) == 0 {
                let error = GetLastError();
                self.set_crt_errno(crate::crt::errno_from_win32(error));
                return Err(error);
            }
            Ok(pos)
        }
        unsafe fn file_close(&self, h: Handle) -> bool {
            if CloseHandle(h) != 0 {
                true
            } else {
                self.set_crt_errno(crate::crt::errno_from_win32(GetLastError()));
                false
            }
        }
        unsafe fn file_delete(&self, path: &[u8]) -> Result<(), u32> {
            if DeleteFileA(path.as_ptr()) == 0 {
                Err(GetLastError())
            } else {
                Ok(())
            }
        }
        fn invalid_parameter(&self) -> ! {
            // __fastfail(FAST_FAIL_INVALID_ARG = 5)
            unsafe {
                core::arch::asm!("int 0x29", in("ecx") 5u32, options(noreturn));
            }
        }
        fn report_gs_failure(&self) -> ! {
            // __fastfail(FAST_FAIL_STACK_COOKIE_CHECK_FAILURE = 2)
            unsafe {
                core::arch::asm!("int 0x29", in("ecx") 2u32, options(noreturn));
            }
        }
    }

    pub static WIN: Win = Win;
}
