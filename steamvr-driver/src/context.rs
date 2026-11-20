//! `vr::COpenVRDriverContext` (the module-wide driver context of openvr_driver.h) and `vr::VRDriverContext()`.
//!
//! The context holds the property helpers (with the `IVRProperties*`), the hidden-area
//! helpers, settings, properties, the server driver host, the watchdog
//! host, the driver log, the driver manager, resources and the `IVRDriverContext*`.
//!
//! The provider and the watchdog share this context.

use crate::openvr::{self, EVRInitError, GetGenericInterfaceFn, Interface};
use std::ffi::c_char;
use std::ptr::null_mut;

pub struct DriverContext {
    pub property_helpers: *mut Interface,
    pub hidden_area_helpers: *mut Interface,
    pub settings: *mut Interface,
    pub properties: *mut Interface,
    pub server_driver_host: *mut Interface,
    pub watchdog_host: *mut Interface,
    pub reserved_40: *mut Interface,
    pub driver_log: *mut Interface,
    pub driver_manager: *mut Interface,
    pub resources: *mut Interface,
    pub reserved_60: [*mut Interface; 3],
    pub context: *mut Interface,
}

pub struct ContextCell(std::cell::UnsafeCell<DriverContext>);
// SAFETY: vrserver drives the provider from its own threads, one call at a time;
// no locking is added.
unsafe impl Sync for ContextCell {}

pub static CONTEXT: ContextCell = ContextCell(std::cell::UnsafeCell::new(DriverContext {
    property_helpers: null_mut(),
    hidden_area_helpers: null_mut(),
    settings: null_mut(),
    properties: null_mut(),
    server_driver_host: null_mut(),
    watchdog_host: null_mut(),
    reserved_40: null_mut(),
    driver_log: null_mut(),
    driver_manager: null_mut(),
    resources: null_mut(),
    reserved_60: [null_mut(); 3],
    context: null_mut(),
}));

/// The module context.
///
/// # Safety
/// Callers must not keep two references alive at once (single-threaded use).
#[allow(clippy::mut_from_ref)]
pub unsafe fn ctx() -> &'static mut DriverContext {
    &mut *CONTEXT.0.get()
}

/// What dereferencing a NULL interface pointer does: an access violation, i.e. SIGSEGV on Linux (the crash
/// handler logs it and aborts).
pub fn null_interface_call() -> ! {
    // SAFETY: raising a signal on the current thread.
    unsafe {
        libc::raise(libc::SIGSEGV);
        libc::abort()
    }
}

impl DriverContext {
    /// `COpenVRDriverContext::Clear()` (the property helper pointers are not part of it).
    pub fn clear(&mut self) {
        self.settings = null_mut();
        self.properties = null_mut();
        self.server_driver_host = null_mut();
        self.watchdog_host = null_mut();
        self.reserved_40 = null_mut();
        self.driver_log = null_mut();
        self.driver_manager = null_mut();
        self.resources = null_mut();
        self.reserved_60 = [null_mut(); 3];
    }

    /// `VRDriverContext()->GetGenericInterface(version, &err)`.
    pub fn get_generic_interface(&mut self, version: &[u8], err: &mut EVRInitError) -> *mut Interface {
        let c = self.context;
        if c.is_null() {
            null_interface_call();
        }
        // SAFETY: `c` is the IVRDriverContext vrserver passed to Init; slot 0 is GetGenericInterface.
        unsafe {
            let f: GetGenericInterfaceFn = std::mem::transmute(openvr::slot(c, openvr::CONTEXT_GET_GENERIC_INTERFACE));
            f(c, version.as_ptr() as *const c_char, err)
        }
    }

    /// `vr::VRServerDriverHost()`
    pub fn vr_server_driver_host(&mut self, err: &mut EVRInitError) -> *mut Interface {
        if self.server_driver_host.is_null() {
            self.server_driver_host = self.get_generic_interface(openvr::IVR_SERVER_DRIVER_HOST_VERSION, err);
        }
        self.server_driver_host
    }

