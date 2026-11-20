//! Quantities the Codemasters-family listeners derive from the car frame: orientation angles, the velocity
//! projections, and the numerically differentiated vertical acceleration.

use super::packet::Frame;
use openrigcue_model::native_value::LegacyValue;

/// Radians to degrees: `radians * 180.0 / π`.
pub fn radians_to_degrees(radians: f64) -> f64 {
    radians * 180.0 / std::f64::consts::PI
}

/// Roll in degrees from the right vector's y component: its arcsine.
/// `None` outside the arcsine's domain (|ry| > 1).
pub fn roll_degrees(ry: f64) -> Option<f64> {
    (ry.abs() <= 1.0 || ry.is_nan()).then(|| radians_to_degrees(ry.asin()))
}

/// Yaw in degrees: `atan2(rx, rz)`.
pub fn yaw_degrees(rx: f64, rz: f64) -> f64 {
    radians_to_degrees(rx.atan2(rz))
}

/// Pitch in degrees:
/// `atan2(rx*fz - rz*fx, fy)` in degrees, plus 90.
pub fn pitch_degrees(rx: f64, rz: f64, fx: f64, fy: f64, fz: f64) -> f64 {
    radians_to_degrees((rx * fz - rz * fx).atan2(fy)) + 90.0
}

/// Pitch of a frame ([`pitch_degrees`]).
pub fn frame_pitch(frame: &Frame) -> f64 {
    let [rx, _, rz] = frame.right;
    let [fx, fy, fz] = frame.forward;
    pitch_degrees(rx, rz, fx, fy, fz)
}

/// Yaw of a frame ([`yaw_degrees`]).
pub fn frame_yaw(frame: &Frame) -> f64 {
    yaw_degrees(frame.right[0], frame.right[2])
}

/// World velocity projected on the car's right axis: `rx*xv + ry*yv + rz*zv` (the listeners' `lateral_velocity`).
pub fn lateral_speed(frame: &Frame) -> f64 {
    let [rx, ry, rz] = frame.right;
    let [xv, yv, zv] = frame.velocity;
    rx * xv + ry * yv + rz * zv
}

/// World velocity projected on the car's up axis `forward × right`:
/// `ux = fy*rz - fz*ry`, `uy = fz*rx - fx*rz`, `uz = fx*ry - fy*rx`, then `ux*xv + uy*yv + uz*zv`.
pub fn vertical_speed(frame: &Frame) -> f64 {
    let [rx, ry, rz] = frame.right;
    let [fx, fy, fz] = frame.forward;
    let [xv, yv, zv] = frame.velocity;
    let ux = fy * rz - fz * ry;
    let uy = fz * rx - fx * rz;
    let uz = fx * ry - fy * rx;
    ux * xv + uy * yv + uz * zv
}

/// How a listener differentiates the vertical speed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaveRule {
    /// dirt, f1, grid, wrc: a zero time step becomes 1e-4, and a zero result repeats the last non-zero one.
    HoldLastNonZero,
    /// tdusc: a time step `<= 0` becomes 1e-4 and the raw result is used.
    Raw,
}

/// The vertical-acceleration estimate kept by the `vertical_acceleration` channel
/// (previous time, previous vertical speed, previous vertical acceleration, all 0 at start).
#[derive(Debug, Clone, PartialEq)]
pub struct HeaveEstimator {
    rule: HeaveRule,
    previous_time: f64,
    previous_vertical_speed: f64,
    previous_acceleration: LegacyValue,
}

/// Time step substituted for a degenerate one.
const MIN_TIME_STEP: f64 = 0.0001;

impl HeaveEstimator {
    pub fn new(rule: HeaveRule) -> Self {
        HeaveEstimator {
            rule,
            previous_time: 0.0,
            previous_vertical_speed: 0.0,
            previous_acceleration: LegacyValue::Int(0),
        }
    }

    /// One `vertical_acceleration` evaluation for a packet with game time `time` (s) and
    /// vertical speed `vertical_speed` (m/s). Returns the unscaled acceleration (m/s²).
    pub fn next(&mut self, time: f64, vertical_speed: f64) -> f64 {
        self.next_native(time, vertical_speed).number().expect("heave always returns a number")
    }

