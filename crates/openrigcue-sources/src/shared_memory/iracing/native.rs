//! Typed iRacing getter results, separate from the numeric compatibility replay.

use std::cmp::Ordering;

use openrigcue_model::native_value::{Dict, Key, LegacyValue as V};
use openrigcue_model::python_compare::{native_equal, numeric_cmp};

use super::*;
use crate::shared_memory::estimated_wheel_slip::native_estimated_wheel_slip;

impl Frame<'_> {
    fn raw(&mut self, name: &str) -> Result<V, Raised> {
        native_value(self.get(name)?)
    }

    fn raw_or_zero(&mut self, name: &str) -> Result<V, Raised> {
        Ok(match self.raw(name)? {
            V::None => V::Int(0),
            value => value,
        })
    }

    fn repeated_non_none(&mut self, name: &str) -> Result<V, Raised> {
        if self.get(name)?.is_none() {
            Ok(V::None)
        } else {
            self.raw(name)
        }
    }

    pub(super) fn native_answer(&mut self, name: &str) -> Result<V, Raised> {
        if self.native_paused()? {
            return Ok(V::Int(0));
        }
        // These branches precede the engine-off test.
        match name {
            "speed" => return self.repeated_non_none("Speed"),
            "gear" => return self.repeated_non_none("Gear"),
            "max_rpm" => {
                let info = self.get("DriverInfo")?;
                return if info.truthy() {
                    native_yaml(index(&self.get("DriverInfo")?, "DriverCarSLBlinkRPM")?)
                } else {
                    Ok(V::None)
                };
            }
            "car_name" => return self.native_car_name(),
            "steering_wheel_angle_max" => return self.repeated_non_none("SteeringWheelAngleMax"),
            "steering_wheel_angle" => {
                return if self.native_dams_is_steering()? {
                    Ok(match self.repeated_non_none("SteeringWheelAngle")? {
                        V::None => V::Int(0),
                        value => value,
                    })
                } else {
                    self.repeated_non_none("LatAccel")
                }
            }
            "wheel_slip_lf" | "wheel_slip_lr" => return self.native_slip(WheelSide::Left),
            "wheel_slip_rf" | "wheel_slip_rr" => return self.native_slip(WheelSide::Right),
            _ => {}
        }
        if self.native_engine_off()? {
            return Ok(V::Int(0));
        }
        match name {
            "rpm" => self.repeated_non_none("RPM"),
            "lateral_velocity" => match self.repeated_non_none("VelocityY")? {
                V::None => Ok(V::None),
                value => multiply(value, -1),
            },
            // The test is on Z; the value returned is X.
            "longitudinal_velocity" => {
                if self.get("VelocityZ")?.is_none() {
                    Ok(V::None)
                } else {
                    self.raw("VelocityX")
                }
            }
            "vertical_velocity" => self.repeated_non_none("VelocityX"),
            "vertical_acceleration" => {
                Ok(V::Float(self.native_acceleration("VertAccel").number().ok_or(Raised)? - GRAVITY))
            }
            "lateral_acceleration" => Ok(self.native_acceleration("LatAccel")),
            "longitudinal_acceleration" => Ok(self.native_acceleration("LongAccel")),
            "yaw" | "pitch" | "roll" => {
                let key = match name {
                    "yaw" => "Yaw",
                    "pitch" => "Pitch",
                    _ => "Roll",
                };
                let angle = rad_to_deg(self.get(key)?.number()?);
                Ok(V::Float(if name == "roll" { angle } else { -angle }))
            }
            "raw_flag" => self.raw("SessionFlags"),
            "flag" => self.native_flag(),
            "spotter_left" | "spotter_right" => {
                let side = self.raw("CarLeftRight")?;
                let codes = if name == "spotter_left" { [2, 4, 6] } else { [3, 4, 6] };
                Ok(V::Int(i64::from(codes.iter().any(|&code| native_equal(&side, &V::Int(code))))))
            }
            "abs_lock" => native_integer(self.raw("BrakeABSactive")?),
            "oil_pressure" | "turbo_boost_pressure" => {
                let key = if name == "oil_pressure" { "OilPress" } else { "ManifoldPress" };
                match self.repeated_non_none(key)? {
                    V::None => Ok(V::Int(0)),
                    value => multiply(value, 100),
                }
            }
            "track_loc" => Ok(self.native_track_name().unwrap_or(V::Int(0))),
            _ => {
                if let Some((channel, corner)) = corner(name) {
                    return match channel {
                        "suspension_travel" | "suspension_velocity" => {
                            let suffix = if channel == "suspension_travel" { "shockDefl" } else { "shockVel" };
                            let fallbacks = match corner {
                                "LF" => ["LF", "RF", "LR"],
                                "LR" => ["LR", "RR", "LF"],
                                "RF" => ["RF", "LF", "RR"],
                                _ => ["RR", "LR", "RF"],
                            };
                            for corner in fallbacks {
                                let value = self.raw(&format!("{corner}{suffix}"))?;
                                if value != V::None {
                                    return self.raw(&format!("{corner}{suffix}"));
                                }
                            }
                            Ok(V::Int(0))
                        }
                        "rumble_pitch" => self.repeated_non_none(&format!("Tire{corner}_RumblePitch")),
                        "tyre_temperature" => self.native_tyre_minimum(corner, ["tempCM", "tempCR", "tempCL"]),
                        "tyre_wear" => self.native_tyre_minimum(corner, ["wearM", "wearR", "wearL"]),
                        _ => unreachable!(),
                    };
                }
                let key = match name {
                    "lap" => "Lap",
                    "lap_completed" => "LapCompleted",
                    "lap_dist" => "LapDist",
                    "lap_dist_percent" => "LapDistPct",
                    "race_laps" => "RaceLaps",
                    "lap_best_lap" => "LapBestLap",
                    "lap_best_lap_time" => "LapBestLapTime",
                    "lap_last_lap_time" => "LapLastLapTime",
                    "lap_current_lap_time" => "LapCurrentLapTime",
                    "lap_delta_to_best_lap" => "LapDeltaToBestLap",
                    "lap_delta_to_optimal_lap" => "LapDeltaToOptimalLap",
                    "lap_delta_to_session_best_lap" => "LapDeltaToSessionBestLap",
                    "lap_delta_to_session_optimal_lap" => "LapDeltaToSessionOptimalLap",
                    "lap_delta_to_session_last_lap" => "LapDeltaToSessionLastLap",
                    "oil_temperature" => "OilTemp",
                    "oil_level" => "OilLevel",
                    "water_temperature" => "WaterTemp",
                    "water_level" => "WaterLevel",
                    "fuel_pressure" => "FuelPress",
                    "fuel_level" => "FuelLevelPct",
                    "throttle" => "Throttle",
                    "brake" => "Brake",
                    "clutch" => "Clutch",
                    "yaw_rate" => "YawRate",
                    _ => name,
                };
                self.raw_or_zero(key)
            }
        }
    }

    fn native_engine_off(&mut self) -> Result<bool, Raised> {
        if self.get("Speed")?.is_none() || self.get("RPM")?.is_none() {
            return Ok(false);
        }
        if numeric_cmp(&self.raw("RPM")?, &V::Int(400)).map_err(|_| Raised)? != Some(Ordering::Less) {
            return Ok(false);
        }
        Ok(numeric_cmp(&self.raw("Speed")?, &V::Int(1)).map_err(|_| Raised)? == Some(Ordering::Less))
    }

    fn native_flag(&mut self) -> Result<V, Raised> {
        for (mask, flag) in [
            (1024, 1),
            (32, 2),
            (2, 3),
            (16, 5),
            (8, 6),
            (1048576, 9),
            (65536, 10),
            (1, 11),
            (4, 1),
            (256, 13),
            (1073741824, 20),
            (536870912, 21),
            (2147483648, 22),
            (128, 30),
            (131072, 30),
            (64, 31),
            (512, 32),
            (32768, 34),
            (524288, 35),
            (16384, 36),
        ] {
            if self.get("SessionFlags")?.integer()? & mask != 0 {
                return Ok(V::Int(flag));
            }
        }
        Ok(V::Int(0))
    }

    fn native_paused(&mut self) -> Result<bool, Raised> {
        let replay = self.get("IsReplayPlaying")?;
        if !replay.is_none()
            && self.get("IsReplayPlaying")?.truthy()
            && super::super::native_pause(self.pause_reader, self.options.pause_when_not_playing).ok_or(Raised)?
        {
            return Ok(true);
        }
        let on_track = self.get("IsOnTrackCar")?;
        let pause = super::super::native_pause(self.pause_reader, self.options.pause_when_not_playing).ok_or(Raised)?;
        Ok(pause && !on_track.truthy())
    }

    fn native_dams_is_steering(&self) -> Result<bool, Raised> {
        let Some(reader) = self.dams_reader else {
            return self.options.dams_front_tl_mode.map(|mode| mode == DAMS_MODE_STEERING).ok_or(Raised);
        };
        let value = reader().ok_or(Raised)?;
        // The converter validates the integer conversion separately from its range category.
        // Every out-of-range integer is also unequal to 2; no socket operation occurs.
        let converted = crate::udp_racing::socket::ImportedPort::from_native(&value).ok_or(Raised)?;
        Ok(converted == crate::udp_racing::socket::ImportedPort::Valid(DAMS_MODE_STEERING as u16))
    }

    fn native_car_name(&mut self) -> Result<V, Raised> {
        let info = self.get("DriverInfo")?;
        if !info.truthy() {
            return Ok(V::None);
        }
        let car = native_yaml(index(&self.get("DriverInfo")?, "DriverCarIdx")?)?;
        let drivers_info = self.get("DriverInfo")?;
        let drivers = match index(&drivers_info, "Drivers")? {
            Yaml::Array(drivers) => drivers.as_slice(),
            Yaml::Hash(entries) if entries.is_empty() => &[],
            Yaml::String(text) if text.is_empty() => &[],
            _ => return Err(Raised),
        };
        for driver in drivers {
            let Yaml::Hash(fields) = driver else { return Err(Raised) };
            let idx = fields.get(&Yaml::String("CarIdx".into())).ok_or(Raised)?;
            if native_equal(&native_yaml(idx)?, &car) {
                return native_yaml(fields.get(&Yaml::String("CarScreenName".into())).ok_or(Raised)?);
            }
        }
        Ok(V::Str("-1".into()))
    }

    fn native_track_name(&mut self) -> Result<V, Raised> {
        let weekend = self.get("WeekendInfo")?;
        if !weekend.truthy() {
            return Ok(V::Int(0));
        }
        native_yaml(index(&self.get("WeekendInfo")?, "TrackDisplayShortName")?)
    }

    fn native_tyre_minimum(&mut self, corner: &str, suffixes: [&str; 3]) -> Result<V, Raised> {
        let first = self.raw(&format!("{corner}{}", suffixes[0]))?;
        let second = self.raw(&format!("{corner}{}", suffixes[1]))?;
        let third = self.raw(&format!("{corner}{}", suffixes[2]))?;
        let low = if numeric_cmp(&first, &third).map_err(|_| Raised)? == Some(Ordering::Less) { first } else { third };
        Ok(if numeric_cmp(&second, &low).map_err(|_| Raised)? == Some(Ordering::Less) { second } else { low })
    }

    fn native_slip(&mut self, side: WheelSide) -> Result<V, Raised> {
        let lateral_velocity = self.native_answer("lateral_velocity")?.number().ok_or(Raised)?;
        let speed = self.native_answer("speed")?.number().ok_or(Raised)?;
        let rpm = self.native_answer("rpm")?.number().ok_or(Raised)?;
        let gear = match self.native_answer("gear")? {
            V::Int(value) => value,
            V::Bool(value) => i64::from(value),
            _ => return Err(Raised),
        };
        let previous_speed = self.previous.speed.ok_or(Raised)?;
        let previous_rpm = self.previous.rpm.ok_or(Raised)?;
        let throttle = self.native_answer("throttle")?.number().ok_or(Raised)?;
        let brake = self.native_answer("brake")?.number().ok_or(Raised)?;
        self.native_answer("clutch")?.number().ok_or(Raised)?;
        Ok(native_estimated_wheel_slip(
            side,
            &SlipInputs { lateral_velocity, speed, rpm, gear, previous_speed, previous_rpm, throttle, brake },
        ))
    }

    fn native_acceleration(&mut self, name: &str) -> V {
        // A failed lookup sets the raw acceleration to integer zero before the float conversion. Each failure prints.
        let raw = self.raw(name).unwrap_or_else(|_| {
            crate::printed::line(format!("cannot set {name} acceleration raw"));
            V::Int(0)
        });
        if let Some(value) = float(&raw) {
            return V::Float(value);
        }
        crate::printed::line(format!("cannot set {name} acceleration"));
        match first(&raw).and_then(|value| float(&value)) {
            Some(value) => V::Float(value),
            None => {
                crate::printed::line(format!("cannot set {name} acceleration using the list, setting to 0"));
                V::Int(0)
            }
        }
    }
}