    /// `vr::VRProperties()`: fetches the interface while none is stored. Returns the pointer the
    /// property helper holds and calls through
    /// (NULL when the context had no IVRProperties_001; calling through it crashes).
    pub fn vr_properties(&mut self, err: &mut EVRInitError) -> *mut Interface {
        if self.properties.is_null() {
            let p = self.get_generic_interface(openvr::IVR_PROPERTIES_VERSION, err);
            self.property_helpers = p;
            self.hidden_area_helpers = p;
            self.properties = p;
        }
        self.property_helpers
    }

    /// `vr::VRSettings()`
    pub fn vr_settings(&mut self, err: &mut EVRInitError) -> *mut Interface {
        if self.settings.is_null() {
            self.settings = self.get_generic_interface(openvr::IVR_SETTINGS_VERSION, err);
        }
        self.settings
    }

    /// `vr::VRDriverLog()`
    pub fn vr_driver_log(&mut self, err: &mut EVRInitError) -> *mut Interface {
        if self.driver_log.is_null() {
            self.driver_log = self.get_generic_interface(openvr::IVR_DRIVER_LOG_VERSION, err);
        }
        self.driver_log
    }

    /// `vr::VRDriverManager()`
    pub fn vr_driver_manager(&mut self, err: &mut EVRInitError) -> *mut Interface {
        if self.driver_manager.is_null() {
            self.driver_manager = self.get_generic_interface(openvr::IVR_DRIVER_MANAGER_VERSION, err);
        }
        self.driver_manager
    }

    /// `vr::VRResources()`
    pub fn vr_resources(&mut self, err: &mut EVRInitError) -> *mut Interface {
        if self.resources.is_null() {
            self.resources = self.get_generic_interface(openvr::IVR_RESOURCES_VERSION, err);
        }
        self.resources
    }

    /// `vr::VRWatchdogHost()`
    pub fn vr_watchdog_host(&mut self, err: &mut EVRInitError) -> *mut Interface {
        if self.watchdog_host.is_null() {
            self.watchdog_host = self.get_generic_interface(openvr::IVR_WATCHDOG_HOST_VERSION, err);
        }
        self.watchdog_host
    }

    /// `VR_INIT_SERVER_DRIVER_CONTEXT(pContext)`: Clear, store the context, `InitServer()`.
    pub fn init_server(&mut self, context: *mut Interface) -> EVRInitError {
        self.clear();
        self.context = context;
        let mut e = [0 as EVRInitError; 6];
        // The properties interface is not tested for NULL here, whatever the context answered.
        let ok = !self.vr_server_driver_host(&mut e[0]).is_null()
            && !self.vr_settings(&mut e[1]).is_null()
            && {
                self.vr_properties(&mut e[2]);
                true
            }
            && !self.vr_driver_log(&mut e[3]).is_null()
            && !self.vr_driver_manager(&mut e[4]).is_null()
            && !self.vr_resources(&mut e[5]).is_null();
        if ok {
            openvr::VR_INIT_ERROR_NONE
        } else {
            openvr::VR_INIT_ERROR_INIT_INTERFACE_NOT_FOUND
        }
    }

    /// `VR_INIT_WATCHDOG_DRIVER_CONTEXT(pContext)`: Clear, store the context, `InitWatchdog()`.
    pub fn init_watchdog(&mut self, context: *mut Interface) -> EVRInitError {
        self.clear();
        self.context = context;
        let mut e = [0 as EVRInitError; 3];
        let ok = !self.vr_watchdog_host(&mut e[0]).is_null()
            && !self.vr_settings(&mut e[1]).is_null()
            && !self.vr_driver_log(&mut e[2]).is_null();
        if ok {
            openvr::VR_INIT_ERROR_NONE
        } else {
            openvr::VR_INIT_ERROR_INIT_INTERFACE_NOT_FOUND
        }
    }

    /// `VR_CLEANUP_WATCHDOG_DRIVER_CONTEXT()`: Clear and forget the context.
    pub fn cleanup_watchdog(&mut self) {
        self.context = null_mut();
        self.clear();
    }
}
