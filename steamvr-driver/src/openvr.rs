//! The slice of the OpenVR driver ABI (openvr_driver.h, SteamVR 1.14.15 headers)
//! that driver_simracingstudio uses: types, constants and the vtable slots it calls or provides.
//!
//! Linux/x86_64 uses the Itanium C++ ABI: an object starts with a pointer to its vtable, `this` is the first
//! argument, and a `DriverPose_t` returned by value goes through a hidden pointer passed before `this` (which is
//! exactly what `extern "C" fn(*mut T) -> DriverPose` does for a 280-byte `#[repr(C)]` struct).

use std::ffi::{c_char, c_void};

/// `vr::EVRInitError`
pub type EVRInitError = i32;
pub const VR_INIT_ERROR_NONE: EVRInitError = 0;
/// `VRInitError_Init_InterfaceNotFound` (0x69)
pub const VR_INIT_ERROR_INIT_INTERFACE_NOT_FOUND: EVRInitError = 105;

/// `vr::k_unTrackedDeviceIndexInvalid`
pub const TRACKED_DEVICE_INDEX_INVALID: u32 = 0xFFFF_FFFF;
/// `vr::TrackedDeviceClass_Controller` (what `Init` passes to `TrackedDeviceAdded`)
pub const TRACKED_DEVICE_CLASS_CONTROLLER: i32 = 2;
/// `vr::TrackedDeviceClass_GenericTracker` (written to `Prop_DeviceClass_Int32`)
pub const TRACKED_DEVICE_CLASS_GENERIC_TRACKER: i32 = 3;
/// `vr::TrackedControllerRole_OptOut`
pub const TRACKED_CONTROLLER_ROLE_OPT_OUT: i32 = 3;

/// `vr::TrackingResult_Calibrating_InProgress`
pub const TRACKING_RESULT_CALIBRATING_IN_PROGRESS: i32 = 100;
/// `vr::TrackingResult_Running_OK`
pub const TRACKING_RESULT_RUNNING_OK: i32 = 200;

pub const PROP_MODEL_NUMBER_STRING: i32 = 1001;
pub const PROP_RENDER_MODEL_NAME_STRING: i32 = 1003;
pub const PROP_DEVICE_CLASS_INT32: i32 = 1029;
pub const PROP_CURRENT_UNIVERSE_ID_UINT64: i32 = 2004;
pub const PROP_CONTROLLER_ROLE_HINT_INT32: i32 = 3007;

pub const PROPERTY_WRITE_SET: i32 = 0;
pub const INT32_PROPERTY_TAG: u32 = 2;
pub const UINT64_PROPERTY_TAG: u32 = 3;
pub const STRING_PROPERTY_TAG: u32 = 5;