fn corner(name: &str) -> Option<(&str, &str)> {
    let (channel, wheel) = name.rsplit_once('_')?;
    if !matches!(
        channel,
        "suspension_travel" | "suspension_velocity" | "rumble_pitch" | "tyre_temperature" | "tyre_wear"
    ) {
        return None;
    }
    Some((
        channel,
        match wheel {
            "lf" => "LF",
            "rf" => "RF",
            "lr" => "LR",
            "rr" => "RR",
            _ => return None,
        },
    ))
}

fn scalar(value: Scalar) -> V {
    match value {
        Scalar::Char(byte) => V::Bytes(vec![byte]),
        Scalar::Bool(value) => V::Bool(value),
        Scalar::Int(value) => V::Int(value),
        Scalar::Float(value) => V::Float(value),
    }
}

fn native_value(value: Value) -> Result<V, Raised> {
    match value {
        Value::None => Ok(V::None),
        Value::Var(VarValue::One(value)) => Ok(scalar(value)),
        Value::Var(VarValue::Many(values)) => Ok(V::List(values.into_iter().map(scalar).collect())),
        Value::Doc(value) => native_yaml(&value),
    }
}

fn native_yaml(value: &Yaml) -> Result<V, Raised> {
    Ok(match value {
        Yaml::Null => V::None,
        Yaml::Boolean(value) => V::Bool(*value),
        Yaml::Integer(value) => V::Int(*value),
        Yaml::Real(_) => V::Float(value.as_f64().ok_or(Raised)?),
        Yaml::String(value) => V::Str(value.clone()),
        Yaml::Array(values) => V::List(values.iter().map(native_yaml).collect::<Result<_, _>>()?),
        Yaml::Hash(values) => {
            let mut dict = Dict::new();
            for (key, value) in values {
                let key = match key {
                    Yaml::String(value) => Key::Str(value.clone()),
                    Yaml::Integer(value) => Key::Int(*value),
                    Yaml::Boolean(value) => Key::Bool(*value),
                    Yaml::Real(_) => Key::float(key.as_f64().ok_or(Raised)?).ok_or(Raised)?,
                    Yaml::Null => Key::None,
                    _ => return Err(Raised),
                };
                dict.insert(key, native_yaml(value)?);
            }
            V::Dict(dict)
        }
        _ => return Err(Raised),
    })
}

