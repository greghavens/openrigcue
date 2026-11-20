//! Small double-precision rigid-body maths in OpenVR's right-handed frame
//! (+x right, +y up, +z back, so "forward" is −z).

use std::ops::{Add, AddAssign, Mul, Neg, Sub};

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Vec3 {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

impl Vec3 {
    pub const ZERO: Vec3 = Vec3 { x: 0.0, y: 0.0, z: 0.0 };

    pub const fn new(x: f64, y: f64, z: f64) -> Self {
        Vec3 { x, y, z }
    }

    pub fn from_array(a: [f64; 3]) -> Self {
        Vec3::new(a[0], a[1], a[2])
    }

    pub fn to_array(self) -> [f64; 3] {
        [self.x, self.y, self.z]
    }

    pub fn dot(self, o: Vec3) -> f64 {
        self.x * o.x + self.y * o.y + self.z * o.z
    }

    pub fn cross(self, o: Vec3) -> Vec3 {
        Vec3::new(self.y * o.z - self.z * o.y, self.z * o.x - self.x * o.z, self.x * o.y - self.y * o.x)
    }

    pub fn norm(self) -> f64 {
        self.dot(self).sqrt()
    }

    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite() && self.z.is_finite()
    }
}

impl Add for Vec3 {
    type Output = Vec3;
    fn add(self, o: Vec3) -> Vec3 {
        Vec3::new(self.x + o.x, self.y + o.y, self.z + o.z)
    }
}

impl AddAssign for Vec3 {
    fn add_assign(&mut self, o: Vec3) {
        *self = *self + o;
    }
}

impl Sub for Vec3 {
    type Output = Vec3;
    fn sub(self, o: Vec3) -> Vec3 {
        Vec3::new(self.x - o.x, self.y - o.y, self.z - o.z)
    }
}

impl Neg for Vec3 {
    type Output = Vec3;
    fn neg(self) -> Vec3 {
        Vec3::new(-self.x, -self.y, -self.z)
    }
}

impl Mul<f64> for Vec3 {
    type Output = Vec3;
    fn mul(self, s: f64) -> Vec3 {
        Vec3::new(self.x * s, self.y * s, self.z * s)
    }
}

/// Unit quaternion, Hamilton convention; `a * b` rotates by `b` first, then `a`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Quat {
    pub w: f64,
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

impl Default for Quat {
    fn default() -> Self {
        Quat::IDENTITY
    }
}

impl Quat {
    pub const IDENTITY: Quat = Quat { w: 1.0, x: 0.0, y: 0.0, z: 0.0 };

    pub const fn new(w: f64, x: f64, y: f64, z: f64) -> Self {
        Quat { w, x, y, z }
    }

    /// Rotation by `angle` radians about the unit vector `axis` (right-hand rule).
    pub fn from_axis_angle(axis: Vec3, angle: f64) -> Self {
        let (s, c) = (angle * 0.5).sin_cos();
        Quat::new(c, axis.x * s, axis.y * s, axis.z * s)
    }

    pub fn rot_x(angle: f64) -> Self {
        Quat::from_axis_angle(Vec3::new(1.0, 0.0, 0.0), angle)
    }

    pub fn rot_y(angle: f64) -> Self {
        Quat::from_axis_angle(Vec3::new(0.0, 1.0, 0.0), angle)
    }

    pub fn rot_z(angle: f64) -> Self {
        Quat::from_axis_angle(Vec3::new(0.0, 0.0, 1.0), angle)
    }

    /// Exponential map: the rotation by `|v|` radians about `v`.
    pub fn from_rotation_vector(v: Vec3) -> Self {
        let angle = v.norm();
        if angle < 1e-12 {
            // Second-order accurate for tiny rotations, and exact identity at zero.
            return Quat::new(1.0, v.x * 0.5, v.y * 0.5, v.z * 0.5).normalized();
        }
        Quat::from_axis_angle(v * (1.0 / angle), angle)
    }