    /// The held initial zero is an integer; calculated and remembered nonzero results are floats.
    pub(crate) fn next_native(&mut self, time: f64, vertical_speed: f64) -> LegacyValue {
        let mut delta = time - self.previous_time;
        let degenerate = match self.rule {
            HeaveRule::HoldLastNonZero => delta == 0.0,
            HeaveRule::Raw => delta <= 0.0,
        };
        if degenerate {
            delta = MIN_TIME_STEP;
        }
        let acceleration = (vertical_speed - self.previous_vertical_speed) / delta;
        self.previous_vertical_speed = vertical_speed;
        self.previous_time = time;
        match self.rule {
            HeaveRule::Raw => LegacyValue::Float(acceleration),
            HeaveRule::HoldLastNonZero => {
                if acceleration != 0.0 {
                    self.previous_acceleration = LegacyValue::Float(acceleration);
                }
                if acceleration == 0.0 {
                    self.previous_acceleration.clone()
                } else {
                    LegacyValue::Float(acceleration)
                }
            }
        }
    }
}

/// π / 2 as the tdusc listener uses it (`1.57079637`, rounded to float32 precision).
const TDUSC_HALF_PI: f64 = 1.57079637;
/// π as the tdusc listener uses it (`3.14159274`).
const TDUSC_PI: f64 = 3.14159274;

/// Degrees (pitch, yaw, roll) of the tdusc car frame.
///
/// Both packet vectors are negated, the up
/// vector is `r' × f'`, the matrix rows are `[r'x, r'z, r'y]`, `[ux, uz, uy]`, `[f'x, f'z, f'y]`, converted to
/// a quaternion and then to pitch, yaw and roll; roll is folded back into its limit and negated.
/// The quaternion branches always take the square root of a value >= 1, so (unlike the arcsine roll) this
/// never fails for finite input.
pub fn tdusc_orientation(frame: &Frame) -> (f64, f64, f64) {
    let [rx, ry, rz] = frame.right.map(|v| -v);
    let [fx, fy, fz] = frame.forward.map(|v| -v);
    let ux = ry * fz - rz * fy;
    let uy = rz * fx - rx * fz;
    let uz = rx * fy - ry * fx;
    let matrix = [[rx, rz, ry], [ux, uz, uy], [fx, fz, fy]];
    let (pitch, yaw, roll) = quaternion_to_pitch_yaw_roll(matrix_to_quaternion(&matrix));
    let to_degrees = 180.0 / std::f64::consts::PI;
    let roll = fold_angle(roll, TDUSC_HALF_PI);
    (pitch * to_degrees, yaw * to_degrees, -roll * to_degrees)
}

/// The quaternion of a rotation matrix; returns `(x, y, z, w)`.
fn matrix_to_quaternion(m: &[[f64; 3]; 3]) -> [f64; 4] {
    let trace = m[0][0] + m[1][1] + m[2][2];
    let scale = |radicand: f64| radicand.sqrt() * 2.0;
    if trace > 0.0 {
        let s = scale(trace + 1.0);
        [(m[2][1] - m[1][2]) / s, (m[0][2] - m[2][0]) / s, (m[1][0] - m[0][1]) / s, 0.25 * s]
    } else if m[0][0] > m[1][1] && m[0][0] > m[2][2] {
        let s = scale(1.0 + m[0][0] - m[1][1] - m[2][2]);
        [0.25 * s, (m[0][1] + m[1][0]) / s, (m[0][2] + m[2][0]) / s, (m[2][1] - m[1][2]) / s]
    } else if m[1][1] > m[2][2] {
        let s = scale(1.0 + m[1][1] - m[0][0] - m[2][2]);
        [(m[0][1] + m[1][0]) / s, 0.25 * s, (m[1][2] + m[2][1]) / s, (m[0][2] - m[2][0]) / s]
    } else {
        let s = scale(1.0 + m[2][2] - m[0][0] - m[1][1]);
        [(m[0][2] + m[2][0]) / s, (m[1][2] + m[2][1]) / s, 0.25 * s, (m[1][0] - m[0][1]) / s]
    }
}

