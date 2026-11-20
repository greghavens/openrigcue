//! The DLL start-up of the plugin: DLL entry, C initialisation, the security
//! cookie, the onexit table, the run-time check stubs and the type-info
//! clean-up, with the MSVCR110.dll / KERNEL32
//! imports it makes. It replaces the mingw-w64 CRT in the release DLL.
//!
//! Only built for the release DLL (no `std`, no debug assertions, Windows
//! target); `build.rs` links that DLL with `-nostartfiles` and this module's
//! entry point. Functions that carry SEH scopes (`__DllMainCRTStartup`,
//! `_onexit`, `_IsNonwritableInCurrentImage`), the frame-capturing
//! `__report_gsfailure`, `__security_check_cookie` and, on i386,
//! `__SEH_prolog4` / `__SEH_epilog4` / `_except_handler4` are assembly
//! (`crt_x86_64.s`, `crt_x86.s`) since Rust has no `__try`; everything else
//! is here.
//!
//! What it does: the security cookie is
//! seeded on DLL_PROCESS_ATTACH, `_CRT_INIT` runs the `__xi` table
//! (`pre_c_init`, `__atonexitinit`) through MSVCR110 `_initterm_e` and the
//! (empty) `__xc` table through `_initterm`, registers `_RTC_Terminate` and
//! `__clean_type_info_names` with the DLL's own encoded onexit table, calls
//! DllMain, and on DLL_PROCESS_DETACH calls DllMain then runs and frees the
//! onexit table.

use core::ffi::c_void;
use core::ptr::null_mut;
use core::sync::atomic::{AtomicPtr, Ordering};

use crate::os::iat_import;

type Pvfv = unsafe extern "C" fn();
type Pifv = unsafe extern "C" fn() -> i32;
/// `_pRawDllMain` / `__dyn_tls_init_callback` / DllMain signature.
pub type DllMainFn = unsafe extern "system" fn(*mut c_void, u32, *mut c_void) -> i32;
type TlsCallback = unsafe extern "system" fn(*mut c_void, u32, *mut c_void);

// MSVCR110.dll and KERNEL32.dll. The calls made through the IAT
// (`call [__imp_X]`) use `iat_import!`; the ones made through a
// `jmp [IAT]` thunk are plain extern functions, which resolve to the thunks
// in imports_*.s.
iat_import!("msvcr110", "C", fn _malloc_crt(size: usize) -> *mut c_void, "_malloc_crt", "_malloc_crt");
iat_import!("msvcr110", "C", fn _calloc_crt(count: usize, size: usize) -> *mut c_void, "_calloc_crt", "_calloc_crt");
iat_import!("msvcr110", "C", fn free(p: *mut c_void), "free", "free");
extern "C" {
    fn __crtUnhandledException(p: *const c_void) -> i32;
    fn _initterm(begin: *const Option<Pvfv>, end: *const Option<Pvfv>);
    fn _initterm_e(begin: *const Option<Pifv>, end: *const Option<Pifv>) -> i32;
    fn _amsg_exit(code: i32);
    fn __clean_type_info_names_internal(root: *mut c_void);
    fn __crtTerminateProcess(code: u32);
    #[cfg(target_arch = "x86_64")]
    fn __crt_debugger_hook(code: i32);
    #[cfg(target_arch = "x86")]
    fn _crt_debugger_hook(code: i32);
}
#[cfg(target_arch = "x86")]
use _crt_debugger_hook as __crt_debugger_hook;

iat_import!("kernel32", "system", fn EncodePointer(p: *mut c_void) -> *mut c_void, "EncodePointer", "EncodePointer@4");
iat_import!("kernel32", "system", fn DecodePointer(p: *mut c_void) -> *mut c_void, "DecodePointer", "DecodePointer@4");
iat_import!(
    "kernel32",
    "system",
    fn GetSystemTimeAsFileTime(ft: *mut u64),
    "GetSystemTimeAsFileTime",
    "GetSystemTimeAsFileTime@4"
);
iat_import!("kernel32", "system", fn GetCurrentThreadId() -> u32, "GetCurrentThreadId", "GetCurrentThreadId@0");
iat_import!("kernel32", "system", fn GetTickCount64() -> u64, "GetTickCount64", "GetTickCount64@0");
iat_import!(
    "kernel32",
    "system",
    fn QueryPerformanceCounter(count: *mut u64) -> i32,
    "QueryPerformanceCounter",
    "QueryPerformanceCounter@4"
);
iat_import!("kernel32", "system", fn IsDebuggerPresent() -> i32, "IsDebuggerPresent", "IsDebuggerPresent@0");