    /// Logarithm map: the rotation vector (axis × angle, angle in [0, π]) of this rotation.
    pub fn to_rotation_vector(self) -> Vec3 {
        let q = if self.w < 0.0 { -self } else { self };
        let s = Vec3::new(q.x, q.y, q.z).norm();
        if s < 1e-12 {
            return Vec3::new(q.x, q.y, q.z) * 2.0;
        }
        let angle = 2.0 * s.atan2(q.w);
        Vec3::new(q.x, q.y, q.z) * (angle / s)
    }

    pub fn conj(self) -> Self {
        Quat::new(self.w, -self.x, -self.y, -self.z)
    }

    pub fn dot(self, o: Quat) -> f64 {
        self.w * o.w + self.x * o.x + self.y * o.y + self.z * o.z
    }

    pub fn normalized(self) -> Self {
        let n = self.dot(self).sqrt();
        if n < 1e-12 || !n.is_finite() {
            return Quat::IDENTITY;
        }
        Quat::new(self.w / n, self.x / n, self.y / n, self.z / n)
    }

    pub fn rotate(self, v: Vec3) -> Vec3 {
        // v' = v + 2w(u×v) + 2u×(u×v), u = vector part.
        let u = Vec3::new(self.x, self.y, self.z);
        let t = u.cross(v) * 2.0;
        v + t * self.w + u.cross(t)
    }

    /// Spherical interpolation from identity (`t = 0`) to `self` (`t = 1`).
    pub fn scaled(self, t: f64) -> Self {
        Quat::from_rotation_vector(self.to_rotation_vector() * t)
    }

    /// Heading about +y of the body's forward (−z) axis; 0 = looking along −z, positive = turned left.
    /// Falls back to the right (+x) axis when looking nearly straight up or down.
    pub fn yaw(self) -> f64 {
        let f = self.rotate(Vec3::new(0.0, 0.0, -1.0));
        // (x² + z²) > 0.2², written without hypot(), whose glibc symbol is GLIBC_2.35.
        if f.x * f.x + f.z * f.z > 0.04 {
            return (-f.x).atan2(-f.z);
        }
        let r = self.rotate(Vec3::new(1.0, 0.0, 0.0));
        (-r.z).atan2(r.x)
    }

    pub fn angle(self) -> f64 {
        self.to_rotation_vector().norm()
    }

    pub fn is_finite(self) -> bool {
        self.w.is_finite() && self.x.is_finite() && self.y.is_finite() && self.z.is_finite()
    }
}

impl Neg for Quat {
    type Output = Quat;
    fn neg(self) -> Quat {
        Quat::new(-self.w, -self.x, -self.y, -self.z)
    }
}

impl Mul for Quat {
    type Output = Quat;
    fn mul(self, b: Quat) -> Quat {
        let a = self;
        Quat::new(
            a.w * b.w - a.x * b.x - a.y * b.y - a.z * b.z,
            a.w * b.x + a.x * b.w + a.y * b.z - a.z * b.y,
            a.w * b.y - a.x * b.z + a.y * b.w + a.z * b.x,
            a.w * b.z + a.x * b.y - a.y * b.x + a.z * b.w,
        )
    }
}

/// A rigid transform `p ↦ rot·p + pos` (child frame expressed in its parent).
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Rigid {
    pub rot: Quat,
    pub pos: Vec3,
}

impl Rigid {
    pub const IDENTITY: Rigid = Rigid { rot: Quat::IDENTITY, pos: Vec3::ZERO };

    pub const fn new(rot: Quat, pos: Vec3) -> Self {
        Rigid { rot, pos }
    }

    /// `self ∘ inner`: apply `inner` first, then `self`.
    pub fn then_after(self, inner: Rigid) -> Rigid {
        Rigid::new((self.rot * inner.rot).normalized(), self.rot.rotate(inner.pos) + self.pos)
    }

    pub fn inverse(self) -> Rigid {
        let r = self.rot.conj();
        Rigid::new(r, -r.rotate(self.pos))
    }

    pub fn apply(self, p: Vec3) -> Vec3 {
        self.rot.rotate(p) + self.pos
    }

    /// Screw-free interpolation from identity (`t = 0`) to `self` (`t = 1`).
    pub fn scaled(self, t: f64) -> Rigid {
        Rigid::new(self.rot.scaled(t), self.pos * t)
    }

