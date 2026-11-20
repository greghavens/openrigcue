//! Rig attitude and height estimated from actuator positions, used by motion compensation when no
//! sensor reading applies: the blind-calibrated six-actuator fits, the four-corner and calibration-table
//! tilts, and the plane fit for the platform height.

use super::numeric::{divide, DivisionByZero};

/// Six 10-bit motor positions `m0..m5` (511 is the centre).
pub type MotorCounts = [f64; 6];

const CENTRE_COUNT: f64 = 511.0;

/// Pitch of a DOF Reality six-actuator rig in degrees; 0 while every motor is centred.
pub fn estimate_pitch(m: MotorCounts) -> f64 {
    if m.iter().all(|v| *v == CENTRE_COUNT) {
        return 0.0;
    }
    -0.006368 * ((m[5] - m[2]) + 0.5 * (m[4] - m[3]) + 0.5 * (m[0] - m[1])) - 0.2356
}

/// Roll of a DOF Reality six-actuator rig in degrees; 0 while every motor is centred.
pub fn estimate_roll(m: MotorCounts) -> f64 {
    if m.iter().all(|v| *v == CENTRE_COUNT) {
        return 0.0;
    }
    -0.005192 * ((m[0] + m[3]) - (m[1] + m[4])) - 0.07605
}

/// Yaw of a DOF Reality six-actuator rig in degrees; 0 while `m1..m4` are centred.
pub fn estimate_yaw(m: MotorCounts) -> f64 {
    if m[1..5].iter().all(|v| *v == CENTRE_COUNT) {
        return 0.0;
    }
    0.0031138 * ((m[1] + m[2]) - 6.0 * (m[3] + m[4])) + 15.8436
}

/// Which rotation a tilt estimate is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tilt {
    Pitch,
    Roll,
}

/// Corner actuator positions of a four-actuator rig.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Corners {
    pub front_left: f64,
    pub front_right: f64,
    pub rear_left: f64,
    pub rear_right: f64,
}

/// Pitch or roll in degrees from the corner positions (halved, millimetres to metres) and the actuator
/// spacing in millimetres. Both angles are computed and one is returned, so either spacing being
/// zero is an error.
pub fn corner_tilt(
    tilt: Tilt,
    corners: &Corners,
    spacing_lr_mm: f64,
    spacing_fb_mm: f64,
) -> Result<f64, DivisionByZero> {
    let metres = |v: f64| v / 2.0 / 1000.0;
    let (fl, fr, rl, rr) = (
        metres(corners.front_left),
        metres(corners.front_right),
        metres(corners.rear_left),
        metres(corners.rear_right),
    );
    let pitch = divide(fr + fl - rr - rl, spacing_fb_mm / 1000.0)?.atan().to_degrees();
    let roll = divide(fl + rl - fr - rr, spacing_lr_mm / 1000.0)?.atan().to_degrees();
    Ok(match tilt {
        Tilt::Pitch => pitch,
        Tilt::Roll => roll,
    })
}

/// Calibration point: left motor, right motor, angle in degrees.
type CalibrationPoint = (f64, f64, f64);

/// The regular pitch and roll calibration tables and the H4 ones.
const PITCH_REG: [CalibrationPoint; 5] =
    [(972.0, 51.0, 8.5), (741.0, 281.0, 4.5), (511.0, 511.0, 0.0), (281.0, 741.0, -3.8), (51.0, 972.0, -8.7)];
const ROLL_REG: [CalibrationPoint; 5] =
    [(51.0, 51.0, -13.6), (281.0, 281.0, -7.4), (511.0, 511.0, 0.0), (741.0, 741.0, 6.9), (972.0, 972.0, 12.7)];
const PITCH_H4: [CalibrationPoint; 5] =
    [(972.0, 972.0, 8.5), (741.0, 741.0, 4.5), (511.0, 511.0, 0.0), (281.0, 281.0, -3.8), (50.0, 50.0, -8.7)];
const ROLL_H4: [CalibrationPoint; 5] =
    [(51.0, 972.0, -13.6), (281.0, 741.0, -7.4), (511.0, 511.0, 0.0), (741.0, 281.0, 6.9), (972.0, 50.0, 12.7)];

/// Column of a calibration point: 0 = left motor, 1 = right motor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Column {
    Left,
    Right,
}

fn column(p: &CalibrationPoint, which: Column) -> f64 {
    match which {
        Column::Left => p.0,
        Column::Right => p.1,
    }
}

/// The bracketing pair of the table sorted by that column.
fn closest_points(value: f64, table: &[CalibrationPoint], which: Column) -> (CalibrationPoint, CalibrationPoint) {
    let mut sorted = table.to_vec();
    sorted.sort_by(|a, b| column(a, which).total_cmp(&column(b, which)));
    match sorted.iter().position(|p| value < column(p, which)) {
        Some(0) => (sorted[0], sorted[1]),
        Some(i) => (sorted[i - 1], sorted[i]),
        None => (sorted[sorted.len() - 2], sorted[sorted.len() - 1]),
    }
}