// Defined in crt_*.s.
extern "C" {
    /// `_onexit`, the DLL-local one.
    #[link_name = "ets2_crt_onexit"]
    fn crt_onexit(f: Pvfv) -> Option<Pvfv>;
    /// `_IsNonwritableInCurrentImage`: whether an address lies in a read-only section of this image.
    #[link_name = "ets2_crt_is_nonwritable_in_current_image"]
    fn is_nonwritable_in_current_image(target: *const u8) -> i32;
}

// ---- .data / .bss --------------------------------------------------------

#[cfg(target_pointer_width = "64")]
const DEFAULT_SECURITY_COOKIE: usize = 0x2b99_2ddf_a232;
#[cfg(target_pointer_width = "32")]
const DEFAULT_SECURITY_COOKIE: usize = 0xbb40_e64e;

/// The security cookie.
static mut SECURITY_COOKIE: usize = DEFAULT_SECURITY_COOKIE;
/// The complement of the security cookie.
static mut SECURITY_COOKIE_COMPLEMENT: usize = !DEFAULT_SECURITY_COOKIE;
/// The reason of the DllMain call in progress.
static mut NATIVE_DLLMAIN_REASON: u32 = u32::MAX;
/// The count of process attaches.
static mut PROC_ATTACHED: i32 = 0;
/// The start-up lock.
static NATIVE_STARTUP_LOCK: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
/// The start-up state:
/// 0 uninitialized, 1 initializing, 2 initialized.
static mut NATIVE_STARTUP_STATE: u32 = 0;
/// The end of the onexit table, encoded.
static mut ONEXITEND: *mut c_void = null_mut();
/// The start of the onexit table, encoded.
static mut ONEXITBEGIN: *mut c_void = null_mut();
/// The dynamic TLS initialisation callback: no TLS
/// callbacks in this image, so it stays NULL.
static mut DYN_TLS_INIT_CALLBACK: Option<TlsCallback> = None;
/// The root node of the type-info name list.
static mut TYPE_INFO_ROOT_NODE: [usize; 2] = [0; 2];
/// Whether a debugger was present at a security failure.
static mut DEBUGGER_WAS_PRESENT: i32 = 0;

/// The raw DllMain hook: NULL.
static RAW_DLL_MAIN: Option<DllMainFn> = None;

/// The exception record of a security failure (x64 0x98 bytes; i386 0x50).
static mut GS_EXCEPTION_RECORD: [usize; if cfg!(target_pointer_width = "64") { 0x98 / 8 } else { 0x50 / 4 }] =
    [0; if cfg!(target_pointer_width = "64") { 0x98 / 8 } else { 0x50 / 4 }];
#[repr(C, align(16))]
pub struct GsContext(pub [u8; if cfg!(target_pointer_width = "64") { 0x4d0 } else { 0x2cc }]);
/// The context record of a security failure (x64 0x4d0 bytes; i386 0x2cc).
static mut GS_CONTEXT_RECORD: GsContext =
    GsContext([0; if cfg!(target_pointer_width = "64") { 0x4d0 } else { 0x2cc }]);

// ---- .rdata: the initializer tables (CRT$XIA..XIZ, CRT$XCA..XCZ) -----------

/// The C initializer table `__xi_a .. __xi_z`.
static XI: [Option<Pifv>; 3] = [None, Some(pre_c_init), Some(atonexitinit)];
/// The C++ initializer table `__xc_a .. __xc_z`.
static XC: [Option<Pvfv>; 1] = [None];
/// The run-time check tables `__rtc_iaa .. __rtc_izz` and `__rtc_taa .. __rtc_tzz`: empty.
static RTC_I: [Option<Pvfv>; 0] = [];
static RTC_T: [Option<Pvfv>; 0] = [];

// ---- functions -------------------------------------------------------------

