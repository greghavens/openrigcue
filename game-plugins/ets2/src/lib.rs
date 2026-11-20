//! The ETS2/ATS telemetry plugin
//! (`ets2-telemetry.dll`, x86_64, and `ets2-telemetry_32.dll`,
//! i386): an SCS SDK telemetry 1.00 plugin that publishes truck telemetry in
//! the 16 KiB named mapping `Local\SimTelemetryETS2` (revision 5 layout).
//!
//! Exports: `scs_telemetry_init` (ordinal 1) and
//! `scs_telemetry_shutdown` (ordinal 2), stdcall on i386, undecorated names.
//! `DllMain` is not exported; the CRT's DLL entry calls it.
//!
//! The release DLL is built without std, panic = "abort". The plugin code
//! imports from KERNEL32 and MSVCR110
//! (CreateFileMappingW, OpenFileMappingW, MapViewOfFile, UnmapViewOfFile,
//! CloseHandle, GetLastError; operator new, strncpy, memset). The start-up
//! code is the CRT DLL start-up in `crt.rs`, with its
//! MSVCR110/KERNEL32 imports.
//! Debug builds, unit tests and the `std` feature (harness) link std.
#![cfg_attr(not(any(test, feature = "std", debug_assertions)), no_std)]
// Every function here works on pointers handed over by the game (SDK params,
// event infos, channel values, the mapping view); the one
// safety rule for all of them is "called by an SCS game through the SDK".
#![allow(clippy::missing_safety_doc)]

pub mod layout;
pub mod os;
pub mod plugin;
pub mod scs;

#[cfg(windows)]
mod exports {
    use crate::plugin;
    use crate::scs::{ScsResult, ScsTelemetryInitParams};

    /// The plugin's initialisation entry (ordinal 1).
    #[no_mangle]
    pub unsafe extern "system" fn scs_telemetry_init(version: u32, params: *const ScsTelemetryInitParams) -> ScsResult {
        plugin::telemetry_init(version, params, crate::os::native())
    }

    /// The plugin's shutdown entry (ordinal 2).
    #[no_mangle]
    pub unsafe extern "system" fn scs_telemetry_shutdown() {
        plugin::telemetry_shutdown(crate::os::native())
    }
}

/// DllMain (stdcall), called by the CRT
/// entry. Not exported.
#[cfg(windows)]
pub(crate) unsafe extern "system" fn dll_main_entry(
    _module: *mut core::ffi::c_void,
    reason: u32,
    _reserved: *mut core::ffi::c_void,
) -> i32 {
    plugin::dll_main(reason, crate::os::native())
}

// The release DLL carries its own CRT start-up (crt.rs), whose
// __DllMainCRTStartup calls dll_main_entry. Builds with std (the harness,
// debug builds) keep the mingw-w64 CRT, which calls DllMain by this name.
#[cfg(all(windows, not(any(test, feature = "std", debug_assertions))))]
mod crt;
#[cfg(all(windows, target_arch = "x86_64", any(test, feature = "std", debug_assertions)))]
core::arch::global_asm!(".globl DllMain", "DllMain:", "jmp {f}", f = sym dll_main_entry);
#[cfg(all(windows, target_arch = "x86", any(test, feature = "std", debug_assertions)))]
core::arch::global_asm!(".globl _DllMain@12", "_DllMain@12:", "jmp {f}", f = sym dll_main_entry);

#[cfg(not(any(test, feature = "std", debug_assertions)))]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    // Nothing in the plugin panics (no indexing or arithmetic checks in a
    // release build); this only satisfies no_std.
    loop {}
}

#[cfg(test)]
mod tests;
