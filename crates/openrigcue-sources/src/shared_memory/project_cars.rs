//! Project CARS (listener `projectcars`): the `$pcars$` shared memory.
//!
//! The 8000-byte block is mapped create-or-open and viewed live as a
//! `GameInstance` (7316-byte structure, `SHARED_MEMORY_VERSION` 5 required).

use std::sync::{Mutex, PoisonError};

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::answers::{write_all_raised, write_all_zero, zero_generic_channels, MotionRates, ZERO_NAME};
use super::block_reader::{BlockOpener, BlockReader, Fields, MappingSpec};
use super::pages::LiveMirrors;
use super::pcars::{
    native_channels, rates_without_game, write_common_channels, GameInstance, PcarsKind, GAME_INGAME_PLAYING, LF, LR,
    PCARS1, RF, RR,
};
use super::python_numeric::decode_utf8;
use crate::{NativeRead, Source, SourceError};

/// The `$pcars$` mapping: 8000 bytes (the structure uses the first 7316 bytes).
pub const MAPPING: MappingSpec = MappingSpec { name: "$pcars$", size: 8000 };
/// Every mapping this listener reads.
pub const MAPPINGS: &[MappingSpec] = &[MAPPING];

/// The required `SHARED_MEMORY_VERSION`.
const SHARED_MEMORY_VERSION: u32 = 5;

struct LiveGame {
    view: Box<dyn BlockReader>,
    seen: Vec<u8>,
}

/// Listener `projectcars`.
pub struct ProjectCars<O: BlockOpener = LiveMirrors> {
    opener: O,
    pause_when_not_playing: bool,
    pause_reader: Option<super::NativePauseReader>,
    /// The live view (it keeps its mapping alive), with the bytes it showed when
    /// it was made; `None` once the presence sum dropped it.
    game: Mutex<Option<LiveGame>>,
    rates: MotionRates,
}

impl ProjectCars<LiveMirrors> {
    /// Listener reading the bridge mirror. `pause_when_not_playing` is `APP/pause_telemetry_when_not_playing`.
    pub fn new(pause_when_not_playing: bool) -> Self {
        ProjectCars::with_opener(LiveMirrors, pause_when_not_playing)
    }
}

impl<O: BlockOpener> ProjectCars<O> {
    /// Listener reading the mappings `opener` opens.
    pub fn with_opener(opener: O, pause_when_not_playing: bool) -> Self {
        ProjectCars {
            opener,
            pause_when_not_playing,
            pause_reader: None,
            game: Mutex::new(None),
            rates: MotionRates::ZERO,
        }
    }

    /// Updates `APP/pause_telemetry_when_not_playing`.
    pub fn set_pause_when_not_playing(&mut self, pause: bool) {
        self.pause_when_not_playing = pause;
    }

    pub fn set_pause_reader(&mut self, reader: super::NativePauseReader) {
        self.pause_reader = Some(reader);
    }

    /// The rate channels answered by the last [`Source::poll`]: Project CARS implements none, so they fall through
    /// to `0`, except while there is no view with pausing enabled (the pause gate fails).
    pub fn motion_rates(&self) -> MotionRates {
        self.rates
    }

    fn native_answer(&self, name: &str) -> Option<LegacyValue> {
        let pause = super::native_pause(&self.pause_reader, self.pause_when_not_playing)?;
        let mut held = self.game.lock().unwrap_or_else(PoisonError::into_inner);
        let mut read = || {
            held.as_mut()
                .map(|LiveGame { view, seen }| view.read().map(<[u8]>::to_vec).unwrap_or_else(|_| seen.clone()))
        };
        if pause && (GameInstance { fields: Fields(&read()?), layout: &PCARS1 }).game_state() != GAME_INGAME_PLAYING {
            return Some(LegacyValue::Int(0));
        }
        if name == "flag" {
            let colour = (GameInstance { fields: Fields(&read()?), layout: &PCARS1 }).highest_flag_colour();
            return Some(LegacyValue::Int(if matches!(colour, 3 | 4) {
                3
            } else {
                // The test uses one read of the colour; the fallback reads the field again.
                i64::from((GameInstance { fields: Fields(&read()?), layout: &PCARS1 }).highest_flag_colour())
            }));
        }
        // The GameInstance is a live view: the channel expression reads after the pause check.
        let live = read();
        let game = live.as_ref().map(|bytes| GameInstance { fields: Fields(bytes), layout: &PCARS1 });
        native_channels(game.as_ref(), PcarsKind::ProjectCars, false, name)
    }
}

