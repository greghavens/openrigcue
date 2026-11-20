//! VR motion compensation wiring: the compensation sensor pass (IMU `'W'` or
//! IntelliComp `'V'`) feeds the OpenRigCue rig-pose file (`openrigcue-rigpose`) that the OpenXR layer and the
//! SteamVR driver read, at the IMU's output rate, with the IMU's gyro rates for prediction, the
//! motion-compensation switch as the enabled flag, a re-centre generation, and the centre-of-rotation offset.
//!
//! - [`VrCompensation`] is the shared handle (VR settings, re-centre generation, what was published).
//! - [`publisher`] runs the `vr_rig_pose` thread that writes the file.
//! - [`actions`] holds the page buttons and hotkey actions (calibrate, switches, re-centre).
//! - [`routes`] serves `/api/v1/compensation`.
//!
//! The SRS-compatible export (`Local\SimRacingStudioMotionRigPose`, for OXRMC/OVRMC inside a Proton prefix
//! through the bridge) stays with the sensor pass in [`crate::accessories`]; [`VrSettings::srs_export`] switches it.

pub mod actions;
pub mod publisher;
pub mod routes;
mod settings;
mod setup;
mod stream;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant};

use openrigcue_rigpose::{CorOffset, RigSample};

pub use actions::{run_action, run_shortcut, CompensationAction, RawSources};
pub use publisher::RigPoseThread;
pub use settings::{VrSettings, VrSettingsError};
pub use stream::{Framing, PoseStream, Reading};

/// File name of the rig pose next to the bridge mirrors (`/dev/shm/openrigcue/rig-pose`, the consumers' default).
pub const RIG_POSE_FILE: &str = "rig-pose";
/// File name of the VR settings in the configuration directory.
pub const SETTINGS_FILE: &str = "vr-compensation.toml";

/// Where the daemon writes the rig pose: `$OPENRIGCUE_RIG_POSE` when set (every consumer honours it too),
/// else `rig-pose` in the mirror directory.
pub fn rig_pose_path(mirror_dir: &Path) -> PathBuf {
    match std::env::var_os(openrigcue_rigpose::PATH_ENV) {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => mirror_dir.join(RIG_POSE_FILE),
    }
}

/// What the publisher has done, for the page.
#[derive(Debug, Default)]
struct Published {
    /// The publisher thread runs.
    running: bool,
    /// The writer is open.
    writing: bool,
    /// Why the file cannot be written, or the last failed pass.
    error: Option<String>,
    last: Option<RigSample>,
    /// Publish times of the last second, for the rate readout.
    recent: std::collections::VecDeque<Instant>,
}

struct Inner {
    setup: Mutex<Option<setup::SetupPaths>>,
    setup_operation: Mutex<()>,
    setup_errors: Mutex<Vec<(setup::Component, String)>>,
    settings: RwLock<VrSettings>,
    settings_path: Option<PathBuf>,
    rig_pose_path: PathBuf,
    recenter: AtomicU32,
    /// The centre-of-rotation offset of the last framed sample (or of the file taken over).
    framed_cor: Mutex<Option<CorOffset>>,
    published: Mutex<Published>,
}

/// The VR side of motion compensation, shared by the publisher thread, the API and the actions.
#[derive(Clone)]
pub struct VrCompensation(Arc<Inner>);

impl std::fmt::Debug for VrCompensation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VrCompensation").field("rig_pose_path", &self.0.rig_pose_path).finish_non_exhaustive()
    }
}

impl VrCompensation {
    /// Loads the settings from `settings_path` (defaults when missing or unreadable, which is logged; `None`
    /// keeps them in memory only) for a rig pose written to `rig_pose_path`.
    pub fn new(settings_path: Option<PathBuf>, rig_pose_path: PathBuf) -> VrCompensation {
        let settings = match settings_path.as_deref().map(VrSettings::load) {
            Some(Ok(settings)) => settings,
            Some(Err(error)) => {
                tracing::warn!(%error, "VR compensation settings unreadable; using the defaults");
                VrSettings::default()
            }
            None => VrSettings::default(),
        };
        VrCompensation(Arc::new(Inner {
            setup: Mutex::new(None),
            setup_operation: Mutex::new(()),
            setup_errors: Mutex::new(Vec::new()),
            settings: RwLock::new(settings),
            settings_path,
            rig_pose_path,
            recenter: AtomicU32::new(0),
            framed_cor: Mutex::new(None),
            published: Mutex::new(Published::default()),
        }))
    }

    /// The handle the service uses: settings in `config_root`, rig pose in `mirror_dir`.
    pub fn for_service(config_root: &Path, mirror_dir: &Path) -> VrCompensation {
        VrCompensation::new(Some(config_root.join(SETTINGS_FILE)), rig_pose_path(mirror_dir))
    }

