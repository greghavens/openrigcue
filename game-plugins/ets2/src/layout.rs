//! The shared-memory block the plugin publishes, and the two tables that drive
//! it: the channel registrations of `scs_telemetry_init` and the
//! configuration-attribute dispatch table.
//!
//! Offsets are byte offsets into the mapping view; both builds use the same offsets.

/// The mapping name.
pub const MAP_NAME: &str = "Local\\SimTelemetryETS2";
/// The mapping and view size.
pub const MAP_SIZE: usize = 0x4000;

/// `PAGE_READWRITE`, the CreateFileMappingW protection.
pub const PAGE_READWRITE: u32 = 4;
/// `FILE_MAP_ALL_ACCESS`, the OpenFileMappingW / MapViewOfFile access.
pub const FILE_MAP_ALL_ACCESS: u32 = 0x000f_001f;
/// `ERROR_ALREADY_EXISTS`: the only CreateFileMappingW error that falls back
/// to OpenFileMappingW.
pub const ERROR_ALREADY_EXISTS: u32 = 0xb7;

/// Value written to the revision field at init.
pub const PLUGIN_REVISION: u32 = 5;
/// Where the truck model id string goes.
pub const MODEL_STRING_OFFSET: u32 = 0x3c00;
/// Where the trailer (cargo accessory) id goes.
pub const TRAILER_STRING_OFFSET: u32 = 0x3c40;

/// Field offsets in the block.
pub mod off {
    pub const TIME: usize = 0x000; // u32, accumulated paused_simulation_time (us)
    pub const PAUSED: usize = 0x004; // u32, 1 after init / on paused, 0 on started
    pub const REVISION: usize = 0x008; // u32 = 5
    pub const GAME_VERSION_MAJOR: usize = 0x00c; // u32
    pub const GAME_VERSION_MINOR: usize = 0x010; // u32
    pub const ELECTRIC_ENABLED_1: usize = 0x014; // bool (first truck.electric.enabled)
    pub const TRAILER_CONNECTED: usize = 0x015; // bool
    pub const SPEED: usize = 0x018; // f32
    pub const ACCELERATION: usize = 0x01c; // f32 x3
    pub const PLACEMENT: usize = 0x028; // f32 x3 position, f32 heading/pitch/roll
    pub const GEAR: usize = 0x040; // s32
    pub const GEARS_FORWARD: usize = 0x044; // u32
    pub const ENGINE_RPM: usize = 0x050; // f32
    pub const RPM_LIMIT: usize = 0x054; // f32
    pub const FUEL_AMOUNT: usize = 0x058; // f32
    pub const FUEL_CAPACITY: usize = 0x05c; // f32
    pub const FUEL_CONSUMPTION_AVERAGE: usize = 0x064; // f32
    pub const MODEL_OFFSET: usize = 0x090; // s32 = 0x3c00
    pub const MODEL_LENGTH: usize = 0x094; // s32
    pub const TRAILER_OFFSET: usize = 0x098; // s32 = 0x3c40
    pub const TRAILER_LENGTH: usize = 0x09c; // s32
    pub const GAME_TIME: usize = 0x0a0; // u32
    pub const GEARS_REVERSE: usize = 0x0a4; // u32
    pub const CARGO_MASS: usize = 0x0a8; // f32
    pub const CARGO_ID: usize = 0x0ac; // char[64]
    pub const CARGO_NAME: usize = 0x0ec; // char[64]
    pub const JOB_INCOME: usize = 0x12c; // u32
    pub const JOB_DELIVERY_TIME: usize = 0x130; // u32
    pub const SOURCE_CITY: usize = 0x134; // char[64]
    pub const DESTINATION_CITY: usize = 0x174; // char[64]
    pub const SOURCE_COMPANY: usize = 0x1b4; // char[64]
    pub const DESTINATION_COMPANY: usize = 0x1f4; // char[64]
    /// End (exclusive) of the job block cleared 10+ frames after a delivery.
    pub const JOB_BLOCK_END: usize = 0x234;
    pub const CRUISE_CONTROL_ACTIVE: usize = 0x244; // bool, cruise_control > 0
    pub const CRUISE_CONTROL: usize = 0x2a0; // f32
    pub const BRAND: usize = 0x2a4; // char[64]
    pub const BRAND_ID: usize = 0x2e4; // char[64]
    pub const TRUCK_NAME: usize = 0x324; // char[64]
    pub const NAVIGATION_DISTANCE: usize = 0x368; // f32
    pub const FORWARD_RATIOS: usize = 0x374; // f32[24]
    pub const REVERSE_RATIOS: usize = 0x3d4; // f32[8] (index checked < 24)
    pub const DIFFERENTIAL_RATIO: usize = 0x3f4; // f32
    pub const DISPLAYED_GEAR: usize = 0x3f8; // s32
    pub const ON_JOB: usize = 0x3fc; // bool
    pub const JOB_FINISHED: usize = 0x3fd; // bool
}