fn bounded_integer(value: i128) -> V {
    i64::try_from(value).map_or_else(|_| V::Float(value as f64), V::Int)
}

fn multiply(value: V, factor: i64) -> Result<V, Raised> {
    Ok(match value {
        V::Int(value) => bounded_integer(i128::from(value) * i128::from(factor)),
        V::Bool(value) => V::Int(i64::from(value) * factor),
        V::Float(value) => V::Float(value * factor as f64),
        V::Str(value) => V::Str(value.repeat(factor.max(0) as usize)),
        V::Bytes(value) => V::Bytes(value.repeat(factor.max(0) as usize)),
        V::List(values) => V::List((0..factor.max(0)).flat_map(|_| values.iter().cloned()).collect()),
        _ => return Err(Raised),
    })
}

fn native_integer(value: V) -> Result<V, Raised> {
    match value {
        V::Int(_) => Ok(value),
        V::Bool(value) => Ok(V::Int(i64::from(value))),
        V::Float(value) => crate::udp_racing::native_int(value).ok_or(Raised),
        V::Str(value) => openrigcue_model::python_numeric::parse_py_int128(&value).map(bounded_integer).ok_or(Raised),
        V::Bytes(value) => std::str::from_utf8(&value)
            .ok()
            .and_then(openrigcue_model::python_numeric::parse_py_int128)
            .map(bounded_integer)
            .ok_or(Raised),
        _ => Err(Raised),
    }
}

