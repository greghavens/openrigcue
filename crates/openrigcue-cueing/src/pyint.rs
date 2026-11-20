//! Unbounded integers of the mixing tables, and the float arithmetic the mixing stage does with them.
//!
//! Table cells are integers of any size. The mixing stage uses each one in three
//! ways: `percentages / 100` (integer division to a float, correctly rounded, an overflow error when the
//! quotient does not fit a float), `100 - deadzone` (exact), and that difference compared with a float
//! percentage (exact comparison) or multiplied by a float weight (converted to the nearest float,
//! an overflow error when it does not fit).
//! [`TableInt`] holds what those uses need, derived once from the exact value when the table is built.

use num_bigint::{BigInt, BigUint, Sign};
use std::cmp::Ordering;
use std::sync::Arc;

/// An arbitrary-precision integer: the value of a table cell.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PyInt {
    negative: bool,
    /// Magnitude in little-endian 32-bit limbs without high zero limbs (zero is empty).
    magnitude: Vec<u32>,
}

impl From<i64> for PyInt {
    fn from(value: i64) -> Self {
        let m = value.unsigned_abs();
        let mut magnitude = vec![m as u32, (m >> 32) as u32];
        trim(&mut magnitude);
        PyInt { negative: value < 0, magnitude }
    }
}

impl PyInt {
    /// The integer of an ASCII decimal digit string (`b'0'..=b'9'` only).
    pub(crate) fn from_decimal(negative: bool, digits: impl IntoIterator<Item = u8>) -> PyInt {
        let mut magnitude = Vec::new();
        for d in digits {
            mul_add(&mut magnitude, 10, u32::from(d - b'0'));
        }
        trim(&mut magnitude);
        PyInt { negative: negative && !magnitude.is_empty(), magnitude }
    }

    fn neg(&self) -> PyInt {
        PyInt { negative: !self.negative && !self.magnitude.is_empty(), magnitude: self.magnitude.clone() }
    }

    /// `self + other`.
    fn add(&self, other: &PyInt) -> PyInt {
        if self.negative == other.negative {
            let magnitude = add_mag(&self.magnitude, &other.magnitude);
            return PyInt { negative: self.negative && !magnitude.is_empty(), magnitude };
        }
        let (larger, smaller) = match cmp_mag(&self.magnitude, &other.magnitude) {
            Ordering::Less => (other, self),
            _ => (self, other),
        };
        let magnitude = sub_mag(&larger.magnitude, &smaller.magnitude);
        PyInt { negative: larger.negative && !magnitude.is_empty(), magnitude }
    }

    /// The value as a float, kept exact enough to compare: see [`Exact`].
    fn exact(&self) -> Exact {
        let (nearest, residue) = round_scaled(&self.magnitude, false, 0);
        let positive = Exact {
            nearest,
            residue,
            integer: Arc::new(BigInt::from_biguint(Sign::Plus, BigUint::new(self.magnitude.clone()))),
        };
        if self.negative {
            positive.neg()
        } else {
            positive
        }
    }

    /// `self / 100` as a float: the correctly rounded quotient, `None` where the quotient is too
    /// large for a float (an overflow error for the caller).
    fn hundredth(&self) -> Option<f64> {
        const EXACT_BITS: usize = 53;
        let bits = bit_length(&self.magnitude);
        let quotient = if bits <= EXACT_BITS {
            // Both operands fit a double exactly: dividing the doubles is correctly rounded.
            low_u64(&self.magnitude) as f64 / 100.0
        } else {
            // Scale so the integer quotient has at least 55 bits, then round it with the remainder as
            // sticky bit.
            let scale = 62usize.saturating_sub(bits);
            let (q, remainder) = divmod_small(&shl(&self.magnitude, scale), 100);
            let (value, _) = round_scaled(&q, remainder != 0, -(scale as i64));
            if value.is_infinite() {
                return None;
            }
            value
        };
        Some(if self.negative { -quotient } else { quotient })
    }
}

/// An integer as compared with and converted to floats: the nearest float (half to even; infinite where
/// the conversion overflows) and the side of it the exact value lies on.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Exact {
    nearest: f64,
    integer: Arc<BigInt>,
    /// `exact.cmp(nearest)`.
    residue: Ordering,
}

