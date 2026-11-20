//! Assetto Corsa, Assetto Corsa Competizione and Assetto Corsa Rally (listener `ac`): the Kunos shared memory pages
//! `acpmf_physics`, `acpmf_graphics` and `acpmf_static`.
//!
//! The page layouts use 4-byte packing.

use std::sync::{Mutex, PoisonError};

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::answers::{write_all_zero, zero_generic_channels, MotionRates};
use super::block_reader::{BlockOpener, BlockReader, Fields, MappingSpec};
use super::liveness::PacketLiveness;
use super::packed::ArrayFields;
use super::pages::LiveMirrors;
use super::python_numeric::{percent_or_zero, rad_to_deg, round_to_int, truncate_to_int};
use crate::{NativeRead, Source, SourceError};

/// `SPageFilePhysics` (580 bytes), mapping name `acpmf_physics`.
pub const PHYSICS: MappingSpec = MappingSpec { name: "acpmf_physics", size: 580 };
/// `SPageFileGraphic` (1284 bytes), mapping name `acpmf_graphics`.
pub const GRAPHICS: MappingSpec = MappingSpec { name: "acpmf_graphics", size: 1284 };
/// `SPageFileStatic` (452 bytes), mapping name `acpmf_static`.
pub const STATIC: MappingSpec = MappingSpec { name: "acpmf_static", size: 452 };
/// Every mapping this listener reads.
pub const MAPPINGS: [MappingSpec; 3] = [PHYSICS, GRAPHICS, STATIC];

/// Byte offsets inside the Kunos pages that the listener reads (4-byte packing). The physics offsets are shared with
/// the Assetto Corsa EVO physics page, whose first 580 bytes have the same layout.
pub(crate) mod physics {
    pub(crate) const PACKET_ID: usize = 0;
    pub(crate) const FUEL: usize = 12;
    pub(crate) const GEAR: usize = 16;
    pub(crate) const RPMS: usize = 20;
    pub(crate) const STEER_ANGLE: usize = 24;
    pub(crate) const SPEED_KMH: usize = 28;
    pub(crate) const ACC_G: usize = 44;
    pub(crate) const WHEEL_SLIP: usize = 56;
    pub(crate) const SUSPENSION_TRAVEL: usize = 184;
    pub(crate) const HEADING: usize = 208;
    pub(crate) const PITCH: usize = 212;
    pub(crate) const ROLL: usize = 216;
    pub(crate) const ABS: usize = 252;
    pub(crate) const TURBO_BOOST: usize = 276;
    pub(crate) const LOCAL_ANGULAR_VEL: usize = 296;
    pub(crate) const LOCAL_VELOCITY: usize = 568;
}

mod graphics {
    pub(super) const STATUS: usize = 4;
    pub(super) const FLAG_TYPE: usize = 1224;
}

mod statics {
    pub(super) const CAR_MODEL: usize = 68;
    pub(super) const TRACK: usize = 134;
    pub(super) const NAME_CHARS: usize = 33;
    pub(super) const MAX_RPM: usize = 412;
    pub(super) const MAX_FUEL: usize = 416;
}

/// `AC_STATUS.AC_LIVE`: the only status in which channels are published while pausing is enabled.
pub(crate) const STATUS_LIVE: i32 = 2;

/// Which of the three games served by this listener is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcGame {
    /// Game id `ac`.
    AssettoCorsa,
    /// Game id `acc`.
    Competizione,
    /// Game id `acrally`.
    Rally,
}

impl AcGame {
    /// The game id of the catalog.
    pub fn id(self) -> &'static str {
        match self {
            AcGame::AssettoCorsa => "ac",
            AcGame::Competizione => "acc",
            AcGame::Rally => "acrally",
        }
    }

    /// AC and ACC zero every channel outside `AC_LIVE` when pausing is enabled; Rally is never gated.
    fn honours_pause(self) -> bool {
        !matches!(self, AcGame::Rally)
    }
}

/// `max_rpm` answered for Assetto Corsa Rally, which does not read `static.maxRpm`.
const RALLY_MAX_RPM: f64 = 10000.0;

/// Listener `ac`.
pub struct AssettoCorsa<O: BlockOpener = LiveMirrors> {
    opener: std::marker::PhantomData<O>,
    /// The three pages, created or opened at construction and kept for the life of
    /// the listener (live views).
    pages: Mutex<[Option<Box<dyn BlockReader>>; 3]>,
    game: AcGame,
    pause_when_not_playing: bool,
    pause_reader: Option<super::NativePauseReader>,
    game_reader: Option<super::NativeGameReader>,
    liveness: PacketLiveness,
    rates: MotionRates,
}

