//! The compensation core, free of FFI: calibration of the centre of rotation, the per-time
//! compensation transform, its cache, and the pass-through rules.
//!
//! Notation (all poses are `parent_from_local`):
//! - `T` is the layer's own tracking space (STAGE, or LOCAL where STAGE is missing).
//! - `C` = `T_from_neutralRig`: the neutral rig frame, gravity-aligned, origin at the centre of
//!   rotation, found at calibration ([`CorFrame`]).
//! - `P(t)` = `neutralRig_from_rig`: the rig pose from the pose file ([`RigValues::transform`]).
//! - The rig moves everything attached to it by `C·P·C⁻¹` in `T`, so the compensation is
//!   `K(t) = C·P(t)⁻¹·C⁻¹`, left-multiplied onto any rig-attached pose expressed in `T`.
//! - For an application space `B` with `X_B = T_from_B`, the same compensation expressed in `B` is
//!   `X_B⁻¹·K·X_B` ([`Motion::conjugated_by`]).

use std::collections::HashMap;
use std::path::PathBuf;

use openrigcue_rigpose::math::{Pose, Quat, Vec3};
use openrigcue_rigpose::{
    ClaimOutcome, ConsumerClaim, ConsumerKind, CorOffset, RigPoseError, RigPoseReader, RigSample, RigValues, Unusable,
};

use crate::config::Settings;
use crate::log;
use crate::motion::Motion;

/// Step used to differentiate the predicted rig pose, seconds.
const DIFF_STEP_S: f64 = 1e-3;

/// The heading (rotation about +y) of an orientation's forward axis, radians, such that
/// `Quat::rot_y(heading(q))` faces the same way. Looking straight up or down falls back to the up axis.
pub fn heading(q: Quat) -> f64 {
    let f = q.rotate(Vec3::new(0.0, 0.0, -1.0));
    let dir = if f.x.hypot(f.z) > 0.1 {
        f
    } else {
        // Looking down, the head's up axis points forward; looking up, it points backward.
        let u = q.rotate(Vec3::new(0.0, 1.0, 0.0));
        if f.y < 0.0 {
            u
        } else {
            -u
        }
    };
    (-dir.x).atan2(-dir.z)
}

/// The calibrated neutral rig frame `C` in the tracking space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CorFrame {
    pub tracking_from_rig: Pose,
}

impl CorFrame {
    /// Calibrates from the head pose `head` (in `T`) while the rig is at `rig` (which need not be
    /// neutral), assuming the user faces the rig's forward direction.
    ///
    /// The rig-local frame now is `C·P`; its orientation is the head's heading with the rig's tilt,
    /// `Ry(heading(R_head · R_tilt⁻¹)) · R_tilt`, and its origin is `offset` away from the head in
    /// that frame. `C` follows by removing `P`; it comes out yaw-only (gravity-aligned).
    pub fn calibrate(head: Pose, rig: &RigValues, offset: CorOffset) -> CorFrame {
        let tilt = rig.tilt_only().rotation();
        let head_heading = heading(head.rotation * tilt.conjugate());
        let rig_frame_rotation = Quat::rot_y(head_heading) * tilt;
        let cor_now = head.translation + rig_frame_rotation.rotate(offset.to_vec3());
        let p = rig.transform();
        let c_rotation = Quat::rot_y(head_heading - rig.yaw.to_radians());
        let c_translation = cor_now - c_rotation.rotate(p.translation);
        CorFrame { tracking_from_rig: Pose::new(c_rotation, c_translation) }
    }

    /// `K = C·P⁻¹·C⁻¹` with its time derivative, for a rig motion `rig`.
    pub fn compensation(&self, rig: &Motion) -> Motion {
        let c = Motion::fixed(self.tracking_from_rig);
        c.then(&rig.inverse()).then(&c.inverse())
    }
}

/// The predicted rig motion (pose and derivative) for `sample` at `target_ns`.
pub fn rig_motion(sample: &RigSample, target_ns: u64, max_horizon_ns: u64) -> Motion {
    let dt = sample.horizon_s(target_ns, max_horizon_ns);
    let a = sample.extrapolated(dt).transform();
    if sample.rates.is_none() {
        return Motion::fixed(a);
    }
    let b = sample.extrapolated(dt + DIFF_STEP_S).transform();
    Motion::from_poses(a, b, DIFF_STEP_S)
}

