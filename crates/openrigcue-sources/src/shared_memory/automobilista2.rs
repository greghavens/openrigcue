//! Automobilista 2 (listener `automobilista2`): the `$pcars2$` shared memory.
//!
//! The 11708-byte block is mapped read-only (so an absent block is "not running"), with no version
//! check; a copy is taken in the liveness test and again in the receive step, and channel reads use the latter.

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::answers::{write_all_raised, write_all_zero, zero_generic_channels, MotionRates};
use super::block_reader::{BlockOpener, Fields, MappingSpec};
use super::pages::LiveMirrors;
use super::pcars::{
    native_channels, rates_without_game, write_common_channels, GameInstance, PcarsKind, GAME_INGAME_PLAYING, LF, LR,
    PCARS2, PCARS2_TURBO_BOOST_PRESSURE, RF, RR,
};
use super::project_cars2::{write_names, write_suspension};
use crate::{NativeRead, Source, SourceError};

/// The `$pcars2$` mapping: 11708 bytes, read-only.
pub const MAPPING: MappingSpec = MappingSpec { name: "$pcars2$", size: 11708 };
/// Every mapping this listener reads.
pub const MAPPINGS: &[MappingSpec] = &[MAPPING];

/// Listener `automobilista2`.
pub struct Automobilista2<O: BlockOpener = LiveMirrors> {
    opener: O,
    pause_when_not_playing: bool,
    pause_reader: Option<super::NativePauseReader>,
    game: Option<Vec<u8>>,
    rates: MotionRates,
}

impl Automobilista2<LiveMirrors> {
    /// Listener reading the bridge mirror. `pause_when_not_playing` is `APP/pause_telemetry_when_not_playing`.
    pub fn new(pause_when_not_playing: bool) -> Self {
        Automobilista2::with_opener(LiveMirrors, pause_when_not_playing)
    }
}

impl<O: BlockOpener> Automobilista2<O> {
    /// Listener reading the mappings `opener` opens.
    pub fn with_opener(opener: O, pause_when_not_playing: bool) -> Self {
        Automobilista2 { opener, pause_when_not_playing, pause_reader: None, game: None, rates: MotionRates::ZERO }
    }

    /// Updates `APP/pause_telemetry_when_not_playing`.
    pub fn set_pause_when_not_playing(&mut self, pause: bool) {
        self.pause_when_not_playing = pause;
    }

    pub fn set_pause_reader(&mut self, reader: super::NativePauseReader) {
        self.pause_reader = Some(reader);
    }

    /// The rate channels answered by the last [`Source::poll`]: Automobilista 2 implements none, so they fall
    /// through to `0`, except after a failed receive snapshot with pausing enabled (the pause gate fails).
    pub fn motion_rates(&self) -> MotionRates {
        self.rates
    }

    fn native_answer(&self, name: &str) -> Option<LegacyValue> {
        let pause = super::native_pause(&self.pause_reader, self.pause_when_not_playing)?;
        let game = self.game.as_ref().map(|bytes| GameInstance { fields: Fields(bytes), layout: &PCARS2 });
        native_channels(game.as_ref(), PcarsKind::Automobilista2, pause, name)
    }
}

