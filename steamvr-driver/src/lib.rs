//! The OpenRigCue SteamVR driver `driver_simracingstudio`, a Linux SteamVR driver
//! (`bin/linux64/driver_simracingstudio.so`).
//!
//! The driver adds one tracked device,
//! "SimRacingStudioVirtualTracker 1.0.0.1", and on every frame submits its pose: the motion rig's pose read
//! from the named section `Local\SimRacingStudioMotionRigPose` (written by the motion app), plus the offset
//! OpenVR-MotionCompensation publishes in `Local\OVRMC_MMFv1`. It logs to
//! `driver_motionpose.log` and standard output, configured at load from `logging.conf` and a built-in text.
//!
//! Load-time order: the logging storage and the crash
//! handler, then the process-attach step, all from one `.init_array` entry.

pub mod context;
pub mod device;
pub mod elpp;
pub mod math;
pub mod openvr;
pub mod provider;
pub mod section;

use elpp::config::Level;
use std::ffi::{c_char, c_void};

/// The only export of the driver.
///
/// # Safety
/// Called by vrserver with a NUL-terminated interface name and a NULL or writable return code.
#[no_mangle]
pub unsafe extern "C" fn HmdDriverFactory(interface_name: *const c_char, return_code: *mut i32) -> *mut c_void {
    provider::hmd_driver_factory(interface_name, return_code)
}

/// The process-attach step: sets up logging.
pub fn dll_process_attach() {
    elpp::init_logging();
    elpp::log(
        Level::Info,
        b"|========================================================================================|",
    );
    elpp::log(Level::Info, b"motionPose dll loaded...");
    elpp::log(Level::Trace, b"Trace messages enabled.");
    elpp::log(Level::Debug, b"Debug messages enabled.");
}

#[cfg_attr(test, allow(dead_code))]
extern "C" fn on_load() {
    elpp::install_crash_handler();
    dll_process_attach();
}

#[cfg_attr(test, allow(dead_code))]
extern "C" fn on_unload() {
    elpp::shutdown();
}

#[cfg(not(test))]
#[used]
#[link_section = ".init_array"]
static ON_LOAD: extern "C" fn() = on_load;

#[cfg(not(test))]
#[used]
#[link_section = ".fini_array"]
static ON_UNLOAD: extern "C" fn() = on_unload;

#[cfg(test)]
mod tests;
