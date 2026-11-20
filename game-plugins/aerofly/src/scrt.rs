//! The DLL's entry point and its start-up / unload sequence
//! (Windows only). The DLL links no C run-time start files: this module
//! is the image's entry point.
//!
//! Load (DLL_PROCESS_ATTACH), in order:
//! 1. `dll_entry`: seeds the security cookie.
//! 2. `dllmain_dispatch` -> `crt_dispatch` -> `crt_process_attach`:
//!    start-up state check, the type-info list head, the stdio option
//!    flags, the kernel32 function lookup, the nine initialisers of
//!    `INITTERM` in order, state = initialized,
//!    the attach count += 1.
//! 3. the user DllMain (`win::dll_main`).
//!
//! Unload (DLL_PROCESS_DETACH): user DllMain, then
//! `crt_process_detach`: the attach count -= 1,
//! the run of the atexit list in reverse registration order,
//! the flush of the type-info list (nothing on it to free), state = 0.
//!
//! There is no per-thread state, no argv or environment set-up and no
//! further stdio or locale initialisation: nothing in the DLL uses them.
#![allow(non_upper_case_globals)]

use core::ffi::c_void;
use core::ptr::{addr_of, addr_of_mut, null_mut};

use crate::crt::{
    EncodePointer, GetCurrentProcessId, GetCurrentThread, GetCurrentThreadId, GetSystemTimeAsFileTime,
    GetThreadTimes, InitializeSListHead, InterlockedFlushSList, IsProcessorFeaturePresent, QueryPerformanceCounter,
    PF_FASTFAIL_AVAILABLE,
};

const DLL_PROCESS_DETACH: u32 = 0;
const DLL_PROCESS_ATTACH: u32 = 1;
const DLL_THREAD_ATTACH: u32 = 2;
const DLL_THREAD_DETACH: u32 = 3;

// ---- security cookie (complement, then cookie) ---------------------------

/// The default security cookie (x64).
pub const DEFAULT_SECURITY_COOKIE: u64 = 0x0000_2B99_2DDF_A232;

/// The two statics (complement first). The load
/// configuration's `SecurityCookie` field points at `cookie`, so a loader
/// that initialises security cookies does it here.
#[repr(C)]
pub struct SecurityCookie {
    pub complement: u64,
    pub cookie: u64,
}

pub static mut __security_cookie_pair: SecurityCookie =
    SecurityCookie { complement: !DEFAULT_SECURITY_COOKIE, cookie: DEFAULT_SECURITY_COOKIE };

/// Seeds the security cookie.
#[inline(never)]
unsafe fn security_init_cookie() {
    let c = &mut *addr_of_mut!(__security_cookie_pair);
    if core::ptr::read_volatile(&c.cookie) == DEFAULT_SECURITY_COOKIE {
        let mut time: u64 = 0;
        GetSystemTimeAsFileTime(&mut time);
        let mut mixed: u64 = time;
        mixed ^= GetCurrentThreadId() as u64;
        mixed ^= GetCurrentProcessId() as u64;
        let mixed_at: *mut u64 = &mut mixed;
        core::ptr::write_volatile(mixed_at, mixed);
        let mut perf: i64 = 0;
        QueryPerformanceCounter(&mut perf);
        let perf = perf as u64;
        let mut cookie =
            (((perf & 0xFFFF_FFFF) << 32) ^ perf ^ core::ptr::read_volatile(mixed_at) ^ mixed_at as u64)
                & 0x0000_FFFF_FFFF_FFFF;
        if cookie == DEFAULT_SECURITY_COOKIE {
            cookie = DEFAULT_SECURITY_COOKIE + 1;
        }
        core::ptr::write_volatile(&mut c.cookie, cookie);
    }
    core::ptr::write_volatile(&mut c.complement, !core::ptr::read_volatile(&c.cookie));
}

// ---- concurrency / stdio statics touched by the initialiser table -------

/// The concurrency cookie: its flag and its value.
#[repr(C)]
pub struct ConcrtSecurity {
    pub initialized: u32,
    pub _pad: u32,
    pub cookie: u64,
}
pub static mut CONCRT_SECURITY: ConcrtSecurity = ConcrtSecurity { initialized: 0, _pad: 0, cookie: 0 };

