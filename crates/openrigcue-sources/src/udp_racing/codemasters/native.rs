//! The typed Codemasters getters; numeric polling remains a compatibility projection.

use std::sync::{Mutex, PoisonError};

use openrigcue_model::native_value::LegacyValue as V;

use super::kinematics::{
    frame_pitch, frame_yaw, lateral_speed, roll_degrees, tdusc_orientation, vertical_speed, HeaveEstimator,
};
use super::packet::{Corners, MotionPacket};
use super::{stationary, GuardRule, Stationary};
use crate::NativeRead;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Dirt,
    F1,
    Grid,
    Wrc,
    Tdusc,
}

pub(super) fn read(packet: Option<MotionPacket>, heave: &Mutex<HeaveEstimator>, kind: Kind, name: &str) -> NativeRead {
    if matches!(kind, Kind::Dirt | Kind::Wrc | Kind::Tdusc) {
        let Some(packet) = packet else {
            return NativeRead::Value(V::Int(0));
        };
        let rule = if kind == Kind::Tdusc { GuardRule::RpmSpeed } else { GuardRule::RpmTimesTenSpeedPitch };
        match stationary(&packet, rule) {
            Stationary::Yes => return NativeRead::Value(V::Int(0)),
            Stationary::Unreadable => return NativeRead::Raised,
            Stationary::No => {}
        }
    }
    answer(packet, heave, kind, name).map_or(NativeRead::Raised, NativeRead::Value)
}

fn answer(packet: Option<MotionPacket>, heave: &Mutex<HeaveEstimator>, kind: Kind, name: &str) -> Option<V> {
    let extended = matches!(kind, Kind::F1 | Kind::Grid);
    if name == "car_name" {
        return if kind == Kind::F1 { super::f1_classic::native_car_name(packet) } else { Some(V::Int(0)) };
    }
    if extended && matches!(name, "vertical_velocity" | "longitudinal_velocity") {
        return Some(
            packet
                .and_then(|p| p.extended)
                .map_or(V::Int(0), |e| V::Float(e.angular_velocity[if name == "vertical_velocity" { 1 } else { 2 }])),
        );
    }
    let corner = corner(name);
    let implemented = matches!(
        name,
        "rpm"
            | "max_rpm"
            | "gear"
            | "speed"
            | "steering_wheel_angle"
            | "roll"
            | "pitch"
            | "yaw"
            | "lateral_velocity"
            | "vertical_acceleration"
            | "lateral_acceleration"
            | "longitudinal_acceleration"
    ) || corner.is_some();
    if !implemented {
        return Some(V::Int(0));
    }
    let p = packet?;
    let rpm_factor = if matches!(kind, Kind::Dirt | Kind::Grid | Kind::Wrc) { 10.0 } else { 1.0 };
    let float = match name {
        "rpm" => return crate::udp_racing::int_text(p.rpm * rpm_factor).map(V::Str),
        "max_rpm" => {
            let maximum = p.session.and_then(|s| crate::udp_racing::int_text(s.max_rpm * rpm_factor));
            return if matches!(kind, Kind::Dirt | Kind::Wrc | Kind::Tdusc) {
                Some(V::Str(maximum.unwrap_or_else(|| "20000".into())))
            } else {
                maximum.map(V::Str)
            };
        }
        "gear" => {
            return if kind == Kind::F1 {
                int_minus_one(p.gear).map(V::Str)
            } else {
                crate::udp_racing::int_text(p.gear).map(V::Str)
            }
        }
        "speed" => return crate::udp_racing::int_text(p.speed * if extended { 0.6214 } else { 1.0 }).map(V::Str),
        "steering_wheel_angle" => p.steer * if extended { 10.0 } else { 1.0 },
        "roll" | "pitch" | "yaw" if kind == Kind::Tdusc => {
            let (pitch, yaw, roll) = tdusc_orientation(&p.frame);
            match name {
                "roll" => roll,
                "pitch" => pitch,
                _ => yaw,
            }
        }
        "roll" => {
            let component = if kind == Kind::Wrc { p.frame.right[2] } else { p.frame.right[1] };
            let roll = roll_degrees(component)?;
            if matches!(kind, Kind::Dirt | Kind::Grid) {
                -roll
            } else {
                roll
            }
        }
        "pitch" if kind == Kind::Wrc => return Some(V::Int(0)),
        "pitch" => frame_pitch(&p.frame),
        "yaw" => frame_yaw(&p.frame),
        "lateral_velocity" => {
            if extended {
                p.extended.map_or_else(|| lateral_speed(&p.frame), |e| e.angular_velocity[0] * 10.0)
            } else {
                let value = lateral_speed(&p.frame);
                if matches!(kind, Kind::Wrc | Kind::Tdusc) {
                    -value
                } else {
                    value
                }
            }
        }
        "longitudinal_acceleration" => p.gforce_lon * if matches!(kind, Kind::Wrc | Kind::Tdusc) { -1.0 } else { 10.0 },
        "lateral_acceleration" => p.gforce_lat * if kind == Kind::Dirt { 10.0 } else { 1.0 },
        "vertical_acceleration" => {
            if extended {
                if let Some(e) = p.extended {
                    return Some(V::Float(e.gforce_vert));
                }
            }
            let value =
                heave.lock().unwrap_or_else(PoisonError::into_inner).next_native(p.time, vertical_speed(&p.frame));
            let gain = match kind {
                Kind::Dirt | Kind::Wrc => 2,
                Kind::Tdusc => -1,
                _ => 1,
            };
            return Some(match value {
                V::Int(value) => V::Int(value * gain),
                V::Float(value) => V::Float(value * gain as f64),
                _ => unreachable!(),
            });
        }
        _ => {
            let (channel, wheel) = corner?;
            let corners = if channel == "suspension_travel" { p.suspension_position } else { p.suspension_velocity };
            let value = wheel_value(corners, wheel);
            if channel == "suspension_travel" {
                value
                    / match kind {
                        Kind::Dirt | Kind::Tdusc => 10000.0,
                        Kind::Wrc => 2.5,
                        _ => 1000.0,
                    }
            } else {
                value
            }
        }
    };
    Some(V::Float(float))
}

