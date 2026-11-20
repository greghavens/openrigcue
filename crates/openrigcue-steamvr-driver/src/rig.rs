//! Rig pose input as the compensator sees it, and its prediction to a pose time.
//!
//! The samples, their units and the axis convention come from the `openrigcue-rigpose` contract:
//! [`RigValues::transform`] is the platform frame relative to the neutral rig frame, both centred
//! on the centre of rotation (CoR), in OpenVR/OpenXR axes (+x right, +y up, +z back). A head fixed
//! to the platform at `h` (platform coordinates) is therefore at `base ∘ rig(t) ∘ h` in the room.

use std::sync::Mutex;

use openrigcue_rigpose::RateEstimator;
pub use openrigcue_rigpose::{CorOffset, RigSample, RigValues};

use crate::math::{Quat, Rigid, Vec3};
use crate::seqlock::{SeqCell, Words};

/// What the shared rig pose says right now.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RigReading {
    /// No publisher (file missing, wrong layout, or unreadable).
    Absent,
    /// Another consumer holds the active-consumer claim; this driver must pass through.
    ClaimedByOther,
    /// The latest published sample (it may still be disabled, never written or stale).
    Live(RigSample),
}

/// A lock-free, allocation-free source of rig poses (called from `vrserver` pose threads).
pub trait RigSource: Send + Sync {
    fn read(&self) -> RigReading;
}

impl<T: RigSource + ?Sized> RigSource for &T {
    fn read(&self) -> RigReading {
        (**self).read()
    }
}

/// The rig pose given by the contract's convention, as this crate's transform type.
pub fn rig_transform(values: &RigValues) -> Rigid {
    let p = values.transform();
    let q = p.rotation;
    let t = p.translation;
    Rigid::new(Quat::new(q.w, q.x, q.y, q.z).normalized(), Vec3::new(t.x, t.y, t.z))
}

/// The head-to-CoR vector of the contract's offset, in seat coordinates (OpenVR axes).
pub fn cor_vector(offset: CorOffset) -> Vec3 {
    let v = offset.to_vec3();
    Vec3::new(v.x, v.y, v.z)
}

/// A sample together with the per-second rates used to extrapolate it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RigMotion {
    pub sample: RigSample,
    pub rates: RigValues,
}

impl RigMotion {
    /// The rig transform `dt_s` seconds after the measurement (negative = before), with `dt_s`
    /// clamped to `±max_s`.
    pub fn transform_at(&self, dt_s: f64, max_s: f64) -> Rigid {
        let dt = dt_s.clamp(-max_s, max_s);
        rig_transform(&self.sample.values.advanced(&self.rates, dt))
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct EstimatedRates {
    timestamp_ns: u64,
    rates: Option<RigValues>,
}

impl Words<8> for EstimatedRates {
    fn to_words(&self) -> [u64; 8] {
        let r = self.rates.unwrap_or_default().as_array();
        let mut w = [0u64; 8];
        w[0] = self.timestamp_ns;
        w[1] = u64::from(self.rates.is_some());
        for (o, x) in w[2..].iter_mut().zip(r) {
            *o = x.to_bits();
        }
        w
    }

    fn from_words(w: &[u64; 8]) -> Self {
        let r: [f64; 6] = std::array::from_fn(|i| f64::from_bits(w[2 + i]));
        EstimatedRates { timestamp_ns: w[0], rates: (w[1] == 1).then(|| RigValues::from_array(r)) }
    }
}

/// Supplies rates for samples that carry none, using the contract's [`RateEstimator`] on the
/// sequence of distinct samples. Shared by all pose threads: the estimator sits behind a
/// `try_lock` that never blocks (a thread that loses uses the last published estimate).
pub struct Predictor {
    estimator: Mutex<(RateEstimator, u64)>,
    published: SeqCell<8>,
}

impl Predictor {
    pub fn new(smoothing_s: f64) -> Self {
        Predictor { estimator: Mutex::new((RateEstimator::new(smoothing_s), 0)), published: SeqCell::new() }
    }

    pub fn observe(&self, sample: RigSample) -> RigMotion {
        if let Some(rates) = sample.rates {
            return RigMotion { sample, rates };
        }
        let cached: Option<EstimatedRates> = self.published.load();
        let fresh = match cached {
            Some(c) if c.timestamp_ns == sample.timestamp_ns => c,
            _ => match self.estimator.try_lock() {
                Ok(mut guard) => {
                    let (estimator, last_ns) = &mut *guard;
                    if *last_ns != sample.timestamp_ns {
                        *last_ns = sample.timestamp_ns;
                        let next = EstimatedRates {
                            timestamp_ns: sample.timestamp_ns,
                            rates: estimator.update(sample.timestamp_ns, sample.values),
                        };
                        self.published.store(&next);
                        next
                    } else {
                        cached.unwrap_or(EstimatedRates { timestamp_ns: sample.timestamp_ns, rates: None })
                    }
                }
                Err(_) => cached.unwrap_or(EstimatedRates { timestamp_ns: sample.timestamp_ns, rates: None }),
            },
        };
        RigMotion { sample, rates: fresh.rates.unwrap_or_default() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::tests::{assert_quat_near, assert_vec_near};

    fn sample(t_ms: u64, pitch: f64) -> RigSample {
        RigSample {
            timestamp_ns: t_ms * 1_000_000,
            values: RigValues { pitch, ..RigValues::ZERO },
            enabled: true,
            ..RigSample::default()
        }
    }

    #[test]
    fn contract_convention_is_used() {
        // +pitch raises the nose: forward (−z) gains +y.
        let r = rig_transform(&RigValues { pitch: 10.0, ..RigValues::ZERO });
        assert_quat_near(r.rot, Quat::rot_x(10f64.to_radians()), 1e-12);
        let s = rig_transform(&RigValues { heave: 30.0, ..RigValues::ZERO });
        assert_vec_near(s.pos, Vec3::new(0.0, 0.03, 0.0), 1e-12);
    }

    #[test]
    fn publisher_rates_are_used_as_given() {
        let p = Predictor::new(0.0);
        let mut s = sample(100, 1.0);
        s.rates = Some(RigValues { pitch: 100.0, ..RigValues::ZERO });
        let m = p.observe(s);
        assert_quat_near(m.transform_at(0.01, 0.05).rot, Quat::rot_x(2f64.to_radians()), 1e-12);
        // Clamped to the maximum horizon.
        assert_quat_near(m.transform_at(1.0, 0.05).rot, Quat::rot_x(6f64.to_radians()), 1e-12);
    }

    #[test]
    fn rates_are_estimated_when_missing() {
        let p = Predictor::new(0.0);
        assert_eq!(p.observe(sample(100, 1.0)).rates, RigValues::ZERO);
        let m = p.observe(sample(110, 2.0));
        assert!((m.rates.pitch - 100.0).abs() < 1e-9, "{:?}", m.rates);
        // The same sample seen again from another pose thread reuses the estimate.
        assert_eq!(p.observe(sample(110, 2.0)).rates, m.rates);
        assert_quat_near(m.transform_at(0.02, 0.05).rot, Quat::rot_x(4f64.to_radians()), 1e-12);
    }
}
