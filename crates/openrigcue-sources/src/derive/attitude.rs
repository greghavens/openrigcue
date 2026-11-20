//! Yaw, heading, pitch and roll selection.

use super::listener::{Listener, Rate};

/// Whether the profile feeds pitch (or roll) from the angle or from the rate channel
/// (`motion_effects.pitch_type` / `roll_type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttitudeSource {
    Angle,
    Rate,
}

impl AttitudeSource {
    /// Maps the profile value: missing or `"angle"` selects the angle, anything else the rate.
    pub fn from_profile(value: Option<&str>) -> Self {
        match value {
            None | Some("angle") => AttitudeSource::Angle,
            Some(_) => AttitudeSource::Rate,
        }
    }
}

/// `yaw` and `yaw_angle` of one tick.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Heading {
    pub yaw: f64,
    pub yaw_angle: f64,
}

/// Yaw for the active game: flying games publish the yaw rate as `yaw` (falling back to the yaw angle) and the
/// raw yaw as `yaw_angle`; other games publish the yaw angle and its 0…360 heading.
pub fn heading(listener: &dyn Listener, flying: bool) -> Heading {
    let yaw = listener.channel("yaw");
    if flying {
        match listener.rate(Rate::Yaw).or(yaw) {
            Some(rate) => Heading { yaw: rate, yaw_angle: yaw.unwrap_or(0.0) },
            None => Heading { yaw: 0.0, yaw_angle: 0.0 },
        }
    } else {
        let yaw_angle = yaw.map(|angle| if angle >= 0.0 { angle } else { 360.0 + angle });
        Heading { yaw: yaw.unwrap_or(0.0), yaw_angle: yaw_angle.unwrap_or(0.0) }
    }
}

/// Pitch from the profile-selected channel, 0 when unavailable.
pub fn pitch(listener: &dyn Listener, source: AttitudeSource) -> f64 {
    select(listener, source, "pitch", Rate::Pitch)
}

/// Roll from the profile-selected channel, 0 when unavailable.
pub fn roll(listener: &dyn Listener, source: AttitudeSource) -> f64 {
    select(listener, source, "roll", Rate::Roll)
}

fn select(listener: &dyn Listener, source: AttitudeSource, angle: &str, rate: Rate) -> f64 {
    let value = match source {
        AttitudeSource::Angle => listener.channel(angle),
        AttitudeSource::Rate => listener.rate(rate),
    };
    value.unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::derive::testing::FakeListener;

    /// Yaw, non-flying: heading normalised to 0…360, missing yaw → 0/0.
    #[test]
    fn ground_heading() {
        let mut fake = FakeListener::new("dirt");
        fake.set("yaw", -90.0);
        assert_eq!(heading(&fake, false), Heading { yaw: -90.0, yaw_angle: 270.0 });
        fake.set("yaw", 0.0);
        assert_eq!(heading(&fake, false), Heading { yaw: 0.0, yaw_angle: 0.0 });
        let empty = FakeListener::new("dirt");
        assert_eq!(heading(&empty, false), Heading { yaw: 0.0, yaw_angle: 0.0 });
    }

    /// Yaw, flying: yaw rate first, yaw fallback, raw yaw angle, both missing → 0/0.
    #[test]
    fn flying_heading() {
        let mut fake = FakeListener::new("xplane");
        fake.set("yaw", -90.0);
        assert_eq!(heading(&fake, true), Heading { yaw: -90.0, yaw_angle: -90.0 });
        fake.set_rate(Rate::Yaw, 3.0);
        assert_eq!(heading(&fake, true), Heading { yaw: 3.0, yaw_angle: -90.0 });
        let mut rate_only = FakeListener::new("xplane");
        rate_only.set_rate(Rate::Yaw, 3.0);
        assert_eq!(heading(&rate_only, true), Heading { yaw: 3.0, yaw_angle: 0.0 });
        assert_eq!(heading(&FakeListener::new("xplane"), true), Heading { yaw: 0.0, yaw_angle: 0.0 });
    }

    /// Pitch and roll: angle by default, rate for any other profile value, missing → 0.
    #[test]
    fn pitch_and_roll_sources() {
        let mut fake = FakeListener::new("dirt");
        fake.set("pitch", 4.0);
        fake.set("roll", -2.0);
        fake.set_rate(Rate::Pitch, 40.0);
        let angle = AttitudeSource::from_profile(None);
        let rate = AttitudeSource::from_profile(Some("rate"));
        assert_eq!(angle, AttitudeSource::from_profile(Some("angle")));
        assert_eq!(pitch(&fake, angle), 4.0);
        assert_eq!(roll(&fake, angle), -2.0);
        assert_eq!(pitch(&fake, rate), 40.0);
        assert_eq!(roll(&fake, rate), 0.0);
    }
}
