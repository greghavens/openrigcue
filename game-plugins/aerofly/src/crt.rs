//! The few C run-time pieces that the
//! plugin's own code paths reach (Windows only):
//!
//! * KERNEL32 imports used by the heap path (malloc / free
//!   on the process heap), the std::mutex (`InitializeCriticalSectionAndSpinCount`,
//!   `DeleteCriticalSection`), the C++ throw (`RtlPcToFileHeader`,
//!   `RaiseException`) and the invalid-parameter failure.
//! * the 40 run-time resolved kernel32 functions
//!   (`GetModuleHandleW(L"kernel32.dll")` then 40
//!   `GetProcAddress`; std::mutex construction looks only at
//!   InitializeCriticalSectionEx, InitializeConditionVariable,
//!   InitializeSRWLock and TryAcquireSRWLockExclusive).
//! * the invalid-parameter failure: fast fail, or the fault report and
//!   process termination.
#![allow(non_snake_case, clippy::upper_case_acronyms)]

use core::ffi::c_void;
use core::ptr::null_mut;

pub type HANDLE = *mut c_void;

#[link(name = "KERNEL32", kind = "raw-dylib")]
extern "system" {
    pub fn GetProcessHeap() -> HANDLE;
    pub fn HeapAlloc(heap: HANDLE, flags: u32, bytes: usize) -> *mut c_void;
    pub fn HeapFree(heap: HANDLE, flags: u32, mem: *mut c_void) -> i32;
    pub fn GetModuleHandleW(name: *const u16) -> HANDLE;
    pub fn GetProcAddress(module: HANDLE, name: *const u8) -> *mut c_void;
    pub fn InitializeCriticalSectionAndSpinCount(cs: *mut c_void, spin: u32) -> i32;
    pub fn DeleteCriticalSection(cs: *mut c_void);
    pub fn RtlPcToFileHeader(pc: *const c_void, base: *mut *mut c_void) -> *mut c_void;
    pub fn IsProcessorFeaturePresent(feature: u32) -> i32;
    pub fn RtlCaptureContext(context: *mut c_void);
    pub fn RtlLookupFunctionEntry(pc: u64, image_base: *mut u64, history: *mut c_void) -> *mut c_void;
    pub fn RtlVirtualUnwind(
        kind: u32,
        image_base: u64,
        pc: u64,
        entry: *mut c_void,
        context: *mut c_void,
        handler_data: *mut *mut c_void,
        establisher: *mut u64,
        pointers: *mut c_void,
    ) -> *mut c_void;
    pub fn IsDebuggerPresent() -> i32;
    pub fn SetUnhandledExceptionFilter(filter: *mut c_void) -> *mut c_void;
    pub fn UnhandledExceptionFilter(pointers: *mut c_void) -> i32;
    pub fn GetCurrentProcess() -> HANDLE;
    pub fn TerminateProcess(process: HANDLE, code: u32) -> i32;
    // start-up / unload (scrt.rs)
    pub fn GetSystemTimeAsFileTime(ft: *mut u64);
    pub fn GetCurrentThreadId() -> u32;
    pub fn GetCurrentProcessId() -> u32;
    pub fn QueryPerformanceCounter(count: *mut i64) -> i32;
    pub fn EncodePointer(p: *mut c_void) -> *mut c_void;
    pub fn GetCurrentThread() -> HANDLE;
    pub fn GetThreadTimes(thread: HANDLE, creation: *mut u64, exit: *mut u64, kernel: *mut u64, user: *mut u64) -> i32;
    pub fn InitializeSListHead(head: *mut c_void);
    pub fn InterlockedFlushSList(head: *mut c_void) -> *mut c_void;
}

#[link(name = "KERNEL32", kind = "raw-dylib")]
extern "system" {
    /// Raises the C++ exception; when the game catches it, the OS unwinds
    /// through this DLL's frames. It is declared without "-unwind" on
    /// purpose: under panic=abort rustc wraps "-unwind" calls in an
    /// abort-on-unwind guard, which would end the game process where
    /// the exception has to travel on. Every function keeps its x64
    /// unwind data (.pdata/.xdata, `uwtable`) and none has a personality
    /// routine, so the system unwinder passes through them.
    pub fn RaiseException(code: u32, flags: u32, nargs: u32, args: *const usize);
}

// ---- run-time resolved kernel32 functions --------------------------------

pub type InitializeCriticalSectionExFn = unsafe extern "system" fn(*mut c_void, u32, u32) -> i32;
pub type InitializeSRWLockFn = unsafe extern "system" fn(*mut c_void);

