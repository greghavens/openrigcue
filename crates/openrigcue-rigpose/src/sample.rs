//! The rig pose as published: six values in a fixed order and fixed units, their rates, and the
//! one place that defines how they turn into a rigid transform.

use crate::math::{Pose, Quat, Vec3};

/// Names of the values, in the order of [`RigValues::as_array`].
pub const AXES: [&str; 6] = ["sway", "surge", "heave", "yaw", "roll", "pitch"];

/// The six rig values, in published order.
///
/// Translations are millimetres in the rig-local frame: `sway` + right, `surge` + forward, `heave` + up.
/// Rotations are degrees: `pitch` + nose up, `roll` + right side down, `yaw` + nose left
/// (counter-clockwise seen from above). See [`RigValues::transform`] for the exact composition.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RigValues {
    pub sway: f64,
    pub surge: f64,
    pub heave: f64,
    pub yaw: f64,
    pub roll: f64,
    pub pitch: f64,
}

impl RigValues {
    pub const ZERO: RigValues = RigValues { sway: 0.0, surge: 0.0, heave: 0.0, yaw: 0.0, roll: 0.0, pitch: 0.0 };

    pub fn as_array(&self) -> [f64; 6] {
        [self.sway, self.surge, self.heave, self.yaw, self.roll, self.pitch]
    }

    pub fn from_array(a: [f64; 6]) -> Self {
        RigValues { sway: a[0], surge: a[1], heave: a[2], yaw: a[3], roll: a[4], pitch: a[5] }
    }

    /// True when every value is finite.
    pub fn is_finite(&self) -> bool {
        self.as_array().iter().all(|v| v.is_finite())
    }

    /// `self + rate · dt` per axis.
    pub fn advanced(&self, rates: &RigValues, dt_s: f64) -> RigValues {
        let (v, r) = (self.as_array(), rates.as_array());
        RigValues::from_array(std::array::from_fn(|i| v[i] + r[i] * dt_s))
    }

    /// Only the tilt (pitch and roll): yaw and translation zeroed.
    pub fn tilt_only(&self) -> RigValues {
        RigValues { roll: self.roll, pitch: self.pitch, ..RigValues::ZERO }
    }

    /// The rig-local orientation in the neutral (calibrated) rig frame:
    /// `R = Ry(yaw) · Rz(−roll) · Rx(pitch)`, i.e. pitch is applied first, then roll, then yaw.
    ///
    /// With this composition a configuration whose reverse switches were set up for a SteamVR
    /// virtual-tracker motion-compensation setup keeps its signs.
    pub fn rotation(&self) -> Quat {
        let (yaw, roll, pitch) = (self.yaw.to_radians(), self.roll.to_radians(), self.pitch.to_radians());
        (Quat::rot_y(yaw) * Quat::rot_z(-roll) * Quat::rot_x(pitch)).normalized()
    }

    /// The rig-local frame relative to the neutral rig frame, both centred on the centre of rotation:
    /// rotation [`Self::rotation`], translation `R · (sway, heave, −surge) / 1000` metres (the
    /// translation is rotated by the rig orientation).
    pub fn transform(&self) -> Pose {
        let r = self.rotation();
        let local = Vec3::new(self.sway, self.heave, -self.surge).scale(1e-3);
        Pose::new(r, r.rotate(local))
    }
}

/// Where the rig's centre of rotation is relative to the head at calibration, in metres, in the rig
/// frame levelled to the head's heading: `forward` + in front of the eyes, `down` + below, `right` + to
/// the right.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CorOffset {
    pub forward: f64,
    pub down: f64,
    pub right: f64,
}

impl CorOffset {
    /// The offset as a vector in the OpenXR axes of the rig frame.
    pub fn to_vec3(self) -> Vec3 {
        Vec3::new(self.right, -self.down, -self.forward)
    }
}

/// One published rig state.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RigSample {
    /// `CLOCK_MONOTONIC` time of the measurement, nanoseconds ([`crate::monotonic_now_ns`]).
    pub timestamp_ns: u64,
    pub values: RigValues,
    /// Per-second rates of `values` (mm/s, °/s), if the producer has them; used to predict ahead.
    pub rates: Option<RigValues>,
    pub cor_offset: CorOffset,
    /// The user's on/off switch (the `mc_switch` setting); consumers pass through when it is off.
    pub enabled: bool,
    /// Incremented by the daemon on every re-centre request; consumers re-calibrate when it changes.
    pub recenter_generation: u32,
}

/// Why a sample may not be used for compensation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unusable {
    /// The writer has never published (timestamp 0).
    NeverWritten,
    Disabled,
    /// Older than the allowed age.
    Stale {
        age_ns: u64,
    },
    /// A value is NaN or infinite.
    NotFinite,
}

impl RigSample {
    /// Nanoseconds between the measurement and `now_ns` (0 if the sample is from the future).
    pub fn age_ns(&self, now_ns: u64) -> u64 {
        now_ns.saturating_sub(self.timestamp_ns)
    }

