//! Shared memory of the Slightly Mad Studios titles (Project CARS 1/2/3/Pro, Automobilista 2): the `GameInstance`
//! layouts and the channel rules the three listeners share.
//!
//! The layouts use natural alignment: Project
//! CARS (7316 bytes) and Project CARS 2 / Automobilista 2 (11624 / 11708 bytes, identical up to
//! `mSnowDensity`). The layouts used here count three 2-D arrays (`mOrientations`, `mCarNames`,
//! `mCarClassNames`) with one dimension only, so the `mTurboBoostPressure` offset used here is not where the game
//! writes that field.

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::answers::MotionRates;
use super::block_reader::Fields;
use super::packed::ArrayFields;
use super::python_numeric::{rad_to_deg, truncate_to_int};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PcarsKind {
    ProjectCars,
    ProjectCars2,
    Automobilista2,
}

fn corner_channel(name: &str, kind: PcarsKind) -> Option<(&str, usize)> {
    let (channel, wheel) = name.rsplit_once('_')?;
    let wheel = match wheel {
        "lf" => LF,
        "rf" => RF,
        "lr" => LR,
        "rr" => RR,
        _ => return None,
    };
    match channel {
        "wheel_slip" | "rumble_pitch" | "suspension_travel" => Some((channel, wheel)),
        "suspension_velocity" if kind != PcarsKind::ProjectCars => Some((channel, wheel)),
        "tyre_temperature" | "tyre_wear" if kind == PcarsKind::ProjectCars2 => Some((channel, wheel)),
        _ => None,
    }
}

/// One channel read, retaining value kinds and where the pause gate or the channel expression fails.
pub(crate) fn native_channels(
    game: Option<&GameInstance>,
    kind: PcarsKind,
    pause: bool,
    name: &str,
) -> Option<LegacyValue> {
    if pause && game?.game_state() != GAME_INGAME_PLAYING {
        return Some(LegacyValue::Int(0));
    }
    if matches!(name, "car_name" | "track_loc") {
        let decoded = game.and_then(|game| {
            super::python_numeric::decode_utf8(if name == "car_name" { game.car_name() } else { game.track_location() })
        });
        return if kind == PcarsKind::ProjectCars {
            Some(decoded.map_or(LegacyValue::Int(0), LegacyValue::Str))
        } else {
            decoded.map(LegacyValue::Str)
        };
    }
    let corner = corner_channel(name, kind);
    let common = matches!(
        name,
        "rpm"
            | "max_rpm"
            | "gear"
            | "speed"
            | "steering_wheel_angle"
            | "longitudinal_acceleration"
            | "lateral_acceleration"
            | "vertical_acceleration"
            | "vertical_velocity"
            | "longitudinal_velocity"
            | "lateral_velocity"
            | "roll"
            | "pitch"
            | "yaw"
            | "abs_lock"
            | "flag"
            | "raw_flag"
            | "oil_temperature"
            | "oil_pressure"
            | "water_temperature"
            | "water_pressure"
            | "fuel_pressure"
            | "fuel_level"
    ) || (name == "turbo_boost_pressure" && kind != PcarsKind::ProjectCars);
    if !common && corner.is_none() {
        return Some(LegacyValue::Int(0));
    }
    let game = game?;
    let (f, l) = (&game.fields, game.layout);
    let float = match name {
        "rpm" => return crate::udp_racing::int_text(f.f32(l.rpm)).map(LegacyValue::Str),
        "max_rpm" => return crate::udp_racing::int_text(f.f32(l.max_rpm)).map(LegacyValue::Str),
        "speed" => return crate::udp_racing::int_text(f.f32(l.speed)).map(LegacyValue::Str),
        "gear" => return Some(LegacyValue::Str(f.i32(l.gear).to_string())),
        "abs_lock" => return Some(LegacyValue::Int(i64::from(f.bool(l.anti_lock_active)))),
        "flag" => return Some(LegacyValue::Int(normalized_flag(game.highest_flag_colour()) as i64)),
        "raw_flag" => return Some(LegacyValue::Int(i64::from(game.highest_flag_colour()))),
        "steering_wheel_angle" => -f.f32(l.unfiltered_steering),
        "longitudinal_acceleration" => -game.local_acceleration(2),
        "lateral_acceleration" => game.local_acceleration(0),
        "vertical_acceleration" => {
            if kind == PcarsKind::ProjectCars {
                game.local_acceleration(1) / 10.0
            } else {
                game.local_acceleration(1)
            }
        }
        "vertical_velocity" => f.f32_at(l.local_velocity, 1),
        "longitudinal_velocity" => f.f32_at(l.local_velocity, 2),
        "lateral_velocity" => -f.f32_at(l.local_velocity, 0),
        "roll" => rad_to_deg(f.f32_at(l.orientation, 2)),
        "pitch" => rad_to_deg(f.f32_at(l.orientation, 0)),
        "yaw" => rad_to_deg(f.f32_at(l.orientation, 1)),
        "oil_temperature" => f.f32(l.oil_temp_celsius),
        "oil_pressure" => f.f32(l.oil_pressure_kpa),
        "water_temperature" => f.f32(l.water_temp_celsius),
        "water_pressure" => f.f32(l.water_pressure_kpa),
        "fuel_pressure" => f.f32(l.fuel_pressure_kpa),
        "fuel_level" => f.f32(l.fuel_level) * 100.0,
        "turbo_boost_pressure" => f.f32(PCARS2_TURBO_BOOST_PRESSURE),
        _ => {
            let (channel, wheel) = corner?;
            match channel {
                "rumble_pitch" => return Some(LegacyValue::Int(rumble_pitch(f.u32_at(l.terrain, wheel)) as i64)),
                "wheel_slip" => {
                    if kind == PcarsKind::Automobilista2 {
                        game.tyre_slip_speed(wheel) / 3.0
                    } else {
                        game.tyre_slip_speed(wheel)
                    }
                }
                "suspension_travel" if kind == PcarsKind::ProjectCars => game.local_acceleration(1) / 1000.0,
                "suspension_travel" => f.f32_at(PCARS2_SUSPENSION_TRAVEL, wheel),
                "suspension_velocity" => f.f32_at(PCARS2_SUSPENSION_VELOCITY, wheel),
                "tyre_temperature" => f.f32_at(l.tyre_temp, wheel),
                "tyre_wear" => f.f32_at(l.tyre_wear, wheel),
                _ => unreachable!("corner_channel only returns supported channels"),
            }
        }
    };
    Some(LegacyValue::Float(float))
}

