//! The per-game / per-vehicle tuning profile.
//!
//! Field names are the SRS profile keys: they are the interchange format of `.srs` profile files and
//! of `profile.db`, so a tune converts to and from those files without a translation table. Values
//! are UI slider positions (0–100 %, 0–20 for effect levels) unless stated; their meaning belongs to
//! the motion and effect engines. Numbers are `f64` because those files hold ints and floats
//! interchangeably; filter-type indices and on/off integers stay integers.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub use crate::settings::WindType;

/// Whether an axis follows the telemetry angle or rate (`motion_type`: `angle` / `rate`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AxisSource {
    Angle,
    Rate,
}

/// `vehicle` of an exported profile: `0` for a game-level export, else the vehicle name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ExportedVehicle {
    Game(i64),
    Vehicle(String),
}

/// One tuning profile. A game entry holds its vehicle profiles in `vehicles`; each vehicle profile is
/// a complete copy with an empty `vehicles`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tune {
    /// Shift light: % of max RPM where the LEDs flash.
    pub gear_shift_perc: f64,
    pub default_max_speed_kmh: f64,
    /// Vertical actuator travel limit.
    pub motion_max_power: f64,
    /// Overall motion smoothing slider.
    pub motion_smoothing_factor: f64,
    pub min_traction_loss_activation: f64,
    /// 0/1 switch.
    pub motion_enhanced_gforces: i64,
    pub power_wind_max_speed_perc: f64,
    pub power_wind_min_speed_perc: f64,
    pub gear_shift_dim_perc: f64,
    pub wind_curving: i64,
    pub curving_effect: f64,
    pub speed_factor: f64,
    /// 0/1 switch.
    pub shift_sound: i64,
    pub wind_type: WindType,
    pub shaker_volume: ShakerVolumes,
    pub shaker_effects: ShakerLevels,
    pub motion_effects: MotionLevels,
    pub max_telemetry_vars: MotionRanges,
    /// Vehicle profiles of a game entry (always empty inside a vehicle profile).
    pub vehicles: BTreeMap<String, Tune>,
    pub motion_boost_factor: f64,
    pub gear_smoothing_factor: f64,
    /// Marks a profile shared by a tuner (expert sets).
    pub publish: bool,
    pub motion_effect_smoothing_factor: MotionSmoothing,
    pub motion_main_actuators_surge: f64,
    pub motion_main_actuators_sway: f64,
    pub motion_main_actuators_traction_loss: f64,
    /// Horizontal actuator travel limit.
    pub motion_max_horizontal: f64,
    pub thanos_spike_filter_horizontal: f64,
    pub thanos_spike_filter_vertical: f64,
    /// Index into the Thanos smoothing filters (0 none … 4 soft).
    pub thanos_smoothing_factor: i64,
    pub motion_switch: MotionSwitches,
    pub shaker_switch: ShakerSwitches,
    pub motion_belt_tensioner: f64,
    pub shaker_dsp_type: ShakerFilterTypes,
    pub shaker_cutoff: ShakerCutoffs,
    pub shaker_resonance: ShakerResonances,
    /// IntelliTune preference; `''` on games, `None` on vehicles created before v215.
    pub intellitune_motion_preference: Option<String>,
    pub belt_sway_curving_enabled: bool,
    pub belt_surge: f64,
    pub belt_sway_curving_factor: f64,
    pub belt_surge_smoothing: f64,
    pub belt_sway_smoothing: f64,
    pub belt_surge_enabled: bool,
    pub belt_sway_curving_reverse: bool,
    pub belt_heave_smoothing: f64,
    pub belt_heave_factor: f64,
    pub belt_heave_reverse: bool,
    pub belt_heave_enabled: bool,
    /// Shaker engine sound: `E` on a first installation, `O` for profiles migrated at v271.
    pub shaker_engine_type: String,
    pub iflag: FlagSwitches,
    pub rumble_effects: RumbleLevels,
    pub rumble_switch: RumbleSwitches,
    pub power_center_wind_min_speed_perc: f64,
    pub power_center_wind_max_speed_perc: f64,
    pub haptic_effects: HapticLevels,
    pub haptic_switch: HapticSwitches,
    pub enable_ejection_seat: bool,
    pub motion_main_actuators_traction_loss_front: f64,
    pub min_traction_loss_front_activation: f64,
    pub scn6_speed: f64,
    pub scn6_acceleration: f64,
    pub belt_surge_reverse: bool,
    pub motion_reaction_speed: f64,
    /// Present in a few factory profiles only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub belt_heave_curving_reverse: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shaker_pitchshift: Option<ShakerPitchShifts>,
    /// Export metadata kept by the `redrover` factory profile (a pasted `.srs` export).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle: Option<ExportedVehicle>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub srs_version: Option<String>,
    /// The profile's routines; entries retain their raw enabled/profile values, including null profiles.
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "present_value")]
    pub routines: Option<serde_json::Value>,
}