/// `vr::HmdQuaternion_t` (w, x, y, z)
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct HmdQuaternion {
    pub w: f64,
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

/// `vr::DriverPose_t`, 280 (0x118) bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Default)]
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

pub const DRIVER_POSE_SIZE: u32 = std::mem::size_of::<DriverPose>() as u32;

impl DriverPose {
    /// The 280 bytes as vrserver receives them.
    pub fn to_bytes(&self) -> [u8; 280] {
        let mut out = [0u8; 280];
        let mut put = |off: usize, v: f64| out[off..off + 8].copy_from_slice(&v.to_le_bytes());
        put(0x00, self.pose_time_offset);
        let quat = |q: &HmdQuaternion| [q.w, q.x, q.y, q.z];
        for (i, v) in quat(&self.q_world_from_driver_rotation).iter().enumerate() {
            put(0x08 + 8 * i, *v);
        }
        for (i, v) in self.vec_world_from_driver_translation.iter().enumerate() {
            put(0x28 + 8 * i, *v);
        }
        for (i, v) in quat(&self.q_driver_from_head_rotation).iter().enumerate() {
            put(0x40 + 8 * i, *v);
        }
        for (i, v) in self.vec_driver_from_head_translation.iter().enumerate() {
            put(0x60 + 8 * i, *v);
        }
        for (i, v) in self.vec_position.iter().enumerate() {
            put(0x78 + 8 * i, *v);
        }
        for (i, v) in self.vec_velocity.iter().enumerate() {
            put(0x90 + 8 * i, *v);
        }
        for (i, v) in self.vec_acceleration.iter().enumerate() {
            put(0xa8 + 8 * i, *v);
        }
        for (i, v) in quat(&self.q_rotation).iter().enumerate() {
            put(0xc0 + 8 * i, *v);
        }
        for (i, v) in self.vec_angular_velocity.iter().enumerate() {
            put(0xe0 + 8 * i, *v);
        }
        for (i, v) in self.vec_angular_acceleration.iter().enumerate() {
            put(0xf8 + 8 * i, *v);
        }
        out[0x110..0x114].copy_from_slice(&self.result.to_le_bytes());
        out[0x114] = self.pose_is_valid as u8;
        out[0x115] = self.will_drift_in_yaw as u8;
        out[0x116] = self.should_apply_head_model as u8;
        out[0x117] = self.device_is_connected as u8;
        out
    }
}

/// `vr::PropertyWrite_t` (40 bytes, natural alignment: openvr_driver.h declares it outside any `#pragma pack`).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PropertyWrite {
    pub prop: i32,
    pub write_type: i32,
    pub e_set_error: i32,
    pub pv_buffer: *mut c_void,
    pub un_buffer_size: u32,
    pub un_tag: u32,
    pub e_error: i32,
}

/// Any C++ interface object handed over by vrserver: its first word is the vtable pointer.
#[repr(C)]
pub struct Interface {
    pub vtable: *const *const c_void,
}

/// Slot `n` of the vtable of interface object `obj`.
///
/// # Safety
/// `obj` must point to a live C++ object with at least `n + 1` virtual functions.
pub unsafe fn slot(obj: *mut Interface, n: usize) -> *const c_void {
    *(*obj).vtable.add(n)
}

// `IVRDriverContext`
pub const CONTEXT_GET_GENERIC_INTERFACE: usize = 0;
pub type GetGenericInterfaceFn =
    unsafe extern "C" fn(*mut Interface, *const c_char, *mut EVRInitError) -> *mut Interface;

// `IVRServerDriverHost_006`
pub const HOST_TRACKED_DEVICE_ADDED: usize = 0;
pub const HOST_TRACKED_DEVICE_POSE_UPDATED: usize = 1;
pub type TrackedDeviceAddedFn = unsafe extern "C" fn(*mut Interface, *const c_char, i32, *mut c_void) -> bool;
pub type TrackedDevicePoseUpdatedFn = unsafe extern "C" fn(*mut Interface, u32, *const DriverPose, u32);

// `IVRProperties_001`
pub const PROPERTIES_WRITE_PROPERTY_BATCH: usize = 1;
pub const PROPERTIES_TRACKED_DEVICE_TO_PROPERTY_CONTAINER: usize = 3;
pub type WritePropertyBatchFn = unsafe extern "C" fn(*mut Interface, u64, *mut PropertyWrite, u32) -> i32;
pub type TrackedDeviceToPropertyContainerFn = unsafe extern "C" fn(*mut Interface, u32) -> u64;