impl<O: BlockOpener> Source for ProjectCars<O> {
    fn id(&self) -> &'static str {
        "projectcars"
    }

    /// The liveness test: a new create-or-open mapping viewed in place, which fails when
    /// `mVersion != 5` (then the new mapping is dropped, the previous view is kept and the call is false);
    /// otherwise the new view replaces the previous one, and is dropped when `mSpeed + mRpm + mMaxRPM + mGear == 0`.
    fn is_running(&mut self) -> Result<bool, SourceError> {
        let Ok(mut live) = self.opener.create_held(MAPPING) else { return Ok(false) };
        let Ok(bytes) = live.read().map(<[u8]>::to_vec) else { return Ok(false) };
        let game = GameInstance { fields: Fields(&bytes), layout: &PCARS1 };
        if game.version() != SHARED_MEMORY_VERSION {
            return Ok(false);
        }
        // The version is validated before the view is assigned. The following presence expression reads the live
        // fields one at a time; a failed read retains this newly assigned view.
        let held = self.game.get_mut().unwrap_or_else(PoisonError::into_inner);
        *held = Some(LiveGame { view: live, seen: bytes });
        let view = &mut held.as_mut().expect("just assigned").view;
        let read = |view: &mut Box<dyn BlockReader>, offset| view.read_range(offset, 4).ok();
        let Some(speed) = read(view, PCARS1.speed) else { return Ok(false) };
        let Some(rpm) = read(view, PCARS1.rpm) else { return Ok(false) };
        let Some(max_rpm) = read(view, PCARS1.max_rpm) else { return Ok(false) };
        let Some(gear) = read(view, PCARS1.gear) else { return Ok(false) };
        let present =
            Fields(&speed).f32(0) + Fields(&rpm).f32(0) + Fields(&max_rpm).f32(0) + f64::from(Fields(&gear).i32(0))
                != 0.0;
        if !present {
            *held = None;
        }
        Ok(present)
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

    /// Channel reads over the live view (the receive step does nothing).
    fn poll(&mut self, t: &mut Telemetry) -> Result<bool, SourceError> {
        self.rates = MotionRates::ZERO;
        let Some(LiveGame { view, seen }) = self.game.get_mut().unwrap_or_else(PoisonError::into_inner) else {
            // No view: with pausing enabled the pause gate's `mGameState` read fails for every channel.
            // Without it, every implemented channel fails except the names, which answer 0, and the
            // channels this listener does not implement fall through to 0 (only the suspension velocities differ
            // from their registry defaults).
            write_all_raised(t);
            self.rates = rates_without_game(self.pause_when_not_playing);
            if !self.pause_when_not_playing {
                t.vehicle = ZERO_NAME.to_owned();
                t.suspension_velocity_lf = 0.0;
                t.suspension_velocity_lr = 0.0;
                t.suspension_velocity_rf = 0.0;
                t.suspension_velocity_rr = 0.0;
            }
            return Ok(true);
        };
        let live = view.read().map(<[u8]>::to_vec).unwrap_or_else(|_| seen.clone());
        let game = GameInstance { fields: Fields(&live), layout: &PCARS1 };
        if self.pause_when_not_playing && game.game_state() != GAME_INGAME_PLAYING {
            write_all_zero(t);
            return Ok(true);
        }
        zero_generic_channels(t);
        write_common_channels(&game, t);
        let vertical = game.local_acceleration(1);
        t.vertical_acceleration = vertical / 10.0;
        // Project CARS has no suspension data: all four corners get the same pseudo travel.
        let pseudo_travel = vertical / 1000.0;
        t.suspension_travel_lf = pseudo_travel;
        t.suspension_travel_lr = pseudo_travel;
        t.suspension_travel_rf = pseudo_travel;
        t.suspension_travel_rr = pseudo_travel;
        t.wheel_slip_lf = game.tyre_slip_speed(LF);
        t.wheel_slip_rf = game.tyre_slip_speed(RF);
        t.wheel_slip_lr = game.tyre_slip_speed(LR);
        t.wheel_slip_rr = game.tyre_slip_speed(RR);
        // A name that cannot be decoded as UTF-8 answers 0.
        t.location = decode_utf8(game.track_location()).unwrap_or_else(|| ZERO_NAME.to_owned());
        t.vehicle = decode_utf8(game.car_name()).unwrap_or_else(|| ZERO_NAME.to_owned());
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_live_pause_project_cars_reads_once_before_game_state() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};
        let mut source = listener(SHARED_MEMORY_VERSION);
        assert!(source.is_running().unwrap());
        source.opener.set(MAPPING, sample(&PCARS1, MAPPING.size, SHARED_MEMORY_VERSION).u32(8, 0).build());
        let value = Arc::new(Mutex::new(None));
        let calls = Arc::new(AtomicUsize::new(0));
        source.set_pause_reader({
            let (value, calls) = (value.clone(), calls.clone());
            Arc::new(move || {
                calls.fetch_add(1, Ordering::SeqCst);
                value.lock().unwrap().clone()
            })
        });
        assert_eq!(source.read_native("unknown"), crate::NativeRead::Raised, "required setting before channel branch");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        *value.lock().unwrap() = Some(LegacyValue::from("no"));
        assert_eq!(source.read_native("rpm"), crate::NativeRead::Value(LegacyValue::from("6999")));
        *value.lock().unwrap() = Some(LegacyValue::from("Y"));
        assert_eq!(source.read_native("rpm"), crate::NativeRead::Value(LegacyValue::Int(0)));
        *value.lock().unwrap() = Some(LegacyValue::from("invalid"));
        assert_eq!(
            source.read_native("car_name"),
            crate::NativeRead::Raised,
            "required setting before local name handling"
        );
        *value.lock().unwrap() = Some(LegacyValue::None);
        assert_eq!(source.read_native("rpm"), crate::NativeRead::Raised);
        *value.lock().unwrap() = Some(LegacyValue::Bool(false));
        assert_eq!(source.read_native("rpm"), crate::NativeRead::Value(LegacyValue::from("6999")));
        assert_eq!(calls.load(Ordering::SeqCst), 6);
    }
    use crate::shared_memory::block_reader::fake::FakeBlocks;
    use crate::shared_memory::pcars::test_image::sample;
    use crate::shared_memory::python_numeric::rad_to_deg;

    fn listener(version: u32) -> ProjectCars<FakeBlocks> {
        let pages = FakeBlocks::default();
        pages.set(MAPPING, sample(&PCARS1, 7316, version).build());
        ProjectCars::with_opener(pages, true)
    }

    #[test]
    fn native_getter_reads_live_rpm_text() {
        let mut pc = listener(5);
        pc.is_running().unwrap();
        assert_eq!(pc.native_channel("rpm"), Some(openrigcue_model::native_value::LegacyValue::Str("6999".into())));
    }

    #[test]
    fn native_pause_check_and_channel_expression_read_the_live_view_separately() {
        struct NextRead {
            frames: std::collections::VecDeque<Vec<u8>>,
            current: Vec<u8>,
        }
        impl BlockReader for NextRead {
            fn read(&mut self) -> std::io::Result<&[u8]> {
                self.current = self.frames.pop_front().expect("unexpected live property read");
                Ok(&self.current)
            }
        }
        let first = sample(&PCARS1, MAPPING.size, 5).build();
        let newer = sample(&PCARS1, MAPPING.size, 5).f32(PCARS1.rpm, 8123.0).build();
        let mut source = listener(5);
        *source.game.get_mut().unwrap() = Some(LiveGame {
            view: Box::new(NextRead { frames: [first.clone(), newer].into(), current: Vec::new() }),
            seen: first,
        });
        assert_eq!(source.read_native("rpm"), NativeRead::Value(LegacyValue::from("8123")));
        assert!(source.game.get_mut().unwrap().is_some());
    }

    #[test]
    fn native_flag_rereads_only_the_non_special_return_value() {
        struct Flags(std::collections::VecDeque<Vec<u8>>, Vec<u8>);
        impl BlockReader for Flags {
            fn read(&mut self) -> std::io::Result<&[u8]> {
                self.1 = self.0.pop_front().expect("unplanned flag read");
                Ok(&self.1)
            }
        }
        let frame = |flag| sample(&PCARS1, MAPPING.size, 5).u32(PCARS1.highest_flag_colour, flag).build();
        let mut source = listener(5);
        source.set_pause_when_not_playing(false);
        *source.game.get_mut().unwrap() =
            Some(LiveGame { view: Box::new(Flags([frame(1), frame(4), frame(3)].into(), Vec::new())), seen: frame(0) });
        assert_eq!(source.read_native("flag"), NativeRead::Value(LegacyValue::Int(4)));
        assert_eq!(source.read_native("flag"), NativeRead::Value(LegacyValue::Int(3)));
    }

    #[test]
    fn check_status_reads_engine_fields_after_validating_the_live_view() {
        use std::sync::{Arc, Mutex};
        struct LiveSequence {
            initial: Vec<u8>,
            reads: Arc<Mutex<Vec<usize>>>,
            fail_at: Option<usize>,
        }
        impl BlockReader for LiveSequence {
            fn read(&mut self) -> std::io::Result<&[u8]> {
                Ok(&self.initial)
            }
            fn read_range(&mut self, offset: usize, len: usize) -> std::io::Result<Vec<u8>> {
                assert_eq!(len, 4);
                self.reads.lock().unwrap().push(offset);
                if self.fail_at == Some(offset) {
                    return Err(std::io::Error::other("scripted live field failure"));
                }
                let bytes = if offset == PCARS1.gear { (-1_i32).to_le_bytes() } else { 1_f32.to_le_bytes() };
                Ok(bytes.to_vec())
            }
        }
        struct Opener(Arc<Mutex<Vec<usize>>>, Option<usize>);
        impl BlockOpener for Opener {
            fn open(&mut self, _: MappingSpec) -> std::io::Result<Box<dyn BlockReader>> {
                unreachable!()
            }
            fn create_held(&mut self, _: MappingSpec) -> std::io::Result<Box<dyn BlockReader>> {
                let mut initial = vec![0; MAPPING.size];
                initial[PCARS1.version..PCARS1.version + 4].copy_from_slice(&5_u32.to_le_bytes());
                Ok(Box::new(LiveSequence { initial, reads: self.0.clone(), fail_at: self.1 }))
            }
        }
        let reads = Arc::new(Mutex::new(Vec::new()));
        let mut source = ProjectCars::with_opener(Opener(reads.clone(), None), false);
        assert!(source.is_running().unwrap(), "presence fields changed after the version read");
        assert_eq!(*reads.lock().unwrap(), [PCARS1.speed, PCARS1.rpm, PCARS1.max_rpm, PCARS1.gear]);

        reads.lock().unwrap().clear();
        source.opener.1 = Some(PCARS1.max_rpm);
        assert!(!source.is_running().unwrap());
        assert_eq!(*reads.lock().unwrap(), [PCARS1.speed, PCARS1.rpm, PCARS1.max_rpm]);
        assert!(source.game.get_mut().unwrap().is_some(), "valid view was assigned before the failed presence read");
    }

    #[test]
    fn native_live_updates_and_name_catches_reach_the_adapter() {
        use crate::derive::{Listener, SourceListener};
        let blocks = FakeBlocks::default();
        blocks.set(MAPPING, sample(&PCARS1, MAPPING.size, 5).build());
        let pc = ProjectCars::with_opener(blocks.clone(), true);
        let mut listener = SourceListener::new(pc);
        assert!(listener.is_running());
        listener.receive();
        assert_eq!(listener.native_channel("speed"), Some(LegacyValue::Str("41".into())));
        assert_eq!(listener.native_channel("max_rpm"), Some(LegacyValue::Str("8000".into())));
        assert_eq!(listener.native_channel("gear"), Some(LegacyValue::Str("3".into())));
        assert_eq!(listener.native_channel("rumble_pitch_lf"), Some(LegacyValue::Int(40)));
        assert_eq!(listener.native_channel("abs_lock"), Some(LegacyValue::Int(1)));
        assert_eq!(listener.native_channel("raw_flag"), Some(LegacyValue::Int(4)));
        assert_eq!(listener.native_channel("flag"), Some(LegacyValue::Int(3)));
        assert_eq!(listener.native_channel("vertical_acceleration"), Some(LegacyValue::Float(0.9)));
        assert_eq!(listener.native_channel("suspension_velocity_lf"), Some(LegacyValue::Int(0)));
        let mut changed = sample(&PCARS1, MAPPING.size, 5);
        changed.f32(PCARS1.rpm, f32::NAN).bytes(PCARS1.car_name, b"\xff");
        blocks.set(MAPPING, changed.build());
        assert_eq!(listener.read_native("rpm"), NativeRead::Raised);
        assert_eq!(listener.native_channel("car_name"), Some(LegacyValue::Int(0)));
        assert_eq!(listener.native_channel("speed"), Some(LegacyValue::Str("41".into())));
        let mut paused = sample(&PCARS1, MAPPING.size, 5);
        paused.u32(PCARS1.game_state, 3);
        blocks.set(MAPPING, paused.build());
        assert_eq!(listener.native_channel("car_name"), Some(LegacyValue::Int(0)));
        assert_eq!(listener.native_channel("unknown"), Some(LegacyValue::Int(0)));
    }

    #[test]
    fn native_missing_game_keeps_the_prologue_and_local_catches() {
        let mut pc = listener(9);
        assert_eq!(pc.read_native("unknown"), NativeRead::Raised);
        pc.set_pause_when_not_playing(false);
        assert_eq!(pc.read_native("rpm"), NativeRead::Raised);
        for name in ["car_name", "track_loc", "unknown", "suspension_velocity_lf", "yaw_rate"] {
            assert_eq!(pc.native_channel(name), Some(LegacyValue::Int(0)), "{name}");
        }
        let mut t = Telemetry { speed: 999.0, ..Telemetry::off() };
        pc.receive_native(&mut t).unwrap();
        assert_eq!(t.speed, 999.0);
    }

    /// Liveness: version 5 and non-zero engine data required; absent memory is not running.
    #[test]
    fn check_status() {
        assert!(listener(5).is_running().unwrap());
        assert!(!listener(9).is_running().unwrap());
        let mut absent = ProjectCars::with_opener(FakeBlocks::default(), true);
        assert!(!absent.is_running().unwrap());
        let mut idle = listener(5);
        let mut image = sample(&PCARS1, 7316, 5);
        image.f32(PCARS1.speed, 0.0).f32(PCARS1.rpm, 0.0).f32(PCARS1.max_rpm, 0.0).i32(PCARS1.gear, 0);
        idle.opener.set(MAPPING, image.build());
        assert!(!idle.is_running().unwrap());
    }

    /// Channels: PC1 mapping including the /10 heave, the shared /1000 pseudo suspension and the 3/4 flag collapse.
    #[test]
    fn get_channel_mapping() {
        let mut pc = listener(5);
        assert!(pc.is_running().unwrap());
        let mut t = Telemetry::off();
        pc.poll(&mut t).unwrap();
        assert_eq!(t.rpm, 6999.0);
        assert_eq!(t.max_rpm, 8000.0);
        assert_eq!(t.gear, 3.0);
        assert_eq!(t.speed, 41.0);
        assert_eq!(t.wheel_slip_lr, f64::from(0.9f32));
        assert_eq!(t.location, "Brands Hatch");
        assert_eq!(t.vehicle, "Formula Renault");
        assert_eq!(t.steering_wheel_angle, -0.5);
        assert_eq!(t.lateral_acceleration, 2.0);
        assert_eq!(t.vertical_acceleration, 0.9);
        assert_eq!(t.suspension_travel_rr, 9.0 / 1000.0);
        assert_eq!(t.longitudinal_acceleration, 3.0);
        assert_eq!(t.vertical_velocity, -0.5);
        assert_eq!(t.longitudinal_velocity, 40.0);
        assert_eq!(t.lateral_velocity, -1.5);
        assert_eq!(t.roll, rad_to_deg(f64::from(0.3f32)));
        assert_eq!(t.pitch, rad_to_deg(f64::from(0.1f32)));
        assert_eq!(t.yaw, rad_to_deg(f64::from(-0.2f32)));
        assert_eq!(t.abs_lock, 1.0);
        assert_eq!(
            (t.rumble_pitch_lf, t.rumble_pitch_rf, t.rumble_pitch_lr, t.rumble_pitch_rr),
            (40.0, 40.0, 40.0, 0.0)
        );
        assert_eq!(t.flag, 3.0);
        assert_eq!(t.raw_flag, 4.0);
        assert_eq!(t.oil_temperature, 95.0);
        assert_eq!(t.fuel_level, 25.0);
        assert_eq!(t.suspension_velocity_lf, 0.0);
        assert_eq!(t.tyre_temperature_lf, 0.0);
    }

    /// Channels: undecodable names answer 0.
    #[test]
    fn invalid_names_answer_zero() {
        let mut pc = listener(5);
        let mut image = sample(&PCARS1, 7316, 5);
        image.bytes(PCARS1.car_name, b"\xff\xfe");
        pc.opener.set(MAPPING, image.build());
        pc.is_running().unwrap();
        let mut t = Telemetry::off();
        pc.poll(&mut t).unwrap();
        assert_eq!(t.vehicle, ZERO_NAME);
        assert_eq!(t.location, "Brands Hatch");
    }

    /// Pause gate: mGameState other than 2 answers 0 everywhere while pausing is enabled.
    #[test]
    fn pause_gate() {
        let mut pc = listener(5);
        let mut image = sample(&PCARS1, 7316, 5);
        image.u32(PCARS1.game_state, 3);
        pc.opener.set(MAPPING, image.build());
        pc.is_running().unwrap();
        let mut t = Telemetry::off();
        pc.poll(&mut t).unwrap();
        assert_eq!(t.rpm, 0.0);
        assert_eq!(t.vehicle, ZERO_NAME);
    }

    /// The mapping is created when missing and dropped again when the version check fails; a
    /// valid view is kept (one object) until the presence sum drops it, and a later version failure leaves the
    /// kept view in place.
    #[test]
    fn live_view_is_kept_until_the_presence_sum_drops_it() {
        let mut pc = ProjectCars::with_opener(FakeBlocks::default(), true);
        assert!(!pc.is_running().unwrap());
        assert_eq!(pc.opener.held_count(MAPPING.name), 0);
        pc.opener.set(MAPPING, sample(&PCARS1, 7316, 5).build());
        assert!(pc.is_running().unwrap());
        assert!(pc.is_running().unwrap());
        assert_eq!(pc.opener.held_count(MAPPING.name), 1);
        pc.opener.set(MAPPING, sample(&PCARS1, 7316, 9).build());
        assert!(!pc.is_running().unwrap());
        assert_eq!(pc.opener.held_count(MAPPING.name), 1);
        let mut idle = sample(&PCARS1, 7316, 5);
        idle.f32(PCARS1.speed, 0.0).f32(PCARS1.rpm, 0.0).f32(PCARS1.max_rpm, 0.0).i32(PCARS1.gear, 0);
        pc.opener.set(MAPPING, idle.build());
        assert!(!pc.is_running().unwrap());
        assert_eq!(pc.opener.held_count(MAPPING.name), 0);
    }

    /// Without a view (failed liveness test) every channel read fails.
    #[test]
    fn no_game_raises_everywhere() {
        let mut pc = listener(9);
        pc.is_running().unwrap();
        let mut t = Telemetry::off();
        t.rpm = 1234.0;
        pc.poll(&mut t).unwrap();
        assert_eq!(t.rpm, 1234.0);
        assert_eq!(t.raw_flag, -1.0);
    }
    /// Without a view and with pausing enabled the pause gate's `mGameState` read fails for every channel,
    /// so the vehicle keeps its previous value, track_loc stores 0 and the rate channels fail.
    #[test]
    fn no_game_with_pausing_raises_in_the_prologue() {
        let mut pc = listener(9);
        pc.is_running().unwrap();
        let mut t = Telemetry::off();
        t.vehicle = "Formula Renault".into();
        t.suspension_velocity_lf = 0.4;
        pc.poll(&mut t).unwrap();
        assert_eq!(t.vehicle, "Formula Renault");
        assert_eq!(t.location, ZERO_NAME);
        assert_eq!(t.suspension_velocity_lf, 0.4);
        assert_eq!(pc.motion_rates(), MotionRates::RAISED);
    }

    /// car_name / track_loc: without a view and with pausing disabled, the failed
    /// `mCarName` / `mTrackLocation` reads are handled, so both names answer 0; the unimplemented
    /// suspension velocities and the rate channels fall through to 0.
    #[test]
    fn no_game_without_pausing_answers_zero_names() {
        let mut pc = listener(9);
        pc.set_pause_when_not_playing(false);
        pc.is_running().unwrap();
        let mut t = Telemetry::off();
        t.vehicle = "Formula Renault".into();
        t.location = "Brands Hatch".into();
        t.rpm = 1234.0;
        t.suspension_velocity_lf = 0.4;
        pc.poll(&mut t).unwrap();
        assert_eq!(t.vehicle, ZERO_NAME);
        assert_eq!(t.location, ZERO_NAME);
        assert_eq!(t.rpm, 1234.0);
        assert_eq!(t.suspension_velocity_lf, 0.0);
        assert_eq!(pc.motion_rates(), MotionRates::ZERO);
        // The next successful liveness test answers the rates 0 again.
        pc.opener.set(MAPPING, sample(&PCARS1, 7316, 5).build());
        pc.set_pause_when_not_playing(true);
        assert!(pc.is_running().unwrap());
        pc.poll(&mut t).unwrap();
        assert_eq!(pc.motion_rates(), MotionRates::ZERO);
        assert_eq!(t.vehicle, "Formula Renault");
    }
}
