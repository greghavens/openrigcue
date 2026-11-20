use serde::{Deserialize, Serialize};

/// One value per wheel, in the order front-left, front-right, rear-left, rear-right.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Wheels<T>(pub [T; 4]);

/// Wheel position, used to index [`Wheels`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Corner {
    FrontLeft = 0,
    FrontRight = 1,
    RearLeft = 2,
    RearRight = 3,
}

impl Corner {
    pub const ALL: [Corner; 4] = [Corner::FrontLeft, Corner::FrontRight, Corner::RearLeft, Corner::RearRight];
}

impl<T: Copy> Wheels<T> {
    pub fn get(&self, corner: Corner) -> T {
        self.0[corner as usize]
    }
}

/// Transmission state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Gear {
    Reverse,
    #[default]
    Neutral,
    Forward(u8),
}

/// Race flag currently shown to the driver.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Flag {
    #[default]
    None,
    Green,
    Yellow,
    YellowWaving,
    Blue,
    White,
    Red,
    Black,
    Checkered,
    Disqualified,
    Caution,
}

/// The normalized telemetry frame.
///
/// Conventions: angles in degrees, rates in degrees per second, accelerations in g, velocities in m/s.
/// Pitch is positive nose up, roll positive right side down, yaw positive clockwise seen from above.
/// Lateral acceleration is positive to the right, longitudinal acceleration positive when braking,
/// vertical acceleration positive upward.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    pub pitch: f32,
    pub roll: f32,
    pub yaw: f32,
    pub pitch_rate: f32,
    pub roll_rate: f32,
    pub yaw_rate: f32,

    pub accel_lateral: f32,
    pub accel_longitudinal: f32,
    pub accel_vertical: f32,

    pub vel_lateral: f32,
    pub vel_longitudinal: f32,
    pub vel_vertical: f32,
    /// Ground speed, never negative.
    pub speed: f32,

    /// Rear traction-loss cue: raw lateral velocity, or slip angle in degrees when a slip model is active.
    pub slip_rear: f32,
    /// Front traction-loss cue, same units as `slip_rear`.
    pub slip_front: f32,

    pub rpm: f32,
    pub rpm_max: f32,
    pub gear: Gear,
    pub throttle: f32,
    pub brake: f32,
    pub clutch: f32,

    pub steer_angle: f32,
    pub steer_range: f32,

    pub suspension_travel: Wheels<f32>,
    pub suspension_velocity: Wheels<f32>,
    /// Slip ratio per wheel.
    pub wheel_slip: Wheels<f32>,
    /// Kerb/rumble intensity per wheel.
    pub rumble: Wheels<f32>,
    pub tyre_temp: Wheels<f32>,
    pub tyre_wear: Wheels<f32>,
    pub tyre_pressure: Wheels<f32>,

    pub oil_temp: f32,
    pub oil_pressure: f32,
    pub water_temp: f32,
    pub water_pressure: f32,
    pub fuel_pressure: f32,
    pub fuel_level: f32,
    pub oil_level: f32,
    pub water_level: f32,
    pub boost: f32,

    pub flag: Flag,
    pub flag_raw: i32,
    pub spotter_left: bool,
    pub spotter_right: bool,
    pub pit_limiter: bool,
    pub lap: u32,
    pub laps_completed: u32,
    pub lap_distance: f32,
    pub lap_fraction: f32,
    pub race_laps: u32,

    pub abs_active: bool,
    pub afterburner: bool,
    pub cannon: bool,
    pub missile: bool,
    /// Canopy position 0 (closed) .. 1 (open).
    pub canopy: f32,
    /// Landing gear position 0 (up) .. 1 (down), left, nose, right.
    pub landing_gear: [f32; 3],
    /// Touchdown events, left, nose, right.
    pub touchdown: [bool; 3],
    pub turbulence: f32,

    pub vehicle: Option<String>,
    pub track: Option<String>,
    pub paused: bool,
}

