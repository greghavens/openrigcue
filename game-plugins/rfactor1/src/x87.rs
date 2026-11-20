//! The x87 routines the SSE2 entry points fall back to when the
//! caller's FP environment is not the default one (or the argument is huge,
//! infinite or NaN): cos, sin and sqrt, with their loading of special
//! values and their exit path.
//!
//! The arithmetic is inline asm, so the
//! x87 status flags, the precision/rounding used and the control word left
//! behind are fixed. On the error branches, with the relevant exception
//! masked (the normal case), the value computed here is returned and the
//! matching status flag is raised. With it unmasked no IEEE exception is
//! raised.

use core::arch::asm;

const INDEFINITE: u64 = 0xfff8_0000_0000_0000; // the real indefinite as a double
const CW_DOUBLE: u16 = 0x027f; // double precision, all masked, nearest

#[inline(always)]
fn fstcw() -> u16 {
    let mut v: u16 = 0;
    unsafe { asm!("fnstcw [{p}]", p = in(reg) &mut v as *mut u16, options(nostack)) };
    v
}

#[inline(always)]
fn fldcw(v: u16) {
    unsafe { asm!("fldcw [{p}]", p = in(reg) &v as *const u16, options(nostack)) };
}

/// The load of an infinite / NaN double: the 80-bit value with
/// exponent 0x7fff and the 52 mantissa bits shifted up by 11 (no invalid
/// operation is raised for a signalling NaN, unlike `fld qword`).
#[inline(always)]
fn ext_special(x: u64) -> [u8; 10] {
    let hi = (x >> 32) as u32;
    let lo = x as u32;
    let top = hi | 0x7fff_0000;
    let m_hi = (hi << 11) | (lo >> 21);
    let m_lo = lo << 11;
    let mut b = [0u8; 10];
    b[0..4].copy_from_slice(&m_lo.to_le_bytes());
    b[4..8].copy_from_slice(&m_hi.to_le_bytes());
    b[8..10].copy_from_slice(&((top >> 16) as u16).to_le_bytes());
    b
}

/// Loads an 80-bit value and stores it as a double.
#[inline(always)]
fn ext_to_double(v: &[u8; 10]) -> u64 {
    let mut r: u64 = 0;
    unsafe {
        asm!("fld tbyte ptr [{v}]", "fstp qword ptr [{r}]",
             v = in(reg) v.as_ptr(), r = in(reg) &mut r as *mut u64, options(nostack));
    }
    r
}

/// The common body of cos/sin for finite arguments: `fldcw 0x27f` (when the
/// saved word differs), `fsin`/`fcos`, the `fprem1` reduction loop when C2
/// is set, the exit path (`math_exit`), then the store as a double.
#[inline(always)]
fn trig(x: u64, is_sin: bool) -> u64 {
    let saved = fstcw();
    if saved != CW_DOUBLE {
        fldcw(CW_DOUBLE);
    }
    let reduce = crate::libm::REDUCE;
    let mut sw: u16;
    unsafe {
        if is_sin {
            asm!("fld qword ptr [{x}]", "fsin", "fnstsw ax",
                 x = in(reg) &x as *const u64, out("ax") sw, options(nostack));
        } else {
            asm!("fld qword ptr [{x}]", "fcos", "fnstsw ax",
                 x = in(reg) &x as *const u64, out("ax") sw, options(nostack));
        }
        if sw & 0x0400 != 0 {
            // reduce by REDUCE with fprem1 until C2 clears, then fsin/fcos again
            if is_sin {
                asm!("fld tbyte ptr [{c}]", "fxch st(1)", "2:", "fprem1", "fnstsw ax",
                     "test ax, 0x400", "jnz 2b", "fstp st(1)", "fsin", "fnstsw ax",
                     c = in(reg) reduce.as_ptr(), out("ax") sw, options(nostack));
            } else {
                asm!("fld tbyte ptr [{c}]", "fxch st(1)", "2:", "fprem1", "fnstsw ax",
                     "test ax, 0x400", "jnz 2b", "fstp st(1)", "fcos", "fnstsw ax",
                     c = in(reg) reduce.as_ptr(), out("ax") sw, options(nostack));
            }
        }
    }
    math_exit(saved, sw);
    let mut r: u64 = 0;
    unsafe { asm!("fstp qword ptr [{r}]", r = in(reg) &mut r as *mut u64, options(nostack)) };
    r
}

