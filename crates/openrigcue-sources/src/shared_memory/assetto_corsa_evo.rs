//! Assetto Corsa EVO (listener `acevo`): the pages `acevo_pmf_physics`, `acevo_pmf_graphics`, `acevo_pmf_static`.
//!
//! The page layouts use 4-byte packing, with nested blocks padded to fixed sizes. The pages are opened read-only
//! and copied on every liveness test; channel reads use that copy.

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::answers::{write_all_zero, zero_generic_channels, MotionRates};
use super::assetto_corsa::{
    native_fuel_level, native_kunos_physics, physics, uses_kunos_physics, write_kunos_physics, STATUS_LIVE,
};
use super::block_reader::{copy_block, BlockOpener, Fields, MappingSpec};
use super::liveness::PacketLiveness;
use super::packed::{decode_utf8_ignoring_errors, ArrayFields};
use super::pages::LiveMirrors;
use super::python_numeric::{percent_or_zero, rad_to_deg};
use crate::{NativeRead, Source, SourceError};

/// `SPageFilePhysics` of EVO (800 bytes).
pub const PHYSICS: MappingSpec = MappingSpec { name: "acevo_pmf_physics", size: 800 };
/// `SPageFileGraphicEvo` (3940 bytes).
pub const GRAPHICS: MappingSpec = MappingSpec { name: "acevo_pmf_graphics", size: 3940 };
/// `SPageFileStaticEvo` (208 bytes).
pub const STATIC: MappingSpec = MappingSpec { name: "acevo_pmf_static", size: 208 };
/// Every mapping this listener reads.
pub const MAPPINGS: &[MappingSpec] = &[PHYSICS, GRAPHICS, STATIC];

/// `physics.currentMaxRpm` (an EVO addition after the AC physics layout).
const CURRENT_MAX_RPM: usize = 588;

mod graphics {
    pub(super) const STATUS: usize = 4;
    pub(super) const FLAG: usize = 2404;
    pub(super) const CAR_MODEL: usize = 3086;
    pub(super) const MAX_FUEL: usize = 3928;
}

mod statics {
    pub(super) const TRACK: usize = 136;
}

/// Length of the character name fields read (`car_model`, `track`).
const NAME_BYTES: usize = 33;

/// The three pages copied by the last successful liveness test.
struct EvoPages {
    physics: Vec<u8>,
    graphics: Vec<u8>,
    statics: Vec<u8>,
}

/// Listener `acevo`.
pub struct AssettoCorsaEvo<O: BlockOpener = LiveMirrors> {
    opener: O,
    pause_when_not_playing: bool,
    pause_reader: Option<super::NativePauseReader>,
    game_reader: Option<super::NativeGameReader>,
    info: Option<EvoPages>,
    liveness: PacketLiveness,
    rates: MotionRates,
}

impl AssettoCorsaEvo<LiveMirrors> {
    /// Listener reading the bridge mirrors. `pause_when_not_playing` is `APP/pause_telemetry_when_not_playing`.
    pub fn new(pause_when_not_playing: bool) -> Self {
        AssettoCorsaEvo::with_opener(LiveMirrors, pause_when_not_playing)
    }
}