/// 16-byte aligned SLIST_HEADER
#[repr(C, align(16))]
pub struct SListHeader(pub [u8; 16]);
/// The concurrency list head, initialised by `concrt_initialize_slist`.
pub static mut CONCRT_SLIST: SListHeader = SListHeader([0; 16]);
/// The type-info list head.
pub static mut TYPE_INFO_ROOT: SListHeader = SListHeader([0; 16]);

/// The default waiter pool pointer. Only waits
/// create it, and nothing in the DLL waits, so it stays NULL.
pub static mut DEFAULT_WAITER_POOL: *mut c_void = null_mut();

/// One stdio stream (0x58 bytes): pointer, base, count,
/// flags (+0x14), file (+0x18), character buffer, buffer size, temporary
/// file name, the stream lock (+0x30, 0x28 bytes).
#[repr(C)]
pub struct StdioStream {
    pub ptr: *mut u8,
    pub base: *mut u8,
    pub cnt: i32,
    pub flag: u32,
    pub file: i32,
    pub charbuf: i32,
    pub bufsiz: i32,
    pub _pad: u32,
    pub tmpfname: *mut u8,
    pub lock: [u8; 0x28],
}
const fn stream(flag: u32, file: i32) -> StdioStream {
    StdioStream {
        ptr: null_mut(),
        base: null_mut(),
        cnt: 0,
        flag,
        file,
        charbuf: 0,
        bufsiz: 0,
        _pad: 0,
        tmpfname: null_mut(),
        lock: [0; 0x28],
    }
}
/// The three standard streams (flags
/// 0x2001 / 0x2002 / 0x2402, files 0 / 1 / 2).
pub static mut IOB: [StdioStream; 3] = [stream(0x2001, 0), stream(0x2002, 1), stream(0x2402, 2)];
/// The stderr stream pointer stored by `store_stderr_stream`.
/// Nothing reads it.
pub static mut STDERR_STREAM: *mut StdioStream = null_mut();

/// The printf option word and the scanf option word.
pub static mut PRINTF_OPTIONS: u64 = 0;
pub static mut SCANF_OPTIONS: u64 = 0;

// ---- the initialisers outside the SDK code -------------------------------

/// Initialises the concurrency cookie.
unsafe fn concrt_initialize_cookie() {
    let s = &mut *addr_of_mut!(CONCRT_SECURITY);
    core::ptr::write_volatile(&mut s.initialized, 1);
    let cookie_at = addr_of_mut!(s.cookie) as *mut c_void;
    let mut v = EncodePointer(cookie_at) as u64 ^ core::ptr::read_volatile(addr_of!(__security_cookie_pair.cookie));
    let thread = GetCurrentThread();
    let mut creation: u64 = 0;
    let mut other: u64 = 0;
    if GetThreadTimes(thread, &mut creation, &mut other, &mut other, &mut other) != 0 {
        v ^= creation;
    }
    core::ptr::write_volatile(&mut s.cookie, v);
}

/// Initialises the concurrency list head.
unsafe fn concrt_initialize_slist() {
    InitializeSListHead(addr_of_mut!(CONCRT_SLIST) as *mut c_void);
}

/// Registers the waiter pool destructor for exit.
unsafe fn concrt_register_waiter_pool_dtor() {
    atexit(default_waiter_pool_dtor);
}

/// The waiter pool destructor:
/// does nothing while the pool pointer is NULL, which it always is here.
unsafe fn default_waiter_pool_dtor() {
    if !core::ptr::read_volatile(addr_of!(DEFAULT_WAITER_POOL)).is_null() {
        // unreachable: nothing in the DLL creates the pool
        core::arch::asm!("ud2", options(noreturn));
    }
}

/// Stores the stderr stream pointer.
unsafe fn store_stderr_stream() {
    core::ptr::write_volatile(addr_of_mut!(STDERR_STREAM), (addr_of_mut!(IOB) as *mut StdioStream).add(2));
}

