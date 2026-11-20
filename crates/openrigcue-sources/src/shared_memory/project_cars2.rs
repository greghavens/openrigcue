//! Project CARS 2, 3 and Pro (listener `projectcars2`): the `$pcars2$` shared memory.
//!
//! The 11624-byte block is mapped create-or-open per snapshot;
//! a copy of the `GameInstance` (`SHARED_MEMORY_VERSION` 9 required) is taken in the liveness test and again in
//! the receive step, and channel reads use the latter.

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::answers::{write_all_raised, write_all_zero, zero_generic_channels, MotionRates, ZERO_NAME};
use super::block_reader::{BlockOpener, Fields, MappingSpec};
use super::packed::ArrayFields;
use super::pages::LiveMirrors;
use super::pcars::{
    native_channels, rates_without_game, write_common_channels, GameInstance, PcarsKind, GAME_INGAME_PLAYING, LF, LR,
    PCARS2, PCARS2_SUSPENSION_TRAVEL, PCARS2_SUSPENSION_VELOCITY, PCARS2_TURBO_BOOST_PRESSURE, RF, RR,
};
use super::python_numeric::decode_utf8;
use crate::{NativeRead, Source, SourceError};

/// The `$pcars2$` mapping: 11624 bytes. Automobilista 2 maps the same name with
/// 11708 bytes ([`super::automobilista2::MAPPING`]).
pub const MAPPING: MappingSpec = MappingSpec { name: "$pcars2$", size: 11624 };
/// Every mapping this listener reads.
pub const MAPPINGS: &[MappingSpec] = &[MAPPING];

/// The required `SHARED_MEMORY_VERSION`.
const SHARED_MEMORY_VERSION: u32 = 9;

/// The PC2-only `tyre_temperature_*` and `tyre_wear_*` channels (`mTyreTemp`, `mTyreWear` per wheel).
fn write_tyres(game: &GameInstance, t: &mut Telemetry) {
    let f = &game.fields;
    let l = game.layout;
    t.tyre_temperature_lf = f.f32_at(l.tyre_temp, LF);
    t.tyre_temperature_rf = f.f32_at(l.tyre_temp, RF);
    t.tyre_temperature_lr = f.f32_at(l.tyre_temp, LR);
    t.tyre_temperature_rr = f.f32_at(l.tyre_temp, RR);
    t.tyre_wear_lf = f.f32_at(l.tyre_wear, LF);
    t.tyre_wear_rf = f.f32_at(l.tyre_wear, RF);
    t.tyre_wear_lr = f.f32_at(l.tyre_wear, LR);
    t.tyre_wear_rr = f.f32_at(l.tyre_wear, RR);
}

/// The suspension channels of the PC2/AMS2 layout (`mSuspensionTravel`, `mSuspensionVelocity`, per wheel).
pub(crate) fn write_suspension(fields: &Fields, t: &mut Telemetry) {
    t.suspension_travel_lf = fields.f32_at(PCARS2_SUSPENSION_TRAVEL, LF);
    t.suspension_travel_rf = fields.f32_at(PCARS2_SUSPENSION_TRAVEL, RF);
    t.suspension_travel_lr = fields.f32_at(PCARS2_SUSPENSION_TRAVEL, LR);
    t.suspension_travel_rr = fields.f32_at(PCARS2_SUSPENSION_TRAVEL, RR);
    t.suspension_velocity_lf = fields.f32_at(PCARS2_SUSPENSION_VELOCITY, LF);
    t.suspension_velocity_rf = fields.f32_at(PCARS2_SUSPENSION_VELOCITY, RF);
    t.suspension_velocity_lr = fields.f32_at(PCARS2_SUSPENSION_VELOCITY, LR);
    t.suspension_velocity_rr = fields.f32_at(PCARS2_SUSPENSION_VELOCITY, RR);
}

/// The strict UTF-8 decoding of the names: a failing `car_name` keeps the previous vehicle,
/// a failing `track_loc` makes the dispatcher store `0`.
pub(crate) fn write_names(game: &GameInstance, t: &mut Telemetry) {
    t.location = decode_utf8(game.track_location()).unwrap_or_else(|| ZERO_NAME.to_owned());
    if let Some(vehicle) = decode_utf8(game.car_name()) {
        t.vehicle = vehicle;
    }
}

/// Listener `projectcars2` (game ids `projectcars2`, `projectcars3`, `projectcarspro`).
pub struct ProjectCars2<O: BlockOpener = LiveMirrors> {
    opener: O,
    pause_when_not_playing: bool,
    pause_reader: Option<super::NativePauseReader>,
    game: Option<Vec<u8>>,
    rates: MotionRates,
}

