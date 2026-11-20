//! Minimal double-precision rigid-body math shared by the rig-pose consumers.
//!
//! Frame convention (OpenXR and OpenVR agree): right-handed, +x right, +y up, −z forward, metres and
//! radians. A [`Pose`] maps points from its local frame into its parent frame
//! (`parent_from_local`), so `a.then(b)` is the usual matrix product `a · b`.

use std::ops::{Add, Mul, Neg, Sub};

/// A 3-vector.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
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

    pub fn dot(self, o: Vec3) -> f64 {
        self.x * o.x + self.y * o.y + self.z * o.z
    }

    pub fn cross(self, o: Vec3) -> Vec3 {
        Vec3::new(self.y * o.z - self.z * o.y, self.z * o.x - self.x * o.z, self.x * o.y - self.y * o.x)
    }

    pub fn length(self) -> f64 {
        self.dot(self).sqrt()
    }

    pub fn scale(self, s: f64) -> Vec3 {
        Vec3::new(self.x * s, self.y * s, self.z * s)
    }
}

impl Add for Vec3 {
    type Output = Vec3;
    fn add(self, o: Vec3) -> Vec3 {
        Vec3::new(self.x + o.x, self.y + o.y, self.z + o.z)
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

/// A unit quaternion (Hamilton convention, `w` scalar).
#[derive(Debug, Clone, Copy, PartialEq)]
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
    pub fn from_axis_angle(axis: Vec3, angle: f64) -> Quat {
        let (s, c) = (angle * 0.5).sin_cos();
        Quat::new(c, axis.x * s, axis.y * s, axis.z * s)
    }

    /// Rotation about +x.
    pub fn rot_x(angle: f64) -> Quat {
        Quat::from_axis_angle(Vec3::new(1.0, 0.0, 0.0), angle)
    }

    /// Rotation about +y (up): positive turns −z (forward) towards −x (left).
    pub fn rot_y(angle: f64) -> Quat {
        Quat::from_axis_angle(Vec3::new(0.0, 1.0, 0.0), angle)
    }

    /// Rotation about +z (backward).
    pub fn rot_z(angle: f64) -> Quat {
        Quat::from_axis_angle(Vec3::new(0.0, 0.0, 1.0), angle)
    }

    pub fn conjugate(self) -> Quat {
        Quat::new(self.w, -self.x, -self.y, -self.z)
    }

    pub fn norm(self) -> f64 {
        (self.w * self.w + self.x * self.x + self.y * self.y + self.z * self.z).sqrt()
    }

    /// Unit-length copy; the identity for a degenerate (zero or non-finite) input.
    pub fn normalized(self) -> Quat {
        let n = self.norm();
        if n.is_finite() && n > 1e-12 {
            Quat::new(self.w / n, self.x / n, self.y / n, self.z / n)
        } else {
            Quat::IDENTITY
        }
    }

    /// Rotates `v` by this (unit) quaternion.
    pub fn rotate(self, v: Vec3) -> Vec3 {
        // v' = v + 2w(u×v) + 2u×(u×v), u = vector part.
        let u = Vec3::new(self.x, self.y, self.z);
        let t = u.cross(v).scale(2.0);
        v + t.scale(self.w) + u.cross(t)
    }

    /// The rotation vector (axis × angle, radians) of this unit quaternion, angle in [0, π].
    pub fn to_rotation_vector(self) -> Vec3 {
        let q = if self.w < 0.0 { Quat::new(-self.w, -self.x, -self.y, -self.z) } else { self };
        let v = Vec3::new(q.x, q.y, q.z);
        let s = v.length();
        if s < 1e-12 {
            // Small-angle limit: angle ≈ 2s, axis = v/s, so the vector is ≈ 2v.
            return v.scale(2.0);
        }
        let angle = 2.0 * s.atan2(q.w);
        v.scale(angle / s)
    }

    /// Angle in radians between two orientations (0 ..= π).
    pub fn angle_to(self, o: Quat) -> f64 {
        (self.conjugate() * o).to_rotation_vector().length()
    }
}

impl Mul for Quat {
    type Output = Quat;
    /// Hamilton product: `(a * b).rotate(v) == a.rotate(b.rotate(v))`.
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

/// A rigid transform `parent_from_local`: `p_parent = rotation · p_local + translation`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Pose {
    pub rotation: Quat,
    pub translation: Vec3,
}

impl Pose {
    pub const IDENTITY: Pose = Pose { rotation: Quat::IDENTITY, translation: Vec3::ZERO };