fn present_value<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<serde_json::Value>, D::Error> {
    serde_json::Value::deserialize(deserializer).map(Some)
}

/// Per-speaker volume, 0–10 (`shaker_volume`; `channel_mapping` gives the output channel).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShakerVolumes {
    pub rl: f64,
    pub rr: f64,
    pub fl: f64,
    pub fr: f64,
    pub ce: f64,
    pub sub: f64,
}

/// Shaker effect intensities (`shaker_effects`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShakerLevels {
    pub road_bump: f64,
    pub suspension: f64,
    pub engine: f64,
    pub gear: f64,
    pub impact: f64,
    pub deceleration: f64,
    pub acceleration: f64,
    pub lateral_acceleration: f64,
    pub traction_loss: f64,
    pub wheel_slip: f64,
    pub afterburner: f64,
    pub cannon_firing: f64,
    pub landing_gear: f64,
    pub canopy: f64,
    pub missile_firing: f64,
    pub abs_lock: f64,
    pub touchdown: f64,
    pub turbulence: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traction_loss_front: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game_sound: Option<f64>,
}

/// Shaker effects enabled (`shaker_switch`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShakerSwitches {
    pub wheel_slip: bool,
    pub road_bump: bool,
    pub suspension: bool,
    pub engine: bool,
    pub gear: bool,
    pub impact: bool,
    pub deceleration: bool,
    pub acceleration: bool,
    pub lateral_acceleration: bool,
    pub traction_loss: bool,
    pub afterburner: bool,
    pub cannon_firing: bool,
    pub landing_gear: bool,
    pub canopy: bool,
    pub missile_firing: bool,
    pub abs_lock: bool,
    pub touchdown: bool,
    pub turbulence: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traction_loss_front: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game_sound: Option<bool>,
}

/// DSP filter type index per shaker effect (`shaker_dsp_type`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShakerFilterTypes {
    pub wheel_slip: i64,
    pub road_bump: i64,
    pub suspension: i64,
    pub engine: i64,
    pub gear: i64,
    pub impact: i64,
    pub deceleration: i64,
    pub acceleration: i64,
    pub lateral_acceleration: i64,
    pub traction_loss: i64,
    pub afterburner: i64,
    pub cannon_firing: i64,
    pub landing_gear: i64,
    pub canopy: i64,
    pub missile_firing: i64,
    pub abs_lock: i64,
    pub touchdown: i64,
    pub turbulence: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traction_loss_front: Option<i64>,
}

/// Filter cutoff per shaker effect, plus the engine fade-off band in % of max RPM (`shaker_cutoff`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShakerCutoffs {
    pub wheel_slip: f64,
    pub road_bump: f64,
    pub suspension: f64,
    pub engine: f64,
    pub gear: f64,
    pub impact: f64,
    pub deceleration: f64,
    pub acceleration: f64,
    pub lateral_acceleration: f64,
    pub traction_loss: f64,
    pub engine_fadeoff_start: f64,
    pub engine_fadeoff_end: f64,
    pub afterburner: f64,
    pub cannon_firing: f64,
    pub landing_gear: f64,
    pub canopy: f64,
    pub missile_firing: f64,
    pub abs_lock: f64,
    pub touchdown: f64,
    pub turbulence: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traction_loss_front: Option<f64>,
}

