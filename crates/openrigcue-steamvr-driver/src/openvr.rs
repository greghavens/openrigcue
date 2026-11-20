//! The slice of the OpenVR driver ABI this driver uses, written as Rust.
//!
//! Written from `headers/openvr_driver.h` of ValveSoftware/openvr at commit
//! 0924064316de3effbcd1acf1e309182a2deb1c05 (SDK 2.15.6). That header is
//! Copyright (c) 2015, Valve Corporation, under the BSD-3-Clause licence; only type layouts,
//! enum values, interface version strings and vtable slot orders are reproduced here.
//!
//! ABI notes (Linux x86-64, Itanium C++ ABI, as used by `vrserver`):
//! - An interface object's first word points at its vtable's address point; virtual function `n`
//!   (in declaration order, these interfaces have no virtual destructor) is the `n`-th word there.
//! - A virtual member function is called like a SysV C function with `this` as first argument,
//!   so `extern "C" fn(this: *mut c_void, ...)` matches. C++ `bool` is Rust `bool`, C++ enums are
//!   4-byte `int`s, and `const T&` arguments are passed as pointers.
//! - The struct layouts are checked by tests against offsets printed by g++ from the real header.

use std::ffi::{c_char, c_void, CStr};

/// `vr::HmdQuaternion_t`: `{ w, x, y, z }` doubles.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct HmdQuaternion {
    pub w: f64,
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

/// `vr::ETrackingResult::TrackingResult_Running_OK`.
pub const TRACKING_RESULT_RUNNING_OK: i32 = 200;

/// `vr::DriverPose_t`. 280 bytes, 8-byte aligned.
///
/// Positions and velocities are in the driver's world space (metres, +x right, +y up, +z back),
/// angular velocity and acceleration too ("in driver world space", Driver_API_Documentation.md).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DriverPose {
    pub pose_time_offset: f64,
    pub q_world_from_driver_rotation: HmdQuaternion,
    pub vec_world_from_driver_translation: [f64; 3],
    pub q_driver_from_head_rotation: HmdQuaternion,
    pub vec_driver_from_head_translation: [f64; 3],
    pub vec_position: [f64; 3],
    pub vec_velocity: [f64; 3],
    pub vec_acceleration: [f64; 3],
    pub q_rotation: HmdQuaternion,
    pub vec_angular_velocity: [f64; 3],
    pub vec_angular_acceleration: [f64; 3],
    pub result: i32,
    pub pose_is_valid: bool,
    pub will_drift_in_yaw: bool,
    pub should_apply_head_model: bool,
    pub device_is_connected: bool,
}

impl Default for DriverPose {
    fn default() -> Self {
        let identity = HmdQuaternion { w: 1.0, ..HmdQuaternion::default() };
        DriverPose {
            pose_time_offset: 0.0,
            q_world_from_driver_rotation: identity,
            vec_world_from_driver_translation: [0.0; 3],
            q_driver_from_head_rotation: identity,
            vec_driver_from_head_translation: [0.0; 3],
            vec_position: [0.0; 3],
            vec_velocity: [0.0; 3],
            vec_acceleration: [0.0; 3],
            q_rotation: identity,
            vec_angular_velocity: [0.0; 3],
            vec_angular_acceleration: [0.0; 3],
            result: TRACKING_RESULT_RUNNING_OK,
            pose_is_valid: true,
            will_drift_in_yaw: false,
            should_apply_head_model: false,
            device_is_connected: true,
        }
    }
}

/// `vr::ETrackedDeviceClass`.
pub mod device_class {
    pub const INVALID: i32 = 0;
    pub const HMD: i32 = 1;
    pub const CONTROLLER: i32 = 2;
    pub const GENERIC_TRACKER: i32 = 3;
    pub const TRACKING_REFERENCE: i32 = 4;
    pub const DISPLAY_REDIRECT: i32 = 5;
}

/// `vr::k_unTrackedDeviceIndex_Hmd`.
pub const TRACKED_DEVICE_INDEX_HMD: u32 = 0;

/// `vr::EVRInitError` values used here.
pub mod init_error {
    pub const NONE: i32 = 0;
    pub const INIT_INTERFACE_NOT_FOUND: i32 = 105;
    pub const INIT_INTERNAL: i32 = 124;
}