impl ProjectCars2<LiveMirrors> {
    /// Listener reading the bridge mirror. `pause_when_not_playing` is `APP/pause_telemetry_when_not_playing`.
    pub fn new(pause_when_not_playing: bool) -> Self {
        ProjectCars2::with_opener(LiveMirrors, pause_when_not_playing)
    }
}

impl<O: BlockOpener> ProjectCars2<O> {
    /// Listener reading the mappings `opener` opens.
    pub fn with_opener(opener: O, pause_when_not_playing: bool) -> Self {
        ProjectCars2 { opener, pause_when_not_playing, pause_reader: None, game: None, rates: MotionRates::ZERO }
    }

    /// Updates `APP/pause_telemetry_when_not_playing`.
    pub fn set_pause_when_not_playing(&mut self, pause: bool) {
        self.pause_when_not_playing = pause;
    }

    pub fn set_pause_reader(&mut self, reader: super::NativePauseReader) {
        self.pause_reader = Some(reader);
    }

    /// The rate channels answered by the last [`Source::poll`]: Project CARS 2 implements none, so they fall
    /// through to `0`, except after a failed receive snapshot with pausing enabled (the pause gate fails).
    pub fn motion_rates(&self) -> MotionRates {
        self.rates
    }

    /// One snapshot: a copy of the `GameInstance` through a
    /// create-or-open dropped right after the copy (zeros when the section was missing), `None` when it is not
    /// version 9.
    fn snapshot(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        self.opener.create_once(MAPPING).map(|bytes| {
            (GameInstance { fields: Fields(&bytes), layout: &PCARS2 }.version() == SHARED_MEMORY_VERSION)
                .then_some(bytes)
        })
    }

    fn native_answer(&self, name: &str) -> Option<LegacyValue> {
        let pause = super::native_pause(&self.pause_reader, self.pause_when_not_playing)?;
        let game = self.game.as_ref().map(|bytes| GameInstance { fields: Fields(bytes), layout: &PCARS2 });
        native_channels(game.as_ref(), PcarsKind::ProjectCars2, pause, name)
    }
}

