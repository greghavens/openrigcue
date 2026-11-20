//! The eight motion cue axes and a fixed-size per-axis container.

use std::ops::{Index, IndexMut};

use serde::{Deserialize, Serialize};

/// One motion cue channel.
///
/// The declaration order is the column order of the platform tables (`direction` and
/// `percentages` of `motion_config.mapping`) and the order in which the per-axis signals of one
/// actuator are summed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Axis {
    Pitch,
    Roll,
    Yaw,
    Sway,
    Surge,
    Heave,
    /// Rear traction loss (`traction_loss`, fed by the `lateral_velocity` channel).
    TractionLoss,
    /// Front traction loss (`traction_loss_front`, fed by the `front_tl` channel).
    TractionLossFront,
}

impl Axis {
    pub const COUNT: usize = 8;
    pub const ALL: [Axis; Axis::COUNT] = [
        Axis::Pitch,
        Axis::Roll,
        Axis::Yaw,
        Axis::Sway,
        Axis::Surge,
        Axis::Heave,
        Axis::TractionLoss,
        Axis::TractionLossFront,
    ];

    pub fn index(self) -> usize {
        self as usize
    }

    /// The name used in settings files and settings keys.
    pub fn name(self) -> &'static str {
        match self {
            Axis::Pitch => "pitch",
            Axis::Roll => "roll",
            Axis::Yaw => "yaw",
            Axis::Sway => "sway",
            Axis::Surge => "surge",
            Axis::Heave => "heave",
            Axis::TractionLoss => "traction_loss",
            Axis::TractionLossFront => "traction_loss_front",
        }
    }
}

/// One value per axis, indexed by [`Axis`]. Serialized as a table keyed by axis name.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(
    from = "AxisFields<T>",
    into = "AxisFields<T>",
    bound(serialize = "T: Serialize + Clone", deserialize = "T: Deserialize<'de>")
)]
pub struct PerAxis<T: Clone>(pub [T; Axis::COUNT]);

impl<T: Clone> PerAxis<T> {
    /// Every axis set to `value`.
    pub fn splat(value: T) -> Self {
        PerAxis(std::array::from_fn(|_| value.clone()))
    }

    /// Built axis by axis.
    pub fn from_fn(mut f: impl FnMut(Axis) -> T) -> Self {
        PerAxis(std::array::from_fn(|i| f(Axis::ALL[i])))
    }

    /// Each value converted with `f`.
    pub fn map<U: Clone>(self, f: impl FnMut(T) -> U) -> PerAxis<U> {
        PerAxis(self.0.map(f))
    }
}

impl<T: Clone> Index<Axis> for PerAxis<T> {
    type Output = T;
    fn index(&self, axis: Axis) -> &T {
        &self.0[axis.index()]
    }
}

impl<T: Clone> IndexMut<Axis> for PerAxis<T> {
    fn index_mut(&mut self, axis: Axis) -> &mut T {
        &mut self.0[axis.index()]
    }
}

/// Named-field form of [`PerAxis`] for settings files.
#[derive(Serialize, Deserialize)]
struct AxisFields<T> {
    pitch: T,
    roll: T,
    yaw: T,
    sway: T,
    surge: T,
    heave: T,
    traction_loss: T,
    traction_loss_front: T,
}

impl<T: Clone> From<AxisFields<T>> for PerAxis<T> {
    fn from(f: AxisFields<T>) -> Self {
        PerAxis([f.pitch, f.roll, f.yaw, f.sway, f.surge, f.heave, f.traction_loss, f.traction_loss_front])
    }
}

impl<T: Clone> From<PerAxis<T>> for AxisFields<T> {
    fn from(p: PerAxis<T>) -> Self {
        let [pitch, roll, yaw, sway, surge, heave, traction_loss, traction_loss_front] = p.0;
        AxisFields { pitch, roll, yaw, sway, surge, heave, traction_loss, traction_loss_front }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn axis_order_matches_the_original_table_columns() {
        // Mapping table columns and per-axis signal indices: 0 pitch, 1 roll, ... 7 tlf.
        let names: Vec<_> = Axis::ALL.iter().map(|a| a.name()).collect();
        assert_eq!(names, ["pitch", "roll", "yaw", "sway", "surge", "heave", "traction_loss", "traction_loss_front"]);
    }

    #[test]
    fn per_axis_round_trips_as_a_named_table() {
        let v = PerAxis::from_fn(|a| a.index() as f64);
        let text = toml::to_string(&v).unwrap();
        assert!(text.contains("traction_loss_front = 7.0"));
        let back: PerAxis<f64> = toml::from_str(&text).unwrap();
        assert_eq!(back, v);
    }
}