impl<O: BlockOpener> AssettoCorsaEvo<O> {
    /// Listener reading the mappings `opener` opens.
    pub fn with_opener(opener: O, pause_when_not_playing: bool) -> Self {
        AssettoCorsaEvo {
            opener,
            pause_when_not_playing,
            pause_reader: None,
            game_reader: None,
            info: None,
            liveness: PacketLiveness::default(),
            rates: MotionRates::ZERO,
        }
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

    /// A fresh copy of the three pages, or `None` while any is missing.
    fn copy_pages(&mut self) -> Option<EvoPages> {
        let physics = copy_block(&mut self.opener, PHYSICS).ok()?;
        let graphics = copy_block(&mut self.opener, GRAPHICS).ok()?;
        let statics = copy_block(&mut self.opener, STATIC).ok()?;
        Some(EvoPages { physics, graphics, statics })
    }

    fn native_answer(&self, name: &str) -> Option<LegacyValue> {
        let Some(info) = &self.info else { return Some(LegacyValue::Int(0)) };
        let (physics, graphics, statics) = (Fields(&info.physics), Fields(&info.graphics), Fields(&info.statics));
        if self.native_game() == "acevo"
            && super::native_pause(&self.pause_reader, self.pause_when_not_playing)?
            && graphics.i32(graphics::STATUS) != STATUS_LIVE
        {
            return Some(LegacyValue::Int(0));
        }
        if uses_kunos_physics(name) {
            return native_kunos_physics(&physics, name, 4.0, false);
        }
        Some(match name {
            "max_rpm" => {
                if self.native_game() == "acevo" {
                    LegacyValue::Str(physics.i32(CURRENT_MAX_RPM).to_string())
                } else {
                    LegacyValue::Int(10000)
                }
            }
            "car_name" => {
                LegacyValue::Str(decode_utf8_ignoring_errors(graphics.c_chars(graphics::CAR_MODEL, NAME_BYTES)))
            }
            "track_loc" => LegacyValue::Str(decode_utf8_ignoring_errors(statics.c_chars(statics::TRACK, NAME_BYTES))),
            "fuel_level" => native_fuel_level(physics.f32(physics::FUEL), graphics.f32(graphics::MAX_FUEL)),
            "flag" => LegacyValue::Int(evo_flag(graphics.i32(graphics::FLAG)) as i64),
            _ => LegacyValue::Int(0),
        })
    }

    fn native_game(&self) -> String {
        self.game_reader.as_ref().map_or_else(|| "acevo".to_owned(), |read| read())
    }
}

impl<O: BlockOpener> Source for AssettoCorsaEvo<O> {
    fn id(&self) -> &'static str {
        "acevo"
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

    /// The liveness test: when there is no copy yet, a first copy of the
    /// pages (a failure is "not running"), then a refresh on every call (a second copy on the connecting call; a
    /// failure drops the copy and forgets the previous packet id), then the packet-counter test of `ac`.
    fn is_running(&mut self) -> Result<bool, SourceError> {
        if self.info.is_none() {
            let Some(pages) = self.copy_pages() else { return Ok(false) };
            self.info = Some(pages);
        }
        match self.copy_pages() {
            Some(pages) => {
                let packet_id = Fields(&pages.physics).i32(physics::PACKET_ID);
                self.info = Some(pages);
                Ok(self.liveness.observe(packet_id))
            }
            None => {
                self.info = None;
                self.liveness.forget_packet();
                Ok(false)
            }
        }
    }