impl AssettoCorsa<LiveMirrors> {
    /// Listener reading the bridge mirrors. `pause_when_not_playing` is `APP/pause_telemetry_when_not_playing`.
    pub fn new(game: AcGame, pause_when_not_playing: bool) -> Self {
        AssettoCorsa::with_opener(LiveMirrors, game, pause_when_not_playing)
    }
}

impl<O: BlockOpener> AssettoCorsa<O> {
    /// Listener reading the mappings `opener` opens.
    pub fn with_opener(mut opener: O, game: AcGame, pause_when_not_playing: bool) -> Self {
        let pages = MAPPINGS.map(|spec| opener.create_held(spec).ok());
        AssettoCorsa {
            opener: std::marker::PhantomData,
            pages: Mutex::new(pages),
            game,
            pause_when_not_playing,
            pause_reader: None,
            game_reader: None,
            liveness: PacketLiveness::default(),
            rates: MotionRates::ZERO,
        }
    }

    /// Selects the running game (the dispatcher's `game_running`).
    pub fn set_game(&mut self, game: AcGame) {
        self.game = game;
    }

    /// Updates `APP/pause_telemetry_when_not_playing`.
    pub fn set_pause_when_not_playing(&mut self, pause: bool) {
        self.pause_when_not_playing = pause;
    }

    pub fn set_native_readers(&mut self, game: super::NativeGameReader, pause: super::NativePauseReader) {
        self.game_reader = Some(game);
        self.pause_reader = Some(pause);
    }

    /// The rate channels answered by the last [`Source::poll`].
    pub fn motion_rates(&self) -> MotionRates {
        self.rates
    }

    fn copy_page(&self, index: usize) -> Option<Vec<u8>> {
        self.pages.lock().unwrap_or_else(PoisonError::into_inner)[index].as_mut()?.read().ok().map(<[u8]>::to_vec)
    }

    fn native_game(&self) -> String {
        self.game_reader.as_ref().map_or_else(|| self.game.id().to_owned(), |read| read())
    }

    fn native_answer(&self, name: &str) -> Option<LegacyValue> {
        if matches!(self.native_game().as_str(), "ac" | "acc")
            && super::native_pause(&self.pause_reader, self.pause_when_not_playing)?
            && Fields(&self.copy_page(1)?).i32(graphics::STATUS) != STATUS_LIVE
        {
            return Some(LegacyValue::Int(0));
        }
        if name == "roll" {
            let negative = matches!(self.native_game().as_str(), "ac" | "acc" | "acrally");
            return native_kunos_physics(&Fields(&self.copy_page(0)?), name, 2.0, negative);
        }
        if uses_kunos_physics(name) {
            return native_kunos_physics(&Fields(&self.copy_page(0)?), name, 2.0, true);
        }
        Some(match name {
            "max_rpm" => {
                if matches!(self.native_game().as_str(), "ac" | "acc") {
                    LegacyValue::Str(Fields(&self.copy_page(2)?).i32(statics::MAX_RPM).to_string())
                } else {
                    LegacyValue::Int(10000)
                }
            }
            "car_name" => {
                LegacyValue::Str(Fields(&self.copy_page(2)?).wide_chars(statics::CAR_MODEL, statics::NAME_CHARS))
            }
            "track_loc" => {
                LegacyValue::Str(Fields(&self.copy_page(2)?).wide_chars(statics::TRACK, statics::NAME_CHARS))
            }
            "fuel_level" => {
                // Only this branch answers 0 itself when a page cannot be copied or the tank size is zero.
                let fuel = self.copy_page(0).and_then(|physics| self.copy_page(2).map(|statics| (physics, statics)));
                fuel.map_or(LegacyValue::Int(0), |(physics, statics)| {
                    native_fuel_level(Fields(&physics).f32(physics::FUEL), Fields(&statics).f32(statics::MAX_FUEL))
                })
            }
            "flag" => LegacyValue::Int(kunos_flag(Fields(&self.copy_page(1)?).i32(graphics::FLAG_TYPE)) as i64),
            _ => LegacyValue::Int(0),
        })
    }
}