fn corner(name: &str) -> Option<(&str, &str)> {
    let (channel, wheel) = name.rsplit_once('_')?;
    (matches!(channel, "suspension_travel" | "suspension_velocity") && matches!(wheel, "lf" | "rf" | "lr" | "rr"))
        .then_some((channel, wheel))
}

fn wheel_value(corners: Corners, wheel: &str) -> f64 {
    match wheel {
        "lf" => corners.front_left,
        "rf" => corners.front_right,
        "lr" => corners.rear_left,
        "rr" => corners.rear_right,
        _ => unreachable!(),
    }
}

/// The text of a float truncated to an integer, minus 1, including integer magnitudes wider than i128.
fn int_minus_one(value: f64) -> Option<String> {
    let mut digits = crate::udp_racing::int_text(value)?.into_bytes();
    if digits == b"0" {
        return Some("-1".into());
    }
    if digits[0] == b'-' {
        let mut carry = true;
        for digit in digits[1..].iter_mut().rev() {
            if *digit == b'9' {
                *digit = b'0';
            } else {
                *digit += 1;
                carry = false;
                break;
            }
        }
        if carry {
            digits.insert(1, b'1');
        }
    } else {
        for digit in digits.iter_mut().rev() {
            if *digit == b'0' {
                *digit = b'9';
            } else {
                *digit -= 1;
                break;
            }
        }
        if digits[0] == b'0' && digits.len() > 1 {
            digits.remove(0);
        }
    }
    String::from_utf8(digits).ok()
}

#[cfg(test)]
mod tests {
    use super::super::kinematics::HeaveRule;
    use super::super::packet::{encode, field, Layout};
    use super::*;

    fn packet(layout: Layout) -> MotionPacket {
        MotionPacket::decode(
            &encode(
                layout,
                &[
                    (field::TIME, 0.5),
                    (field::RPM, 500.5),
                    (field::MAX_RPM, 700.5),
                    (field::SPEED, 100.0),
                    (field::GEAR, 3.0),
                    (field::RX, 1.0),
                    (field::FY, 1.0),
                    (field::ZV, -1.0),
                ],
            ),
            layout,
        )
        .unwrap()
    }

