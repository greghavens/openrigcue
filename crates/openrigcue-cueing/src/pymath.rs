//! Numeric helpers for the motion formulas: checked integer, float and complex arithmetic.

use crate::error::{CueError, DivisionKind};
use crate::pyvalue::float_to_int;

/// Linear rescale of `x` from `[in_min, in_max]` onto `[out_min, out_max]`, then clamped.
///
/// Plain double arithmetic with no zero check: the result is first limited to
/// `out_max` and then raised to `out_min`, so a NaN result lands on `out_max`.
pub fn rescale(x: f64, in_min: f64, in_max: f64, out_min: f64, out_max: f64) -> f64 {
    let r = (x - in_min) * (out_max - out_min) / (in_max - in_min) + out_min;
    let capped = if r <= out_max { r } else { out_max };
    if out_min <= capped {
        capped
    } else {
        out_min
    }
}

/// The integer of a float: truncation toward zero; NaN (a value error) and infinity (an overflow error) fail.
pub fn trunc(x: f64) -> Result<f64, CueError> {
    if x.is_nan() || x.is_infinite() {
        float_to_int(x)?;
    }
    let integer = x.trunc();
    Ok(if integer == 0.0 { 0.0 } else { integer })
}

/// A float rounded to the nearest integer, half to even; NaN and infinity fail as truncation does.
pub fn round_half_even(x: f64) -> Result<f64, CueError> {
    trunc(x.round_ties_even())
}

/// How a float power can fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerFailure {
    /// Zero raised to a negative power, or a complex power of zero (a division by zero).
    ZeroDivision,
    /// An infinite result from finite operands (an overflow error).
    Overflow,
}

/// A complex number of two doubles, with overflow-checked arithmetic.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Complex {
    pub re: f64,
    pub im: f64,
}

impl Complex {
    /// A real operand as a complex number.
    pub fn from_real(x: f64) -> Self {
        Complex { re: x, im: 0.0 }
    }

    /// The sum.
    pub fn add(self, b: Complex) -> Complex {
        Complex { re: self.re + b.re, im: self.im + b.im }
    }

    /// The product.
    pub fn mul(self, b: Complex) -> Complex {
        Complex { re: self.re * b.re - self.im * b.im, im: self.re * b.im + self.im * b.re }
    }

    /// The quotient; `None` for a zero divisor (a division by zero).
    pub fn div(self, b: Complex) -> Option<Complex> {
        let (abs_re, abs_im) = (b.re.abs(), b.im.abs());
        if abs_re >= abs_im {
            if abs_re == 0.0 {
                return None;
            }
            let ratio = b.im / b.re;
            let denom = b.re + b.im * ratio;
            Some(Complex { re: (self.re + self.im * ratio) / denom, im: (self.im - self.re * ratio) / denom })
        } else if abs_im >= abs_re {
            let ratio = b.re / b.im;
            let denom = b.re * ratio + b.im;
            Some(Complex { re: (self.re * ratio + self.im) / denom, im: (self.im * ratio - self.re) / denom })
        } else {
            Some(Complex { re: f64::NAN, im: f64::NAN })
        }
    }

    /// The magnitude; `None` where it overflows a double (an overflow error).
    pub fn abs(self) -> Option<f64> {
        if !self.re.is_finite() || !self.im.is_finite() {
            if self.re.is_infinite() {
                return Some(self.re.abs());
            }
            if self.im.is_infinite() {
                return Some(self.im.abs());
            }
            return Some(f64::NAN);
        }
        let r = self.re.hypot(self.im);
        r.is_finite().then_some(r)
    }

    /// The power for a real exponent that is not a small integer, computed in polar form.
    fn pow_real(self, exponent: f64) -> Result<Complex, PowerFailure> {
        if exponent == 0.0 {
            return Ok(Complex::from_real(1.0));
        }
        if self.re == 0.0 && self.im == 0.0 {
            if exponent < 0.0 {
                return Err(PowerFailure::ZeroDivision);
            }
            return Ok(Complex::from_real(0.0));
        }
        let vabs = self.re.hypot(self.im);
        let len = vabs.powf(exponent);
        let phase = self.im.atan2(self.re) * exponent;
        let r = Complex { re: len * phase.cos(), im: len * phase.sin() };
        if r.re.is_infinite() || r.im.is_infinite() {
            return Err(PowerFailure::Overflow);
        }
        Ok(r)
    }
}