unsafe fn sdk_message_vector() {
    atexit(exit_message_vector);
}
unsafe fn sdk_unused_vector_258() {
    atexit(exit_unused_vector_258);
}
unsafe fn sdk_unused_vector_240() {
    atexit(exit_unused_vector_240);
}
unsafe fn sdk_mutex() {
    crate::std_mutex::construct();
    atexit(exit_mutex);
}
unsafe fn sdk_name_table() {
    crate::sdk_table::construct();
    atexit(exit_name_table);
}
unsafe fn exit_message_vector() {
    crate::win::MESSAGES.get().destroy();
}
unsafe fn exit_unused_vector_258() {
    crate::win::UNUSED_VECTOR_258.get().destroy();
}
unsafe fn exit_unused_vector_240() {
    crate::win::UNUSED_VECTOR_240.get().destroy();
}
unsafe fn exit_mutex() {
    crate::std_mutex::destroy();
}
unsafe fn exit_name_table() {
    crate::sdk_table::destroy();
}

type Pvf = unsafe fn();

/// The initialiser table, in order.
static INITTERM: [Pvf; 9] = [
    concrt_initialize_cookie,
    concrt_initialize_slist,
    sdk_message_vector,
    sdk_unused_vector_258,
    sdk_unused_vector_240,
    sdk_mutex,
    sdk_name_table,
    concrt_register_waiter_pool_dtor,
    store_stderr_stream,
];

// ---- atexit list ----------------------------------------------------------

/// The registered exit handlers, in registration order.
static mut ATEXIT: [Option<Pvf>; 6] = [None; 6];
static mut ATEXIT_COUNT: usize = 0;

unsafe fn atexit(f: Pvf) {
    let list = &mut *addr_of_mut!(ATEXIT);
    let n = &mut *addr_of_mut!(ATEXIT_COUNT);
    list[*n] = Some(f);
    *n += 1;
}

/// Runs the exit handlers: last registered first; each entry
/// is taken off the list before it runs.
unsafe fn run_atexit() {
    let list = &mut *addr_of_mut!(ATEXIT);
    let n = &mut *addr_of_mut!(ATEXIT_COUNT);
    while *n > 0 {
        *n -= 1;
        if let Some(f) = list[*n].take() {
            f();
        }
    }
}

// ---- dllmain_dispatch -----------------------------------------------------

/// The start-up state: 0 uninitialized,
/// 1 initializing, 2 initialized.
static mut STARTUP_STATE: u32 = 0;
/// The count of process attaches.
static mut PROC_ATTACHED: i32 = 0;

/// Fast fail (FAST_FAIL_FATAL_APP_EXIT) on an inconsistent start-up
/// state. The loader serialises DllMain calls, so it cannot happen.
unsafe fn scrt_fastfail_7() -> ! {
    if IsProcessorFeaturePresent(PF_FASTFAIL_AVAILABLE) != 0 {
        core::arch::asm!("int 0x29", in("ecx") 7u32, options(noreturn));
    }
    loop {
        core::arch::asm!("ud2");
    }
}

/// The run-time part of DLL_PROCESS_ATTACH.
unsafe fn crt_process_attach() -> bool {
    // The heap handle comes first; when it fails the
    // attach returns 0 before the start-up state is touched.
    if !crate::msvc_heap::initialize_heap() {
        return false;
    }
    if core::ptr::read_volatile(addr_of!(STARTUP_STATE)) != 0 {
        scrt_fastfail_7();
    }
    core::ptr::write_volatile(addr_of_mut!(STARTUP_STATE), 1);
    // the type-info list head
    InitializeSListHead(addr_of_mut!(TYPE_INFO_ROOT) as *mut c_void);
    // the default stdio options
    core::ptr::write_volatile(addr_of_mut!(PRINTF_OPTIONS), core::ptr::read_volatile(addr_of!(PRINTF_OPTIONS)) | 0x24);
    core::ptr::write_volatile(addr_of_mut!(SCANF_OPTIONS), core::ptr::read_volatile(addr_of!(SCANF_OPTIONS)) | 2);
    // the initialiser that can fail: the kernel32 function lookup
    if crate::crt::resolve_msvcp_api() != 0 {
        return false;
    }
    // the initialiser table
    for f in INITTERM.iter() {
        f();
    }
    core::ptr::write_volatile(addr_of_mut!(STARTUP_STATE), 2);
    // there is no TLS callback to run
    core::ptr::write_volatile(addr_of_mut!(PROC_ATTACHED), core::ptr::read_volatile(addr_of!(PROC_ATTACHED)) + 1);
    true
}

