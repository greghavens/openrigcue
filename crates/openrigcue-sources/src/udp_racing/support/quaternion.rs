//! Quaternion to Euler helpers, used by Wreckfest 2 and Project
//! Motor Racing. They take `(x, y, z, w)` and return degrees. Some terms
//! differ from the textbook conversion (yaw ignores `z`).

use super::pymath::rad_to_deg;

/// Yaw:
/// `atan2(2(w·y + x·y), 1 − 2(x² + y²))` in degrees.
pub fn yaw(x: f64, y: f64, _z: f64, w: f64) -> f64 {
    rad_to_deg((2.0 * (w * y + x * y)).atan2(1.0 - 2.0 * (x * x + y * y)))
}

/// Roll:
/// `atan2(2(y·x + w·z), 1 − 2(x² + z²))` in degrees.
pub fn roll(x: f64, y: f64, z: f64, w: f64) -> f64 {
    rad_to_deg((2.0 * (y * x + w * z)).atan2(1.0 - 2.0 * (x * x + z * z)))
}

/// Pitch:
/// `atan2(2(w·x − z·y), sqrt((2(w·y + x·y))² + (1 − 2(x² + y²))²))` in degrees.
pub fn pitch(x: f64, y: f64, z: f64, w: f64) -> f64 {
    let a = 2.0 * (w * y + x * y);
    let b = 1.0 - 2.0 * (x * x + y * y);
    rad_to_deg((2.0 * (w * x - z * y)).atan2((a * a + b * b).sqrt()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_quaternion_is_level() {
        assert_eq!(yaw(0.0, 0.0, 0.0, 1.0), 0.0);
        assert_eq!(roll(0.0, 0.0, 0.0, 1.0), 0.0);
        assert_eq!(pitch(0.0, 0.0, 0.0, 1.0), 0.0);
    }

    #[test]
    fn formulas_match_the_compiled_terms() {
        // Yaw, roll and pitch with x=0.1, y=0.2, z=0.3, w=0.9.
        let (x, y, z, w): (f64, f64, f64, f64) = (0.1, 0.2, 0.3, 0.9);
        let expect_yaw = rad_to_deg((2.0 * (w * y + x * y)).atan2(1.0 - 2.0 * (x * x + y * y)));
        assert_eq!(yaw(x, y, z, w), expect_yaw);
        // z does not take part in the yaw term.
        assert_eq!(yaw(x, y, 5.0, w), expect_yaw);
        let expect_roll = rad_to_deg((2.0 * (y * x + w * z)).atan2(1.0 - 2.0 * (x * x + z * z)));
        assert_eq!(roll(x, y, z, w), expect_roll);
        let p = pitch(x, y, z, w);
        assert!((p - 3.486_194_442_627_714).abs() < 1e-12, "{p}");
    }
}