fn float(value: &V) -> Option<f64> {
    match value {
        V::Str(value) => openrigcue_model::python_numeric::parse_py_float(value),
        V::Bytes(value) => std::str::from_utf8(value).ok().and_then(openrigcue_model::python_numeric::parse_py_float),
        _ => value.number(),
    }
}

fn first(value: &V) -> Option<V> {
    match value {
        V::List(values) => values.first().cloned(),
        V::Str(value) => value.chars().next().map(|c| V::Str(c.to_string())),
        V::Bytes(value) => value.first().map(|&b| V::Int(i64::from(b))),
        V::Dict(values) => values.get_key(&Key::Int(0)).cloned(),
        _ => None,
    }
}

#[cfg(test)]
mod yaml_key_tests {
    use super::*;

    #[test]
    fn numeric_yaml_keys_keep_constructor_kinds_through_native_conversion() {
        for (source, expected) in [
            ("true: first\n1: middle\ntrue: last\n'True': text\n", "{True: 'last', 'True': 'text'}"),
            ("1.0: first\ntrue: last\n'1.0': text\n", "{1.0: 'last', '1.0': 'text'}"),
            ("-0.0: first\nfalse: last\n", "{-0.0: 'last'}"),
            ("1.5: value\n'1.5': text\n", "{1.5: 'value', '1.5': 'text'}"),
            (".inf: value\n'inf': text\n", "{inf: 'value', 'inf': 'text'}"),
        ] {
            let yaml = super::super::yaml11::load(source).unwrap().unwrap();
            let value = native_yaml(&yaml).unwrap();
            assert_eq!(openrigcue_model::python_repr::repr(&value), expected);
        }
    }
}