/// Linear interpolation of the angle between two calibration points along a column.
fn interpolate(value: f64, p1: CalibrationPoint, p2: CalibrationPoint, which: Column) -> f64 {
    let (x1, x2) = (column(&p1, which), column(&p2, which));
    if x1 == x2 {
        return p1.2;
    }
    p1.2 + (p2.2 - p1.2) * (value - x1) / (x2 - x1)
}

/// Pitch or roll of a seat mover from its front motors, corrected by the rear (heave) motor and
/// interpolated in the calibration tables; `h4_tables` for the H4 and P4 models. Callers pass 511
/// for a missing motor.
pub fn table_tilt(tilt: Tilt, front_left: f64, front_right: f64, rear_right: f64, h4_tables: bool) -> f64 {
    let (pitch_table, roll_table) = if h4_tables { (&PITCH_H4, &ROLL_H4) } else { (&PITCH_REG, &ROLL_REG) };
    let table = match tilt {
        Tilt::Pitch => pitch_table,
        Tilt::Roll => roll_table,
    };
    let heave_offset = rear_right - CENTRE_COUNT;
    let left = front_left - heave_offset;
    let right = front_right - heave_offset;
    let (a1, a2) = closest_points(left, table, Column::Left);
    let (b1, b2) = closest_points(right, table, Column::Right);
    (interpolate(left, a1, a2, Column::Left) + interpolate(right, b1, b2, Column::Right)) / 2.0
}

/// The three plane points are (nearly) collinear ("Degenerate geometry; the three vertex
/// points are collinear.").
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("degenerate geometry; the three vertex points are collinear")]
pub struct DegenerateGeometry;

/// Platform height defaults: actuator stroke, neutral count of the 12-bit scale, platform width and
/// length in metres.
const STROKE_MM: f64 = 97.651;
const NEUTRAL_COUNT: i64 = 2048;
const WIDTH_M: f64 = 0.762;
const LENGTH_M: f64 = 0.762;
/// Default polarity: actuators 0, 2, 4 extend downwards.
const POLARITY: [f64; 6] = [-1.0, 1.0, -1.0, 1.0, -1.0, 1.0];
/// Default vertex map (rear left, rear right, front) with weights (0.5, 0.5) for every vertex.
const VERTICES: [(usize, usize); 3] = [(0, 3), (1, 4), (2, 5)];
const VERTEX_WEIGHT: f64 = 0.5;
const COLLINEAR_LIMIT: f64 = 1e-12;

/// Determinant of the 3×3 matrix with rows `(a, b, c)`, `(d, e, f)`, `(g, h, i)`.
#[allow(clippy::too_many_arguments)]
fn det3(a: f64, b: f64, c: f64, d: f64, e: f64, f: f64, g: f64, h: f64, i: f64) -> f64 {
    a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g)
}

/// Height `c` at the origin of the plane `z = a x + b y + c` through three points (Cramer's rule).
fn plane_height(p: [[f64; 3]; 3]) -> Result<f64, DegenerateGeometry> {
    let [[x1, y1, z1], [x2, y2, z2], [x3, y3, z3]] = p;
    let d = det3(x1, y1, 1.0, x2, y2, 1.0, x3, y3, 1.0);
    if d.abs() < COLLINEAR_LIMIT {
        return Err(DegenerateGeometry);
    }
    Ok(det3(x1, y1, z1, x2, y2, z2, x3, y3, z3) / d)
}

