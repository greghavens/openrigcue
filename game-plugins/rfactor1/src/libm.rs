//! The three math routines the plugin calls:
//!
//! * `cos`  — table-driven SSE2 cosine
//! * `sin`  — table-driven SSE2 sine
//! * `sqrt` — SSE2 square root
//!
//! Every function takes and returns the raw IEEE-754 bit pattern so that no
//! value crosses a non-inlined call boundary as a float (the i686 ABI returns
//! floats on the x87 stack, which would quiet signalling NaNs).
//!
//! Path selection: the SSE2 code is used only
//! when `(MXCSR & 0x7f80) == 0x1f80` and `(x87 CW & 0x7f) == 0x7f`
//! (all exceptions masked, round-to-nearest); otherwise, and for large,
//! infinite or NaN arguments, the x87 routine (x87.rs) is used.

use crate::sincos_table::SINCOS_TABLE;
use crate::sse::{addsd, cvtsd2si, mulsd, mxcsr, sqrtsd, subsd, x87_cw};

// Constants shared by the cos and sin routines.
const C2A0: [u64; 2] = [0xbfc5555555555555, 0xbfe0000000000000];
const C2B0: [u64; 2] = [0x3f81111111111111, 0x3fa5555555555555];
const C2C0: [u64; 2] = [0xbf2a01a01a01a01a, 0xbf56c16c16c16c17];
const C2D0: [u64; 2] = [0x3ec71de3a556c734, 0x3efa01a01a01a01a];
const C2E0: u64 = 0x40245f306dc9c883; // 32/pi
const C2E8: u64 = 0x4338000000000000; // 1.5 * 2^52 (round-to-int shifter)
const C2F0: [u64; 2] = [0x3d90b4611a600000, 0x3d90b4611a600000];
const C300: u64 = 0x3fb921fb54400000; // pi/32 high part
const C308: u64 = 0x3b63198a2e037073; // pi/32 low part
const ONE: u64 = 0x3ff0000000000000;
const SIN_TINY_DENORM: u64 = 0x3fefffffffffffff;

/// x87 "real indefinite" returned by the x87 routines for an infinite
/// argument, as a double.
const X87_INDEFINITE: u64 = 0xfff8000000000000;
/// 80-bit constant used by the x87 routines' `fprem1` reduction loop.
const X87_REDUCE: [u8; 10] = [0x35, 0xc2, 0x68, 0x21, 0xa2, 0xda, 0x0f, 0xc9, 0x3e, 0x40];
/// Control word loaded by the x87 routines.
const X87_CW_DOUBLE: u16 = 0x027f;

#[inline(always)]
fn d(b: u64) -> f64 {
    f64::from_bits(b)
}

#[inline(always)]
fn fast_env() -> bool {
    (mxcsr() & 0x7f80) == 0x1f80 && (x87_cw() & 0x7f) == 0x7f
}

/// The common table-driven core of cos (`bias` 0x1c7610) and sin (`bias`
/// 0x1c7600). Two-lane values are evaluated lane by lane (`l` / `h`).
#[inline(always)]
fn table_core(x: f64, bias: i32) -> f64 {
    let t = mulsd(d(C2E0), x);
    let edx = cvtsd2si(t);
    // n = t rounded to an integer
    let n = subsd(addsd(t, d(C2E8)), d(C2E8));
    let x3 = mulsd(d(C300), n);
    // table index: ((edx + bias) & 0x3f) rows of four
    let e = ((edx.wrapping_add(bias) & 0x3f) as usize) << 2;
    let t0 = d(SINCOS_TABLE[e]);
    let t1 = d(SINCOS_TABLE[e + 1]);
    let t2 = d(SINCOS_TABLE[e + 2]);
    let t3 = d(SINCOS_TABLE[e + 3]);
    let x2l = mulsd(d(C2F0[0]), n);
    let x2h = mulsd(d(C2F0[1]), n);
    let r = subsd(x, x3);
    let x1 = mulsd(n, d(C308));
    let x4 = subsd(x, x3);
    let x7 = t1;
    let x0l = r;
    let x0h = r;
    let x3 = x4;
    let u = subsd(x4, x2l);
    let x5l = mulsd(d(C2D0[0]), x0l);
    let x5h = mulsd(d(C2D0[1]), x0h);
    let x0l = subsd(x0l, x2l);
    let x0h = subsd(x0h, x2h);
    let x7 = mulsd(x7, u);
    let x3 = subsd(x3, u);
    let x5l = mulsd(x5l, x0l);
    let x5h = mulsd(x5h, x0h);
    let x0l2 = mulsd(x0l, x0l);
    let x0h2 = mulsd(x0h, x0h);
    let x3 = subsd(x3, x2l);
    let x1 = subsd(x1, x3);
    let x2l = addsd(t0, t3);
    let x7 = subsd(x7, x2l);
    let x2l = mulsd(x2l, u);
    let x2h = t1;
    let x6l = mulsd(d(C2B0[0]), x0l2);
    let x6h = mulsd(d(C2B0[1]), x0h2);
    let x3 = mulsd(t3, u);
    let x2l = mulsd(x2l, x0l2);
    let x2h = mulsd(x2h, x0h2);
    let x0l4 = mulsd(x0l2, x0l2);
    let x0h4 = mulsd(x0h2, x0h2);
    let x5l = addsd(x5l, d(C2C0[0]));
    let x5h = addsd(x5h, d(C2C0[1]));
    let x4 = mulsd(u, t0);
    let x6l = addsd(x6l, d(C2A0[0]));
    let x6h = addsd(x6h, d(C2A0[1]));
    let x5l = mulsd(x5l, x0l4);
    let x5h = mulsd(x5h, x0h4);
    let x0 = x3;
    let x3 = addsd(x3, t1);
    let x1 = mulsd(x1, x7);
    let x7 = x4;
    let x4 = addsd(x4, x3);
    let x6l = addsd(x6l, x5l);
    let x6h = addsd(x6h, x5h);
    let x5 = subsd(t1, x3);
    let x3 = subsd(x3, x4);
    let x1 = addsd(x1, t2);
    let x6l = mulsd(x6l, x2l);
    let x6h = mulsd(x6h, x2h);
    let x5 = addsd(x5, x0);
    let x3 = addsd(x3, x7);
    let x1 = addsd(x1, x5);
    let x1 = addsd(x1, x3);
    let x1 = addsd(x1, x6l);
    let x1 = addsd(x1, x6h);
    addsd(x4, x1)
}

