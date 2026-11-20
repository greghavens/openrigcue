//! Per-channel minimum/maximum of the telemetry view.

use openrigcue_model::native_value::LegacyValue;
use openrigcue_model::Telemetry;
use std::cmp::Ordering;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// Lowest and highest value seen for one channel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Extremes {
    pub min: f64,
    pub max: f64,
}

/// Running extremes of every channel since the last reset.
#[derive(Debug, Clone)]
pub struct MinMaxTable {
    entries: Vec<Option<Extremes>>,
    native_entries: Vec<Option<NativeExtremes>>,
}

/// The authoritative table remains available while a listener holds the dispatcher mutex.
#[derive(Debug, Clone, Default)]
pub struct MinMaxHandle(Arc<Mutex<MinMaxTable>>);

impl MinMaxHandle {
    pub fn snapshot(&self) -> MinMaxTable {
        self.lock().clone()
    }

    pub fn clear(&self) {
        self.lock().clear();
    }

    pub(super) fn update_in_order(&self, telemetry: &Telemetry, channels: &[&'static str]) -> Result<(), MinMaxError> {
        for &name in channels {
            self.lock().update_in_order(telemetry, &[name])?;
        }
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, MinMaxTable> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NativeExtremes {
    pub min: LegacyValue,
    pub max: LegacyValue,
}

#[derive(Debug, thiserror::Error)]
#[error("{channel}: {message}")]
pub struct MinMaxError {
    pub channel: &'static str,
    pub message: String,
}

impl Default for MinMaxTable {
    fn default() -> Self {
        MinMaxTable {
            entries: vec![None; Telemetry::CHANNELS.len()],
            native_entries: vec![None; Telemetry::CHANNELS.len()],
        }
    }
}

impl MinMaxTable {
    /// Folds the current values in. A channel's first value is both its minimum and maximum; comparisons are
    /// strict, so a NaN never replaces an extreme and a NaN extreme is never replaced.
    pub fn update(&mut self, telemetry: &Telemetry) {
        let _ = self.update_checked(telemetry);
    }

    /// Strings are converted before the comparisons. An invalid
    /// string or an unsupported comparison ends the pass, retaining all earlier table assignments.
    pub fn update_checked(&mut self, telemetry: &Telemetry) -> Result<(), MinMaxError> {
        self.update_in_order(telemetry, telemetry.channel_names())
    }

    pub(crate) fn update_in_order(
        &mut self,
        telemetry: &Telemetry,
        channels: &[&'static str],
    ) -> Result<(), MinMaxError> {
        for &name in channels {
            let index = Telemetry::CHANNELS.iter().position(|channel| *channel == name).expect("registry channel");
            let value = telemetry
                .native_value(name)
                .cloned()
                .unwrap_or_else(|| LegacyValue::Float(telemetry.get(name).expect("registry channel")));
            let old = self.native_entries[index].clone().unwrap_or_else(|| {
                self.entries[index].map_or_else(
                    || NativeExtremes { min: value.clone(), max: value.clone() },
                    |old| NativeExtremes { min: LegacyValue::Float(old.min), max: LegacyValue::Float(old.max) },
                )
            });
            let fail = |message| MinMaxError { channel: name, message };
            let value = integer_text(value).map_err(&fail)?;
            let min = integer_text(old.min).map_err(&fail)?;
            let max = integer_text(old.max).map_err(&fail)?;
            let new_min =
                if numeric_cmp(&value, &min).map_err(&fail)? == Some(Ordering::Less) { value.clone() } else { min };
            let new_max =
                if numeric_cmp(&value, &max).map_err(&fail)? == Some(Ordering::Greater) { value } else { max };
            self.entries[index] =
                Some(Extremes { min: new_min.number().unwrap_or(f64::NAN), max: new_max.number().unwrap_or(f64::NAN) });
            self.native_entries[index] = Some(NativeExtremes { min: new_min, max: new_max });
        }
        Ok(())
    }

    /// Forgets every extreme (game closed, or the telemetry page "Reset" button).
    pub fn clear(&mut self) {
        self.entries.iter_mut().for_each(|entry| *entry = None);
        self.native_entries.iter_mut().for_each(|entry| *entry = None);
    }

    /// Extremes of a channel, `None` before its first processed tick.
    pub fn get(&self, channel: &str) -> Option<Extremes> {
        let index = Telemetry::CHANNELS.iter().position(|name| *name == channel)?;
        self.entries[index]
    }

    pub fn native(&self, channel: &str) -> Option<&NativeExtremes> {
        let index = Telemetry::CHANNELS.iter().position(|name| *name == channel)?;
        self.native_entries[index].as_ref()
    }
}

fn integer_text(value: LegacyValue) -> Result<LegacyValue, String> {
    match value {
        LegacyValue::Str(text) => openrigcue_model::python_numeric::parse_py_int(&text)
            .map(LegacyValue::Int)
            .ok_or_else(|| format!("int({text:?}) failed")),
        value => Ok(value),
    }
}

pub(super) use openrigcue_model::python_compare::numeric_cmp;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_text_keeps_earlier_updates_and_stops_before_later_channels() {
        let mut table = MinMaxTable::default();
        let mut telemetry = Telemetry::off();
        telemetry.pitch = 1.0;
        telemetry.roll = 2.0;
        telemetry.set_native("yaw", LegacyValue::Str("bad".into()));
        assert_eq!(table.update_checked(&telemetry).unwrap_err().channel, "yaw");
        assert_eq!(table.get("pitch"), Some(Extremes { min: 1.0, max: 1.0 }));
        assert_eq!(table.get("roll"), Some(Extremes { min: 2.0, max: 2.0 }));
        assert_eq!(table.get("yaw"), None);
        assert_eq!(table.get("speed"), None);
        telemetry.set_native("yaw", LegacyValue::Str("１２_٣".into()));
        table.update_checked(&telemetry).unwrap();
        assert_eq!(table.native("yaw").unwrap().max, LegacyValue::Int(123));
        assert_eq!(telemetry.native_value("yaw"), Some(&LegacyValue::Str("１２_٣".into())));
        telemetry.set_native("yaw", LegacyValue::None);
        telemetry.pitch = 3.0;
        assert_eq!(table.update_checked(&telemetry).unwrap_err().channel, "yaw");
        assert_eq!(table.get("pitch").unwrap().max, 3.0);
        assert_eq!(table.native("yaw").unwrap().max, LegacyValue::Int(123));
    }

    #[test]
    fn mixed_integer_float_extremes_compare_without_rounding_the_integer() {
        let mut table = MinMaxTable::default();
        let mut telemetry = Telemetry::off();
        telemetry.set_native("gear", LegacyValue::Int(9_007_199_254_740_993));
        table.update_checked(&telemetry).unwrap();
        telemetry.set_native("gear", LegacyValue::Float(9_007_199_254_740_992.0));
        table.update_checked(&telemetry).unwrap();
        assert_eq!(table.native("gear").unwrap().max, LegacyValue::Int(9_007_199_254_740_993));
        assert_eq!(table.native("gear").unwrap().min, LegacyValue::Float(9_007_199_254_740_992.0));
        for integer in [i64::MIN, -1, 0, 1, i64::MAX] {
            assert_eq!(numeric_cmp(&LegacyValue::Int(integer), &LegacyValue::Float(f64::NAN)).unwrap(), None);
            assert_eq!(
                numeric_cmp(&LegacyValue::Int(integer), &LegacyValue::Float(f64::INFINITY)).unwrap(),
                Some(Ordering::Less)
            );
            assert_eq!(
                numeric_cmp(&LegacyValue::Int(integer), &LegacyValue::Float(f64::NEG_INFINITY)).unwrap(),
                Some(Ordering::Greater)
            );
        }
    }

    /// The first value seeds both extremes, later values widen them, clear forgets.
    #[test]
    fn extremes_follow_values() {
        let mut table = MinMaxTable::default();
        assert_eq!(table.get("rpm"), None);
        let mut telemetry = Telemetry::off();
        telemetry.rpm = 3000.0;
        table.update(&telemetry);
        assert_eq!(table.get("rpm"), Some(Extremes { min: 3000.0, max: 3000.0 }));
        telemetry.rpm = 1000.0;
        table.update(&telemetry);
        telemetry.rpm = 7000.0;
        table.update(&telemetry);
        assert_eq!(table.get("rpm"), Some(Extremes { min: 1000.0, max: 7000.0 }));
        assert_eq!(table.get("gear"), Some(Extremes { min: 11.0, max: 11.0 }));
        table.clear();
        assert_eq!(table.get("rpm"), None);
    }

    /// Comparisons with NaN are false: NaN neither replaces nor is replaced.
    #[test]
    fn nan_comparisons_are_false() {
        let mut table = MinMaxTable::default();
        let mut telemetry = Telemetry::off();
        telemetry.rpm = 5.0;
        table.update(&telemetry);
        telemetry.rpm = f64::NAN;
        table.update(&telemetry);
        assert_eq!(table.get("rpm"), Some(Extremes { min: 5.0, max: 5.0 }));
    }
}