/// The 40 names looked up at start-up. Their pointers land
/// in `MSVCP_API` (one each, in this order, stored as
/// GetProcAddress returned them, not encoded).
pub const MSVCP_API_NAMES: [&[u8]; 40] = [
    b"FlsAlloc\0",
    b"FlsFree\0",
    b"FlsGetValue\0",
    b"FlsSetValue\0",
    b"InitializeCriticalSectionEx\0",
    b"InitOnceExecuteOnce\0",
    b"CreateEventExW\0",
    b"CreateSemaphoreW\0",
    b"CreateSemaphoreExW\0",
    b"CreateThreadpoolTimer\0",
    b"SetThreadpoolTimer\0",
    b"WaitForThreadpoolTimerCallbacks\0",
    b"CloseThreadpoolTimer\0",
    b"CreateThreadpoolWait\0",
    b"SetThreadpoolWait\0",
    b"CloseThreadpoolWait\0",
    b"FlushProcessWriteBuffers\0",
    b"FreeLibraryWhenCallbackReturns\0",
    b"GetCurrentProcessorNumber\0",
    b"CreateSymbolicLinkW\0",
    b"GetCurrentPackageId\0",
    b"GetTickCount64\0",
    b"GetFileInformationByHandleEx\0",
    b"SetFileInformationByHandle\0",
    b"GetSystemTimePreciseAsFileTime\0",
    b"InitializeConditionVariable\0",
    b"WakeConditionVariable\0",
    b"WakeAllConditionVariable\0",
    b"SleepConditionVariableCS\0",
    b"InitializeSRWLock\0",
    b"AcquireSRWLockExclusive\0",
    b"TryAcquireSRWLockExclusive\0",
    b"ReleaseSRWLockExclusive\0",
    b"SleepConditionVariableSRW\0",
    b"CreateThreadpoolWork\0",
    b"SubmitThreadpoolWork\0",
    b"CloseThreadpoolWork\0",
    b"CompareStringEx\0",
    b"GetLocaleInfoEx\0",
    b"LCMapStringEx\0",
];
const IX_INITIALIZE_CRITICAL_SECTION_EX: usize = 4;
const IX_INITIALIZE_CONDITION_VARIABLE: usize = 25;
const IX_INITIALIZE_SRW_LOCK: usize = 29;
const IX_TRY_ACQUIRE_SRW_LOCK_EXCLUSIVE: usize = 31;

/// The 40 pointers, written once by
/// `resolve_msvcp_api` before the C++ initialisers, read-only afterwards.
/// Only the four std::mutex construction reads are ever used in this DLL.
pub static mut MSVCP_API: [*mut c_void; 40] = [null_mut(); 40];

/// The four pointers std::mutex construction tests.
pub struct SyncApi {
    pub initialize_critical_section_ex: *mut c_void,
    pub initialize_condition_variable: *mut c_void,
    pub initialize_srw_lock: *mut c_void,
    pub try_acquire_srw_lock_exclusive: *mut c_void,
}

pub unsafe fn sync_api() -> SyncApi {
    let t = &*core::ptr::addr_of!(MSVCP_API);
    SyncApi {
        initialize_critical_section_ex: t[IX_INITIALIZE_CRITICAL_SECTION_EX],
        initialize_condition_variable: t[IX_INITIALIZE_CONDITION_VARIABLE],
        initialize_srw_lock: t[IX_INITIALIZE_SRW_LOCK],
        try_acquire_srw_lock_exclusive: t[IX_TRY_ACQUIRE_SRW_LOCK_EXCLUSIVE],
    }
}

/// L"kernel32.dll"
static KERNEL32_W: [u16; 13] = [
    b'k' as u16, b'e' as u16, b'r' as u16, b'n' as u16, b'e' as u16, b'l' as u16, b'3' as u16, b'2' as u16,
    b'.' as u16, b'd' as u16, b'l' as u16, b'l' as u16, 0,
];

/// The first C initialiser: GetModuleHandleW(L"kernel32.dll")
/// and GetProcAddress of the 40 names in order. The module
/// handle is not checked. Returns 0 (success for
/// the initialiser run).
pub unsafe fn resolve_msvcp_api() -> i32 {
    let k = GetModuleHandleW(KERNEL32_W.as_ptr());
    let t = &mut *core::ptr::addr_of_mut!(MSVCP_API);
    for (slot, name) in t.iter_mut().zip(MSVCP_API_NAMES.iter()) {
        *slot = GetProcAddress(k, name.as_ptr());
    }
    0
}

// ---- invalid-parameter failure -------------------------------------------

/// `STATUS_INVALID_CRUNTIME_PARAMETER`
pub const STATUS_INVALID_CRUNTIME_PARAMETER: u32 = 0xC000_0417;
/// `PF_FASTFAIL_AVAILABLE`
pub const PF_FASTFAIL_AVAILABLE: u32 = 0x17;
/// `FAST_FAIL_INVALID_ARG`
pub const FAST_FAIL_INVALID_ARG: u32 = 5;
/// `EXCEPTION_NONCONTINUABLE`
pub const EXCEPTION_NONCONTINUABLE: u32 = 1;

