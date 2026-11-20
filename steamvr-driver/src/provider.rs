//! The server provider (`IServerTrackedDeviceProvider_004`) and
//! the watchdog provider (`IVRWatchdogProvider_001`).

use crate::context::{ctx, null_interface_call};
use crate::device::Device;
use crate::openvr::{self, EVRInitError, Interface};
use std::ffi::{c_char, c_void};

#[repr(C)]
pub struct ProviderVtable {
    pub init: unsafe extern "C" fn(*mut Provider, *mut Interface) -> EVRInitError,
    pub cleanup: unsafe extern "C" fn(*mut Provider),
    pub get_interface_versions: unsafe extern "C" fn(*mut Provider) -> *const *const c_char,
    pub run_frame: unsafe extern "C" fn(*mut Provider),
    pub should_block_standby_mode: unsafe extern "C" fn(*mut Provider) -> bool,
    pub enter_standby: unsafe extern "C" fn(*mut Provider),
    pub leave_standby: unsafe extern "C" fn(*mut Provider),
}

#[repr(C)]
pub struct Provider {
    vtable: *const ProviderVtable,
    /// The tracked device.
    controller: *mut Device,
}

#[repr(C)]
pub struct WatchdogVtable {
    pub init: unsafe extern "C" fn(*mut Watchdog, *mut Interface) -> EVRInitError,
    pub cleanup: unsafe extern "C" fn(*mut Watchdog),
}

#[repr(C)]
pub struct Watchdog {
    vtable: *const WatchdogVtable,
}

static PROVIDER_VTABLE: ProviderVtable = ProviderVtable {
    init: provider_init,
    cleanup: provider_cleanup,
    get_interface_versions: provider_get_interface_versions,
    run_frame: provider_run_frame,
    should_block_standby_mode: provider_should_block_standby_mode,
    enter_standby: provider_enter_standby,
    leave_standby: provider_leave_standby,
};

static WATCHDOG_VTABLE: WatchdogVtable = WatchdogVtable { init: watchdog_init, cleanup: watchdog_cleanup };

pub struct Statics {
    pub provider: std::cell::UnsafeCell<Provider>,
    pub watchdog: std::cell::UnsafeCell<Watchdog>,
}
// SAFETY: vrserver serialises calls into the provider.
unsafe impl Sync for Statics {}

/// The server provider and the watchdog provider.
pub static STATICS: Statics = Statics {
    provider: std::cell::UnsafeCell::new(Provider { vtable: &PROVIDER_VTABLE, controller: std::ptr::null_mut() }),
    watchdog: std::cell::UnsafeCell::new(Watchdog { vtable: &WATCHDOG_VTABLE }),
};

/// `vr::k_InterfaceVersions` with its NULL terminator.
pub struct Versions([*const c_char; 12]);
// SAFETY: pointers to static, immutable strings.
unsafe impl Sync for Versions {}

pub static INTERFACE_VERSIONS: Versions = Versions([
    openvr::INTERFACE_VERSIONS[0].as_ptr() as *const c_char,
    openvr::INTERFACE_VERSIONS[1].as_ptr() as *const c_char,
    openvr::INTERFACE_VERSIONS[2].as_ptr() as *const c_char,
    openvr::INTERFACE_VERSIONS[3].as_ptr() as *const c_char,
    openvr::INTERFACE_VERSIONS[4].as_ptr() as *const c_char,
    openvr::INTERFACE_VERSIONS[5].as_ptr() as *const c_char,
    openvr::INTERFACE_VERSIONS[6].as_ptr() as *const c_char,
    openvr::INTERFACE_VERSIONS[7].as_ptr() as *const c_char,
    openvr::INTERFACE_VERSIONS[8].as_ptr() as *const c_char,
    openvr::INTERFACE_VERSIONS[9].as_ptr() as *const c_char,
    openvr::INTERFACE_VERSIONS[10].as_ptr() as *const c_char,
    std::ptr::null(),
]);

/// Initialises the provider with the driver context and adds the tracked device.
unsafe extern "C" fn provider_init(this: *mut Provider, context: *mut Interface) -> EVRInitError {
    let c = ctx();
    let rc = c.init_server(context);
    if rc != openvr::VR_INIT_ERROR_NONE {
        return rc;
    }
    // A previous device (an initialisation without a clean-up) is not deleted.
    let mut device = Device::new();
    let device_ptr = device.as_interface();
    (*this).controller = Box::into_raw(device);
    let mut err: EVRInitError = 0;
    let host = c.vr_server_driver_host(&mut err);
    if host.is_null() {
        null_interface_call();
    }
    let serial = (*(*this).controller).serial_number_c();
    let f: openvr::TrackedDeviceAddedFn = std::mem::transmute(openvr::slot(host, openvr::HOST_TRACKED_DEVICE_ADDED));
    f(host, serial.as_ptr() as *const c_char, openvr::TRACKED_DEVICE_CLASS_CONTROLLER, device_ptr);
    openvr::VR_INIT_ERROR_NONE
}

/// Cleans up: deletes the device through its scalar deleting
/// destructor, with flags 1.
unsafe extern "C" fn provider_cleanup(this: *mut Provider) {
    let controller = (*this).controller;
    if !controller.is_null() {
        ((*(*controller).vtable).scalar_deleting_destructor)(controller, 1);
    }
    (*this).controller = std::ptr::null_mut();
}

/// The interface versions.
unsafe extern "C" fn provider_get_interface_versions(_this: *mut Provider) -> *const *const c_char {
    INTERFACE_VERSIONS.0.as_ptr()
}

/// One frame of the provider: one frame of the device.
unsafe extern "C" fn provider_run_frame(this: *mut Provider) {
    let controller = (*this).controller;
    if !controller.is_null() {
        (*controller).run_frame();
    }
}

/// Standby mode is not blocked: false.
unsafe extern "C" fn provider_should_block_standby_mode(_this: *mut Provider) -> bool {
    false
}

/// Entering and leaving standby do nothing.
unsafe extern "C" fn provider_enter_standby(_this: *mut Provider) {}
unsafe extern "C" fn provider_leave_standby(_this: *mut Provider) {}

/// Initialises the watchdog provider.
unsafe extern "C" fn watchdog_init(_this: *mut Watchdog, context: *mut Interface) -> EVRInitError {
    ctx().init_watchdog(context)
}

/// Cleans up the watchdog provider.
unsafe extern "C" fn watchdog_cleanup(_this: *mut Watchdog) {
    ctx().cleanup_watchdog();
}

/// The driver factory: returns the provider for an interface name.
///
/// # Safety
/// `interface_name` must be a NUL-terminated string; `return_code` NULL or writable.
pub unsafe fn hmd_driver_factory(interface_name: *const c_char, return_code: *mut i32) -> *mut c_void {
    let name = std::ffi::CStr::from_ptr(interface_name).to_bytes_with_nul();
    if name == openvr::ISERVER_TRACKED_DEVICE_PROVIDER_VERSION {
        return STATICS.provider.get().cast();
    }
    if name == openvr::IVR_WATCHDOG_PROVIDER_VERSION {
        return STATICS.watchdog.get().cast();
    }
    if !return_code.is_null() {
        *return_code = openvr::VR_INIT_ERROR_INIT_INTERFACE_NOT_FOUND;
    }
    std::ptr::null_mut()
}
