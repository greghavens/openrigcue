//! IntelliTune's ordered typed capture; numeric samples remain a compatibility view.
use super::min_max::numeric_cmp;
use openrigcue_model::{
    native_value::{Dict, LegacyValue},
    Telemetry,
};
use std::cmp::Ordering;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
mod sort;

#[derive(Debug, Clone, PartialEq)]
pub struct IntellituneReference {
    pub sway: f64,
    pub heave: f64,
    pub surge_acceleration: f64,
    pub yaw: f64,
    pub roll: f64,
    pub pitch: f64,
    pub traction_loss: f64,
    /// Canonical max_telemetry_vars, or the error of its enclosing required lookup.
    pub native: Option<Result<LegacyValue, String>>,
}
type ReferenceOf = fn(&IntellituneReference) -> f64;
const CAPTURED: [(&str, &str, ReferenceOf); 7] = [
    ("lateral_acceleration", "sway", |r| r.sway),
    ("vertical_acceleration", "heave", |r| r.heave),
    ("longitudinal_acceleration", "surge_acceleration", |r| r.surge_acceleration),
    ("yaw", "yaw", |r| r.yaw),
    ("roll", "roll", |r| r.roll),
    ("pitch", "pitch", |r| r.pitch),
    ("lateral_velocity", "traction_loss", |r| r.traction_loss),
];
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IntellituneSamples {
    samples: [Vec<f64>; 7],
    native: Dict,
}

/// The authoritative captured dictionary is readable/resettable during listener waits.
#[derive(Debug, Clone, Default)]
pub struct IntellituneHandle(Arc<Mutex<IntellituneSamples>>);

impl IntellituneHandle {
    pub fn snapshot(&self) -> IntellituneSamples {
        self.lock().clone()
    }

    pub fn take(&self) -> IntellituneSamples {
        std::mem::take(&mut *self.lock())
    }

