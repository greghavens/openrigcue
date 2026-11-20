//! The ESP32 FPU conversions the OBD encoders use (`utrunc.s`, `trunc.s`,
//! `ufloat.s`, all with scale 0). A float is converted with
//! `utrunc.s` and its low byte stored, so out-of-range values follow the FPU's
//! saturation rules, not those of Rust casts.
//!
//! On the target the real instructions are executed (inline asm), so the firmware
//! saturates the way the chip does. On the host the functions run the
//! `model` below, which follows the Xtensa ISA semantics of
//! `utrunc.s` / `trunc.s` / `ufloat.s`, with the documented
//! 2^32 / 2^31 limits.

/// Host model of the three instructions.
pub mod model {
    /// `utrunc.s ar, fs, 0`: NaN -> 0xffffffff; truncated value < 0 -> 0x80000000;
    /// >= 2^32 -> 0xffffffff; otherwise the truncated value.
    pub fn utrunc(x: f32) -> u32 {
        if x.is_nan() {
            return 0xffff_ffff;
        }
        let t = trunc_f64(x as f64);
        if t < 0.0 {
            0x8000_0000
        } else if t >= 4_294_967_296.0 {
            0xffff_ffff
        } else {
            t as u32
        }
    }

    /// `trunc.s ar, fs, 0`: NaN -> 0x7fffffff (the `fs < 0` test is false for NaN);
    /// >= 2^31 -> 0x7fffffff; < -2^31 -> 0x80000000; otherwise the truncated value.
    pub fn trunc(x: f32) -> i32 {
        if x.is_nan() {
            return 0x7fff_ffff;
        }
        let t = trunc_f64(x as f64);
        if t >= 2_147_483_648.0 {
            0x7fff_ffff
        } else if t < -2_147_483_648.0 {
            i32::MIN
        } else {
            t as i32
        }
    }

    /// `ufloat.s fr, as, 0`: unsigned 32-bit integer to single, round to nearest even.
    pub fn ufloat(v: u32) -> f32 {
        v as f32
    }

    /// Truncation toward zero without libm (exact for every f32 value widened to f64).
    fn trunc_f64(x: f64) -> f64 {
        if !(x.abs() < 4_503_599_627_370_496.0) {
            return x; // |x| >= 2^52: already integral (or infinite)
        }
        let i = x as i64; // exact for |x| < 2^52, rounds toward zero
        let t = i as f64;
        if t == 0.0 && x.is_sign_negative() {
            -0.0
        } else {
            t
        }
    }
}

#[cfg(target_arch = "xtensa")]
mod hw {
    use core::arch::asm;

    #[inline(always)]
    pub fn utrunc(x: f32) -> u32 {
        let r: u32;
        unsafe { asm!("utrunc.s {r}, {f}, 0", r = out(reg) r, f = in(freg) x, options(pure, nomem, nostack)) };
        r
    }

    #[inline(always)]
    pub fn trunc(x: f32) -> i32 {
        let r: i32;
        unsafe { asm!("trunc.s {r}, {f}, 0", r = out(reg) r, f = in(freg) x, options(pure, nomem, nostack)) };
        r
    }

    #[inline(always)]
    pub fn ufloat(v: u32) -> f32 {
        let r: f32;
        unsafe { asm!("ufloat.s {r}, {a}, 0", r = out(freg) r, a = in(reg) v, options(pure, nomem, nostack)) };
        r
    }
}

#[cfg(target_arch = "xtensa")]
pub use hw::{trunc, ufloat, utrunc};
#[cfg(not(target_arch = "xtensa"))]
pub use model::{trunc, ufloat, utrunc};
