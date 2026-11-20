//! The C run-time pieces that are linked into the DLL itself. The
//! DLL imports only KERNEL32, so
//! it is linked without any C runtime (`-nostdlib`, entry point
//! `DllMain`) and carries these:
//!
//! * `memset` (the only C string routine the plugin code
//!   calls) and the processor-feature flags it uses; `memcpy`,
//!   `memmove`, `memcmp`, `strlen` for the code LLVM / `core` generate;
//! * the heap (GetProcessHeap at attach), `malloc`,
//!   `free`, `operator new` with the
//!   `std::bad_alloc` throw;
//! * `_purecall` -> `abort` ->
//!   the fault report / `_exit(3)`;
//! * the exit lock and the
//!   function / module caches it
//!   needs (InitializeCriticalSectionEx, GetCurrentPackageId).

use crate::rtti;
use core::arch::{asm, global_asm};
use core::ffi::c_void;

#[link(name = "kernel32")]
extern "system" {
    fn GetProcessHeap() -> *mut c_void;
    fn HeapAlloc(h: *mut c_void, flags: u32, n: usize) -> *mut c_void;
    fn HeapFree(h: *mut c_void, flags: u32, p: *mut c_void) -> i32;
    fn GetLastError() -> u32;
    fn IsProcessorFeaturePresent(f: u32) -> i32;
    fn IsDebuggerPresent() -> i32;
    fn SetUnhandledExceptionFilter(f: *const c_void) -> *const c_void;
    fn UnhandledExceptionFilter(p: *mut c_void) -> i32;
    fn GetModuleHandleW(n: *const u16) -> *mut u8;
    fn GetModuleHandleExW(flags: u32, n: *const u16, h: *mut *mut c_void) -> i32;
    fn GetProcAddress(h: *mut c_void, n: *const u8) -> *const c_void;
    fn FreeLibrary(h: *mut c_void) -> i32;
    fn LoadLibraryExW(n: *const u16, f: *mut c_void, flags: u32) -> *mut c_void;
    fn InitializeCriticalSectionAndSpinCount(cs: *mut c_void, spin: u32) -> i32;
    fn EnterCriticalSection(cs: *mut c_void);
    fn LeaveCriticalSection(cs: *mut c_void);
    fn DeleteCriticalSection(cs: *mut c_void);
    fn GetCurrentProcess() -> *mut c_void;
    fn TerminateProcess(h: *mut c_void, code: u32) -> i32;
    fn ExitProcess(code: u32) -> !;
    fn RaiseException(code: u32, flags: u32, n: u32, args: *const usize);
    fn GetSystemTimeAsFileTime(ft: *mut u32);
    fn GetCurrentThreadId() -> u32;
    fn GetCurrentProcessId() -> u32;
    fn QueryPerformanceCounter(v: *mut i64) -> i32;
}

// ---------------------------------------------------------------------------
// Processor features
// ---------------------------------------------------------------------------

/// The available instruction sets (initial value 1).
static mut ISA_AVAILABLE: u32 = 1;
/// The enabled instruction-set level.
static mut ISA_LEVEL: u32 = 0;
/// Processor preferences: bit 0 Atom, bit 1 ERMS (`rep stosb` preferred).
static mut FAVOR: u32 = 0;

