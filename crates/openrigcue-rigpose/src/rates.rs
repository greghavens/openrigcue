//! Rate estimation for producers that have only positions (the IMU path publishes angles).

use crate::sample::RigValues;

/// Finite-difference rates of successive rig values with exponential smoothing.
///
/// Angles (yaw, roll, pitch) are differenced across the ±180° wrap. A gap longer than
/// [`RateEstimator::MAX_GAP_S`] or a non-increasing timestamp restarts the estimate.
#[derive(Debug, Clone)]
pub struct RateEstimator {
    tau_s: f64,
    last: Option<(u64, RigValues)>,
    rates: Option<RigValues>,
}

impl RateEstimator {
    pub const MAX_GAP_S: f64 = 0.25;

    /// `tau_s` is the smoothing time constant (0 = raw differences).
    pub fn new(tau_s: f64) -> Self {
        RateEstimator { tau_s: tau_s.max(0.0), last: None, rates: None }
    }

    /// Feeds one measurement and returns the current rate estimate (`None` until two measurements).
    pub fn update(&mut self, timestamp_ns: u64, values: RigValues) -> Option<RigValues> {
        let prev = self.last.replace((timestamp_ns, values));
        let Some((t0, v0)) = prev else {
            self.rates = None;
            return None;
        };
        let dt = timestamp_ns.saturating_sub(t0) as f64 * 1e-9;
        if timestamp_ns <= t0 || dt > Self::MAX_GAP_S {
            self.rates = None;
            return None;
        }
        let (a, b) = (v0.as_array(), values.as_array());
        let raw: [f64; 6] = std::array::from_fn(|i| {
            let d = if i >= 3 { wrap_degrees(b[i] - a[i]) } else { b[i] - a[i] };
            d / dt
        });
        let alpha = dt / (self.tau_s + dt);
        let next = match self.rates {
            Some(r) => {
                let r = r.as_array();
                std::array::from_fn(|i| r[i] + alpha * (raw[i] - r[i]))
            }
            None => raw,
        };
        self.rates = Some(RigValues::from_array(next));
        self.rates
    }
}

/// `d` wrapped into (−180, 180].
fn wrap_degrees(d: f64) -> f64 {
    let w = (d + 180.0).rem_euclid(360.0) - 180.0;
    if w == -180.0 {
        180.0
    } else {
        w
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_rate_is_recovered() {
        let mut e = RateEstimator::new(0.02);
        assert_eq!(e.update(0, RigValues::ZERO), None);
        let mut last = None;
        for i in 1..50u64 {
            let t = i * 5_000_000; // 200 Hz
            let s = t as f64 * 1e-9;
            last = e.update(t, RigValues { pitch: 10.0 * s, heave: -30.0 * s, ..RigValues::ZERO });
        }
        let r = last.unwrap();
        assert!((r.pitch - 10.0).abs() < 1e-6 && (r.heave + 30.0).abs() < 1e-6, "{r:?}");
    }

    #[test]
    fn yaw_wraps() {
        let mut e = RateEstimator::new(0.0);
        e.update(0, RigValues { yaw: 179.0, ..RigValues::ZERO });
        let r = e.update(10_000_000, RigValues { yaw: -179.0, ..RigValues::ZERO }).unwrap();
        assert!((r.yaw - 200.0).abs() < 1e-9, "{r:?}");
        assert_eq!(wrap_degrees(180.0), 180.0);
        assert_eq!(wrap_degrees(-180.0), 180.0);
        assert!((wrap_degrees(-190.0) - 170.0).abs() < 1e-12);
    }

    #[test]
    fn gaps_and_time_reversal_restart() {
        let mut e = RateEstimator::new(0.0);
        e.update(0, RigValues::ZERO);
        assert_eq!(e.update(300_000_000, RigValues::ZERO), None);
        assert!(e.update(305_000_000, RigValues::ZERO).is_some());
        assert_eq!(e.update(305_000_000, RigValues::ZERO), None);
    }
}