/// Value written for each channel-callback kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelKind {
    /// Copies the 4 data bytes (float, s32, u32).
    Word,
    /// bool -> 0/1, NULL value -> 0.
    Bool,
    /// fvector, 3 x 4 bytes.
    FVector,
    /// dplacement, doubles narrowed to f32 + euler.
    DPlacement,
}

pub struct Channel {
    pub name: &'static core::ffi::CStr,
    pub value_type: u32,
    pub kind: ChannelKind,
    pub offset: usize,
}

const fn ch(name: &'static core::ffi::CStr, value_type: u32, kind: ChannelKind, offset: usize) -> Channel {
    Channel { name, value_type, kind, offset }
}

use crate::scs::{
    SCS_VALUE_TYPE_BOOL as B, SCS_VALUE_TYPE_DPLACEMENT as DP, SCS_VALUE_TYPE_FLOAT as F, SCS_VALUE_TYPE_FVECTOR as FV,
    SCS_VALUE_TYPE_S32 as S, SCS_VALUE_TYPE_U32 as U,
};
use ChannelKind::{Bool, DPlacement, FVector, Word};

/// Registrations in call order. All use index
/// SCS_U32_NIL and flags SCS_TELEMETRY_CHANNEL_FLAG_no_value; the context is
/// `view + offset`. `truck.electric.enabled` is registered twice (0x14 and
/// 0x248).
pub static CHANNELS: [Channel; 67] = [
    ch(c"truck.electric.enabled", B, Bool, 0x014),
    ch(c"game.time", U, Word, 0x0a0),
    ch(c"trailer.connected", B, Bool, 0x015),
    ch(c"truck.speed", F, Word, 0x018),
    ch(c"truck.local.acceleration.linear", FV, FVector, 0x01c),
    ch(c"truck.world.placement", DP, DPlacement, 0x028),
    ch(c"truck.engine.gear", S, Word, 0x040),
    ch(c"truck.displayed.gear", S, Word, 0x3f8),
    ch(c"truck.engine.rpm", F, Word, 0x050),
    ch(c"truck.fuel.amount", F, Word, 0x058),
    ch(c"truck.fuel.consumption.average", F, Word, 0x064),
    ch(c"truck.input.steering", F, Word, 0x068),
    ch(c"truck.input.throttle", F, Word, 0x06c),
    ch(c"truck.input.brake", F, Word, 0x070),
    ch(c"truck.input.clutch", F, Word, 0x074),
    ch(c"truck.effective.steering", F, Word, 0x078),
    ch(c"truck.effective.throttle", F, Word, 0x07c),
    ch(c"truck.effective.brake", F, Word, 0x080),
    ch(c"truck.effective.clutch", F, Word, 0x084),
    ch(c"truck.brake.retarder", U, Word, 0x234),
    ch(c"truck.hshifter.slot", U, Word, 0x238),
    ch(c"truck.hshifter.select", B, Bool, 0x23c),
    ch(c"truck.wipers", B, Bool, 0x245),
    ch(c"truck.brake.parking", B, Bool, 0x246),
    ch(c"truck.brake.motor", B, Bool, 0x247),
    ch(c"truck.electric.enabled", B, Bool, 0x248),
    ch(c"truck.engine.enabled", B, Bool, 0x249),
    ch(c"truck.lblinker", B, Bool, 0x24a),
    ch(c"truck.rblinker", B, Bool, 0x24b),
    ch(c"truck.light.lblinker", B, Bool, 0x24c),
    ch(c"truck.light.rblinker", B, Bool, 0x24d),
    ch(c"truck.light.parking", B, Bool, 0x24e),
    ch(c"truck.light.beam.low", B, Bool, 0x24f),
    ch(c"truck.light.beam.high", B, Bool, 0x250),
    ch(c"truck.light.aux.front", B, Bool, 0x251),
    ch(c"truck.light.aux.roof", B, Bool, 0x252),
    ch(c"truck.light.beacon", B, Bool, 0x253),
    ch(c"truck.light.brake", B, Bool, 0x254),
    ch(c"truck.light.reverse", B, Bool, 0x255),
    ch(c"truck.battery.voltage.warning", B, Bool, 0x256),
    ch(c"truck.brake.air.pressure.warning", B, Bool, 0x257),
    ch(c"truck.brake.air.pressure.emergency", B, Bool, 0x258),
    ch(c"truck.adblue.warning", B, Bool, 0x259),
    ch(c"truck.oil.pressure.warning", B, Bool, 0x25a),
    ch(c"truck.water.temperature.warning", B, Bool, 0x25b),
    ch(c"truck.brake.air.pressure", F, Word, 0x25c),
    ch(c"truck.brake.temperature", F, Word, 0x260),
    ch(c"truck.fuel.warning", B, Bool, 0x264),
    ch(c"truck.adblue", F, Word, 0x268),
    ch(c"truck.adblue.consumption.average", F, Word, 0x26c),
    ch(c"truck.oil.pressure", F, Word, 0x270),
    ch(c"truck.oil.temperature", F, Word, 0x274),
    ch(c"truck.water.temperature", F, Word, 0x278),
    ch(c"truck.battery.voltage", F, Word, 0x27c),
    ch(c"truck.dashboard.backlight", F, Word, 0x280),
    ch(c"truck.wear.engine", F, Word, 0x284),
    ch(c"truck.wear.transmission", F, Word, 0x288),
    ch(c"truck.wear.cabin", F, Word, 0x28c),
    ch(c"truck.wear.chassis", F, Word, 0x290),
    ch(c"truck.wear.wheels", F, Word, 0x294),
    ch(c"trailer.wear.chassis", F, Word, 0x298),
    ch(c"truck.odometer", F, Word, 0x29c),
    ch(c"truck.cruise_control", F, Word, 0x2a0),
    ch(c"truck.navigation.speed.limit", F, Word, 0x364),
    ch(c"truck.navigation.distance", F, Word, 0x368),
    ch(c"truck.navigation.time", F, Word, 0x36c),
    ch(c"truck.fuel.range", F, Word, 0x370),
];

