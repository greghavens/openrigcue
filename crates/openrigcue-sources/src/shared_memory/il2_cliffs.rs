//! IL-2 Sturmovik: Cliffs of Dover: the DeviceLink map `CLODDeviceLink`, an array of doubles indexed by slot
//! (listener key `il2cliffs`).

use std::time::Instant;

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::block_reader::{copy_block, BlockOpener, BridgeMirrors, Fields, MappingSpec};
use super::listener_channels::begin_tick;
use crate::udp_flight::{AngularRates, FlightSource};
use crate::{Source, SourceError};

const MAP: MappingSpec = MappingSpec { name: "CLODDeviceLink", size: 16384 };

/// DeviceLink slots the listener reads.
const SLOT_YAW: usize = 840;
const SLOT_PITCH: usize = 841;
const SLOT_ROLL: usize = 842;
const SLOT_RPM: usize = 1480;
const SLOT_SPEED: usize = 1609;
const SLOT_DIRECTION_INDICATOR: usize = 1769;

/// A slot value at or beyond this magnitude is "unavailable".
const UNAVAILABLE_LIMIT: f64 = 1e100;
/// Exponential smoothing weight of the previous rate (the new raw rate gets `1 - alpha`).
const RATE_SMOOTHING_ALPHA: f64 = 0.2;
/// Samples closer together than this (seconds) do not update the rates.
const MIN_RATE_INTERVAL: f64 = 0.0001;

/// A slot value, or `None` when it is NaN, infinite or at least 1e100 in magnitude.
fn slot(snapshot: &[u8], index: usize) -> Option<f64> {
    let value = Fields(snapshot).f64(index * 8);
    (value.is_finite() && value.abs() < UNAVAILABLE_LIMIT).then_some(value)
}

/// `v` modulo 360 with the sign of the divisor (result in `[0, 360)`).
fn normalize_360(value: f64) -> f64 {
    let normalized = value.rem_euclid(360.0);
    // An exact zero takes the divisor's positive sign.
    if normalized == 0.0 {
        0.0
    } else {
        normalized
    }
}

/// Shortest signed difference of two headings: `((c - p) + 180) % 360 - 180`.
fn wrapped_delta(current: f64, previous: f64) -> f64 {
    normalize_360((current - previous) + 180.0) - 180.0
}

/// Smoothed angular rates from successive attitudes.
#[derive(Debug, Clone, Default, PartialEq)]
struct RateTracker {
    previous_time: Option<f64>,
    previous: [f64; 3],
    rates: [f64; 3],
}

impl RateTracker {
    /// Takes in one sample: once a previous sample exists and more than 0.1 ms passed, each rate becomes
    /// `rate * alpha + raw * (1 - alpha)` (yaw and roll through the wrapped difference); the sample is always
    /// remembered. Returns (yaw, pitch, roll) rates in degrees per second.
    fn update(&mut self, attitude: [f64; 3], time: f64) -> [f64; 3] {
        if let Some(previous_time) = self.previous_time {
            let dt = time - previous_time;
            if dt > MIN_RATE_INTERVAL {
                let [yaw, pitch, roll] = attitude;
                let [previous_yaw, previous_pitch, previous_roll] = self.previous;
                let raw = [
                    wrapped_delta(yaw, previous_yaw) / dt,
                    (pitch - previous_pitch) / dt,
                    wrapped_delta(roll, previous_roll) / dt,
                ];
                for (rate, raw) in self.rates.iter_mut().zip(raw) {
                    *rate = *rate * RATE_SMOOTHING_ALPHA + raw * (1.0 - RATE_SMOOTHING_ALPHA);
                }
            }
        }
        self.previous = attitude;
        self.previous_time = Some(time);
        self.rates
    }
}

/// The channels decoded from one snapshot.
#[derive(Debug, Clone, Copy, PartialEq)]
struct CliffsFrame {
    yaw: f64,
    pitch: f64,
    roll: f64,
    rates: [f64; 3],
    rpm: f64,
    max_rpm: f64,
    speed: f64,
}