    /// Channel reads over the copy taken by the last liveness test.
    fn poll(&mut self, t: &mut Telemetry) -> Result<bool, SourceError> {
        let Some(info) = &self.info else {
            write_all_zero(t);
            self.rates = MotionRates::ZERO;
            return Ok(true);
        };
        let (physics, graphics, statics) = (Fields(&info.physics), Fields(&info.graphics), Fields(&info.statics));
        if self.pause_when_not_playing && graphics.i32(graphics::STATUS) != STATUS_LIVE {
            write_all_zero(t);
            self.rates = MotionRates::ZERO;
            return Ok(true);
        }
        zero_generic_channels(t);
        write_kunos_physics(&physics, t);
        self.rates = MotionRates::yaw_only(physics.f32_at(physics::LOCAL_ANGULAR_VEL, 0));
        t.max_rpm = f64::from(physics.i32(CURRENT_MAX_RPM));
        t.vertical_acceleration = physics.f32_at(physics::ACC_G, 1) * 4.0;
        t.roll = rad_to_deg(physics.f32(physics::ROLL));
        t.location = decode_utf8_ignoring_errors(statics.c_chars(statics::TRACK, NAME_BYTES));
        t.vehicle = decode_utf8_ignoring_errors(graphics.c_chars(graphics::CAR_MODEL, NAME_BYTES));
        t.fuel_level = percent_or_zero(physics.f32(physics::FUEL), graphics.f32(graphics::MAX_FUEL));
        t.flag = evo_flag(graphics.i32(graphics::FLAG));
        Ok(true)
    }
}

/// The `flag` channel: EVO flag type values to the normalized flag.
fn evo_flag(flag: i32) -> f64 {
    match flag {
        4 => 2.0,  // AC_BLUE_FLAG
        2 => 12.0, // AC_GREEN_FLAG
        8 => 11.0, // AC_CHECKERED_FLAG
        5 => 6.0,  // AC_YELLOW_FLAG
        1 => 3.0,  // AC_WHITE_FLAG
        6 => 10.0, // AC_BLACK_FLAG
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_kunos_evo_environment_keeps_info_none_and_non_evo_guards() {
        use std::sync::{Arc, Mutex};
        let mut evo = AssettoCorsaEvo::with_opener(pages(1, 0, 0), true);
        let game = Arc::new(Mutex::new(String::from("none")));
        evo.set_native_readers(
            {
                let game = game.clone();
                Arc::new(move || game.lock().unwrap().clone())
            },
            Arc::new(|| None),
        );
        assert_eq!(
            evo.read_native("max_rpm"),
            NativeRead::Value(LegacyValue::Int(0)),
            "info None precedes all environment reads"
        );
        evo.is_running().unwrap();
        assert_eq!(evo.read_native("rpm"), NativeRead::Value(LegacyValue::from("900")), "non-Evo skips pause read");
        assert_eq!(evo.read_native("max_rpm"), NativeRead::Value(LegacyValue::Int(10000)));
        *game.lock().unwrap() = "acevo".into();
        assert_eq!(evo.read_native("unknown"), NativeRead::Raised);
    }

    #[test]
    fn native_kunos_evo_second_game_read_and_empty_info_order() {
        use std::collections::VecDeque;
        use std::sync::{Arc, Mutex};
        let mut evo = AssettoCorsaEvo::with_opener(pages(1, 2, 0), false);
        let sequence = Arc::new(Mutex::new(VecDeque::from(["acevo", "none", "none", "acevo"])));
        let reads = Arc::new(Mutex::new(Vec::new()));
        evo.set_native_readers(
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
        assert_eq!(evo.read_native("unknown"), NativeRead::Value(LegacyValue::Int(0)));
        assert!(reads.lock().unwrap().is_empty());
        evo.is_running().unwrap();
        assert_eq!(evo.read_native("max_rpm"), NativeRead::Value(LegacyValue::Int(10000)));
        assert_eq!(evo.read_native("max_rpm"), NativeRead::Value(LegacyValue::from("8800")));
        assert_eq!(*reads.lock().unwrap(), ["game", "pause", "game", "game", "game"]);
    }
    use crate::shared_memory::answers::ZERO_NAME;
    use crate::shared_memory::block_reader::fake::BlockBuilder as Image;
    use crate::shared_memory::block_reader::fake::FakeBlocks;

    #[test]
    fn native_missing_info_returns_integer_zero() {
        let evo = AssettoCorsaEvo::with_opener(pages(1, 2, 0), true);
        assert_eq!(evo.native_channel("rpm"), Some(openrigcue_model::native_value::LegacyValue::Int(0)));
    }

    #[test]
    fn native_getters_keep_check_status_copy_and_original_types() {
        use crate::derive::{Listener, SourceListener};
        let blocks = pages(1, 2, 2);
        let evo = AssettoCorsaEvo::with_opener(blocks.clone(), true);
        let mut listener = SourceListener::new(evo);
        assert!(!listener.is_running());
        listener.receive();
        for (name, expected) in [
            ("rpm", LegacyValue::Str("900".into())),
            ("max_rpm", LegacyValue::Str("8800".into())),
            ("gear", LegacyValue::Str("-1".into())),
            ("speed", LegacyValue::Str("28".into())),
            ("car_name", LegacyValue::Str("bmw_m4_gt3".into())),
            ("track_loc", LegacyValue::Str("imola".into())),
            ("flag", LegacyValue::Int(12)),
            ("vertical_acceleration", LegacyValue::Float(2.0)),
            ("roll", LegacyValue::Float(rad_to_deg(0.5))),
            ("fuel_level", LegacyValue::Float(25.0)),
            ("unknown", LegacyValue::Int(0)),
        ] {
            assert_eq!(listener.native_channel(name), Some(expected), "{name}");
        }
        let mut changed = physics_page(2);
        changed[physics::RPMS..physics::RPMS + 4].copy_from_slice(&7777i32.to_le_bytes());
        blocks.set(PHYSICS, changed);
        listener.receive();
        assert_eq!(listener.native_channel("rpm"), Some(LegacyValue::Str("900".into())));
        assert!(listener.is_running());
        assert_eq!(listener.native_channel("rpm"), Some(LegacyValue::Str("7777".into())));
        let mut graphics = Image::new(GRAPHICS.size);
        graphics.i32(graphics::STATUS, 1);
        blocks.set(GRAPHICS, graphics.build());
        assert_eq!(listener.native_channel("rpm"), Some(LegacyValue::Str("7777".into())));
        listener.is_running();
        assert_eq!(listener.native_channel("rpm"), Some(LegacyValue::Int(0)));
    }

    #[test]
    fn native_receive_leaves_projection_and_cached_pages_alone() {
        let mut evo = AssettoCorsaEvo::with_opener(pages(1, 2, 0), true);
        let mut t = Telemetry { speed: 999.0, ..Telemetry::off() };
        evo.receive_native(&mut t).unwrap();
        assert_eq!(t.speed, 999.0);
        assert_eq!(evo.native_channel("rpm"), Some(LegacyValue::Int(0)));
        evo.is_running().unwrap();
        evo.opener.remove(PHYSICS.name);
        evo.receive_native(&mut t).unwrap();
        assert_eq!(evo.native_channel("rpm"), Some(LegacyValue::Str("900".into())));
        evo.is_running().unwrap();
        assert_eq!(evo.native_channel("rpm"), Some(LegacyValue::Int(0)));
    }

    /// SPageFilePhysics / SPageFileGraphicEvo / SPageFileStaticEvo (4-byte packing, padded
    /// blocks): the EVO-only offsets the listener reads (the shared physics prefix is pinned by
    /// `assetto_corsa::tests::layout_offsets`).
    #[test]
    fn layout_offsets() {
        assert_eq!(CURRENT_MAX_RPM, 588);
        assert_eq!((graphics::STATUS, graphics::FLAG, graphics::CAR_MODEL, graphics::MAX_FUEL), (4, 2404, 3086, 3928));
        assert_eq!(statics::TRACK, 136);
        assert_eq!((PHYSICS.size, GRAPHICS.size, STATIC.size), (800, 3940, 208));
    }

    fn physics_page(packet_id: i32) -> Vec<u8> {
        let mut p = Image::new(PHYSICS.size);
        p.i32(physics::PACKET_ID, packet_id)
            .f32(physics::FUEL, 15.0)
            .i32(physics::GEAR, 0)
            .i32(physics::RPMS, 900)
            .f32(physics::SPEED_KMH, 100.0)
            .f32(physics::ACC_G + 4, 0.5)
            .f32(physics::ROLL, 0.5)
            .f32(physics::LOCAL_ANGULAR_VEL, -0.3)
            .i32(CURRENT_MAX_RPM, 8800);
        p.build()
    }

    fn pages(packet_id: i32, status: i32, flag: i32) -> FakeBlocks {
        let pages = FakeBlocks::default();
        pages.set(PHYSICS, physics_page(packet_id));
        let mut g = Image::new(GRAPHICS.size);
        g.i32(graphics::STATUS, status)
            .i32(graphics::FLAG, flag)
            .bytes(graphics::CAR_MODEL, b"bmw_m4_gt3\xff")
            .f32(graphics::MAX_FUEL, 60.0);
        pages.set(GRAPHICS, g.build());
        let mut s = Image::new(STATIC.size);
        s.bytes(statics::TRACK, b"imola");
        pages.set(STATIC, s.build());
        pages
    }

    /// Liveness: absent pages are not running; the packet test then behaves like `ac`.
    #[test]
    fn check_status_lazy_connect_and_tolerance() {
        let mut evo = AssettoCorsaEvo::with_opener(FakeBlocks::default(), true);
        assert!(!evo.is_running().unwrap());
        evo.opener = pages(3, 2, 0);
        assert!(!evo.is_running().unwrap());
        assert!(evo.is_running().unwrap());
    }

    /// Liveness: the connecting call copies the pages twice (first copy, then refresh), every later
    /// call once.
    #[test]
    fn connecting_call_copies_twice() {
        let mut evo = AssettoCorsaEvo::with_opener(pages(3, 2, 0), true);
        evo.is_running().unwrap();
        assert_eq!(evo.opener.open_count(), 6);
        evo.is_running().unwrap();
        assert_eq!(evo.opener.open_count(), 9);
    }

    /// Liveness: a refresh failure clears the copy and the previous packet id, so the next success is false again.
    #[test]
    fn refresh_failure_forgets_previous_packet() {
        let mut evo = AssettoCorsaEvo::with_opener(pages(3, 2, 0), true);
        evo.is_running().unwrap();
        evo.opener.set(PHYSICS, physics_page(4));
        assert!(evo.is_running().unwrap());
        evo.opener.remove(GRAPHICS.name);
        assert!(!evo.is_running().unwrap());
        evo.opener = pages(5, 2, 0);
        assert!(!evo.is_running().unwrap());
        evo.opener.set(PHYSICS, physics_page(6));
        assert!(evo.is_running().unwrap());
    }

    /// Channels: EVO differences from AC (currentMaxRpm, accG[1]*4, roll not negated, byte strings, graphics
    /// max_fuel and the EVO flag values).
    #[test]
    fn get_channel_mapping() {
        let mut evo = AssettoCorsaEvo::with_opener(pages(3, 2, 2), true);
        evo.is_running().unwrap();
        let mut t = Telemetry::off();
        evo.poll(&mut t).unwrap();
        assert_eq!(t.max_rpm, 8800.0);
        assert_eq!(t.rpm, 900.0);
        assert_eq!(t.gear, -1.0);
        assert_eq!(t.speed, 28.0);
        assert_eq!(t.vertical_acceleration, 2.0);
        assert_eq!(t.roll, rad_to_deg(0.5));
        assert_eq!(t.vehicle, "bmw_m4_gt3");
        assert_eq!(t.location, "imola");
        assert_eq!(t.fuel_level, 25.0);
        assert_eq!(t.flag, 12.0);
        assert_eq!(evo.motion_rates().yaw_rate, Some(f64::from(-0.3f32)));
    }

    /// Channels: EVO flag values, the red and orange flags are not mapped.
    #[test]
    fn flag_mapping() {
        for (flag, expected) in [
            (0, 0.0),
            (1, 3.0),
            (2, 12.0),
            (3, 0.0),
            (4, 2.0),
            (5, 6.0),
            (6, 10.0),
            (7, 0.0),
            (8, 11.0),
            (9, 0.0),
            (10, 0.0),
        ] {
            assert_eq!(evo_flag(flag), expected, "flag {flag}");
        }
    }

    /// Pause gate: outside status 2 every channel answers 0 when pausing is enabled.
    #[test]
    fn pause_gate() {
        let mut evo = AssettoCorsaEvo::with_opener(pages(3, 1, 0), true);
        evo.is_running().unwrap();
        let mut t = Telemetry::off();
        evo.poll(&mut t).unwrap();
        assert_eq!(t.rpm, 0.0);
        assert_eq!(t.location, ZERO_NAME);
        evo.set_pause_when_not_playing(false);
        evo.poll(&mut t).unwrap();
        assert_eq!(t.rpm, 900.0);
    }

    /// Without a copy (no successful liveness test yet) every channel answers 0.
    #[test]
    fn no_info_answers_zero() {
        let mut evo = AssettoCorsaEvo::with_opener(pages(3, 2, 0), true);
        let mut t = Telemetry::off();
        t.rpm = 5.0;
        evo.poll(&mut t).unwrap();
        assert_eq!(t.rpm, 0.0);
    }
}