impl<O: BlockOpener> Source for AssettoCorsa<O> {
    fn id(&self) -> &'static str {
        "ac"
    }

    /// The liveness test: alive while `physics.packetId` changes, and for 49 further
    /// unchanged calls; never on the first call.
    fn is_running(&mut self) -> Result<bool, SourceError> {
        let Some(physics) = self.copy_page(0) else {
            return Ok(false);
        };
        let packet_id = Fields(&physics).i32(physics::PACKET_ID);
        Ok(self.liveness.observe(packet_id))
    }

    fn receive_native(&mut self, _t: &mut Telemetry) -> Result<bool, SourceError> {
        Ok(true)
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        self.native_answer(name)
    }

    fn read_native(&self, name: &str) -> NativeRead {
        self.native_answer(name).map_or(NativeRead::Raised, NativeRead::Value)
    }

    /// Channel reads over the live pages (the receive step itself does nothing).
    fn poll(&mut self, t: &mut Telemetry) -> Result<bool, SourceError> {
        let Some(pages) = read_pages(self.pages.get_mut().unwrap_or_else(PoisonError::into_inner)) else {
            return Ok(false);
        };
        let (physics, graphics, statics) = (Fields(&pages[0]), Fields(&pages[1]), Fields(&pages[2]));
        if self.pause_when_not_playing && self.game.honours_pause() && graphics.i32(graphics::STATUS) != STATUS_LIVE {
            write_all_zero(t);
            self.rates = MotionRates::ZERO;
            return Ok(true);
        }
        zero_generic_channels(t);
        write_kunos_physics(&physics, t);
        self.rates = MotionRates::yaw_only(physics.f32_at(physics::LOCAL_ANGULAR_VEL, 0));
        t.vertical_acceleration = physics.f32_at(physics::ACC_G, 1) * 2.0;
        t.roll = -rad_to_deg(physics.f32(physics::ROLL));
        t.max_rpm = match self.game {
            AcGame::Rally => RALLY_MAX_RPM,
            _ => f64::from(statics.i32(statics::MAX_RPM)),
        };
        t.vehicle = statics.wide_chars(statics::CAR_MODEL, statics::NAME_CHARS);
        t.location = statics.wide_chars(statics::TRACK, statics::NAME_CHARS);
        t.fuel_level = percent_or_zero(physics.f32(physics::FUEL), statics.f32(statics::MAX_FUEL));
        t.flag = kunos_flag(graphics.i32(graphics::FLAG_TYPE));
        Ok(true)
    }
}

pub(crate) fn uses_kunos_physics(name: &str) -> bool {
    matches!(
        name,
        "rpm"
            | "gear"
            | "speed"
            | "steering_wheel_angle"
            | "wheel_slip_lf"
            | "wheel_slip_rf"
            | "wheel_slip_lr"
            | "wheel_slip_rr"
            | "suspension_travel_lf"
            | "suspension_travel_rf"
            | "suspension_travel_lr"
            | "suspension_travel_rr"
            | "longitudinal_acceleration"
            | "lateral_acceleration"
            | "vertical_acceleration"
            | "roll"
            | "yaw"
            | "pitch"
            | "yaw_rate"
            | "vertical_velocity"
            | "lateral_velocity"
            | "longitudinal_velocity"
            | "abs_lock"
            | "turbo_boost_pressure"
    )
}

/// A single getter, retaining its text, integer or float result. `None` means the read fails.
pub(crate) fn native_kunos_physics(
    p: &Fields,
    name: &str,
    vertical_gain: f64,
    invert_roll: bool,
) -> Option<LegacyValue> {
    use physics::*;
    let value = match name {
        "rpm" => return Some(LegacyValue::Str(p.i32(RPMS).to_string())),
        "gear" => return Some(LegacyValue::Str((i64::from(p.i32(GEAR)) - 1).to_string())),
        "speed" => {
            return Some(LegacyValue::Str(crate::udp_racing::int_text(round_to_int(p.f32(SPEED_KMH) * 0.277778)?)?))
        }
        "abs_lock" => return crate::udp_racing::native_int(p.f32(ABS)),
        "turbo_boost_pressure" => return crate::udp_racing::native_int(p.f32(TURBO_BOOST)),
        "steering_wheel_angle" => -p.f32(STEER_ANGLE),
        "wheel_slip_lf" => p.f32_at(WHEEL_SLIP, 0),
        "wheel_slip_rf" => p.f32_at(WHEEL_SLIP, 1),
        "wheel_slip_lr" => p.f32_at(WHEEL_SLIP, 2),
        "wheel_slip_rr" => p.f32_at(WHEEL_SLIP, 3),
        "suspension_travel_lf" => p.f32_at(SUSPENSION_TRAVEL, 0) / 3.0,
        "suspension_travel_rf" => p.f32_at(SUSPENSION_TRAVEL, 1) / 3.0,
        "suspension_travel_lr" => p.f32_at(SUSPENSION_TRAVEL, 2) / 3.0,
        "suspension_travel_rr" => p.f32_at(SUSPENSION_TRAVEL, 3) / 3.0,
        "longitudinal_acceleration" => p.f32_at(ACC_G, 2) * 10.0,
        "lateral_acceleration" => p.f32_at(ACC_G, 0) * 8.0,
        "vertical_acceleration" => p.f32_at(ACC_G, 1) * vertical_gain,
        "roll" => {
            let roll = rad_to_deg(p.f32(ROLL));
            if invert_roll {
                -roll
            } else {
                roll
            }
        }
        "yaw" => rad_to_deg(p.f32(HEADING)),
        "pitch" => rad_to_deg(p.f32(PITCH)),
        "yaw_rate" => p.f32_at(LOCAL_ANGULAR_VEL, 0),
        "vertical_velocity" => p.f32_at(LOCAL_VELOCITY, 1),
        "lateral_velocity" => -p.f32_at(LOCAL_VELOCITY, 0),
        "longitudinal_velocity" => p.f32_at(LOCAL_VELOCITY, 2),
        _ => return Some(LegacyValue::Int(0)),
    };
    Some(LegacyValue::Float(value))
}

