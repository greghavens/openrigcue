//! The tracked device: the virtual tracker whose pose is the motion
//! rig's pose from `Local\SimRacingStudioMotionRigPose`, offset by OpenVR-MotionCompensation's
//! `Local\OVRMC_MMFv1` when that section exists.

use crate::context::{ctx, null_interface_call};
use crate::elpp::{self, config::Level};
use crate::math::{quaternion_from_yaw_roll_pitch, rig_pose};
use crate::openvr::{self, DriverPose, EVRInitError, HmdQuaternion, Interface, PropertyWrite};
use crate::section::{self, Handle, InPageError, NULL_HANDLE};
use std::ffi::{c_char, c_void};

pub const SERIAL_NUMBER: &[u8] = b"SimRacingStudioVirtualTracker 1.0.0.1";
pub const MODEL_NUMBER: &[u8] = b"SimRacingStudioVirtualTracker";
pub const RENDER_MODEL_NAME: &[u8] = b"locator";

/// The device's virtual table: `ITrackedDeviceServerDriver_005` slots 0..5 (the ones vrserver calls), then the
/// device's own virtuals: slot 6 the scalar deleting destructor (which
/// the provider's clean-up calls with flags 1), slot 7 the power-off.
#[repr(C)]
pub struct DeviceVtable {
    pub activate: unsafe extern "C" fn(*mut Device, u32) -> EVRInitError,
    pub deactivate: unsafe extern "C" fn(*mut Device),
    pub enter_standby: unsafe extern "C" fn(*mut Device),
    pub get_component: unsafe extern "C" fn(*mut Device, *const c_char) -> *mut c_void,
    pub debug_request: unsafe extern "C" fn(*mut Device, *const c_char, *mut c_char, u32),
    pub get_pose: unsafe extern "C" fn(*mut Device) -> DriverPose,
    pub scalar_deleting_destructor: unsafe extern "C" fn(*mut Device, u32) -> *mut Device,
    pub power_off: unsafe extern "C" fn(*mut Device),
}

pub static DEVICE_VTABLE: DeviceVtable = DeviceVtable {
    activate: device_activate,
    deactivate: device_deactivate,
    enter_standby: device_enter_standby,
    get_component: device_get_component,
    debug_request: device_debug_request,
    get_pose: device_get_pose,
    scalar_deleting_destructor: device_scalar_deleting_destructor,
    power_off: device_power_off,
};

#[repr(C)]
pub struct Device {
    pub(crate) vtable: *const DeviceVtable,
    /// The view of the rig pose section.
    pub rig_pose: Handle,
    /// The current pose.
    pub pose: DriverPose,
    /// The rig offset.
    pub rig_offset: [f64; 3],
    /// The rig yaw offset.
    pub rig_yaw_offset: f64,
    pub mmf_file_mover: Handle,
    pub map_file_mover: Handle,
    pub mover_connected: bool,
    pub mmf_file_ovrmc: Handle,
    pub map_file_ovrmc: Handle,
    pub data_ovrmc: Handle,
    pub ovrmc_connected: bool,
    pub object_id: u32,
    pub property_container: u64,
    pub serial_number: Vec<u8>,
    pub model_number: Vec<u8>,
    /// The pose submitted last; the error path of `run_frame` submits it again.
    pub last_submitted: DriverPose,
}

