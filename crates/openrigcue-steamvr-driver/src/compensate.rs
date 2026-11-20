//! Removing the rig's motion from a device pose reported to `vrserver`.
//!
//! Model. In room ("world", OpenVR raw tracking) coordinates the seat's neutral frame is `B`
//! (origin at the rig's centre of rotation, gravity-aligned, facing the seat's forward). With the
//! rig at pose `R(t)`, anything fixed to the seat at `x` (seat coordinates) is at `B·R(t)·x`.
//! Applying `C(t) = B·R(t)⁻¹·B⁻¹` to a tracked pose therefore returns everything that rides on the
//! rig to where it would be with the rig at neutral, and leaves the user's own head motion intact.
//! This is the inverse rig motion about the CoR.
//!
//! Device poses arrive in each driver's own world space, `world = W·driver`, so the transform
//! applied to the driver-space pose is `W⁻¹·C·W`. Because compensation happens in raw tracking
//! space, a SteamVR seated/standing recentre (an app-side transform) does not disturb it.
//!
//! Calibration finds `B` from the headset: the CoR is at a configured offset from the head, the
//! seat's heading is the head's heading, and the rig's current pose is folded in so calibration
//! also works with the rig off neutral.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::config::{CorSource, Settings, VelocityMode, SETTINGS_WORDS};
use crate::math::{Quat, Rigid, Vec3};
use crate::openvr::{DriverPose, HmdQuaternion};
use crate::rig::{cor_vector, CorOffset, Predictor, RigReading, RigSource};
use crate::seqlock::{SeqCell, Words};

/// The kinds of tracked device the compensator distinguishes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceKind {
    Hmd,
    Controller,
    Tracker,
    /// Base stations, display redirects, unknown devices: never touched.
    Other,
}

/// Why a pose was passed through unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skip {
    SwitchedOff = 1,
    NotSelected,
    InvalidPose,
    NoRigPose,
    RigDisabled,
    ClaimedByOther,
    Stale,
    Uncalibrated,
    NonFinite,
}

impl Skip {
    const ALL: [Skip; 9] = [
        Skip::SwitchedOff,
        Skip::NotSelected,
        Skip::InvalidPose,
        Skip::NoRigPose,
        Skip::RigDisabled,
        Skip::ClaimedByOther,
        Skip::Stale,
        Skip::Uncalibrated,
        Skip::NonFinite,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Skip::SwitchedOff => "switched-off",
            Skip::NotSelected => "device-not-selected",
            Skip::InvalidPose => "invalid-pose",
            Skip::NoRigPose => "no-rig-pose",
            Skip::RigDisabled => "rig-disabled",
            Skip::ClaimedByOther => "claimed-by-other-consumer",
            Skip::Stale => "rig-pose-stale",
            Skip::Uncalibrated => "uncalibrated",
            Skip::NonFinite => "non-finite",
        }
    }

    pub fn from_code(code: u64) -> Option<Skip> {
        Skip::ALL.into_iter().find(|s| *s as u64 == code)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Compensated,
    /// Compensated at reduced strength while the rig pose goes stale.
    Fading,
    Skipped(Skip),
}

/// The seat's neutral frame in world space, and the headset's world-from-driver at calibration.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Calibration {
    pub base: Rigid,
    pub world_from_driver: Rigid,
}

const CALIBRATION_WORDS: usize = 14;

impl Words<CALIBRATION_WORDS> for Calibration {
    fn to_words(&self) -> [u64; CALIBRATION_WORDS] {
        let mut w = [0u64; CALIBRATION_WORDS];
        for (i, r) in [self.base, self.world_from_driver].iter().enumerate() {
            let v = [r.rot.w, r.rot.x, r.rot.y, r.rot.z, r.pos.x, r.pos.y, r.pos.z];
            for (j, x) in v.into_iter().enumerate() {
                w[i * 7 + j] = x.to_bits();
            }
        }
        w
    }

    fn from_words(w: &[u64; CALIBRATION_WORDS]) -> Self {
        let r = |o: usize| {
            let f = |i: usize| f64::from_bits(w[o + i]);
            Rigid::new(Quat::new(f(0), f(1), f(2), f(3)), Vec3::new(f(4), f(5), f(6)))
        };
        Calibration { base: r(0), world_from_driver: r(7) }
    }
}

