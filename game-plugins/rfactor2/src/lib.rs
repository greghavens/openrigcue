//! `rFactor2SharedMemoryMapPlugin64.dll` (PE32+ x86-64; shared-memory
//! format of The Iron Wolf's rF2 Shared Memory Map Plugin v3.0.1.0):
//! an rFactor 2 / Le Mans Ultimate InternalsPluginV07 plugin
//! that publishes telemetry, scoring, rules, multi-session rules and
//! extended state in the named mappings `$rFactor2SMMP_Telemetry$`,
//! `$rFactor2SMMP_Scoring$`, `$rFactor2SMMP_Rules$`,
//! `$rFactor2SMMP_MultiRules$` and `$rFactor2SMMP_Extended$`.
//!
//! Exports (ordinals 1..5): `CreatePluginObject`,
//! `DestroyPluginObject`, `GetPluginName`, `GetPluginType` (3 =
//! PO_INTERNALS), `GetPluginVersion` (7). The object handed to the game is
//! a C++ object with an MSVC x64 InternalsPluginV07 vtable at +0 and the
//! PluginObject base at +8.
//!
//!
#![cfg_attr(not(any(test, feature = "std")), no_std)]
#![allow(clippy::missing_safety_doc)]

pub mod allocation;
pub mod cfmt;
pub mod crt;
pub mod dump;
pub mod host;
pub mod layout;
pub mod log;
pub mod mapped;
pub mod plugin;
pub mod sys;
pub mod vtbl;

use core::ffi::c_void;

/// GetPluginName's string.
pub static PLUGIN_NAME: &[u8] = b"rFactor 2 Shared Memory Map Plugin - v3.0\0";
/// PluginObjectType PO_INTERNALS.
pub const PLUGIN_TYPE: i32 = 3;
/// InternalsPluginV07.
pub const PLUGIN_VERSION: i32 = 7;

/// CreatePluginObject: `new SharedMemoryPlugin` as `PluginObject *`
/// (object + 8).
pub unsafe fn create_plugin_object(cx: log::Ctx) -> *mut c_void {
    let p = plugin::SharedMemoryPlugin::create(cx);
    (p as *mut u8).add(8) as *mut c_void
}

/// DestroyPluginObject: `delete` through the virtual destructor.
pub unsafe fn destroy_plugin_object(obj: *mut c_void) {
    if obj.is_null() {
        return;
    }
    let this = (obj as *mut u8).sub(8);
    let f: unsafe extern "C" fn(*mut u8, u32) -> *mut u8 =
        core::mem::transmute(*(*(this as *const *const *const ())).add(0));
    f(this, 1);
}

#[cfg(all(windows, not(test)))]
mod exports {
    use core::ffi::c_void;

    static mut GLOBALS: crate::log::Globals = crate::log::Globals::new();

    fn cx() -> crate::log::Ctx {
        crate::log::Ctx { sys: &crate::sys::win::WIN, g: core::ptr::addr_of_mut!(GLOBALS) }
    }

    #[no_mangle]
    pub unsafe extern "C" fn CreatePluginObject() -> *mut c_void {
        crate::create_plugin_object(cx())
    }

    #[no_mangle]
    pub unsafe extern "C" fn DestroyPluginObject(obj: *mut c_void) {
        crate::destroy_plugin_object(obj)
    }

    #[no_mangle]
    pub extern "C" fn GetPluginName() -> *const u8 {
        crate::PLUGIN_NAME.as_ptr()
    }

    #[no_mangle]
    pub extern "C" fn GetPluginType() -> i32 {
        crate::PLUGIN_TYPE
    }

    #[no_mangle]
    pub extern "C" fn GetPluginVersion() -> i32 {
        crate::PLUGIN_VERSION
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn DisableThreadLibraryCalls(h: *mut c_void) -> i32;
    }

    /// DllMain, called by the CRT entry.
    #[no_mangle]
    pub unsafe extern "system" fn DllMain(h: *mut c_void, reason: u32, _reserved: *mut c_void) -> i32 {
        if reason == 1 {
            DisableThreadLibraryCalls(h);
        }
        1
    }
}

#[cfg(not(any(test, feature = "std")))]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    // Unreachable in practice; end the process like the CRT's fast fail.
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!("int 0x29", in("ecx") 7u32, options(noreturn));
    }
    #[allow(unreachable_code)]
    loop {}
}

/// core's precompiled unwind tables reference the personality routine even
/// with panic = "abort"; nothing ever unwinds through it.
#[cfg(all(windows, not(any(test, feature = "std"))))]
#[no_mangle]
pub extern "C" fn rust_eh_personality() {}

#[cfg(test)]
mod tests;