/// The run-time part of DLL_PROCESS_DETACH.
unsafe fn crt_process_detach(_is_terminating: bool) -> bool {
    if core::ptr::read_volatile(addr_of!(PROC_ATTACHED)) < 1 {
        return false;
    }
    core::ptr::write_volatile(addr_of_mut!(PROC_ATTACHED), core::ptr::read_volatile(addr_of!(PROC_ATTACHED)) - 1);
    if core::ptr::read_volatile(addr_of!(STARTUP_STATE)) != 2 {
        scrt_fastfail_7();
    }
    // the exit handlers
    run_atexit();
    // the type-info list: nothing was ever pushed
    // (type_info::name is never called), so the flushed list is empty
    let _ = InterlockedFlushSList(addr_of_mut!(TYPE_INFO_ROOT) as *mut c_void);
    core::ptr::write_volatile(addr_of_mut!(STARTUP_STATE), 0);
    true
}

/// The run-time part of a DllMain call, by reason.
unsafe fn crt_dispatch(reason: u32, reserved: *mut c_void) -> bool {
    match reason {
        DLL_PROCESS_DETACH => crt_process_detach(!reserved.is_null()),
        DLL_PROCESS_ATTACH => crt_process_attach(),
        // thread attach / detach: there is no per-thread data
        DLL_THREAD_ATTACH | DLL_THREAD_DETACH => true,
        _ => true,
    }
}

/// Orders the run-time part and the user DllMain for each reason.
unsafe fn dllmain_dispatch(instance: *mut c_void, reason: u32, reserved: *mut c_void) -> i32 {
    if reason == DLL_PROCESS_DETACH && core::ptr::read_volatile(addr_of!(PROC_ATTACHED)) < 1 {
        return 0;
    }
    if (reason == DLL_PROCESS_ATTACH || reason == DLL_THREAD_ATTACH) && !crt_dispatch(reason, reserved) {
        return 0;
    }
    let mut result = crate::win::dll_main(instance, reason, reserved);
    if reason == DLL_PROCESS_ATTACH && result == 0 {
        crate::win::dll_main(instance, DLL_PROCESS_DETACH, reserved);
        crt_dispatch(DLL_PROCESS_DETACH, reserved);
    }
    if reason == DLL_PROCESS_DETACH || reason == DLL_THREAD_DETACH {
        result = crt_dispatch(reason, reserved) as i32;
    }
    result
}

/// The body of the image entry point.
unsafe extern "system" fn dll_entry(instance: *mut c_void, reason: u32, reserved: *mut c_void) -> i32 {
    if reason == DLL_PROCESS_ATTACH {
        security_init_cookie();
    }
    dllmain_dispatch(instance, reason, reserved)
}