/// Field offsets of one `GameInstance` layout.
pub(crate) struct GameInstanceLayout {
    pub(crate) version: usize,
    pub(crate) game_state: usize,
    pub(crate) unfiltered_steering: usize,
    pub(crate) car_name: usize,
    pub(crate) track_location: usize,
    pub(crate) highest_flag_colour: usize,
    pub(crate) oil_temp_celsius: usize,
    pub(crate) oil_pressure_kpa: usize,
    pub(crate) water_temp_celsius: usize,
    pub(crate) water_pressure_kpa: usize,
    pub(crate) fuel_pressure_kpa: usize,
    pub(crate) fuel_level: usize,
    pub(crate) speed: usize,
    pub(crate) rpm: usize,
    pub(crate) max_rpm: usize,
    pub(crate) gear: usize,
    pub(crate) anti_lock_active: usize,
    pub(crate) orientation: usize,
    pub(crate) local_velocity: usize,
    pub(crate) local_acceleration: usize,
    pub(crate) terrain: usize,
    pub(crate) tyre_slip_speed: usize,
    pub(crate) tyre_temp: usize,
    pub(crate) tyre_wear: usize,
}

/// The `GameInstance` layout of Project CARS.
pub(crate) const PCARS1: GameInstanceLayout = GameInstanceLayout {
    version: 0,
    game_state: 8,
    unfiltered_steering: 6436,
    car_name: 6444,
    track_location: 6576,
    highest_flag_colour: 6796,
    oil_temp_celsius: 6816,
    oil_pressure_kpa: 6820,
    water_temp_celsius: 6824,
    water_pressure_kpa: 6828,
    fuel_pressure_kpa: 6832,
    fuel_level: 6836,
    speed: 6844,
    rpm: 6848,
    max_rpm: 6852,
    gear: 6872,
    anti_lock_active: 6884,
    orientation: 6904,
    local_velocity: 6916,
    local_acceleration: 6952,
    terrain: 7004,
    tyre_slip_speed: 7052,
    tyre_temp: 7068,
    tyre_wear: 7132,
};

/// The `GameInstance` layout of Project CARS 2/3/Pro and Automobilista 2: the PC1 layout with
/// `mNumSectors` inserted after `mTrackLength`, plus later fields.
pub(crate) const PCARS2: GameInstanceLayout = GameInstanceLayout {
    version: 0,
    game_state: 8,
    unfiltered_steering: 6436,
    car_name: 6444,
    track_location: 6576,
    highest_flag_colour: 6800,
    oil_temp_celsius: 6820,
    oil_pressure_kpa: 6824,
    water_temp_celsius: 6828,
    water_pressure_kpa: 6832,
    fuel_pressure_kpa: 6836,
    fuel_level: 6840,
    speed: 6848,
    rpm: 6852,
    max_rpm: 6856,
    gear: 6876,
    anti_lock_active: 6888,
    orientation: 6908,
    local_velocity: 6920,
    local_acceleration: 6956,
    terrain: 7008,
    tyre_slip_speed: 7056,
    tyre_temp: 7072,
    tyre_wear: 7136,
};