    #[test]
    fn native_kinds_fallbacks_and_individual_failures() {
        for kind in [Kind::Dirt, Kind::F1, Kind::Grid, Kind::Wrc, Kind::Tdusc] {
            let heave = Mutex::new(HeaveEstimator::new(HeaveRule::HoldLastNonZero));
            let mut p = packet(Layout::Extradata3);
            assert!(matches!(read(Some(p), &heave, kind, "rpm"), NativeRead::Value(V::Str(_))));
            assert_eq!(read(Some(p), &heave, kind, "suspension_travel_lf"), NativeRead::Value(V::Float(0.0)));
            assert_eq!(read(Some(p), &heave, kind, "unknown"), NativeRead::Value(V::Int(0)));
            p.speed = f64::NAN; // nonzero RPM short-circuits the global stationary test
            assert_eq!(read(Some(p), &heave, kind, "speed"), NativeRead::Raised);
            assert!(matches!(read(Some(p), &heave, kind, "rpm"), NativeRead::Value(V::Str(_))));
            p.session = None;
            let maximum = read(Some(p), &heave, kind, "max_rpm");
            if matches!(kind, Kind::Dirt | Kind::Wrc | Kind::Tdusc) {
                assert_eq!(maximum, NativeRead::Value(V::Str("20000".into())));
                p.rpm = f64::NAN;
                assert_eq!(read(Some(p), &heave, kind, "unknown"), NativeRead::Raised);
            } else {
                assert_eq!(maximum, NativeRead::Raised);
                assert_eq!(read(None, &heave, kind, "rpm"), NativeRead::Raised);
                assert_eq!(read(None, &heave, kind, "vertical_velocity"), NativeRead::Value(V::Int(0)));
                assert_eq!(read(None, &heave, kind, "longitudinal_velocity"), NativeRead::Value(V::Int(0)));
                assert_eq!(read(None, &heave, kind, "car_name"), NativeRead::Value(V::Int(0)));
            }
        }
    }

    #[test]
    fn native_heave_runs_only_at_the_request_and_retains_the_held_kind() {
        let heave = Mutex::new(HeaveEstimator::new(HeaveRule::HoldLastNonZero));
        let mut p = packet(Layout::Extradata3);
        assert!(matches!(read(Some(p), &heave, Kind::Dirt, "speed"), NativeRead::Value(V::Str(_))));
        assert_eq!(read(Some(p), &heave, Kind::Dirt, "vertical_acceleration"), NativeRead::Value(V::Float(4.0)));
        assert_eq!(read(Some(p), &heave, Kind::Dirt, "vertical_acceleration"), NativeRead::Value(V::Float(4.0)));
        let heave = Mutex::new(HeaveEstimator::new(HeaveRule::HoldLastNonZero));
        p.frame.velocity = [0.0; 3];
        assert_eq!(read(Some(p), &heave, Kind::Dirt, "vertical_acceleration"), NativeRead::Value(V::Int(0)));
        let heave = Mutex::new(HeaveEstimator::new(HeaveRule::HoldLastNonZero));
        let extended = packet(Layout::Extended);
        assert_eq!(read(Some(extended), &heave, Kind::F1, "vertical_acceleration"), NativeRead::Value(V::Float(0.0)));
        assert_eq!(
            read(Some(packet(Layout::Extradata3)), &heave, Kind::F1, "vertical_acceleration"),
            NativeRead::Value(V::Float(2.0))
        );
    }

    #[test]
    fn native_f1_name_catches_none_and_wide_gear_text() {
        let heave = Mutex::new(HeaveEstimator::new(HeaveRule::HoldLastNonZero));
        let mut p = packet(Layout::Extended);
        p.extended.as_mut().unwrap().era = 2017.0;
        p.session.as_mut().unwrap().team_info = 9.0;
        assert_eq!(read(Some(p), &heave, Kind::F1, "car_name"), NativeRead::Value(V::None));
        p.session.as_mut().unwrap().team_info = 100.0;
        assert_eq!(read(Some(p), &heave, Kind::F1, "car_name"), NativeRead::Raised);
        p.extended.as_mut().unwrap().era = -1.0;
        assert_eq!(read(Some(p), &heave, Kind::F1, "car_name"), NativeRead::Value(V::Int(0)));
        for (input, expected) in [
            (0.0, "-1"),
            (1.0, "0"),
            (1000.0, "999"),
            (-999.0, "-1000"),
            (2.0f64.powi(100), "1267650600228229401496703205375"),
            (-2.0f64.powi(100), "-1267650600228229401496703205377"),
        ] {
            assert_eq!(int_minus_one(input).as_deref(), Some(expected));
        }
    }
}
