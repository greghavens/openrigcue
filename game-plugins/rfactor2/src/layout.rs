//! Byte layouts: the five shared-memory buffers the plugin publishes, the
//! ISI InternalsPluginV07 structures it copies from (the offsets it
//! reads or the copy sizes it uses), and the plugin's rF2Extended state.
//!
//! The numbers here are the mapping sizes (v3.0.1.0 format), the sizes
//! cleared in
//! the clear/shutdown paths, the copy lengths and the field offsets of the
//! update callbacks.

/// Every mapping starts with `{ u32 mVersionUpdateBegin; u32 mVersionUpdateEnd; }`;
/// the payload ("mpBuff") starts 8 bytes in.
pub const VERSION_BLOCK_SIZE: usize = 8;

/// Highest number of vehicles/participants copied into any buffer.
pub const MAX_MAPPED_VEHICLES: usize = 128;
/// Size of the per-mID tables (`id = max(id, 0) & 0x1ff`).
pub const MAX_MAPPED_IDS: usize = 512;

/// `max(id, 0) & 0x1ff`.
#[inline]
pub fn mapped_id(id: i32) -> usize {
    (if id > 0 { id } else { 0 } as u32 & 0x1ff) as usize
}

// ---------------------------------------------------------------------------
// Mapping names.
pub const TELEMETRY_NAME: &[u8] = b"$rFactor2SMMP_Telemetry$\0";
pub const SCORING_NAME: &[u8] = b"$rFactor2SMMP_Scoring$\0";
pub const RULES_NAME: &[u8] = b"$rFactor2SMMP_Rules$\0";
pub const MULTI_RULES_NAME: &[u8] = b"$rFactor2SMMP_MultiRules$\0";
pub const EXTENDED_NAME: &[u8] = b"$rFactor2SMMP_Extended$\0";

// ---------------------------------------------------------------------------
// rF2Telemetry: 0x3b010 bytes mapped.
pub const TELEMETRY_MAP_SIZE: usize = 0x3b010;
pub const TELEMETRY_PAYLOAD_SIZE: usize = 0x3b008; // memset in ClearState/Shutdown
/// payload +0 mBytesUpdatedHint (i32), +4 mNumVehicles (i32), +8 vehicles.
pub const TEL_BYTES_HINT: usize = 0x0;
pub const TEL_NUM_VEHICLES: usize = 0x4;
pub const TEL_VEHICLES: usize = 0x8;

// rF2Scoring: 0x12630 mapped.
pub const SCORING_MAP_SIZE: usize = 0x12630;
pub const SCORING_PAYLOAD_SIZE: usize = 0x12628;
/// payload +0 mBytesUpdatedHint, +4 rF2ScoringInfo (0x224), +0x228 vehicles (0x248 each).
pub const SC_BYTES_HINT: usize = 0x0;
pub const SC_INFO: usize = 0x4;
pub const SC_VEHICLES: usize = 0x228;

// rF2Rules: 0xb0d8 mapped.
pub const RULES_MAP_SIZE: usize = 0xb0d8;
pub const RULES_PAYLOAD_SIZE: usize = 0xb0d0;
/// payload +0 hint, +4 TrackRulesV01 (0x2cc), +0x2d0 actions (0x10 each), +0xad0 participants (0x14c each).
pub const RU_BYTES_HINT: usize = 0x0;
pub const RU_RULES: usize = 0x4;
pub const RU_ACTIONS: usize = 0x2d0;
pub const RU_PARTICIPANTS: usize = 0xad0;

// rF2MultiRules: 0x9b70 mapped.
pub const MULTI_RULES_MAP_SIZE: usize = 0x9b70;
pub const MULTI_RULES_PAYLOAD_SIZE: usize = 0x9b68;
/// payload +0 hint, +4 MultiSessionRulesV01 (0x164), +0x168 participants (0x134 each).
pub const MR_BYTES_HINT: usize = 0x0;
pub const MR_RULES: usize = 0x4;
pub const MR_PARTICIPANTS: usize = 0x168;

// rF2Extended: 0x24e4 mapped.
pub const EXTENDED_MAP_SIZE: usize = 0x24e4;
pub const EXTENDED_PAYLOAD_SIZE: usize = 0x24dc;

// ---------------------------------------------------------------------------
// ISI InternalsPluginV07 structures (as the game passes them).

