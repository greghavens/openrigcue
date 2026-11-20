//! Adaptive top speed (`max_speed`).

use std::cmp::Ordering;

use openrigcue_model::native_value::LegacyValue;
use openrigcue_model::python_compare::{native_equal, numeric_cmp};

/// Conversion factor km/h → m/s used for the profile's default top speed.
const KMH_TO_MPS: f64 = 0.277778;

/// Listeners whose top speed restarts from the profile default when the car or the track changes. `"ir"` is not a
/// listener id (iRacing is `"iracing"`), so iRacing never restarts.
const RESTART_ON_CAR_OR_TRACK: [&str; 11] = [
    "ac",
    "ir",
    "projectcars",
    "projectcars2",
    "automobilista2",
    "projectcars2udp",
    "projectcarsudp",
    "rf1",
    "rfpro",
    "rf2",
    "forza",
];

/// What the tracker sees of the active listener this tick.
#[derive(Debug, Clone, Copy)]
pub struct SpeedSample<'a> {
    /// Listener id.
    pub listener: &'a str,
    /// Raw listener speed, m/s, not clamped.
    pub speed: Option<f64>,
    /// Vehicle identifier reported by the listener.
    pub vehicle: Option<&'a str>,
    /// Track identifier reported by the listener.
    pub location: Option<&'a str>,
}

/// Highest speed seen in the current car/track session, never below the profile's default top speed.
#[derive(Debug, Clone)]
pub struct MaxSpeedTracker {
    max_speed: LegacyValue,
    vehicle: LegacyValue,
    location: LegacyValue,
    previous_default_kmh: LegacyValue,
}

impl Default for MaxSpeedTracker {
    fn default() -> Self {
        Self {
            max_speed: LegacyValue::Int(0),
            vehicle: LegacyValue::Str(String::new()),
            location: LegacyValue::Str(String::new()),
            previous_default_kmh: LegacyValue::Int(0),
        }
    }
}

impl MaxSpeedTracker {
    /// Folds one tick into the tracker and returns the top speed in whole m/s.
    ///
    /// Returns `None` (tracker untouched) when the listener cannot provide a finite speed, which
    /// ends the dispatcher's pass.
    pub fn update(&mut self, sample: SpeedSample<'_>, default_max_speed_kmh: f64) -> Option<f64> {
        let speed = crate::udp_racing::native_int(sample.speed?)?;
        // The compatibility sample has no failed-read information; absent names leave
        // the identity unchanged. Typed dispatch uses update_native's actual read callbacks.
        let listener = if sample.vehicle.is_some() && sample.location.is_some() { sample.listener } else { "" };
        self.update_native(listener, speed, LegacyValue::Float(default_max_speed_kmh), |name| {
            match name {
                "track_loc" => sample.location,
                _ => sample.vehicle,
            }
            .map(|text| LegacyValue::Str(text.into()))
        })
        .and_then(|value| value.number())
    }

    pub(crate) fn tracks_identity(listener: &str) -> bool {
        RESTART_ON_CAR_OR_TRACK.contains(&listener)
    }

