//! Thanos AMC motion controllers.
//!
//! - [`protocol`]: byte framing, parameter codes, configuration layouts.
//! - [`Amc`]: one controller on a serial link, including the XTEA authentication handshake.
//! - [`ThanosDriver`]: controller discovery (main and secondary), streaming, reverse flags, spike filter
//!   and UI parameter push; implements [`super::MotionDriver`].
//!
//! The helpers at this level ([`rescale`], [`ActuatorLayout`], [`VerticalPositions`]) are shared by the
//! other drivers of this unit.

mod amc;
mod platform;
pub mod protocol;
mod xtea;

pub use amc::{Amc, AmcError, CHALLENGE_ALPHABET, CHALLENGE_LEN};
pub use platform::{
    random_challenge, ComPortCache, ControllerState, MemoryComPortCache, NativeSpikeFilters, PendingChange, PortCache,
    Role, ThanosDriver, ThanosSettings, ThanosShared,
};

use crate::motion::driver_support::CornerTravel;
use crate::motion::MotionError;

/// Linear rescale clamped to `[out_min, out_max]`, in 64-bit floats
/// (`r =(x - in_min) * (out_max - out_min) / (in_max - in_min) + out_min`, then `min(r, out_max)`,
/// then `max(.., out_min)`; no zero-division guard, IEEE semantics).
pub(crate) fn rescale(x: f64, in_min: f64, in_max: f64, out_min: f64, out_max: f64) -> f64 {
    let r = (x - in_min) * (out_max - out_min) / (in_max - in_min) + out_min;
    let upper = if r <= out_max { r } else { out_max };
    if out_min <= upper {
        upper
    } else {
        out_min
    }
}

/// How the four vertical actuators map onto actuator slots, chosen by model name
/// (`'3 Actuators [2 Front + 1 Rear]'`-style names, square brackets for THANOS, round for PT CANBus).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActuatorLayout {
    TwoFrontOneRear,
    OneFrontTwoRear,
    FourCorners,
}

impl ActuatorLayout {
    /// The layout of a model name: `'3 Actuators [2 Front + 1 Rear]'`/`'… (2 Front + 1 Rear) CANBus'` and
    /// the `1 Front + 2 Rear` pair; every other model uses the four-corner layout.
    pub fn from_model_name(name: &str) -> Self {
        match name {
            "3 Actuators [2 Front + 1 Rear]" | "3 Actuators (2 Front + 1 Rear) CANBus" => Self::TwoFrontOneRear,
            "3 Actuators [1 Front + 2 Rear]" | "3 Actuators (1 Front + 2 Rear) CANBus" => Self::OneFrontTwoRear,
            _ => Self::FourCorners,
        }
    }

    /// Slot indices for (front_left, front_right, rear_left, rear_right).
    pub(crate) fn slots(self) -> [usize; 4] {
        match self {
            Self::TwoFrontOneRear => [1, 2, 0, 0],
            Self::OneFrontTwoRear => [0, 0, 2, 1],
            Self::FourCorners => [1, 2, 0, 3],
        }
    }
}

/// Vertical actuator extension in millimetres.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct VerticalPositions {
    pub front_left: f64,
    pub front_right: f64,
    pub rear_left: f64,
    pub rear_right: f64,
}

impl VerticalPositions {
    /// Slot `m[i]` rescaled from 0..65535 to 0..`stroke_mm` per corner. `None` if a needed slot is missing.
    #[cfg(test)]
    pub(crate) fn from_slots(slots: &[i64], layout: ActuatorLayout, stroke_mm: i64) -> Option<Self> {
        let pos = |i: usize| -> Option<f64> {
            slots.get(i).map(|&v| rescale(v as f64, 0.0, 65_535.0, 0.0, stroke_mm as f64))
        };
        let [fl, fr, rl, rr] = layout.slots();
        Some(VerticalPositions {
            front_left: pos(fl)?,
            front_right: pos(fr)?,
            rear_left: pos(rl)?,
            rear_right: pos(rr)?,
        })
    }
}

impl From<VerticalPositions> for CornerTravel {
    fn from(p: VerticalPositions) -> Self {
        CornerTravel {
            front_left: p.front_left,
            front_right: p.front_right,
            rear_left: p.rear_left,
            rear_right: p.rear_right,
        }
    }
}

/// The file name in the frames an error of this driver passes.
pub(crate) const THANOS_FILE: &str = "backend/controllers/thanos.py";

/// The frame for `function` and `line` in this driver's file.
pub(crate) const fn at(function: &'static str, line: u32) -> crate::motion::PyFrame {
    crate::motion::PyFrame::new(THANOS_FILE, function, line)
}

/// Whether a port's hardware id contains `VID:PID`.
pub(crate) fn has_usb_id(
    port: &crate::discovery::ListPortInfo,
    vendor: u16,
    product: u16,
) -> Result<bool, MotionError> {
    port.hwid_contains(&format!("{vendor:04X}:{product:04X}"))
        .map_err(|message| MotionError::Native(format!("TypeError: {message}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rescale: linear, clamped to [out_min, out_max], 64-bit float division.
    #[test]
    fn functions_map_clamps_and_divides_like_c() {
        assert_eq!(rescale(32768.0, 0.0, 65536.0, 0.0, 22.0), 11.0);
        assert_eq!(rescale(70000.0, 0.0, 65536.0, 0.0, 22.0), 22.0);
        assert_eq!(rescale(-5.0, 0.0, 65536.0, 0.0, 22.0), 0.0);
        assert_eq!(rescale(10.0, 0.0, 0.0, 0.0, 255.0), 255.0, "x/0 = inf, clamped");
        assert_eq!(rescale(0.0, 0.0, 0.0, 0.0, 255.0), 255.0, "0/0 = NaN fails r <= out_max");
        assert_eq!(rescale(-1.0, 0.0, 0.0, 0.0, 255.0), 0.0, "-inf");
    }

    /// The slot choice of the actuator positions per model name.
    #[test]
    fn actuator_positions_per_layout() {
        let m = [0, 65535, 32767, 13107];
        let p =
            VerticalPositions::from_slots(&m, ActuatorLayout::from_model_name("3 Actuators [2 Front + 1 Rear]"), 100)
                .unwrap();
        assert_eq!(
            (p.front_left, p.front_right, p.rear_left, p.rear_right),
            (100.0, 32767.0 * 100.0 / 65535.0, 0.0, 0.0)
        );
        let p = VerticalPositions::from_slots(
            &m,
            ActuatorLayout::from_model_name("3 Actuators (1 Front + 2 Rear) CANBus"),
            100,
        )
        .unwrap();
        assert_eq!((p.front_left, p.front_right, p.rear_right), (0.0, 0.0, 100.0));
        let p =
            VerticalPositions::from_slots(&m, ActuatorLayout::from_model_name("4 Actuators [2 Front + 2 Rear]"), 100)
                .unwrap();
        assert_eq!(p.rear_right, 20.0);
        assert!(VerticalPositions::from_slots(&m[..3], ActuatorLayout::FourCorners, 100).is_none());
    }
}