    pub const fn new(rotation: Quat, translation: Vec3) -> Self {
        Pose { rotation, translation }
    }

    /// `self · other`: apply `other` first, then `self`.
    pub fn then(self, other: Pose) -> Pose {
        Pose::new(
            (self.rotation * other.rotation).normalized(),
            self.translation + self.rotation.rotate(other.translation),
        )
    }

    pub fn inverse(self) -> Pose {
        let r = self.rotation.conjugate();
        Pose::new(r, -r.rotate(self.translation))
    }

    pub fn transform_point(self, p: Vec3) -> Vec3 {
        self.rotation.rotate(p) + self.translation
    }

    /// Largest of the translation distance (metres) and rotation angle (radians) to `o`; for tests and
    /// tolerances.
    pub fn distance(self, o: Pose) -> f64 {
        (self.translation - o.translation).length().max(self.rotation.angle_to(o.rotation))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f64 = 1e-12;

    fn close(a: Vec3, b: Vec3) -> bool {
        (a - b).length() < 1e-9
    }

    #[test]
    fn basic_rotations_follow_the_right_hand_rule() {
        let forward = Vec3::new(0.0, 0.0, -1.0);
        let half_pi = std::f64::consts::FRAC_PI_2;
        // +90° about +y turns forward (−z) to the left (−x).
        assert!(close(Quat::rot_y(half_pi).rotate(forward), Vec3::new(-1.0, 0.0, 0.0)));
        // +90° about +x turns forward (−z) up (+y).
        assert!(close(Quat::rot_x(half_pi).rotate(forward), Vec3::new(0.0, 1.0, 0.0)));
        // +90° about +z turns right (+x) up (+y).
        assert!(close(Quat::rot_z(half_pi).rotate(Vec3::new(1.0, 0.0, 0.0)), Vec3::new(0.0, 1.0, 0.0)));
    }

    #[test]
    fn product_composes_like_matrices() {
        let a = Quat::rot_y(0.3);
        let b = Quat::rot_x(-0.7);
        let v = Vec3::new(0.2, -1.0, 3.0);
        assert!(close((a * b).rotate(v), a.rotate(b.rotate(v))));
    }

    #[test]
    fn pose_inverse_round_trips() {
        let p = Pose::new((Quat::rot_y(0.4) * Quat::rot_z(0.2)).normalized(), Vec3::new(1.0, 2.0, -3.0));
        let id = p.then(p.inverse());
        assert!(id.distance(Pose::IDENTITY) < 1e-12);
        let x = Vec3::new(-0.5, 0.25, 4.0);
        assert!(close(p.inverse().transform_point(p.transform_point(x)), x));
        assert!(close(p.then(p).transform_point(x), p.transform_point(p.transform_point(x))));
    }

    #[test]
    fn rotation_vector_matches_axis_angle() {
        let q = Quat::from_axis_angle(Vec3::new(0.0, 0.6, 0.8), 1.1);
        let rv = q.to_rotation_vector();
        assert!((rv.length() - 1.1).abs() < 1e-12);
        assert!(close(rv.scale(1.0 / 1.1), Vec3::new(0.0, 0.6, 0.8)));
        assert!(Quat::IDENTITY.to_rotation_vector().length() < EPS);
        // The double cover maps to the same short rotation.
        let neg = Quat::new(-q.w, -q.x, -q.y, -q.z);
        assert!(close(neg.to_rotation_vector(), rv));
    }

    #[test]
    fn degenerate_quaternion_normalizes_to_identity() {
        assert_eq!(Quat::new(0.0, 0.0, 0.0, 0.0).normalized(), Quat::IDENTITY);
        assert_eq!(Quat::new(f64::NAN, 0.0, 0.0, 0.0).normalized(), Quat::IDENTITY);
    }
}