    /// Change of frame: `frame⁻¹ ∘ self ∘ frame`, i.e. this transform as seen from `frame`.
    pub fn conjugated_by(self, frame: Rigid) -> Rigid {
        frame.inverse().then_after(self).then_after(frame)
    }

    pub fn is_finite(self) -> bool {
        self.rot.is_finite() && self.pos.is_finite()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::f64::consts::FRAC_PI_2;

    pub fn assert_vec_near(a: Vec3, b: Vec3, eps: f64) {
        assert!((a - b).norm() < eps, "{a:?} != {b:?}");
    }

    pub fn assert_quat_near(a: Quat, b: Quat, eps: f64) {
        assert!(a.dot(b).abs() > 1.0 - eps, "{a:?} != {b:?}");
    }

    #[test]
    fn rotations_follow_the_right_hand_rule() {
        assert_vec_near(Quat::rot_y(FRAC_PI_2).rotate(Vec3::new(0.0, 0.0, -1.0)), Vec3::new(-1.0, 0.0, 0.0), 1e-12);
        assert_vec_near(Quat::rot_x(FRAC_PI_2).rotate(Vec3::new(0.0, 0.0, -1.0)), Vec3::new(0.0, 1.0, 0.0), 1e-12);
        assert_vec_near(Quat::rot_z(FRAC_PI_2).rotate(Vec3::new(1.0, 0.0, 0.0)), Vec3::new(0.0, 1.0, 0.0), 1e-12);
    }

    #[test]
    fn composition_order() {
        let a = Quat::rot_y(0.3);
        let b = Quat::rot_x(0.7);
        let v = Vec3::new(0.1, 0.2, 0.3);
        assert_vec_near((a * b).rotate(v), a.rotate(b.rotate(v)), 1e-12);
    }

    #[test]
    fn rotation_vector_round_trip() {
        for v in [Vec3::new(0.3, -0.2, 0.9), Vec3::new(1e-9, 0.0, 0.0), Vec3::ZERO, Vec3::new(0.0, 3.0, 0.0)] {
            assert_vec_near(Quat::from_rotation_vector(v).to_rotation_vector(), v, 1e-9);
        }
    }

    #[test]
    fn yaw_extraction() {
        for yaw in [-2.5, -0.4, 0.0, 0.9, 3.0] {
            let q = Quat::rot_y(yaw) * Quat::rot_x(0.3) * Quat::rot_z(-0.2);
            assert!((q.yaw() - yaw).abs() < 1e-9, "{yaw} -> {}", q.yaw());
        }
        // Looking straight down still yields the heading.
        let q = Quat::rot_y(0.5) * Quat::rot_x(-FRAC_PI_2);
        assert!((q.yaw() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn rigid_inverse_and_composition() {
        let a = Rigid::new(Quat::rot_y(0.4) * Quat::rot_x(0.1), Vec3::new(1.0, 2.0, 3.0));
        let b = Rigid::new(Quat::rot_z(-0.6), Vec3::new(-0.5, 0.0, 0.25));
        let p = Vec3::new(0.3, -0.7, 0.2);
        assert_vec_near(a.then_after(b).apply(p), a.apply(b.apply(p)), 1e-12);
        assert_vec_near(a.inverse().apply(a.apply(p)), p, 1e-12);
        let id = a.then_after(a.inverse());
        assert_vec_near(id.pos, Vec3::ZERO, 1e-12);
        assert_quat_near(id.rot, Quat::IDENTITY, 1e-12);
    }

    #[test]
    fn scaled_endpoints() {
        let a = Rigid::new(Quat::rot_x(0.4), Vec3::new(1.0, 0.0, 0.0));
        assert_eq!(a.scaled(0.0).pos, Vec3::ZERO);
        assert_quat_near(a.scaled(0.0).rot, Quat::IDENTITY, 1e-12);
        assert_quat_near(a.scaled(1.0).rot, a.rot, 1e-12);
        assert_quat_near(a.scaled(0.5).rot, Quat::rot_x(0.2), 1e-12);
    }
}