/// The invalid-parameter failure. The DLL never installs an invalid-parameter
/// handler, so this always goes on to `invoke_watson`.
pub unsafe fn invalid_parameter_noinfo_noreturn() -> ! {
    invoke_watson()
}

/// Ends the process: fast fail when available, else the fault report and termination.
unsafe fn invoke_watson() -> ! {
    if IsProcessorFeaturePresent(PF_FASTFAIL_AVAILABLE) != 0 {
        core::arch::asm!("int 0x29", in("ecx") FAST_FAIL_INVALID_ARG, options(noreturn));
    }
    call_reportfault(STATUS_INVALID_CRUNTIME_PARAMETER, EXCEPTION_NONCONTINUABLE);
    TerminateProcess(GetCurrentProcess(), STATUS_INVALID_CRUNTIME_PARAMETER);
    loop {
        core::arch::asm!("int3");
    }
}

#[repr(C, align(16))]
struct Context([u8; 0x4d0]);
#[repr(C, align(8))]
struct ExceptionRecord([u8; 0x98]);

/// CONTEXT.Rip / CONTEXT.Rsp offsets (x64).
const CONTEXT_RIP: usize = 0xf8;
const CONTEXT_RSP: usize = 0x98;
/// EXCEPTION_RECORD.ExceptionAddress offset (x64).
const EXCEPTION_ADDRESS: usize = 0x10;

// The fault report is entered through this tail-jump so the body
// gets the address of its own return address (its entry rsp)
// as a third argument; the jump keeps
// rsp, so the body returns straight to `invoke_watson`.
core::arch::global_asm!(
    ".text",
    ".p2align 4",
    "aerofly_call_reportfault:",
    "mov r8, rsp",
    "jmp {body}",
    body = sym call_reportfault_body,
);

extern "C" {
    fn aerofly_call_reportfault(code: u32, flags: u32);
}

#[inline(always)]
unsafe fn call_reportfault(code: u32, flags: u32) {
    aerofly_call_reportfault(code, flags)
}

/// The fault report: the caller's
/// context (RtlCaptureContext, then one RtlVirtualUnwind when a function
/// entry is found), then unconditionally CONTEXT.Rip = the return address,
/// CONTEXT.Rsp = address of the return address + 8, and an exception
/// record with ExceptionCode, ExceptionFlags and ExceptionAddress (= the
/// return address) set, offered to UnhandledExceptionFilter after the filter
/// has been reset.
#[inline(never)]
unsafe extern "C" fn call_reportfault_body(code: u32, flags: u32, ret_slot: *const u64) {
    let mut rec = ExceptionRecord([0u8; 0x98]);
    let mut ctx = Context([0u8; 0x4d0]);
    let mut ptrs: [*mut c_void; 2] = [rec.0.as_mut_ptr() as *mut c_void, ctx.0.as_mut_ptr() as *mut c_void];
    RtlCaptureContext(ctx.0.as_mut_ptr() as *mut c_void);
    let rip = u64::from_le_bytes(ctx.0[CONTEXT_RIP..CONTEXT_RIP + 8].try_into().unwrap_or([0; 8]));
    let mut image_base = 0u64;
    let entry = RtlLookupFunctionEntry(rip, &mut image_base, null_mut());
    if !entry.is_null() {
        let mut handler_data: *mut c_void = null_mut();
        let mut establisher = 0u64;
        RtlVirtualUnwind(
            0,
            image_base,
            rip,
            entry,
            ctx.0.as_mut_ptr() as *mut c_void,
            &mut handler_data,
            &mut establisher,
            null_mut(),
        );
    }
    let ret = core::ptr::read(ret_slot);
    let rsp = ret_slot as u64 + 8;
    ctx.0[CONTEXT_RIP..CONTEXT_RIP + 8].copy_from_slice(&ret.to_le_bytes());
    rec.0[0..4].copy_from_slice(&code.to_le_bytes());
    ctx.0[CONTEXT_RSP..CONTEXT_RSP + 8].copy_from_slice(&rsp.to_le_bytes());
    rec.0[EXCEPTION_ADDRESS..EXCEPTION_ADDRESS + 8].copy_from_slice(&ret.to_le_bytes());
    rec.0[4..8].copy_from_slice(&flags.to_le_bytes());
    let _debugger = IsDebuggerPresent();
    SetUnhandledExceptionFilter(null_mut());
    let _ = UnhandledExceptionFilter(ptrs.as_mut_ptr() as *mut c_void);
}