/// Where rig samples come from, and the consumer claim.
pub trait PoseSource {
    /// The latest sample, or `None` if the file is missing or unreadable right now.
    fn read(&mut self, now_ns: u64) -> Option<RigSample>;
    /// Takes or refreshes the active-consumer claim for the OpenXR layer.
    fn claim(&mut self, now_ns: u64) -> Result<(), ConsumerClaim>;
    /// Gives the claim up.
    fn release(&mut self);
}

/// [`PoseSource`] backed by the rig-pose file; reopens it when it appears or seems replaced.
#[derive(Debug)]
pub struct FileSource {
    path: PathBuf,
    max_age_ns: u64,
    reader: Option<RigPoseReader>,
    next_open_ns: u64,
    last_fresh_ns: u64,
}

impl FileSource {
    const RETRY_NS: u64 = 1_000_000_000;

    pub fn new(path: PathBuf, max_age_ns: u64) -> Self {
        FileSource { path, max_age_ns, reader: None, next_open_ns: 0, last_fresh_ns: 0 }
    }

    fn reader(&mut self, now_ns: u64) -> Option<&RigPoseReader> {
        if self.reader.is_none() && now_ns >= self.next_open_ns {
            self.next_open_ns = now_ns.saturating_add(Self::RETRY_NS);
            match RigPoseReader::open(&self.path) {
                Ok(r) => {
                    log::info!("opened rig pose {}", self.path.display());
                    self.last_fresh_ns = now_ns;
                    self.reader = Some(r);
                }
                Err(RigPoseError::Missing(_)) => {}
                Err(e) => log::debug!("{e}"),
            }
        }
        self.reader.as_ref()
    }
}

impl PoseSource for FileSource {
    fn read(&mut self, now_ns: u64) -> Option<RigSample> {
        let max_age_ns = self.max_age_ns;
        let result = self.reader(now_ns)?.read();
        match result {
            Ok(snap) => {
                if snap.sample.age_ns(now_ns) <= max_age_ns {
                    self.last_fresh_ns = now_ns;
                } else if now_ns.saturating_sub(self.last_fresh_ns) > Self::RETRY_NS {
                    // Nothing new for a while: the daemon may have recreated the file, so look again.
                    self.reader = None;
                    self.last_fresh_ns = now_ns;
                }
                Some(snap.sample)
            }
            Err(RigPoseError::Busy) => None,
            Err(e) => {
                log::info!("rig pose read failed, reopening: {e}");
                self.reader = None;
                None
            }
        }
    }

    fn claim(&mut self, now_ns: u64) -> Result<(), ConsumerClaim> {
        match self.reader(now_ns).map(|r| r.claim(ConsumerKind::XR_LAYER, now_ns)) {
            Some(Ok(ClaimOutcome::Yielded(holder))) => Err(holder),
            // Granted, or the claim cannot be read: nothing says another consumer is active.
            _ => Ok(()),
        }
    }

    fn release(&mut self) {
        if let Some(r) = &self.reader {
            let _ = r.release(ConsumerKind::XR_LAYER);
        }
    }
}

/// Why the layer is (not) compensating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// No readable rig-pose file.
    NoPose,
    Unusable(Unusable),
    /// Another consumer (the SteamVR driver) compensates.
    Yielded(ConsumerKind),
    /// Waiting for a calibration (focus, or a re-centre from the daemon).
    Uncalibrated,
    Active,
}

/// What made a calibration necessary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    /// First calibration of the session, taken once the session is focused.
    Auto,
    /// The daemon's re-centre generation changed.
    Recenter,
    /// The tracking space was re-centred by the runtime; calibrate from frames at or after this time.
    SpaceChange { after: i64 },
}

/// A fixed-capacity ring of compensations keyed by `XrTime`.
#[derive(Debug)]
pub struct DeltaRing {
    entries: Vec<(i64, Motion)>,
    capacity: usize,
    next: usize,
}

impl DeltaRing {
    pub fn new(capacity: usize) -> Self {
        DeltaRing { entries: Vec::with_capacity(capacity), capacity: capacity.max(1), next: 0 }
    }

    pub fn exact(&self, time: i64) -> Option<Motion> {
        self.entries.iter().find(|(t, _)| *t == time).map(|(_, m)| *m)
    }