/// A number as the motion formulas carry it: an int (the `0` of a failed percentage, of an
/// inactive traction-loss axis or of an unused direction cell), a float, or the complex number a
/// negative base raised to a fractional power produces. A complex value never reaches the rig: it fails the first
/// comparison or the output map it meets (a type error).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Num {
    Int(i128),
    Real(f64),
    Complex(Complex),
}

impl Num {
    fn real(self) -> Option<f64> {
        match self {
            Num::Int(i) => Some(i as f64),
            Num::Real(x) => Some(x),
            Num::Complex(_) => None,
        }
    }

    fn complex(self) -> Complex {
        match self {
            Num::Int(i) => Complex::from_real(i as f64),
            Num::Real(x) => Complex::from_real(x),
            Num::Complex(c) => c,
        }
    }

    /// The type name used in error texts (`int`, `float`, `complex`).
    pub fn type_name(self) -> &'static str {
        match self {
            Num::Int(_) => "int",
            Num::Real(_) => "float",
            Num::Complex(_) => "complex",
        }
    }

    /// `a + b`.
    pub fn add(self, b: Num) -> Num {
        match (self, b) {
            (Num::Int(x), Num::Int(y)) => x.checked_add(y).map_or(Num::Real(x as f64 + y as f64), Num::Int),
            (Num::Complex(_), _) | (_, Num::Complex(_)) => Num::Complex(self.complex().add(b.complex())),
            _ => Num::Real(self.real().unwrap_or(0.0) + b.real().unwrap_or(0.0)),
        }
    }

    /// `a * b`.
    pub fn mul(self, b: Num) -> Num {
        match (self, b) {
            (Num::Int(x), Num::Int(y)) => x.checked_mul(y).map_or(Num::Real(x as f64 * y as f64), Num::Int),
            (Num::Complex(_), _) | (_, Num::Complex(_)) => Num::Complex(self.complex().mul(b.complex())),
            _ => Num::Real(self.real().unwrap_or(0.0) * b.real().unwrap_or(0.0)),
        }
    }

    /// `a / b`; a division by zero fails with the text of the operand types.
    pub fn div(self, b: Num) -> Result<Num, CueError> {
        match (self, b) {
            (Num::Complex(_), _) | (_, Num::Complex(_)) => self
                .complex()
                .div(b.complex())
                .map(Num::Complex)
                .ok_or_else(|| CueError::zero_division(DivisionKind::Complex)),
            (Num::Int(_), Num::Int(0)) => Err(CueError::zero_division(DivisionKind::Int)),
            (Num::Int(x), Num::Int(y)) => Ok(Num::Real(
                openrigcue_model::python_numeric::int_true_divide(x, y).expect("zero denominator checked above"),
            )),
            _ => {
                let (x, y) = (self.real().unwrap_or(0.0), b.real().unwrap_or(0.0));
                if y == 0.0 {
                    Err(CueError::zero_division(DivisionKind::Float))
                } else {
                    Ok(Num::Real(x / y))
                }
            }
        }
    }

    /// The magnitude; `None` where it overflows (an overflow error).
    pub fn magnitude(self) -> Option<f64> {
        match self {
            Num::Int(i) => Some((i as f64).abs()),
            Num::Real(x) => Some(x.abs()),
            Num::Complex(c) => c.abs(),
        }
    }
}

/// Whether `x` is an odd integer: `fmod(fabs(x), 2.0) == 1.0`.
fn is_odd_integer(x: f64) -> bool {
    x.abs() % 2.0 == 1.0
}

