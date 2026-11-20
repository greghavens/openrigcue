//! The shared motion sensor values slot, independent of device I/O and pose snapshots.

use std::sync::{Arc, RwLock};

use crate::native_value::{Dict, LegacyValue};

#[derive(Debug, Clone)]
pub struct MotionSensorValues(Arc<RwLock<Dict>>);

impl Default for MotionSensorValues {
    fn default() -> Self {
        Self(Arc::new(RwLock::new(initial_values())))
    }
}

// A clone refers to the same parameter slot; callers compare snapshots to compare stored values.
impl PartialEq for MotionSensorValues {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for MotionSensorValues {}

fn initial_values() -> Dict {
    let mut values = Dict::new();
    for key in ["yaw", "roll", "pitch", "raw_yaw", "raw_roll", "raw_pitch"] {
        values.insert(key, LegacyValue::Int(0));
    }
    values
}

impl MotionSensorValues {
    pub fn get(&self, key: &str) -> Option<LegacyValue> {
        self.0.read().unwrap_or_else(std::sync::PoisonError::into_inner).get(key).cloned()
    }

    pub fn set(&self, key: &str, value: LegacyValue) {
        self.0.write().unwrap_or_else(std::sync::PoisonError::into_inner).insert(key, value);
    }

    pub fn reset(&self) {
        *self.0.write().unwrap_or_else(std::sync::PoisonError::into_inner) = initial_values();
    }

    pub fn snapshot(&self) -> Dict {
        self.0.read().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_values_and_reset_preserve_native_kinds_and_sparse_keys() {
        let values = MotionSensorValues::default();
        let reader = values.clone();
        assert_eq!(values.snapshot().str_keys(), ["yaw", "roll", "pitch", "raw_yaw", "raw_roll", "raw_pitch"]);
        assert_eq!(reader.get("raw_yaw"), Some(LegacyValue::Int(0)));
        assert_eq!(reader.get("sway"), None);
        values.set("raw_yaw", LegacyValue::Float(0.0));
        values.set("calculated_yaw", LegacyValue::Float(f64::NAN));
        assert_eq!(reader.get("raw_yaw"), Some(LegacyValue::Float(0.0)));
        assert!(matches!(reader.get("calculated_yaw"), Some(LegacyValue::Float(value)) if value.is_nan()));
        values.reset();
        assert_eq!(reader.get("raw_yaw"), Some(LegacyValue::Int(0)));
        assert_eq!(reader.get("calculated_yaw"), None);
    }
}