/// The exit path (value in st0): control word 0x27f -> nothing;
/// precision exception unmasked, or PE not raised -> reload the saved
/// control word; PM masked and PE (sticky) set -> the error handling for an
/// inexact result, which only rounds the value to double through memory.
#[inline(always)]
fn math_exit(saved: u16, _sw: u16) {
    if saved == CW_DOUBLE {
        return;
    }
    if saved & 0x20 == 0 {
        fldcw(saved);
        return;
    }
    let sw: u16;
    unsafe { asm!("fnstsw ax", out("ax") sw, options(nostack, nomem)) };
    if sw & 0x20 == 0 {
        fldcw(saved);
        return;
    }
    // Inexact result with PE masked: PE is raised again.
    error_exit(saved, 0x20);
}

/// 80-bit 2^1041 (`fistp dword` of it is an invalid operation,
/// `fstp qword` of it overflows) and 80-bit 2^-12287 + 1 ulp
/// (`fstp qword` underflows).
pub(crate) static STATFP_BIG: [u8; 10] = [0, 0, 0, 0, 0, 0, 0, 0x80, 0x10, 0x44];
pub(crate) static STATFP_TINY: [u8; 10] = [1, 0, 0, 0, 0, 0, 0, 0x80, 0x00, 0x30];

/// Raises the x87 exceptions in `mask` by computing them.
///   0x01 IE: the big constant stored as a 32-bit integer
///   0x08 OE: the big constant stored as a double
///   0x10 UE: the tiny constant stored as a double
///   0x04 ZE: 1 / 0
///   0x20 PE: pi stored as a double
/// It is called with 1 for a masked invalid operation
/// (DOMAIN error) and with 0x20 for a masked
/// inexact result.
#[inline(never)]
fn set_statfp(mask: u32) {
    let mut tmp = [0u32; 3];
    unsafe {
        asm!(
            "test cl, 1",
            "jz 2f",
            "fld tbyte ptr [{big}]",
            "fistp dword ptr [{t} + 8]",
            "fwait",
            "2:",
            "test cl, 8",
            "jz 3f",
            "fwait",
            "fnstsw ax",
            "fld tbyte ptr [{big}]",
            "fstp qword ptr [{t}]",
            "fwait",
            "fwait",
            "fnstsw ax",
            "3:",
            "test cl, 0x10",
            "jz 4f",
            "fld tbyte ptr [{tiny}]",
            "fstp qword ptr [{t}]",
            "fwait",
            "4:",
            "test cl, 4",
            "jz 5f",
            "fldz",
            "fld1",
            ".byte 0xde, 0xf1", // fdivrp st(1), st (1 / 0)
            "fstp st(0)",
            "fwait",
            "5:",
            "test cl, 0x20",
            "jz 6f",
            "fldpi",
            "fstp qword ptr [{t}]",
            "fwait",
            "6:",
            big = in(reg) STATFP_BIG.as_ptr(),
            tiny = in(reg) STATFP_TINY.as_ptr(),
            t = in(reg) tmp.as_mut_ptr(),
            in("ecx") mask,
            out("eax") _,
            options(nostack),
        );
    }
}

/// The error exit when the error is handled without raising:
/// the value is stored as a double under the current control
/// word, the saved control word is restored, the value is loaded again, and
/// the saved word is loaded once more (when
/// it is not 0x27f). `statfp` is the `set_statfp` mask
/// raised between the store and the control-word reload (0 for
/// none).
#[inline(always)]
fn error_exit(saved: u16, statfp: u32) {
    let mut tmp: u64 = 0;
    unsafe { asm!("fstp qword ptr [{t}]", t = in(reg) &mut tmp as *mut u64, options(nostack)) };
    if statfp != 0 {
        set_statfp(statfp);
    }
    fldcw(saved);
    unsafe { asm!("fld qword ptr [{t}]", t = in(reg) &tmp as *const u64, options(nostack)) };
    if saved != CW_DOUBLE {
        fldcw(saved);
    }
}