/// A change of the headset's world-from-driver transform larger than this (metres / radians)
/// means the tracking universe moved (room setup), which invalidates the calibration.
const UNIVERSE_SHIFT_M: f64 = 0.01;
const UNIVERSE_SHIFT_RAD: f64 = 0.01;

/// Step used to differentiate the compensation transform for velocities.
const DERIVATIVE_STEP_S: f64 = 0.002;

/// Counters for the status file.
#[derive(Default)]
pub struct Counters {
    pub compensated: AtomicU64,
    pub skipped: AtomicU64,
    pub calibrations: AtomicU64,
    /// [`Skip`] code of the latest pass-through of a headset pose (0 = none yet).
    pub last_hmd_skip: AtomicU64,
    /// `CLOCK_MONOTONIC` of the latest compensated headset pose.
    pub last_hmd_compensated_ns: AtomicU64,
}

/// Shared by every pose thread; all methods are lock-free and allocation-free.
pub struct Compensator<S> {
    source: S,
    settings: SeqCell<SETTINGS_WORDS>,
    calibration: SeqCell<CALIBRATION_WORDS>,
    predictor: Predictor,
    recenter: AtomicBool,
    /// Last seen recentre generation of the publisher, plus one (0 = none seen).
    generation: AtomicU64,
    pub counters: Counters,
}

pub fn quat_from(q: HmdQuaternion) -> Quat {
    Quat::new(q.w, q.x, q.y, q.z)
}

pub fn quat_to(q: Quat) -> HmdQuaternion {
    HmdQuaternion { w: q.w, x: q.x, y: q.y, z: q.z }
}

impl<S: RigSource> Compensator<S> {
    pub fn new(source: S, settings: Settings) -> Self {
        let c = Compensator {
            source,
            settings: SeqCell::new(),
            calibration: SeqCell::new(),
            predictor: Predictor::new(settings.rate_smoothing_ms.max(0.0) * 1e-3),
            recenter: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            counters: Counters::default(),
        };
        c.settings.store(&settings);
        c
    }

    pub fn source(&self) -> &S {
        &self.source
    }

    pub fn settings(&self) -> Settings {
        self.settings.load().unwrap_or_default()
    }

    pub fn set_settings(&self, settings: Settings) {
        self.settings.store(&settings);
    }

    pub fn calibration(&self) -> Option<Calibration> {
        self.calibration.load()
    }

    /// Recalibrates from the next valid headset pose with a live rig pose.
    pub fn request_recenter(&self) {
        self.recenter.store(true, Ordering::Release);
    }

    pub fn recenter_pending(&self) -> bool {
        self.recenter.load(Ordering::Acquire)
    }

    pub fn clear_calibration(&self) {
        self.calibration.clear();
    }

    /// Compensates `pose` in place. `now_ns` is `CLOCK_MONOTONIC` at the pose update call.
    /// A skipped pose is left exactly as it was.
    pub fn process(&self, kind: DeviceKind, pose: &mut DriverPose, now_ns: u64) -> Outcome {
        let outcome = self.process_inner(kind, pose, now_ns);
        match outcome {
            Outcome::Skipped(skip) => {
                self.counters.skipped.fetch_add(1, Ordering::Relaxed);
                if kind == DeviceKind::Hmd {
                    self.counters.last_hmd_skip.store(skip as u64, Ordering::Relaxed);
                }
            }
            Outcome::Compensated | Outcome::Fading => {
                self.counters.compensated.fetch_add(1, Ordering::Relaxed);
                if kind == DeviceKind::Hmd {
                    self.counters.last_hmd_skip.store(0, Ordering::Relaxed);
                    self.counters.last_hmd_compensated_ns.store(now_ns, Ordering::Relaxed);
                }
            }
        }
        outcome
    }