/// `mSuspensionTravel` (PC2/AMS2 layouts only).
pub(crate) const PCARS2_SUSPENSION_TRAVEL: usize = 7340;
/// `mSuspensionVelocity` (PC2/AMS2 layouts only).
pub(crate) const PCARS2_SUSPENSION_VELOCITY: usize = 7356;
/// `mTurboBoostPressure` at the offset of the layout used here (see the module documentation).
pub(crate) const PCARS2_TURBO_BOOST_PRESSURE: usize = 10432;

/// Length of the character name fields (`STRING_LENGTH_MAX`).
pub(crate) const NAME_BYTES: usize = 64;

/// `GAME_INGAME_PLAYING`: the only game state in which channels are published while pausing is enabled.
pub(crate) const GAME_INGAME_PLAYING: u32 = 2;

/// The rate channels (`yaw_rate`, `pitch_rate`, `roll_rate`, none implemented by the three listeners) while the
/// listener has no snapshot: with pausing enabled the pause gate (which reads
/// `mGameState`) fails; without it the gate is skipped and they fall through to `0`.
pub(crate) fn rates_without_game(pause_when_not_playing: bool) -> MotionRates {
    if pause_when_not_playing {
        MotionRates::RAISED
    } else {
        MotionRates::ZERO
    }
}

/// One `GameInstance` image read through its layout.
pub(crate) struct GameInstance<'a> {
    pub(crate) fields: Fields<'a>,
    pub(crate) layout: &'static GameInstanceLayout,
}

impl GameInstance<'_> {
    pub(crate) fn version(&self) -> u32 {
        self.fields.u32(self.layout.version)
    }

    pub(crate) fn game_state(&self) -> u32 {
        self.fields.u32(self.layout.game_state)
    }

    /// The presence test: `mSpeed + mRpm + mMaxRPM + mGear != 0`, summed left to right as doubles.
    pub(crate) fn has_engine_data(&self) -> bool {
        let l = self.layout;
        let sum = self.fields.f32(l.speed)
            + self.fields.f32(l.rpm)
            + self.fields.f32(l.max_rpm)
            + f64::from(self.fields.i32(l.gear));
        sum != 0.0
    }

    pub(crate) fn car_name(&self) -> &[u8] {
        self.fields.c_chars(self.layout.car_name, NAME_BYTES)
    }

    pub(crate) fn track_location(&self) -> &[u8] {
        self.fields.c_chars(self.layout.track_location, NAME_BYTES)
    }

    pub(crate) fn highest_flag_colour(&self) -> u32 {
        self.fields.u32(self.layout.highest_flag_colour)
    }

    pub(crate) fn tyre_slip_speed(&self, wheel: usize) -> f64 {
        self.fields.f32_at(self.layout.tyre_slip_speed, wheel)
    }

    pub(crate) fn local_acceleration(&self, axis: usize) -> f64 {
        self.fields.f32_at(self.layout.local_acceleration, axis)
    }
}

/// Wheel indices of the SMS arrays (`TYRE_FRONT_LEFT` .. `TYRE_REAR_RIGHT`).
pub(crate) const LF: usize = 0;
pub(crate) const RF: usize = 1;
pub(crate) const LR: usize = 2;
pub(crate) const RR: usize = 3;

