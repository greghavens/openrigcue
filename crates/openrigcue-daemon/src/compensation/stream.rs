//! Turns compensation passes into rig-pose samples: values, rates for the consumers' prediction, the
//! enabled flag, the centre-of-rotation offset and the re-centre generation.

use std::time::Duration;

use openrigcue_effects::accessories::compensation::{CompensationError, CompensationInputs, PoseTracker, RigPose};
use openrigcue_effects::accessories::imu::ImuSample;
use openrigcue_rigpose::{CorOffset, RateEstimator, RigSample, RigValues};

/// Smoothing of the finite-difference rates (sway, surge, heave, yaw, and pitch/roll without gyro data): about
/// a quarter of the JY901's 42 Hz filter period, so it adds little lag at 200 Hz.
pub const RATE_SMOOTHING_S: f64 = 0.005;
/// A gyro reading older than this, relative to the angles it would go with, is not used.
pub const GYRO_MAX_SKEW: Duration = Duration::from_millis(50);

/// What one pass starts from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reading {
    /// `CLOCK_MONOTONIC` time of the measurement (ns).
    pub timestamp_ns: u64,
    /// The IMU angles in `'W'` mode, `None` without a sensor (or in `'V'` mode).
    pub angles: Option<[f64; 3]>,
    /// The IMU's angular velocity (°/s) measured with those angles.
    pub gyro: Option<[f64; 3]>,
}

impl Reading {
    /// A pass without sensor data (IntelliComp, or no IMU connected) at `now_ns`.
    pub fn without_sensor(now_ns: u64) -> Reading {
        Reading { timestamp_ns: now_ns, angles: None, gyro: None }
    }

    /// An IMU reading; `now_ns` and `now` are the same moment on `CLOCK_MONOTONIC` and as an `Instant`.
    pub fn from_imu(sample: &ImuSample, now_ns: u64, now: std::time::Instant) -> Reading {
        let age = now.saturating_duration_since(sample.received);
        let gyro = sample
            .rates
            .filter(|(_, at)| {
                sample.received.saturating_duration_since(*at) <= GYRO_MAX_SKEW
                    && at.saturating_duration_since(sample.received) <= GYRO_MAX_SKEW
            })
            .map(|(rates, _)| rates);
        Reading {
            timestamp_ns: now_ns.saturating_sub(u64::try_from(age.as_nanos()).unwrap_or(u64::MAX)),
            angles: Some(sample.angles),
            gyro,
        }
    }
}

/// What the sample carries besides the pass.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Framing {
    pub cor_offset: CorOffset,
    pub recenter_generation: u32,
}

fn values_of(pose: &RigPose) -> RigValues {
    RigValues::from_array(pose.fields())
}

/// The rig-pose producer: one [`PoseTracker`] pass per reading, plus rates.
#[derive(Debug)]
pub struct PoseStream {
    tracker: PoseTracker,
    rates: RateEstimator,
}

impl Default for PoseStream {
    fn default() -> Self {
        PoseStream { tracker: PoseTracker::new(), rates: RateEstimator::new(RATE_SMOOTHING_S) }
    }
}

impl PoseStream {
    pub fn new() -> Self {
        Self::default()
    }

