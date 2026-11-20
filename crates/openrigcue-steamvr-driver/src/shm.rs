//! The rig-pose file as a [`RigSource`], plus the slow housekeeping around it (opening and
//! reopening the file, the active-consumer claim, settings reload, recentre requests, status).
//!
//! Split by thread: [`ShmRigSource::read`] runs on `vrserver`'s pose threads and only does the
//! contract's lock-free seqlock read; everything that may block or allocate runs in
//! [`Housekeeper::tick`] on the driver's own thread.

use std::fmt::Write as _;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::SystemTime;

use openrigcue_rigpose::{ClaimOutcome, ConsumerKind, RigPoseReader};

use crate::compensate::{Compensator, Skip};
use crate::config::Settings;
use crate::rig::{RigReading, RigSource};

/// Readers are never dropped while the driver runs (a pose thread may be using one), so a reopen
/// takes the next slot. 32 reopens per SteamVR session is far more than a daemon restart or two;
/// the daemon also reuses the file in place, so reopening is rarely needed at all.
const READER_SLOTS: usize = 32;

const CLAIM_FREE: u8 = 0;
const CLAIM_YIELDED: u8 = 1;

pub struct ShmRigSource {
    path: PathBuf,
    readers: [OnceLock<RigPoseReader>; READER_SLOTS],
    /// Index + 1 of the reader in use (0 = none).
    current: AtomicUsize,
    claim: AtomicU8,
}