/// Pitch, yaw and roll of the quaternion `(x, y, z, w)`.
fn quaternion_to_pitch_yaw_roll([x, y, z, w]: [f64; 4]) -> (f64, f64, f64) {
    let pitch = (2.0 * (y * w + x * z)).atan2(1.0 - 2.0 * (x * x + y * y));
    let yaw = clamp_unit_like_python(2.0 * (x * w - y * z)).asin();
    let roll = (2.0 * (x * y + z * w)).atan2(1.0 - 2.0 * (x * x + z * z));
    (pitch, yaw, roll)
}

/// The clamp of the arcsine argument: `t` is kept while `t < 1.0` and becomes 1.0 otherwise, then
/// is kept while `t > -1.0` and becomes -1.0 otherwise. Unlike `f64::clamp`, a NaN fails the first test and becomes 1.0.
fn clamp_unit_like_python(t: f64) -> f64 {
    let t = if t < 1.0 { t } else { 1.0 };
    if t > -1.0 {
        t
    } else {
        -1.0
    }
}

/// Folds an angle: one beyond `±limit` is mirrored back,
/// `pi * sign(angle) - angle`, using `3.14159274` for pi.
fn fold_angle(angle: f64, limit: f64) -> f64 {
    let magnitude = angle.abs();
    if magnitude > limit {
        TDUSC_PI * (angle / magnitude) - angle
    } else {
        angle
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f64 = 1e-9;

    #[test]
    fn convert_rad_to_deg_divides_by_pi_last() {
        // Radians to degrees: radians * 180.0 / π
        assert_eq!(radians_to_degrees(1.0), 180.0 / std::f64::consts::PI);
        assert_eq!(radians_to_degrees(0.1).to_bits(), (0.1f64 * 180.0 / std::f64::consts::PI).to_bits());
    }

    #[test]
    fn vmatrix_helpers_match_their_formulas() {
        // Roll, yaw and pitch from the frame vectors
        assert!((roll_degrees(0.5).unwrap() - 30.0).abs() < EPS);
        assert_eq!(roll_degrees(1.0000001), None);
        assert!((yaw_degrees(1.0, 0.0) - 90.0).abs() < EPS);
        // right = +x, forward = +y: atan2(0, 1) = 0 -> 90; forward = +z: atan2(1, 0) -> 180.
        assert_eq!(pitch_degrees(1.0, 0.0, 0.0, 1.0, 0.0), 90.0);
        assert_eq!(pitch_degrees(1.0, 0.0, 0.0, 0.0, 1.0), 180.0);
        // All-zero vectors (menus) give exactly 90 too: atan2(0, 0) = 0.
        assert_eq!(pitch_degrees(0.0, 0.0, 0.0, 0.0, 0.0), 90.0);
    }

    #[test]
    fn projections_use_the_documented_cross_product() {
        let frame = Frame { velocity: [1.0, 2.0, 3.0], right: [1.0, 0.0, 0.0], forward: [0.0, 0.0, 1.0] };
        assert_eq!(lateral_speed(&frame), 1.0);
        // up = (fy*rz - fz*ry, fz*rx - fx*rz, fx*ry - fy*rx) = (0, 1, 0)
        assert_eq!(vertical_speed(&frame), 2.0);
    }

    #[test]
    fn heave_hold_rule_matches_dirt_vertical_acceleration() {
        // The dirt-style vertical acceleration estimate
        let mut h = HeaveEstimator::new(HeaveRule::HoldLastNonZero);
        assert_eq!(h.next(2.0, 1.0), 0.5); // first frame: previous_time 0, previous speed 0
        assert_eq!(h.next(2.0, 1.0), 0.5); // same time and speed: 0 / 1e-4 = 0 -> hold 0.5
        assert_eq!(h.next(2.0, 2.0), 1.0 / 0.0001); // zero step becomes 1e-4
        assert_eq!(h.next(3.0, 2.0), 1.0 / 0.0001); // zero result repeats the last non-zero one
    }

    #[test]
    fn heave_raw_rule_matches_tdusc_vertical_acceleration() {
        // The tdusc-style vertical acceleration estimate: delta <= 0 -> 1e-4, no hold.
        let mut h = HeaveEstimator::new(HeaveRule::Raw);
        assert_eq!(h.next(1.0, 2.0), 2.0);
        assert_eq!(h.next(0.5, 2.0), 0.0);
        assert_eq!(h.next(0.5, 3.0), 1.0 / 0.0001);
    }

    #[test]
    fn tdusc_constants_are_the_module_literals() {
        // The tdusc constants are binary64 0x3ff921fb5fdd3550 and 0x400921fb5fdd3550.
        assert_eq!(TDUSC_HALF_PI.to_bits(), 0x3ff9_21fb_5fdd_3550);
        assert_eq!(TDUSC_PI.to_bits(), 0x4009_21fb_5fdd_3550);
    }

    #[test]
    fn tdusc_orientation_of_the_identity_matrix_is_zero() {
        // Orientation: r = (-1,0,0), f = (0,-1,0) negate to rows [1,0,0], [0,1,0], [0,0,1].
        let frame = Frame { velocity: [0.0; 3], right: [-1.0, 0.0, 0.0], forward: [0.0, -1.0, 0.0] };
        let (pitch, yaw, roll) = tdusc_orientation(&frame);
        assert_eq!((pitch, yaw, roll.abs()), (0.0, 0.0, 0.0));
    }

    #[test]
    fn tdusc_orientation_half_turn_uses_the_largest_diagonal_branch_and_folds_roll() {
        // Rows [1,0,0], [0,-1,0], [0,0,-1]: trace -1, m00 largest -> q = (1, 0, 0, -0); the signed zeros of the
        // negated vectors make pitch atan2(-0, -1) = -pi; roll atan2(0, -1) = pi folds to 3.14159274 - pi.
        let frame = Frame { velocity: [0.0; 3], right: [-1.0, 0.0, 0.0], forward: [0.0, 1.0, 0.0] };
        let (pitch, yaw, roll) = tdusc_orientation(&frame);
        let to_degrees = 180.0 / std::f64::consts::PI;
        assert_eq!(pitch, -std::f64::consts::PI * to_degrees);
        assert_eq!(yaw, 0.0);
        assert_eq!(roll, -(TDUSC_PI - std::f64::consts::PI) * to_degrees);
    }

    #[test]
    fn tdusc_orientation_of_zero_vectors_takes_the_last_branch() {
        // Menus send zero vectors: trace 0 and no larger diagonal -> S = sqrt(1) * 2 = 2, q = (0, 0, 0.5, 0).
        assert_eq!(matrix_to_quaternion(&[[0.0; 3]; 3]), [0.0, 0.0, 0.5, 0.0]);
        let (pitch, yaw, roll) = tdusc_orientation(&Frame::default());
        assert_eq!((pitch, yaw), (0.0, 0.0));
        assert!(roll.abs() < 1e-12);
    }

    #[test]
    fn get_pyr_from_quaternion_clamp_sends_nan_to_one() {
        // The arcsine clamp: `NaN < 1.0` is false -> 1.0, so yaw = asin(1.0) = 90 degrees.
        assert_eq!(clamp_unit_like_python(f64::NAN), 1.0);
        assert_eq!(
            (clamp_unit_like_python(1.5), clamp_unit_like_python(-1.5), clamp_unit_like_python(-0.25)),
            (1.0, -1.0, -0.25)
        );
        let frame = Frame { velocity: [0.0; 3], right: [f64::NAN, 0.0, 0.0], forward: [0.0, -1.0, 0.0] };
        let (pitch, yaw, roll) = tdusc_orientation(&frame);
        assert_eq!(yaw, 1.0f64.asin() * (180.0 / std::f64::consts::PI));
        assert!(pitch.is_nan() && roll.is_nan());
    }

    #[test]
    fn loop_angle_rad_mirrors_beyond_the_limit() {
        assert_eq!(fold_angle(1.0, TDUSC_HALF_PI), 1.0);
        assert_eq!(fold_angle(2.0, TDUSC_HALF_PI), TDUSC_PI - 2.0);
        assert_eq!(fold_angle(-2.0, TDUSC_HALF_PI), -TDUSC_PI + 2.0);
    }
}