    /// One sample. Outside a compensation session (`'W'` without a sensor) the sample is neutral and
    /// disabled, so the consumers pass through at once; in a session it is enabled while `mc_switch` is on.
    pub fn next(
        &mut self,
        reading: &Reading,
        inputs: &CompensationInputs<'_>,
        framing: Framing,
    ) -> Result<RigSample, CompensationError> {
        let tracked = self.tracker.update(reading.angles, reading.gyro, inputs)?;
        let base = RigSample {
            timestamp_ns: reading.timestamp_ns,
            cor_offset: framing.cor_offset,
            recenter_generation: framing.recenter_generation,
            ..RigSample::default()
        };
        let Some(tracked) = tracked else {
            self.rates = RateEstimator::new(RATE_SMOOTHING_S);
            return Ok(RigSample { values: RigValues::ZERO, rates: None, enabled: false, ..base });
        };
        let values = values_of(&tracked.pose);
        let estimated = self.rates.update(reading.timestamp_ns, values);
        let rates = match tracked.tilt_rates {
            Some(tilt) => {
                Some(RigValues { pitch: tilt.pitch, roll: tilt.roll, ..estimated.unwrap_or(RigValues::ZERO) })
            }
            None => estimated,
        };
        Ok(RigSample { values, rates, enabled: inputs.switch_on, ..base })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrigcue_effects::accessories::compensation::{
        ActuatorPositions, AxisSettings, CompensationKind, CompensationSettings, HorizontalSurgeSettings, Rig, RigModel,
    };
    use openrigcue_effects::accessories::imu::InstallationReader;
    use openrigcue_effects::accessories::rig_geometry::Corners;

    fn settings(kind: CompensationKind) -> CompensationSettings {
        let on = AxisSettings { enabled: true, reverse: false, offset: 0.0 };
        CompensationSettings {
            kind,
            pitch: on,
            roll: AxisSettings { reverse: true, ..on },
            sway: on,
            surge: on,
            yaw: on,
            heave: on,
            swap_roll_pitch: false,
            pitch_factor: 100,
            roll_factor: 100,
            yaw_factor: 100,
            estimator_factor_failures: [false; 3],
            native_estimator_factors: None,
            surge_factor: 100,
            sway_factor: 100,
            height: 1000.0,
            actuator_spacing_mm: 558,
            horizontal_surge: HorizontalSurgeSettings { stroke_mm: 100, reverse: false, factor_percent: 0 },
            refresh_rate: 90,
        }
    }

    const RIG: Rig = Rig { model: RigModel::Other, dof_reality: Some(false), horizontal_surge: None };
    const POSITIONS: ActuatorPositions = ActuatorPositions {
        corners: Corners { front_left: 0.0, front_right: 0.0, rear_left: 0.0, rear_right: 0.0 },
        motors: None,
    };

    fn inputs<'a>(s: &'a CompensationSettings, switch_on: bool) -> CompensationInputs<'a> {
        CompensationInputs {
            native_kind: None,
            settings: s,
            native_displacement: None,
            native_refresh_rate: None,
            native_horizontal_surge: None,
            native_axis_enabled: None,
            native_axis_controls: None,
            switch_on,
            rig: &RIG,
            actuators: &POSITIONS,
            heave_from_actuators: true,
            game_running: None,
            installation: horizontal(),
            native_installation: None,
        }
    }

    fn horizontal() -> &'static InstallationReader {
        static READER: std::sync::OnceLock<InstallationReader> = std::sync::OnceLock::new();
        READER.get_or_init(|| std::sync::Arc::new(|| "H".to_owned()))
    }

    const FRAMING: Framing =
        Framing { cor_offset: CorOffset { forward: 0.1, down: 1.0, right: 0.0 }, recenter_generation: 7 };

    /// One sample per IMU reading in a compensation session: the values in stream order and units (pitch
    /// from the first angle, roll reversed from the second, surge/sway from tan(tilt) * height / 100), gyro rates on the tilts,
    /// estimated rates on the rest, the CoR and generation, enabled with mc_switch.
    #[test]
    fn sensor_readings_become_samples() {
        let s = settings(CompensationKind::Sensor);
        let mut stream = PoseStream::new();
        let first =
            Reading { timestamp_ns: 1_000_000_000, angles: Some([2.0, 1.0, 0.0]), gyro: Some([10.0, 4.0, 0.0]) };
        let a = stream.next(&first, &inputs(&s, true), FRAMING).unwrap();
        assert!(a.enabled);
        assert_eq!((a.values.pitch, a.values.roll), (2.0, -1.0));
        assert_eq!(a.values.surge, 2f64.to_radians().tan() * 1000.0 * 100.0 / 100.0);
        assert_eq!(a.values.sway, (-1f64).to_radians().tan() * 1000.0 * 100.0 / 100.0);
        assert_eq!(
            a.rates,
            Some(RigValues { pitch: 10.0, roll: -4.0, ..RigValues::ZERO }),
            "gyro only until two readings"
        );
        assert_eq!((a.cor_offset, a.recenter_generation, a.timestamp_ns), (FRAMING.cor_offset, 7, 1_000_000_000));
        let second = Reading { timestamp_ns: 1_005_000_000, angles: Some([2.1, 1.0, 0.0]), gyro: None };
        let b = stream.next(&second, &inputs(&s, true), FRAMING).unwrap();
        let rates = b.rates.unwrap();
        let surge_rate = (b.values.surge - a.values.surge) / 0.005;
        assert!((rates.surge - surge_rate).abs() < 1e-6, "the first estimate is the raw difference: {rates:?}");
        assert!((rates.pitch - 20.0).abs() < 1e-6, "no gyro: the finite difference of pitch");
        let third = Reading { timestamp_ns: 1_010_000_000, angles: Some([2.1, 1.0, 0.0]), gyro: None };
        let c = stream.next(&third, &inputs(&s, true), FRAMING).unwrap();
        let alpha = 0.005 / (RATE_SMOOTHING_S + 0.005);
        assert!((c.rates.unwrap().pitch - 20.0 * (1.0 - alpha)).abs() < 1e-6, "then smoothed");
        let off = stream.next(&third, &inputs(&s, false), FRAMING).unwrap();
        assert!(!off.enabled);
        assert_eq!(off.values, RigValues::ZERO);
    }

    /// Without a sensor in 'W' mode there is no session: a neutral, disabled sample without rates.
    /// IntelliComp ('V') runs from the actuators without one.
    #[test]
    fn no_session_without_a_sensor() {
        let s = settings(CompensationKind::Sensor);
        let mut stream = PoseStream::new();
        let idle = stream.next(&Reading::without_sensor(5), &inputs(&s, true), FRAMING).unwrap();
        assert_eq!((idle.enabled, idle.values, idle.rates, idle.timestamp_ns), (false, RigValues::ZERO, None, 5));
        let v = settings(CompensationKind::Virtual);
        let sample = stream.next(&Reading::without_sensor(6), &inputs(&v, true), FRAMING).unwrap();
        assert!(sample.enabled);
        assert_eq!(sample.rates, None);
    }

    /// The reading's time is the parser's decode time; a gyro packet too far from the angles is dropped.
    #[test]
    fn imu_reading_times_and_gyro_skew() {
        let now = std::time::Instant::now();
        let received = now - Duration::from_millis(3);
        let sample =
            ImuSample { sequence: 4, angles: [1.0, 2.0, 3.0], received, rates: Some(([5.0, 6.0, 7.0], received)) };
        let reading = Reading::from_imu(&sample, 10_000_000_000, now);
        assert_eq!(reading.timestamp_ns, 10_000_000_000 - 3_000_000);
        assert_eq!(reading.gyro, Some([5.0, 6.0, 7.0]));
        let stale = ImuSample { rates: Some(([5.0, 6.0, 7.0], received - Duration::from_millis(51))), ..sample };
        assert_eq!(Reading::from_imu(&stale, 10_000_000_000, now).gyro, None);
    }
}