impl Exact {
    pub fn native(&self) -> openrigcue_model::native_value::LegacyValue {
        openrigcue_model::native_value::LegacyValue::from_integer(self.integer.as_ref().clone())
    }

    pub fn neg(self) -> Exact {
        Exact { nearest: -self.nearest, residue: self.residue.reverse(), integer: Arc::new(-self.integer.as_ref()) }
    }

    /// The absolute value.
    #[cfg(test)]
    pub fn abs(&self) -> Exact {
        if self.nearest < 0.0 {
            self.clone().neg()
        } else {
            self.clone()
        }
    }

    /// Whether the value is exactly `n`.
    pub fn is(&self, n: f64) -> bool {
        self.nearest == n && self.residue == Ordering::Equal
    }

    /// The nearest float; `None` where the value is too large for a float (an overflow error for
    /// the caller).
    pub fn to_float(&self) -> Option<f64> {
        self.nearest.is_finite().then_some(self.nearest)
    }

    /// How the float `x` compares with the exact value (an exact float/integer comparison); `None`
    /// for NaN, which compares false with everything.
    pub fn compare_float(&self, x: f64) -> Option<Ordering> {
        match x.partial_cmp(&self.nearest)? {
            // Rounding is monotonic: a float other than the nearest one lies on the same side of the
            // exact value as of its rounding.
            Ordering::Equal => Some(self.residue.reverse()),
            other => Some(other),
        }
    }
}

/// One cell of a mixing table, with what the mixing stage derives from it.
#[derive(Debug, Clone, PartialEq)]
pub struct TableInt {
    /// `cell / 100`, used where the cell is an axis share; `None` is an overflow error when used.
    hundredth: Option<f64>,
    /// `100 - cell`, used where the cell is a deadzone.
    complement: Exact,
}

impl TableInt {
    pub fn new(value: &PyInt) -> TableInt {
        TableInt { hundredth: value.hundredth(), complement: PyInt::from(100).add(&value.neg()).exact() }
    }

    pub(crate) fn hundredth(&self) -> Option<f64> {
        self.hundredth
    }

    pub(crate) fn complement(&self) -> Exact {
        self.complement.clone()
    }
}

impl From<i64> for TableInt {
    fn from(value: i64) -> Self {
        TableInt::new(&PyInt::from(value))
    }
}

fn trim(m: &mut Vec<u32>) {
    while m.last() == Some(&0) {
        m.pop();
    }
}

fn mul_add(m: &mut Vec<u32>, mul: u32, add: u32) {
    let mut carry = u64::from(add);
    for limb in m.iter_mut() {
        let v = u64::from(*limb) * u64::from(mul) + carry;
        *limb = v as u32;
        carry = v >> 32;
    }
    if carry != 0 {
        m.push(carry as u32);
    }
}

fn cmp_mag(a: &[u32], b: &[u32]) -> Ordering {
    a.len().cmp(&b.len()).then_with(|| a.iter().rev().cmp(b.iter().rev()))
}

fn add_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len().max(b.len()) + 1);
    let mut carry = 0u64;
    for i in 0..a.len().max(b.len()) {
        let v = u64::from(*a.get(i).unwrap_or(&0)) + u64::from(*b.get(i).unwrap_or(&0)) + carry;
        out.push(v as u32);
        carry = v >> 32;
    }
    if carry != 0 {
        out.push(carry as u32);
    }
    trim(&mut out);
    out
}

/// `a - b` for `a >= b`.
fn sub_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len());
    let mut borrow = 0i64;
    for (i, &limb) in a.iter().enumerate() {
        let mut v = i64::from(limb) - i64::from(*b.get(i).unwrap_or(&0)) - borrow;
        borrow = 0;
        if v < 0 {
            v += 1 << 32;
            borrow = 1;
        }
        out.push(v as u32);
    }
    trim(&mut out);
    out
}

fn bit_length(m: &[u32]) -> usize {
    m.last().map_or(0, |top| (m.len() - 1) * 32 + (32 - top.leading_zeros() as usize))
}

fn bit(m: &[u32], i: usize) -> bool {
    m.get(i / 32).is_some_and(|limb| limb >> (i % 32) & 1 == 1)
}