/// Filter resonance per shaker effect (`shaker_resonance`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShakerResonances {
    pub wheel_slip: f64,
    pub road_bump: f64,
    pub suspension: f64,
    pub engine: f64,
    pub gear: f64,
    pub impact: f64,
    pub deceleration: f64,
    pub acceleration: f64,
    pub lateral_acceleration: f64,
    pub traction_loss: f64,
    pub afterburner: f64,
    pub cannon_firing: f64,
    pub landing_gear: f64,
    pub canopy: f64,
    pub missile_firing: f64,
    pub abs_lock: f64,
    pub touchdown: f64,
    pub turbulence: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traction_loss_front: Option<f64>,
}

/// Per-effect pitch shift; present only in two pasted export profiles (`shaker_pitchshift`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShakerPitchShifts {
    pub acceleration: f64,
    pub deceleration: f64,
    pub engine: f64,
    pub gear: f64,
    pub impact: f64,
    pub lateral_acceleration: f64,
    pub road_bump: f64,
    pub suspension: f64,
    pub traction_loss: f64,
    pub wheel_slip: f64,
}

/// Motion effect intensities (`motion_effects`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MotionLevels {
    pub pitch: f64,
    pub roll: f64,
    pub yaw: f64,
    pub sway: f64,
    pub surge_acceleration: f64,
    pub surge_deceleration: f64,
    pub heave: f64,
    pub traction_loss: f64,
    pub wheel_slip: f64,
    pub rumble_strips: f64,
    pub afterburner: f64,
    pub cannon_firing: f64,
    pub landing_gear: f64,
    pub missile_firing: f64,
    pub canopy: f64,
    pub engine: f64,
    pub abs_lock: f64,
    pub traction_loss_front: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pitch_type: Option<AxisSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roll_type: Option<AxisSource>,
    /// Stray key written into the emulator profile by migration v394; nothing reads it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub motion_switch: Option<bool>,
}

/// Motion effects enabled (`motion_switch`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MotionSwitches {
    pub wheel_slip: bool,
    pub pitch: bool,
    pub roll: bool,
    pub yaw: bool,
    pub sway: bool,
    pub surge_acceleration: bool,
    pub surge_deceleration: bool,
    pub heave: bool,
    pub traction_loss: bool,
    pub rumble_strips: bool,
    pub afterburner: bool,
    pub cannon_firing: bool,
    pub landing_gear: bool,
    pub missile_firing: bool,
    pub canopy: bool,
    pub engine: bool,
    pub abs_lock: bool,
    pub traction_loss_front: bool,
}

/// Telemetry value mapped to full motion per axis (`max_telemetry_vars`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MotionRanges {
    pub pitch: f64,
    pub roll: f64,
    pub yaw: f64,
    pub sway: f64,
    pub heave: f64,
    pub traction_loss: f64,
    pub surge_acceleration: f64,
    pub surge_deceleration: f64,
    pub traction_loss_front: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pitch_rate: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roll_rate: Option<f64>,
}

/// Per-axis motion smoothing (`motion_effect_smoothing_factor`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MotionSmoothing {
    pub surge: f64,
    pub sway: f64,
    pub heave: f64,
    pub traction_loss: f64,
    pub pitch: f64,
    pub roll: f64,
    pub yaw: f64,
    pub traction_loss_front: f64,
}

