//! Quaternion maths and the pose arithmetic of the device.
//!
//! Every floating-point operation is issued as one explicit SSE2 instruction with fixed
//! destination/source operands (module [`sse`]), so the IEEE-754 results are fixed bit for bit, NaN signs
//! and payloads and signed zeros included: the compiler may not reassociate, commute or fold them (plain Rust
//! float expressions leave NaN bits unspecified, and LLVM folds `(-x) * c` into `x * (-c)`).

use crate::openvr::HmdQuaternion;
use sse::{add, div, mul, neg, sub};

#[cfg(not(target_arch = "x86_64"))]
compile_error!("the driver is the x86_64 Linux build of an x86_64 Windows DLL");

/// SSE2 scalar double instructions. `op(dst, src)` is AT&T `opsd src, dst`: `dst = dst op src`.
pub mod sse {
    use std::arch::asm;

    #[inline(always)]
    pub fn mul(dst: f64, src: f64) -> f64 {
        let mut r = dst;
        // SAFETY: register-only arithmetic.
        unsafe { asm!("mulsd {r}, {s}", r = inout(xmm_reg) r, s = in(xmm_reg) src, options(pure, nomem, nostack)) };
        r
    }

    #[inline(always)]
    pub fn add(dst: f64, src: f64) -> f64 {
        let mut r = dst;
        // SAFETY: register-only arithmetic.
        unsafe { asm!("addsd {r}, {s}", r = inout(xmm_reg) r, s = in(xmm_reg) src, options(pure, nomem, nostack)) };
        r
    }

    #[inline(always)]
    pub fn sub(dst: f64, src: f64) -> f64 {
        let mut r = dst;
        // SAFETY: register-only arithmetic.
        unsafe { asm!("subsd {r}, {s}", r = inout(xmm_reg) r, s = in(xmm_reg) src, options(pure, nomem, nostack)) };
        r
    }

    #[inline(always)]
    pub fn div(dst: f64, src: f64) -> f64 {
        let mut r = dst;
        // SAFETY: register-only arithmetic.
        unsafe { asm!("divsd {r}, {s}", r = inout(xmm_reg) r, s = in(xmm_reg) src, options(pure, nomem, nostack)) };
        r
    }

    /// `xorps` with the sign mask: negation.
    #[inline(always)]
    pub fn neg(x: f64) -> f64 {
        let mut r = x;
        let mask = f64::from_bits(0x8000_0000_0000_0000);
        // SAFETY: register-only bit operation.
        unsafe { asm!("xorps {r}, {m}", r = inout(xmm_reg) r, m = in(xmm_reg) mask, options(pure, nomem, nostack)) };
        r
    }
}

/// Degrees to radians (bits 0x3F91DF46A2529D37).
pub const DEG_TO_RAD: f64 = 0.017_453_292_519_943_29;
/// Millimetres to metres divisor.
pub const MM_PER_M: f64 = 1000.0;

/// The quaternion product `a * b`.
pub fn quat_mul(a: &HmdQuaternion, b: &HmdQuaternion) -> HmdQuaternion {
    let (a0, a1, a2, a3) = (a.w, a.x, a.y, a.z);
    let (b0, b1, b2, b3) = (b.w, b.x, b.y, b.z);
    HmdQuaternion {
        w: sub(sub(sub(mul(b0, a0), mul(b1, a1)), mul(b2, a2)), mul(b3, a3)),
        x: sub(add(add(mul(a1, b0), mul(b1, a0)), mul(b3, a2)), mul(a3, b2)),
        y: sub(add(add(mul(a2, b0), mul(b2, a0)), mul(a3, b1)), mul(b3, a1)),
        z: sub(add(add(mul(a3, b0), mul(b3, a0)), mul(b2, a1)), mul(a2, b1)),
    }
}

/// The quaternion of a yaw, roll and pitch.
///
/// The cos/sin calls are in this order: pitch first (cos, then sin), then roll, then yaw;
/// the yaw/roll product is formed inline and multiplied by the pitch quaternion with [`quat_mul`].
pub fn quaternion_from_yaw_roll_pitch(yaw: f64, roll: f64, pitch: f64) -> HmdQuaternion {
    quaternion_from_yaw_roll_pitch_with(yaw, roll, pitch, trig::cos, trig::sin)
}

/// The C runtime's `cos`/`sin` (the platform libm).
pub mod trig {
    #[cfg(test)]
    pub static MUSL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    pub fn cos(x: f64) -> f64 {
        #[cfg(test)]
        if MUSL.load(std::sync::atomic::Ordering::Relaxed) {
            return libm::cos(x);
        }
        x.cos()
    }

    pub fn sin(x: f64) -> f64 {
        #[cfg(test)]
        if MUSL.load(std::sync::atomic::Ordering::Relaxed) {
            return libm::sin(x);
        }
        x.sin()
    }
}