pub(crate) fn native_fuel_level(numerator: f64, denominator: f64) -> LegacyValue {
    if denominator == 0.0 {
        LegacyValue::Int(0)
    } else {
        LegacyValue::Float(numerator / denominator * 100.0)
    }
}

/// Copies of the three pages, or `None` while any cannot be read.
fn read_pages(pages: &mut [Option<Box<dyn BlockReader>>; 3]) -> Option<[Vec<u8>; 3]> {
    let mut read = |index: usize| pages[index].as_mut()?.read().ok().map(<[u8]>::to_vec);
    Some([read(0)?, read(1)?, read(2)?])
}

/// The channels the `ac` and `acevo` listeners compute identically from the physics page.
pub(crate) fn write_kunos_physics(physics: &Fields, t: &mut Telemetry) {
    use physics::*;
    t.rpm = f64::from(physics.i32(RPMS));
    t.gear = f64::from(physics.i32(GEAR)) - 1.0;
    if let Some(speed) = round_to_int(physics.f32(SPEED_KMH) * 0.277778) {
        t.speed = speed;
    }
    t.steering_wheel_angle = -physics.f32(STEER_ANGLE);
    t.wheel_slip_lf = physics.f32_at(WHEEL_SLIP, 0);
    t.wheel_slip_rf = physics.f32_at(WHEEL_SLIP, 1);
    t.wheel_slip_lr = physics.f32_at(WHEEL_SLIP, 2);
    t.wheel_slip_rr = physics.f32_at(WHEEL_SLIP, 3);
    t.suspension_travel_lf = physics.f32_at(SUSPENSION_TRAVEL, 0) / 3.0;
    t.suspension_travel_rf = physics.f32_at(SUSPENSION_TRAVEL, 1) / 3.0;
    t.suspension_travel_lr = physics.f32_at(SUSPENSION_TRAVEL, 2) / 3.0;
    t.suspension_travel_rr = physics.f32_at(SUSPENSION_TRAVEL, 3) / 3.0;
    t.longitudinal_acceleration = physics.f32_at(ACC_G, 2) * 10.0;
    t.lateral_acceleration = physics.f32_at(ACC_G, 0) * 8.0;
    t.vertical_velocity = physics.f32_at(LOCAL_VELOCITY, 1);
    t.lateral_velocity = -physics.f32_at(LOCAL_VELOCITY, 0);
    t.longitudinal_velocity = physics.f32_at(LOCAL_VELOCITY, 2);
    t.yaw = rad_to_deg(physics.f32(HEADING));
    t.pitch = rad_to_deg(physics.f32(PITCH));
    // NaN/inf has no integer value, which makes the dispatcher store the registry default 0.
    t.abs_lock = truncate_to_int(physics.f32(ABS)).unwrap_or(0.0);
    t.turbo_boost_pressure = truncate_to_int(physics.f32(TURBO_BOOST)).unwrap_or(0.0);
}