/// The IL-2 Cliffs of Dover listener.
pub struct Il2Cliffs<O: BlockOpener = BridgeMirrors> {
    opener: O,
    epoch: Instant,
    tracker: Option<RateTracker>,
    max_observed_rpm: f64,
    latest: Option<CliffsFrame>,
}

impl Il2Cliffs<BridgeMirrors> {
    pub fn new() -> Self {
        Self::with_opener(BridgeMirrors)
    }
}

impl Default for Il2Cliffs<BridgeMirrors> {
    fn default() -> Self {
        Self::new()
    }
}

impl<O: BlockOpener> Il2Cliffs<O> {
    pub fn with_opener(opener: O) -> Self {
        Il2Cliffs { opener, epoch: Instant::now(), tracker: None, max_observed_rpm: 0.0, latest: None }
    }

    /// Decodes the channels of one snapshot; `None` when a required slot is unavailable. `max_rpm` is the
    /// highest rpm seen since the last reset plus one.
    fn decode(&mut self, snapshot: &[u8], clock: impl FnOnce() -> f64) -> Option<CliffsFrame> {
        let raw_yaw = slot(snapshot, SLOT_YAW)?;
        // The direction indicator falls back to the raw heading when unavailable.
        let indicator = slot(snapshot, SLOT_DIRECTION_INDICATOR).unwrap_or(raw_yaw);
        let yaw = normalize_360(if (-0.5..=360.5).contains(&indicator) { indicator } else { raw_yaw });
        let pitch = -slot(snapshot, SLOT_PITCH)?;
        let roll = slot(snapshot, SLOT_ROLL)?;
        let tracker = self.tracker.get_or_insert_with(RateTracker::default);
        let rates = tracker.update([yaw, pitch, roll], clock());
        let rpm = slot(snapshot, SLOT_RPM).unwrap_or(0.0);
        if rpm > self.max_observed_rpm {
            self.max_observed_rpm = rpm;
        }
        Some(CliffsFrame {
            yaw,
            pitch,
            roll,
            rates,
            rpm,
            max_rpm: self.max_observed_rpm + 1.0,
            speed: slot(snapshot, SLOT_SPEED).unwrap_or(0.0),
        })
    }

    /// The failure branch of the liveness test: forget the rates, the channels and the
    /// observed maximum rpm.
    fn reset(&mut self) {
        self.tracker = None;
        self.latest = None;
        self.max_observed_rpm = 0.0;
    }

    fn check_with_clock(&mut self, clock: impl FnOnce() -> f64) -> bool {
        let decoded = match copy_block(&mut self.opener, MAP) {
            Ok(snapshot) => self.decode(&snapshot, clock),
            Err(error) => {
                tracing::debug!(%error, "CLODDeviceLink telemetry unavailable");
                None
            }
        };
        match decoded {
            Some(frame) => {
                self.latest = Some(frame);
                true
            }
            None => {
                self.reset();
                false
            }
        }
    }

    #[cfg(test)]
    fn check_at(&mut self, time: f64) -> bool {
        self.check_with_clock(|| time)
    }
}

impl<O: BlockOpener> Source for Il2Cliffs<O> {
    fn id(&self) -> &'static str {
        "il2cliffs"
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        let Some(f) = self.latest else { return Some(LegacyValue::Int(0)) };
        let value = match name {
            "yaw" => f.yaw,
            "pitch" => f.pitch,
            "roll" => f.roll,
            "rpm" => f.rpm,
            "max_rpm" => f.max_rpm,
            "speed" => f.speed,
            "yaw_rate" => f.rates[0],
            "pitch_rate" => f.rates[1],
            "roll_rate" => f.rates[2],
            _ => return Some(LegacyValue::Int(0)),
        };
        Some(LegacyValue::Float(value))
    }

    /// The liveness test: open the existing map (never created), decode, update the rates.
    fn is_running(&mut self) -> Result<bool, SourceError> {
        let epoch = self.epoch;
        Ok(self.check_with_clock(|| epoch.elapsed().as_secs_f64()))
    }

    fn receive_native(&mut self, _telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        Ok(true)
    }

    /// Numeric compatibility view (a missing channel reads 0).
    fn poll(&mut self, t: &mut Telemetry) -> Result<bool, SourceError> {
        begin_tick(t);
        if let Some(frame) = self.latest {
            t.yaw = frame.yaw;
            t.pitch = frame.pitch;
            t.roll = frame.roll;
            t.rpm = frame.rpm;
            t.max_rpm = frame.max_rpm;
            t.speed = frame.speed;
        }
        Ok(true)
    }
}