    fn process_inner(&self, kind: DeviceKind, pose: &mut DriverPose, now_ns: u64) -> Outcome {
        let settings = self.settings();
        if !settings.enabled {
            return Outcome::Skipped(Skip::SwitchedOff);
        }
        let selected = match kind {
            DeviceKind::Hmd => true,
            DeviceKind::Controller => settings.compensate_controllers,
            DeviceKind::Tracker => settings.compensate_trackers,
            DeviceKind::Other => false,
        };
        if !selected {
            return Outcome::Skipped(Skip::NotSelected);
        }
        if !pose.pose_is_valid {
            return Outcome::Skipped(Skip::InvalidPose);
        }
        let sample = match self.source.read() {
            RigReading::Absent => return Outcome::Skipped(Skip::NoRigPose),
            RigReading::ClaimedByOther => return Outcome::Skipped(Skip::ClaimedByOther),
            RigReading::Live(s) => s,
        };
        if sample.timestamp_ns == 0 {
            return Outcome::Skipped(Skip::NoRigPose);
        }
        // A new recentre generation from the daemon means "recalibrate now".
        let generation = u64::from(sample.recenter_generation) + 1;
        let seen = self.generation.swap(generation, Ordering::AcqRel);
        if seen != 0 && seen != generation {
            self.request_recenter();
        }
        if !sample.enabled {
            return Outcome::Skipped(Skip::RigDisabled);
        }
        if !sample.values.is_finite() || !sample.rates.is_none_or(|r| r.is_finite()) {
            return Outcome::Skipped(Skip::NonFinite);
        }
        let motion = self.predictor.observe(sample);

        // Strength: full while fresh, then a linear fade to zero.
        let age_s = sample.age_ns(now_ns) as f64 * 1e-9;
        let stale_s = settings.stale_after_ms.max(0.0) * 1e-3;
        let fade_s = settings.fade_ms.max(0.0) * 1e-3;
        let gain = if age_s <= stale_s {
            1.0
        } else if fade_s > 0.0 {
            1.0 - (age_s - stale_s) / fade_s
        } else {
            0.0
        };
        if gain <= 0.0 {
            return Outcome::Skipped(Skip::Stale);
        }

        // The rig pose at the instant this device pose describes: the call time plus the driver's
        // poseTimeOffset, against the measurement time (timestamp minus the configured latency).
        let pose_time_ns = now_ns as f64 + pose.pose_time_offset * 1e9;
        let measured_ns = sample.timestamp_ns as f64 - settings.rig_latency_ms * 1e6;
        let max_extrapolation_s = settings.max_extrapolation_ms.max(0.0) * 1e-3;
        let rig_at = |extra_s: f64| -> Rigid {
            motion.transform_at((pose_time_ns - measured_ns) * 1e-9 + extra_s, max_extrapolation_s)
        };

        let world_from_driver = Rigid::new(
            quat_from(pose.q_world_from_driver_rotation).normalized(),
            Vec3::from_array(pose.vec_world_from_driver_translation),
        );
        let driver_pose = Rigid::new(quat_from(pose.q_rotation).normalized(), Vec3::from_array(pose.vec_position));
        if !world_from_driver.is_finite() || !driver_pose.is_finite() {
            return Outcome::Skipped(Skip::NonFinite);
        }

        if kind == DeviceKind::Hmd {
            let offset = match settings.cor_source {
                CorSource::File => cor_vector(sample.cor_offset),
                CorSource::Config => cor_vector(CorOffset {
                    forward: settings.cor_forward_m,
                    down: settings.cor_down_m,
                    right: settings.cor_right_m,
                }),
            };
            self.maybe_calibrate(&settings, pose, world_from_driver, driver_pose, rig_at(0.0), offset);
        }
        let Some(calibration) = self.calibration() else {
            return Outcome::Skipped(Skip::Uncalibrated);
        };

        let compensation_at = |extra_s: f64| -> Rigid {
            let rig = rig_at(extra_s);
            let world = calibration.base.then_after(rig.inverse()).then_after(calibration.base.inverse());
            world.scaled(gain).conjugated_by(world_from_driver)
        };
        let c = compensation_at(0.0);
        let mut out = *pose;
        let rotated_pos = c.rot.rotate(driver_pose.pos);
        out.vec_position = (rotated_pos + c.pos).to_array();
        out.q_rotation = quat_to((c.rot * driver_pose.rot).normalized());

        let rotate = |v: [f64; 3]| c.rot.rotate(Vec3::from_array(v));
        match settings.velocity {
            VelocityMode::Zero => {
                out.vec_velocity = [0.0; 3];
                out.vec_angular_velocity = [0.0; 3];
                out.vec_acceleration = [0.0; 3];
                out.vec_angular_acceleration = [0.0; 3];
            }
            VelocityMode::Keep => {
                out.vec_velocity = rotate(pose.vec_velocity).to_array();
                out.vec_angular_velocity = rotate(pose.vec_angular_velocity).to_array();
                out.vec_acceleration = rotate(pose.vec_acceleration).to_array();
                out.vec_angular_acceleration = rotate(pose.vec_angular_acceleration).to_array();
            }
            VelocityMode::Compensate => {
                // d/dt (C·p) = C.rot·v + ω_C × (C.rot·p) + d(C.pos)/dt, with ω_C and d(C.pos)/dt
                // from the rig's motion model. This includes the lever arm from the CoR to the device.
                let ahead = compensation_at(DERIVATIVE_STEP_S);
                let omega_c = (ahead.rot * c.rot.conj()).to_rotation_vector() * (1.0 / DERIVATIVE_STEP_S);
                let trans_rate = (ahead.pos - c.pos) * (1.0 / DERIVATIVE_STEP_S);
                out.vec_velocity = (rotate(pose.vec_velocity) + omega_c.cross(rotated_pos) + trans_rate).to_array();
                out.vec_angular_velocity = (rotate(pose.vec_angular_velocity) + omega_c).to_array();
                // The rig's own accelerations are not modelled (constant-rate prediction).
                out.vec_acceleration = rotate(pose.vec_acceleration).to_array();
                out.vec_angular_acceleration = rotate(pose.vec_angular_acceleration).to_array();
            }
        }

        let finite = out
            .vec_position
            .iter()
            .chain(&out.vec_velocity)
            .chain(&out.vec_angular_velocity)
            .chain(&out.vec_acceleration)
            .chain(&out.vec_angular_acceleration)
            .all(|x| x.is_finite())
            && quat_from(out.q_rotation).is_finite();
        if !finite {
            return Outcome::Skipped(Skip::NonFinite);
        }
        *pose = out;
        if gain < 1.0 {
            Outcome::Fading
        } else {
            Outcome::Compensated
        }
    }