/// iFlag patterns enabled (`iflag`; the flags whose group is not `none`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlagSwitches {
    pub green: bool,
    pub blue: bool,
    pub white: bool,
    pub red: bool,
    pub yellow: bool,
    pub yellow_waving: bool,
    pub repair: bool,
    pub black: bool,
    pub checkered: bool,
    pub start_ready: bool,
    pub start_set: bool,
    pub start_go: bool,
    pub disqualify: bool,
    pub debris: bool,
    pub one_lap_to_green: bool,
    pub caution_waving: bool,
    pub furled: bool,
    pub spotter_left: bool,
    pub pit_hold: bool,
    pub pit_release: bool,
    pub pit_done: bool,
    pub pit_limit: bool,
    pub spotter_right: bool,
    pub rpm: bool,
    pub gear_display: bool,
    pub sli: bool,
}

/// SIM3D rumble intensities, RPM fading and activation thresholds (`rumble_effects`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RumbleLevels {
    pub throttle_wheel_slip: f64,
    pub throttle_gear: f64,
    pub throttle_heave: f64,
    pub throttle_rumble_strips: f64,
    pub throttle_surge_acceleration: f64,
    pub throttle_traction_loss: f64,
    pub throttle_engine: f64,
    pub brake_wheel_slip: f64,
    pub brake_abs_lock: f64,
    pub brake_surge_deceleration: f64,
    pub brake_gear: f64,
    pub brake_heave: f64,
    pub brake_rumble_strips: f64,
    pub brake_traction_loss: f64,
    pub clutch_gear: f64,
    pub seat_wheel_slip: f64,
    pub seat_gear: f64,
    pub seat_heave: f64,
    pub seat_rumble_strips: f64,
    pub seat_surge_acceleration: f64,
    pub seat_traction_loss: f64,
    pub seat_engine: f64,
    pub rpm_start_rumble: f64,
    pub rpm_start_fading: f64,
    pub rpm_stop_fading: f64,
    pub wheel_slip_min_activation: f64,
    pub wheel_lock_min_activation: f64,
    pub traction_loss_min_activation: f64,
}

/// SIM3D rumble effects enabled (`rumble_switch`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RumbleSwitches {
    pub throttle_wheel_slip: bool,
    pub throttle_gear: bool,
    pub throttle_heave: bool,
    pub throttle_rumble_strips: bool,
    pub throttle_surge_acceleration: bool,
    pub throttle_traction_loss: bool,
    pub throttle_engine: bool,
    pub brake_wheel_slip: bool,
    pub brake_abs_lock: bool,
    pub brake_surge_deceleration: bool,
    pub brake_gear: bool,
    pub brake_heave: bool,
    pub brake_rumble_strips: bool,
    pub brake_traction_loss: bool,
    pub clutch_gear: bool,
    pub seat_wheel_slip: bool,
    pub seat_gear: bool,
    pub seat_heave: bool,
    pub seat_rumble_strips: bool,
    pub seat_surge_acceleration: bool,
    pub seat_traction_loss: bool,
    pub seat_engine: bool,
}

/// HapticR intensities, RPM fading and activation thresholds (`haptic_effects`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HapticLevels {
    pub wheel_wheel_slip: f64,
    pub wheel_abs_lock: f64,
    pub wheel_gear: f64,
    pub wheel_heave: f64,
    pub wheel_rumble_strips: f64,
    pub wheel_surge_acceleration: f64,
    pub wheel_surge_deceleration: f64,
    pub wheel_traction_loss: f64,
    pub wheel_engine: f64,
    pub traction_loss_min_activation: f64,
    pub rpm_start_rumble: f64,
    pub rpm_start_fading: f64,
    pub rpm_stop_fading: f64,
    pub wheel_slip_min_activation: f64,
    pub wheel_lock_min_activation: f64,
}

/// HapticR effects enabled (`haptic_switch`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HapticSwitches {
    pub wheel_wheel_slip: bool,
    pub wheel_abs_lock: bool,
    pub wheel_gear: bool,
    pub wheel_heave: bool,
    pub wheel_rumble_strips: bool,
    pub wheel_surge_acceleration: bool,
    pub wheel_surge_deceleration: bool,
    pub wheel_traction_loss: bool,
    pub wheel_engine: bool,
}
