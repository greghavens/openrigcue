//! Per-game motion tune: the motion keys of the game configuration, as typed values.
//!
//! The motion code reads the tune through [`GameConfigView`] (dynamically typed values, converted
//! where they are read). [`MotionTune`] is a tune whose values are already numbers and booleans; it
//! serves every key it holds. A tune held as dynamically typed values is a [`crate::RawGameConfig`].
//!
//! [`MotionTune::default`] is the master configuration's `joy`/`vehicles`/`Default` entry.

use std::borrow::Cow;

use crate::axis::{Axis, PerAxis};
use crate::pyvalue::PyValue;
use crate::views::GameConfigView;

/// One value per tunable motion effect (`motion_effects` / `motion_switch` keys).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Effects<T> {
    pub pitch: T,
    pub roll: T,
    pub yaw: T,
    pub sway: T,
    pub surge_acceleration: T,
    pub surge_deceleration: T,
    pub heave: T,
    pub traction_loss: T,
    pub traction_loss_front: T,
    pub wheel_slip: T,
    pub rumble_strips: T,
    pub afterburner: T,
    pub cannon_firing: T,
    pub landing_gear: T,
    pub missile_firing: T,
    pub canopy: T,
    pub engine: T,
    pub abs_lock: T,
}

/// `pitch_type` / `roll_type` of a typed tune: the string `'angle'` or `'rate'`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RotationInput {
    /// `'angle'`: scale with `max_telemetry_vars.pitch` / `.roll`.
    #[default]
    Angle,
    /// `'rate'`: scale with `max_telemetry_vars.pitch_rate` / `.roll_rate`.
    Rate,
}

/// `max_telemetry_vars`: the telemetry value that means 50 % on each axis.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TelemetryMaxima {
    pub pitch: f64,
    pub roll: f64,
    pub yaw: f64,
    pub sway: f64,
    pub heave: f64,
    pub traction_loss: f64,
    pub traction_loss_front: f64,
    /// Used for positive surge.
    pub surge_acceleration: f64,
    /// Used for negative surge.
    pub surge_deceleration: f64,
    /// `None`: the key is absent.
    pub pitch_rate: Option<f64>,
    /// `None`: the key is absent.
    pub roll_rate: Option<f64>,
}

/// `motion_main_actuators_*`: shares (percent) that replace the table on the platform's main actuators.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MainActuatorShares {
    pub sway: f64,
    pub surge: f64,
    pub traction_loss: f64,
    pub traction_loss_front: f64,
}

/// The motion part of one game's (or vehicle's) tune.
#[derive(Debug, Clone, PartialEq)]
pub struct MotionTune {
    /// `motion_effects`: gain per effect, 0–20.
    pub effects: Effects<f64>,
    /// `motion_switch`: on/off per effect.
    pub switches: Effects<bool>,
    /// `motion_effects.pitch_type`.
    pub pitch_input: RotationInput,
    /// `motion_effects.roll_type`.
    pub roll_input: RotationInput,
    /// `max_telemetry_vars`.
    pub maxima: TelemetryMaxima,
    /// `motion_smoothing_factor` (rate shaping).
    pub smoothing_factor: f64,
    /// `motion_boost_factor` (rate shaping).
    pub boost_factor: f64,
    /// `motion_effect_smoothing_factor`: moving-average window per axis.
    pub effect_smoothing: PerAxis<f64>,
    /// `motion_max_power`, percent of travel for main actuators.
    pub max_power: f64,
    /// `motion_max_horizontal`, percent of travel for the other actuators.
    pub max_horizontal: f64,
    /// `min_traction_loss_activation`, percent.
    pub min_traction_loss_activation: f64,
    /// `min_traction_loss_front_activation`, percent.
    pub min_traction_loss_front_activation: f64,
    /// `motion_enhanced_gforces`: selects the platform's enhanced mixing table.
    pub enhanced_gforces: bool,
    /// `motion_belt_tensioner`, 0–100: surge share of the belt tensioner slot.
    pub belt_tensioner: f64,
    /// `motion_main_actuators_*`.
    pub main_actuator_shares: MainActuatorShares,
    /// `gear_smoothing_factor`, percent.
    pub gear_smoothing_factor: i64,
}