/// The channels computed identically by the `projectcars`, `projectcars2` and
/// `automobilista2` listeners; acceleration scaling, wheel slip and the names differ per listener.
pub(crate) fn write_common_channels(game: &GameInstance, t: &mut Telemetry) {
    let f = &game.fields;
    let l = game.layout;
    // The float fields as integers; NaN/inf has no integer value and keeps the previous value (no registry default).
    if let Some(rpm) = truncate_to_int(f.f32(l.rpm)) {
        t.rpm = rpm;
    }
    if let Some(max_rpm) = truncate_to_int(f.f32(l.max_rpm)) {
        t.max_rpm = max_rpm;
    }
    t.gear = f64::from(f.i32(l.gear));
    if let Some(speed) = truncate_to_int(f.f32(l.speed)) {
        t.speed = speed;
    }
    t.steering_wheel_angle = -f.f32(l.unfiltered_steering);
    t.longitudinal_acceleration = -game.local_acceleration(2);
    t.lateral_acceleration = game.local_acceleration(0);
    t.vertical_velocity = f.f32_at(l.local_velocity, 1);
    t.longitudinal_velocity = f.f32_at(l.local_velocity, 2);
    t.lateral_velocity = -f.f32_at(l.local_velocity, 0);
    t.roll = rad_to_deg(f.f32_at(l.orientation, 2));
    t.pitch = rad_to_deg(f.f32_at(l.orientation, 0));
    t.yaw = rad_to_deg(f.f32_at(l.orientation, 1));
    t.abs_lock = if f.bool(l.anti_lock_active) { 1.0 } else { 0.0 };
    t.rumble_pitch_lf = rumble_pitch(f.u32_at(l.terrain, LF));
    t.rumble_pitch_rf = rumble_pitch(f.u32_at(l.terrain, RF));
    t.rumble_pitch_lr = rumble_pitch(f.u32_at(l.terrain, LR));
    t.rumble_pitch_rr = rumble_pitch(f.u32_at(l.terrain, RR));
    let colour = game.highest_flag_colour();
    t.flag = normalized_flag(colour);
    t.raw_flag = f64::from(colour);
    t.oil_temperature = f.f32(l.oil_temp_celsius);
    t.oil_pressure = f.f32(l.oil_pressure_kpa);
    t.water_temperature = f.f32(l.water_temp_celsius);
    t.water_pressure = f.f32(l.water_pressure_kpa);
    t.fuel_pressure = f.f32(l.fuel_pressure_kpa);
    t.fuel_level = f.f32(l.fuel_level) * 100.0;
}

/// The `rumble_pitch_*` channels: `40` on the kerb terrains (`TERRAIN_RUMBLE_STRIPS` 10,
/// `TERRAIN_EXIT_RUMBLE_STRIPS` 25, `B1RUMBLES` 40, `B2RUMBLES` 41), otherwise `0`.
pub(crate) fn rumble_pitch(terrain: u32) -> f64 {
    if matches!(terrain, 10 | 25 | 40 | 41) {
        40.0
    } else {
        0.0
    }
}

/// The `flag` channel: colours 3 and 4 both become 3, every other `mHighestFlagColour` passes through.
pub(crate) fn normalized_flag(colour: u32) -> f64 {
    if colour == 3 || colour == 4 {
        3.0
    } else {
        f64::from(colour)
    }
}

#[cfg(test)]
pub(crate) mod test_image {
    //! A `GameInstance` image builder for the three listeners' tests.

    use super::*;
    use crate::shared_memory::block_reader::fake::BlockBuilder as Image;