/// TelemInfoV01: copied whole, 0x760 bytes.
pub mod telem {
    pub const SIZE: usize = 0x760;
    pub const ID: usize = 0x0; // long mID
    pub const DELTA_TIME: usize = 0x4;
    pub const ELAPSED_TIME: usize = 0xc;
    pub const LAP_NUMBER: usize = 0x14;
    pub const LAP_START_ET: usize = 0x18;
    pub const VEHICLE_NAME: usize = 0x20;
    pub const TRACK_NAME: usize = 0x60;
    pub const POS: usize = 0xa0; // TelemVect3 (3 x f64)
    pub const LOCAL_VEL: usize = 0xb8;
    pub const LOCAL_ACCEL: usize = 0xd0;
    pub const ORI: usize = 0xe8; // 3 x TelemVect3
    pub const LOCAL_ROT: usize = 0x130;
    pub const LOCAL_ROT_ACCEL: usize = 0x148;
    pub const GEAR: usize = 0x160;
    pub const ENGINE_RPM: usize = 0x164;
    pub const ENGINE_WATER_TEMP: usize = 0x16c;
    pub const ENGINE_OIL_TEMP: usize = 0x174;
    pub const CLUTCH_RPM: usize = 0x17c;
    pub const UNFILTERED_THROTTLE: usize = 0x184;
    pub const UNFILTERED_BRAKE: usize = 0x18c;
    pub const UNFILTERED_STEERING: usize = 0x194;
    pub const UNFILTERED_CLUTCH: usize = 0x19c;
    pub const FILTERED_THROTTLE: usize = 0x1a4;
    pub const FILTERED_BRAKE: usize = 0x1ac;
    pub const FILTERED_STEERING: usize = 0x1b4;
    pub const FILTERED_CLUTCH: usize = 0x1bc;
    pub const STEERING_SHAFT_TORQUE: usize = 0x1c4;
    pub const FRONT_3RD_DEFLECTION: usize = 0x1cc;
    pub const REAR_3RD_DEFLECTION: usize = 0x1d4;
    pub const FRONT_WING_HEIGHT: usize = 0x1dc;
    pub const FRONT_RIDE_HEIGHT: usize = 0x1e4;
    pub const REAR_RIDE_HEIGHT: usize = 0x1ec;
    pub const DRAG: usize = 0x1f4;
    pub const FRONT_DOWNFORCE: usize = 0x1fc;
    pub const REAR_DOWNFORCE: usize = 0x204;
    pub const FUEL: usize = 0x20c;
    pub const ENGINE_MAX_RPM: usize = 0x214;
    pub const SCHEDULED_STOPS: usize = 0x21c;
    pub const OVERHEATING: usize = 0x21d;
    pub const DETACHED: usize = 0x21e;
    pub const DENT_SEVERITY: usize = 0x220; // u8[8]
    pub const LAST_IMPACT_ET: usize = 0x228;
    pub const LAST_IMPACT_MAGNITUDE: usize = 0x230;
    pub const LAST_IMPACT_POS: usize = 0x238;
    pub const WHEELS: usize = 0x350; // TelemWheelV01[4]
    pub const WHEEL_SIZE: usize = 0x104;
    pub mod wheel {
        pub const SUSPENSION_DEFLECTION: usize = 0x0;
        pub const RIDE_HEIGHT: usize = 0x8;
        pub const SUSP_FORCE: usize = 0x10;
        pub const BRAKE_TEMP: usize = 0x18;
        pub const BRAKE_PRESSURE: usize = 0x20;
        pub const ROTATION: usize = 0x28;
        pub const LATERAL_PATCH_VEL: usize = 0x30;
        pub const LONGITUDINAL_PATCH_VEL: usize = 0x38;
        pub const LATERAL_GROUND_VEL: usize = 0x40;
        pub const LONGITUDINAL_GROUND_VEL: usize = 0x48;
        pub const CAMBER: usize = 0x50;
        pub const LATERAL_FORCE: usize = 0x58;
        pub const LONGITUDINAL_FORCE: usize = 0x60;
        pub const TIRE_LOAD: usize = 0x68;
        pub const GRIP_FRACT: usize = 0x70;
        pub const PRESSURE: usize = 0x78;
        pub const TEMPERATURE: usize = 0x80; // f64[3]
        pub const WEAR: usize = 0x98;
        pub const TERRAIN_NAME: usize = 0xa0; // char[16]
        pub const SURFACE_TYPE: usize = 0xb0;
        pub const FLAT: usize = 0xb1;
        pub const DETACHED: usize = 0xb2;
    }
}