impl Device {
    /// A new device.
    pub fn new() -> Box<Device> {
        // The identity rotations and zero translations; everything else is zero.
        let identity = HmdQuaternion { w: 1.0, x: 0.0, y: 0.0, z: 0.0 };
        let pose = DriverPose {
            pose_time_offset: 0.0,
            q_world_from_driver_rotation: identity,
            q_driver_from_head_rotation: identity,
            q_rotation: identity,
            vec_driver_from_head_translation: [0.0; 3],
            vec_world_from_driver_translation: [0.0; 3],
            vec_position: [0.0; 3],
            ..DriverPose::default()
        };
        let dev = Box::new(Device {
            vtable: &DEVICE_VTABLE,
            rig_pose: NULL_HANDLE,
            pose,
            rig_offset: [0.0; 3],
            rig_yaw_offset: 0.0,
            mmf_file_mover: NULL_HANDLE,
            map_file_mover: NULL_HANDLE,
            mover_connected: false,
            mmf_file_ovrmc: NULL_HANDLE,
            map_file_ovrmc: NULL_HANDLE,
            data_ovrmc: NULL_HANDLE,
            ovrmc_connected: false,
            object_id: openvr::TRACKED_DEVICE_INDEX_INVALID,
            property_container: 0,
            serial_number: SERIAL_NUMBER.to_vec(),
            model_number: MODEL_NUMBER.to_vec(),
            last_submitted: DriverPose::default(),
        });
        elpp::log(Level::Trace, b"CMotionPoseControllerDriver()");
        dev
    }

    /// The serial number as a NUL-terminated copy.
    pub fn serial_number_c(&self) -> Vec<u8> {
        let mut s = self.serial_number.clone();
        s.push(0);
        s
    }

    /// Opens the named section and maps a view of it; sets `connected` and logs on success.
    fn open_mmf(map_file: &mut Handle, mmf_file: &mut Handle, name: &str, connected: &mut bool) -> bool {
        *map_file = section::open_file_mapping(name);
        if *map_file == NULL_HANDLE {
            return false;
        }
        *mmf_file = section::map_view_of_file(*map_file);
        if *mmf_file == NULL_HANDLE {
            section::close_handle(*map_file);
            return false;
        }
        *connected = true;
        let mut msg = b"Successfully connected to ".to_vec();
        msg.extend_from_slice(name.as_bytes());
        elpp::log(Level::Info, &msg);
        true
    }

    /// The current pose. `Err` is an in-page error while reading a section.
    pub fn get_pose(&mut self) -> Result<DriverPose, InPageError> {
        self.pose.pose_is_valid = false;
        self.pose.device_is_connected = self.mover_connected;
        self.pose.should_apply_head_model = false;
        self.pose.will_drift_in_yaw = false;

        if !self.ovrmc_connected {
            if Self::open_mmf(
                &mut self.map_file_ovrmc,
                &mut self.mmf_file_ovrmc,
                section::OVRMC_SECTION,
                &mut self.ovrmc_connected,
            ) {
                self.data_ovrmc = self.mmf_file_ovrmc;
            }
        } else if self.data_ovrmc != NULL_HANDLE {
            // The OVRMC section: translation (3 doubles at 0), rotation (3 doubles at 24); the yaw is at 32.
            let d = section::read_view::<40>(self.data_ovrmc)?;
            self.rig_offset = [section::f64_at(&d, 0), section::f64_at(&d, 1), section::f64_at(&d, 2)];
            self.rig_yaw_offset = section::f64_at(&d, 4);
        }

        if !self.mover_connected {
            self.pose.result = openvr::TRACKING_RESULT_CALIBRATING_IN_PROGRESS;
            self.pose.q_rotation = quaternion_from_yaw_roll_pitch(self.rig_yaw_offset, 0.0, 0.0);
            if !Self::open_mmf(
                &mut self.map_file_mover,
                &mut self.mmf_file_mover,
                section::RIG_POSE_SECTION,
                &mut self.mover_connected,
            ) {
                return Ok(self.pose);
            }
            self.rig_pose = self.mmf_file_mover;
        }

        if self.rig_pose != NULL_HANDLE {
            // The rig pose section: sway, surge, heave, yaw, roll, pitch (doubles).
            let r = section::read_view::<48>(self.rig_pose)?;
            let rig: [f64; 6] = std::array::from_fn(|i| section::f64_at(&r, i));
            let (q, position) = rig_pose(&rig, &self.rig_offset, self.rig_yaw_offset);
            self.pose.q_rotation = q;
            self.pose.vec_position = position;
            self.pose.pose_is_valid = true;
            self.pose.result = openvr::TRACKING_RESULT_RUNNING_OK;
        }
        Ok(self.pose)
    }

