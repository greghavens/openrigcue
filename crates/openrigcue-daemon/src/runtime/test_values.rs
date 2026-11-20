use std::sync::Arc;

use openrigcue_cueing::{Axis, PyValue, TestSliders};
use openrigcue_model::native_value::LegacyValue;
use serde_json::Value;

use super::Controls;

#[derive(Debug, Clone, PartialEq)]
pub(super) struct NativeTestValue {
    projection: u64,
    value: Arc<LegacyValue>,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct NativeMotionTests([NativeTestValue; Axis::COUNT]);

impl Default for NativeMotionTests {
    fn default() -> Self {
        // The motion test starts as eight integer zeros.
        Self(std::array::from_fn(|_| NativeTestValue {
            projection: 0.0f64.to_bits(),
            value: Arc::new(LegacyValue::Int(0)),
        }))
    }
}

impl Controls {
    pub fn belt_test_value(&self) -> LegacyValue {
        match &self.native_belt_test_value {
            Some(slot) if slot.projection == self.tests.belt_test_value.to_bits() => slot.value.as_ref().clone(),
            _ => LegacyValue::Float(self.tests.belt_test_value),
        }
    }

    pub fn assign_belt_test_value(&mut self, value: LegacyValue, projection: f64) {
        self.tests.belt_test_value = projection;
        self.native_belt_test_value =
            Some(NativeTestValue { projection: projection.to_bits(), value: Arc::new(value) });
    }

    pub fn reset_belt_test_value(&mut self) {
        self.assign_belt_test_value(LegacyValue::Int(0), 0.0);
    }

    fn motion_test_projection(&self, axis: Axis) -> f64 {
        let values = self.tests.motion_sliders;
        match axis {
            Axis::Pitch => values.pitch,
            Axis::Roll => values.roll,
            Axis::Yaw => values.yaw,
            Axis::Sway => values.sway,
            Axis::Surge => values.surge,
            Axis::Heave => values.heave,
            Axis::TractionLoss => values.traction_loss,
            Axis::TractionLossFront => values.traction_loss_front,
        }
    }

    pub fn motion_test_value(&self, key: &str) -> Option<LegacyValue> {
        let axis = Axis::ALL.into_iter().find(|axis| axis.name() == key)?;
        let projection = self.motion_test_projection(axis);
        let slot = &self.native_motion_tests.0[axis.index()];
        Some(if slot.projection == projection.to_bits() {
            slot.value.as_ref().clone()
        } else {
            LegacyValue::Float(projection)
        })
    }

    pub fn native_motion_test_values(&self) -> [PyValue; Axis::COUNT] {
        std::array::from_fn(|index| PyValue::from(&self.motion_test_value(Axis::ALL[index].name()).unwrap()))
    }

    /// The UI writes the object it received; the numeric view remains available to API callers.
    pub fn assign_motion_test_value(&mut self, key: &str, value: LegacyValue, projection: f64) -> Result<(), String> {
        let axis = Axis::ALL
            .into_iter()
            .find(|axis| axis.name() == key)
            .ok_or_else(|| format!("KeyError: {}", openrigcue_model::python_repr::repr_str(key)))?;
        let field = match axis {
            Axis::Pitch => &mut self.tests.motion_sliders.pitch,
            Axis::Roll => &mut self.tests.motion_sliders.roll,
            Axis::Yaw => &mut self.tests.motion_sliders.yaw,
            Axis::Sway => &mut self.tests.motion_sliders.sway,
            Axis::Surge => &mut self.tests.motion_sliders.surge,
            Axis::Heave => &mut self.tests.motion_sliders.heave,
            Axis::TractionLoss => &mut self.tests.motion_sliders.traction_loss,
            Axis::TractionLossFront => &mut self.tests.motion_sliders.traction_loss_front,
        };
        *field = projection;
        self.native_motion_tests.0[axis.index()] =
            NativeTestValue { projection: projection.to_bits(), value: Arc::new(value) };
        Ok(())
    }

    pub fn reset_motion_test_sliders(&mut self) {
        self.tests.motion_sliders = TestSliders::default();
        self.native_motion_tests = NativeMotionTests::default();
    }

    /// A numeric API edit replaces just its supplied entries; unrelated patches retain the stored objects.
    pub(crate) fn retain_test_values_after_patch(&mut self, before: &Controls, patch: &Value) {
        self.native_belt_test_value = before.native_belt_test_value.clone();
        if patch.get("tests").is_some_and(Value::is_null) {
            self.reset_belt_test_value();
        } else if let Some(value) = patch.get("tests").and_then(|tests| tests.get("belt_test_value")) {
            let value = if value.is_null() { LegacyValue::Int(0) } else { LegacyValue::from(value.clone()) };
            self.assign_belt_test_value(value, self.tests.belt_test_value);
        }
        self.native_motion_tests = before.native_motion_tests.clone();
        let Some(tests) = patch.get("tests") else { return };
        if tests.is_null() {
            self.native_motion_tests = NativeMotionTests::default();
            return;
        }
        let Some(sliders) = tests.get("motion_sliders") else { return };
        if sliders.is_null() {
            self.native_motion_tests = NativeMotionTests::default();
            return;
        }
        for axis in Axis::ALL {
            if let Some(value) = sliders.get(axis.name()) {
                let value = if value.is_null() { LegacyValue::Int(0) } else { LegacyValue::from(value.clone()) };
                self.native_motion_tests.0[axis.index()] =
                    NativeTestValue { projection: self.motion_test_projection(axis).to_bits(), value: Arc::new(value) };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_belt_test_scalars_retain_objects_and_typed_edits() {
        let mut controls = Controls::default();
        let raw = LegacyValue::Bytes(b"17".to_vec());
        controls.assign_belt_test_value(raw.clone(), 17.0);
        assert_eq!(controls.belt_test_value(), raw);
        let mut after: Controls = serde_json::from_value(serde_json::to_value(&controls).unwrap()).unwrap();
        after.retain_test_values_after_patch(&controls, &serde_json::json!({"switches":{"motion":false}}));
        assert_eq!(after.belt_test_value(), raw);
        after.tests.belt_test_value = 18.0;
        assert_eq!(after.belt_test_value(), LegacyValue::Float(18.0));
        after.reset_belt_test_value();
        assert_eq!(after.belt_test_value(), LegacyValue::Int(0));
    }

    #[test]
    fn native_motion_test_scalars_retain_defaults_objects_and_typed_edits() {
        let mut controls = Controls::default();
        for axis in Axis::ALL {
            assert_eq!(controls.motion_test_value(axis.name()), Some(LegacyValue::Int(0)));
        }
        let raw = LegacyValue::Bytes(b"17".to_vec());
        controls.assign_motion_test_value("pitch", raw.clone(), 17.0).unwrap();
        assert_eq!(controls.motion_test_value("pitch"), Some(raw));
        controls.tests.motion_sliders.pitch = 18.0;
        assert_eq!(controls.motion_test_value("pitch"), Some(LegacyValue::Float(18.0)));
        controls.reset_motion_test_sliders();
        assert_eq!(controls.motion_test_value("pitch"), Some(LegacyValue::Int(0)));
    }
}