    /// The update after the speed has become an integer. Identity conditions and refreshes perform their own reads;
    /// a failure leaves assignments already completed inside this helper intact.
    pub(crate) fn update_native(
        &mut self,
        listener: &str,
        speed: LegacyValue,
        default_max_speed_kmh: LegacyValue,
        mut identity: impl FnMut(&'static str) -> Option<LegacyValue>,
    ) -> Option<LegacyValue> {
        speed.number().filter(|value| value.is_finite())?;
        let default_max =
            crate::udp_racing::native_int((default_max_speed_kmh.number()? * KMH_TO_MPS).round_ties_even())?;
        if !native_equal(&default_max_speed_kmh, &self.previous_default_kmh) {
            self.max_speed = default_max.clone();
        }
        if Self::tracks_identity(listener)
            && (!native_equal(&self.location, &identity("track_loc")?)
                || !native_equal(&self.vehicle, &identity("car_name")?))
        {
            self.max_speed = default_max.clone();
            self.location = identity("track_loc")?;
            self.vehicle = identity("car_name")?;
        }
        if numeric_cmp(&speed, &self.max_speed).ok()? == Some(Ordering::Greater) {
            self.max_speed = speed;
        }
        if numeric_cmp(&self.max_speed, &default_max).ok()? == Some(Ordering::Less) {
            self.max_speed = default_max;
        }
        self.previous_default_kmh = default_max_speed_kmh;
        Some(self.max_speed.clone())
    }

    /// Clears the top speed when telemetry is lost (stand-by transition).
    pub fn reset(&mut self) {
        self.max_speed = LegacyValue::Int(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_identity_condition_short_circuit_and_refresh_have_separate_reads() {
        use LegacyValue::{Int, Str};
        let mut tracker = MaxSpeedTracker::default();
        assert_eq!(
            tracker.update_native("api", Int(50), Int(0), |_| panic!("unlisted identity lookup")),
            Some(Int(50))
        );
        let mut calls = Vec::new();
        assert_eq!(
            tracker.update_native("rf1", Int(5), Int(0), |name| {
                calls.push(name);
                Some(Str(if name == "track_loc" { "track" } else { "car" }.into()))
            }),
            Some(Int(5))
        );
        assert_eq!(calls, ["track_loc", "track_loc", "car_name"]);
        calls.clear();
        assert_eq!(
            tracker.update_native("rf1", Int(7), Int(0), |name| {
                calls.push(name);
                Some(Str(if name == "track_loc" { "track" } else { "car" }.into()))
            }),
            Some(Int(7))
        );
        assert_eq!(calls, ["track_loc", "car_name"]);
    }

    #[test]
    fn native_identity_failure_retains_the_prior_reset_and_completed_refresh() {
        use LegacyValue::{Int, Str};
        let mut tracker = MaxSpeedTracker::default();
        tracker.update_native("api", Int(500), Int(0), |_| panic!("unlisted identity lookup")).unwrap();
        let mut calls = Vec::new();
        assert_eq!(
            tracker.update_native("rf1", Int(1000), Int(100), |name| {
                calls.push(name);
                match calls.len() {
                    1 => Some(Str("condition track".into())),
                    2 => Some(Str("refreshed track".into())),
                    _ => None,
                }
            }),
            None
        );
        assert_eq!(calls, ["track_loc", "track_loc", "car_name"]);
        // previous_default is assigned only at successful return, so default 0 does not erase the failed call's reset.
        assert_eq!(tracker.update_native("api", Int(1), Int(0), |_| panic!("unlisted identity lookup")), Some(Int(28)));
        calls.clear();
        tracker
            .update_native("rf1", Int(2), Int(0), |name| {
                calls.push(name);
                Some(Str(if name == "track_loc" { "refreshed track" } else { "car" }.into()))
            })
            .unwrap();
        assert_eq!(
            calls,
            ["track_loc", "car_name", "track_loc", "car_name"],
            "the completed location refresh survives failure of the later car read"
        );
    }

    #[test]
    fn native_identity_and_maximum_keep_integer_kinds_and_precision() {
        use LegacyValue::{Int, Str};
        let mut tracker = MaxSpeedTracker::default();
        assert_eq!(
            tracker.update_native("api", Int(9_007_199_254_740_993), Int(0), |_| None),
            Some(Int(9_007_199_254_740_993))
        );
        let mut calls = Vec::new();
        assert_eq!(
            tracker.update_native("rf1", Int(4), Int(0), |name| {
                calls.push(name);
                Some(Int(0))
            }),
            Some(Int(4))
        );
        assert_eq!(calls, ["track_loc", "track_loc", "car_name"], "integer zero differs from the initial empty string");
        calls.clear();
        tracker
            .update_native("rf1", Int(3), Int(0), |name| {
                calls.push(name);
                Some(Str(String::new()))
            })
            .unwrap();
        assert_eq!(calls, ["track_loc", "track_loc", "car_name"]);
    }

    fn sample<'a>(listener: &'a str, speed: f64, vehicle: &'a str, location: &'a str) -> SpeedSample<'a> {
        SpeedSample { listener, speed: Some(speed), vehicle: Some(vehicle), location: Some(location) }
    }

    /// Default 200 km/h → round(55.5556) = 56 m/s floor, raised by faster samples.
    #[test]
    fn floor_and_growth() {
        let mut tracker = MaxSpeedTracker::default();
        assert_eq!(tracker.update(sample("dirt", 30.9, "", ""), 200.0), Some(56.0));
        assert_eq!(tracker.update(sample("dirt", 70.7, "", ""), 200.0), Some(70.0));
        assert_eq!(tracker.update(sample("dirt", 10.0, "", ""), 200.0), Some(70.0));
    }

    /// Rounding: round(90 · 0.277778 = 25.00002) = 25 and round(9 · 0.277778 = 2.500002) = 3.
    #[test]
    fn default_is_rounded_to_whole_mps() {
        let mut tracker = MaxSpeedTracker::default();
        assert_eq!(tracker.update(sample("dirt", 0.0, "", ""), 90.0), Some(25.0));
        let mut tracker = MaxSpeedTracker::default();
        assert_eq!(tracker.update(sample("dirt", 0.0, "", ""), 9.0), Some(3.0));
    }

    /// A changed profile default restarts the session maximum.
    #[test]
    fn default_change_restarts() {
        let mut tracker = MaxSpeedTracker::default();
        tracker.update(sample("dirt", 90.0, "", ""), 200.0);
        assert_eq!(tracker.update(sample("dirt", 10.0, "", ""), 100.0), Some(28.0));
    }

    /// A car/track change restarts only for the listed listeners ("iracing" is not listed).
    #[test]
    fn car_or_track_change_restarts_listed_listeners() {
        let mut ac = MaxSpeedTracker::default();
        ac.update(sample("ac", 90.0, "car1", "monza"), 200.0);
        assert_eq!(ac.update(sample("ac", 10.0, "car1", "monza"), 200.0), Some(90.0));
        assert_eq!(ac.update(sample("ac", 10.0, "car2", "monza"), 200.0), Some(56.0));

        let mut iracing = MaxSpeedTracker::default();
        iracing.update(sample("iracing", 90.0, "car1", "monza"), 200.0);
        assert_eq!(iracing.update(sample("iracing", 10.0, "car2", "spa"), 200.0), Some(90.0));
    }

    /// Stand-by sets max_speed = 0; the next sample starts from the floor again.
    #[test]
    fn reset_clears_maximum() {
        let mut tracker = MaxSpeedTracker::default();
        tracker.update(sample("dirt", 90.0, "", ""), 200.0);
        tracker.reset();
        assert_eq!(tracker.update(sample("dirt", 10.0, "", ""), 200.0), Some(56.0));
    }

    /// The update fails when the listener has no speed: nothing changes.
    #[test]
    fn missing_speed_leaves_tracker_untouched() {
        let mut tracker = MaxSpeedTracker::default();
        let missing = SpeedSample { listener: "dirt", speed: None, vehicle: None, location: None };
        assert_eq!(tracker.update(missing, 200.0), None);
        assert_eq!(tracker.update(sample("dirt", 0.0, "", ""), 200.0), Some(56.0));
    }
}
