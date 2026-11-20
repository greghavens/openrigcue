//! The subset of the SCS Software telemetry SDK ABI (`scssdk_*.h`, telemetry
//! API 1.00) the plugin uses. Every constant here is a value the
//! plugin passes or compares, and every
//! struct is laid out the way the SDK defines it; `layout_asserts` pins the
//! offsets for both the x86_64 and the i386 build.

use core::ffi::{c_char, c_void};

pub type ScsResult = i32;
pub type ScsContext = *mut c_void;

/// `SCS_RESULT_ok`.
pub const SCS_RESULT_OK: ScsResult = 0;
/// `SCS_RESULT_unsupported`: returned for any API version other than 1.00.
pub const SCS_RESULT_UNSUPPORTED: ScsResult = -1;
/// `SCS_RESULT_generic_error` (0xfffffff9): every other failure of init.
pub const SCS_RESULT_GENERIC_ERROR: ScsResult = -7;

/// `SCS_TELEMETRY_VERSION_1_00` = SCS_MAKE_VERSION(1, 0).
pub const SCS_TELEMETRY_VERSION_1_00: u32 = 0x0001_0000;
/// `SCS_U32_NIL`: the channel index of every registration.
pub const SCS_U32_NIL: u32 = 0xffff_ffff;

pub const SCS_TELEMETRY_EVENT_FRAME_START: u32 = 1;
pub const SCS_TELEMETRY_EVENT_PAUSED: u32 = 3;
pub const SCS_TELEMETRY_EVENT_STARTED: u32 = 4;
pub const SCS_TELEMETRY_EVENT_CONFIGURATION: u32 = 5;

/// `SCS_TELEMETRY_FRAME_START_FLAG_timer_restart`.
pub const SCS_TELEMETRY_FRAME_START_FLAG_TIMER_RESTART: u32 = 1;

/// `SCS_TELEMETRY_CHANNEL_FLAG_no_value`: the flags of every registration;
/// the game calls the callback with a NULL value when the
/// channel has none.
pub const SCS_TELEMETRY_CHANNEL_FLAG_NO_VALUE: u32 = 2;

pub const SCS_VALUE_TYPE_BOOL: u32 = 1;
pub const SCS_VALUE_TYPE_S32: u32 = 2;
pub const SCS_VALUE_TYPE_U32: u32 = 3;
pub const SCS_VALUE_TYPE_FLOAT: u32 = 5;
pub const SCS_VALUE_TYPE_FVECTOR: u32 = 7;
pub const SCS_VALUE_TYPE_DPLACEMENT: u32 = 11;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ScsDPlacement {
    pub position: [f64; 3],
    /// heading, pitch, roll (fractions of a turn as the SDK delivers them).
    pub orientation: [f32; 3],
    pub _padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union ScsValueData {
    pub value_bool: u8,
    pub value_s32: i32,
    pub value_u32: u32,
    pub value_u64: u64,
    pub value_float: f32,
    pub value_double: f64,
    pub value_fvector: [f32; 3],
    pub value_dvector: [f64; 3],
    pub value_dplacement: ScsDPlacement,
    pub value_string: *const c_char,
}

/// `scs_value_t`: type, 4 bytes padding, data at +8 (both builds read the
/// data at `value+8`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ScsValue {
    pub type_: u32,
    pub _padding: u32,
    pub value: ScsValueData,
}

/// `scs_named_value_t`: one configuration attribute. Stride 0x40 on x64,
/// 0x38 on i386.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ScsNamedValue {
    pub name: *const c_char,
    pub index: u32,
    pub value: ScsValue,
}

/// `scs_telemetry_configuration_t`.
#[repr(C)]
pub struct ScsTelemetryConfiguration {
    pub id: *const c_char,
    pub attributes: *const ScsNamedValue,
}

/// `scs_telemetry_frame_start_t`; the plugin only reads `flags` (+0) and
/// `paused_simulation_time` (+0x18).
#[repr(C)]
pub struct ScsTelemetryFrameStart {
    pub flags: u32,
    pub _padding: u32,
    pub render_time: u64,
    pub simulation_time: u64,
    pub paused_simulation_time: u64,
}