    pub(super) fn capture_checked(
        &self,
        telemetry: &Telemetry,
        reference: &IntellituneReference,
    ) -> Result<(), String> {
        for index in 0..CAPTURED.len() {
            self.lock().capture_channel(telemetry, reference, index)?;
        }
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, IntellituneSamples> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl IntellituneSamples {
    /// Compatibility entry point; production propagates capture_checked's failures.
    pub fn capture(&mut self, telemetry: &Telemetry, reference: &IntellituneReference) {
        let _ = self.capture_checked(telemetry, reference);
    }
    pub fn capture_checked(&mut self, telemetry: &Telemetry, reference: &IntellituneReference) -> Result<(), String> {
        for index in 0..CAPTURED.len() {
            self.capture_channel(telemetry, reference, index)?;
        }
        Ok(())
    }

    fn capture_channel(
        &mut self,
        telemetry: &Telemetry,
        reference: &IntellituneReference,
        index: usize,
    ) -> Result<(), String> {
        let (channel, field, reference_of) = CAPTURED[index];
        let current = telemetry
            .native_value(channel)
            .cloned()
            .or_else(|| telemetry.get_assigned(channel).map(LegacyValue::Float))
            .ok_or_else(|| format!("'{channel}'"))?;
        let current = absolute(&current)?;
        let default = match &reference.native {
            None => Number::Float(reference_of(reference).abs()),
            Some(Err(error)) => return Err(error.clone()),
            Some(Ok(value)) => absolute(value.item(field)?)?,
        };
        let threshold = match default {
            Number::Int(value) => Number::Int(value * 5),
            Number::Float(value) => Number::Float(value * 5.0),
        };
        if current.cmp(threshold) == Some(Ordering::Greater) {
            return Ok(());
        }
        let current = current.value();
        let existing = self.native.get(channel);
        let values = if let Some(LegacyValue::List(values)) = existing {
            values
        } else {
            self.native.insert(channel, LegacyValue::List(Vec::new()));
            self.samples[index].clear();
            match self.native.get(channel).unwrap() {
                LegacyValue::List(values) => values,
                _ => unreachable!(),
            }
        };
        let mut values = values.clone();
        if values.len() > 30 {
            sort::descending(&mut values)?;
            if numeric_cmp(&current, &values[0])? != Some(Ordering::Greater) {
                return Ok(());
            }
            values.pop();
        }
        values.push(current);
        self.samples[index] = values.iter().map(|value| value.number().unwrap_or(f64::NAN)).collect();
        self.native.insert(channel, LegacyValue::List(values));
        Ok(())
    }
    pub fn samples(&self, channel: &str) -> Option<&[f64]> {
        let index = CAPTURED.iter().position(|(name, _, _)| *name == channel)?;
        Some(&self.samples[index])
    }
    pub fn native_values(&self) -> &Dict {
        &self.native
    }
}

#[derive(Clone, Copy)]
enum Number {
    Int(i128),
    Float(f64),
}
fn absolute(value: &LegacyValue) -> Result<Number, String> {
    match value {
        LegacyValue::Int(value) => Ok(Number::Int(i128::from(*value).abs())),
        LegacyValue::Bool(value) => Ok(Number::Int(i128::from(*value))),
        LegacyValue::Float(value) => Ok(Number::Float(value.abs())),
        value => Err(format!("bad operand type for abs(): '{}'", value.type_name())),
    }
}
impl Number {
    fn value(self) -> LegacyValue {
        match self {
            Self::Int(value) => i64::try_from(value).map(LegacyValue::Int).unwrap_or(LegacyValue::Float(value as f64)),
            Self::Float(value) => LegacyValue::Float(value),
        }
    }
    fn cmp(self, other: Self) -> Option<Ordering> {
        fn mixed(integer: i128, float: f64) -> Option<Ordering> {
            if float.is_nan() {
                return None;
            }
            if float >= 2.0_f64.powi(127) {
                return Some(Ordering::Less);
            }
            if float < -2.0_f64.powi(127) {
                return Some(Ordering::Greater);
            }
            let order = integer.cmp(&(float as i128));
            Some(if order == Ordering::Equal {
                if float.fract() > 0.0 {
                    Ordering::Less
                } else if float.fract() < 0.0 {
                    Ordering::Greater
                } else {
                    Ordering::Equal
                }
            } else {
                order
            })
        }
        match (self, other) {
            (Self::Int(a), Self::Int(b)) => Some(a.cmp(&b)),
            (Self::Float(a), Self::Float(b)) => a.partial_cmp(&b),
            (Self::Int(a), Self::Float(b)) => mixed(a, b),
            (Self::Float(a), Self::Int(b)) => mixed(b, a).map(Ordering::reverse),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference() -> IntellituneReference {
        IntellituneReference {
            sway: 2.0,
            heave: 1.0,
            surge_acceleration: 1.0,
            yaw: 1.0,
            roll: 1.0,
            pitch: 1.0,
            traction_loss: 1.0,
            native: None,
        }
    }

    fn sway(value: f64) -> Telemetry {
        let mut telemetry = Telemetry::off();
        telemetry.lateral_acceleration = value;
        telemetry
    }

    /// Capture: |value| > 5·|reference| is rejected; the boundary is accepted.
    #[test]
    fn outliers_are_rejected() {
        let mut samples = IntellituneSamples::default();
        samples.capture(&sway(-10.0), &reference());
        samples.capture(&sway(10.5), &reference());
        assert_eq!(samples.samples("lateral_acceleration").unwrap(), &[10.0]);
        assert_eq!(samples.samples("pitch").unwrap(), &[0.0, 0.0]);
    }

    /// Capture: 31 free samples, then only a new maximum replaces the smallest.
    #[test]
    fn list_grows_to_31_then_keeps_maxima() {
        let mut samples = IntellituneSamples::default();
        for value in 1..=31 {
            samples.capture(&sway(f64::from(value) / 10.0), &reference());
        }
        assert_eq!(samples.samples("lateral_acceleration").unwrap().len(), 31);
        samples.capture(&sway(3.0), &reference());
        assert_eq!(samples.samples("lateral_acceleration").unwrap().len(), 31);
        assert_eq!(samples.samples("lateral_acceleration").unwrap()[0], 0.1);

        samples.capture(&sway(4.0), &reference());
        let kept = samples.samples("lateral_acceleration").unwrap();
        assert_eq!(kept.len(), 31);
        assert_eq!(kept[0], 3.1);
        assert_eq!(kept[29], 0.2);
        assert_eq!(kept[30], 4.0);
    }
}