// Interface version strings the driver requests or answers (openvr_driver.h 1.14.15).
pub const ISERVER_TRACKED_DEVICE_PROVIDER_VERSION: &[u8] = b"IServerTrackedDeviceProvider_004\0";
pub const IVR_WATCHDOG_PROVIDER_VERSION: &[u8] = b"IVRWatchdogProvider_001\0";
pub const IVR_SERVER_DRIVER_HOST_VERSION: &[u8] = b"IVRServerDriverHost_006\0";
pub const IVR_SETTINGS_VERSION: &[u8] = b"IVRSettings_003\0";
pub const IVR_PROPERTIES_VERSION: &[u8] = b"IVRProperties_001\0";
pub const IVR_DRIVER_LOG_VERSION: &[u8] = b"IVRDriverLog_001\0";
pub const IVR_DRIVER_MANAGER_VERSION: &[u8] = b"IVRDriverManager_001\0";
pub const IVR_RESOURCES_VERSION: &[u8] = b"IVRResources_001\0";
pub const IVR_WATCHDOG_HOST_VERSION: &[u8] = b"IVRWatchdogHost_002\0";

/// The interface versions the driver answers with (`vr::k_InterfaceVersions`).
pub const INTERFACE_VERSIONS: [&[u8]; 11] = [
    b"IVRSettings_003\0",
    b"ITrackedDeviceServerDriver_005\0",
    b"IVRDisplayComponent_002\0",
    b"IVRDriverDirectModeComponent_007\0",
    b"IVRCameraComponent_003\0",
    b"IServerTrackedDeviceProvider_004\0",
    b"IVRWatchdogProvider_001\0",
    b"IVRVirtualDisplay_002\0",
    b"IVRDriverManager_001\0",
    b"IVRResources_001\0",
    b"IVRCompositorPluginProvider_001\0",
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    #[test]
    fn driver_pose_layout_matches_openvr() {
        assert_eq!(size_of::<DriverPose>(), 0x118);
        assert_eq!(offset_of!(DriverPose, q_world_from_driver_rotation), 0x08);
        assert_eq!(offset_of!(DriverPose, vec_world_from_driver_translation), 0x28);
        assert_eq!(offset_of!(DriverPose, q_driver_from_head_rotation), 0x40);
        assert_eq!(offset_of!(DriverPose, vec_driver_from_head_translation), 0x60);
        assert_eq!(offset_of!(DriverPose, vec_position), 0x78);
        assert_eq!(offset_of!(DriverPose, vec_velocity), 0x90);
        assert_eq!(offset_of!(DriverPose, vec_acceleration), 0xa8);
        assert_eq!(offset_of!(DriverPose, q_rotation), 0xc0);
        assert_eq!(offset_of!(DriverPose, vec_angular_velocity), 0xe0);
        assert_eq!(offset_of!(DriverPose, vec_angular_acceleration), 0xf8);
        assert_eq!(offset_of!(DriverPose, result), 0x110);
        assert_eq!(offset_of!(DriverPose, pose_is_valid), 0x114);
        assert_eq!(offset_of!(DriverPose, will_drift_in_yaw), 0x115);
        assert_eq!(offset_of!(DriverPose, should_apply_head_model), 0x116);
        assert_eq!(offset_of!(DriverPose, device_is_connected), 0x117);
    }

    #[test]
    fn driver_pose_bytes_are_the_memory_image() {
        let mut p = DriverPose::default();
        p.q_rotation.w = 1.0;
        p.vec_position = [1.5, -2.0, 3.25];
        p.result = 200;
        p.pose_is_valid = true;
        p.device_is_connected = true;
        // SAFETY: DriverPose is a plain repr(C) struct without padding.
        let image: [u8; 280] = unsafe { std::mem::transmute(p) };
        assert_eq!(image, p.to_bytes());
    }

    #[test]
    fn property_write_layout_matches_openvr() {
        assert_eq!(size_of::<PropertyWrite>(), 40);
        assert_eq!(offset_of!(PropertyWrite, write_type), 4);
        assert_eq!(offset_of!(PropertyWrite, e_set_error), 8);
        assert_eq!(offset_of!(PropertyWrite, pv_buffer), 16);
        assert_eq!(offset_of!(PropertyWrite, un_buffer_size), 24);
        assert_eq!(offset_of!(PropertyWrite, un_tag), 28);
        assert_eq!(offset_of!(PropertyWrite, e_error), 32);
    }
}
