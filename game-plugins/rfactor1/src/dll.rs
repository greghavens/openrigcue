//! The DLL surface (i686 only): exports, DllMain, the two vtables and the
//! thiscall entry points.

use crate::crt::Clock;
use crate::os::win::WIN;
use crate::plugin::{self, Plugin};
use crate::rt::{operator_delete as op_delete, operator_new as op_new, purecall};
use crate::rtti::{self, Vtbl};
use core::ffi::c_void;


static mut CLOCK: Clock = Clock::UNSET;


/// The static `InternalsPluginInfo` object (0x84 bytes).
#[repr(C)]
pub struct Info {
    pub vtbl: *const (),
    pub full_name: [u8; 128],
}
const _: () = assert!(core::mem::size_of::<Info>() == crate::layout::INFO_SIZE);

static mut INFO: Info = Info { vtbl: core::ptr::null(), full_name: [0; 128] };

fn clock() -> &'static Clock {
    unsafe { &*core::ptr::addr_of!(CLOCK) }
}


// ---- InternalsPluginInfo vtable ----

/// Destructor: vfptr = PluginObjectInfo::vftable; delete (0x84) if flags & 1.
unsafe extern "thiscall" fn info_dtor(this: *mut Info, flags: u32) -> *mut Info {
    (*this).vtbl = PLUGIN_OBJECT_INFO_VTBL.vfptr();
    if flags & 1 != 0 {
        op_delete(this as *mut c_void);
    }
    this
}
unsafe extern "thiscall" fn info_get_name(_this: *mut Info) -> *const u8 {
    crate::INFO_NAME.as_ptr()
}
unsafe extern "thiscall" fn info_get_full_name(this: *mut Info) -> *const u8 {
    (*this).full_name.as_ptr()
}
unsafe extern "thiscall" fn info_get_desc(_this: *mut Info) -> *const u8 {
    crate::PLUGIN_NAME.as_ptr()
}
unsafe extern "thiscall" fn info_get_type(_this: *mut Info) -> u32 {
    crate::INFO_TYPE
}
unsafe extern "thiscall" fn info_get_subtype(_this: *mut Info) -> *const u8 {
    crate::INFO_SUBTYPE.as_ptr()
}
unsafe extern "thiscall" fn info_get_version(_this: *mut Info) -> u32 {
    crate::INFO_VERSION
}
/// `new SharedMemoryMapPlugin` (0x20 bytes, only the vfptr set).
/// operator new throws std::bad_alloc on failure, so the NULL check after it
/// is never taken.
unsafe extern "thiscall" fn info_create(_this: *mut Info) -> *mut Plugin {
    let p = op_new(crate::layout::PLUGIN_SIZE) as *mut Plugin;
    if p.is_null() {
        return p;
    }
    (*p).vtbl = PLUGIN_VTBL.vfptr();
    p
}

static INFO_VTBL: Vtbl<8> = Vtbl { col: &rtti::COL_INTERNALS_PLUGIN_INFO.0, slots: [
    info_dtor as *const (),
    info_get_name as *const (),
    info_get_full_name as *const (),
    info_get_desc as *const (),
    info_get_type as *const (),
    info_get_subtype as *const (),
    info_get_version as *const (),
    info_create as *const (),
] };

/// PluginObjectInfo::vftable: destructor + 7 pure calls.
unsafe extern "thiscall" fn object_info_dtor(this: *mut Info, flags: u32) -> *mut Info {
    (*this).vtbl = PLUGIN_OBJECT_INFO_VTBL.vfptr();
    if flags & 1 != 0 {
        op_delete(this as *mut c_void);
    }
    this
}
static PLUGIN_OBJECT_INFO_VTBL: Vtbl<8> = Vtbl { col: &rtti::COL_PLUGIN_OBJECT_INFO.0, slots: [
    object_info_dtor as *const (),
    purecall as *const (),
    purecall as *const (),
    purecall as *const (),
    purecall as *const (),
    purecall as *const (),
    purecall as *const (),
    purecall as *const (),
] };

// ---- PluginObject::vftable: destructor + 5 pure calls ----