/// Whether any bit below position `i` is set.
fn any_below(m: &[u32], i: usize) -> bool {
    let (whole, part) = (i / 32, i % 32);
    m[..whole.min(m.len())].iter().any(|&limb| limb != 0)
        || (part != 0 && m.get(whole).is_some_and(|limb| limb & ((1u32 << part) - 1) != 0))
}

/// Bits `[from, from + 64)` as an integer.
fn bits_from(m: &[u32], from: usize) -> u64 {
    (0..64).filter(|&j| bit(m, from + j)).fold(0u64, |v, j| v | 1 << j)
}

fn low_u64(m: &[u32]) -> u64 {
    bits_from(m, 0)
}

fn shl(m: &[u32], shift: usize) -> Vec<u32> {
    let mut out = vec![0u32; shift / 32];
    let part = shift % 32;
    let mut carry = 0u32;
    for &limb in m {
        if part == 0 {
            out.push(limb);
        } else {
            out.push(limb << part | carry);
            carry = limb >> (32 - part);
        }
    }
    if carry != 0 {
        out.push(carry);
    }
    trim(&mut out);
    out
}

fn divmod_small(m: &[u32], d: u32) -> (Vec<u32>, u32) {
    let mut out = vec![0u32; m.len()];
    let mut remainder = 0u64;
    for i in (0..m.len()).rev() {
        let v = remainder << 32 | u64::from(m[i]);
        out[i] = (v / u64::from(d)) as u32;
        remainder = v % u64::from(d);
    }
    trim(&mut out);
    (out, remainder as u32)
}

/// `2^k` for `-1022 <= k <= 1023`.
fn pow2(k: i64) -> f64 {
    f64::from_bits(((k + 1023) as u64) << 52)
}