    /// The entry closest to `time` within `tolerance_ns`.
    pub fn nearest(&self, time: i64, tolerance_ns: u64) -> Option<Motion> {
        self.entries
            .iter()
            .map(|(t, m)| (t.abs_diff(time), m))
            .filter(|(d, _)| *d <= tolerance_ns)
            .min_by_key(|(d, _)| *d)
            .map(|(_, m)| *m)
    }

    /// Records what was applied for `time`: replaces an existing entry for `time` unless that would
    /// replace a real compensation by the identity (several locates for one frame, of which only
    /// some could be compensated, still rendered compensated views).
    pub fn record(&mut self, time: i64, m: Motion) {
        if let Some(e) = self.entries.iter_mut().find(|(t, _)| *t == time) {
            if !m.is_identity() || e.1.is_identity() {
                e.1 = m;
            }
        } else {
            self.insert(time, m);
        }
    }

    pub fn insert(&mut self, time: i64, m: Motion) {
        if self.entries.len() < self.capacity {
            self.entries.push((time, m));
        } else {
            self.entries[self.next] = (time, m);
            self.next = (self.next + 1) % self.capacity;
        }
    }
}

/// Compensation per `XrTime`, so every call for one frame (views, spaces, the submitted layers) uses
/// the same transform.
///
/// Transforms for predicted display times (the frames `xrEndFrame` will revert) live in their own
/// ring, so an application that locates spaces at many other times (physics, input) cannot evict them.
#[derive(Debug)]
pub struct DeltaCache {
    frames: DeltaRing,
    other: DeltaRing,
    display_times: DeltaRing,
}

impl Default for DeltaCache {
    fn default() -> Self {
        DeltaCache { frames: DeltaRing::new(16), other: DeltaRing::new(32), display_times: DeltaRing::new(16) }
    }
}

impl DeltaCache {
    /// Records a predicted display time from `xrWaitFrame`.
    pub fn note_display_time(&mut self, time: i64) {
        if self.display_times.exact(time).is_none() {
            self.display_times.insert(time, Motion::IDENTITY);
        }
    }

    pub fn exact(&self, time: i64) -> Option<Motion> {
        self.frames.exact(time).or_else(|| self.other.exact(time))
    }

    /// The entry closest to `time` within `tolerance_ns`, preferring frame entries.
    pub fn nearest(&self, time: i64, tolerance_ns: u64) -> Option<Motion> {
        self.frames.nearest(time, tolerance_ns).or_else(|| self.other.nearest(time, tolerance_ns))
    }

    pub fn insert(&mut self, time: i64, m: Motion) {
        if self.display_times.exact(time).is_some() {
            self.frames.insert(time, m);
        } else {
            self.other.insert(time, m);
        }
    }

    /// Like [`Self::insert`], with the replacement rule of [`DeltaRing::record`].
    pub fn record(&mut self, time: i64, m: Motion) {
        if self.display_times.exact(time).is_some() {
            self.frames.record(time, m);
        } else {
            self.other.record(time, m);
        }
    }
}

/// Cached `T_from_space` of room-fixed spaces, valid from the last reference-space change on.
#[derive(Debug, Default)]
struct Offsets {
    poses: HashMap<u64, Pose>,
    /// Locations at earlier times describe the spaces before the last change and are not cached.
    valid_from: i64,
}

/// Per-session compensation state.
#[derive(Debug)]
pub struct Compensator<S: PoseSource> {
    source: S,
    settings: Settings,
    calibration: Option<CorFrame>,
    pending: Option<Pending>,
    focused: bool,
    generation: Option<u32>,
    claimed: bool,
    status: Status,
    cache: DeltaCache,
    /// The compensation actually applied to the head (views, or the VIEW space) per `XrTime`; the
    /// identity where it was located uncompensated. `xrEndFrame` reverts exactly this.
    applied: DeltaCache,
    offsets: Offsets,
}

impl<S: PoseSource> Compensator<S> {
    pub fn new(source: S, settings: Settings) -> Self {
        Compensator {
            source,
            settings,
            calibration: None,
            pending: None,
            focused: false,
            generation: None,
            claimed: false,
            status: Status::NoPose,
            cache: DeltaCache::default(),
            applied: DeltaCache::default(),
            offsets: Offsets { poses: HashMap::new(), valid_from: i64::MIN },
        }
    }

    pub fn status(&self) -> Status {
        self.status
    }