    /// One frame: takes the pose and submits it to vrserver.
    pub fn run_frame(&mut self) {
        let new_pose = match self.get_pose() {
            Ok(p) => p,
            Err(InPageError) => {
                // The last error is cleared before the failure is logged, so the
                // message ends after "MMF failed: Unknown exception ".
                // SAFETY: errno location is thread-local.
                unsafe { *libc::__errno_location() = 0 };
                // The log line is "MMF failed: Unknown exception " followed by the last-error text,
                // written up to its first NUL.
                let last_error = section::last_error_string();
                let mut msg = b"MMF failed: Unknown exception ".to_vec();
                msg.extend_from_slice(section::c_str(&last_error));
                elpp::log(Level::Error, &msg);
                if !section::close_handle(self.map_file_mover) {
                    let e = section::last_error();
                    elpp::log(Level::Error, format!("Failed to close Mover handle! Error: {e}").as_bytes());
                }
                if !section::close_handle(self.map_file_ovrmc) {
                    let e = section::last_error();
                    elpp::log(Level::Error, format!("Failed to close OVRMC handle! Error: {e}").as_bytes());
                }
                self.mover_connected = false;
                self.ovrmc_connected = false;
                // No new pose is available: the pose submitted last is submitted again, with the
                // result "calibrating in progress" and marked not valid.
                let mut p = self.last_submitted;
                p.pose_is_valid = false;
                p.result = openvr::TRACKING_RESULT_CALIBRATING_IN_PROGRESS;
                p
            }
        };
        self.last_submitted = new_pose;
        // SAFETY: single-threaded use of the module context.
        let c = unsafe { ctx() };
        let mut err: EVRInitError = 0;
        let host = c.vr_server_driver_host(&mut err);
        if host.is_null() {
            null_interface_call();
        }
        // SAFETY: host is the IVRServerDriverHost_006 object; slot 1 is TrackedDevicePoseUpdated.
        unsafe {
            let f: openvr::TrackedDevicePoseUpdatedFn =
                std::mem::transmute(openvr::slot(host, openvr::HOST_TRACKED_DEVICE_POSE_UPDATED));
            f(host, self.object_id, &new_pose, openvr::DRIVER_POSE_SIZE);
        }
    }

    /// Sets one property: a property batch of one write.
    fn write_property(&self, props: *mut Interface, prop: i32, buffer: *mut c_void, size: u32, tag: u32) {
        if props.is_null() {
            null_interface_call();
        }
        // Only the property, the write type, the buffer, its size and the tag are stored;
        // the two error fields are left for vrserver to
        // write. The batch is written field by field and those two are left alone.
        let mut batch = std::mem::MaybeUninit::<PropertyWrite>::uninit();
        let w = batch.as_mut_ptr();
        // SAFETY: field writes into our own allocation; the callee only receives the pointer. Props is the
        // IVRProperties_001 object; slot 1 is WritePropertyBatch.
        unsafe {
            std::ptr::addr_of_mut!((*w).prop).write(prop);
            std::ptr::addr_of_mut!((*w).write_type).write(openvr::PROPERTY_WRITE_SET);
            std::ptr::addr_of_mut!((*w).pv_buffer).write(buffer);
            std::ptr::addr_of_mut!((*w).un_buffer_size).write(size);
            std::ptr::addr_of_mut!((*w).un_tag).write(tag);
            let f: openvr::WritePropertyBatchFn =
                std::mem::transmute(openvr::slot(props, openvr::PROPERTIES_WRITE_PROPERTY_BATCH));
            f(props, self.property_container, w, 1);
        }
    }