/// ScoringInfoV01: copied 0x224 bytes.
pub mod scoring_info {
    pub const SIZE: usize = 0x224;
    pub const TRACK_NAME: usize = 0x0;
    pub const SESSION: usize = 0x40;
    pub const CURRENT_ET: usize = 0x44;
    pub const END_ET: usize = 0x4c;
    pub const MAX_LAPS: usize = 0x54;
    pub const LAP_DIST: usize = 0x58;
    pub const RESULTS_STREAM: usize = 0x60; // char *
    pub const NUM_VEHICLES: usize = 0x68;
    pub const GAME_PHASE: usize = 0x6c;
    pub const YELLOW_FLAG_STATE: usize = 0x6d;
    pub const SECTOR_FLAG: usize = 0x6e; // i8[3]
    pub const START_LIGHT: usize = 0x71;
    pub const NUM_RED_LIGHTS: usize = 0x72;
    pub const IN_REALTIME: usize = 0x73;
    pub const PLAYER_NAME: usize = 0x74;
    pub const PLR_FILE_NAME: usize = 0x94;
    pub const DARK_CLOUD: usize = 0xd4;
    pub const RAINING: usize = 0xdc;
    pub const AMBIENT_TEMP: usize = 0xe4;
    pub const TRACK_TEMP: usize = 0xec;
    pub const WIND: usize = 0xf4;
    pub const MIN_PATH_WETNESS: usize = 0x10c;
    pub const MAX_PATH_WETNESS: usize = 0x114;
    pub const VEHICLE: usize = 0x21c; // VehicleScoringInfoV01 *
}

/// VehicleScoringInfoV01: copied 0x248 bytes each.
pub mod vehicle_scoring {
    pub const SIZE: usize = 0x248;
    pub const ID: usize = 0x0;
    pub const DRIVER_NAME: usize = 0x4;
    pub const VEHICLE_NAME: usize = 0x24;
    pub const TOTAL_LAPS: usize = 0x64; // short
    pub const SECTOR: usize = 0x66; // signed char
    pub const FINISH_STATUS: usize = 0x67; // signed char
    pub const LAP_DIST: usize = 0x68;
    pub const PATH_LATERAL: usize = 0x70;
    pub const TRACK_EDGE: usize = 0x78;
    pub const BEST_SECTOR1: usize = 0x80;
    pub const BEST_SECTOR2: usize = 0x88;
    pub const BEST_LAP_TIME: usize = 0x90;
    pub const LAST_SECTOR1: usize = 0x98;
    pub const LAST_SECTOR2: usize = 0xa0;
    pub const LAST_LAP_TIME: usize = 0xa8;
    pub const CUR_SECTOR1: usize = 0xb0;
    pub const CUR_SECTOR2: usize = 0xb8;
    pub const NUM_PITSTOPS: usize = 0xc0; // short
    pub const NUM_PENALTIES: usize = 0xc2; // short
    pub const IS_PLAYER: usize = 0xc4;
    pub const CONTROL: usize = 0xc5; // signed char
    pub const IN_PITS: usize = 0xc6;
    pub const PLACE: usize = 0xc7;
    pub const VEHICLE_CLASS: usize = 0xc8;
    pub const TIME_BEHIND_NEXT: usize = 0xe8;
    pub const LAPS_BEHIND_NEXT: usize = 0xf0;
    pub const TIME_BEHIND_LEADER: usize = 0xf4;
    pub const LAPS_BEHIND_LEADER: usize = 0xfc;
    pub const LAP_START_ET: usize = 0x100;
    pub const POS: usize = 0x108;
    pub const LOCAL_VEL: usize = 0x120;
    pub const LOCAL_ACCEL: usize = 0x138;
    pub const ORI: usize = 0x150;
    pub const LOCAL_ROT: usize = 0x198;
    pub const LOCAL_ROT_ACCEL: usize = 0x1b0;
    pub const PIT_STATE: usize = 0x1c9;
    /// mPitState value the damage tracker resets on.
    pub const PIT_STATE_STOPPED: u8 = 3;
}

/// TrackRulesV01: copied 0x2cc bytes.
pub mod track_rules {
    pub const SIZE: usize = 0x2cc;
    pub const NUM_ACTIONS: usize = 0x10;
    pub const ACTION: usize = 0x14; // TrackRulesActionV01 *
    pub const NUM_PARTICIPANTS: usize = 0x1c;
    pub const YELLOW_FLAG_STATE: usize = 0x140; // signed char
    pub const MESSAGE: usize = 0x164; // char[96]
    pub const PARTICIPANT: usize = 0x1c4; // TrackRulesParticipantV01 *
    pub const ACTION_SIZE: usize = 0x10;
    pub const PARTICIPANT_SIZE: usize = 0x14c;
    pub const PARTICIPANT_ID: usize = 0x0;
    pub const PARTICIPANT_MESSAGE: usize = 0x2c; // char[96]
    pub const MESSAGE_LEN: usize = 0x60;
}