/// Height of the platform centre in millimetres from the six 10-bit motor positions.
///
/// With the defaults above: each count is rescaled to 12 bits
/// (0..1023 to 0..4095, truncated), offset from the neutral 2048 and scaled by `stroke_mm / 4096`, signed
/// by the polarity; each vertex (rear left, rear right, front) averages its two actuators; the plane
/// through the vertices at (-W/2, -L/2), (W/2, -L/2) and (0, L/2) gives the height at the centre.
pub fn center_height_mm(counts: [i64; 6]) -> Result<f64, DegenerateGeometry> {
    let mm_per_count = STROKE_MM / 4096.0;
    let signed: [f64; 6] = std::array::from_fn(|i| {
        let twelve_bit = (counts[i] as f64 * 4095.0 / 1023.0).trunc() as i64;
        POLARITY[i] * ((twelve_bit - NEUTRAL_COUNT) as f64 * mm_per_count)
    });
    let [rear_left, rear_right, front] = VERTICES.map(|(a, b)| VERTEX_WEIGHT * signed[a] + VERTEX_WEIGHT * signed[b]);
    let (half_w, half_l) = (WIDTH_M * -0.5, LENGTH_M * -0.5);
    let centre_m = plane_height([
        [half_w, half_l, rear_left / 1000.0],
        [-half_w, half_l, rear_right / 1000.0],
        [0.0, -half_l, front / 1000.0],
    ])?;
    Ok(centre_m * 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Six-actuator pitch, roll and yaw: centred guards and the linear fits.
    #[test]
    fn blind_calibrated_estimators() {
        assert_eq!(estimate_pitch([511.0; 6]), 0.0);
        assert_eq!(estimate_roll([511.0; 6]), 0.0);
        let m = [611.0, 511.0, 511.0, 611.0, 511.0, 511.0];
        assert_eq!(estimate_roll(m), -0.005192 * ((611.0 + 611.0) - (511.0 + 511.0)) - 0.07605);
        assert_eq!(
            estimate_pitch(m),
            -0.006368 * ((511.0 - 511.0) + 0.5 * (511.0 - 611.0) + 0.5 * (611.0 - 511.0)) - 0.2356
        );
        // The yaw estimate only looks at m1..m4.
        assert_eq!(estimate_yaw([0.0, 511.0, 511.0, 511.0, 511.0, 0.0]), 0.0);
        assert_eq!(
            estimate_yaw([0.0, 600.0, 511.0, 511.0, 511.0, 0.0]),
            0.0031138 * ((600.0 + 511.0) - 6.0 * (511.0 + 511.0)) + 15.8436
        );
    }

    /// Four-corner tilt: halved positions in metres over the spacing; a zero spacing is an error.
    #[test]
    fn corner_tilt_from_four_actuators() {
        let c = Corners { front_left: 200.0, front_right: 200.0, rear_left: 0.0, rear_right: 0.0 };
        assert_eq!(corner_tilt(Tilt::Pitch, &c, 600.0, 400.0), Ok((0.2f64 / 0.4).atan().to_degrees()));
        assert_eq!(corner_tilt(Tilt::Roll, &c, 600.0, 400.0), Ok(0.0));
        let left_up = Corners { front_left: 300.0, rear_left: 300.0, ..Default::default() };
        assert_eq!(corner_tilt(Tilt::Roll, &left_up, 600.0, 400.0), Ok((0.3f64 / 0.6).atan().to_degrees()));
        assert_eq!(corner_tilt(Tilt::Pitch, &c, 0.0, 400.0), Err(DivisionByZero));
    }

    /// Seat-mover tilt, bracketing and interpolation on the regular and H4 tables.
    #[test]
    fn calibration_table_tilt() {
        assert_eq!(table_tilt(Tilt::Pitch, 511.0, 511.0, 511.0, false), 0.0);
        // Halfway between (741, 281, 4.5) and (972, 51, 8.5) on both columns.
        assert_eq!(table_tilt(Tilt::Pitch, 856.5, 166.0, 511.0, false), 6.5);
        // The rear motor shifts both front motors: 611/611 with the rear at 611 reads as centred.
        assert_eq!(table_tilt(Tilt::Roll, 611.0, 611.0, 611.0, false), 0.0);
        // Beyond the table: the last pair extrapolates.
        assert_eq!(
            table_tilt(Tilt::Roll, 1023.0, 1023.0, 511.0, false),
            6.9 + (12.7 - 6.9) * (1023.0 - 741.0) / (972.0 - 741.0)
        );
        // H4 tables: both front motors move together for pitch.
        assert_eq!(table_tilt(Tilt::Pitch, 741.0, 741.0, 511.0, true), 4.5);
    }

    /// Platform height: neutral and the plane height for one raised actuator.
    #[test]
    fn centre_height_plane_fit() {
        // 511 -> 511 * 4095 / 1023 truncated = 2045 on every actuator: the alternating polarity cancels.
        assert_eq!(center_height_mm([511; 6]), Ok(0.0));
        // m3 at 1023 -> 4095: rear left vertex = 0.5 * (3 + 2047) counts; height = RL / 4 for this plane.
        let k = STROKE_MM / 4096.0;
        let rear_left_mm = 0.5 * (-(-3.0 * k)) + 0.5 * (2047.0 * k);
        let h = center_height_mm([511, 511, 511, 1023, 511, 511]).unwrap();
        assert!((h - rear_left_mm / 4.0).abs() < 1e-12, "{h}");
        // Front pair raised: the front vertex weighs half.
        let front_mm = 0.5 * (-(-3.0 * k)) + 0.5 * (2047.0 * k);
        let h = center_height_mm([511, 511, 511, 511, 511, 1023]).unwrap();
        assert!((h - front_mm / 2.0).abs() < 1e-12, "{h}");
    }

    /// Collinear plane points are an error.
    #[test]
    fn degenerate_plane() {
        assert_eq!(plane_height([[0.0, 0.0, 1.0], [1.0, 1.0, 2.0], [2.0, 2.0, 3.0]]), Err(DegenerateGeometry));
    }
}
