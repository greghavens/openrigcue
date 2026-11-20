//! Value -> sample-number lookup tables and the numeric helpers the effect engine uses.

/// One table row: inclusive `[min, max]` range and the sample it selects.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Band {
    pub min: f64,
    pub max: f64,
    pub sample: i64,
}

const fn b(min: f64, max: f64, sample: i64) -> Band {
    Band { min, max, sample }
}

/// The sample of the first row whose inclusive range holds
/// `value`, or -1 (gaps between rows fall through).
pub fn lookup(table: &[Band], value: f64) -> i64 {
    table.iter().find(|r| r.min <= value && value <= r.max).map_or(-1, |r| r.sample)
}

/// Wheel slip table.
pub const WHEEL_SLIP: [Band; 7] = [
    b(0.0, 1.99, 0),
    b(2.0, 3.49, 1),
    b(3.5, 5.99, 2),
    b(6.0, 7.49, 3),
    b(7.5, 8.99, 4),
    b(9.0, 10.49, 5),
    b(10.5, 10000.0, 6),
];

/// Rear traction loss table (percent of the per-game maximum).
pub const TRACTION_LOSS_REAR: [Band; 7] = [
    b(0.0, 0.99, 0),
    b(1.0, 1.99, 1),
    b(2.0, 3.99, 2),
    b(4.0, 6.99, 3),
    b(7.0, 19.99, 4),
    b(20.0, 49.99, 5),
    b(50.0, 10000.0, 6),
];

/// Lateral table (absolute lateral acceleration).
pub const LATERAL: [Band; 3] = [b(0.0, 4.99, 0), b(5.0, 11.99, 1), b(12.0, 1000.0, 2)];

/// Brake table; the second row overlaps the first, so values <= -30 hit row 1 (silence).
pub const BRAKE: [Band; 4] = [b(-10000.0, -29.9, 0), b(-30.0, -5.0, 2), b(-4.99, -4.0, 1), b(-3.9, 10000.0, 0)];

/// Acceleration table.
pub const ACCEL: [Band; 4] = [b(-10000.0, 1.9, 0), b(2.0, 3.9, 1), b(4.0, 30.0, 2), b(30.1, 10000.0, 0)];

/// Impact table.
pub const IMPACT: [Band; 5] =
    [b(-10000.0, -50.0, 6), b(-49.99, -30.0, 5), b(-29.99, 29.99, 0), b(30.0, 49.99, 5), b(50.0, 10000.0, 6)];

/// Suspension table: rows of velocity type 0 and 1.
pub const SUSPENSION: [[Band; 8]; 2] = [
    [
        b(0.0, 0.3, 0),
        b(0.31, 0.49, 2),
        b(0.5, 1.99, 3),
        b(2.0, 3.99, 4),
        b(4.0, 5.99, 4),
        b(6.0, 7.99, 5),
        b(8.0, 10.99, 5),
        b(11.0, 10000.0, 6),
    ],
    TYPE_ONE,
];

/// Road bump table: as the suspension table except the last type-0 bound (1000).
pub const ROAD_BUMP: [[Band; 8]; 2] = [
    [
        b(0.0, 0.3, 0),
        b(0.31, 0.49, 2),
        b(0.5, 1.99, 3),
        b(2.0, 3.99, 4),
        b(4.0, 5.99, 4),
        b(6.0, 7.99, 5),
        b(8.0, 10.99, 5),
        b(11.0, 1000.0, 6),
    ],
    TYPE_ONE,
];

const TYPE_ONE: [Band; 8] = [
    b(0.0, 0.1, 0),
    b(0.11, 0.99, 0),
    b(1.0, 1.99, 1),
    b(2.0, 3.99, 2),
    b(4.0, 5.99, 3),
    b(6.0, 7.99, 4),
    b(8.0, 10.99, 5),
    b(11.0, 10000.0, 6),
];

/// Rounds a float to `ndigits` decimals: correctly rounded, ties to even on the exact value.
pub fn py_round(x: f64, ndigits: usize) -> f64 {
    format!("{x:.ndigits$}").parse().unwrap_or(x)
}