    fn maybe_calibrate(
        &self,
        settings: &Settings,
        pose: &DriverPose,
        world_from_driver: Rigid,
        driver_pose: Rigid,
        rig: Rigid,
        cor_offset: Vec3,
    ) {
        let current = self.calibration();
        let universe_moved = current.is_some_and(|c| {
            let d = c.world_from_driver.inverse().then_after(world_from_driver);
            d.pos.norm() > UNIVERSE_SHIFT_M || d.rot.angle() > UNIVERSE_SHIFT_RAD
        });
        if universe_moved {
            self.calibration.clear();
        }
        let requested = self.recenter.load(Ordering::Acquire);
        let wanted = requested || (self.calibration().is_none() && settings.auto_calibrate);
        if !wanted {
            return;
        }
        let head_from_driver = Rigid::new(
            quat_from(pose.q_driver_from_head_rotation).normalized(),
            Vec3::from_array(pose.vec_driver_from_head_translation),
        );
        let head = world_from_driver.then_after(driver_pose).then_after(head_from_driver);
        let base = calibrate_base(head, rig, cor_offset, settings.cor_yaw_offset_deg.to_radians());
        if !base.is_finite() {
            return;
        }
        if self.calibration.try_store(&Calibration { base, world_from_driver }) {
            if requested {
                self.recenter.store(false, Ordering::Release);
            }
            self.counters.calibrations.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Finds the seat's neutral frame `B` from the head's world pose and the rig pose at that time.
///
/// `cor_offset` is the head-to-CoR vector in seat coordinates (OpenVR axes); `yaw_offset` turns the
/// seat's heading relative to the head's (radians, + left). Assumes the head faces along the seat
/// at calibration.
pub fn calibrate_base(head: Rigid, rig: Rigid, cor_offset: Vec3, yaw_offset: f64) -> Rigid {
    // Heading: solve yaw(R⁻¹·Ry(−ψ)·head) = 0, i.e. the head has no heading relative to the
    // tilted seat. Exact for an untilted rig after the first estimate; a few steps converge for tilts.
    let mut psi = head.rot.yaw() - rig.rot.yaw();
    for _ in 0..6 {
        let relative = rig.rot.conj() * Quat::rot_y(-psi) * head.rot;
        psi += relative.yaw();
    }
    let rot = Quat::rot_y(psi + yaw_offset);
    // The CoR now is head + B.rot·R.rot·offset, and also B.pos + B.rot·R.pos.
    let pos = head.pos + rot.rotate(rig.rot.rotate(cor_offset) - rig.pos);
    Rigid::new(rot, pos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::tests::{assert_quat_near, assert_vec_near};
    use crate::rig::{rig_transform, RigSample, RigValues};
    use std::sync::Mutex;

    /// A rig source the test drives directly.
    struct FakeRig(Mutex<RigReading>);

    impl FakeRig {
        fn new(r: RigReading) -> Self {
            FakeRig(Mutex::new(r))
        }
        fn set(&self, r: RigReading) {
            *self.0.lock().unwrap() = r;
        }
    }

    impl RigSource for FakeRig {
        fn read(&self) -> RigReading {
            *self.0.lock().unwrap()
        }
    }

    const MS: u64 = 1_000_000;

    /// Head 0.7 m above and 0.15 m in front of the CoR.
    const COR: CorOffset = CorOffset { forward: -0.15, down: 0.7, right: 0.0 };

    fn values(pitch: f64, roll: f64, yaw: f64, heave: f64) -> RigValues {
        RigValues { pitch, roll, yaw, heave, ..RigValues::ZERO }
    }

    fn sample_at(t_ms: u64, v: RigValues) -> RigSample {
        RigSample { timestamp_ns: t_ms * MS, values: v, cor_offset: COR, enabled: true, ..RigSample::default() }
    }

    fn live(t_ms: u64, v: RigValues) -> RigReading {
        RigReading::Live(sample_at(t_ms, v))
    }

    /// The true seat frame and a head fixed on the seat, consistent with `COR`.
    fn truth() -> (Rigid, Rigid) {
        let base = Rigid::new(Quat::rot_y(0.8), Vec3::new(1.0, 0.4, -2.0));
        let head_on_seat = Rigid::new(Quat::rot_x(-0.15), Vec3::new(0.0, 0.7, -0.15));
        (base, head_on_seat)
    }

    /// A driver pose for a device at world pose `world`, reported in a driver space `w`.
    fn driver_pose(w: Rigid, world: Rigid) -> DriverPose {
        let d = w.inverse().then_after(world);
        DriverPose {
            q_world_from_driver_rotation: quat_to(w.rot),
            vec_world_from_driver_translation: w.pos.to_array(),
            q_rotation: quat_to(d.rot),
            vec_position: d.pos.to_array(),
            ..DriverPose::default()
        }
    }

    fn world_of(p: &DriverPose) -> Rigid {
        let w = Rigid::new(
            quat_from(p.q_world_from_driver_rotation),
            Vec3::from_array(p.vec_world_from_driver_translation),
        );
        w.then_after(Rigid::new(quat_from(p.q_rotation), Vec3::from_array(p.vec_position)))
    }

    /// The tracked world pose of something fixed on the seat at `on_seat` with the rig at `v`.
    fn riding(v: RigValues, on_seat: Rigid) -> Rigid {
        truth().0.then_after(rig_transform(&v)).then_after(on_seat)
    }

    /// A compensator that has calibrated with the rig at neutral at t = 1000 ms.
    fn calibrated(rig: &FakeRig, settings: Settings) -> Compensator<&FakeRig> {
        let comp = Compensator::new(rig, settings);
        let mut p = driver_pose(Rigid::IDENTITY, riding(RigValues::ZERO, truth().1));
        assert_eq!(comp.process(DeviceKind::Hmd, &mut p, 1000 * MS), Outcome::Compensated);
        comp
    }

    #[test]
    fn calibration_recovers_the_seat_frame_even_off_neutral() {
        let (base, head_on_seat) = truth();
        for v in [RigValues::ZERO, values(6.0, -4.0, 3.0, 20.0), values(-11.0, 9.0, -6.0, -30.0)] {
            let rig = rig_transform(&v);
            let head = base.then_after(rig).then_after(head_on_seat);
            let found = calibrate_base(head, rig, cor_vector(COR), 0.0);
            assert_vec_near(found.pos, base.pos, 1e-9);
            assert_quat_near(found.rot, base.rot, 1e-9);
        }
    }

    #[test]
    fn seat_fixed_head_and_hands_stay_put_while_the_rig_moves() {
        let (base, head_on_seat) = truth();
        // A driver space that differs from world, to exercise the W⁻¹·C·W conversion.
        let w = Rigid::new(Quat::rot_y(-0.3) * Quat::rot_x(0.02), Vec3::new(0.2, -0.1, 0.5));
        let start = values(3.0, 1.0, 0.0, 0.0);
        let rig = FakeRig::new(live(1000, start));
        let comp = Compensator::new(&rig, Settings::default());

        let mut p = driver_pose(w, riding(start, head_on_seat));
        assert_eq!(comp.process(DeviceKind::Hmd, &mut p, 1000 * MS), Outcome::Compensated);
        let neutral_head = base.then_after(head_on_seat);
        assert_vec_near(world_of(&p).pos, neutral_head.pos, 1e-9);

        let hand_on_seat = Rigid::new(Quat::rot_z(0.4), Vec3::new(0.25, 0.35, -0.4));
        for (i, v) in [values(11.0, -6.0, 3.0, 25.0), values(-9.0, 12.0, -6.0, -15.0)].into_iter().enumerate() {
            let t = 2000 + i as u64 * 1000;
            rig.set(live(t, v));
            let mut p = driver_pose(w, riding(v, head_on_seat));
            assert_eq!(comp.process(DeviceKind::Hmd, &mut p, t * MS), Outcome::Compensated);
            let got = world_of(&p);
            assert_vec_near(got.pos, neutral_head.pos, 1e-9);
            assert_quat_near(got.rot, neutral_head.rot, 1e-9);

            let mut c = driver_pose(w, riding(v, hand_on_seat));
            assert_eq!(comp.process(DeviceKind::Controller, &mut c, t * MS), Outcome::Compensated);
            assert_vec_near(world_of(&c).pos, base.then_after(hand_on_seat).pos, 1e-9);
        }
    }

    #[test]
    fn own_head_motion_is_preserved() {
        let (base, head_on_seat) = truth();
        let rig = FakeRig::new(live(1000, RigValues::ZERO));
        let comp = calibrated(&rig, Settings::default());
        let v = values(11.0, 6.0, 0.0, 0.0);
        rig.set(live(1100, v));
        let turned = head_on_seat.then_after(Rigid::new(Quat::rot_y(0.5), Vec3::ZERO));
        let mut p = driver_pose(Rigid::IDENTITY, riding(v, turned));
        comp.process(DeviceKind::Hmd, &mut p, 1100 * MS);
        assert_quat_near(world_of(&p).rot, base.then_after(turned).rot, 1e-9);
    }

    #[test]
    fn rig_velocity_including_lever_arm_is_removed() {
        let (base, head_on_seat) = truth();
        let rig = FakeRig::new(live(1000, RigValues::ZERO));
        let comp = calibrated(&rig, Settings::default());

        // Rig pitching at 57.3°/s (1 rad/s) about the CoR; head rigidly on the seat.
        let v = values(6.0, 0.0, 0.0, 0.0);
        let mut s = sample_at(1100, v);
        s.rates = Some(RigValues { pitch: 1f64.to_degrees(), ..RigValues::ZERO });
        rig.set(RigReading::Live(s));
        let head = riding(v, head_on_seat);
        // The tracked head's true world velocity: ω_world × (head − CoR).
        let omega_world = base.rot.rotate(Vec3::new(1.0, 0.0, 0.0));
        let mut p = driver_pose(Rigid::IDENTITY, head);
        p.vec_velocity = omega_world.cross(head.pos - base.pos).to_array();
        p.vec_angular_velocity = omega_world.to_array();
        comp.process(DeviceKind::Hmd, &mut p, 1100 * MS);
        assert_vec_near(Vec3::from_array(p.vec_velocity), Vec3::ZERO, 1e-3);
        assert_vec_near(Vec3::from_array(p.vec_angular_velocity), Vec3::ZERO, 1e-3);
    }

    #[test]
    fn prediction_extrapolates_to_the_pose_time() {
        let (base, head_on_seat) = truth();
        // Calibrated off neutral, at 5° pitch.
        let rig = FakeRig::new(live(1000, values(5.0, 0.0, 0.0, 0.0)));
        let comp = Compensator::new(&rig, Settings { rate_smoothing_ms: 0.0, ..Settings::default() });
        let mut p = driver_pose(Rigid::IDENTITY, riding(values(5.0, 0.0, 0.0, 0.0), head_on_seat));
        assert_eq!(comp.process(DeviceKind::Hmd, &mut p, 1000 * MS), Outcome::Compensated);

        // Samples 5° and 6° pitch, 10 ms apart: 100°/s. The pose at 1020 ms is at 7°.
        rig.set(live(1010, values(6.0, 0.0, 0.0, 0.0)));
        let mut p = driver_pose(Rigid::IDENTITY, riding(values(7.0, 0.0, 0.0, 0.0), head_on_seat));
        p.pose_time_offset = -0.005; // measured 5 ms before the call at 1025 ms
        comp.process(DeviceKind::Hmd, &mut p, 1025 * MS);
        assert_vec_near(world_of(&p).pos, base.then_after(head_on_seat).pos, 1e-6);
    }

    #[test]
    fn rig_latency_moves_the_prediction() {
        let (base, head_on_seat) = truth();
        let mut s = sample_at(1000, RigValues::ZERO);
        s.rates = Some(RigValues::ZERO);
        let rig = FakeRig::new(RigReading::Live(s));
        let settings = Settings { rig_latency_ms: 20.0, ..Settings::default() };
        let comp = calibrated(&rig, settings);
        // Sample at 2000 ms says 4° at 100°/s, measured 20 ms before its timestamp: 6° at 2000 ms.
        let mut s = sample_at(2000, values(4.0, 0.0, 0.0, 0.0));
        s.rates = Some(RigValues { pitch: 100.0, ..RigValues::ZERO });
        rig.set(RigReading::Live(s));
        let mut p = driver_pose(Rigid::IDENTITY, riding(values(6.0, 0.0, 0.0, 0.0), head_on_seat));
        comp.process(DeviceKind::Hmd, &mut p, 2000 * MS);
        assert_vec_near(world_of(&p).pos, base.then_after(head_on_seat).pos, 1e-6);
    }

    #[test]
    fn skips_leave_the_pose_untouched() {
        let (_, head_on_seat) = truth();
        let rig = FakeRig::new(live(1000, RigValues::ZERO));
        let comp = calibrated(&rig, Settings::default());
        let original = driver_pose(Rigid::IDENTITY, riding(values(11.0, 0.0, 0.0, 0.0), head_on_seat));

        let disabled = RigSample { enabled: false, ..sample_at(1000, values(11.0, 0.0, 0.0, 0.0)) };
        let never = RigSample { timestamp_ns: 0, ..disabled };
        let nan = sample_at(1000, values(f64::NAN, 0.0, 0.0, 0.0));
        for (reading, skip) in [
            (RigReading::ClaimedByOther, Skip::ClaimedByOther),
            (RigReading::Absent, Skip::NoRigPose),
            (RigReading::Live(disabled), Skip::RigDisabled),
            (RigReading::Live(never), Skip::NoRigPose),
            (RigReading::Live(nan), Skip::NonFinite),
        ] {
            rig.set(reading);
            let mut p = original;
            assert_eq!(comp.process(DeviceKind::Hmd, &mut p, 1010 * MS), Outcome::Skipped(skip));
            assert_eq!(p, original, "{skip:?} must leave the pose untouched");
            assert_eq!(Skip::from_code(comp.counters.last_hmd_skip.load(Ordering::Relaxed)), Some(skip));
        }
    }

    #[test]
    fn stale_samples_fade_out() {
        let (base, head_on_seat) = truth();
        let rig = FakeRig::new(live(1000, RigValues::ZERO));
        let comp = calibrated(&rig, Settings::default());
        let original = driver_pose(Rigid::IDENTITY, riding(values(11.0, 0.0, 0.0, 0.0), head_on_seat));
        rig.set(live(1000, values(11.0, 0.0, 0.0, 0.0)));
        // 250 ms stale threshold + 500 ms fade: half strength at 1500 ms, off after 1750 ms.
        let mut p = original;
        assert_eq!(comp.process(DeviceKind::Hmd, &mut p, 1500 * MS), Outcome::Fading);
        let full = base.then_after(head_on_seat).pos;
        let got = world_of(&p).pos;
        assert!((got - full).norm() > 1e-3 && (got - world_of(&original).pos).norm() > 1e-3, "partial");
        let mut p = original;
        assert_eq!(comp.process(DeviceKind::Hmd, &mut p, 1800 * MS), Outcome::Skipped(Skip::Stale));
        assert_eq!(p, original);
    }

    #[test]
    fn device_selection_and_invalid_poses() {
        let rig = FakeRig::new(live(1000, RigValues::ZERO));
        let comp = Compensator::new(&rig, Settings::default());
        let mut p = DriverPose::default();
        assert_eq!(comp.process(DeviceKind::Other, &mut p, 1000 * MS), Outcome::Skipped(Skip::NotSelected));
        assert_eq!(comp.process(DeviceKind::Tracker, &mut p, 1000 * MS), Outcome::Skipped(Skip::NotSelected));
        // Controllers before any headset pose: nothing to calibrate from yet.
        assert_eq!(comp.process(DeviceKind::Controller, &mut p, 1000 * MS), Outcome::Skipped(Skip::Uncalibrated));
        p.pose_is_valid = false;
        assert_eq!(comp.process(DeviceKind::Hmd, &mut p, 1000 * MS), Outcome::Skipped(Skip::InvalidPose));
        comp.set_settings(Settings { enabled: false, ..Settings::default() });
        assert_eq!(comp.process(DeviceKind::Hmd, &mut p, 1000 * MS), Outcome::Skipped(Skip::SwitchedOff));
    }

    #[test]
    fn recenter_requests_generation_and_universe_change() {
        let (_, head_on_seat) = truth();
        let rig = FakeRig::new(live(1000, RigValues::ZERO));
        let comp = Compensator::new(&rig, Settings { auto_calibrate: false, ..Settings::default() });
        let mut p = driver_pose(Rigid::IDENTITY, riding(RigValues::ZERO, head_on_seat));
        assert_eq!(comp.process(DeviceKind::Hmd, &mut p, 1000 * MS), Outcome::Skipped(Skip::Uncalibrated));

        comp.request_recenter();
        let mut p = driver_pose(Rigid::IDENTITY, riding(RigValues::ZERO, head_on_seat));
        assert_eq!(comp.process(DeviceKind::Hmd, &mut p, 1000 * MS), Outcome::Compensated);
        assert_eq!(comp.counters.calibrations.load(Ordering::Relaxed), 1);
        assert!(!comp.recenter_pending());

        // The user moves and the daemon bumps the recentre generation: the new head pose defines the seat.
        let moved = Rigid::new(Quat::rot_y(-0.4), Vec3::new(-1.0, 0.4, 0.5));
        rig.set(RigReading::Live(RigSample { recenter_generation: 1, ..sample_at(1000, RigValues::ZERO) }));
        let mut p = driver_pose(Rigid::IDENTITY, moved.then_after(head_on_seat));
        comp.process(DeviceKind::Hmd, &mut p, 1000 * MS);
        assert_vec_near(comp.calibration().unwrap().base.pos, moved.pos, 1e-9);
        assert_eq!(comp.counters.calibrations.load(Ordering::Relaxed), 2);

        // Tracking universe moves (room setup re-run): calibration dropped, no auto-calibrate.
        let w = Rigid::new(Quat::IDENTITY, Vec3::new(0.5, 0.0, 0.0));
        let mut p = driver_pose(w, moved.then_after(head_on_seat));
        assert_eq!(comp.process(DeviceKind::Hmd, &mut p, 1000 * MS), Outcome::Skipped(Skip::Uncalibrated));
    }

    #[test]
    fn config_cor_overrides_the_file() {
        let (_, head_on_seat) = truth();
        let rig = FakeRig::new(live(1000, RigValues::ZERO));
        let settings = Settings { cor_source: CorSource::Config, cor_down_m: 0.2, ..Settings::default() };
        let comp = Compensator::new(&rig, settings);
        let head = riding(RigValues::ZERO, head_on_seat);
        let mut p = driver_pose(Rigid::IDENTITY, head);
        comp.process(DeviceKind::Hmd, &mut p, 1000 * MS);
        assert_vec_near(comp.calibration().unwrap().base.pos, head.pos - Vec3::new(0.0, 0.2, 0.0), 1e-9);
    }

    #[test]
    fn velocity_zero_mode() {
        let (_, head_on_seat) = truth();
        let rig = FakeRig::new(live(1000, RigValues::ZERO));
        let comp = Compensator::new(&rig, Settings { velocity: VelocityMode::Zero, ..Settings::default() });
        let mut p = driver_pose(Rigid::IDENTITY, riding(RigValues::ZERO, head_on_seat));
        p.vec_velocity = [1.0, 2.0, 3.0];
        p.vec_angular_acceleration = [1.0, 0.0, 0.0];
        comp.process(DeviceKind::Hmd, &mut p, 1000 * MS);
        assert_eq!(p.vec_velocity, [0.0; 3]);
        assert_eq!(p.vec_angular_acceleration, [0.0; 3]);
    }
}
