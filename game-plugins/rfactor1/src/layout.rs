//! Sizes and offsets of `$rFactorShared$` and of the plugin object.

/// Mapping name.
pub const MAP_NAME: &[u8] = b"$rFactorShared$\0";
/// CreateFileMappingA / MapViewOfFile / memset size.
pub const MAP_SIZE: usize = 0x9afc;
/// FILE_MAP_ALL_ACCESS as passed to OpenFileMappingA and MapViewOfFile.
pub const MAP_ACCESS: u32 = 0xf001f;
pub const PAGE_READWRITE: u32 = 4;
pub const ERROR_ALREADY_EXISTS: u32 = 0xb7;

/// Telemetry part (written by UpdateTelemetry).
pub const T_DELTA_SINCE_SCORING: usize = 0x000; // f32 (clock() - scoring clock) / 1000
pub const T_WHEELS: usize = 0x148; // 4 x 0x43
pub const T_WHEEL_STRIDE: usize = 0x43;
/// Scoring part (written by UpdateScoring).
pub const S_ET: usize = 0x258; // f32, + telemetry delta on every UpdateTelemetry
pub const S_NUM_VEHICLES: usize = 0x268; // i32
pub const S_IN_REALTIME: usize = 0x273; // u8 (object +0x1d)
pub const S_VEHICLES: usize = 0x2fc; // 128 x 0x130
pub const S_VEHICLE_STRIDE: usize = 0x130;
/// UpdateScoring writes 0x130 zero bytes here, i.e. past the 0x9afc bytes
/// of the view (inside its last page).
pub const S_PAST_END: usize = 0x9afc;
pub const S_PAST_END_LEN: usize = 0x130;

/// Game structures (rFactor 1 InternalsPluginV3, 32-bit).
pub const SCORING_VEHICLES_PTR: usize = 0x1e8; // VehicleScoringInfoV2 *
pub const SCORING_VEHICLE_STRIDE: usize = 0x1ac;
pub const SCORING_NUM_VEHICLES: usize = 0x58;
pub const TELEM_WHEELS: usize = 0x17c;
pub const TELEM_WHEEL_STRIDE: usize = 0x64;

/// Object sizes (operator new / delete).
pub const PLUGIN_SIZE: usize = 0x20;
pub const INFO_SIZE: usize = 0x84;