/// Linear map clamped to `[out_min, out_max]`
/// (NaN maps to `out_max`).
pub fn map_clamped(x: f64, in_min: f64, in_max: f64, out_min: f64, out_max: f64) -> f64 {
    let r = (x - in_min) * (out_max - out_min) / (in_max - in_min) + out_min;
    let r = if r <= out_max { r } else { out_max };
    if out_min <= r {
        r
    } else {
        out_min
    }
}

/// Linear map clamped above only.
/// `None` for a zero-width input range (`in_max == in_min`).
pub fn map_upper_clamped(x: f64, in_min: f64, in_max: f64, out_min: f64, out_max: f64) -> Option<f64> {
    if in_max == in_min {
        return None;
    }
    let r = (x - in_min) * (out_max - out_min) / (in_max - in_min) + out_min;
    Some(if r > out_max { out_max } else { r })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_vibration_file_bounds_and_gaps() {
        assert_eq!(lookup(&WHEEL_SLIP, 1.99), 0);
        assert_eq!(lookup(&WHEEL_SLIP, 1.995), -1); // gap between 1.99 and 2
        assert_eq!(lookup(&WHEEL_SLIP, 2.0), 1);
        assert_eq!(lookup(&WHEEL_SLIP, 10.5), 6);
        assert_eq!(lookup(&WHEEL_SLIP, -0.1), -1);
        assert_eq!(lookup(&LATERAL, 5.0), 1);
        assert_eq!(lookup(&LATERAL, 1000.01), -1);
    }

    #[test]
    fn brake_overlap_and_accel() {
        assert_eq!(lookup(&BRAKE, -40.0), 0); // first row wins
        assert_eq!(lookup(&BRAKE, -29.95), 0);
        assert_eq!(lookup(&BRAKE, -29.89), 2);
        assert_eq!(lookup(&BRAKE, -4.5), 1);
        assert_eq!(lookup(&BRAKE, -3.95), -1);
        assert_eq!(lookup(&ACCEL, 30.05), -1);
        assert_eq!(lookup(&ACCEL, 4.0), 2);
        assert_eq!(lookup(&IMPACT, -55.0), 6);
        assert_eq!(lookup(&IMPACT, 30.0), 5);
    }

    #[test]
    fn get_vibration_file_by_type_rows() {
        assert_eq!(lookup(&SUSPENSION[0], 0.31), 2);
        assert_eq!(lookup(&SUSPENSION[1], 0.31), 0);
        assert_eq!(lookup(&SUSPENSION[0], 2000.0), 6);
        assert_eq!(lookup(&ROAD_BUMP[0], 2000.0), -1);
        assert_eq!(lookup(&ROAD_BUMP[1], 2000.0), 6);
    }

    #[test]
    fn python_round_semantics() {
        assert_eq!(py_round(0.125, 2), 0.12);
        assert_eq!(py_round(0.375, 2), 0.38);
        assert_eq!(py_round(2.675, 2), 2.67);
        assert_eq!(py_round(0.305, 2), 0.3);
        assert_eq!(py_round(-1.236, 2), -1.24);
    }

    #[test]
    fn map_variants() {
        assert_eq!(map_clamped(55.0, 50.0, 60.0, 4.0, 6.0), 5.0);
        assert_eq!(map_clamped(70.0, 50.0, 60.0, 4.0, 6.0), 6.0);
        assert_eq!(map_clamped(40.0, 50.0, 60.0, 4.0, 6.0), 4.0);
        assert_eq!(map_clamped(f64::NAN, 50.0, 60.0, 4.0, 6.0), 6.0);
        assert_eq!(map_upper_clamped(-10.0, 0.0, 100.0, 0.0, 10000.0), Some(-1000.0));
        assert_eq!(map_upper_clamped(200.0, 0.0, 100.0, 0.0, 10000.0), Some(10000.0));
        assert_eq!(map_upper_clamped(1.0, 0.0, 0.0, 0.0, 10000.0), None);
    }
}