impl Default for MotionTune {
    fn default() -> Self {
        let gain = 10.0;
        let event = 4.0;
        MotionTune {
            effects: Effects {
                pitch: gain,
                roll: gain,
                yaw: gain,
                sway: gain,
                surge_acceleration: gain,
                surge_deceleration: gain,
                heave: gain,
                traction_loss: gain,
                traction_loss_front: gain,
                wheel_slip: event,
                rumble_strips: event,
                afterburner: event,
                cannon_firing: event,
                landing_gear: event,
                missile_firing: event,
                canopy: event,
                engine: 1.0,
                abs_lock: event,
            },
            switches: Effects {
                pitch: true,
                roll: true,
                yaw: true,
                sway: true,
                surge_acceleration: true,
                surge_deceleration: true,
                heave: true,
                traction_loss: true,
                traction_loss_front: true,
                wheel_slip: true,
                rumble_strips: true,
                afterburner: true,
                cannon_firing: true,
                landing_gear: true,
                missile_firing: true,
                canopy: true,
                engine: false,
                abs_lock: true,
            },
            pitch_input: RotationInput::Angle,
            roll_input: RotationInput::Angle,
            maxima: TelemetryMaxima {
                pitch: 5.0,
                roll: 5.0,
                yaw: 5.0,
                sway: 5.0,
                heave: 5.0,
                traction_loss: 5.0,
                traction_loss_front: 5.0,
                surge_acceleration: 5.0,
                surge_deceleration: 5.0,
                pitch_rate: None,
                roll_rate: None,
            },
            smoothing_factor: 1.0,
            boost_factor: 5.0,
            effect_smoothing: PerAxis::splat(20.0),
            max_power: 100.0,
            max_horizontal: 100.0,
            min_traction_loss_activation: 0.0,
            min_traction_loss_front_activation: 0.0,
            enhanced_gforces: true,
            belt_tensioner: 50.0,
            main_actuator_shares: MainActuatorShares {
                sway: 100.0,
                surge: 100.0,
                traction_loss: 0.0,
                traction_loss_front: 0.0,
            },
            gear_smoothing_factor: 95,
        }
    }
}