/// `vr::Prop_DeviceClass_Int32`.
pub const PROP_DEVICE_CLASS_INT32: i32 = 1029;
/// `vr::k_unInt32PropertyTag`.
pub const INT32_PROPERTY_TAG: u32 = 2;
/// `vr::TrackedProp_Success`.
pub const TRACKED_PROP_SUCCESS: i32 = 0;

/// `vr::PropertyRead_t`. 32 bytes.
#[repr(C)]
#[derive(Debug)]
pub struct PropertyRead {
    pub prop: i32,
    pub pv_buffer: *mut c_void,
    pub un_buffer_size: u32,
    pub un_tag: u32,
    pub un_required_buffer_size: u32,
    pub e_error: i32,
}

/// Interface version strings (NUL-terminated so they can be handed to C directly).
pub mod version {
    pub const SERVER_TRACKED_DEVICE_PROVIDER: &[u8] = b"IServerTrackedDeviceProvider_004\0";
    pub const SERVER_DRIVER_HOST_006: &[u8] = b"IVRServerDriverHost_006\0";
    pub const SERVER_DRIVER_HOST_005: &[u8] = b"IVRServerDriverHost_005\0";
    pub const SERVER_DRIVER_HOST_004: &[u8] = b"IVRServerDriverHost_004\0";
    pub const PROPERTIES: &[u8] = b"IVRProperties_001\0";
    pub const DRIVER_LOG: &[u8] = b"IVRDriverLog_001\0";

    /// Host interface versions whose slot 0 is `TrackedDeviceAdded` and slot 1 is
    /// `TrackedDevicePoseUpdated(uint32_t, const DriverPose_t&, uint32_t)`. Checked against the
    /// 1.5.17 (005) and 2.x (006) headers; 004 has the same two slots.
    pub const POSE_HOSTS: [&[u8]; 3] = [SERVER_DRIVER_HOST_006, SERVER_DRIVER_HOST_005, SERVER_DRIVER_HOST_004];
}

/// `vr::k_InterfaceVersions` from the header this ABI slice was taken from: what a driver built
/// against SDK 2.15.6 reports through `GetInterfaceVersions`.
pub const INTERFACE_VERSIONS: [&[u8]; 12] = [
    b"IVRSettings_003\0",
    b"ITrackedDeviceServerDriver_005\0",
    b"IVRDisplayComponent_003\0",
    b"IVRDriverDirectModeComponent_009\0",
    b"IVRCameraComponent_003\0",
    b"IServerTrackedDeviceProvider_004\0",
    b"IVRWatchdogProvider_001\0",
    b"IVRVirtualDisplay_002\0",
    b"IVRDriverManager_001\0",
    b"IVRResources_001\0",
    b"IVRCompositorPluginProvider_001\0",
    b"IVRIPCResourceManagerClient_003\0",
];

/// Vtable slot indices.
pub mod slot {
    /// `IVRDriverContext::GetGenericInterface(const char*, EVRInitError*)`.
    pub const CONTEXT_GET_GENERIC_INTERFACE: usize = 0;
    /// `IVRServerDriverHost::TrackedDevicePoseUpdated(uint32_t, const DriverPose_t&, uint32_t)`.
    pub const HOST_TRACKED_DEVICE_POSE_UPDATED: usize = 1;
    /// `IVRProperties::ReadPropertyBatch(PropertyContainerHandle_t, PropertyRead_t*, uint32_t)`.
    pub const PROPERTIES_READ_PROPERTY_BATCH: usize = 0;
    /// `IVRProperties::TrackedDeviceToPropertyContainer(TrackedDeviceIndex_t)`.
    pub const PROPERTIES_TRACKED_DEVICE_TO_PROPERTY_CONTAINER: usize = 3;
    /// `IVRDriverLog::Log(const char*)`.
    pub const DRIVER_LOG_LOG: usize = 0;
}

pub type GetGenericInterfaceFn =
    unsafe extern "C" fn(this: *mut c_void, version: *const c_char, error: *mut i32) -> *mut c_void;
pub type TrackedDevicePoseUpdatedFn =
    unsafe extern "C" fn(this: *mut c_void, device: u32, pose: *const DriverPose, size: u32);