/// What a configuration attribute handler does with the attribute value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttrAction {
    /// `strncpy(view + off, value_string, 64)`, skipped if the view pointer is NULL.
    Str64(usize),
    /// 4 value bytes to `off`, no NULL check on the view pointer.
    WordUnchecked(usize),
    /// 4 value bytes to `off`, skipped if the view pointer is NULL.
    Word(usize),
    /// If the string starts with 'v', it is copied to
    /// `view + [0x90]` and its length stored at 0x94.
    ModelId,
    /// The string is copied to `view + [0x98]` and its length stored at 0x9c (no 'v' test).
    TrailerId,
    /// A signed index below 24 -> 4 value bytes to `base + 4*index`.
    Ratio(usize),
}

pub struct Attr {
    pub name: &'static core::ffi::CStr,
    pub action: AttrAction,
}

const fn at(name: &'static core::ffi::CStr, action: AttrAction) -> Attr {
    Attr { name, action }
}

use AttrAction::{ModelId, Ratio, Str64, TrailerId, Word as AWord, WordUnchecked};

/// Dispatch table in table order (21 entries, first exact match wins). The
/// configuration id ("truck", "trailer", "job", ...) is never looked at.
pub static ATTRIBUTES: [Attr; 21] = [
    at(c"brand", Str64(0x2a4)),
    at(c"brand_id", Str64(0x2e4)),
    at(c"name", Str64(0x324)),
    at(c"id", ModelId),
    at(c"cargo.accessory.id", TrailerId),
    at(c"fuel.capacity", WordUnchecked(0x05c)),
    at(c"rpm.limit", WordUnchecked(0x054)),
    at(c"gears.forward", WordUnchecked(0x044)),
    at(c"gears.reverse", WordUnchecked(0x0a4)),
    at(c"income", AWord(0x12c)),
    at(c"delivery.time", AWord(0x130)),
    at(c"cargo.mass", AWord(0x0a8)),
    at(c"cargo.id", Str64(0x0ac)),
    at(c"cargo", Str64(0x0ec)),
    at(c"source.city", Str64(0x134)),
    at(c"destination.city", Str64(0x174)),
    at(c"source.company", Str64(0x1b4)),
    at(c"destination.company", Str64(0x1f4)),
    at(c"forward.ratio", Ratio(0x374)),
    at(c"reverse.ratio", Ratio(0x3d4)),
    at(c"differential.ratio", AWord(0x3f4)),
];

/// Navigation distance at or below which an active job counts as delivered
/// once the trailer is disconnected (float bits 0x3dcccccd).
pub const DELIVERY_DISTANCE: f32 = 0.1;
/// Frames after delivery before the job block is cleared.
pub const JOB_CLEAR_FRAMES: i32 = 10;
