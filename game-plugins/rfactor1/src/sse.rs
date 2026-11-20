//! Scalar SSE/SSE2 primitives with a fixed operand order.
//!
//! The plugin does all of its float arithmetic with
//! `addss/subss/mulss/divss`, `addsd/subsd/mulsd`, `cvtss2sd/cvtps2pd`,
//! `cvtsd2ss/cvtpd2ps`, `cvtdq2ps` and `sqrtsd`. IEEE results are the same for
//! any compiler, but which NaN comes out of an operation whose two inputs are
//! both NaN depends on the operand order (`dst op src` returns `dst`'s NaN),
//! and LLVM is free to commute `a + b`. Every operation here is therefore one
//! inline-asm instruction with `dst` = first argument,
//! and all of them use the caller's MXCSR
//! (rounding mode, DAZ/FTZ).

use core::arch::asm;

macro_rules! ss_op {
    ($name:ident, $ins:literal) => {
        /// `dst = dst <op> src` on f32 (one SSE instruction).
        #[inline(always)]
        pub fn $name(dst: f32, src: f32) -> f32 {
            let mut d = dst;
            unsafe {
                asm!(concat!($ins, " {d}, {s}"), d = inout(xmm_reg) d, s = in(xmm_reg) src,
                     options(pure, nomem, nostack, preserves_flags));
            }
            d
        }
    };
}

macro_rules! sd_op {
    ($name:ident, $ins:literal) => {
        /// `dst = dst <op> src` on f64 (one SSE2 instruction).
        #[inline(always)]
        pub fn $name(dst: f64, src: f64) -> f64 {
            let mut d = dst;
            unsafe {
                asm!(concat!($ins, " {d}, {s}"), d = inout(xmm_reg) d, s = in(xmm_reg) src,
                     options(pure, nomem, nostack, preserves_flags));
            }
            d
        }
    };
}

ss_op!(addss, "addss");
ss_op!(subss, "subss");
ss_op!(mulss, "mulss");
ss_op!(divss, "divss");
sd_op!(addsd, "addsd");
sd_op!(subsd, "subsd");
sd_op!(mulsd, "mulsd");

/// `cvtss2sd` (same lane-0 result as `cvtps2pd`).
#[inline(always)]
pub fn cvtss2sd(x: f32) -> f64 {
    let r: f64;
    unsafe {
        asm!("cvtss2sd {r}, {x}", r = out(xmm_reg) r, x = in(xmm_reg) x,
             options(pure, nomem, nostack, preserves_flags));
    }
    r
}

/// `cvtsd2ss` (same lane-0 result as `cvtpd2ps`).
#[inline(always)]
pub fn cvtsd2ss(x: f64) -> f32 {
    let r: f32;
    unsafe {
        asm!("cvtsd2ss {r}, {x}", r = out(xmm_reg) r, x = in(xmm_reg) x,
             options(pure, nomem, nostack, preserves_flags));
    }
    r
}

/// `movd xmm, r32; cvtdq2ps xmm, xmm` (int -> float, MXCSR rounding).
#[inline(always)]
pub fn cvtdq2ps(x: i32) -> f32 {
    let r: f32;
    unsafe {
        asm!("movd {r}, {x:e}", "cvtdq2ps {r}, {r}", r = out(xmm_reg) r, x = in(reg) x,
             options(pure, nomem, nostack, preserves_flags));
    }
    r
}

/// `cvtsd2si r32, xmm` (MXCSR rounding; 0x80000000 on overflow/NaN).
#[inline(always)]
pub fn cvtsd2si(x: f64) -> i32 {
    let r: i32;
    unsafe {
        asm!("cvtsd2si {r:e}, {x}", r = out(reg) r, x = in(xmm_reg) x,
             options(pure, nomem, nostack, preserves_flags));
    }
    r
}

/// `sqrtsd xmm, xmm`.
#[inline(always)]
pub fn sqrtsd(x: f64) -> f64 {
    let mut r = x;
    unsafe {
        asm!("sqrtsd {r}, {r}", r = inout(xmm_reg) r, options(pure, nomem, nostack, preserves_flags));
    }
    r
}

/// Current MXCSR (`stmxcsr`).
#[inline(always)]
pub fn mxcsr() -> u32 {
    let mut v: u32 = 0;
    unsafe {
        asm!("stmxcsr [{p}]", p = in(reg) &mut v as *mut u32, options(nostack, preserves_flags));
    }
    v
}

/// Current x87 control word (`fnstcw`).
#[inline(always)]
pub fn x87_cw() -> u16 {
    let mut v: u16 = 0;
    unsafe {
        asm!("fnstcw [{p}]", p = in(reg) &mut v as *mut u16, options(nostack, preserves_flags));
    }
    v
}