/// `NtCurrentTeb()->NtTib.StackBase`, the fiber id `_CRT_INIT` locks with.
#[inline(always)]
unsafe fn stack_base() -> *mut c_void {
    let base: *mut c_void;
    #[cfg(target_arch = "x86_64")]
    core::arch::asm!("mov {t}, qword ptr gs:[0x30]", "mov {b}, qword ptr [{t} + 8]", t = out(reg) _, b = out(reg) base,
        options(nostack, readonly, preserves_flags));
    #[cfg(target_arch = "x86")]
    core::arch::asm!("mov {t}, dword ptr fs:[0x18]", "mov {b}, dword ptr [{t} + 4]", t = out(reg) _, b = out(reg) base,
        options(nostack, readonly, preserves_flags));
    base
}

/// The start-up lock spin: true when this fiber already holds it.
#[inline(always)]
unsafe fn startup_lock() -> bool {
    let fiber = stack_base();
    loop {
        match NATIVE_STARTUP_LOCK.compare_exchange(null_mut(), fiber, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return false,
            Err(owner) if owner == fiber => return true,
            Err(_) => {}
        }
    }
}

/// `_CRT_INIT` (stdcall): C initialisation on attach, the onexit run on detach.
unsafe extern "system" fn crt_init(instance: *mut c_void, reason: u32, reserved: *mut c_void) -> i32 {
    if reason == 0 {
        if PROC_ATTACHED <= 0 {
            return 0;
        }
        PROC_ATTACHED -= 1;
        let nested = startup_lock();
        if NATIVE_STARTUP_STATE != 2 {
            _amsg_exit(0x1f); // _RT_CRT_INIT_CONFLICT
            return 1;
        }
        let mut begin = DecodePointer(ONEXITBEGIN) as *mut *mut c_void;
        if !begin.is_null() {
            let end = DecodePointer(ONEXITEND) as *mut *mut c_void;
            let mut saved_begin = begin;
            let mut saved_end = end;
            let mut p = end;
            loop {
                p = p.wrapping_sub(1);
                if p < begin {
                    break;
                }
                if (*p).is_null() || *p == EncodePointer(null_mut()) {
                    continue;
                }
                let f: Pvfv = core::mem::transmute(DecodePointer(*p));
                *p = EncodePointer(null_mut());
                f();
                let new_begin = DecodePointer(ONEXITBEGIN) as *mut *mut c_void;
                let new_end = DecodePointer(ONEXITEND) as *mut *mut c_void;
                if saved_begin != new_begin || saved_end != new_end {
                    saved_begin = new_begin;
                    begin = new_begin;
                    saved_end = new_end;
                    p = new_end;
                }
            }
            if begin as usize != usize::MAX {
                free(begin as *mut c_void);
            }
            let e = EncodePointer(null_mut());
            ONEXITEND = e;
            ONEXITBEGIN = e;
        }
        NATIVE_STARTUP_STATE = 0;
        if !nested {
            NATIVE_STARTUP_LOCK.swap(null_mut(), Ordering::SeqCst);
        }
    } else if reason == 1 {
        let nested = startup_lock();
        if NATIVE_STARTUP_STATE != 0 {
            _amsg_exit(0x1f);
        } else {
            NATIVE_STARTUP_STATE = 1;
            if _initterm_e(XI.as_ptr(), XI.as_ptr().add(XI.len())) != 0 {
                return 0;
            }
            _initterm(XC.as_ptr(), XC.as_ptr().add(XC.len()));
            NATIVE_STARTUP_STATE = 2;
        }
        if !nested {
            NATIVE_STARTUP_LOCK.swap(null_mut(), Ordering::SeqCst);
        }
        let tls = core::ptr::addr_of!(DYN_TLS_INIT_CALLBACK);
        if let Some(cb) = *tls {
            if is_nonwritable_in_current_image(tls as *const u8) != 0 {
                cb(instance, 2, reserved); // DLL_THREAD_ATTACH
            }
        }
        PROC_ATTACHED += 1;
    }
    1
}

/// The first C initializer: the onexit table
/// (32 entries), the run-time check initialisation, and the two exit registrations.
unsafe extern "C" fn pre_c_init() -> i32 {
    let table = _malloc_crt(32 * core::mem::size_of::<usize>()) as *mut usize;
    let e = EncodePointer(table as *mut c_void);
    ONEXITBEGIN = e;
    ONEXITEND = e;
    if table.is_null() {
        return 1;
    }
    *table = 0;
    rtc_initialize();
    atexit(rtc_terminate);
    atexit(clean_type_info_names);
    0
}