/// Replaces st0 with the real indefinite and handles the DOMAIN error.
/// With IM masked IE is raised through `set_statfp(1)`;
/// with IM unmasked no
/// STATUS_FLOAT_INVALID_OPERATION is raised.
#[inline(always)]
fn domain_error(saved: u16) -> u64 {
    let ind: [u8; 10] = [0, 0, 0, 0, 0, 0, 0, 0xc0, 0xff, 0xff];
    unsafe { asm!("fstp st(0)", "fld tbyte ptr [{i}]", i = in(reg) ind.as_ptr(), options(nostack)) };
    // IM masked -> IE is raised.
    error_exit(saved, if saved & 1 != 0 { 1 } else { 0 });
    let mut r: u64 = 0;
    unsafe { asm!("fstp qword ptr [{r}]", r = in(reg) &mut r as *mut u64, options(nostack)) };
    let _ = INDEFINITE;
    r
}

#[inline(always)]
fn trig_entry(x: u64, is_sin: bool) -> u64 {
    let hi = (x >> 32) as u32;
    if hi & 0x7ff0_0000 != 0x7ff0_0000 {
        return trig(x, is_sin);
    }
    let e = ext_special(x);
    if hi & 0xfffff == 0 && x as u32 == 0 {
        let saved = fstcw();
        unsafe { asm!("fld tbyte ptr [{e}]", e = in(reg) e.as_ptr(), options(nostack)) };
        return domain_error(saved);
    }
    // NaN: returned as loaded; the caller's fstp quiets a signalling NaN.
    ext_to_double(&e)
}

/// x87 cosine of a double given and returned as bits.
pub fn cos(x: u64) -> u64 {
    trig_entry(x, false)
}

/// x87 sine of a double given and returned as bits.
pub fn sin(x: u64) -> u64 {
    trig_entry(x, true)
}

/// x87 square root of a double given and returned as bits.
pub fn sqrt(x: u64) -> u64 {
    let hi = (x >> 32) as u32;
    let lo = x as u32;
    let special = hi & 0x7ff0_0000 == 0x7ff0_0000;
    // load the argument (special values through their 80-bit form)
    let e = ext_special(x);
    unsafe {
        if special {
            asm!("fld tbyte ptr [{e}]", e = in(reg) e.as_ptr(), options(nostack));
        } else {
            asm!("fld qword ptr [{x}]", x = in(reg) &x as *const u64, options(nostack));
        }
    }
    let saved = fstcw();
    if special {
        if hi & 0xfffff != 0 || lo != 0 {
            // A signalling NaN is quieted by adding 1.0;
            // then the error exit with no flag.
            if hi & 0x80000 == 0 {
                let one: u64 = 0x3ff0_0000_0000_0000;
                unsafe { asm!("fadd qword ptr [{o}]", o = in(reg) &one as *const u64, options(nostack)) };
            }
            error_exit(saved, 0);
            return fstp_result();
        }
        if hi & 0x8000_0000 == 0 {
            // +inf: no fsqrt, the exit path.
            math_exit(saved, 0);
            return fstp_result();
        }
        return domain_error(saved);
    }
    if saved != CW_DOUBLE {
        // Keep the precision control, mask everything, nearest.
        fldcw((saved & 0x300) | 0x7f);
    }
    if hi & 0x8000_0000 != 0 {
        if hi & 0x7ff0_0000 == 0 && hi & 0xfffff == 0 && lo == 0 {
            // -0.0: no fsqrt.
            math_exit(saved, 0);
            return fstp_result();
        }
        return domain_error(saved);
    }
    unsafe { asm!("fsqrt", options(nostack, nomem)) };
    math_exit(saved, 0);
    fstp_result()
}

/// Stores st0 as a double and returns its bits.
#[inline(always)]
fn fstp_result() -> u64 {
    let mut r: u64 = 0;
    unsafe { asm!("fstp qword ptr [{r}]", r = in(reg) &mut r as *mut u64, options(nostack)) };
    r
}