impl ShmRigSource {
    pub fn new(path: PathBuf) -> Self {
        ShmRigSource {
            path,
            readers: [const { OnceLock::new() }; READER_SLOTS],
            current: AtomicUsize::new(0),
            claim: AtomicU8::new(CLAIM_FREE),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn reader(&self) -> Option<&RigPoseReader> {
        match self.current.load(Ordering::Acquire) {
            0 => None,
            i => self.readers[i - 1].get(),
        }
    }

    /// Installs a freshly opened reader; false when all slots are used up.
    fn install(&self, reader: RigPoseReader) -> bool {
        let next = self.current.load(Ordering::Acquire);
        if next >= READER_SLOTS || self.readers[next].set(reader).is_err() {
            return false;
        }
        self.current.store(next + 1, Ordering::Release);
        true
    }

    fn set_yielded(&self, yielded: bool) {
        self.claim.store(if yielded { CLAIM_YIELDED } else { CLAIM_FREE }, Ordering::Release);
    }

    pub fn yielded(&self) -> bool {
        self.claim.load(Ordering::Acquire) == CLAIM_YIELDED
    }
}

impl RigSource for ShmRigSource {
    fn read(&self) -> RigReading {
        let Some(reader) = self.reader() else {
            return RigReading::Absent;
        };
        if self.yielded() {
            return RigReading::ClaimedByOther;
        }
        match reader.read() {
            Ok(snapshot) => RigReading::Live(snapshot.sample),
            Err(_) => RigReading::Absent,
        }
    }
}

/// Facts the housekeeper needs from the hooking side.
#[derive(Clone, Copy, Debug, Default)]
pub struct HookState {
    pub host_hooks: usize,
    pub context_hooks: usize,
    /// `CLOCK_MONOTONIC` of the latest headset pose seen by the detour (0 = never).
    pub last_hmd_pose_ns: u64,
    pub pose_calls: u64,
}

/// A headset pose more recent than this means the headset is in use, so the driver holds the claim.
const HMD_ACTIVE_NS: u64 = 1_000_000_000;
const REOPEN_INTERVAL_NS: u64 = 1_000_000_000;
const SETTINGS_INTERVAL_NS: u64 = 1_000_000_000;
const STATUS_INTERVAL_NS: u64 = 1_000_000_000;

/// The driver's slow loop: call [`Housekeeper::tick`] every ~100 ms (well inside the contract's
/// 500 ms claim timeout).
pub struct Housekeeper {
    settings_path: Option<PathBuf>,
    settings_stamp: Option<SystemTime>,
    opened_inode: Option<u64>,
    last_reopen_ns: u64,
    last_settings_ns: u64,
    last_status_ns: u64,
    holding_claim: bool,
    pub messages: Vec<String>,
    pub claim_note: &'static str,
}

impl Housekeeper {
    pub fn new(settings_path: Option<PathBuf>) -> Self {
        Housekeeper {
            settings_path,
            settings_stamp: None,
            opened_inode: None,
            last_reopen_ns: 0,
            last_settings_ns: 0,
            last_status_ns: 0,
            holding_claim: false,
            messages: Vec::new(),
            claim_note: "none",
        }
    }

    pub fn recenter_request_path(source: &ShmRigSource) -> PathBuf {
        sibling(source.path(), "steamvr-driver.recenter")
    }

    pub fn status_path(source: &ShmRigSource) -> PathBuf {
        sibling(source.path(), "steamvr-driver.status")
    }

    /// Loads settings now (first call) and whenever the file's modification time changes.
    pub fn reload_settings(&mut self, compensator: &Compensator<&ShmRigSource>) {
        let Some(path) = &self.settings_path else {
            return;
        };
        let stamp = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        if stamp == self.settings_stamp && self.last_settings_ns != 0 {
            return;
        }
        self.settings_stamp = stamp;
        match Settings::load(path) {
            Ok(s) => {
                compensator.set_settings(s);
                self.messages.push(format!("settings loaded from {}", path.display()));
            }
            Err(e) => self.messages.push(format!("settings not changed: {e}")),
        }
    }

    pub fn tick(&mut self, compensator: &Compensator<&ShmRigSource>, hooks: HookState, now_ns: u64) {
        let source = *compensator.source();
        if now_ns.saturating_sub(self.last_settings_ns) >= SETTINGS_INTERVAL_NS {
            self.reload_settings(compensator);
            self.last_settings_ns = now_ns;
        }
        self.maintain_reader(source, now_ns);
        self.maintain_claim(compensator, hooks, now_ns);

        let request = Self::recenter_request_path(source);
        if request.exists() && std::fs::remove_file(&request).is_ok() {
            compensator.request_recenter();
            self.messages.push("recentre requested".into());
        }
        if now_ns.saturating_sub(self.last_status_ns) >= STATUS_INTERVAL_NS {
            self.last_status_ns = now_ns;
            let text = self.status_text(compensator, hooks, now_ns);
            let path = Self::status_path(source);
            let tmp = path.with_extension("status.tmp");
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }

    /// Releases the claim (driver shutdown).
    pub fn shutdown(&mut self, source: &ShmRigSource) {
        if let Some(reader) = source.reader() {
            let _ = reader.release(ConsumerKind::STEAMVR_DRIVER);
        }
        self.holding_claim = false;
        let _ = std::fs::remove_file(Self::status_path(source));
    }

    fn maintain_reader(&mut self, source: &ShmRigSource, now_ns: u64) {
        let on_disk = std::fs::metadata(source.path()).ok().map(|m| m.ino());
        let healthy = source.reader().is_some_and(|r| r.read().is_ok()) && on_disk == self.opened_inode;
        if healthy || now_ns.saturating_sub(self.last_reopen_ns) < REOPEN_INTERVAL_NS {
            return;
        }
        self.last_reopen_ns = now_ns;
        if on_disk.is_none() {
            return;
        }
        match RigPoseReader::open(source.path()) {
            Ok(reader) => {
                let writable = reader.is_writable();
                if source.install(reader) {
                    self.opened_inode = on_disk;
                    self.holding_claim = false;
                    self.messages.push(format!(
                        "rig pose file {} opened{}",
                        source.path().display(),
                        if writable { "" } else { " read-only (cannot record the active-consumer claim)" }
                    ));
                } else {
                    self.messages.push("rig pose file changed too often; keeping the old reader".into());
                    self.opened_inode = on_disk;
                }
            }
            Err(e) => self.messages.push(format!("rig pose file not usable yet: {e}")),
        }
    }

    fn maintain_claim(&mut self, compensator: &Compensator<&ShmRigSource>, hooks: HookState, now_ns: u64) {
        let source = *compensator.source();
        let Some(reader) = source.reader() else {
            source.set_yielded(false);
            self.claim_note = "no rig pose file";
            return;
        };
        let settings = compensator.settings();
        if settings.yield_to_openxr_layer {
            if self.holding_claim {
                let _ = reader.release(ConsumerKind::STEAMVR_DRIVER);
                self.holding_claim = false;
            }
            let layer_live = matches!(
                reader.consumer(),
                Ok(Some(c)) if c.kind == ConsumerKind::XR_LAYER && c.is_live(now_ns)
            );
            source.set_yielded(layer_live);
            self.claim_note = if layer_live { "yielding to a live OpenXR layer" } else { "not claiming (yield mode)" };
            return;
        }
        let headset_active = hooks.host_hooks > 0
            && hooks.last_hmd_pose_ns != 0
            && now_ns.saturating_sub(hooks.last_hmd_pose_ns) < HMD_ACTIVE_NS;
        // Fresh enough that the compensator still applies it (stale threshold plus fade-out).
        let max_age_ns = ((settings.stale_after_ms.max(0.0) + settings.fade_ms.max(0.0)) * 1e6) as u64;
        let usable = matches!(
            reader.read(),
            Ok(s) if s.sample.enabled && s.sample.timestamp_ns != 0 && s.sample.age_ns(now_ns) <= max_age_ns
        );
        if settings.enabled && headset_active && usable {
            match reader.claim(ConsumerKind::STEAMVR_DRIVER, now_ns) {
                Ok(ClaimOutcome::Granted { recorded }) => {
                    source.set_yielded(false);
                    self.holding_claim = recorded;
                    self.claim_note = if recorded {
                        "holding the claim"
                    } else {
                        "compensating; claim not recordable (read-only file)"
                    };
                }
                Ok(ClaimOutcome::Yielded(_)) => {
                    source.set_yielded(true);
                    self.holding_claim = false;
                    self.claim_note = "yielded to a higher-ranked consumer";
                }
                Err(e) => {
                    self.claim_note = "claim I/O failed";
                    self.messages.push(format!("claim failed: {e}"));
                }
            }
        } else {
            if self.holding_claim {
                let _ = reader.release(ConsumerKind::STEAMVR_DRIVER);
                self.holding_claim = false;
            }
            source.set_yielded(false);
            self.claim_note = "idle (not compensating, claim released)";
        }
    }

    fn status_text(&self, compensator: &Compensator<&ShmRigSource>, hooks: HookState, now_ns: u64) -> String {
        let c = &compensator.counters;
        let mut s = String::new();
        let _ = writeln!(s, "driver=openrigcue-steamvr-driver {}", env!("CARGO_PKG_VERSION"));
        let _ = writeln!(s, "pid={}", std::process::id());
        let _ = writeln!(s, "host_vtables_hooked={}", hooks.host_hooks);
        let _ = writeln!(s, "context_vtables_hooked={}", hooks.context_hooks);
        let _ = writeln!(s, "pose_updates_seen={}", hooks.pose_calls);
        let _ = writeln!(s, "poses_compensated={}", c.compensated.load(Ordering::Relaxed));
        let _ = writeln!(s, "poses_passed_through={}", c.skipped.load(Ordering::Relaxed));
        let last_hmd = c.last_hmd_compensated_ns.load(Ordering::Relaxed);
        let _ = writeln!(
            s,
            "hmd_last_compensated_ms_ago={}",
            if last_hmd == 0 { "never".into() } else { (now_ns.saturating_sub(last_hmd) / 1_000_000).to_string() }
        );
        let skip = Skip::from_code(c.last_hmd_skip.load(Ordering::Relaxed)).map_or("none", Skip::name);
        let _ = writeln!(s, "hmd_last_pass_through_reason={skip}");
        let _ = writeln!(s, "calibrations={}", c.calibrations.load(Ordering::Relaxed));
        let _ = writeln!(s, "calibrated={}", compensator.calibration().is_some());
        if let Some(cal) = compensator.calibration() {
            let p = cal.base.pos;
            let _ = writeln!(s, "cor_world_m={:.3},{:.3},{:.3}", p.x, p.y, p.z);
            let _ = writeln!(s, "seat_heading_deg={:.1}", cal.base.rot.yaw().to_degrees());
        }
        let _ = writeln!(s, "claim={}", self.claim_note);
        let reader = compensator.source().reader();
        match reader.map(|r| r.read()) {
            Some(Ok(snap)) => {
                let v = snap.sample.values;
                let _ = writeln!(s, "rig_enabled={}", snap.sample.enabled);
                let _ = writeln!(s, "rig_sample_age_ms={}", snap.sample.age_ns(now_ns) / 1_000_000);
                let _ = writeln!(
                    s,
                    "rig_values=sway {:.1} surge {:.1} heave {:.1} mm, yaw {:.2} roll {:.2} pitch {:.2} deg",
                    v.sway, v.surge, v.heave, v.yaw, v.roll, v.pitch
                );
                let _ = writeln!(s, "rig_rates_published={}", snap.sample.rates.is_some());
            }
            Some(Err(e)) => {
                let _ = writeln!(s, "rig_pose=error: {e}");
            }
            None => {
                let _ = writeln!(s, "rig_pose=not open ({})", compensator.source().path().display());
            }
        }
        let settings = compensator.settings();
        let _ = writeln!(s, "settings={settings:?}");
        s
    }
}

fn sibling(path: &Path, name: &str) -> PathBuf {
    path.parent().map_or_else(|| PathBuf::from(name), |d| d.join(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compensate::{DeviceKind, Outcome};
    use crate::openvr::DriverPose;
    use openrigcue_rigpose::{monotonic_now_ns, RigPoseWriter, RigSample, RigValues};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("openrigcue-steamvr-driver-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn active_hooks(now: u64) -> HookState {
        HookState { host_hooks: 1, context_hooks: 1, last_hmd_pose_ns: now, pose_calls: 1 }
    }

    #[test]
    fn missing_file_is_absent_then_opened_when_it_appears() {
        let dir = temp_dir("open");
        let path = dir.join("rig-pose");
        let source = ShmRigSource::new(path.clone());
        let comp = Compensator::new(&source, Settings::default());
        let mut hk = Housekeeper::new(None);
        hk.tick(&comp, HookState::default(), 10_000_000_000);
        assert_eq!(source.read(), RigReading::Absent);

        let mut writer = RigPoseWriter::create(&path).unwrap();
        let now = monotonic_now_ns();
        let sample = RigSample {
            timestamp_ns: now,
            values: RigValues { pitch: 2.0, ..RigValues::ZERO },
            enabled: true,
            ..RigSample::default()
        };
        writer.publish(&sample).unwrap();
        hk.tick(&comp, HookState::default(), 20_000_000_000);
        assert_eq!(source.read(), RigReading::Live(sample));
        assert!(Housekeeper::status_path(&source).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn claims_while_the_headset_is_active_and_releases_when_idle() {
        let dir = temp_dir("claim");
        let path = dir.join("rig-pose");
        let mut writer = RigPoseWriter::create(&path).unwrap();
        let now = monotonic_now_ns();
        writer.publish(&RigSample { timestamp_ns: now, enabled: true, ..RigSample::default() }).unwrap();
        let source = ShmRigSource::new(path.clone());
        let comp = Compensator::new(&source, Settings::default());
        let mut hk = Housekeeper::new(None);
        let observer = RigPoseReader::open(&path).unwrap();

        // An OpenXR layer holds the claim; the driver outranks it per the contract.
        observer.claim(ConsumerKind::XR_LAYER, now).unwrap();
        hk.tick(&comp, active_hooks(now), now);
        let holder = observer.consumer().unwrap().unwrap();
        assert_eq!(holder.kind, ConsumerKind::STEAMVR_DRIVER);
        assert!(!source.yielded());

        // Headset idle: the driver releases its claim.
        hk.tick(&comp, HookState { last_hmd_pose_ns: now - 2_000_000_000, ..active_hooks(now) }, now);
        assert_eq!(observer.consumer().unwrap(), None);

        // A consumer ranked above the driver (a future kind) makes it yield.
        observer.claim(ConsumerKind(9), now).unwrap();
        hk.tick(&comp, active_hooks(now), now);
        assert!(source.yielded());
        assert_eq!(source.read(), RigReading::ClaimedByOther);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn yield_mode_passes_through_while_a_layer_is_live() {
        let dir = temp_dir("yield");
        let path = dir.join("rig-pose");
        let mut writer = RigPoseWriter::create(&path).unwrap();
        let now = monotonic_now_ns();
        writer.publish(&RigSample { timestamp_ns: now, enabled: true, ..RigSample::default() }).unwrap();
        let source = ShmRigSource::new(path.clone());
        let comp = Compensator::new(&source, Settings { yield_to_openxr_layer: true, ..Settings::default() });
        let mut hk = Housekeeper::new(None);
        let layer = RigPoseReader::open(&path).unwrap();

        hk.tick(&comp, active_hooks(now), now);
        assert!(!source.yielded());
        assert_eq!(layer.consumer().unwrap(), None, "yield mode never writes a claim");

        layer.claim(ConsumerKind::XR_LAYER, now).unwrap();
        hk.tick(&comp, active_hooks(now), now);
        assert!(source.yielded());
        let mut pose = DriverPose::default();
        assert_eq!(comp.process(DeviceKind::Hmd, &mut pose, now), Outcome::Skipped(Skip::ClaimedByOther));

        // The layer's heartbeat expires: compensation resumes.
        hk.tick(&comp, active_hooks(now), now + 600_000_000);
        assert!(!source.yielded());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn recenter_request_file_and_settings_reload() {
        let dir = temp_dir("files");
        let path = dir.join("rig-pose");
        let settings_path = dir.join("steamvr-driver.toml");
        std::fs::write(&settings_path, "compensate_controllers = false\n").unwrap();
        let source = ShmRigSource::new(path.clone());
        let comp = Compensator::new(&source, Settings::default());
        let mut hk = Housekeeper::new(Some(settings_path.clone()));
        hk.tick(&comp, HookState::default(), 10_000_000_000);
        assert!(!comp.settings().compensate_controllers);

        std::fs::write(Housekeeper::recenter_request_path(&source), "").unwrap();
        hk.tick(&comp, HookState::default(), 10_100_000_000);
        assert!(comp.recenter_pending());
        assert!(!Housekeeper::recenter_request_path(&source).exists());

        // A malformed edit keeps the previous settings.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&settings_path, "compensate_controllers = maybe\n").unwrap();
        hk.tick(&comp, HookState::default(), 12_000_000_000);
        assert!(!comp.settings().compensate_controllers);
        assert!(hk.messages.iter().any(|m| m.contains("settings not changed")));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