// ---- image entry point, load configuration, guard pointers, mem* ---------
//
// `DllMainCRTStartup` is the linker's default DLL entry symbol; being
// defined in assembly it is not in the export table (which stays the four
// Aerofly entry points). memcpy/memset/strlen are statically linked, so the
// image imports nothing from a C run-time DLL. They are leaf functions that
// never move rsp (rdi/rsi are kept in r10/r11), so the x64 unwinder needs no
// .pdata entry for them (a fault inside one unwinds as from a leaf).
//
// `_load_config_used` (IMAGE_LOAD_CONFIG_DIRECTORY64, 0x130 bytes):
// Size 0x130, SecurityCookie, the CFG check
// and dispatch pointer slots, GuardFlags 0x100 (CF_INSTRUMENTED) and the
// three XFG pointer slots; every other field 0. The
// image's DllCharacteristics do not have GUARD_CF, so the loader leaves the
// slots as they are: the check slots hold the `ret` that is also the
// Shutdown export, the dispatch slot a
// `jmp rax`, the XFG dispatch slots a jump through it.
core::arch::global_asm!(
    ".text",
    ".globl DllMainCRTStartup",
    ".p2align 4",
    "DllMainCRTStartup:",
    "jmp {entry}",
    ".p2align 4",
    "__guard_dispatch_icall_nop:",
    "jmp rax",
    ".p2align 4",
    "__guard_xfg_dispatch_icall_nop:",
    "jmp qword ptr [rip + __guard_dispatch_icall_fptr]",
    ".p2align 4",
    ".globl memcpy",
    "memcpy:",
    "mov rax, rcx",
    "mov r10, rdi",
    "mov r11, rsi",
    "mov rdi, rcx",
    "mov rsi, rdx",
    "mov rcx, r8",
    "rep movsb",
    "mov rsi, r11",
    "mov rdi, r10",
    "ret",
    ".p2align 4",
    ".globl memmove",
    "memmove:",
    "mov rax, rcx",
    "mov r10, rdi",
    "mov r11, rsi",
    "mov rdi, rcx",
    "mov rsi, rdx",
    "mov rcx, r8",
    "cmp rdi, rsi",
    "jbe 2f",
    "lea rsi, [rsi + rcx - 1]",
    "lea rdi, [rdi + rcx - 1]",
    "std",
    "rep movsb",
    "cld",
    "jmp 3f",
    "2:",
    "rep movsb",
    "3:",
    "mov rsi, r11",
    "mov rdi, r10",
    "ret",
    ".p2align 4",
    ".globl memset",
    "memset:",
    "mov r9, rcx",
    "mov r10, rdi",
    "mov rdi, rcx",
    "mov eax, edx",
    "mov rcx, r8",
    "rep stosb",
    "mov rdi, r10",
    "mov rax, r9",
    "ret",
    ".p2align 4",
    ".globl memcmp",
    "memcmp:",
    "xor eax, eax",
    "4:",
    "test r8, r8",
    "jz 5f",
    "movzx eax, byte ptr [rcx]",
    "movzx r9d, byte ptr [rdx]",
    "sub eax, r9d",
    "jnz 5f",
    "inc rcx",
    "inc rdx",
    "dec r8",
    "jmp 4b",
    "5:",
    "ret",
    ".p2align 4",
    ".globl strlen",
    "strlen:",
    "mov rax, rcx",
    "6:",
    "cmp byte ptr [rax], 0",
    "je 7f",
    "inc rax",
    "jmp 6b",
    "7:",
    "sub rax, rcx",
    "ret",
    ".section .rdata,\"dr\"",
    ".p2align 3",
    "__guard_check_icall_fptr:",
    ".quad {ret}",
    "__guard_xfg_check_icall_fptr:",
    ".quad {ret}",
    "__guard_dispatch_icall_fptr:",
    ".quad __guard_dispatch_icall_nop",
    "__guard_xfg_dispatch_icall_fptr:",
    ".quad __guard_xfg_dispatch_icall_nop",
    "__guard_xfg_table_dispatch_icall_fptr:",
    ".quad __guard_xfg_dispatch_icall_nop",
    ".p2align 4",
    ".globl _load_config_used",
    "_load_config_used:",
    ".long 0x130",                    // Size
    ".fill 0x54, 1, 0",               // 0x04..0x58
    ".quad {cookie} + 8",             // 0x58 SecurityCookie
    ".quad 0, 0",                     // 0x60 SEHandlerTable / Count
    ".quad __guard_check_icall_fptr", // 0x70 GuardCFCheckFunctionPointer
    ".quad __guard_dispatch_icall_fptr", // 0x78 GuardCFDispatchFunctionPointer
    ".quad 0, 0",                     // 0x80 GuardCFFunctionTable / Count
    ".long 0x100",                    // 0x90 GuardFlags
    ".fill 0x84, 1, 0",               // 0x94..0x118
    ".quad __guard_xfg_check_icall_fptr",          // 0x118
    ".quad __guard_xfg_dispatch_icall_fptr",       // 0x120
    ".quad __guard_xfg_table_dispatch_icall_fptr", // 0x128
    ".text",
    entry = sym dll_entry,
    ret = sym crate::exports::Aerofly_FS_2_External_DLL_Shutdown,
    cookie = sym __security_cookie_pair,
);
