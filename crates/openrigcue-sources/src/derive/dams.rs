//! DAMS traction-loss channels: `front_tl` and the dispatcher's override of `lateral_velocity`.

use std::f64::consts::PI;

/// Speed above which slip angles are computed, m/s (1 km/h). The dispatcher's speed is whole m/s, so in practice
/// the test is "at least 1 m/s".
const MIN_SLIP_SPEED: f64 = 0.277_777_78;

/// `CUSTOM_FEATURES.dams_front_tl_mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DamsMode {
    /// Mode 0 (and any value other than 1 or 2): `front_tl` copies the raw lateral velocity.
    Off,
    /// Mode 1: front slip angle from the velocity vector.
    FrontSlip,
    /// Mode 2: front and rear slip angles with yaw-rate lever arms and steering compensation.
    FrontAndRearSlip,
    /// The key is missing or its value is not an integer: both channels are 0 on every tick.
    Unreadable,
}

impl DamsMode {
    /// Maps `CUSTOM_FEATURES.dams_front_tl_mode` as an integer to a mode; `None` where the conversion (or the key lookup) fails.
    pub fn from_config(mode: Option<i64>) -> Self {
        match mode {
            Some(1) => DamsMode::FrontSlip,
            Some(2) => DamsMode::FrontAndRearSlip,
            Some(_) => DamsMode::Off,
            None => DamsMode::Unreadable,
        }
    }
}

/// The DAMS settings of `config.ini` `[CUSTOM_FEATURES]`, converted from the stored values.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DamsSettings {
    pub mode: DamsMode,
    /// `dams_front_tl_factor`: gain on both slip angles; `None` when unreadable (the slip angle is then 0).
    pub factor: Option<f64>,
    /// `dams_rear_tl_a`: yaw-rate lever arm of the front slip angle, 0 when unreadable.
    /// No configuration migration creates this key, so it reads 0 unless the user adds it.
    pub front_lever: f64,
    /// `dams_rear_tl_b`: yaw-rate lever arm of the rear slip angle, 0 when unreadable.
    pub rear_lever: f64,
    /// `dams_front_steerratio`: steering-wheel to road-wheel ratio; `None` when unreadable (the front slip
    /// angle is then 0, as for a zero ratio).
    pub steering_ratio: Option<f64>,
}

/// What the slip computation reads this tick.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SlipInputs {
    /// Listener lateral velocity (0 when unavailable).
    pub lateral: f64,
    /// Listener longitudinal velocity (0 when unavailable).
    pub longitudinal: f64,
    /// Dispatcher speed of this tick, whole m/s.
    pub speed: f64,
    /// Listener yaw rate.
    pub yaw_rate: Option<f64>,
    /// Listener steering wheel angle.
    pub steering: Option<f64>,
}

impl SlipInputs {
    pub(super) fn sliding(&self) -> bool {
        self.longitudinal != 0.0 && self.speed > MIN_SLIP_SPEED
    }
}

/// `front_tl` for this tick.
///
/// Every failure (unreadable
/// mode or factor, missing yaw rate, unreadable or zero steering ratio) yields 0.
pub fn front_traction_loss(settings: &DamsSettings, inputs: &SlipInputs) -> f64 {
    match settings.mode {
        DamsMode::Off => inputs.lateral,
        DamsMode::FrontSlip if inputs.sliding() => settings
            .factor
            .map(|factor| (inputs.lateral / inputs.longitudinal).atan() * 180.0 / PI * factor)
            .unwrap_or(0.0),
        DamsMode::FrontAndRearSlip if inputs.sliding() => compensated_front_slip(settings, inputs).unwrap_or(0.0),
        _ => 0.0,
    }
}

/// `lateral_velocity` for this tick: the rear slip angle in mode 2, 0 for an unreadable mode, otherwise the
/// listener's lateral velocity (0 when unavailable).
pub fn rear_traction_loss(settings: &DamsSettings, inputs: &SlipInputs, raw_lateral: Option<f64>) -> f64 {
    match settings.mode {
        DamsMode::FrontAndRearSlip if inputs.sliding() => rear_slip(settings, inputs).unwrap_or(0.0),
        DamsMode::FrontAndRearSlip | DamsMode::Unreadable => 0.0,
        DamsMode::Off | DamsMode::FrontSlip => raw_lateral.unwrap_or(0.0),
    }
}

fn compensated_front_slip(settings: &DamsSettings, inputs: &SlipInputs) -> Option<f64> {
    let yaw_rate = inputs.yaw_rate?;
    let ratio = settings.steering_ratio.filter(|ratio| *ratio != 0.0)?;
    let factor = settings.factor?;
    let steering = inputs.steering.unwrap_or(0.0);
    let slip = slip_angle(inputs, settings.front_lever, yaw_rate) - steering / ratio;
    Some(slip * factor)
}

fn rear_slip(settings: &DamsSettings, inputs: &SlipInputs) -> Option<f64> {
    let yaw_rate = inputs.yaw_rate?;
    Some(slip_angle(inputs, settings.rear_lever, yaw_rate) * settings.factor?)
}