impl<O: BlockOpener> FlightSource for Il2Cliffs<O> {
    /// The smoothed rates of the last snapshot; 0 while there is none.
    fn angular_rates(&self) -> AngularRates {
        match self.latest {
            Some(CliffsFrame { rates: [yaw, pitch, roll], .. }) => AngularRates { yaw, pitch, roll },
            None => AngularRates::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared_memory::block_reader::fake::FakeBlocks;

    #[test]
    fn native_dictionary_float_values_and_reset_integer_defaults() {
        use LegacyValue::{Float, Int};
        let mut s = source(Some(snapshot(&[(SLOT_YAW, 10.0), (SLOT_RPM, 900.5), (SLOT_SPEED, f64::NAN)])));
        assert_eq!(s.native_channel("rpm"), Some(Int(0)));
        assert_eq!(s.native_channel("yaw_rate"), Some(Int(0)));
        assert!(s.check_at(0.0));
        for (name, expected) in [
            ("rpm", Float(900.5)),
            ("max_rpm", Float(901.5)),
            ("speed", Float(0.0)),
            ("yaw_rate", Float(0.0)),
            ("pitch_rate", Float(0.0)),
            ("gear", Int(0)),
        ] {
            assert_eq!(s.native_channel(name), Some(expected), "{name}");
        }
        s.reset();
        assert_eq!(s.native_channel("speed"), Some(Int(0)));
        assert_eq!(s.native_channel("yaw"), Some(Int(0)));
        assert!(s.poll(&mut Telemetry::off()).unwrap());
    }

    fn snapshot(values: &[(usize, f64)]) -> Vec<u8> {
        let mut bytes = vec![0u8; MAP.size];
        for &(index, value) in values {
            bytes[index * 8..index * 8 + 8].copy_from_slice(&value.to_le_bytes());
        }
        bytes
    }

    fn source(bytes: Option<Vec<u8>>) -> Il2Cliffs<FakeBlocks> {
        let blocks = FakeBlocks::default();
        if let Some(bytes) = bytes {
            blocks.set(MAP, bytes);
        }
        Il2Cliffs::with_opener(blocks)
    }

    #[test]
    fn native_clock_is_sampled_after_reading_and_releasing_snapshot() {
        use std::sync::{Arc, Mutex};
        struct TimedOpener(Arc<Mutex<Option<Instant>>>);
        struct TimedReader {
            finished: Arc<Mutex<Option<Instant>>>,
            bytes: Vec<u8>,
        }
        impl super::super::block_reader::BlockOpener for TimedOpener {
            fn open(&mut self, _: MappingSpec) -> std::io::Result<Box<dyn super::super::block_reader::BlockReader>> {
                Ok(Box::new(TimedReader {
                    finished: self.0.clone(),
                    bytes: snapshot(&[(SLOT_YAW, 12.0), (SLOT_DIRECTION_INDICATOR, 12.0)]),
                }))
            }
        }
        impl super::super::block_reader::BlockReader for TimedReader {
            fn read(&mut self) -> std::io::Result<&[u8]> {
                // A two-millisecond blocking read makes the pre-read clock regression deterministic.
                std::thread::sleep(std::time::Duration::from_millis(2));
                Ok(&self.bytes)
            }
        }
        impl Drop for TimedReader {
            fn drop(&mut self) {
                *self.finished.lock().unwrap() = Some(Instant::now());
            }
        }
        let finished = Arc::new(Mutex::new(None));
        let mut source = Il2Cliffs::with_opener(TimedOpener(finished.clone()));
        assert!(source.is_running().unwrap());
        let released = finished.lock().unwrap().unwrap().duration_since(source.epoch).as_secs_f64();
        let sampled = source.tracker.as_ref().unwrap().previous_time.unwrap();
        assert!(sampled >= released, "timestamp {sampled} precedes completed snapshot {released}");
    }

    #[test]
    fn unavailable_attitudes_and_failed_maps_do_not_read_clock() {
        for values in [
            vec![(SLOT_YAW, f64::NAN)],
            vec![(SLOT_YAW, 5.0), (SLOT_PITCH, f64::INFINITY)],
            vec![(SLOT_YAW, 5.0), (SLOT_ROLL, 1e100)],
        ] {
            let mut s = source(Some(snapshot(&values)));
            assert!(!s.check_with_clock(|| panic!("invalid attitude sampled clock")));
        }
        let mut absent = source(None);
        assert!(!absent.check_with_clock(|| panic!("failed map sampled clock")));
        let mut valid = source(Some(snapshot(&[(SLOT_YAW, 12.0)])));
        let mut calls = 0;
        assert!(valid.check_with_clock(|| {
            calls += 1;
            42.0
        }));
        assert_eq!(calls, 1);
        assert_eq!(valid.tracker.as_ref().unwrap().previous_time, Some(42.0));
    }

    /// An open never creates the map, so a missing map is not running.
    #[test]
    fn missing_map_is_not_running() {
        assert!(!source(None).check_at(0.0));
    }

    /// The open fails once the game has gone. The bridge
    /// marks that by emptying the per-poll mirror to length 0 (all-zero slots would otherwise be "available"); the
    /// listener stops running, resets, and runs again when the same file is refilled.
    #[test]
    fn emptied_bridge_mirror_is_not_running() {
        use std::os::unix::fs::FileExt;
        // A temporary mirror directory, so the test never touches the real /dev/shm/openrigcue.
        let dir = tempfile::tempdir().unwrap();
        let path = super::super::mapping::mirror_path_in(dir.path(), MAP.name);
        std::fs::write(&path, snapshot(&[(SLOT_YAW, 10.0), (SLOT_RPM, 900.0)])).unwrap();
        let mut s = Il2Cliffs::with_opener(super::super::block_reader::MirrorsIn(dir.path().to_path_buf()));
        assert!(s.check_at(0.0));
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(0).unwrap();
        assert!(!s.check_at(1.0));
        assert_eq!((s.max_observed_rpm, s.latest.is_none()), (0.0, true));
        file.write_all_at(&snapshot(&[(SLOT_YAW, 20.0), (SLOT_DIRECTION_INDICATOR, 20.0)]), 0).unwrap();
        assert!(s.check_at(2.0));
        assert_eq!(s.latest.unwrap().yaw, 20.0);
    }

    /// The direction indicator wins while within -0.5..360.5, else the raw
    /// heading; both are reduced modulo 360; pitch is negated; max_rpm is the highest observed rpm plus 1.0.
    #[test]
    fn heading_selection_and_channels() {
        let mut s = source(Some(snapshot(&[
            (SLOT_YAW, 370.0),
            (SLOT_DIRECTION_INDICATOR, 360.25),
            (SLOT_PITCH, 5.0),
            (SLOT_ROLL, -3.0),
            (SLOT_RPM, 0.5),
            (SLOT_SPEED, 250.0),
        ])));
        assert!(s.check_at(0.0));
        let mut t = Telemetry::off();
        assert!(s.poll(&mut t).unwrap());
        assert_eq!((t.yaw, t.pitch, t.roll), (0.25, -5.0, -3.0));
        assert_eq!((t.rpm, t.max_rpm, t.speed), (0.5, 1.5, 250.0));
        assert_eq!(t.gear, 0.0);

        s.opener.set(MAP, snapshot(&[(SLOT_YAW, -10.0), (SLOT_DIRECTION_INDICATOR, 400.0)]));
        assert!(s.check_at(1.0));
        assert_eq!(s.latest.unwrap().yaw, 350.0);
        // A lower rpm keeps the observed maximum.
        assert_eq!(s.latest.unwrap().max_rpm, 1.5);
        // An unavailable indicator (1e100) falls back to the raw heading.
        s.opener.set(MAP, snapshot(&[(SLOT_YAW, 20.0), (SLOT_DIRECTION_INDICATOR, 1e100)]));
        assert!(s.check_at(2.0));
        assert_eq!(s.latest.unwrap().yaw, 20.0);
    }

    /// Failure branch: an unavailable heading resets tracker, channels and max rpm.
    #[test]
    fn unavailable_heading_resets() {
        let mut s = source(Some(snapshot(&[(SLOT_YAW, 10.0), (SLOT_RPM, 2500.0)])));
        assert!(s.check_at(0.0));
        assert_eq!(s.max_observed_rpm, 2500.0);
        s.opener.set(MAP, snapshot(&[(SLOT_YAW, f64::NAN)]));
        assert!(!s.check_at(1.0));
        assert_eq!((s.max_observed_rpm, s.tracker.is_none(), s.latest.is_none()), (0.0, true, true));
    }

    /// Rates: no rate on the first sample, wrapped yaw/roll differences,
    /// `rate * alpha + raw * (1 - alpha)` with alpha 0.2, and samples within 0.1 ms only
    /// move the reference.
    #[test]
    fn rate_tracker_smoothing() {
        let mut tracker = RateTracker::default();
        assert_eq!(tracker.update([359.0, 0.0, 179.0], 10.0), [0.0, 0.0, 0.0]);
        let rates = tracker.update([1.0, 1.0, -179.0], 10.5);
        assert_eq!(rates, [0.0 * 0.2 + 4.0 * 0.8, 0.0 * 0.2 + 2.0 * 0.8, 0.0 * 0.2 + 4.0 * 0.8]);
        assert_eq!(rates, [3.2, 1.6, 3.2]);
        let unchanged = tracker.update([50.0, 50.0, 50.0], 10.50005);
        assert_eq!(unchanged, rates);
        assert_eq!(tracker.previous, [50.0, 50.0, 50.0]);
        // A second smoothed step weights the previous rate by alpha.
        let next = tracker.update([50.0, 50.0, 50.0], 11.50005);
        assert_eq!(next, [3.2 * 0.2, 1.6 * 0.2, 3.2 * 0.2]);
    }

    /// Heading normalisation and the wrapped difference: the modulo takes the divisor's sign.
    #[test]
    fn normalized_heading_uses_positive_zero_for_negative_multiples() {
        for input in [-0.0, -360.0, -720.0, 0.0, 360.0] {
            assert_eq!(normalize_360(input).to_bits(), 0.0_f64.to_bits(), "input {input}");
            let mut s = source(Some(snapshot(&[(SLOT_YAW, input), (SLOT_DIRECTION_INDICATOR, input)])));
            assert!(s.check_at(0.0));
            let Some(LegacyValue::Float(yaw)) = s.native_channel("yaw") else { panic!("float yaw expected") };
            assert_eq!(yaw.to_bits(), 0.0_f64.to_bits(), "native input {input}");
        }
    }

    #[test]
    fn python_modulo_helpers() {
        assert_eq!(normalize_360(-1.0), 359.0);
        assert_eq!(normalize_360(720.5), 0.5);
        assert_eq!(wrapped_delta(1.0, 359.0), 2.0);
        assert_eq!(wrapped_delta(359.0, 1.0), -2.0);
    }
}