pub type ReadPropertyBatchFn =
    unsafe extern "C" fn(this: *mut c_void, container: u64, batch: *mut PropertyRead, count: u32) -> i32;
pub type TrackedDeviceToPropertyContainerFn = unsafe extern "C" fn(this: *mut c_void, device: u32) -> u64;
pub type DriverLogFn = unsafe extern "C" fn(this: *mut c_void, message: *const c_char);

/// Converts one of the NUL-terminated byte strings above into a `CStr`.
pub fn cstr(bytes: &'static [u8]) -> &'static CStr {
    CStr::from_bytes_with_nul(bytes).expect("interface version strings are NUL-terminated")
}

/// Returns the address of slot `slot` of the vtable of the interface object `object`.
///
/// # Safety
/// `object` must point at a live C++ object whose class has at least `slot + 1` virtual functions.
pub unsafe fn vtable_slot_address(object: *mut c_void, slot: usize) -> *mut usize {
    // SAFETY: by the caller's contract the first word of `object` is its vtable pointer and the
    // vtable has more than `slot` entries; this computes an address without reading the slot.
    unsafe {
        let vtable = *(object as *const *mut usize);
        vtable.add(slot)
    }
}

/// Reads the function pointer currently in slot `slot` of `object`'s vtable.
///
/// # Safety
/// As for [`vtable_slot_address`].
pub unsafe fn vtable_entry(object: *mut c_void, slot: usize) -> usize {
    // SAFETY: the slot address is valid per the caller's contract; vtable entries are word-aligned.
    // Read atomically because another hooker in the process may be rewriting it.
    unsafe {
        let address = vtable_slot_address(object, slot);
        std::sync::atomic::AtomicUsize::from_ptr(address).load(std::sync::atomic::Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{align_of, offset_of, size_of};

    /// Offsets printed by g++ 16 from the real `openvr_driver.h` (SDK 2.15.6) with `offsetof`.
    #[test]
    fn driver_pose_layout_matches_the_header() {
        assert_eq!(size_of::<DriverPose>(), 280);
        assert_eq!(align_of::<DriverPose>(), 8);
        assert_eq!(offset_of!(DriverPose, q_world_from_driver_rotation), 8);
        assert_eq!(offset_of!(DriverPose, vec_world_from_driver_translation), 40);
        assert_eq!(offset_of!(DriverPose, q_driver_from_head_rotation), 64);
        assert_eq!(offset_of!(DriverPose, vec_driver_from_head_translation), 96);
        assert_eq!(offset_of!(DriverPose, vec_position), 120);
        assert_eq!(offset_of!(DriverPose, vec_velocity), 144);
        assert_eq!(offset_of!(DriverPose, vec_acceleration), 168);
        assert_eq!(offset_of!(DriverPose, q_rotation), 192);
        assert_eq!(offset_of!(DriverPose, vec_angular_velocity), 224);
        assert_eq!(offset_of!(DriverPose, vec_angular_acceleration), 248);
        assert_eq!(offset_of!(DriverPose, result), 272);
        assert_eq!(offset_of!(DriverPose, pose_is_valid), 276);
        assert_eq!(offset_of!(DriverPose, will_drift_in_yaw), 277);
        assert_eq!(offset_of!(DriverPose, should_apply_head_model), 278);
        assert_eq!(offset_of!(DriverPose, device_is_connected), 279);
    }

    #[test]
    fn property_read_layout_matches_the_header() {
        assert_eq!(size_of::<PropertyRead>(), 32);
        assert_eq!(offset_of!(PropertyRead, pv_buffer), 8);
        assert_eq!(offset_of!(PropertyRead, un_buffer_size), 16);
        assert_eq!(offset_of!(PropertyRead, un_tag), 20);
        assert_eq!(offset_of!(PropertyRead, un_required_buffer_size), 24);
        assert_eq!(offset_of!(PropertyRead, e_error), 28);
    }

    #[test]
    fn version_strings_are_c_strings() {
        for v in INTERFACE_VERSIONS.iter().chain(version::POSE_HOSTS.iter()) {
            assert!(cstr(v).to_bytes().iter().all(|b| b.is_ascii_graphic()));
        }
    }
}