/// Initialises the onexit table.
unsafe extern "C" fn atonexitinit() -> i32 {
    if !ONEXITBEGIN.is_null() {
        return 0;
    }
    let table = _calloc_crt(32, core::mem::size_of::<usize>()) as *mut usize;
    let e = EncodePointer(table as *mut c_void);
    ONEXITBEGIN = e;
    ONEXITEND = e;
    if table.is_null() {
        return 0x18; // _RT_ONEXIT
    }
    *table = 0;
    0
}

/// `atexit`: registers a function in the onexit table.
unsafe extern "C" fn atexit(f: Pvfv) -> i32 {
    if crt_onexit(f).is_some() {
        0
    } else {
        -1
    }
}

/// Runs the run-time check initializer table.
#[inline(never)]
unsafe extern "C" fn rtc_initialize() {
    for f in RTC_I.iter().flatten() {
        f();
    }
}

/// Runs the run-time check terminator table.
#[inline(never)]
unsafe extern "C" fn rtc_terminate() {
    for f in RTC_T.iter().flatten() {
        f();
    }
}

/// Cleans up the type-info name list.
unsafe extern "C" fn clean_type_info_names() {
    __clean_type_info_names_internal(core::ptr::addr_of_mut!(TYPE_INFO_ROOT_NODE) as *mut c_void);
}

/// Seeds the security cookie (x64).
#[cfg(target_pointer_width = "64")]
unsafe extern "C" fn security_init_cookie() {
    let mut system_time: u64 = 0;
    let mut cookie = SECURITY_COOKIE;
    if cookie == DEFAULT_SECURITY_COOKIE {
        GetSystemTimeAsFileTime(&mut system_time);
        let mut perf: u64 = system_time;
        perf ^= GetCurrentThreadId() as u64;
        perf ^= GetTickCount64() << 24;
        perf ^= GetTickCount64();
        let mut counter: u64 = 0;
        QueryPerformanceCounter(&mut counter);
        let perf_addr = core::ptr::addr_of_mut!(perf);
        let perf = core::ptr::read_volatile(perf_addr);
        cookie = ((counter << 32) ^ counter ^ perf ^ (perf_addr as u64)) as usize & 0xffff_ffff_ffff;
        if cookie == DEFAULT_SECURITY_COOKIE {
            cookie = DEFAULT_SECURITY_COOKIE + 1;
        }
        SECURITY_COOKIE = cookie;
    }
    SECURITY_COOKIE_COMPLEMENT = !cookie;
}

/// Seeds the security cookie (i386).
#[cfg(target_pointer_width = "32")]
unsafe extern "C" fn security_init_cookie() {
    let mut system_time: u64 = 0;
    let cookie = SECURITY_COOKIE;
    if cookie != DEFAULT_SECURITY_COOKIE && cookie & 0xffff_0000 != 0 {
        SECURITY_COOKIE_COMPLEMENT = !cookie;
        return;
    }
    GetSystemTimeAsFileTime(&mut system_time);
    let mut perf: u32 = (system_time >> 32) as u32 ^ system_time as u32;
    perf ^= GetCurrentThreadId();
    perf ^= GetTickCount64() as u32;
    let mut counter: u64 = 0;
    QueryPerformanceCounter(&mut counter);
    let perf_addr = core::ptr::addr_of_mut!(perf);
    let perf = core::ptr::read_volatile(perf_addr);
    let mut c = (counter >> 32) as u32 ^ counter as u32 ^ perf ^ (perf_addr as u32);
    if c == DEFAULT_SECURITY_COOKIE as u32 {
        c = DEFAULT_SECURITY_COOKIE as u32 + 1;
    } else if c & 0xffff_0000 == 0 {
        c |= (c | 0x4711) << 16;
    }
    SECURITY_COOKIE = c as usize;
    SECURITY_COOKIE_COMPLEMENT = !(c as usize);
}