    pub fn rig_pose_path(&self) -> &Path {
        &self.0.rig_pose_path
    }

    fn setup_paths(&self) -> setup::SetupPaths {
        self.0.setup.lock().unwrap_or_else(PoisonError::into_inner).clone().unwrap_or_else(setup::SetupPaths::discover)
    }

    /// Called only by the real service startup, never by constructors or status reads.
    pub fn initialize_native_integrations(&self) {
        let _operation = self.0.setup_operation.lock().unwrap_or_else(PoisonError::into_inner);
        let errors = self.setup_paths().startup();
        for (component, error) in &errors {
            let error = crate::i18n::plain(error);
            tracing::warn!(component = component.name(), %error, "VR integration setup failed");
        }
        *self.0.setup_errors.lock().unwrap_or_else(PoisonError::into_inner) = errors;
    }

    fn setup_view(&self) -> setup::SetupStatus {
        let mut status = self.setup_paths().status();
        for (component, error) in self.0.setup_errors.lock().unwrap_or_else(PoisonError::into_inner).iter() {
            status.set_error(*component, error.clone());
        }
        let live = openrigcue_rigpose::RigPoseReader::open(self.rig_pose_path())
            .ok()
            .and_then(|reader| reader.consumer().ok().flatten())
            .filter(|claim| claim.is_live(openrigcue_rigpose::monotonic_now_ns()))
            .map(|claim| claim.kind);
        status.set_activity(live);
        status
    }

    fn configure_integration(
        &self,
        component: setup::Component,
        uninstall: bool,
    ) -> Result<setup::SetupStatus, String> {
        let _operation = self.0.setup_operation.lock().unwrap_or_else(PoisonError::into_inner);
        let paths = self.setup_paths();
        let result = if uninstall { paths.uninstall(component) } else { paths.install(component) };
        let mut errors = self.0.setup_errors.lock().unwrap_or_else(PoisonError::into_inner);
        errors.retain(|(key, _)| *key != component);
        if let Err(error) = &result {
            errors.push((component, error.clone()));
        }
        drop(errors);
        result.map(|_| self.setup_view())
    }

