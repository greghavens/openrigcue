//! Rigid transforms with their time derivatives, and conversion to and from OpenXR's types.
//!
//! A [`Motion`] is a pose `parent_from_local` plus the linear velocity of its origin and its angular
//! velocity, both expressed in the parent frame: the same convention as `XrSpaceVelocity`.

use openrigcue_rigpose::math::{Pose, Quat, Vec3};
use openxr_sys as xr;

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Motion {
    pub pose: Pose,
    pub linear: Vec3,
    pub angular: Vec3,
}

impl Motion {
    pub const IDENTITY: Motion = Motion { pose: Pose::IDENTITY, linear: Vec3::ZERO, angular: Vec3::ZERO };

    /// A transform that does not change over time.
    pub fn fixed(pose: Pose) -> Self {
        Motion { pose, linear: Vec3::ZERO, angular: Vec3::ZERO }
    }

    /// `self · other` with the product rule: `other` is expressed in `self`'s local frame.
    pub fn then(&self, other: &Motion) -> Motion {
        let r = self.pose.rotation;
        let offset = r.rotate(other.pose.translation);
        Motion {
            pose: self.pose.then(other.pose),
            linear: self.linear + self.angular.cross(offset) + r.rotate(other.linear),
            angular: self.angular + r.rotate(other.angular),
        }
    }

    /// The inverse transform `local_from_parent` and its derivatives (expressed in the local frame).
    pub fn inverse(&self) -> Motion {
        let inv = self.pose.rotation.conjugate();
        Motion {
            pose: self.pose.inverse(),
            linear: inv.rotate(self.angular.cross(self.pose.translation) - self.linear),
            angular: -inv.rotate(self.angular),
        }
    }

    /// The motion between two poses `h` seconds apart (finite difference): pose `a`, velocities
    /// `(b − a) / h`.
    pub fn from_poses(a: Pose, b: Pose, h: f64) -> Motion {
        let linear = (b.translation - a.translation).scale(1.0 / h);
        let angular = (b.rotation * a.rotation.conjugate()).normalized().to_rotation_vector().scale(1.0 / h);
        Motion { pose: a, linear, angular }
    }

    /// `X⁻¹ · self · X` for a fixed `x`: the same transform expressed in the frame `x` maps from.
    pub fn conjugated_by(&self, x: Pose) -> Motion {
        let x = Motion::fixed(x);
        x.inverse().then(self).then(&x)
    }

    pub fn is_identity(&self) -> bool {
        *self == Motion::IDENTITY
    }
}

pub fn pose_from_xr(p: &xr::Posef) -> Pose {
    let q = p.orientation;
    let t = p.position;
    Pose::new(
        Quat::new(f64::from(q.w), f64::from(q.x), f64::from(q.y), f64::from(q.z)).normalized(),
        Vec3::new(f64::from(t.x), f64::from(t.y), f64::from(t.z)),
    )
}

pub fn pose_to_xr(p: Pose) -> xr::Posef {
    xr::Posef { orientation: quat_to_xr(p.rotation), position: vec_to_xr(p.translation) }
}

pub fn quat_from_xr(q: &xr::Quaternionf) -> Quat {
    Quat::new(f64::from(q.w), f64::from(q.x), f64::from(q.y), f64::from(q.z)).normalized()
}

pub fn quat_to_xr(q: Quat) -> xr::Quaternionf {
    let q = q.normalized();
    xr::Quaternionf { x: q.x as f32, y: q.y as f32, z: q.z as f32, w: q.w as f32 }
}

pub fn vec_from_xr(v: &xr::Vector3f) -> Vec3 {
    Vec3::new(f64::from(v.x), f64::from(v.y), f64::from(v.z))
}

pub fn vec_to_xr(v: Vec3) -> xr::Vector3f {
    xr::Vector3f { x: v.x as f32, y: v.y as f32, z: v.z as f32 }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A transform moving with constant velocities: pose at time `t`.
    fn moving(t: f64, base: Pose, lin: Vec3, ang: Vec3) -> Pose {
        let rv = ang.scale(t);
        let n = rv.length();
        let dq = if n > 0.0 { Quat::from_axis_angle(rv.scale(1.0 / n), n) } else { Quat::IDENTITY };
        Pose::new((dq * base.rotation).normalized(), base.translation + lin.scale(t))
    }

    fn sample_motion(base: Pose, lin: Vec3, ang: Vec3) -> Motion {
        Motion { pose: base, linear: lin, angular: ang }
    }

    fn assert_close(a: Vec3, b: Vec3, tol: f64) {
        assert!((a - b).length() < tol, "{a:?} vs {b:?}");
    }

    #[test]
    fn composition_and_inverse_match_finite_differences() {
        let a0 = Pose::new((Quat::rot_y(0.4) * Quat::rot_x(0.2)).normalized(), Vec3::new(0.3, 1.1, -0.4));
        let b0 = Pose::new(Quat::rot_z(-0.3), Vec3::new(-0.1, 0.05, 0.6));
        let (la, wa) = (Vec3::new(0.2, -0.1, 0.05), Vec3::new(0.3, 0.7, -0.2));
        let (lb, wb) = (Vec3::new(-0.4, 0.2, 0.1), Vec3::new(-0.5, 0.1, 0.9));
        let h = 1e-6;
        let at = |t: f64| moving(t, a0, la, wa).then(moving(t, b0, lb, wb));
        let fd = Motion::from_poses(at(0.0), at(h), h);
        let m = sample_motion(a0, la, wa).then(&sample_motion(b0, lb, wb));
        assert!(m.pose.distance(fd.pose) < 1e-12);
        assert_close(m.linear, fd.linear, 1e-5);
        assert_close(m.angular, fd.angular, 1e-5);

        let inv_at = |t: f64| moving(t, a0, la, wa).inverse();
        let fd = Motion::from_poses(inv_at(0.0), inv_at(h), h);
        let m = sample_motion(a0, la, wa).inverse();
        assert_close(m.linear, fd.linear, 1e-5);
        assert_close(m.angular, fd.angular, 1e-5);
        let id = sample_motion(a0, la, wa).then(&m);
        assert!(id.pose.distance(Pose::IDENTITY) < 1e-12);
        assert_close(id.linear, Vec3::ZERO, 1e-12);
        assert_close(id.angular, Vec3::ZERO, 1e-12);
    }

    #[test]
    fn xr_conversion_round_trips() {
        let p = Pose::new((Quat::rot_y(1.0) * Quat::rot_z(0.5)).normalized(), Vec3::new(1.5, -2.0, 0.25));
        assert!(pose_from_xr(&pose_to_xr(p)).distance(p) < 1e-6);
    }
}