unsafe extern "thiscall" fn object_dtor(this: *mut Plugin, flags: u32) -> *mut Plugin {
    (*this).vtbl = PLUGIN_OBJECT_VTBL.vfptr();
    if flags & 1 != 0 {
        op_delete(this as *mut c_void);
    }
    this
}
static PLUGIN_OBJECT_VTBL: Vtbl<6> = Vtbl { col: &rtti::COL_PLUGIN_OBJECT.0, slots: [
    object_dtor as *const (),
    purecall as *const (),
    purecall as *const (),
    purecall as *const (),
    purecall as *const (),
    purecall as *const (),
] };

// ---- SharedMemoryMapPlugin::vftable (28 slots) ----

/// Slot 0: the destructor.
unsafe extern "thiscall" fn p_dtor(this: *mut Plugin, flags: u32) -> *mut Plugin {
    (*this).vtbl = PLUGIN_OBJECT_VTBL.vfptr();
    if flags & 1 != 0 {
        op_delete(this as *mut c_void);
    }
    this
}
unsafe fn slot(this: *mut Plugin, i: usize) -> *const () {
    *((*this).vtbl as *const *const ()).add(i)
}
/// Slot 1 (Destroy): a virtual call of slot 7.
unsafe extern "thiscall" fn p_destroy(this: *mut Plugin) {
    let f: unsafe extern "thiscall" fn(*mut Plugin) = core::mem::transmute(slot(this, 7));
    f(this)
}
/// Slot 2 (GetInfo).
unsafe extern "thiscall" fn p_get_info(_this: *mut Plugin) -> *mut Info {
    core::ptr::addr_of_mut!(INFO)
}
unsafe extern "thiscall" fn ret0_u32(_this: *mut Plugin) -> u32 {
    0
}
unsafe extern "thiscall" fn ret0_u32_a1(_this: *mut Plugin, _a: u32) -> u32 {
    0
}
unsafe extern "thiscall" fn p_startup(this: *mut Plugin) {
    plugin::startup(this, &WIN)
}
unsafe extern "thiscall" fn p_shutdown(this: *mut Plugin) {
    plugin::shutdown(this, &WIN)
}
/// Slot 8: entering realtime.
unsafe extern "thiscall" fn p_enter_realtime(this: *mut Plugin) {
    (*this).in_realtime = 1;
}
/// Slot 9: leaving realtime.
unsafe extern "thiscall" fn p_exit_realtime(this: *mut Plugin) {
    (*this).in_realtime = 0;
}
// The bool slots: only al is set, no frame.
//   10 / 20: b0 01 c3        mov al,1 ; ret
//   12 / 14: 32 c0 c3        xor al,al ; ret
//   19 / 27: 32 c0 c2 04 00  xor al,al ; ret 4
//   18:      32 c0 c2 08 00  xor al,al ; ret 8
core::arch::global_asm!(
    ".text",
    ".p2align 4",
    "_rf1_true0:",
    ".byte 0xb0, 0x01, 0xc3",
    ".p2align 4",
    "_rf1_false0:",
    ".byte 0x32, 0xc0, 0xc3",
    ".p2align 4",
    "_rf1_false1:",
    ".byte 0x32, 0xc0, 0xc2, 0x04, 0x00",
    ".p2align 4",
    "_rf1_false2:",
    ".byte 0x32, 0xc0, 0xc2, 0x08, 0x00",
    ".globl _rf1_true0",
    ".globl _rf1_false0",
    ".globl _rf1_false1",
    ".globl _rf1_false2",
);
extern "C" {
    fn rf1_true0();
    fn rf1_false0();
    fn rf1_false1();
    fn rf1_false2();
}
unsafe extern "thiscall" fn void0(_this: *mut Plugin) {}
unsafe extern "thiscall" fn void1(_this: *mut Plugin, _a: u32) {}
/// Slot 22: the session start.
unsafe extern "thiscall" fn p_reset(this: *mut Plugin) {
    plugin::reset(this)
}
/// Slot 23: a virtual call of slot 22, then +0x1c = 0.
unsafe extern "thiscall" fn p_end(this: *mut Plugin) {
    let f: unsafe extern "thiscall" fn(*mut Plugin) = core::mem::transmute(slot(this, 22));
    f(this);
    (*this).session = 0;
}
/// Slot 24 (UpdateTelemetry).
unsafe extern "thiscall" fn p_update_telemetry(this: *mut Plugin, info: *const u8) {
    plugin::update_telemetry(this, &WIN, clock(), info)
}
/// Slot 26 (UpdateScoring).
unsafe extern "thiscall" fn p_update_scoring(this: *mut Plugin, info: *const u8) {
    plugin::update_scoring(this, &WIN, clock(), info)
}