impl<T: Copy> Effects<T> {
    /// The `motion_effects` / `motion_switch` keys and their values, in the stored key order.
    fn entries(&self) -> [(&'static str, T); 18] {
        [
            ("wheel_slip", self.wheel_slip),
            ("pitch", self.pitch),
            ("roll", self.roll),
            ("yaw", self.yaw),
            ("sway", self.sway),
            ("surge_acceleration", self.surge_acceleration),
            ("surge_deceleration", self.surge_deceleration),
            ("heave", self.heave),
            ("traction_loss", self.traction_loss),
            ("rumble_strips", self.rumble_strips),
            ("afterburner", self.afterburner),
            ("cannon_firing", self.cannon_firing),
            ("landing_gear", self.landing_gear),
            ("missile_firing", self.missile_firing),
            ("canopy", self.canopy),
            ("engine", self.engine),
            ("abs_lock", self.abs_lock),
            ("traction_loss_front", self.traction_loss_front),
        ]
    }
}

fn dict(entries: impl IntoIterator<Item = (&'static str, PyValue)>) -> PyValue {
    PyValue::Dict(entries.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}

fn rotation(input: RotationInput) -> PyValue {
    PyValue::from(match input {
        RotationInput::Angle => "angle",
        RotationInput::Rate => "rate",
    })
}

impl GameConfigView for MotionTune {
    fn item(&self, key: &str) -> Option<Cow<'_, PyValue>> {
        let value = match key {
            "motion_effects" => {
                let mut entries: Vec<(&str, PyValue)> =
                    self.effects.entries().into_iter().map(|(k, v)| (k, PyValue::Float(v))).collect();
                entries.push(("pitch_type", rotation(self.pitch_input)));
                entries.push(("roll_type", rotation(self.roll_input)));
                dict(entries)
            }
            "motion_switch" => dict(self.switches.entries().into_iter().map(|(k, v)| (k, PyValue::Bool(v)))),
            "max_telemetry_vars" => {
                let m = &self.maxima;
                let mut entries = vec![
                    ("pitch", PyValue::Float(m.pitch)),
                    ("roll", PyValue::Float(m.roll)),
                    ("yaw", PyValue::Float(m.yaw)),
                    ("sway", PyValue::Float(m.sway)),
                    ("heave", PyValue::Float(m.heave)),
                    ("traction_loss", PyValue::Float(m.traction_loss)),
                    ("surge_acceleration", PyValue::Float(m.surge_acceleration)),
                    ("surge_deceleration", PyValue::Float(m.surge_deceleration)),
                    ("traction_loss_front", PyValue::Float(m.traction_loss_front)),
                ];
                entries.extend(m.pitch_rate.map(|v| ("pitch_rate", PyValue::Float(v))));
                entries.extend(m.roll_rate.map(|v| ("roll_rate", PyValue::Float(v))));
                dict(entries)
            }
            "motion_effect_smoothing_factor" => {
                dict(Axis::ALL.map(|axis| (axis.name(), PyValue::Float(self.effect_smoothing[axis]))))
            }
            "motion_smoothing_factor" => PyValue::Float(self.smoothing_factor),
            "motion_boost_factor" => PyValue::Float(self.boost_factor),
            "motion_max_power" => PyValue::Float(self.max_power),
            "motion_max_horizontal" => PyValue::Float(self.max_horizontal),
            "min_traction_loss_activation" => PyValue::Float(self.min_traction_loss_activation),
            "min_traction_loss_front_activation" => PyValue::Float(self.min_traction_loss_front_activation),
            "motion_enhanced_gforces" => PyValue::Int(i128::from(self.enhanced_gforces)),
            "motion_belt_tensioner" => PyValue::Float(self.belt_tensioner),
            "motion_main_actuators_sway" => PyValue::Float(self.main_actuator_shares.sway),
            "motion_main_actuators_surge" => PyValue::Float(self.main_actuator_shares.surge),
            "motion_main_actuators_traction_loss" => PyValue::Float(self.main_actuator_shares.traction_loss),
            "motion_main_actuators_traction_loss_front" => {
                PyValue::Float(self.main_actuator_shares.traction_loss_front)
            }
            "gear_smoothing_factor" => PyValue::from(self.gear_smoothing_factor),
            _ => return None,
        };
        Some(Cow::Owned(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_the_master_config_default_entry() {
        let t = MotionTune::default();
        assert_eq!(t.smoothing_factor, 1.0);
        assert_eq!(t.boost_factor, 5.0);
        assert_eq!(t.effects.engine, 1.0);
        assert!(!t.switches.engine);
        assert_eq!(t.gear_smoothing_factor, 95);
        assert!(t.enhanced_gforces);
        assert_eq!(t.main_actuator_shares.traction_loss, 0.0);
    }

    #[test]
    fn typed_tune_serves_game_config_keys() {
        let t = MotionTune::default();
        assert_eq!(t.item("motion_effects").unwrap().item("pitch_type"), Ok(&PyValue::from("angle")));
        assert_eq!(t.item("motion_switch").unwrap().item("engine"), Ok(&PyValue::Bool(false)));
        assert_eq!(t.item("max_telemetry_vars").unwrap().item("pitch_rate").unwrap_err().to_string(), "'pitch_rate'");
        assert_eq!(t.item("motion_enhanced_gforces").as_deref(), Some(&PyValue::Int(1)));
        assert!(t.item("no_such_key").is_none());
    }
}
