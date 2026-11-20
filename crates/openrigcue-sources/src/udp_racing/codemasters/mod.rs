//! Codemasters "extradata" UDP listeners: DiRT (`dirt`), F1 2013–2017 (`f1`), GRID (`grid`),
//! WRC Generations (`wrc`) and Test Drive Unlimited Solar Crown (`tdusc`).

pub mod dirt;
pub mod f1_classic;
pub mod grid;
pub mod kinematics;
mod native;
pub mod packet;
mod probe;
pub mod tdusc;
pub mod wrc;

use kinematics::frame_pitch;
use openrigcue_model::Telemetry;
use packet::MotionPacket;

use crate::udp_racing::listener_output::zero_dispatcher_channels;
use crate::udp_racing::unhandled::DispatcherEnds;

/// Result of the stationary test that precedes every channel read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stationary {
    /// The car is parked in a menu: every channel reads 0.
    Yes,
    No,
    /// A NaN/infinite value has no integer value: every channel read fails.
    Unreadable,
}

/// Which stationary test a listener applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GuardRule {
    /// dirt, wrc: rpm*10 and speed truncate to 0 and the pitch is exactly 90.
    RpmTimesTenSpeedPitch,
    /// tdusc: rpm and speed truncate to 0.
    RpmSpeed,
}

/// Evaluate the stationary test left to right, stopping at the first false term.
fn stationary(packet: &MotionPacket, rule: GuardRule) -> Stationary {
    let rpm = match rule {
        GuardRule::RpmTimesTenSpeedPitch => packet.rpm * 10.0,
        GuardRule::RpmSpeed => packet.rpm,
    };
    let is_zero = |value: f64| crate::udp_racing::listener_output::python_int(value).map(|v| v == 0.0);
    match is_zero(rpm) {
        None => return Stationary::Unreadable,
        Some(false) => return Stationary::No,
        Some(true) => {}
    }
    match is_zero(packet.speed) {
        None => return Stationary::Unreadable,
        Some(false) => return Stationary::No,
        Some(true) => {}
    }
    let parked = match rule {
        GuardRule::RpmTimesTenSpeedPitch => frame_pitch(&packet.frame) == 90.0,
        GuardRule::RpmSpeed => true,
    };
    if parked {
        Stationary::Yes
    } else {
        Stationary::No
    }
}

/// What the dispatcher stores as the car name: the answer of the `car_name` read,
/// or nothing when that read fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CarName {
    /// The listener answered a name.
    Named(&'static str),
    /// The listener answered `None` or `0`: no name.
    Blank,
    /// The read failed: the previous name stays.
    Raised,
}

impl CarName {
    fn store(self, vehicle: &mut String) {
        match self {
            CarName::Named(name) => *vehicle = name.to_string(),
            CarName::Blank => vehicle.clear(),
            CarName::Raised => {}
        }
    }
}

/// Start a tick on which the listener's reads do not fail: every channel the dispatcher reads is 0 unless the
/// caller then writes the listener's answer. None of these listeners maps `track_loc`, so the location is the
/// `0` sentinel too.
///
/// These listeners answer 0 for every name they do not map, which the dispatcher stores for
/// every auto-loaded channel, and 0 for every name while stationary or before the first packet.
fn publish_unmapped_as_zero(t: &mut Telemetry, car_name: CarName) {
    zero_dispatcher_channels(t);
    t.location.clear();
    car_name.store(&mut t.vehicle);
}

/// Publish a tick on which the listener's `speed` read fails (a NaN or infinite rpm/speed has no integer value, or the
/// f1/grid listeners before their first packet): the car name, the location (`0` on failure) and
/// `lateral_velocity` (`0` when its own read fails as well) are stored, nothing else.
///
/// The dispatcher reads `car_name` and `track_loc` first, handling their failures, then `speed` (also handled),
/// stores `lateral_velocity` (0 on failure), and then its top-speed update repeats the `speed` read
/// without handling a failure. That failure ends the dispatcher's pass and is logged,
/// so pitch, yaw, roll and every auto-loaded channel keep their values.
///
/// Returns the [`DispatcherEnds`] the source reports for that tick.
fn publish_speed_unreadable(t: &mut Telemetry, lateral_velocity: f64, car_name: CarName) -> Result<(), DispatcherEnds> {
    t.location.clear();
    car_name.store(&mut t.vehicle);
    t.lateral_velocity = lateral_velocity;
    Err(crate::udp_racing::support::channels::speed_unreadable())
}

#[cfg(test)]
mod tests {
    use super::packet::{encode, field, Layout};
    use super::*;

    fn packet(fields: &[(usize, f32)]) -> MotionPacket {
        MotionPacket::decode(&encode(Layout::Extradata3, fields), Layout::Extradata3).unwrap()
    }

    #[test]
    fn guard_short_circuits_like_python_and() {
        // A NaN speed is only evaluated when rpm*10 truncates to 0.
        assert_eq!(
            stationary(&packet(&[(field::RPM, 50.0), (field::SPEED, f32::NAN)]), GuardRule::RpmTimesTenSpeedPitch),
            Stationary::No
        );
        assert_eq!(
            stationary(&packet(&[(field::SPEED, f32::NAN)]), GuardRule::RpmTimesTenSpeedPitch),
            Stationary::Unreadable
        );
        assert_eq!(stationary(&packet(&[(field::RPM, f32::NAN)]), GuardRule::RpmSpeed), Stationary::Unreadable);
    }

    #[test]
    fn guard_rules_differ_in_rpm_scale_and_pitch() {
        // tdusc tests the truncated rpm only; dirt tests the truncated rpm*10 and pitch == 90.
        let low_rpm = packet(&[(field::RPM, 0.5)]);
        assert_eq!(stationary(&low_rpm, GuardRule::RpmSpeed), Stationary::Yes);
        assert_eq!(stationary(&low_rpm, GuardRule::RpmTimesTenSpeedPitch), Stationary::No);
        let tilted = packet(&[(field::RX, 1.0), (field::FZ, 1.0)]);
        assert_eq!(stationary(&tilted, GuardRule::RpmTimesTenSpeedPitch), Stationary::No);
        assert_eq!(stationary(&tilted, GuardRule::RpmSpeed), Stationary::Yes);
    }
}