/// Detects the processor features (DLL_PROCESS_ATTACH).
unsafe fn isa_available_init() {
    use core::arch::x86::{__cpuid, __cpuid_count};
    ISA_LEVEL = 0;
    ISA_AVAILABLE |= 1;
    if IsProcessorFeaturePresent(10) == 0 {
        return;
    }
    let saved = ISA_AVAILABLE;
    let mut ebx7 = 0u32;
    ISA_AVAILABLE |= 2;
    ISA_LEVEL = 1;
    let l0 = __cpuid(0);
    let l1 = __cpuid(1);
    let ecx1 = l1.ecx;
    if l0.ecx == 0x6c65_746e && l0.edx == 0x4965_6e69 && l0.ebx == 0x756e_6547 {
        let m = l1.eax & 0x0fff_3ff0;
        if m == 0x106c0 || m == 0x20660 || m == 0x20670 || m == 0x30650 || m == 0x30660 || m == 0x30670 {
            FAVOR |= 1;
        }
    }
    if l0.eax > 6 {
        ebx7 = __cpuid_count(7, 0).ebx;
        if ebx7 & 0x200 != 0 {
            FAVOR |= 2;
        }
    }
    if ecx1 & 0x10_0000 != 0 {
        ISA_AVAILABLE = saved | 6;
        ISA_LEVEL = 2;
        if ecx1 & 0x800_0000 != 0 && ecx1 & 0x1000_0000 != 0 {
            let lo: u32;
            asm!("xgetbv", in("ecx") 0u32, out("eax") lo, out("edx") _, options(nomem, nostack));
            if lo & 6 == 6 {
                ISA_AVAILABLE = saved | 0xe;
                ISA_LEVEL = 3;
                if ebx7 & 0x20 != 0 {
                    ISA_AVAILABLE = saved | 0x2e;
                    ISA_LEVEL = 5;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// memset, memcpy / memmove / memcmp
// ---------------------------------------------------------------------------

extern "C" {
    /// `memset`.
    pub fn rf1_memset(d: *mut u8, c: i32, n: usize) -> *mut u8;
}

global_asm!(
    ".text",
    ".globl _rf1_memset",
    ".globl _memset",
    ".p2align 4",
    "_rf1_memset:",
    "_memset:",
    "mov ecx, dword ptr [esp + 0xc]",
    "movzx eax, byte ptr [esp + 8]",
    "mov edx, edi",
    "mov edi, dword ptr [esp + 4]",
    "test ecx, ecx",
    "jz 90f",
    "imul eax, eax, 0x01010101",
    "cmp ecx, 0x20",
    "jle 70f",
    "cmp ecx, 0x80",
    "jl 50f",
    "bt dword ptr [{favor}], 1",
    "jae 20f",
    "rep stosb",
    "mov eax, dword ptr [esp + 4]",
    "mov edi, edx",
    "ret",
    "20:",
    "bt dword ptr [{isa}], 1",
    "jae 70f",
    "movd xmm0, eax",
    "pshufd xmm0, xmm0, 0",
    "add ecx, edi",
    "movups xmmword ptr [edi], xmm0",
    "add edi, 0x10",
    "and edi, -16",
    "sub ecx, edi",
    "cmp ecx, 0x80",
    "jle 50f",
    "30:",
    "movdqa xmmword ptr [edi], xmm0",
    "movdqa xmmword ptr [edi + 0x10], xmm0",
    "movdqa xmmword ptr [edi + 0x20], xmm0",
    "movdqa xmmword ptr [edi + 0x30], xmm0",
    "movdqa xmmword ptr [edi + 0x40], xmm0",
    "movdqa xmmword ptr [edi + 0x50], xmm0",
    "movdqa xmmword ptr [edi + 0x60], xmm0",
    "movdqa xmmword ptr [edi + 0x70], xmm0",
    "lea edi, [edi + 0x80]",
    "sub ecx, 0x80",
    "test ecx, 0xffffff00",
    "jnz 30b",
    "jmp 60f",
    "50:",
    "bt dword ptr [{isa}], 1",
    "jae 70f",
    "movd xmm0, eax",
    "pshufd xmm0, xmm0, 0",
    "60:",
    "cmp ecx, 0x20",
    "jb 65f",
    "61:",
    "movdqu xmmword ptr [edi], xmm0",
    "movdqu xmmword ptr [edi + 0x10], xmm0",
    "add edi, 0x20",
    "sub ecx, 0x20",
    "cmp ecx, 0x20",
    "jae 61b",
    "test ecx, 0x1f",
    "jz 90f",
    "65:",
    "lea edi, [ecx + edi - 0x20]",
    "movdqu xmmword ptr [edi], xmm0",
    "movdqu xmmword ptr [edi + 0x10], xmm0",
    "mov eax, dword ptr [esp + 4]",
    "mov edi, edx",
    "ret",
    "70:",
    "test ecx, 3",
    "jz 72f",
    "71:",
    "mov byte ptr [edi], al",
    "inc edi",
    "sub ecx, 1",
    "test ecx, 3",
    "jnz 71b",
    "72:",
    "test ecx, 4",
    "jz 73f",
    "mov dword ptr [edi], eax",
    "add edi, 4",
    "sub ecx, 4",
    "73:",
    "test ecx, 0xfffffff8",
    "jz 90f",
    "74:",
    "mov dword ptr [edi], eax",
    "mov dword ptr [edi + 4], eax",
    "add edi, 8",
    "sub ecx, 8",
    "test ecx, 0xfffffff8",
    "jnz 74b",
    "90:",
    "mov eax, dword ptr [esp + 4]",
    "mov edi, edx",
    "ret",
    // memcpy / memmove (rep movsb, backwards for an overlapping move)
    ".globl _memcpy",
    ".globl _memmove",
    ".p2align 4",
    "_memcpy:",
    "_memmove:",
    "push esi",
    "push edi",
    "mov edi, dword ptr [esp + 0xc]",
    "mov esi, dword ptr [esp + 0x10]",
    "mov ecx, dword ptr [esp + 0x14]",
    "mov eax, edi",
    "cmp edi, esi",
    "jbe 2f",
    "lea edx, [esi + ecx]",
    "cmp edi, edx",
    "jae 2f",
    "lea esi, [esi + ecx - 1]",
    "lea edi, [edi + ecx - 1]",
    "std",
    "rep movsb",
    "cld",
    "pop edi",
    "pop esi",
    "ret",
    "2:",
    "rep movsb",
    "pop edi",
    "pop esi",
    "ret",
    // strlen
    ".globl _strlen",
    ".p2align 4",
    "_strlen:",
    "mov edx, dword ptr [esp + 4]",
    "mov eax, edx",
    "5:",
    "cmp byte ptr [eax], 0",
    "je 6f",
    "inc eax",
    "jmp 5b",
    "6:",
    "sub eax, edx",
    "ret",
    // memcmp
    ".globl _memcmp",
    ".p2align 4",
    "_memcmp:",
    "push esi",
    "push edi",
    "mov esi, dword ptr [esp + 0xc]",
    "mov edi, dword ptr [esp + 0x10]",
    "mov ecx, dword ptr [esp + 0x14]",
    "xor eax, eax",
    "3:",
    "test ecx, ecx",
    "jz 4f",
    "movzx eax, byte ptr [esi]",
    "movzx edx, byte ptr [edi]",
    "sub eax, edx",
    "jnz 4f",
    "inc esi",
    "inc edi",
    "dec ecx",
    "jmp 3b",
    "4:",
    "pop edi",
    "pop esi",
    "ret",
    favor = sym FAVOR,
    isa = sym ISA_AVAILABLE,
);

// ---------------------------------------------------------------------------
// Heap: malloc, free, operator new / delete
// ---------------------------------------------------------------------------

/// The heap: GetProcessHeap() at attach.
static mut ACRT_HEAP: *mut c_void = core::ptr::null_mut();

/// `malloc`. A failed HeapAlloc is not retried.
pub unsafe fn malloc(n: usize) -> *mut c_void {
    if n < 0xffff_ffe1 {
        let n = if n == 0 { 1 } else { n };
        let p = HeapAlloc(ACRT_HEAP, 0, n);
        if !p.is_null() {
            return p;
        }
    }
    core::ptr::null_mut()
}

/// `free` (a failed HeapFree reads GetLastError).
pub unsafe fn free(p: *mut c_void) {
    if !p.is_null() && HeapFree(ACRT_HEAP, 0, p) == 0 {
        GetLastError();
    }
}

/// `operator new`: malloc; on failure (no new
/// handler is ever installed) throw `std::bad_alloc`.
/// There is no `bad_array_new_length` branch
/// (the only call is `new(0x20)` in Create).
pub unsafe fn operator_new(n: usize) -> *mut c_void {
    let p = malloc(n);
    if !p.is_null() {
        return p;
    }
    throw_bad_alloc()
}

/// `operator delete(p, size)` -> `free`.
pub unsafe fn operator_delete(p: *mut c_void) {
    free(p)
}

/// Throws `std::bad_alloc`: a local `bad_alloc("bad
/// allocation")` thrown as a C++ exception,
/// i.e. RaiseException(0xe06d7363, EXCEPTION_NONCONTINUABLE, 3,
/// {0x19930520, &obj, &ThrowInfo}).
#[inline(never)]
unsafe fn throw_bad_alloc() -> ! {
    let mut obj = rtti::StdException { vfptr: core::ptr::null(), data: rtti::ExceptionData { what: core::ptr::null(), do_free: 0, _pad: [0; 3] } };
    rtti::bad_alloc_ctor(&mut obj);
    let args: [usize; 3] = [0x1993_0520, &mut obj as *mut _ as usize, rtti::throw_info_bad_alloc() as usize];
    RaiseException(0xe06d_7363, 1, 3, args.as_ptr());
    asm!("int3", options(noreturn));
}

// ---------------------------------------------------------------------------
// Optional Windows API functions: module and function caches
// ---------------------------------------------------------------------------

/// The module cache (20 module ids): 0 = not tried,
/// usize::MAX = unavailable.
static mut MODULES: [usize; 20] = [0; 20];
/// The function cache, by function id: 0 = not tried,
/// usize::MAX = unavailable.
static mut FUNCTIONS: [usize; 0x18] = [0; 0x18];

const fn w<const N: usize>(s: &[u8; N]) -> [u16; N] {
    let mut o = [0u16; N];
    let mut i = 0;
    while i < N {
        o[i] = s[i] as u16;
        i += 1;
    }
    o
}
/// Module names of the ids used.
static MOD0: [u16; 35] = w(b"api-ms-win-appmodel-runtime-l1-1-1\0");
static MOD8: [u16; 29] = w(b"api-ms-win-core-synch-l1-2-0\0");
static MOD14: [u16; 43] = w(b"ext-ms-win-kernel32-package-current-l1-1-0\0");
static MOD18: [u16; 9] = w(b"kernel32\0");

fn module_name(id: usize) -> *const u16 {
    match id {
        0 => MOD0.as_ptr(),
        8 => MOD8.as_ptr(),
        14 => MOD14.as_ptr(),
        _ => MOD18.as_ptr(),
    }
}

/// The module of an id, loaded on first use and cached.
unsafe fn try_get_module(id: usize) -> *mut c_void {
    let slot = core::ptr::addr_of_mut!(MODULES[id]);
    let v = *slot;
    if v != 0 {
        return if v == usize::MAX { core::ptr::null_mut() } else { v as *mut c_void };
    }
    let name = module_name(id);
    let mut h = LoadLibraryExW(name, core::ptr::null_mut(), 0x800);
    if h.is_null() {
        if GetLastError() == 0x57 {
            h = LoadLibraryExW(name, core::ptr::null_mut(), 0);
        }
        if h.is_null() {
            core::ptr::write_volatile(slot, usize::MAX);
            return core::ptr::null_mut();
        }
    }
    let old = core::ptr::replace(slot, h as usize);
    if old != 0 {
        FreeLibrary(h);
    }
    h
}

/// A cached function: the first module of `ids` that loads is asked for `name`.
unsafe fn try_get_function(id: usize, name: &[u8], ids: &[usize]) -> *const c_void {
    let slot = core::ptr::addr_of_mut!(FUNCTIONS[id]);
    let v = *slot;
    if v == usize::MAX {
        return core::ptr::null();
    }
    if v != 0 {
        return v as *const c_void;
    }
    let mut h = core::ptr::null_mut();
    for &m in ids {
        h = try_get_module(m);
        if !h.is_null() {
            break;
        }
    }
    if !h.is_null() {
        let f = GetProcAddress(h, name.as_ptr());
        if !f.is_null() {
            core::ptr::write_volatile(slot, f as usize);
            return f;
        }
    }
    core::ptr::write_volatile(slot, usize::MAX);
    core::ptr::null()
}

// ---------------------------------------------------------------------------
// Locks
// ---------------------------------------------------------------------------

/// The exit lock, a CRITICAL_SECTION.
static mut EXIT_LOCK: [u32; 6] = [0; 6];

fn exit_lock() -> *mut c_void {
    core::ptr::addr_of_mut!(EXIT_LOCK) as *mut c_void
}

/// Initialises the exit lock with InitializeCriticalSectionEx (id 0x14, modules 8
/// "api-ms-win-core-synch-l1-2-0" then 18 "kernel32"; fallback
/// InitializeCriticalSectionAndSpinCount),
/// spin count 4000, flags 0.
unsafe fn initialize_critical_section_ex(cs: *mut c_void, spin: u32, flags: u32) {
    let f = try_get_function(0x14, b"InitializeCriticalSectionEx\0", &[8, 18]);
    if f.is_null() {
        InitializeCriticalSectionAndSpinCount(cs, spin);
    } else {
        let f: unsafe extern "system" fn(*mut c_void, u32, u32) -> i32 = core::mem::transmute(f);
        f(cs, spin, flags);
    }
}

// ---------------------------------------------------------------------------
// /GS security cookie and the load configuration directory
// ---------------------------------------------------------------------------

/// The security cookie (initial 0xbb40e64e). Named in the
/// load configuration directory, so the Windows loader may seed it before
/// the entry point runs (then `security_init_cookie` keeps it).
#[no_mangle]
pub static mut __security_cookie: u32 = 0xbb40_e64e;
/// The complement of the security cookie (initial 0x44bf19b1).
#[no_mangle]
pub static mut __security_cookie_complement: u32 = 0x44bf_19b1;

/// Seeds the security cookie (called by the entry point on
/// DLL_PROCESS_ATTACH before everything else).
#[inline(never)]
pub unsafe fn security_init_cookie() {
    let c = core::ptr::read_volatile(core::ptr::addr_of!(__security_cookie));
    if c != 0xbb40_e64e && c & 0xffff_0000 != 0 {
        core::ptr::write_volatile(core::ptr::addr_of_mut!(__security_cookie_complement), !c);
        return;
    }
    let mut ft = [0u32; 2];
    GetSystemTimeAsFileTime(ft.as_mut_ptr());
    // The local lives in memory: its address is mixed in as well.
    let mut x: u32 = ft[1] ^ ft[0];
    core::ptr::write_volatile(&mut x, x ^ GetCurrentThreadId());
    core::ptr::write_volatile(&mut x, core::ptr::read_volatile(&x) ^ GetCurrentProcessId());
    let mut q: i64 = 0;
    QueryPerformanceCounter(&mut q);
    let mut v = ((q >> 32) as u32) ^ (q as u32) ^ core::ptr::read_volatile(&x) ^ (core::ptr::addr_of!(x) as usize as u32);
    if v == 0xbb40_e64e {
        v = 0xbb40_e64f;
    } else if v & 0xffff_0000 == 0 {
        v |= (v | 0x4711) << 16;
    }
    core::ptr::write_volatile(core::ptr::addr_of_mut!(__security_cookie), v);
    core::ptr::write_volatile(core::ptr::addr_of_mut!(__security_cookie_complement), !v);
}

global_asm!(
    ".text",
    ".globl __guard_check_icall_nop",
    "__guard_check_icall_nop:",
    "ret",
);
extern "C" {
    fn _guard_check_icall_nop();
}
/// The guard-check function pointer: points at
/// a lone `ret`; DllCharacteristics has no GUARD_CF, so the
/// loader never replaces it.
#[no_mangle]
pub static __guard_check_icall_fptr: unsafe extern "C" fn() = _guard_check_icall_nop;

/// IMAGE_LOAD_CONFIG_DIRECTORY32 (`__load_config_used`,
/// 0x5c bytes; the data directory entry says 0x40).
#[repr(C)]
pub struct LoadConfig {
    pub size: u32,
    pub zero: [u32; 14], // +0x04..+0x3b: timestamp .. EditList, all 0
    pub security_cookie: *const u32,
    pub se_handler_table: *const u32,
    pub se_handler_count: u32,
    pub guard_cf_check: *const unsafe extern "C" fn(),
    pub guard_cf_dispatch: u32,
    pub guard_cf_table: u32,
    pub guard_cf_count: u32,
    pub guard_flags: u32,
}
unsafe impl Sync for LoadConfig {}
const _: () = assert!(core::mem::size_of::<LoadConfig>() == 0x5c);

/// The SafeSEH table. The DLL
/// registers no SEH frame, so the table is present and empty: no address
/// in this image is a valid handler.
static SAFESEH_TABLE: [u32; 1] = [0];

#[no_mangle]
#[used]
pub static _load_config_used: LoadConfig = LoadConfig {
    size: 0x5c,
    zero: [0; 14],
    security_cookie: core::ptr::addr_of!(__security_cookie),
    se_handler_table: SAFESEH_TABLE.as_ptr(),
    se_handler_count: 0,
    guard_cf_check: core::ptr::addr_of!(__guard_check_icall_fptr),
    guard_cf_dispatch: 0,
    guard_cf_table: 0,
    guard_cf_count: 0,
    guard_flags: 0x100,
};

// ---------------------------------------------------------------------------
// CRT attach / detach
// ---------------------------------------------------------------------------

/// The run-time part of DLL_PROCESS_ATTACH before the initialisers:
/// the processor features, the exit lock,
/// the heap.
pub unsafe fn process_attach() {
    isa_available_init();
    initialize_critical_section_ex(exit_lock(), 4000, 0);
    ACRT_HEAP = GetProcessHeap();
}

/// The run-time part of DLL_PROCESS_DETACH after the exit entries:
/// deletes the exit lock and
/// frees the cached modules unless the process is terminating.
pub unsafe fn process_detach(terminating: bool) {
    DeleteCriticalSection(exit_lock());
    if !terminating {
        let m = core::ptr::addr_of_mut!(MODULES) as *mut usize;
        for i in 0..20 {
            let v = *m.add(i);
            if v != 0 && v != usize::MAX {
                FreeLibrary(v as *mut c_void);
            }
            *m.add(i) = 0;
        }
    }
}

// ---------------------------------------------------------------------------
// _purecall -> abort -> reportfault / _exit(3)
// ---------------------------------------------------------------------------

/// The abort behaviour (value 2 =
/// _CALL_REPORTFAULT; it is never changed).
static mut ABORT_BEHAVIOR: u32 = 2;
/// The debugger hook word, cleared by the debugger hook.
static mut DEBUGGER_HOOK_DUMMY: u32 = 0;
/// Exit state: whether the process is exiting, and whether exit has run.
static mut C_EXIT_DONE: u8 = 0;
static mut C_TERMINATION_STARTED: u32 = 0;
/// Whether the process is a packaged app: 0 unknown, 1 yes, 2 no.
static mut PACKAGED: u32 = 0;

extern "C" {
    fn rf1_call_reportfault(code: i32, exception_code: u32, flags: u32);
}

// The fault report: CONTEXT (0x2cc bytes at
// ebp-0x2d0) and EXCEPTION_RECORD (0x50 bytes at ebp-0x320) zeroed with
// memset, the registers, segment registers, EFlags, Eip = return address,
// Esp = &return address and the caller's Ebp captured, ContextFlags
// 0x10001, then IsDebuggerPresent, SetUnhandledExceptionFilter(NULL),
// UnhandledExceptionFilter(&{record, context}) and the debugger hook.
global_asm!(
    ".text",
    ".globl _rf1_call_reportfault",
    ".p2align 4",
    "_rf1_call_reportfault:",
    "push ebp",
    "mov ebp, esp",
    "sub esp, 0x328",
    "push edi",
    "cmp dword ptr [ebp + 8], -1",
    "je 1f",
    "push dword ptr [ebp + 8]",
    "call _rf1_debugger_hook",
    "pop ecx",
    "1:",
    "push 0x50",
    "lea eax, [ebp - 0x320]",
    "push 0",
    "push eax",
    "call _rf1_memset",
    "push 0x2cc",
    "lea eax, [ebp - 0x2d0]",
    "push 0",
    "push eax",
    "call _rf1_memset",
    "lea eax, [ebp - 0x320]",
    "add esp, 0x18",
    "mov dword ptr [ebp - 0x328], eax",
    "lea eax, [ebp - 0x2d0]",
    "mov dword ptr [ebp - 0x324], eax",
    "mov dword ptr [ebp - 0x220], eax",
    "mov dword ptr [ebp - 0x224], ecx",
    "mov dword ptr [ebp - 0x228], edx",
    "mov dword ptr [ebp - 0x22c], ebx",
    "mov dword ptr [ebp - 0x230], esi",
    "mov dword ptr [ebp - 0x234], edi",
    "mov word ptr [ebp - 0x208], ss",
    "mov word ptr [ebp - 0x214], cs",
    "mov word ptr [ebp - 0x238], ds",
    "mov word ptr [ebp - 0x23c], es",
    "mov word ptr [ebp - 0x240], fs",
    "mov word ptr [ebp - 0x244], gs",
    "pushfd",
    "pop dword ptr [ebp - 0x210]",
    "mov eax, dword ptr [ebp + 4]",
    "mov dword ptr [ebp - 0x218], eax",
    "lea eax, [ebp + 4]",
    "mov dword ptr [ebp - 0x20c], eax",
    "mov dword ptr [ebp - 0x2d0], 0x10001",
    "mov eax, dword ptr [eax - 4]",
    "mov dword ptr [ebp - 0x21c], eax",
    "mov eax, dword ptr [ebp + 0xc]",
    "mov dword ptr [ebp - 0x320], eax",
    "mov eax, dword ptr [ebp + 0x10]",
    "mov dword ptr [ebp - 0x31c], eax",
    "mov eax, dword ptr [ebp + 4]",
    "mov dword ptr [ebp - 0x314], eax",
    "call {idp}",
    "push 0",
    "mov edi, eax",
    "call {suef}",
    "lea eax, [ebp - 0x328]",
    "push eax",
    "call {uef}",
    "test eax, eax",
    "jne 2f",
    "test edi, edi",
    "jne 2f",
    "cmp dword ptr [ebp + 8], -1",
    "je 2f",
    "push dword ptr [ebp + 8]",
    "call _rf1_debugger_hook",
    "pop ecx",
    "2:",
    "pop edi",
    "mov esp, ebp",
    "pop ebp",
    "ret",
    // the debugger hook: clears the hook word
    ".p2align 4",
    "_rf1_debugger_hook:",
    "and dword ptr [{dummy}], 0",
    "ret",
    idp = sym IsDebuggerPresent,
    suef = sym SetUnhandledExceptionFilter,
    uef = sym UnhandledExceptionFilter,
    dummy = sym DEBUGGER_HOOK_DUMMY,
);

/// Whether the process is a managed app: its image is PE32 with a COM
/// descriptor directory.
unsafe fn is_managed_app() -> bool {
    let h = GetModuleHandleW(core::ptr::null());
    if h.is_null() || core::ptr::read_unaligned(h as *const u16) != 0x5a4d {
        return false;
    }
    let nt = h.add(core::ptr::read_unaligned(h.add(0x3c) as *const u32) as usize);
    if core::ptr::read_unaligned(nt as *const u32) != 0x4550
        || core::ptr::read_unaligned(nt.add(0x18) as *const u16) != 0x10b
        || core::ptr::read_unaligned(nt.add(0x74) as *const u32) <= 0xe
    {
        return false;
    }
    core::ptr::read_unaligned(nt.add(0xe8) as *const u32) != 0
}

static MSCOREE: [u16; 12] = w(b"mscoree.dll\0");

/// Lets the managed run-time end a managed process.
unsafe fn try_cor_exit_process(code: u32) {
    let mut h: *mut c_void = core::ptr::null_mut();
    if GetModuleHandleExW(0, MSCOREE.as_ptr(), &mut h) != 0 {
        let f = GetProcAddress(h, b"CorExitProcess\0".as_ptr());
        if !f.is_null() {
            let f: unsafe extern "system" fn(u32) = core::mem::transmute(f);
            f(code);
        }
    }
    if !h.is_null() {
        FreeLibrary(h);
    }
}

/// Whether the process is a packaged app (GetCurrentPackageId, function id 8,
/// modules 0 and 14; ERROR_INSUFFICIENT_BUFFER means packaged).
unsafe fn is_packaged_app() -> bool {
    let c = core::ptr::read_volatile(core::ptr::addr_of!(PACKAGED));
    if c != 0 {
        return c == 1;
    }
    let f = try_get_function(8, b"GetCurrentPackageId\0", &[0, 14]);
    if !f.is_null() {
        let f: unsafe extern "system" fn(*mut u32, *mut u8) -> u32 = core::mem::transmute(f);
        let mut len = 0u32;
        if f(&mut len, core::ptr::null_mut()) == 0x7a {
            core::ptr::write_volatile(core::ptr::addr_of_mut!(PACKAGED), 1);
            return true;
        }
    }
    core::ptr::write_volatile(core::ptr::addr_of_mut!(PACKAGED), 2);
    false
}

/// Exits or terminates the process: a packaged app without
/// FLG_APPLICATION_VERIFIER in the PEB's NtGlobalFlag is terminated.
unsafe fn exit_or_terminate_process(code: u32) -> ! {
    if is_packaged_app() {
        let peb: *const u8;
        asm!("mov {p}, dword ptr fs:[0x30]", p = out(reg) peb, options(nostack, readonly));
        let gflags = core::ptr::read_volatile(peb.add(0x68) as *const u32);
        if (gflags >> 8) & 1 == 0 {
            TerminateProcess(GetCurrentProcess(), code);
        }
    }
    try_cor_exit_process(code);
    ExitProcess(code)
}

/// `_exit`: ends the process without cleanup, so no atexit
/// entry runs.
unsafe fn exit_no_cleanup(code: u32) -> ! {
    if is_managed_app() {
        try_cor_exit_process(code);
    }
    EnterCriticalSection(exit_lock());
    if core::ptr::read_volatile(core::ptr::addr_of!(C_EXIT_DONE)) == 0 {
        core::ptr::write_volatile(core::ptr::addr_of_mut!(C_TERMINATION_STARTED), 1);
        core::ptr::write_volatile(core::ptr::addr_of_mut!(C_EXIT_DONE), 1);
    }
    LeaveCriticalSection(exit_lock());
    exit_or_terminate_process(code)
}

/// `abort`. No SIGABRT handler can be registered (nothing in
/// the module calls `signal`), and the abort behaviour is 2: fast fail when
/// PF_FASTFAIL_AVAILABLE (0x17), else report the fault
/// (STATUS_FATAL_APP_EXIT 0x40000015, EXCEPTION_NONCONTINUABLE) and
/// `_exit(3)`.
unsafe fn abort() -> ! {
    if core::ptr::read_volatile(core::ptr::addr_of!(ABORT_BEHAVIOR)) & 2 != 0 {
        if IsProcessorFeaturePresent(0x17) != 0 {
            asm!("int 0x29", in("ecx") 7u32, options(noreturn));
        }
        rf1_call_reportfault(3, 0x4000_0015, 1);
    }
    exit_no_cleanup(3)
}

/// `_purecall`: no purecall handler is installed,
/// so abort().
pub unsafe extern "C" fn purecall() -> ! {
    abort()
}