impl<O: BlockOpener> Source for Automobilista2<O> {
    fn id(&self) -> &'static str {
        "automobilista2"
    }

    /// The liveness test: the block exists and `mSpeed + mRpm + mMaxRPM + mGear != 0`.
    fn is_running(&mut self) -> Result<bool, SourceError> {
        let Ok(bytes) = self.opener.copy_read_only_once(MAPPING) else { return Ok(false) };
        let present = GameInstance { fields: Fields(&bytes), layout: &PCARS2 }.has_engine_data();
        self.game = present.then_some(bytes);
        Ok(present)
    }

    fn receive_native(&mut self, _t: &mut Telemetry) -> Result<bool, SourceError> {
        self.game = self.opener.copy_read_only_once(MAPPING).ok();
        Ok(self.game.is_some())
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
            // As for Project CARS 2: the implemented channels fail, the others have registry defaults.
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
        t.wheel_slip_lf = game.tyre_slip_speed(LF) / 3.0;
        t.wheel_slip_rf = game.tyre_slip_speed(RF) / 3.0;
        t.wheel_slip_lr = game.tyre_slip_speed(LR) / 3.0;
        t.wheel_slip_rr = game.tyre_slip_speed(RR) / 3.0;
        write_suspension(&game.fields, t);
        t.turbo_boost_pressure = game.fields.f32(PCARS2_TURBO_BOOST_PRESSURE);
        write_names(&game, t);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_live_pause_automobilista2_reads_once_before_game_state() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};
        let mut source = listener(10);
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
    use crate::shared_memory::answers::ZERO_NAME;
    use crate::shared_memory::block_reader::fake::FakeBlocks;
    use crate::shared_memory::pcars::test_image::sample;
    use crate::shared_memory::pcars::PCARS2_SUSPENSION_TRAVEL;

    fn listener(version: u32) -> Automobilista2<FakeBlocks> {
        let mut image = sample(&PCARS2, MAPPING.size, version);
        image.f32(PCARS2_SUSPENSION_TRAVEL + 4, 0.02).f32(PCARS2.tyre_temp, 90.0);
        let pages = FakeBlocks::default();
        pages.set(MAPPING, image.build());
        Automobilista2::with_opener(pages, true)
    }

    #[test]
    fn native_getter_receive_does_not_project() {
        let mut ams = listener(10);
        let mut t = Telemetry { speed: 999.0, ..Telemetry::off() };
        ams.receive_native(&mut t).unwrap();
        assert_eq!(t.speed, 999.0);
    }

    #[test]
    fn native_copy_kinds_and_failed_check_vs_receive() {
        let mut ams = listener(10);
        assert!(ams.is_running().unwrap());
        assert_eq!(ams.native_channel("rpm"), Some(LegacyValue::Str("6999".into())));
        assert_eq!(ams.native_channel("wheel_slip_lf"), Some(LegacyValue::Float(f64::from(0.3f32) / 3.0)));
        assert_eq!(ams.native_channel("tyre_temperature_lf"), Some(LegacyValue::Int(0)));
        assert_eq!(ams.native_channel("suspension_travel_rf"), Some(LegacyValue::Float(f64::from(0.02f32))));
        assert_eq!(ams.native_channel("fuel_level"), Some(LegacyValue::Float(25.0)));
        ams.opener.remove(MAPPING.name);
        ams.opener.fail_open(MAPPING.name, Some(std::io::ErrorKind::PermissionDenied));
        assert!(!ams.is_running().unwrap());
        assert_eq!(ams.native_channel("rpm"), Some(LegacyValue::Str("6999".into())));
        assert!(!ams.receive_native(&mut Telemetry::off()).unwrap());
        assert_eq!(ams.read_native("unknown"), NativeRead::Raised);
        ams.set_pause_when_not_playing(false);
        assert_eq!(ams.native_channel("unknown"), Some(LegacyValue::Int(0)));
        assert_eq!(ams.read_native("car_name"), NativeRead::Raised);
        ams.opener.fail_open(MAPPING.name, None);
        ams.opener.set(MAPPING, sample(&PCARS2, MAPPING.size, 10).build());
        ams.receive_native(&mut Telemetry::off()).unwrap();
        assert_eq!(ams.native_channel("rpm"), Some(LegacyValue::Str("6999".into())));
    }

    /// Liveness: no version check (any mVersion), an absent block is not running.
    #[test]
    fn check_status() {
        assert!(listener(10).is_running().unwrap());
        assert!(listener(3).is_running().unwrap());
        assert!(!Automobilista2::with_opener(FakeBlocks::default(), true).is_running().unwrap());
    }

    #[test]
    fn missing_read_only_snapshot_is_a_temporary_zero_copy() {
        let blocks = FakeBlocks::default();
        let mut source = Automobilista2::with_opener(blocks.clone(), true);
        assert!(!source.is_running().unwrap());
        assert_eq!(blocks.read_only_copies(), 1);
        assert!(source.receive_native(&mut Telemetry::off()).unwrap());
        assert_eq!(source.native_channel("speed"), Some(LegacyValue::Int(0)));
        assert_eq!(blocks.read_only_copies(), 2);
        assert_eq!(blocks.held_count(MAPPING.name), 0);
    }

    /// A failed receive step (no snapshot): every channel read fails, and those answers are the tick's telemetry.
    #[test]
    fn failed_snapshot_raises_everywhere() {
        let blocks = FakeBlocks::default();
        blocks.fail_open(MAPPING.name, Some(std::io::ErrorKind::PermissionDenied));
        let mut ams2 = Automobilista2::with_opener(blocks, true);
        let mut t = Telemetry::off();
        t.rpm = 5000.0;
        t.oil_pressure = 3.0;
        assert!(ams2.poll(&mut t).unwrap());
        assert_eq!(t.rpm, 5000.0);
        assert_eq!(t.oil_pressure, 0.0);
        assert_eq!(t.raw_flag, -1.0);
        assert_eq!(t.location, ZERO_NAME);
    }

    /// A failed snapshot after a successful liveness test makes the
    /// pause gate fail for the rate channels while pausing is enabled; without pausing they answer 0.
    #[test]
    fn failed_receive_data_rates() {
        let mut ams2 = listener(10);
        assert!(ams2.is_running().unwrap());
        ams2.opener.fail_read(MAPPING.name, true);
        let mut t = Telemetry::off();
        ams2.poll(&mut t).unwrap();
        assert_eq!(ams2.motion_rates(), MotionRates::RAISED);
        ams2.set_pause_when_not_playing(false);
        ams2.poll(&mut t).unwrap();
        assert_eq!(ams2.motion_rates(), MotionRates::ZERO);
    }

    /// Channels: wheel slip divided by 3, suspension per wheel, no tyre channels.
    #[test]
    fn get_channel_mapping() {
        let mut ams2 = listener(10);
        let mut t = Telemetry::off();
        assert!(ams2.poll(&mut t).unwrap());
        assert_eq!(t.wheel_slip_lf, f64::from(0.3f32) / 3.0);
        assert_eq!(t.wheel_slip_rr, f64::from(1.2f32) / 3.0);
        assert_eq!(t.suspension_travel_rf, f64::from(0.02f32));
        assert_eq!(t.vertical_acceleration, 9.0);
        assert_eq!(t.tyre_temperature_lf, 0.0);
        assert_eq!(t.longitudinal_acceleration, 3.0);
        assert_eq!(t.raw_flag, 4.0);
        assert_eq!(t.location, "Brands Hatch");
    }

    /// Pause gate: mGameState other than 2 answers 0 everywhere while pausing is enabled.
    #[test]
    fn pause_gate() {
        let mut ams2 = listener(10);
        let mut image = sample(&PCARS2, MAPPING.size, 10);
        image.u32(PCARS2.game_state, 4);
        ams2.opener.set(MAPPING, image.build());
        let mut t = Telemetry::off();
        ams2.poll(&mut t).unwrap();
        assert_eq!(t.speed, 0.0);
        assert_eq!(t.flag, 0.0);
    }
}
