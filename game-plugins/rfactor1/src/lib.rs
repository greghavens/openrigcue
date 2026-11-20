//! `SRS_Rfactor1.dll` (internal name
//! `rFactorSharedMemoryMap.dll`, PE32 i386):
//! an rFactor 1 InternalsPluginV3 plugin that publishes telemetry
//! and scoring in the named mapping `$rFactorShared$` (0x9afc bytes).
//!
//! Exports (ordinals 1..4, cdecl): `GetPluginName` -> "rFactorSharedMemoryMap",
//! `GetPluginObjectCount` -> 1, `GetPluginObjectInfo(i)` -> the static
//! `InternalsPluginInfo` for i == 0 else NULL, `GetPluginVersion` -> 2.
//!
//!
#![cfg_attr(not(test), no_std)]
#![allow(clippy::missing_safety_doc)]

pub mod crt;
pub mod layout;
pub mod libm;
pub mod os;
pub mod plugin;
pub mod sincos_table;
pub mod sse;
pub mod x87;

/// The name returned by GetPluginName and InternalsPluginInfo::GetDesc.
pub static PLUGIN_NAME: [u8; 23] = *b"rFactorSharedMemoryMap\0";
/// The name returned by InternalsPluginInfo::GetName.
pub static INFO_NAME: [u8; 23] = *b"rFactorSharedMemoryMap\0";
/// The sub-type returned by GetSubType.
pub static INFO_SUBTYPE: [u8; 10] = *b"Internals\0";
/// The plugin version and object count.
pub const PLUGIN_VERSION: u32 = 2;
pub const PLUGIN_OBJECT_COUNT: u32 = 1;
/// InternalsPluginInfo::GetType / GetVersion.
pub const INFO_TYPE: u32 = 9;
pub const INFO_VERSION: u32 = 3;

/// The full name built at start-up:
/// the description, " - ", and the name.
pub fn full_name() -> [u8; 128] {
    let mut b = [0u8; 128];
    let s = b"rFactorSharedMemoryMap - rFactorSharedMemoryMap";
    b[..s.len()].copy_from_slice(s);
    b
}

#[cfg(all(windows, target_arch = "x86"))]
pub mod dll;
#[cfg(all(windows, target_arch = "x86"))]
pub mod rt;
#[cfg(all(windows, target_arch = "x86"))]
pub mod rtti;

#[cfg(not(test))]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    // Never reached; end the process like the CRT's fast fail.
    #[cfg(target_arch = "x86")]
    unsafe {
        core::arch::asm!("int 0x29", in("ecx") 7u32, options(noreturn));
    }
    #[allow(unreachable_code)]
    loop {}
}

#[cfg(test)]
mod tests;