/// The `flag` channel: Kunos `AC_FLAG_TYPE` to the normalized flag.
fn kunos_flag(flag_type: i32) -> f64 {
    match flag_type {
        1 => 2.0,  // AC_BLUE_FLAG
        7 => 12.0, // AC_GREEN_FLAG
        5 => 11.0, // AC_CHECKERED_FLAG
        2 => 6.0,  // AC_YELLOW_FLAG
        4 => 3.0,  // AC_WHITE_FLAG
        3 => 10.0, // AC_BLACK_FLAG
        _ => 0.0,  // AC_NO_FLAG, AC_PENALTY_FLAG
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_kunos_environment_reads_current_game_and_pause_at_each_stage() {
        use std::sync::{Arc, Mutex};
        let mut ac = AssettoCorsa::with_opener(pages(1, 0, 0, 50.0), AcGame::AssettoCorsa, true);
        let game = Arc::new(Mutex::new(String::from("acrally")));
        ac.set_native_readers(
            {
                let game = game.clone();
                Arc::new(move || game.lock().unwrap().clone())
            },
            Arc::new(|| None),
        );
        assert_eq!(ac.read_native("rpm"), NativeRead::Value(LegacyValue::from("6543")), "Rally skips pause read");
        assert_eq!(ac.read_native("max_rpm"), NativeRead::Value(LegacyValue::Int(10000)));
        *game.lock().unwrap() = "ac".into();
        assert_eq!(ac.read_native("unknown"), NativeRead::Raised, "required pause read precedes unknown branch");
        *game.lock().unwrap() = "none".into();
        assert_eq!(ac.read_native("rpm"), NativeRead::Value(LegacyValue::from("6543")));
        assert_eq!(ac.read_native("max_rpm"), NativeRead::Value(LegacyValue::Int(10000)));
    }

    #[test]
    fn native_kunos_second_game_read_controls_maximum_and_roll() {
        use std::collections::VecDeque;
        use std::sync::{Arc, Mutex};
        let mut ac = AssettoCorsa::with_opener(pages(1, 2, 0, 50.0), AcGame::AssettoCorsa, false);
        let sequence = Arc::new(Mutex::new(VecDeque::from(["ac", "none", "none", "ac"])));
        let reads = Arc::new(Mutex::new(Vec::new()));
        ac.set_native_readers(
            {
                let (sequence, reads) = (sequence.clone(), reads.clone());
                Arc::new(move || {
                    reads.lock().unwrap().push("game");
                    sequence.lock().unwrap().pop_front().unwrap().to_owned()
                })
            },
            {
                let reads = reads.clone();
                Arc::new(move || {
                    reads.lock().unwrap().push("pause");
                    Some(LegacyValue::Bool(false))
                })
            },
        );
        assert_eq!(ac.read_native("max_rpm"), NativeRead::Value(LegacyValue::Int(10000)));
        assert_eq!(ac.read_native("max_rpm"), NativeRead::Value(LegacyValue::from("7500")));
        assert_eq!(*reads.lock().unwrap(), ["game", "pause", "game", "game", "game"]);
        sequence.lock().unwrap().extend(["none", "none", "none", "ac"]);
        let NativeRead::Value(LegacyValue::Float(positive)) = ac.read_native("roll") else { panic!("float roll") };
        let NativeRead::Value(LegacyValue::Float(negative)) = ac.read_native("roll") else { panic!("float roll") };
        assert!(positive > 0.0 && negative < 0.0);
        assert_eq!(positive, -negative);
    }
    use crate::shared_memory::answers::ZERO_NAME;
    use crate::shared_memory::block_reader::fake::BlockBuilder as Image;
    use crate::shared_memory::block_reader::fake::FakeBlocks;
    use crate::shared_memory::packed::utf16le;

    #[test]
    fn native_receive_does_not_project_live_getters() {
        let mut ac = AssettoCorsa::with_opener(pages(1, 2, 0, 50.0), AcGame::AssettoCorsa, true);
        let mut t = Telemetry { speed: 999.0, ..Telemetry::off() };
        ac.receive_native(&mut t).unwrap();
        assert_eq!(t.speed, 999.0);
    }

    #[test]
    fn native_getters_retain_original_types_and_live_pages() {
        use crate::derive::{Listener, Rate, SourceListener};
        let blocks = pages(1, 2, 7, 60.0);
        let ac = AssettoCorsa::with_opener(blocks.clone(), AcGame::AssettoCorsa, true);
        let mut listener = SourceListener::new(ac);
        listener.receive();
        for (name, expected) in [
            ("rpm", LegacyValue::Str("6543".into())),
            ("gear", LegacyValue::Str("2".into())),
            ("speed", LegacyValue::Str("3".into())),
            ("max_rpm", LegacyValue::Str("7500".into())),
            ("car_name", LegacyValue::Str("ks_mazda_mx5_cup".into())),
            ("track_loc", LegacyValue::Str("magione".into())),
            ("flag", LegacyValue::Int(12)),
            ("abs_lock", LegacyValue::Int(0)),
            ("turbo_boost_pressure", LegacyValue::Int(1)),
            ("vertical_acceleration", LegacyValue::Float(3.0)),
            ("longitudinal_velocity", LegacyValue::Float(33.0)),
            ("fuel_level", LegacyValue::Float(50.0)),
            ("unknown", LegacyValue::Int(0)),
        ] {
            assert_eq!(listener.native_channel(name), Some(expected), "{name}");
        }
        assert_eq!(listener.rate(Rate::Yaw), Some(f64::from(0.7f32)));
        assert_eq!(listener.rate(Rate::Pitch), Some(0.0));
        let mut changed = physics_page(2);
        changed.i32(physics::RPMS, 7777).i32(physics::GEAR, i32::MIN);
        blocks.set(PHYSICS, changed.build());
        assert_eq!(listener.native_channel("rpm"), Some(LegacyValue::Str("7777".into())));
        assert_eq!(listener.native_channel("gear"), Some(LegacyValue::Str("-2147483649".into())));
        let mut graphics = Image::new(GRAPHICS.size);
        graphics.i32(graphics::STATUS, 1);
        blocks.set(GRAPHICS, graphics.build());
        for name in ["rpm", "car_name", "max_rpm", "unknown"] {
            assert_eq!(listener.native_channel(name), Some(LegacyValue::Int(0)), "{name}");
        }
    }

    #[test]
    fn native_failures_are_local_and_fuel_zero_kinds_differ() {
        let blocks = pages(1, 2, 0, 0.0);
        let ac = AssettoCorsa::with_opener(blocks.clone(), AcGame::Competizione, true);
        assert_eq!(ac.native_channel("fuel_level"), Some(LegacyValue::Int(0)));
        let mut statics = Image::new(STATIC.size);
        statics.f32(statics::MAX_FUEL, 60.0);
        blocks.set(STATIC, statics.build());
        let mut physics = physics_page(2);
        physics
            .f32(physics::FUEL, 0.0)
            .f32(physics::SPEED_KMH, f32::NAN)
            .f32(physics::ABS, f32::INFINITY)
            .f32(physics::TURBO_BOOST, f32::NEG_INFINITY);
        blocks.set(PHYSICS, physics.build());
        assert_eq!(ac.native_channel("fuel_level"), Some(LegacyValue::Float(0.0)));
        assert_eq!(ac.native_channel("car_name"), Some(LegacyValue::Str(String::new())));
        for name in ["speed", "abs_lock", "turbo_boost_pressure"] {
            assert_eq!(ac.read_native(name), NativeRead::Raised, "{name}");
        }
        assert_eq!(ac.read_native("rpm"), NativeRead::Value(LegacyValue::Str("6543".into())));
        let rally = AssettoCorsa::with_opener(pages(1, 0, 0, 50.0), AcGame::Rally, true);
        assert_eq!(rally.native_channel("max_rpm"), Some(LegacyValue::Int(10000)));
        assert_eq!(rally.native_channel("rpm"), Some(LegacyValue::Str("6543".into())));
    }

    /// SPageFilePhysics / SPageFileGraphic / SPageFileStatic (4-byte packing): every offset the
    /// listener reads.
    #[test]
    fn layout_offsets() {
        let physics = [
            (physics::PACKET_ID, 0),
            (physics::FUEL, 12),
            (physics::GEAR, 16),
            (physics::RPMS, 20),
            (physics::STEER_ANGLE, 24),
            (physics::SPEED_KMH, 28),
            (physics::ACC_G, 44),
            (physics::WHEEL_SLIP, 56),
            (physics::SUSPENSION_TRAVEL, 184),
            (physics::HEADING, 208),
            (physics::PITCH, 212),
            (physics::ROLL, 216),
            (physics::ABS, 252),
            (physics::TURBO_BOOST, 276),
            (physics::LOCAL_ANGULAR_VEL, 296),
            (physics::LOCAL_VELOCITY, 568),
        ];
        for (offset, expected) in physics {
            assert_eq!(offset, expected);
        }
        assert_eq!((graphics::STATUS, graphics::FLAG_TYPE), (4, 1224));
        assert_eq!((statics::CAR_MODEL, statics::TRACK, statics::MAX_RPM, statics::MAX_FUEL), (68, 134, 412, 416));
        assert_eq!((PHYSICS.size, GRAPHICS.size, STATIC.size), (580, 1284, 452));
    }

    fn physics_page(packet_id: i32) -> Image {
        let mut p = Image::new(PHYSICS.size);
        p.i32(physics::PACKET_ID, packet_id)
            .f32(physics::FUEL, 30.0)
            .i32(physics::GEAR, 3)
            .i32(physics::RPMS, 6543)
            .f32(physics::STEER_ANGLE, 0.25)
            .f32(physics::SPEED_KMH, 9.0)
            .f32(physics::ACC_G, 0.5)
            .f32(physics::ACC_G + 4, 1.5)
            .f32(physics::ACC_G + 8, -0.25)
            .f32(physics::WHEEL_SLIP, 0.1)
            .f32(physics::WHEEL_SLIP + 4, 0.2)
            .f32(physics::WHEEL_SLIP + 8, 0.3)
            .f32(physics::WHEEL_SLIP + 12, 0.4)
            .f32(physics::SUSPENSION_TRAVEL, 0.03)
            .f32(physics::SUSPENSION_TRAVEL + 12, 0.06)
            .f32(physics::HEADING, std::f32::consts::FRAC_PI_2)
            .f32(physics::PITCH, 0.1)
            .f32(physics::ROLL, 0.2)
            .f32(physics::ABS, 0.9)
            .f32(physics::TURBO_BOOST, 1.7)
            .f32(physics::LOCAL_ANGULAR_VEL, 0.7)
            .f32(physics::LOCAL_ANGULAR_VEL + 4, 9.0)
            .f32(physics::LOCAL_VELOCITY, 2.0)
            .f32(physics::LOCAL_VELOCITY + 4, -1.0)
            .f32(physics::LOCAL_VELOCITY + 8, 33.0);
        p
    }

    fn pages(packet_id: i32, status: i32, flag_type: i32, max_fuel: f32) -> FakeBlocks {
        let pages = FakeBlocks::default();
        pages.set(PHYSICS, physics_page(packet_id).build());
        let mut g = Image::new(GRAPHICS.size);
        g.i32(graphics::STATUS, status).i32(graphics::FLAG_TYPE, flag_type);
        pages.set(GRAPHICS, g.build());
        let mut s = Image::new(STATIC.size);
        s.bytes(statics::CAR_MODEL, &utf16le("ks_mazda_mx5_cup"))
            .bytes(statics::TRACK, &utf16le("magione"))
            .i32(statics::MAX_RPM, 7500)
            .f32(statics::MAX_FUEL, max_fuel);
        pages.set(STATIC, s.build());
        pages
    }

    /// Liveness: false on the first call, true while packetId changes, then 49 more unchanged calls.
    #[test]
    fn check_status_tolerates_49_unchanged_packets() {
        let blocks = pages(10, 2, 0, 50.0);
        let mut ac = AssettoCorsa::with_opener(blocks.clone(), AcGame::AssettoCorsa, true);
        assert!(!ac.is_running().unwrap());
        for _ in 0..49 {
            assert!(ac.is_running().unwrap());
        }
        assert!(!ac.is_running().unwrap());
        blocks.set(PHYSICS, physics_page(11).build());
        assert!(ac.is_running().unwrap());
    }

    /// The three pages are created (zero-filled where missing) and kept from
    /// the moment the listener exists, whatever runs.
    #[test]
    fn pages_are_created_and_kept_at_import() {
        let blocks = FakeBlocks::default();
        let mut ac = AssettoCorsa::with_opener(blocks.clone(), AcGame::AssettoCorsa, true);
        for spec in MAPPINGS {
            assert_eq!(blocks.held_count(spec.name), 1, "{}", spec.name);
        }
        assert!(!ac.is_running().unwrap());
        let mut t = Telemetry::off();
        assert!(ac.poll(&mut t).unwrap());
        assert_eq!(t.rpm, 0.0);
        drop(ac);
        assert_eq!(blocks.held_count(PHYSICS.name), 0);
    }

    /// Liveness: the first observed packet never counts, even when it differs from zero.
    #[test]
    fn check_status_first_call_is_false() {
        let blocks = pages(1, 2, 0, 50.0);
        let mut ac = AssettoCorsa::with_opener(blocks.clone(), AcGame::Competizione, true);
        assert!(!ac.is_running().unwrap());
        blocks.set(PHYSICS, physics_page(2).build());
        assert!(ac.is_running().unwrap());
    }

    /// Channels: every mapped channel with its scale factor and sign.
    #[test]
    fn get_channel_mapping() {
        let mut ac = AssettoCorsa::with_opener(pages(1, 2, 7, 60.0), AcGame::AssettoCorsa, true);
        let mut t = Telemetry::off();
        t.tyre_temperature_lf = 80.0;
        assert!(ac.poll(&mut t).unwrap());
        assert_eq!(t.rpm, 6543.0);
        assert_eq!(t.max_rpm, 7500.0);
        assert_eq!(t.gear, 2.0);
        assert_eq!(t.speed, 3.0); // round(9 * 0.277778) = round(2.500002)
        assert_eq!(t.vehicle, "ks_mazda_mx5_cup");
        assert_eq!(t.location, "magione");
        assert_eq!(t.steering_wheel_angle, -0.25);
        assert_eq!(ac.motion_rates().yaw_rate, Some(f64::from(0.7f32)));
        assert_eq!(t.wheel_slip_rr, f64::from(0.4f32));
        assert_eq!(t.suspension_travel_lf, f64::from(0.03f32) / 3.0);
        assert_eq!(t.suspension_travel_rr, f64::from(0.06f32) / 3.0);
        assert_eq!(t.vertical_acceleration, 3.0);
        assert_eq!(t.longitudinal_acceleration, -2.5);
        assert_eq!(t.lateral_acceleration, 4.0);
        assert_eq!(t.vertical_velocity, -1.0);
        assert_eq!(t.lateral_velocity, -2.0);
        assert_eq!(t.longitudinal_velocity, 33.0);
        assert!((t.yaw - 90.0).abs() < 1e-5);
        assert_eq!(t.roll, -rad_to_deg(f64::from(0.2f32)));
        assert_eq!(t.pitch, rad_to_deg(f64::from(0.1f32)));
        assert_eq!(t.abs_lock, 0.0);
        assert_eq!(t.turbo_boost_pressure, 1.0);
        assert_eq!(t.fuel_level, 50.0);
        assert_eq!(t.flag, 12.0);
        assert_eq!(t.tyre_temperature_lf, 0.0); // unimplemented channel answers 0
    }

    /// Channels: speed is round(speedKmh * 0.277778); a NaN speed fails and the previous speed is kept.
    #[test]
    fn speed_rounding_and_nan() {
        for (kmh, expected) in [(5.0, 1.0), (f32::NAN, 7.0)] {
            let p = pages(1, 2, 0, 50.0);
            let mut physics = physics_page(1);
            physics.f32(physics::SPEED_KMH, kmh);
            p.set(PHYSICS, physics.build());
            let mut ac = AssettoCorsa::with_opener(p, AcGame::AssettoCorsa, true);
            let mut t = Telemetry::off();
            t.speed = 7.0;
            ac.poll(&mut t).unwrap();
            assert_eq!(t.speed, expected, "speedKmh {kmh}");
        }
    }

    /// Channels: fuel_level division by a zero maxFuel answers 0.
    #[test]
    fn fuel_level_zero_max_fuel() {
        let mut ac = AssettoCorsa::with_opener(pages(1, 2, 0, 0.0), AcGame::AssettoCorsa, true);
        let mut t = Telemetry::off();
        ac.poll(&mut t).unwrap();
        assert_eq!(t.fuel_level, 0.0);
    }

    /// Channels: flagType mapping, including the unmapped penalty flag.
    #[test]
    fn flag_mapping() {
        for (flag_type, expected) in [(0, 0.0), (1, 2.0), (2, 6.0), (3, 10.0), (4, 3.0), (5, 11.0), (6, 0.0), (7, 12.0)]
        {
            assert_eq!(kunos_flag(flag_type), expected, "flagType {flag_type}");
        }
    }

    /// Pause gate: AC and ACC answer 0 everywhere outside AC_LIVE when pausing is enabled.
    #[test]
    fn pause_gate_for_ac_and_acc() {
        for game in [AcGame::AssettoCorsa, AcGame::Competizione] {
            let mut ac = AssettoCorsa::with_opener(pages(1, 3, 1, 50.0), game, true);
            let mut t = Telemetry::off();
            t.rpm = 3000.0;
            ac.poll(&mut t).unwrap();
            assert_eq!(t.rpm, 0.0);
            assert_eq!(t.vehicle, ZERO_NAME);
            assert_eq!(ac.motion_rates().yaw_rate, Some(0.0));
        }
        let mut ac = AssettoCorsa::with_opener(pages(1, 3, 1, 50.0), AcGame::AssettoCorsa, false);
        let mut t = Telemetry::off();
        ac.poll(&mut t).unwrap();
        assert_eq!(t.rpm, 6543.0);
    }

    /// Channels: Rally is never paused, gets max_rpm 10000 and the negated roll.
    #[test]
    fn rally_specifics() {
        let mut ac = AssettoCorsa::with_opener(pages(1, 0, 0, 50.0), AcGame::Rally, true);
        let mut t = Telemetry::off();
        ac.poll(&mut t).unwrap();
        assert_eq!(t.rpm, 6543.0);
        assert_eq!(t.max_rpm, 10000.0);
        assert_eq!(t.roll, -rad_to_deg(f64::from(0.2f32)));
    }

    /// Liveness with no game: the pages the listener created read packetId 0, which the tolerance counts like any
    /// unchanged packet (as when a game is detected before it has written the pages).
    #[test]
    fn created_pages_follow_the_packet_tolerance() {
        let mut ac = AssettoCorsa::with_opener(FakeBlocks::default(), AcGame::AssettoCorsa, true);
        assert!(!ac.is_running().unwrap());
        for _ in 0..49 {
            assert!(ac.is_running().unwrap());
        }
        assert!(!ac.is_running().unwrap());
    }
}