    /// Activates the device: stores its object id and writes its properties.
    pub fn activate(&mut self, object_id: u32) -> EVRInitError {
        elpp::log(Level::Info, b"Activate motionPoseController");
        self.object_id = object_id;
        // SAFETY: single-threaded use of the module context.
        let c = unsafe { ctx() };
        let mut err: EVRInitError = 0;
        let props = c.vr_properties(&mut err);
        if props.is_null() {
            null_interface_call();
        }
        // SAFETY: slot 3 is TrackedDeviceToPropertyContainer.
        self.property_container = unsafe {
            let f: openvr::TrackedDeviceToPropertyContainerFn =
                std::mem::transmute(openvr::slot(props, openvr::PROPERTIES_TRACKED_DEVICE_TO_PROPERTY_CONTAINER));
            f(props, self.object_id)
        };

        let mut model = self.model_number.clone();
        model.push(0);
        let props = c.vr_properties(&mut err);
        self.write_property(
            props,
            openvr::PROP_MODEL_NUMBER_STRING,
            model.as_mut_ptr().cast(),
            model.len() as u32,
            openvr::STRING_PROPERTY_TAG,
        );
        let mut render_model = RENDER_MODEL_NAME.to_vec();
        render_model.push(0);
        let props = c.vr_properties(&mut err);
        self.write_property(
            props,
            openvr::PROP_RENDER_MODEL_NAME_STRING,
            render_model.as_mut_ptr().cast(),
            render_model.len() as u32,
            openvr::STRING_PROPERTY_TAG,
        );
        let props = c.vr_properties(&mut err);
        let mut universe: u64 = 2;
        self.write_property(
            props,
            openvr::PROP_CURRENT_UNIVERSE_ID_UINT64,
            (&mut universe as *mut u64).cast(),
            8,
            openvr::UINT64_PROPERTY_TAG,
        );
        let props = c.vr_properties(&mut err);
        let mut role: i32 = openvr::TRACKED_CONTROLLER_ROLE_OPT_OUT;
        self.write_property(
            props,
            openvr::PROP_CONTROLLER_ROLE_HINT_INT32,
            (&mut role as *mut i32).cast(),
            4,
            openvr::INT32_PROPERTY_TAG,
        );
        let props = c.vr_properties(&mut err);
        let mut class: i32 = openvr::TRACKED_DEVICE_CLASS_GENERIC_TRACKER;
        self.write_property(
            props,
            openvr::PROP_DEVICE_CLASS_INT32,
            (&mut class as *mut i32).cast(),
            4,
            openvr::INT32_PROPERTY_TAG,
        );
        openvr::VR_INIT_ERROR_NONE
    }

    pub fn as_interface(self: &mut Box<Self>) -> *mut c_void {
        (&mut **self as *mut Device).cast()
    }
}

impl Drop for Device {
    /// Logs the destruction; handles and views stay open.
    fn drop(&mut self) {
        elpp::log(Level::Trace, b"~CMotionPoseControllerDriver()");
    }
}

unsafe extern "C" fn device_activate(this: *mut Device, object_id: u32) -> EVRInitError {
    (*this).activate(object_id)
}

/// Deactivates the device: its object id becomes invalid.
unsafe extern "C" fn device_deactivate(this: *mut Device) {
    (*this).object_id = openvr::TRACKED_DEVICE_INDEX_INVALID;
}

/// Entering standby does nothing.
unsafe extern "C" fn device_enter_standby(_this: *mut Device) {}

/// The device has no components: NULL.
unsafe extern "C" fn device_get_component(_this: *mut Device, _name: *const c_char) -> *mut c_void {
    std::ptr::null_mut()
}

/// A debug request is answered with an empty string.
unsafe extern "C" fn device_debug_request(_this: *mut Device, _req: *const c_char, buf: *mut c_char, size: u32) {
    if size >= 1 {
        *buf = 0;
    }
}

/// The pose, when vrserver asks for it: an in-page error here ends the
/// process (abort).
unsafe extern "C" fn device_get_pose(this: *mut Device) -> DriverPose {
    match (*this).get_pose() {
        Ok(p) => p,
        Err(InPageError) => libc::abort(),
    }
}

/// The scalar deleting destructor: runs the destructor, then frees
/// the object when bit 0 of `flags` is set; returns `this`.
unsafe extern "C" fn device_scalar_deleting_destructor(this: *mut Device, flags: u32) -> *mut Device {
    if flags & 1 != 0 {
        drop(Box::from_raw(this));
    } else {
        std::ptr::drop_in_place(this);
    }
    this
}

/// Powering off does nothing.
unsafe extern "C" fn device_power_off(_this: *mut Device) {}