/// `q · 2^exp2` (plus a non-zero fraction below `q`'s last bit when `sticky`) rounded to the nearest
/// double, ties to even; infinity where the rounded value reaches 2^1024. Also returns
/// how the exact value compares with the result. With `sticky`, `q` must have more than 54 bits.
fn round_scaled(q: &[u32], sticky: bool, exp2: i64) -> (f64, Ordering) {
    const MANTISSA_BITS: usize = 53;
    let n = bit_length(q);
    if n == 0 {
        return (0.0, if sticky { Ordering::Greater } else { Ordering::Equal });
    }
    let (mut mantissa, mut shift, residue);
    if n <= MANTISSA_BITS {
        (mantissa, shift) = (low_u64(q), 0usize);
        residue = if sticky { Ordering::Greater } else { Ordering::Equal };
    } else {
        shift = n - MANTISSA_BITS;
        mantissa = bits_from(q, shift) & ((1u64 << MANTISSA_BITS) - 1);
        let half = bit(q, shift - 1);
        let rest = sticky || any_below(q, shift - 1);
        if half && (rest || mantissa & 1 == 1) {
            mantissa += 1;
            residue = Ordering::Less;
            if mantissa == 1 << MANTISSA_BITS {
                mantissa >>= 1;
                shift += 1;
            }
        } else {
            residue = if half || rest { Ordering::Greater } else { Ordering::Equal };
        }
    }
    let k = shift as i64 + exp2;
    let top = 63 - i64::from(mantissa.leading_zeros()) + k;
    if top >= 1024 {
        return (f64::INFINITY, Ordering::Less);
    }
    (mantissa as f64 * pow2(k), residue)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(2^53 - 1) · 2^971 + 2^970`: the smallest integer whose float conversion overflows.
    const TIE: &str = "179769313486231580793728971405303415079934132710037826936173778980444968292764750946649017977587207096330286416692887910946555547851940402630657488671505820681908902000708383676273854845817711531764475730270069855571366959622842914819860834936475292719074168444365510704342711559699508093042880177904174497792";
    /// `100 * TIE`.
    const TIE_TIMES_100: &str = "17976931348623158079372897140530341507993413271003782693617377898044496829276475094664901797758720709633028641669288791094655554785194040263065748867150582068190890200070838367627385484581771153176447573027006985557136695962284291481986083493647529271907416844436551070434271155969950809304288017790417449779200";

    fn int(s: &str) -> PyInt {
        let (negative, digits) = match s.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, s),
        };
        PyInt::from_decimal(negative, digits.bytes())
    }

    #[test]
    fn decimal_and_i64_agree() {
        assert_eq!(int("0"), PyInt::from(0));
        assert_eq!(int("-0"), PyInt::from(0));
        assert_eq!(int("-9223372036854775808"), PyInt::from(i64::MIN));
        assert_eq!(int("4294967296"), PyInt::from(1 << 32));
    }

    #[test]
    fn true_division_by_100_like_cpython() {
        // Correctly rounded quotients of the decimal integer `s` by 100.
        let cases: [(&str, f64); 7] = [
            ("100", 1.0),
            ("-30", -0.3),
            ("9007199254740993", 90071992547409.94),
            ("9007199254740992", 90071992547409.92),
            ("123456789012345678901234567890", 1.2345678901234569e27),
            ("18446744073709551617", 1.8446744073709552e17),
            ("-99999999999999999999999", -1e21),
        ];
        for (s, expected) in cases {
            assert_eq!(int(s).hundredth(), Some(expected), "{s}");
        }
        // 10^310 / 100 = 1e308; 10^311 / 100 overflows.
        assert_eq!(PyInt::from_decimal(false, std::iter::once(b'1').chain([b'0'; 310])).hundredth(), Some(1e308));
        assert_eq!(PyInt::from_decimal(false, std::iter::once(b'1').chain([b'0'; 311])).hundredth(), None);
        // 100 * (DBL_MAX + half an ulp): the tie rounds to even (2^1024) and overflows; one less fits.
        let tie = int(TIE_TIMES_100);
        assert_eq!(tie.hundredth(), None);
        assert_eq!(tie.add(&PyInt::from(-1)).hundredth(), Some(f64::MAX));
        assert_eq!(tie.add(&PyInt::from(1)).hundredth(), None);
    }

    #[test]
    fn float_conversion_and_comparison_like_cpython() {
        // 2^53 + 1 converts to 2^53 (tie to even), the exact value lies above.
        let e = int("9007199254740993").exact();
        assert_eq!(e.to_float(), Some(9007199254740992.0));
        assert_eq!(e.compare_float(9007199254740992.0), Some(Ordering::Less));
        assert_eq!(e.compare_float(9007199254740994.0), Some(Ordering::Greater));
        assert_eq!(e.compare_float(f64::NAN), None);
        // 2^53 + 3 rounds up to 2^53 + 4.
        let e = int("9007199254740995").exact();
        assert_eq!(e.to_float(), Some(9007199254740996.0));
        assert_eq!(e.compare_float(9007199254740996.0), Some(Ordering::Greater));
        // Beyond the double range: an overflow error on conversion, every finite float is smaller.
        let huge = PyInt::from_decimal(true, std::iter::once(b'1').chain([b'0'; 400])).exact();
        assert_eq!(huge.to_float(), None);
        assert_eq!(huge.compare_float(f64::MIN), Some(Ordering::Greater));
        assert_eq!(huge.compare_float(f64::NEG_INFINITY), Some(Ordering::Less));
        assert_eq!(huge.abs().compare_float(f64::INFINITY), Some(Ordering::Greater));
        assert_eq!(huge.abs().compare_float(f64::MAX), Some(Ordering::Less));
        // DBL_MAX + half an ulp ties to 2^1024 (an overflow error); one less rounds to DBL_MAX.
        assert_eq!(int(TIE).exact().to_float(), None);
        let below = int(TIE).add(&PyInt::from(-1)).exact();
        assert_eq!(below.to_float(), Some(f64::MAX));
        assert_eq!(below.compare_float(f64::MAX), Some(Ordering::Less));
    }

    #[test]
    fn complement_is_exact() {
        let t = TableInt::new(&int("9007199254741093"));
        // 100 - (2^53 + 101) = -(2^53 + 1).
        assert_eq!(t.complement().to_float(), Some(-9007199254740992.0));
        assert_eq!(t.complement().compare_float(-9007199254740992.0), Some(Ordering::Greater));
        assert!(TableInt::from(0).complement().is(100.0));
        assert!(!TableInt::from(30).complement().is(100.0));
        assert!(TableInt::from(-50).complement().is(150.0));
    }
}