/// MultiSessionRulesV01: copied 0x164 bytes.
pub mod multi_rules {
    pub const SIZE: usize = 0x164;
    pub const NUM_PARTICIPANTS: usize = 0x28;
    pub const PARTICIPANT: usize = 0x2c; // MultiSessionParticipantV01 *
    pub const PARTICIPANT_SIZE: usize = 0x134;
}

/// PhysicsOptionsV01: 0x28 bytes.
pub const PHYSICS_OPTIONS_SIZE: usize = 0x28;
/// MessageInfoV01::mText: char[128] at +0.
pub const MESSAGE_TEXT_LEN: usize = 0x80;

/// CustomVariableV01 { char mCaption[128]; long mNumSettings; long mCurrentSetting; }.
pub mod custom_var {
    pub const SIZE: usize = 0x88;
    pub const CAPTION: usize = 0x0;
    pub const CAPTION_LEN: usize = 0x80;
    pub const NUM_SETTINGS: usize = 0x80;
    pub const CURRENT_SETTING: usize = 0x84;
}
/// CustomSettingV01 { char mName[128]; }.
pub const CUSTOM_SETTING_NAME_LEN: usize = 0x80;

// ---------------------------------------------------------------------------
// rF2Extended (payload of $rFactor2SMMP_Extended$, 0x24dc bytes). The plugin
// keeps it at object +0x38, followed by its private per-mID damage tracking
// table (0x24dc bytes, then 0x2000 bytes at +0x24e0).
pub mod ext {
    pub const VERSION: usize = 0x0; // char[8] = "3.0.1.0"
    pub const VERSION_LEN: usize = 8;
    pub const IS_64BIT: usize = 0x8; // bool = 1
    pub const PHYSICS: usize = 0xc; // PhysicsOptionsV01
    pub const TRACKED_DAMAGES: usize = 0x34; // {f64 mMaxImpactMagnitude; f64 mAccumulatedImpactMagnitude;}[512]
    pub const TRACKED_DAMAGE_SIZE: usize = 0x10;
    pub const IN_REALTIME_FC: usize = 0x2034;
    pub const MULTIMEDIA_THREAD_STARTED: usize = 0x2035;
    pub const SIMULATION_THREAD_STARTED: usize = 0x2036;
    pub const SESSION_STARTED: usize = 0x2037;
    pub const TICKS_SESSION_STARTED: usize = 0x2038; // u64 GetTickCount64
    pub const TICKS_SESSION_ENDED: usize = 0x2040;
    /// rF2SessionTransitionCapture.
    pub const ST_GAME_PHASE: usize = 0x2048;
    pub const ST_SESSION: usize = 0x204c;
    pub const ST_NUM_SCORING_VEHICLES: usize = 0x2050;
    pub const ST_VEHICLES: usize = 0x2054; // {i32 mID; u8 mPlace; bool mIsPlayer; i8 mFinishStatus; pad}[128]
    pub const ST_VEHICLE_SIZE: usize = 8;
    pub const DISPLAYED_MESSAGE: usize = 0x2454; // char[128]
    pub const SCR_IS_HOSTED: usize = 0x24d4; // bool
    pub const SCR_DOUBLE_FILE_TYPE: usize = 0x24d8; // long, -1 at construction
    /// Private part (not mapped): per-mID {f64 lastImpactProcessedET; f64 lastPitStopET;}[512].
    pub const DAMAGE_TRACKING: usize = 0x24e0;
    pub const DAMAGE_TRACKING_SIZE: usize = 0x10;
    pub const STATE_SIZE: usize = 0x44e0;
}
const _: () = assert!(ext::TRACKED_DAMAGES + MAX_MAPPED_IDS * ext::TRACKED_DAMAGE_SIZE == ext::IN_REALTIME_FC);
const _: () = assert!(ext::ST_VEHICLES + MAX_MAPPED_VEHICLES * ext::ST_VEHICLE_SIZE == ext::DISPLAYED_MESSAGE);
const _: () = assert!(ext::DISPLAYED_MESSAGE + 0x80 == ext::SCR_IS_HOSTED);
const _: () = assert!(ext::SCR_DOUBLE_FILE_TYPE + 4 == EXTENDED_PAYLOAD_SIZE);
const _: () = assert!(ext::DAMAGE_TRACKING + MAX_MAPPED_IDS * ext::DAMAGE_TRACKING_SIZE == ext::STATE_SIZE);
const _: () = assert!(TEL_VEHICLES + MAX_MAPPED_VEHICLES * telem::SIZE == TELEMETRY_PAYLOAD_SIZE);
const _: () = assert!(SC_INFO + scoring_info::SIZE == SC_VEHICLES);
const _: () = assert!(SC_VEHICLES + MAX_MAPPED_VEHICLES * vehicle_scoring::SIZE == SCORING_PAYLOAD_SIZE);
const _: () = assert!(RU_RULES + track_rules::SIZE == RU_ACTIONS);
const _: () = assert!(RU_ACTIONS + MAX_MAPPED_VEHICLES * track_rules::ACTION_SIZE == RU_PARTICIPANTS);
const _: () = assert!(RU_PARTICIPANTS + MAX_MAPPED_VEHICLES * track_rules::PARTICIPANT_SIZE == RULES_PAYLOAD_SIZE);
const _: () = assert!(MR_RULES + multi_rules::SIZE == MR_PARTICIPANTS);
const _: () = assert!(MR_PARTICIPANTS + MAX_MAPPED_VEHICLES * multi_rules::PARTICIPANT_SIZE == MULTI_RULES_PAYLOAD_SIZE);
const _: () = assert!(TELEMETRY_PAYLOAD_SIZE + VERSION_BLOCK_SIZE == TELEMETRY_MAP_SIZE);
const _: () = assert!(SCORING_PAYLOAD_SIZE + VERSION_BLOCK_SIZE == SCORING_MAP_SIZE);
const _: () = assert!(RULES_PAYLOAD_SIZE + VERSION_BLOCK_SIZE == RULES_MAP_SIZE);
const _: () = assert!(MULTI_RULES_PAYLOAD_SIZE + VERSION_BLOCK_SIZE == MULTI_RULES_MAP_SIZE);
const _: () = assert!(EXTENDED_PAYLOAD_SIZE + VERSION_BLOCK_SIZE == EXTENDED_MAP_SIZE);