fn slip_angle(inputs: &SlipInputs, lever: f64, yaw_rate: f64) -> f64 {
    ((inputs.lateral + lever * yaw_rate) / inputs.longitudinal).atan() * (180.0 / PI)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(mode: DamsMode) -> DamsSettings {
        DamsSettings { mode, factor: Some(2.0), front_lever: 0.0, rear_lever: 1.44, steering_ratio: Some(14.5) }
    }

    fn inputs(speed: f64) -> SlipInputs {
        SlipInputs { lateral: 1.0, longitudinal: 10.0, speed, yaw_rate: Some(0.5), steering: Some(29.0) }
    }

    /// front_tl, mode 0: raw lateral velocity.
    #[test]
    fn mode_off_copies_lateral() {
        let off = settings(DamsMode::from_config(Some(0)));
        assert_eq!(front_traction_loss(&off, &inputs(30.0)), 1.0);
        assert_eq!(rear_traction_loss(&off, &inputs(30.0), Some(-3.0)), -3.0);
        assert_eq!(rear_traction_loss(&off, &inputs(30.0), None), 0.0);
        assert_eq!(DamsMode::from_config(Some(7)), DamsMode::Off);
    }

    /// front_tl, mode 1: atan(lat/lon)·180/π·k above 1 km/h, else 0.
    #[test]
    fn mode_one_front_slip() {
        let front = settings(DamsMode::FrontSlip);
        let expected = (0.1f64).atan() * 180.0 / PI * 2.0;
        assert_eq!(front_traction_loss(&front, &inputs(1.0)), expected);
        assert_eq!(front_traction_loss(&front, &inputs(0.0)), 0.0);
        let stopped = SlipInputs { longitudinal: 0.0, ..inputs(30.0) };
        assert_eq!(front_traction_loss(&front, &stopped), 0.0);
        assert_eq!(rear_traction_loss(&front, &inputs(30.0), Some(1.0)), 1.0);
    }

    /// front_tl/lateral_velocity, mode 2: lever arms, steering compensation and failure → 0.
    #[test]
    fn mode_two_front_and_rear_slip() {
        let both = settings(DamsMode::FrontAndRearSlip);
        let front = ((1.0f64 / 10.0).atan() * (180.0 / PI) - 29.0 / 14.5) * 2.0;
        assert_eq!(front_traction_loss(&both, &inputs(5.0)), front);
        let rear = ((1.0f64 + 1.44 * 0.5) / 10.0).atan() * (180.0 / PI) * 2.0;
        assert_eq!(rear_traction_loss(&both, &inputs(5.0), Some(9.0)), rear);

        let no_yaw_rate = SlipInputs { yaw_rate: None, ..inputs(5.0) };
        assert_eq!(front_traction_loss(&both, &no_yaw_rate), 0.0);
        assert_eq!(rear_traction_loss(&both, &no_yaw_rate, Some(9.0)), 0.0);

        let no_steering = SlipInputs { steering: None, ..inputs(5.0) };
        assert_eq!(front_traction_loss(&both, &no_steering), (0.1f64).atan() * (180.0 / PI) * 2.0);

        let zero_ratio = DamsSettings { steering_ratio: Some(0.0), ..both };
        assert_eq!(front_traction_loss(&zero_ratio, &inputs(5.0)), 0.0);
        assert_eq!(rear_traction_loss(&both, &inputs(0.0), Some(9.0)), 0.0);
    }

    /// An unreadable `dams_front_tl_mode` sets `front_tl` and
    /// `lateral_velocity` to 0 on every tick, sliding or not.
    #[test]
    fn unreadable_mode_zeroes_both_channels() {
        let broken = settings(DamsMode::from_config(None));
        assert_eq!(broken.mode, DamsMode::Unreadable);
        for speed in [0.0, 30.0] {
            assert_eq!(front_traction_loss(&broken, &inputs(speed)), 0.0);
            assert_eq!(rear_traction_loss(&broken, &inputs(speed), Some(-3.0)), 0.0);
        }
    }

    /// An unreadable `dams_front_tl_factor` (or, in mode 2,
    /// `dams_front_steerratio`) zeroes only the slip angle being computed; the raw copy of mode 1 is unaffected.
    #[test]
    fn unreadable_factor_or_ratio_zeroes_the_slip_angle() {
        let front = DamsSettings { factor: None, ..settings(DamsMode::FrontSlip) };
        assert_eq!(front_traction_loss(&front, &inputs(30.0)), 0.0);
        assert_eq!(rear_traction_loss(&front, &inputs(30.0), Some(-3.0)), -3.0);

        let both = DamsSettings { factor: None, ..settings(DamsMode::FrontAndRearSlip) };
        assert_eq!(front_traction_loss(&both, &inputs(30.0)), 0.0);
        assert_eq!(rear_traction_loss(&both, &inputs(30.0), Some(-3.0)), 0.0);

        let no_ratio = DamsSettings { steering_ratio: None, ..settings(DamsMode::FrontAndRearSlip) };
        assert_eq!(front_traction_loss(&no_ratio, &inputs(30.0)), 0.0);
        let rear = ((1.0f64 + 1.44 * 0.5) / 10.0).atan() * (180.0 / PI) * 2.0;
        assert_eq!(rear_traction_loss(&no_ratio, &inputs(30.0), Some(-3.0)), rear);
    }
}