    pub fn settings(&self) -> VrSettings {
        self.0.settings.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Replaces the settings and saves them. A moved centre of rotation re-centres the consumers at the next
    /// framed sample (see [`VrCompensation::framing`]).
    pub fn set_settings(&self, settings: VrSettings) -> Result<(), VrSettingsError> {
        settings.validate()?;
        if let Some(path) = &self.0.settings_path {
            settings.save(path)?;
        }
        *self.0.settings.write().unwrap_or_else(PoisonError::into_inner) = settings;
        Ok(())
    }

    /// The CoR offset and re-centre generation of the next sample, for `seated_height_mm`
    /// (`MOTION_COMPENSATION.height`). The consumers take the offset only at calibration (vr-xr-layer.md), so
    /// whenever the effective offset differs from the last framed one, whether from the VR settings or from
    /// Seated Height while "below the eyes" follows it, the generation is bumped first.
    pub fn framing(&self, seated_height_mm: f64) -> stream::Framing {
        let cor_offset = self.settings().cor_offset(seated_height_mm);
        let mut last = self.0.framed_cor.lock().unwrap_or_else(PoisonError::into_inner);
        if last.is_some_and(|previous| previous != cor_offset) {
            self.recenter();
        }
        *last = Some(cor_offset);
        stream::Framing { cor_offset, recenter_generation: self.recenter_generation() }
    }

    /// Asks the consumers to re-calibrate (seat frame and centre of rotation from the current head pose).
    pub fn recenter(&self) -> u32 {
        self.0.recenter.fetch_add(1, Ordering::SeqCst).wrapping_add(1)
    }

    pub fn recenter_generation(&self) -> u32 {
        self.0.recenter.load(Ordering::SeqCst)
    }

    /// The CoR offset of the last framed sample (zero before the first), for samples written without a pass.
    fn last_framed_cor(&self) -> CorOffset {
        self.0.framed_cor.lock().unwrap_or_else(PoisonError::into_inner).unwrap_or_default()
    }

    /// Continues the generation of a file written before (by an earlier run or by `openrigcue-rigpose-sim`),
    /// so a restart does not look like a re-centre request. Requests made meanwhile are kept on top.
    /// The file's CoR offset becomes the last framed one, so a CoR moved while no sample was written still
    /// re-centres the consumers.
    fn continue_file(&self, previous: Option<RigSample>) {
        if let Some(previous) = previous {
            self.0.recenter.fetch_add(previous.recenter_generation, Ordering::SeqCst);
            *self.0.framed_cor.lock().unwrap_or_else(PoisonError::into_inner) = Some(previous.cor_offset);
        }
    }

    fn published(&self) -> std::sync::MutexGuard<'_, Published> {
        self.0.published.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn note_running(&self, running: bool) {
        let mut p = self.published();
        p.running = running;
        if !running {
            p.writing = false;
        }
    }

    fn note_writer(&self, result: Result<(), String>) {
        let mut p = self.published();
        p.writing = result.is_ok();
        p.error = result.err();
    }

    fn note_error(&self, error: Option<String>) {
        self.published().error = error;
    }

    fn note_published(&self, sample: RigSample, at: Instant) {
        let mut p = self.published();
        p.last = Some(sample);
        p.recent.push_back(at);
        while p.recent.front().is_some_and(|t| at.saturating_duration_since(*t) > Duration::from_secs(1)) {
            p.recent.pop_front();
        }
    }

    /// The publisher's state for the status view.
    fn publisher_view(&self, now: Instant) -> PublisherView {
        let p = self.published();
        PublisherView {
            running: p.running,
            writing: p.writing,
            error: p.error.clone(),
            last: p.last,
            rate_hz: p.recent.iter().filter(|t| now.saturating_duration_since(**t) <= Duration::from_secs(1)).count()
                as f64,
        }
    }
}

/// A copy of [`Published`] for one status read.
#[derive(Debug, Clone)]
struct PublisherView {
    running: bool,
    writing: bool,
    error: Option<String>,
    last: Option<RigSample>,
    rate_hz: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Re-centre requests count up; a moved CoR is one (from the VR settings, or from Seated Height while
    /// "below the eyes" follows it); other settings changes are not; restarts continue the file's generation.
    #[test]
    fn recenter_generation() {
        let dir = tempfile::tempdir().unwrap();
        let vr = VrCompensation::new(Some(dir.path().join(SETTINGS_FILE)), dir.path().join(RIG_POSE_FILE));
        assert_eq!(vr.recenter_generation(), 0);
        assert_eq!(vr.framing(800.0).recenter_generation, 0, "the first framed sample is no move");
        assert_eq!(vr.recenter(), 1);
        vr.set_settings(VrSettings { srs_export: false, ..vr.settings() }).unwrap();
        assert_eq!(vr.framing(800.0).recenter_generation, 1);
        vr.set_settings(VrSettings { cor_forward_mm: 50.0, ..vr.settings() }).unwrap();
        let framing = vr.framing(800.0);
        assert_eq!(
            (framing.cor_offset, framing.recenter_generation),
            (CorOffset { forward: 0.05, down: 0.8, right: 0.0 }, 2)
        );
        assert_eq!(vr.framing(800.0).recenter_generation, 2, "an unchanged CoR is no move");
        let framing = vr.framing(900.0);
        assert_eq!(
            (framing.cor_offset.down, framing.recenter_generation),
            (0.9, 3),
            "cor_down_mm = None follows Seated Height"
        );
        vr.set_settings(VrSettings { cor_down_mm: Some(900.0), ..vr.settings() }).unwrap();
        assert_eq!(vr.framing(950.0).recenter_generation, 3, "a fixed CoR ignores Seated Height");
        assert!(vr.set_settings(VrSettings { cor_right_mm: f64::INFINITY, ..vr.settings() }).is_err());
        assert_eq!(vr.settings().cor_right_mm, 0.0, "refused settings are not kept");
        vr.set_settings(VrSettings { cor_down_mm: None, ..vr.settings() }).unwrap();
        vr.continue_file(Some(RigSample {
            recenter_generation: 40,
            cor_offset: CorOffset { forward: 0.05, down: 0.95, right: 0.0 },
            ..RigSample::default()
        }));
        assert_eq!(vr.recenter_generation(), 43);
        assert_eq!(vr.framing(950.0).recenter_generation, 43, "the file's CoR is the last framed one");
        assert_eq!(vr.framing(800.0).recenter_generation, 44);
        let reloaded = VrCompensation::new(Some(dir.path().join(SETTINGS_FILE)), dir.path().join(RIG_POSE_FILE));
        assert_eq!(
            reloaded.settings(),
            VrSettings { cor_forward_mm: 50.0, srs_export: false, ..VrSettings::default() }
        );
    }

    #[test]
    fn rig_pose_path_beside_the_mirrors() {
        if std::env::var_os(openrigcue_rigpose::PATH_ENV).is_none() {
            assert_eq!(
                rig_pose_path(Path::new("/dev/shm/openrigcue")),
                PathBuf::from(openrigcue_rigpose::DEFAULT_PATH)
            );
        }
    }
}