    /// An image of `size` bytes in `layout` with recognisable values in every field the listeners read.
    pub(crate) fn sample(layout: &GameInstanceLayout, size: usize, version: u32) -> Image {
        let mut i = Image::new(size);
        i.u32(layout.version, version)
            .u32(layout.game_state, GAME_INGAME_PLAYING)
            .f32(layout.unfiltered_steering, 0.5)
            .bytes(layout.car_name, b"Formula Renault")
            .bytes(layout.track_location, b"Brands Hatch")
            .u32(layout.highest_flag_colour, 4)
            .f32(layout.oil_temp_celsius, 95.0)
            .f32(layout.oil_pressure_kpa, 400.0)
            .f32(layout.water_temp_celsius, 85.0)
            .f32(layout.water_pressure_kpa, 120.0)
            .f32(layout.fuel_pressure_kpa, 300.0)
            .f32(layout.fuel_level, 0.25)
            .f32(layout.speed, 41.9)
            .f32(layout.rpm, 6999.9)
            .f32(layout.max_rpm, 8000.5)
            .i32(layout.gear, 3)
            .u8(layout.anti_lock_active, 1)
            .f32(layout.orientation, 0.1)
            .f32(layout.orientation + 4, -0.2)
            .f32(layout.orientation + 8, 0.3)
            .f32(layout.local_velocity, 1.5)
            .f32(layout.local_velocity + 4, -0.5)
            .f32(layout.local_velocity + 8, 40.0)
            .f32(layout.local_acceleration, 2.0)
            .f32(layout.local_acceleration + 4, 9.0)
            .f32(layout.local_acceleration + 8, -3.0)
            .u32(layout.terrain, 10)
            .u32(layout.terrain + 4, 25)
            .u32(layout.terrain + 8, 40)
            .u32(layout.terrain + 12, 7)
            .f32(layout.tyre_slip_speed, 0.3)
            .f32(layout.tyre_slip_speed + 4, 0.6)
            .f32(layout.tyre_slip_speed + 8, 0.9)
            .f32(layout.tyre_slip_speed + 12, 1.2);
        i
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The offsets of one `GameInstanceLayout`, in field order.
    fn offsets(l: &GameInstanceLayout) -> [usize; 24] {
        [
            l.version,
            l.game_state,
            l.unfiltered_steering,
            l.car_name,
            l.track_location,
            l.highest_flag_colour,
            l.oil_temp_celsius,
            l.oil_pressure_kpa,
            l.water_temp_celsius,
            l.water_pressure_kpa,
            l.fuel_pressure_kpa,
            l.fuel_level,
            l.speed,
            l.rpm,
            l.max_rpm,
            l.gear,
            l.anti_lock_active,
            l.orientation,
            l.local_velocity,
            l.local_acceleration,
            l.terrain,
            l.tyre_slip_speed,
            l.tyre_temp,
            l.tyre_wear,
        ]
    }

    /// The Project CARS GameInstance (version 5): the offsets of every field the listener reads
    /// (mVersion, mGameState, mUnfilteredSteering, mCarName, mTrackLocation, mHighestFlagColour, mOilTempCelsius ..
    /// mFuelLevel, mSpeed, mRpm, mMaxRPM, mGear, mAntiLockActive, mOrientation, mLocalVelocity, mLocalAcceleration,
    /// mTerrain, mTyreSlipSpeed, mTyreTemp, mTyreWear).
    #[test]
    fn carseour_layout() {
        assert_eq!(
            offsets(&PCARS1),
            [
                0, 8, 6436, 6444, 6576, 6796, 6816, 6820, 6824, 6828, 6832, 6836, 6844, 6848, 6852, 6872, 6884, 6904,
                6916, 6952, 7004, 7052, 7068, 7132
            ]
        );
    }

    /// The Project CARS 2 / Automobilista 2 GameInstance (identical up to mTurboBoostPressure): the same fields,
    /// plus mSuspensionTravel, mSuspensionVelocity and mTurboBoostPressure.
    #[test]
    fn pypcars2api_layout() {
        assert_eq!(
            offsets(&PCARS2),
            [
                0, 8, 6436, 6444, 6576, 6800, 6820, 6824, 6828, 6832, 6836, 6840, 6848, 6852, 6856, 6876, 6888, 6908,
                6920, 6956, 7008, 7056, 7072, 7136
            ]
        );
        assert_eq!(
            (PCARS2_SUSPENSION_TRAVEL, PCARS2_SUSPENSION_VELOCITY, PCARS2_TURBO_BOOST_PRESSURE),
            (7340, 7356, 10432)
        );
    }

    /// rumble_pitch_*: the four kerb terrains give 40.
    #[test]
    fn rumble_terrains() {
        for terrain in [10, 25, 40, 41] {
            assert_eq!(rumble_pitch(terrain), 40.0);
        }
        for terrain in [0, 9, 11, 24, 26, 39, 42] {
            assert_eq!(rumble_pitch(terrain), 0.0);
        }
    }

    /// flag: 3 and 4 collapse to 3, other colours pass through.
    #[test]
    fn flag_collapse() {
        assert_eq!(normalized_flag(3), 3.0);
        assert_eq!(normalized_flag(4), 3.0);
        assert_eq!(normalized_flag(6), 6.0);
        assert_eq!(normalized_flag(0), 0.0);
        assert_eq!(normalized_flag(11), 11.0);
    }

    /// Presence test: all four engine fields zero means not running; any non-zero sum is running.
    #[test]
    fn engine_data_sum() {
        let image = test_image::sample(&PCARS1, 7316, 5).build();
        let game = GameInstance { fields: Fields(&image), layout: &PCARS1 };
        assert!(game.has_engine_data());
        let mut zero = test_image::sample(&PCARS1, 7316, 5);
        zero.f32(PCARS1.speed, 0.0).f32(PCARS1.rpm, 0.0).f32(PCARS1.max_rpm, 0.0).i32(PCARS1.gear, 0);
        let zero = zero.build();
        assert!(!GameInstance { fields: Fields(&zero), layout: &PCARS1 }.has_engine_data());
        let mut cancel = test_image::sample(&PCARS1, 7316, 5);
        cancel.f32(PCARS1.speed, 0.0).f32(PCARS1.rpm, 1.0).f32(PCARS1.max_rpm, 0.0).i32(PCARS1.gear, -1);
        let cancel = cancel.build();
        assert!(!GameInstance { fields: Fields(&cancel), layout: &PCARS1 }.has_engine_data());
    }
}
