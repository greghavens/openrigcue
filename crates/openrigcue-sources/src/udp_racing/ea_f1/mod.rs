//! Codemasters / EA F1 2018 to F1 25 UDP listeners (`f12018` … `f12025`).

mod layout;
mod names;
pub mod season2018;
pub mod seasons;

use openrigcue_model::Telemetry;

/// A wheel and its index in the games' player arrays (`lr` 0, `rr` 1, `lf` 2, `rf` 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wheel {
    RearLeft,
    RearRight,
    FrontLeft,
    FrontRight,
}

const WHEELS: [Wheel; 4] = [Wheel::RearLeft, Wheel::RearRight, Wheel::FrontLeft, Wheel::FrontRight];

impl Wheel {
    fn index(self) -> usize {
        match self {
            Wheel::RearLeft => 0,
            Wheel::RearRight => 1,
            Wheel::FrontLeft => 2,
            Wheel::FrontRight => 3,
        }
    }

    /// This wheel's `(suspension_travel, wheel_slip, suspension_velocity, rumble_pitch)` channels.
    fn channels(self, t: &mut Telemetry) -> (&mut f64, &mut f64, &mut f64, &mut f64) {
        match self {
            Wheel::RearLeft => (
                &mut t.suspension_travel_lr,
                &mut t.wheel_slip_lr,
                &mut t.suspension_velocity_lr,
                &mut t.rumble_pitch_lr,
            ),
            Wheel::RearRight => (
                &mut t.suspension_travel_rr,
                &mut t.wheel_slip_rr,
                &mut t.suspension_velocity_rr,
                &mut t.rumble_pitch_rr,
            ),
            Wheel::FrontLeft => (
                &mut t.suspension_travel_lf,
                &mut t.wheel_slip_lf,
                &mut t.suspension_velocity_lf,
                &mut t.rumble_pitch_lf,
            ),
            Wheel::FrontRight => (
                &mut t.suspension_travel_rf,
                &mut t.wheel_slip_rf,
                &mut t.suspension_velocity_rf,
                &mut t.rumble_pitch_rf,
            ),
        }
    }
}

fn wheel_channel(name: &str) -> Option<(&str, Wheel)> {
    let (channel, wheel) = name.rsplit_once('_')?;
    if !matches!(channel, "suspension_travel" | "suspension_velocity" | "wheel_slip" | "rumble_pitch") {
        return None;
    }
    Some((
        channel,
        match wheel {
            "lf" => Wheel::FrontLeft,
            "rf" => Wheel::FrontRight,
            "lr" => Wheel::RearLeft,
            "rr" => Wheel::RearRight,
            _ => return None,
        },
    ))
}

/// The `flag` channel from the FIA flag (`-1` invalid, 0 none, 1 green, 2 blue, 3 yellow, 4 red):
/// 1 → 12, 2 → 2, 3 → 6, 4 → 5, anything else → 0.
fn fia_flag_code(fia_flag: i8) -> f64 {
    match fia_flag {
        1 => 12.0,
        2 => 2.0,
        3 => 6.0,
        4 => 5.0,
        _ => 0.0,
    }
}

/// `rumble_pitch_*`: 40 when the wheel's surface type is 1 (rumble strip), else 0.
fn rumble_strength(surface_type: u8) -> f64 {
    if surface_type == 1 {
        40.0
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_mapping_of_the_f1_listeners() {
        assert_eq!([-1, 0, 1, 2, 3, 4, 5].map(fia_flag_code), [0.0, 0.0, 12.0, 2.0, 6.0, 5.0, 0.0]);
    }

    #[test]
    fn wheel_indices_follow_wheel_dict() {
        assert_eq!(WHEELS.map(Wheel::index), [0, 1, 2, 3]);
        assert_eq!((rumble_strength(1), rumble_strength(0), rumble_strength(4)), (40.0, 0.0, 0.0));
    }
}