impl<O: BlockOpener> Source for ProjectCars2<O> {
    fn id(&self) -> &'static str {
        "projectcars2"
    }

    /// The liveness test: a valid snapshot with `mSpeed + mRpm + mMaxRPM + mGear != 0`.
    fn is_running(&mut self) -> Result<bool, SourceError> {
        let Ok(snapshot) = self.snapshot() else { return Ok(false) };
        self.game = snapshot;
        let present = self
            .game
            .as_ref()
            .is_some_and(|bytes| GameInstance { fields: Fields(bytes), layout: &PCARS2 }.has_engine_data());
        if !present {
            self.game = None;
        }
        Ok(present)
    }

    fn receive_native(&mut self, _t: &mut Telemetry) -> Result<bool, SourceError> {
        match self.snapshot() {
            Ok(game) => {
                self.game = game;
                Ok(true)
            }
            Err(_) => {
                self.game = None;
                Ok(false)
            }
        }
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        self.native_answer(name)
    }

    fn read_native(&self, name: &str) -> NativeRead {
        self.native_answer(name).map_or(NativeRead::Raised, NativeRead::Value)
    }

    /// The receive step (a fresh snapshot, none when it fails) and
    /// the channel reads over it.
    fn poll(&mut self, t: &mut Telemetry) -> Result<bool, SourceError> {
        self.receive_native(t)?;
        let Some(bytes) = &self.game else {
            // Every implemented channel fails without a snapshot; the ones it does not implement all have registry
            // defaults, so only the rates depend on whether the pause gate failed first.
            write_all_raised(t);
            self.rates = rates_without_game(self.pause_when_not_playing);
            return Ok(true);
        };
        self.rates = MotionRates::ZERO;
        let game = GameInstance { fields: Fields(bytes), layout: &PCARS2 };
        if self.pause_when_not_playing && game.game_state() != GAME_INGAME_PLAYING {
            write_all_zero(t);
            return Ok(true);
        }
        zero_generic_channels(t);
        write_common_channels(&game, t);
        t.vertical_acceleration = game.local_acceleration(1);
        t.wheel_slip_lf = game.tyre_slip_speed(LF);
        t.wheel_slip_rf = game.tyre_slip_speed(RF);
        t.wheel_slip_lr = game.tyre_slip_speed(LR);
        t.wheel_slip_rr = game.tyre_slip_speed(RR);
        write_suspension(&game.fields, t);
        write_tyres(&game, t);
        t.turbo_boost_pressure = game.fields.f32(PCARS2_TURBO_BOOST_PRESSURE);
        write_names(&game, t);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_live_pause_project_cars2_reads_once_before_game_state() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};
        let mut source = listener(image());
        assert!(source.is_running().unwrap());
        source.game.as_mut().unwrap()[8..12].copy_from_slice(&0_u32.to_le_bytes());
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
    use crate::shared_memory::block_reader::fake::BlockBuilder as Image;
    use crate::shared_memory::block_reader::fake::FakeBlocks;
    use crate::shared_memory::pcars::test_image::sample;

    fn image() -> Image {
        let mut i = sample(&PCARS2, MAPPING.size, SHARED_MEMORY_VERSION);
        i.f32(PCARS2_SUSPENSION_TRAVEL, 0.01)
            .f32(PCARS2_SUSPENSION_TRAVEL + 12, 0.04)
            .f32(PCARS2_SUSPENSION_VELOCITY + 4, -0.2)
            .f32(PCARS2.tyre_temp + 8, 77.0)
            .f32(PCARS2.tyre_wear + 12, 0.5)
            .f32(PCARS2_TURBO_BOOST_PRESSURE, 150.0);
        i
    }

    fn listener(image: Image) -> ProjectCars2<FakeBlocks> {
        let pages = FakeBlocks::default();
        pages.set(MAPPING, image.build());
        ProjectCars2::with_opener(pages, true)
    }

    #[test]
    fn native_getter_reads_check_status_copy() {
        let mut pc = listener(image());
        pc.is_running().unwrap();
        assert_eq!(pc.native_channel("rpm"), Some(openrigcue_model::native_value::LegacyValue::Str("6999".into())));
    }

    #[test]
    fn native_copy_refreshes_at_receive_and_check_status() {
        use crate::derive::{Listener, SourceListener};
        let blocks = FakeBlocks::default();
        blocks.set(MAPPING, image().build());
        let pc = ProjectCars2::with_opener(blocks.clone(), true);
        let mut listener = SourceListener::new(pc);
        assert!(listener.is_running());
        let mut changed = image();
        changed.f32(PCARS2.rpm, 7777.9);
        blocks.set(MAPPING, changed.build());
        assert_eq!(listener.native_channel("rpm"), Some(LegacyValue::Str("6999".into())));
        let copies = blocks.created_once_count();
        assert_eq!(listener.native_channel("tyre_temperature_lr"), Some(LegacyValue::Float(77.0)));
        assert_eq!(listener.native_channel("suspension_velocity_rf"), Some(LegacyValue::Float(f64::from(-0.2f32))));
        assert_eq!(blocks.created_once_count(), copies);
        listener.receive();
        assert_eq!(listener.native_channel("rpm"), Some(LegacyValue::Str("7777".into())));
        assert_eq!(blocks.created_once_count(), copies + 1);
        let mut bad = image();
        bad.f32(PCARS2.rpm, f32::INFINITY).bytes(PCARS2.car_name, b"\xff");
        blocks.set(MAPPING, bad.build());
        assert!(listener.is_running());
        assert_eq!(listener.read_native("rpm"), NativeRead::Raised);
        assert_eq!(listener.read_native("car_name"), NativeRead::Raised);
        assert_eq!(listener.native_channel("gear"), Some(LegacyValue::Str("3".into())));
    }

    #[test]
    fn native_no_game_defaults_and_failed_version_receive_result() {
        let mut pc = listener(image());
        assert_eq!(pc.read_native("unknown"), NativeRead::Raised);
        pc.set_pause_when_not_playing(false);
        assert_eq!(pc.read_native("car_name"), NativeRead::Raised);
        assert_eq!(pc.native_channel("unknown"), Some(LegacyValue::Int(0)));
        let mut t = Telemetry { speed: 999.0, ..Telemetry::off() };
        pc.opener.set(MAPPING, sample(&PCARS2, MAPPING.size, 5).build());
        assert!(pc.receive_native(&mut t).unwrap(), "snapshot returned False normally; receiveData returned True");
        assert_eq!(t.speed, 999.0);
        assert_eq!(pc.read_native("rpm"), NativeRead::Raised);
    }

    /// Liveness: version 9 required, engine data required.
    #[test]
    fn check_status() {
        assert!(listener(image()).is_running().unwrap());
        assert!(!listener(sample(&PCARS2, MAPPING.size, 5)).is_running().unwrap());
        let mut idle = image();
        idle.f32(PCARS2.speed, 0.0).f32(PCARS2.rpm, 0.0).f32(PCARS2.max_rpm, 0.0).i32(PCARS2.gear, 0);
        assert!(!listener(idle).is_running().unwrap());
    }

    /// Channels: PC2 mapping (no heave scaling, real suspension, tyre temperature and wear, the turbo offset).
    /// Each snapshot makes a fresh create-or-open mapping for its copy and drops it: one
    /// per-poll create-or-open in the liveness test and one in the receive step, and none is kept; a missing section reads as
    /// the zeros of the section that call created (version 0: not running).
    #[test]
    fn every_snapshot_is_a_dropped_create_or_open() {
        let mut pc2 = listener(image());
        assert!(pc2.is_running().unwrap());
        pc2.poll(&mut Telemetry::off()).unwrap();
        assert_eq!(pc2.opener.created_once_count(), 2);
        assert_eq!(pc2.opener.held_count(MAPPING.name), 0);
        let mut absent = ProjectCars2::with_opener(FakeBlocks::default(), true);
        assert!(!absent.is_running().unwrap());
        assert_eq!(absent.opener.created_once_count(), 1);
    }

    #[test]
    fn get_channel_mapping() {
        let mut pc2 = listener(image());
        let mut t = Telemetry::off();
        assert!(pc2.poll(&mut t).unwrap());
        assert_eq!(t.vertical_acceleration, 9.0);
        assert_eq!(t.wheel_slip_rr, f64::from(1.2f32));
        assert_eq!(t.suspension_travel_lf, f64::from(0.01f32));
        assert_eq!(t.suspension_travel_rr, f64::from(0.04f32));
        assert_eq!(t.suspension_velocity_rf, f64::from(-0.2f32));
        assert_eq!(t.tyre_temperature_lr, 77.0);
        assert_eq!(t.tyre_wear_rr, 0.5);
        assert_eq!(t.turbo_boost_pressure, 150.0);
        assert_eq!(t.flag, 3.0);
        assert_eq!(t.speed, 41.0);
        assert_eq!(t.vehicle, "Formula Renault");
    }

    /// Channels: names that cannot be decoded; car_name keeps the previous value, track_loc becomes 0.
    #[test]
    fn invalid_names() {
        let mut bad = image();
        bad.bytes(PCARS2.car_name, b"\xc3").bytes(PCARS2.track_location, b"\xc3");
        let mut pc2 = listener(bad);
        let mut t = Telemetry::off();
        t.vehicle = "before".into();
        pc2.poll(&mut t).unwrap();
        assert_eq!(t.vehicle, "before");
        assert_eq!(t.location, ZERO_NAME);
    }

    /// A failed receive step (no snapshot): every channel read fails.
    #[test]
    fn failed_snapshot_raises_everywhere() {
        let mut pc2 = ProjectCars2::with_opener(FakeBlocks::default(), true);
        let mut t = Telemetry::off();
        t.rpm = 5000.0;
        assert!(pc2.poll(&mut t).unwrap(), "the raised-channel answers are this tick's telemetry");
        assert_eq!(t.rpm, 5000.0);
        assert_eq!(t.location, ZERO_NAME);
    }

    /// The liveness test succeeds, then the receive step's snapshot fails.
    /// With pausing enabled the pause gate fails for every channel, the rates included; without it
    /// the rates fall through to 0. The next good snapshot answers them 0 again.
    #[test]
    fn failed_receive_data_rates() {
        let mut pc2 = listener(image());
        assert!(pc2.is_running().unwrap());
        pc2.opener.remove(MAPPING.name);
        let mut t = Telemetry::off();
        pc2.poll(&mut t).unwrap();
        assert_eq!(pc2.motion_rates(), MotionRates::RAISED);
        pc2.set_pause_when_not_playing(false);
        pc2.poll(&mut t).unwrap();
        assert_eq!(pc2.motion_rates(), MotionRates::ZERO);
        pc2.set_pause_when_not_playing(true);
        pc2.poll(&mut t).unwrap();
        pc2.opener.set(MAPPING, image().build());
        pc2.poll(&mut t).unwrap();
        assert_eq!(pc2.motion_rates(), MotionRates::ZERO);
        assert_eq!(t.rpm, 6999.0);
    }

    /// A snapshot maps the section afresh on every call: once the bridge has emptied the mirror (the
    /// game exited) the liveness test fails at once, and a restarted game is seen again.
    #[test]
    fn game_exit_and_restart() {
        let mut pc2 = listener(image());
        assert!(pc2.is_running().unwrap());
        pc2.opener.remove(MAPPING.name);
        assert!(!pc2.is_running().unwrap());
        pc2.opener.set(MAPPING, image().build());
        assert!(pc2.is_running().unwrap());
    }

    /// Pause gate: mGameState other than 2 answers 0 everywhere while pausing is enabled.
    #[test]
    fn pause_gate() {
        let mut replay = image();
        replay.u32(PCARS2.game_state, 6);
        let mut pc2 = listener(replay);
        let mut t = Telemetry::off();
        pc2.poll(&mut t).unwrap();
        assert_eq!(t.rpm, 0.0);
        pc2.set_pause_when_not_playing(false);
        pc2.poll(&mut t).unwrap();
        assert_eq!(t.rpm, 6999.0);
    }
}