/// [`quaternion_from_yaw_roll_pitch`] with the cos and sin functions passed in.
pub fn quaternion_from_yaw_roll_pitch_with(
    yaw: f64,
    roll: f64,
    pitch: f64,
    cos: fn(f64) -> f64,
    sin: fn(f64) -> f64,
) -> HmdQuaternion {
    let half = 0.5;
    let hp = mul(pitch, half);
    let q_pitch = HmdQuaternion { w: cos(hp), x: sin(hp), y: 0.0, z: 0.0 };
    let hr = mul(roll, half);
    let (cr, sr) = (cos(hr), sin(hr));
    let hy = mul(yaw, half);
    let (cy, sy) = (cos(hy), sin(hy));
    let zero = 0.0f64; // xorps xmm7, xmm7
    let cy0 = mul(cy, zero);
    let sr0 = mul(sr, zero);
    let cr0 = mul(cr, zero);
    let sy0 = mul(sy, zero);
    let q_yaw_roll = HmdQuaternion {
        w: sub(sub(sub(mul(cy, cr), zero), sy0), sr0),
        x: sub(add(mul(sy, sr), add(cy0, cr0)), zero),
        y: sub(add(add(mul(sy, cr), cy0), zero), sr0),
        z: sub(add(add(mul(cy, sr), cr0), zero), sy0),
    };
    quat_mul(&q_yaw_roll, &q_pitch)
}

/// Rotates the vector `v` by `q`: `q * (0, v) * conj(q)`, vector part.
pub fn quaternion_rotate_vector(q: &HmdQuaternion, v: &[f64; 3]) -> [f64; 3] {
    let p = HmdQuaternion { w: 0.0, x: v[0], y: v[1], z: v[2] };
    let t = quat_mul(q, &p);
    let (q0, n1, n2, n3) = (q.w, neg(q.x), neg(q.y), neg(q.z));
    let (t0, t1, t2, t3) = (t.w, t.x, t.y, t.z);
    [
        sub(add(add(mul(n1, t0), mul(q0, t1)), mul(n3, t2)), mul(n2, t3)),
        sub(add(add(mul(n2, t0), mul(q0, t2)), mul(n1, t3)), mul(n3, t1)),
        sub(add(add(mul(n3, t0), mul(q0, t3)), mul(n2, t1)), mul(n1, t2)),
    ]
}

/// The rig arithmetic of the pose: mm -> m translation, degrees -> radians rotation
/// (yaw plus OVRMC's yaw offset, roll negated), rotated translation plus OVRMC's offset.
/// `rig` is the rig pose section: sway, surge, heave, yaw, roll, pitch.
pub fn rig_pose(rig: &[f64; 6], rig_offset: &[f64; 3], rig_yaw_offset: f64) -> (HmdQuaternion, [f64; 3]) {
    let translation = [div(rig[0], MM_PER_M), div(rig[2], MM_PER_M), div(neg(rig[1]), MM_PER_M)];
    let pitch = mul(rig[5], DEG_TO_RAD);
    let roll = mul(neg(rig[4]), DEG_TO_RAD);
    let yaw = add(mul(rig[3], DEG_TO_RAD), rig_yaw_offset);
    let q = quaternion_from_yaw_roll_pitch(yaw, roll, pitch);
    let v = quaternion_rotate_vector(&q, &translation);
    (q, [add(v[0], rig_offset[0]), add(v[1], rig_offset[1]), add(v[2], rig_offset[2])])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_ops_keep_operand_order() {
        let qnan = f64::from_bits(0x7ff8_0000_0000_0001);
        let other = f64::from_bits(0xfff8_0000_0000_0002);
        // Two NaN operands: the destination's NaN wins.
        assert_eq!(mul(qnan, other).to_bits(), qnan.to_bits());
        assert_eq!(mul(other, qnan).to_bits(), other.to_bits());
        // Invalid operation: the x86 default NaN (negative).
        assert_eq!(sub(f64::INFINITY, f64::INFINITY).to_bits(), 0xfff8_0000_0000_0000);
        assert_eq!(neg(qnan).to_bits(), 0xfff8_0000_0000_0001);
        assert_eq!(sub(-0.0, 0.0).to_bits(), (-0.0f64).to_bits());
    }

    #[test]
    fn constants_are_the_binary_bit_patterns() {
        assert_eq!(DEG_TO_RAD.to_bits(), 0x3F91_DF46_A252_9D37);
        assert_eq!(MM_PER_M.to_bits(), 0x408F_4000_0000_0000);
    }

    #[test]
    fn identity_rotation() {
        let q = quaternion_from_yaw_roll_pitch(0.0, 0.0, 0.0);
        assert_eq!(q, HmdQuaternion { w: 1.0, x: 0.0, y: 0.0, z: 0.0 });
        assert_eq!(quaternion_rotate_vector(&q, &[1.0, 2.0, 3.0]), [1.0, 2.0, 3.0]);
    }
}