pub type ScsLog = unsafe extern "system" fn(log_type: i32, message: *const c_char);
pub type ScsEventCallback = unsafe extern "system" fn(event: u32, event_info: *const c_void, context: ScsContext);
pub type ScsChannelCallback =
    unsafe extern "system" fn(name: *const c_char, index: u32, value: *const ScsValue, context: ScsContext);
pub type ScsRegisterForEvent =
    unsafe extern "system" fn(event: u32, callback: ScsEventCallback, context: ScsContext) -> ScsResult;
pub type ScsUnregisterFromEvent = unsafe extern "system" fn(event: u32) -> ScsResult;
pub type ScsRegisterForChannel = unsafe extern "system" fn(
    name: *const c_char,
    index: u32,
    type_: u32,
    flags: u32,
    callback: ScsChannelCallback,
    context: ScsContext,
) -> ScsResult;
pub type ScsUnregisterFromChannel = unsafe extern "system" fn(name: *const c_char, index: u32, type_: u32) -> ScsResult;

/// `scs_sdk_init_params_v100_t`.
#[repr(C)]
pub struct ScsSdkInitParams {
    pub game_name: *const c_char,
    pub game_id: *const c_char,
    /// SCS_MAKE_VERSION(major, minor) = major << 16 | minor; the plugin reads
    /// the two halves as u16 (+0x12 major, +0x10 minor on x64).
    pub game_version: u32,
    pub log: ScsLog,
}

/// `scs_telemetry_init_params_v100_t`.
#[repr(C)]
pub struct ScsTelemetryInitParams {
    pub common: ScsSdkInitParams,
    pub register_for_event: ScsRegisterForEvent,
    pub unregister_from_event: ScsUnregisterFromEvent,
    pub register_for_channel: ScsRegisterForChannel,
    pub unregister_from_channel: ScsUnregisterFromChannel,
}

/// The offsets the plugin reads, per build.
#[allow(dead_code)]
mod layout_asserts {
    use super::*;
    use core::mem::{offset_of, size_of};

    const _: () = assert!(offset_of!(ScsValue, value) == 8);
    const _: () = assert!(size_of::<ScsValue>() == 0x30);
    const _: () = assert!(offset_of!(ScsDPlacement, orientation) == 0x18);
    const _: () = assert!(offset_of!(ScsTelemetryFrameStart, paused_simulation_time) == 0x18);

    #[cfg(target_pointer_width = "64")]
    mod w64 {
        use super::*;
        // value data of an attribute at +0x18
        const _: () = assert!(offset_of!(ScsNamedValue, index) == 8);
        const _: () = assert!(offset_of!(ScsNamedValue, value) + 8 == 0x18);
        const _: () = assert!(size_of::<ScsNamedValue>() == 0x40);
        const _: () = assert!(offset_of!(ScsTelemetryConfiguration, attributes) == 8);
        const _: () = assert!(offset_of!(ScsSdkInitParams, game_version) == 0x10);
        const _: () = assert!(offset_of!(ScsTelemetryInitParams, register_for_event) == 0x20);
        const _: () = assert!(offset_of!(ScsTelemetryInitParams, register_for_channel) == 0x30);
    }
    #[cfg(target_pointer_width = "32")]
    mod w32 {
        use super::*;
        // value data of an attribute at +0x10, index at +4
        const _: () = assert!(offset_of!(ScsNamedValue, index) == 4);
        const _: () = assert!(offset_of!(ScsNamedValue, value) + 8 == 0x10);
        const _: () = assert!(size_of::<ScsNamedValue>() == 0x38);
        const _: () = assert!(offset_of!(ScsTelemetryConfiguration, attributes) == 4);
        const _: () = assert!(offset_of!(ScsSdkInitParams, game_version) == 8);
        const _: () = assert!(offset_of!(ScsTelemetryInitParams, register_for_event) == 0x10);
        const _: () = assert!(offset_of!(ScsTelemetryInitParams, register_for_channel) == 0x18);
    }
}