/// Raises a security failure: the unhandled exception filter, then process termination.
unsafe extern "C" fn raise_securityfailure(exception_pointers: *const c_void) {
    DEBUGGER_WAS_PRESENT = IsDebuggerPresent();
    __crt_debugger_hook(1);
    __crtUnhandledException(exception_pointers);
    if core::ptr::read_volatile(core::ptr::addr_of!(DEBUGGER_WAS_PRESENT)) == 0 {
        __crt_debugger_hook(1);
    }
    __crtTerminateProcess(0xc000_0409); // STATUS_STACK_BUFFER_OVERRUN
}

/// Whether an address is the base of a valid PE image.
unsafe extern "C" fn validate_image_base(base: *const u8) -> i32 {
    if (base as *const u16).read_unaligned() != 0x5a4d {
        return 0;
    }
    let nt = base.offset(((base.add(0x3c)) as *const i32).read_unaligned() as isize);
    if (nt as *const u32).read_unaligned() != 0x4550 {
        return 0;
    }
    #[cfg(target_pointer_width = "64")]
    const MAGIC: u16 = 0x20b;
    #[cfg(target_pointer_width = "32")]
    const MAGIC: u16 = 0x10b;
    ((nt.add(0x18) as *const u16).read_unaligned() == MAGIC) as i32
}

/// The section of a PE image that holds an RVA.
unsafe extern "C" fn find_pe_section(base: *const u8, rva: usize) -> *const u8 {
    let nt = base.offset(((base.add(0x3c)) as *const i32).read_unaligned() as isize);
    let count = (nt.add(6) as *const u16).read_unaligned() as u32;
    let mut section = nt.add(0x18 + (nt.add(0x14) as *const u16).read_unaligned() as usize);
    let mut i = 0u32;
    while i < count {
        let va = (section.add(0xc) as *const u32).read_unaligned() as usize;
        if rva >= va && rva < ((section.add(8) as *const u32).read_unaligned()).wrapping_add(va as u32) as usize {
            return section;
        }
        i += 1;
        section = section.add(0x28);
    }
    core::ptr::null()
}

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    include_str!("crt_x86_64.s"),
    dll_main = sym crate::dll_main_entry,
    crt_init = sym crt_init,
    init_cookie = sym security_init_cookie,
    raise_securityfailure = sym raise_securityfailure,
    validate_image_base = sym validate_image_base,
    find_pe_section = sym find_pe_section,
    cookie = sym SECURITY_COOKIE,
    cookie_complement = sym SECURITY_COOKIE_COMPLEMENT,
    dllmain_reason = sym NATIVE_DLLMAIN_REASON,
    proc_attached = sym PROC_ATTACHED,
    onexitbegin = sym ONEXITBEGIN,
    onexitend = sym ONEXITEND,
    raw_dll_main = sym RAW_DLL_MAIN,
    gs_record = sym GS_EXCEPTION_RECORD,
    gs_context = sym GS_CONTEXT_RECORD,
);

#[cfg(target_arch = "x86")]
core::arch::global_asm!(
    include_str!("crt_x86.s"),
    dll_main = sym crate::dll_main_entry,
    crt_init = sym crt_init,
    init_cookie = sym security_init_cookie,
    raise_securityfailure = sym raise_securityfailure,
    validate_image_base = sym validate_image_base,
    find_pe_section = sym find_pe_section,
    cookie = sym SECURITY_COOKIE,
    cookie_complement = sym SECURITY_COOKIE_COMPLEMENT,
    dllmain_reason = sym NATIVE_DLLMAIN_REASON,
    proc_attached = sym PROC_ATTACHED,
    onexitbegin = sym ONEXITBEGIN,
    onexitend = sym ONEXITEND,
    raw_dll_main = sym RAW_DLL_MAIN,
    gs_record = sym GS_EXCEPTION_RECORD,
    gs_context = sym GS_CONTEXT_RECORD,
);

// The import tables (directory, lookup tables, hint/names, IAT)
// and the `jmp [IAT]` thunks, written out so that the image needs no mingw-w64
// import libraries.
#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(include_str!("imports_x86_64.s"));
#[cfg(target_arch = "x86")]
core::arch::global_asm!(include_str!("imports_x86.s"));

// The .rsrc section (the RT_MANIFEST resource), see rsrc.s.
core::arch::global_asm!(
    include_str!("rsrc.s"),
    concat!(".incbin \"", env!("CARGO_MANIFEST_DIR"), "/res/ets2-telemetry.manifest\""),
    ".Lmanifest_end:",
    ".p2align 2",
);