static PLUGIN_VTBL: Vtbl<28> = Vtbl { col: &rtti::COL_SHARED_MEMORY_MAP_PLUGIN.0, slots: [
    p_dtor as *const (),             // 0
    p_destroy as *const (),          // 1
    p_get_info as *const (),         // 2
    ret0_u32 as *const (),           // 3
    ret0_u32_a1 as *const (),        // 4
    ret0_u32_a1 as *const (),        // 5
    p_startup as *const (),          // 6
    p_shutdown as *const (),         // 7
    p_enter_realtime as *const (),   // 8
    p_exit_realtime as *const (),    // 9
    rf1_true0 as *const (),               // 10
    void1 as *const (),              // 11
    rf1_false0 as *const (),             // 12
    void1 as *const (),              // 13
    rf1_false0 as *const (),             // 14
    void1 as *const (),              // 15
    void0 as *const (),              // 16
    void0 as *const (),              // 17
    rf1_false2 as *const (),             // 18
    rf1_false1 as *const (),             // 19
    rf1_true0 as *const (),               // 20
    void1 as *const (),              // 21
    p_reset as *const (),            // 22
    p_end as *const (),              // 23
    p_update_telemetry as *const (), // 24
    void1 as *const (),              // 25
    p_update_scoring as *const (),   // 26
    rf1_false1 as *const (),             // 27
] };

#[no_mangle]
pub extern "C" fn GetPluginName() -> *const u8 {
    crate::PLUGIN_NAME.as_ptr()
}
#[no_mangle]
pub extern "C" fn GetPluginObjectCount() -> u32 {
    crate::PLUGIN_OBJECT_COUNT
}
#[no_mangle]
pub unsafe extern "C" fn GetPluginObjectInfo(i: u32) -> *mut Info {
    if i != 0 {
        return core::ptr::null_mut();
    }
    core::ptr::addr_of_mut!(INFO)
}
#[no_mangle]
pub extern "C" fn GetPluginVersion() -> u32 {
    crate::PLUGIN_VERSION
}

/// The entry point (linked with `-e _DllMain@12`, no C runtime):
/// DLL_PROCESS_ATTACH first seeds
/// the security cookie, then runs the run-time
/// initialisation the plugin depends on (processor features, the exit
/// lock, the heap, see rt.rs), the clock base, then the
/// construction of the static InternalsPluginInfo.
/// DLL_PROCESS_DETACH destroys that object: vfptr =
/// PluginObjectInfo::vftable, then the run-time uninitialisation (exit lock,
/// cached modules unless the process is terminating, i.e. lpReserved != NULL).
#[no_mangle]
pub unsafe extern "system" fn DllMain(_h: *mut c_void, reason: u32, reserved: *mut c_void) -> i32 {
    match reason {
        1 => {
            crate::rt::security_init_cookie();
            crate::rt::process_attach();
            CLOCK = Clock::init(&WIN);
            let info = core::ptr::addr_of_mut!(INFO);
            (*info).vtbl = INFO_VTBL.vfptr();
            (*info).full_name = crate::full_name();
        }
        0 => {
            let info = core::ptr::addr_of_mut!(INFO);
            (*info).vtbl = PLUGIN_OBJECT_INFO_VTBL.vfptr();
            crate::rt::process_detach(!reserved.is_null());
        }
        _ => {}
    }
    1
}

/// core's precompiled unwind tables reference these even with panic = abort.
#[no_mangle]
pub extern "C" fn rust_eh_personality() {}