    pub fn calibration(&self) -> Option<CorFrame> {
        self.calibration
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    fn set_status(&mut self, s: Status) {
        if s != self.status {
            let old = std::mem::replace(&mut self.status, s);
            log::info!("compensation {old:?} -> {s:?}");
        }
    }

    fn usable_sample(&mut self, now_ns: u64) -> Result<RigSample, Status> {
        let sample = self.source.read(now_ns).ok_or(Status::NoPose)?;
        sample.check_usable(now_ns, self.settings.max_age_ns).map_err(Status::Unusable)?;
        Ok(sample)
    }

    /// Once per frame (after `xrWaitFrame`): reads the pose file, applies the pass-through rules,
    /// refreshes the consumer claim and tracks re-centre requests. Returns true when the caller
    /// should locate the head at `display_time` and call [`Self::calibrate`].
    pub fn on_frame(&mut self, now_ns: u64, display_time: i64) -> bool {
        self.cache.note_display_time(display_time);
        self.applied.note_display_time(display_time);
        let sample = match self.usable_sample(now_ns) {
            Ok(s) => s,
            Err(status) => {
                self.release_claim();
                self.set_status(status);
                return false;
            }
        };
        match self.generation {
            None => {
                if self.calibration.is_none() && self.settings.auto_calibrate {
                    self.pending.get_or_insert(Pending::Auto);
                }
            }
            Some(g) if g != sample.recenter_generation => {
                log::info!("re-centre requested (generation {g} -> {})", sample.recenter_generation);
                self.pending = Some(Pending::Recenter);
            }
            Some(_) => {}
        }
        self.generation = Some(sample.recenter_generation);

        if let Err(holder) = self.source.claim(now_ns) {
            self.claimed = false;
            self.set_status(Status::Yielded(holder.kind));
            return false;
        }
        self.claimed = true;
        self.set_status(if self.calibration.is_some() { Status::Active } else { Status::Uncalibrated });
        match self.pending {
            Some(Pending::Auto) => self.focused,
            Some(Pending::Recenter) => true,
            Some(Pending::SpaceChange { after }) => display_time >= after,
            None => false,
        }
    }

    /// Calibrates the centre of rotation from the head pose `head` in the tracking space at
    /// `display_ns` (monotonic). Returns false (and keeps the request) if no usable sample exists.
    pub fn calibrate(&mut self, head: Pose, display_ns: u64, now_ns: u64) -> bool {
        let Ok(sample) = self.usable_sample(now_ns) else {
            return false;
        };
        let rig = sample.predict(display_ns, self.settings.max_prediction_ns);
        let cor = CorFrame::calibrate(head, &rig, sample.cor_offset);
        log::info!(
            "calibrated: centre of rotation at {:?}, heading {:.1}°, rig {:?}",
            cor.tracking_from_rig.translation,
            heading(cor.tracking_from_rig.rotation).to_degrees(),
            rig
        );
        self.calibration = Some(cor);
        self.pending = None;
        if self.claimed {
            self.set_status(Status::Active);
        }
        true
    }

    /// The compensation `K` in the tracking space for `time` (XrTime) = `time_ns` (monotonic),
    /// cached per `time`. The identity while passing through.
    pub fn delta(&mut self, time: i64, time_ns: u64, now_ns: u64) -> Motion {
        if let Some(m) = self.cache.exact(time) {
            return m;
        }
        let m = self.compute(time_ns, now_ns);
        self.cache.insert(time, m);
        m
    }

    /// Records the compensation `k` (tracking space) that was applied to the head located for
    /// `time`, or the identity when the head was located uncompensated.
    pub fn record_applied(&mut self, time: i64, k: Motion) {
        self.applied.record(time, k);
    }

    /// The compensation to revert for layers submitted for `time`: what was applied to the head
    /// located for that frame (exactly, or the nearest located frame within the submit tolerance;
    /// the identity if that frame was located uncompensated). Only when no head location is
    /// recorded near `time` is a fresh transform computed (for world-locked layers of an
    /// application that never locates the head).
    pub fn delta_for_submit(&mut self, time: i64, time_ns: u64, now_ns: u64) -> Motion {
        if let Some(m) =
            self.applied.exact(time).or_else(|| self.applied.nearest(time, self.settings.submit_tolerance_ns))
        {
            return m;
        }
        self.compute(time_ns, now_ns)
    }

    fn compute(&mut self, time_ns: u64, now_ns: u64) -> Motion {
        let (Status::Active, Some(cor)) = (self.status, self.calibration) else {
            return Motion::IDENTITY;
        };
        match self.usable_sample(now_ns) {
            Ok(sample) => cor.compensation(&rig_motion(&sample, time_ns, self.settings.max_prediction_ns)),
            Err(_) => Motion::IDENTITY,
        }
    }

    /// The cached `T_from_space` of a room-fixed space, if it is valid at `time`.
    pub fn offset(&self, space: u64, time: i64) -> Option<Pose> {
        if time < self.offsets.valid_from {
            return None;
        }
        self.offsets.poses.get(&space).copied()
    }

    /// Caches `T_from_space` as located at `time`; locations from before the last
    /// reference-space change are not kept.
    pub fn store_offset(&mut self, space: u64, pose: Pose, time: i64) {
        if time >= self.offsets.valid_from {
            self.offsets.poses.insert(space, pose);
        }
    }

    pub fn forget_space(&mut self, space: u64) {
        self.offsets.poses.remove(&space);
    }

    pub fn on_focus(&mut self, focused: bool) {
        self.focused = focused;
    }

    /// A reference space of the session is about to change at `change_time`. Cached space offsets
    /// are dropped; if it is the tracking space itself, the calibration is invalid and is taken again
    /// (automatically if auto-calibration is on, otherwise at the next re-centre).
    pub fn on_reference_space_change(&mut self, tracking_space_changed: bool, change_time: i64) {
        self.offsets.poses.clear();
        self.offsets.valid_from = self.offsets.valid_from.max(change_time);
        if tracking_space_changed {
            log::info!("tracking space re-centred: calibration discarded");
            self.calibration = None;
            self.pending = self.settings.auto_calibrate.then_some(Pending::SpaceChange { after: change_time });
            if self.status == Status::Active {
                self.set_status(Status::Uncalibrated);
            }
        }
    }

    fn release_claim(&mut self) {
        if self.claimed {
            self.source.release();
            self.claimed = false;
        }
    }

    /// Ends the session: gives the consumer claim up.
    pub fn shutdown(&mut self) {
        self.release_claim();
        self.set_status(Status::NoPose);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrigcue_rigpose::RigValues;

    #[derive(Debug, Default)]
    struct FakeSource {
        sample: Option<RigSample>,
        holder: Option<ConsumerClaim>,
        released: u32,
    }

    impl PoseSource for FakeSource {
        fn read(&mut self, _now_ns: u64) -> Option<RigSample> {
            self.sample
        }
        fn claim(&mut self, _now_ns: u64) -> Result<(), ConsumerClaim> {
            self.holder.map_or(Ok(()), Err)
        }
        fn release(&mut self) {
            self.released += 1;
        }
    }

    const NOW: u64 = 10_000_000_000;

    fn sample(values: RigValues, offset: CorOffset) -> RigSample {
        RigSample { timestamp_ns: NOW, values, rates: None, cor_offset: offset, enabled: true, recenter_generation: 0 }
    }

    /// A rig whose neutral frame is yawed and placed in the room, the head 0.6 m above and 0.1 m
    /// behind... the CoR, i.e. the CoR is 0.6 m down and 0.1 m forward of the eyes.
    fn scene() -> (CorFrame, Pose, CorOffset) {
        let offset = CorOffset { forward: 0.1, down: 0.6, right: 0.02 };
        let c = Pose::new(Quat::rot_y(0.5), Vec3::new(0.4, 0.5, -1.2));
        // The head in the neutral rig frame is at −offset, facing the rig's forward.
        let head_in_rig = Pose::new(Quat::IDENTITY, -offset.to_vec3());
        (CorFrame { tracking_from_rig: c }, c.then(head_in_rig), offset)
    }

    /// The head as tracked when the rig is at `rig` and the head is at `head_true` relative to the cockpit.
    fn tracked(cor: CorFrame, rig: &RigValues, head_true: Pose) -> Pose {
        let c = cor.tracking_from_rig;
        c.then(rig.transform()).then(c.inverse()).then(head_true)
    }

    fn settled(rig: RigValues, offset: CorOffset) -> Compensator<FakeSource> {
        let src = FakeSource { sample: Some(sample(rig, offset)), ..FakeSource::default() };
        let mut c = Compensator::new(src, Settings::default());
        c.on_focus(true);
        c
    }

    #[test]
    fn heading_of_basic_orientations() {
        assert!((heading(Quat::rot_y(0.7)) - 0.7).abs() < 1e-12);
        assert!((heading(Quat::rot_y(-2.0) * Quat::rot_x(0.3)) + 2.0).abs() < 1e-12);
        // Straight down or up still gives the heading.
        assert!((heading(Quat::rot_y(1.0) * Quat::rot_x(-std::f64::consts::FRAC_PI_2)) - 1.0).abs() < 1e-9);
        assert!((heading(Quat::rot_y(1.0) * Quat::rot_x(std::f64::consts::FRAC_PI_2)) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn calibration_at_neutral_recovers_the_rig_frame() {
        let (truth, head, offset) = scene();
        let cor = CorFrame::calibrate(head, &RigValues::ZERO, offset);
        assert!(cor.tracking_from_rig.distance(truth.tracking_from_rig) < 1e-12, "{cor:?}");
    }

    #[test]
    fn calibration_off_neutral_recovers_the_rig_frame() {
        let (truth, head, offset) = scene();
        let rig = RigValues { sway: 12.0, surge: -30.0, heave: 25.0, yaw: 4.0, roll: -6.0, pitch: 8.0 };
        let cor = CorFrame::calibrate(tracked(truth, &rig, head), &rig, offset);
        assert!(cor.tracking_from_rig.distance(truth.tracking_from_rig) < 1e-12, "{cor:?}");
        // C is gravity-aligned.
        let up = cor.tracking_from_rig.rotation.rotate(Vec3::new(0.0, 1.0, 0.0));
        assert!((up - Vec3::new(0.0, 1.0, 0.0)).length() < 1e-12);
    }

    #[test]
    fn compensation_removes_the_rig_motion_about_the_cor() {
        let (truth, head, _) = scene();
        let rig = RigValues { sway: -8.0, surge: 15.0, heave: 40.0, yaw: -3.0, roll: 7.0, pitch: -5.0 };
        // The user also turned their head relative to the cockpit.
        let head_true = head.then(Pose::new(Quat::rot_y(0.3) * Quat::rot_x(-0.1), Vec3::new(0.05, -0.02, 0.03)));
        let k = truth.compensation(&Motion::fixed(rig.transform()));
        let compensated = k.pose.then(tracked(truth, &rig, head_true));
        assert!(compensated.distance(head_true) < 1e-12);
        // A point at the CoR does not move under pure rotation.
        let pure_rot = RigValues { pitch: 10.0, roll: 5.0, ..RigValues::ZERO };
        let moved = truth
            .compensation(&Motion::fixed(pure_rot.transform()))
            .pose
            .transform_point(truth.tracking_from_rig.translation);
        assert!((moved - truth.tracking_from_rig.translation).length() < 1e-12);
    }

    #[test]
    fn compensation_velocity_cancels_the_rig_induced_head_velocity() {
        let (truth, head, offset) = scene();
        let rates = RigValues { sway: 20.0, surge: -10.0, heave: 50.0, yaw: 3.0, roll: -20.0, pitch: 30.0 };
        let s = RigSample { rates: Some(rates), ..sample(RigValues { pitch: 2.0, ..RigValues::ZERO }, offset) };
        let h = 1e-4;
        let head_at = |dt: f64| tracked(truth, &s.extrapolated(dt), head);
        // The head moves only because of the rig: compensated head velocity must be ~0.
        let tracked_motion = Motion::from_poses(head_at(0.0), head_at(h), h);
        let k = truth.compensation(&rig_motion(&s, NOW, 50_000_000));
        let comp = k.then(&tracked_motion);
        assert!(comp.pose.distance(head) < 1e-12);
        assert!(comp.linear.length() < 1e-3, "{:?}", comp.linear);
        assert!(comp.angular.length() < 1e-3, "{:?}", comp.angular);
    }

    #[test]
    fn auto_calibration_waits_for_focus_then_compensates() {
        let (truth, head, offset) = scene();
        let rig = RigValues { pitch: 6.0, roll: 3.0, ..RigValues::ZERO };
        let mut c = settled(rig, offset);
        c.on_focus(false);
        assert!(!c.on_frame(NOW, 1));
        assert_eq!(c.status(), Status::Uncalibrated);
        assert!(c.delta(1, NOW, NOW).is_identity());
        c.on_focus(true);
        assert!(c.on_frame(NOW, 2));
        assert!(c.calibrate(tracked(truth, &rig, head), NOW, NOW));
        assert_eq!(c.status(), Status::Active);
        assert!(!c.on_frame(NOW, 3), "no further calibration");
        let k = c.delta(3, NOW, NOW);
        assert!(k.pose.then(tracked(truth, &rig, head)).distance(head) < 1e-12);
    }

    #[test]
    fn no_auto_calibration_waits_for_a_recenter() {
        let (truth, head, offset) = scene();
        let src = FakeSource { sample: Some(sample(RigValues::ZERO, offset)), ..FakeSource::default() };
        let mut c = Compensator::new(src, Settings { auto_calibrate: false, ..Settings::default() });
        c.on_focus(true);
        assert!(!c.on_frame(NOW, 1));
        c.source.sample.as_mut().unwrap().recenter_generation = 1;
        assert!(c.on_frame(NOW, 2));
        assert!(c.calibrate(tracked(truth, &RigValues::ZERO, head), NOW, NOW));
        assert_eq!(c.status(), Status::Active);
    }

    #[test]
    fn recenter_generation_triggers_recalibration() {
        let (truth, head, offset) = scene();
        let mut c = settled(RigValues::ZERO, offset);
        assert!(c.on_frame(NOW, 1));
        // A bad first calibration: the user was looking 40° to the side.
        assert!(c.calibrate(head.then(Pose::new(Quat::rot_y(0.7), Vec3::ZERO)), NOW, NOW));
        let bad = c.calibration().unwrap();
        assert!(bad.tracking_from_rig.distance(truth.tracking_from_rig) > 0.1);
        assert!(!c.on_frame(NOW, 2));
        c.source.sample.as_mut().unwrap().recenter_generation = 5;
        assert!(c.on_frame(NOW, 3));
        assert!(c.calibrate(head, NOW, NOW));
        assert!(c.calibration().unwrap().tracking_from_rig.distance(truth.tracking_from_rig) < 1e-12);
        assert!(!c.on_frame(NOW, 4));
    }

    #[test]
    fn pass_through_when_missing_stale_disabled_or_yielded() {
        let (truth, head, offset) = scene();
        let rig = RigValues { pitch: 6.0, ..RigValues::ZERO };
        let mut c = settled(rig, offset);
        assert!(c.on_frame(NOW, 1));
        c.calibrate(tracked(truth, &rig, head), NOW, NOW);
        assert!(!c.delta(1, NOW, NOW).is_identity());

        // Stale: 101 ms old.
        let later = NOW + 101_000_000;
        c.on_frame(later, 2);
        assert_eq!(c.status(), Status::Unusable(Unusable::Stale { age_ns: 101_000_000 }));
        assert!(c.delta(2, later, later).is_identity());
        assert_eq!(c.source.released, 1, "claim released while passing through");

        // Disabled.
        c.source.sample = Some(RigSample { enabled: false, ..sample(rig, offset) });
        c.on_frame(NOW, 3);
        assert_eq!(c.status(), Status::Unusable(Unusable::Disabled));
        assert!(c.delta(3, NOW, NOW).is_identity());

        // Missing.
        c.source.sample = None;
        c.on_frame(NOW, 4);
        assert_eq!(c.status(), Status::NoPose);
        assert!(c.delta(4, NOW, NOW).is_identity());

        // The SteamVR driver holds the claim.
        c.source.sample = Some(sample(rig, offset));
        c.source.holder = Some(ConsumerClaim { kind: ConsumerKind::STEAMVR_DRIVER, pid: 1, heartbeat_ns: NOW });
        c.on_frame(NOW, 5);
        assert_eq!(c.status(), Status::Yielded(ConsumerKind::STEAMVR_DRIVER));
        assert!(c.delta(5, NOW, NOW).is_identity());

        // Back to normal: the calibration survived.
        c.source.holder = None;
        assert!(!c.on_frame(NOW, 6));
        assert_eq!(c.status(), Status::Active);
        assert!(!c.delta(6, NOW, NOW).is_identity());
    }

    #[test]
    fn deltas_are_cached_per_time_and_reused_for_submission() {
        let (truth, head, offset) = scene();
        let rig = RigValues { roll: 4.0, ..RigValues::ZERO };
        let mut c = settled(rig, offset);
        c.on_frame(NOW, 100);
        c.calibrate(tracked(truth, &rig, head), NOW, NOW);
        let first = c.delta(100, NOW, NOW);
        c.record_applied(100, first);
        // The rig moves; the frame at time 100 keeps the transform it was located with.
        c.source.sample = Some(sample(RigValues { roll: -4.0, ..RigValues::ZERO }, offset));
        assert_eq!(c.delta(100, NOW, NOW), first);
        assert_eq!(c.delta_for_submit(100, NOW, NOW), first);
        // A submission 1 ms off reuses the nearest located frame.
        assert_eq!(c.delta_for_submit(1_000_100, NOW, NOW), first);
        // Far outside the tolerance a fresh transform is computed.
        let fresh = c.delta_for_submit(10_000_000_000, NOW, NOW);
        assert_ne!(fresh, first);
        // Passing through after locating still reverts what the frame was located with.
        c.source.sample = None;
        c.on_frame(NOW, 101);
        assert_eq!(c.delta_for_submit(100, NOW, NOW), first);
    }

    #[test]
    fn submission_reverts_only_what_was_applied() {
        let (truth, head, offset) = scene();
        let rig = RigValues { pitch: 3.0, ..RigValues::ZERO };
        let mut c = settled(rig, offset);
        c.on_frame(NOW, 100);
        c.calibrate(tracked(truth, &rig, head), NOW, NOW);
        let k = c.delta(100, NOW, NOW);
        assert!(!k.is_identity());
        c.record_applied(100, k);
        // The next frame's views could not be compensated (tracking lost, space not locatable):
        // its submission reverts nothing, not the previous frame's transform.
        c.on_frame(NOW, 200);
        c.record_applied(200, Motion::IDENTITY);
        assert!(c.delta_for_submit(200, NOW, NOW).is_identity());
        // A submission time next to the uncompensated frame follows that frame.
        assert!(c.delta_for_submit(210, NOW, NOW).is_identity());
        // A later successful locate of the same frame wins; an identity does not replace it.
        c.record_applied(200, k);
        c.record_applied(200, Motion::IDENTITY);
        assert_eq!(c.delta_for_submit(200, NOW, NOW), k);
    }

    #[test]
    fn cache_wraps() {
        let at = |t: i64| Motion::fixed(Pose::new(Quat::IDENTITY, Vec3::new(t as f64, 0.0, 0.0)));
        let mut cache = DeltaCache::default();
        for t in 0..40 {
            cache.insert(t, at(t));
        }
        assert!(cache.exact(0).is_none());
        assert_eq!(cache.exact(39).unwrap().pose.translation.x, 39.0);
        assert_eq!(cache.nearest(1000, 961).unwrap().pose.translation.x, 39.0);
        assert!(cache.nearest(1000, 900).is_none());
    }

    #[test]
    fn frame_entries_survive_many_other_locates() {
        let at = |t: i64| Motion::fixed(Pose::new(Quat::IDENTITY, Vec3::new(t as f64, 0.0, 0.0)));
        let mut cache = DeltaCache::default();
        cache.note_display_time(1_000);
        cache.insert(1_000, at(1_000));
        // A physics loop locates spaces at many other times before the frame is submitted.
        for t in 0..500 {
            cache.insert(2_000 + t, at(2_000 + t));
        }
        assert_eq!(cache.exact(1_000), Some(at(1_000)));
        // The nearest-match fallback prefers frame entries.
        assert_eq!(cache.nearest(1_001, 1_000_000), Some(at(1_000)));
    }

    #[test]
    fn tracking_space_change_discards_calibration() {
        let (truth, head, offset) = scene();
        let mut c = settled(RigValues::ZERO, offset);
        c.on_frame(NOW, 1);
        c.calibrate(tracked(truth, &RigValues::ZERO, head), NOW, NOW);
        c.store_offset(7, Pose::IDENTITY, 10);
        assert_eq!(c.offset(7, 10), Some(Pose::IDENTITY));
        c.on_reference_space_change(false, 50);
        assert!(c.offset(7, 60).is_none());
        // A location from before the change describes the old space: used once, never cached.
        c.store_offset(7, Pose::IDENTITY, 49);
        assert!(c.offset(7, 60).is_none());
        assert!(c.offset(7, 49).is_none());
        c.store_offset(7, Pose::IDENTITY, 50);
        assert_eq!(c.offset(7, 60), Some(Pose::IDENTITY));
        assert!(c.calibration().is_some());
        c.on_reference_space_change(true, 50);
        assert!(c.calibration().is_none());
        assert_eq!(c.status(), Status::Uncalibrated);
        assert!(!c.on_frame(NOW, 49), "waits for frames after the change");
        assert!(c.on_frame(NOW, 50));
    }
}