/// Version string written into rF2Extended::mVersion.
pub const PLUGIN_VERSION: &[u8] = b"3.0.1.0\0";

// ---------------------------------------------------------------------------
// Raw little-endian access to game-owned / mapped memory.

#[inline]
pub unsafe fn rd_i32(p: *const u8, off: usize) -> i32 {
    core::ptr::read_unaligned(p.add(off) as *const i32)
}
#[inline]
pub unsafe fn rd_u32(p: *const u8, off: usize) -> u32 {
    core::ptr::read_unaligned(p.add(off) as *const u32)
}
#[inline]
pub unsafe fn rd_i16(p: *const u8, off: usize) -> i16 {
    core::ptr::read_unaligned(p.add(off) as *const i16)
}
#[inline]
pub unsafe fn rd_u8(p: *const u8, off: usize) -> u8 {
    *p.add(off)
}
#[inline]
pub unsafe fn rd_i8(p: *const u8, off: usize) -> i8 {
    *p.add(off) as i8
}
#[inline]
pub unsafe fn rd_f64(p: *const u8, off: usize) -> f64 {
    core::ptr::read_unaligned(p.add(off) as *const f64)
}
#[inline]
pub unsafe fn rd_u64(p: *const u8, off: usize) -> u64 {
    core::ptr::read_unaligned(p.add(off) as *const u64)
}
#[inline]
pub unsafe fn rd_ptr(p: *const u8, off: usize) -> *mut u8 {
    core::ptr::read_unaligned(p.add(off) as *const *mut u8)
}
#[inline]
pub unsafe fn wr_i32(p: *mut u8, off: usize, v: i32) {
    core::ptr::write_unaligned(p.add(off) as *mut i32, v)
}
#[inline]
pub unsafe fn wr_u8(p: *mut u8, off: usize, v: u8) {
    *p.add(off) = v
}
#[inline]
pub unsafe fn wr_f64(p: *mut u8, off: usize, v: f64) {
    core::ptr::write_unaligned(p.add(off) as *mut f64, v)
}
#[inline]
pub unsafe fn wr_u64(p: *mut u8, off: usize, v: u64) {
    core::ptr::write_unaligned(p.add(off) as *mut u64, v)
}

/// Bytes of a NUL-terminated C string (without the NUL).
pub unsafe fn cstr<'a>(p: *const u8) -> &'a [u8] {
    let mut n = 0;
    while *p.add(n) != 0 {
        n += 1;
    }
    core::slice::from_raw_parts(p, n)
}