/// The biased exponent word less 0x3030, and its two
/// compares against 0x10c5 (unsigned / signed, 16 bit).
#[inline(always)]
fn classify(x: u64) -> (bool, bool, u16) {
    let ax = (((x >> 48) as u16) & 0x7fff).wrapping_sub(0x3030);
    (ax > 0x10c5, (ax as i16) > 0x10c5, ax)
}

/// Cosine of a double given and returned as bits.
#[inline(never)]
pub fn cos(x: u64) -> u64 {
    if !fast_env() {
        return crate::x87::cos(x);
    }
    let (above, greater, _) = classify(x);
    if !above {
        return table_core(d(x), 0x1c7610).to_bits();
    }
    if greater {
        return crate::x87::cos(x);
    }
    // tiny argument: 1.0 - |x|
    subsd(d(ONE), d(x & 0x7fff_ffff_ffff_ffff)).to_bits()
}

/// Sine of a double given and returned as bits.
#[inline(never)]
pub fn sin(x: u64) -> u64 {
    if !fast_env() {
        return crate::x87::sin(x);
    }
    let (above, greater, ax) = classify(x);
    if !above {
        return table_core(d(x), 0x1c7600).to_bits();
    }
    if greater {
        return crate::x87::sin(x);
    }
    if (ax >> 4) == 0xcfd {
        // zero / denormal: x * 0x3fefffffffffffff
        return mulsd(d(x), d(SIN_TINY_DENORM)).to_bits();
    }
    // Tiny argument: x is returned unchanged; the three operations
    // below only set the MXCSR flags.
    let t = mulsd(d(SIN_SCALE_UP), d(x));
    let t = subsd(t, d(x));
    let _ = core::hint::black_box(mulsd(t, d(SIN_SCALE_DOWN)));
    x
}

/// 2^55 and 2^-55.
const SIN_SCALE_UP: u64 = 0x4360000000000000;
const SIN_SCALE_DOWN: u64 = 0x3c80000000000000;

/// Square root of a double given and returned as bits.
#[inline(never)]
pub fn sqrt(x: u64) -> u64 {
    if !fast_env() {
        return crate::x87::sqrt(x);
    }
    let hi16 = (x >> 48) as u32;
    if hi16 & 0x7ff0 == 0x7ff0 {
        // Infinity (mantissa 0) continues at the sign test;
        // a NaN is returned with its quiet bit set.
        if x & 0x000f_ffff_ffff_ffff != 0 {
            return x | 0x0008_0000_0000_0000;
        }
    }
    if hi16 & 0x8000 == 0 {
        return sqrtsd(d(x)).to_bits();
    }
    // Negative: only -0.0 (low dword 0, top word 0x8000, word 2
    // 0) reaches sqrtsd; everything else returns the default NaN.
    if (x as u32) == 0 && hi16 == 0x8000 && ((x >> 32) as u16) == 0 {
        return sqrtsd(d(x)).to_bits();
    }
    X87_INDEFINITE
}

#[allow(dead_code)]
pub(crate) const REDUCE: [u8; 10] = X87_REDUCE;
#[allow(dead_code)]
pub(crate) const CW_DOUBLE: u16 = X87_CW_DOUBLE;