    /// Checks that the sample is published, enabled, finite and at most `max_age_ns` old at `now_ns`.
    pub fn check_usable(&self, now_ns: u64, max_age_ns: u64) -> Result<(), Unusable> {
        if self.timestamp_ns == 0 {
            return Err(Unusable::NeverWritten);
        }
        if !self.enabled {
            return Err(Unusable::Disabled);
        }
        let age_ns = self.age_ns(now_ns);
        if age_ns > max_age_ns {
            return Err(Unusable::Stale { age_ns });
        }
        if !self.values.is_finite() || !self.rates.is_none_or(|r| r.is_finite()) {
            return Err(Unusable::NotFinite);
        }
        Ok(())
    }

    /// The prediction horizon in seconds from the sample to `target_ns`, clamped to `0 ..= max_horizon_ns`.
    pub fn horizon_s(&self, target_ns: u64, max_horizon_ns: u64) -> f64 {
        target_ns.saturating_sub(self.timestamp_ns).min(max_horizon_ns) as f64 * 1e-9
    }

    /// The values linearly extrapolated by `dt_s` seconds (unchanged without rates).
    pub fn extrapolated(&self, dt_s: f64) -> RigValues {
        match &self.rates {
            Some(r) => self.values.advanced(r, dt_s),
            None => self.values,
        }
    }

    /// The values predicted to `target_ns`, never more than `max_horizon_ns` ahead of the measurement.
    pub fn predict(&self, target_ns: u64, max_horizon_ns: u64) -> RigValues {
        self.extrapolated(self.horizon_s(target_ns, max_horizon_ns))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: Vec3, b: Vec3) -> bool {
        (a - b).length() < 1e-9
    }

    const FORWARD: Vec3 = Vec3::new(0.0, 0.0, -1.0);
    const RIGHT: Vec3 = Vec3::new(1.0, 0.0, 0.0);

    #[test]
    fn positive_pitch_raises_the_nose() {
        let v = RigValues { pitch: 10.0, ..RigValues::ZERO };
        let f = v.rotation().rotate(FORWARD);
        assert!(f.y > 0.17 && f.y < 0.18, "{f:?}");
    }

    #[test]
    fn positive_roll_lowers_the_right_side() {
        let v = RigValues { roll: 10.0, ..RigValues::ZERO };
        let r = v.rotation().rotate(RIGHT);
        assert!(r.y < -0.17 && r.y > -0.18, "{r:?}");
    }

    #[test]
    fn positive_yaw_turns_the_nose_left() {
        let v = RigValues { yaw: 10.0, ..RigValues::ZERO };
        let f = v.rotation().rotate(FORWARD);
        assert!(f.x < -0.17 && f.y.abs() < 1e-12, "{f:?}");
    }

    #[test]
    fn rotation_order_is_pitch_then_roll_then_yaw() {
        let v = RigValues { yaw: 20.0, roll: -15.0, pitch: 7.0, ..RigValues::ZERO };
        let expected =
            Quat::rot_y(20f64.to_radians()) * Quat::rot_z(15f64.to_radians()) * Quat::rot_x(7f64.to_radians());
        assert!(v.rotation().angle_to(expected) < 1e-12);
    }

    #[test]
    fn translation_is_millimetres_in_the_rotated_rig_frame() {
        let v = RigValues { sway: 10.0, surge: 20.0, heave: 30.0, ..RigValues::ZERO };
        assert!(close(v.transform().translation, Vec3::new(0.010, 0.030, -0.020)));
        let yawed = RigValues { yaw: 90.0, surge: 100.0, ..RigValues::ZERO };
        // Surge forward after a 90° left yaw points to −x.
        assert!(close(yawed.transform().translation, Vec3::new(-0.1, 0.0, 0.0)));
    }

    #[test]
    fn prediction_extrapolates_and_clamps() {
        let s = RigSample {
            timestamp_ns: 1_000_000_000,
            values: RigValues { pitch: 1.0, ..RigValues::ZERO },
            rates: Some(RigValues { pitch: 10.0, ..RigValues::ZERO }),
            enabled: true,
            ..RigSample::default()
        };
        assert!((s.predict(1_020_000_000, 50_000_000).pitch - 1.2).abs() < 1e-12);
        assert!((s.predict(1_200_000_000, 50_000_000).pitch - 1.5).abs() < 1e-12);
        // Never backwards.
        assert_eq!(s.predict(900_000_000, 50_000_000).pitch, 1.0);
        let no_rates = RigSample { rates: None, ..s };
        assert_eq!(no_rates.predict(1_020_000_000, 50_000_000).pitch, 1.0);
    }

    #[test]
    fn usability_checks() {
        let s = RigSample { timestamp_ns: 1_000, enabled: true, ..RigSample::default() };
        assert_eq!(s.check_usable(1_000 + 100, 100), Ok(()));
        assert_eq!(s.check_usable(1_000 + 101, 100), Err(Unusable::Stale { age_ns: 101 }));
        assert_eq!(RigSample { enabled: false, ..s }.check_usable(1_000, 100), Err(Unusable::Disabled));
        assert_eq!(RigSample { timestamp_ns: 0, ..s }.check_usable(0, 100), Err(Unusable::NeverWritten));
        let nan = RigSample { values: RigValues { yaw: f64::NAN, ..RigValues::ZERO }, ..s };
        assert_eq!(nan.check_usable(1_000, 100), Err(Unusable::NotFinite));
    }

    #[test]
    fn cor_offset_axes() {
        let o = CorOffset { forward: 0.1, down: 0.5, right: 0.02 };
        assert!(close(o.to_vec3(), Vec3::new(0.02, -0.5, -0.1)));
    }
}