impl Frame {
    /// The frame outputs see while no game is live.
    pub fn idle() -> Self {
        Frame {
            pitch: 0.0,
            roll: 0.0,
            yaw: 0.0,
            pitch_rate: 0.0,
            roll_rate: 0.0,
            yaw_rate: 0.0,
            accel_lateral: 0.0,
            accel_longitudinal: 0.0,
            accel_vertical: 0.0,
            vel_lateral: 0.0,
            vel_longitudinal: 0.0,
            vel_vertical: 0.0,
            speed: 0.0,
            slip_rear: 0.0,
            slip_front: 0.0,
            rpm: 0.0,
            rpm_max: 20_000.0,
            gear: Gear::Neutral,
            throttle: 0.0,
            brake: 0.0,
            clutch: 0.0,
            steer_angle: 0.0,
            steer_range: 0.0,
            suspension_travel: Wheels::default(),
            suspension_velocity: Wheels::default(),
            wheel_slip: Wheels::default(),
            rumble: Wheels::default(),
            tyre_temp: Wheels::default(),
            tyre_wear: Wheels::default(),
            tyre_pressure: Wheels::default(),
            oil_temp: 0.0,
            oil_pressure: 0.0,
            water_temp: 0.0,
            water_pressure: 0.0,
            fuel_pressure: 0.0,
            fuel_level: 0.0,
            oil_level: 0.0,
            water_level: 0.0,
            boost: 0.0,
            flag: Flag::None,
            flag_raw: 0,
            spotter_left: false,
            spotter_right: false,
            pit_limiter: false,
            lap: 0,
            laps_completed: 0,
            lap_distance: 0.0,
            lap_fraction: 0.0,
            race_laps: 0,
            abs_active: false,
            afterburner: false,
            cannon: false,
            missile: false,
            canopy: 0.0,
            landing_gear: [0.0; 3],
            touchdown: [false; 3],
            turbulence: 0.0,
            vehicle: None,
            track: None,
            paused: false,
        }
    }

    /// Replace every non-finite number by zero and clamp speed at zero.
    pub fn sanitize(&mut self) {
        fn fix(v: &mut f32) {
            if !v.is_finite() {
                *v = 0.0;
            }
        }
        for v in [
            &mut self.pitch,
            &mut self.roll,
            &mut self.yaw,
            &mut self.pitch_rate,
            &mut self.roll_rate,
            &mut self.yaw_rate,
            &mut self.accel_lateral,
            &mut self.accel_longitudinal,
            &mut self.accel_vertical,
            &mut self.vel_lateral,
            &mut self.vel_longitudinal,
            &mut self.vel_vertical,
            &mut self.speed,
            &mut self.slip_rear,
            &mut self.slip_front,
            &mut self.rpm,
            &mut self.rpm_max,
            &mut self.throttle,
            &mut self.brake,
            &mut self.clutch,
            &mut self.steer_angle,
            &mut self.steer_range,
            &mut self.canopy,
            &mut self.turbulence,
        ] {
            fix(v);
        }
        for wheels in
            [&mut self.suspension_travel, &mut self.suspension_velocity, &mut self.wheel_slip, &mut self.rumble]
        {
            wheels.0.iter_mut().for_each(fix);
        }
        self.landing_gear.iter_mut().for_each(fix);
        if self.speed < 0.0 {
            self.speed = 0.0;
        }
    }
}

impl Default for Frame {
    fn default() -> Self {
        Self::idle()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_replaces_non_finite_and_negative_speed() {
        let mut f = Frame::idle();
        f.pitch = f32::NAN;
        f.speed = -3.0;
        f.wheel_slip.0[2] = f32::INFINITY;
        f.sanitize();
        assert_eq!(f.pitch, 0.0);
        assert_eq!(f.speed, 0.0);
        assert_eq!(f.wheel_slip.get(Corner::RearLeft), 0.0);
    }
}