/// `base` raised to `exponent` for float operands: a negative base with a fractional exponent gives a
/// complex number.
pub fn power(base: f64, exponent: f64) -> Result<Num, PowerFailure> {
    let (iv, iw) = (base, exponent);
    if iw == 0.0 {
        return Ok(Num::Real(1.0));
    }
    if iv.is_nan() {
        return Ok(Num::Real(iv));
    }
    if iw.is_nan() {
        return Ok(Num::Real(if iv == 1.0 { 1.0 } else { iw }));
    }
    if iw.is_infinite() {
        let v = iv.abs();
        return Ok(Num::Real(if v == 1.0 {
            1.0
        } else if (iw > 0.0) == (v > 1.0) {
            iw.abs()
        } else {
            0.0
        }));
    }
    if iv.is_infinite() {
        let odd = is_odd_integer(iw);
        return Ok(Num::Real(if iw > 0.0 {
            if odd {
                iv
            } else {
                iv.abs()
            }
        } else if odd {
            0.0f64.copysign(iv)
        } else {
            0.0
        }));
    }
    if iv == 0.0 {
        if iw < 0.0 {
            return Err(PowerFailure::ZeroDivision);
        }
        return Ok(Num::Real(if is_odd_integer(iw) { iv } else { 0.0 }));
    }
    let mut negate = false;
    let mut v = iv;
    if iv < 0.0 {
        if iw != iw.floor() {
            return Complex::from_real(iv).pow_real(iw).map(Num::Complex);
        }
        v = -iv;
        negate = is_odd_integer(iw);
    }
    if v == 1.0 {
        return Ok(Num::Real(if negate { -1.0 } else { 1.0 }));
    }
    let ix = v.powf(iw);
    if ix.is_infinite() {
        return Err(PowerFailure::Overflow);
    }
    Ok(Num::Real(if negate { -ix } else { ix }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rescale_is_linear_then_clamped_like_functions_map() {
        // The stream output map: power from [-100, 100] onto [204, 3891] for a 4095 rig.
        assert_eq!(rescale(0.0, -100.0, 100.0, 204.0, 3891.0), 2047.5);
        assert_eq!(rescale(150.0, -100.0, 100.0, 204.0, 3891.0), 3891.0);
        assert_eq!(rescale(-150.0, -100.0, 100.0, 204.0, 3891.0), 204.0);
        // NaN fails `r <= out_max`, takes out_max, which passes `out_min <= d`.
        assert_eq!(rescale(f64::NAN, -100.0, 100.0, 204.0, 3891.0), 3891.0);
        // out_min > out_max: always out_min (second comparison wins).
        assert_eq!(rescale(50.0, 0.0, 100.0, 99.0, 90.0), 99.0);
    }

    #[test]
    fn trunc_and_round_follow_python() {
        assert_eq!(trunc(-2.7), Ok(-2.0));
        assert_eq!(trunc(204.75), Ok(204.0));
        assert_eq!(trunc(f64::NAN).unwrap_err().to_string(), "cannot convert float NaN to integer");
        assert_eq!(round_half_even(2.5), Ok(2.0));
        assert_eq!(round_half_even(3.5), Ok(4.0));
        assert_eq!(round_half_even(-0.5), Ok(-0.0));
        assert_eq!(round_half_even(f64::INFINITY).unwrap_err().to_string(), "cannot convert float infinity to integer");
    }

    #[test]
    fn integer_conversions_discard_negative_zero() {
        // Both conversions return an integer: converting that integer back to
        // the floating representation used by the formulas cannot retain -0.
        for value in [-0.0, -f64::from_bits(1), -0.25] {
            assert_eq!(trunc(value).unwrap().to_bits(), 0.0f64.to_bits());
            assert_eq!(round_half_even(value).unwrap().to_bits(), 0.0f64.to_bits());
        }
        assert_eq!(round_half_even(-0.5).unwrap().to_bits(), 0.0f64.to_bits());
    }

    #[test]
    fn round_resolves_halfway_neighbors_before_integer_conversion() {
        for (value, expected) in [
            (0.5_f64.next_down(), 0.0),
            (0.5, 0.0),
            (0.5_f64.next_up(), 1.0),
            (1.5_f64.next_down(), 1.0),
            (1.5, 2.0),
            (1.5_f64.next_up(), 2.0),
            (2.5_f64.next_down(), 2.0),
            (2.5, 2.0),
            (2.5_f64.next_up(), 3.0),
        ] {
            assert_eq!(round_half_even(value).unwrap(), expected, "{value:?}");
            let negative = round_half_even(-value).unwrap();
            assert_eq!(negative, -expected, "{:?}", -value);
            assert_eq!(negative.is_sign_negative(), expected != 0.0);
        }
    }

    #[test]
    fn power_preserves_cpython_zero_signs_and_binary64_boundaries() {
        // The IEEE 754 pow special cases, plus exact powers
        // of two on each side of binary64 underflow and overflow.
        for (base, exponent, expected) in [
            (-0.0, 1.0, -0.0),
            (-0.0, 0.5, 0.0),
            (-0.0, 2.0, 0.0),
            (f64::NEG_INFINITY, -1.0, -0.0),
            (f64::NEG_INFINITY, -0.5, 0.0),
            (f64::NEG_INFINITY, 3.0, f64::NEG_INFINITY),
            (-1.0, f64::INFINITY, 1.0),
            (f64::NAN, -0.0, 1.0),
            (2.0, -1074.0, f64::from_bits(1)),
            (2.0, -1075.0, 0.0),
            (-2.0, -1075.0, -0.0),
            (2.0, 1023.0, f64::from_bits(0x7fe0_0000_0000_0000)),
        ] {
            let Num::Real(actual) = power(base, exponent).unwrap() else { panic!("real expected") };
            assert_eq!(actual.to_bits(), expected.to_bits(), "{base:?} ** {exponent:?}");
        }
        assert_eq!(power(2.0, 1024.0), Err(PowerFailure::Overflow));
        assert_eq!(power(-0.0, -1.0), Err(PowerFailure::ZeroDivision));
    }

    #[test]
    fn power_follows_float_pow() {
        assert_eq!(power(0.0, -1.8), Err(PowerFailure::ZeroDivision));
        assert_eq!(power(1e200, 2.0), Err(PowerFailure::Overflow));
        assert_eq!(power(-5.0, 2.0), Ok(Num::Real(25.0)));
        assert_eq!(power(-5.0, 3.0), Ok(Num::Real(-125.0)));
        assert_eq!(power(0.0, 0.0), Ok(Num::Real(1.0)));
        assert_eq!(power(4.0, 0.5), Ok(Num::Real(2.0)));
        // Special cases are settled before the platform pow.
        assert_eq!(power(0.0, f64::NEG_INFINITY), Ok(Num::Real(f64::INFINITY)));
        assert_eq!(power(f64::NEG_INFINITY, 0.5), Ok(Num::Real(f64::INFINITY)));
        assert_eq!(power(1.0, f64::NAN), Ok(Num::Real(1.0)));
        assert_eq!(power(-1.0, 1e300), Ok(Num::Real(1.0)));
    }

    #[test]
    fn negative_base_fractional_exponent_is_complex() {
        // (-18)^1.5 = -1.4028488111014991e-14 - 76.36753236814714i
        let Ok(Num::Complex(c)) = power(-18.0, 1.5) else { panic!("complex expected") };
        let len = 18f64.powf(1.5);
        let phase = std::f64::consts::PI * 1.5;
        assert_eq!(c, Complex { re: len * phase.cos(), im: len * phase.sin() });
        assert!((c.im + 76.367532368147).abs() < 1e-9);
        // 2 * c, then 5 / (2 * c), then * 100 keeps the complex type.
        let q = Num::Real(5.0).div(Num::Real(2.0).mul(Num::Complex(c))).unwrap().mul(Num::Real(100.0));
        assert!(matches!(q, Num::Complex(_)));
        assert!((q.magnitude().unwrap() - 500.0 / (2.0 * len)).abs() < 1e-9);
    }

    #[test]
    fn complex_division_by_zero_and_abs_overflow() {
        assert_eq!(Complex::from_real(1.0).div(Complex::from_real(0.0)), None);
        assert_eq!(Num::Real(1.0).div(Num::Int(0)).unwrap_err().to_string(), "float division by zero");
        assert_eq!(Num::Int(0).div(Num::Int(0)).unwrap_err().to_string(), "division by zero");
        assert_eq!(
            Num::Complex(Complex::from_real(1.0)).div(Num::Int(0)).unwrap_err().to_string(),
            "complex division by zero"
        );
        assert_eq!(Num::Int(3).div(Num::Int(2)), Ok(Num::Real(1.5)));
        assert_eq!(Num::Int(0).mul(Num::Int(-1)), Num::Int(0));
        assert_eq!(Complex { re: 1.5e308, im: 1.5e308 }.abs(), None);
        assert_eq!(Complex { re: f64::INFINITY, im: f64::NAN }.abs(), Some(f64::INFINITY));
    }

    #[test]
    fn represented_integer_ratio_rounds_once_before_entering_float_formulas() {
        // Integer-division vectors with correctly rounded quotients.
        // Rounding each operand to f64 first loses the low bit in these ratios.
        for (numerator, denominator, bits) in [
            (9_007_199_254_740_993_i128, 3_i128, 0x4325_5555_5555_5556_u64),
            (9_007_199_254_740_993, 7, 0x4312_4924_9249_2493),
        ] {
            let Num::Real(value) = Num::Int(numerator).div(Num::Int(denominator)).unwrap() else {
                panic!("integer true division produces a float")
            };
            assert_eq!(value.to_bits(), bits);
        }
    }
}
