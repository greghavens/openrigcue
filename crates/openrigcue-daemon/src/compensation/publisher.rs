//! The `vr_rig_pose` thread: one rig-pose sample per IMU reading (`'W'`), or per `1 / refresh_rate`
//! (IntelliComp `'V'`), written with [`RigPoseWriter`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use openrigcue_config::settings::CompensationSensor;
use openrigcue_effects::accessories::compensation::refresh_pause;
use openrigcue_effects::accessories::dispatcher::Accessories;
use openrigcue_effects::accessories::imu::{Imu, ImuSamples};
use openrigcue_rigpose::{monotonic_now_ns, RigPoseReader, RigPoseWriter, RigSample};

use super::stream::{PoseStream, Reading};
use super::VrCompensation;
use crate::accessories::ServiceFeed;
use crate::state::ServiceState;

/// The longest wait for an IMU reading before the loop looks at the settings again.
const IMU_WAIT: Duration = Duration::from_millis(100);
/// How often a neutral, disabled sample is written while nothing is measured, so consumers pass through.
const IDLE_PERIOD: Duration = Duration::from_millis(100);
/// Retry period while the file cannot be opened (another writer, such as `openrigcue-rigpose-sim`, holds it).
const REOPEN_PERIOD: Duration = Duration::from_secs(1);
/// The pause after a failed pass.
const ERROR_PAUSE: Duration = Duration::from_secs(1);

/// What the thread reads.
pub struct PublisherContext {
    pub vr: VrCompensation,
    pub state: ServiceState,
    pub feed: Arc<ServiceFeed>,
    pub accessories: Arc<Accessories>,
}

/// The running thread.
pub struct RigPoseThread {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl RigPoseThread {
    pub fn start(context: PublisherContext) -> std::io::Result<RigPoseThread> {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = std::thread::Builder::new().name("vr_rig_pose".into()).spawn(move || {
            context.vr.note_running(true);
            Publisher::new(context).run(&flag);
        })?;
        Ok(RigPoseThread { stop, handle: Some(handle) })
    }

    /// Ends the thread; the writer's drop publishes the last pose disabled, so consumers pass through at once.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for RigPoseThread {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Sleeps `period` in slices, returning early once `stop` is set.
/// Why the rig-pose file cannot be written, as a sentence with its values ([`crate::i18n::sentence`]) for the
/// status the page shows; the path and the system's error text are values.
fn rig_pose_failure(error: &openrigcue_rigpose::RigPoseError) -> String {
    use openrigcue_rigpose::RigPoseError;
    let shown = |path: &std::path::Path| path.display().to_string();
    match error {
        RigPoseError::Missing(path) => {
            crate::i18n::sentence("rig-pose file ^path^ does not exist", &[("path", &shown(path))])
        }
        RigPoseError::AlreadyLocked { path } => crate::i18n::sentence(
            "rig-pose file ^path^ is already being written by another process",
            &[("path", &shown(path))],
        ),
        RigPoseError::BadHeader { path, check } => crate::i18n::sentence(
            "rig-pose file ^path^ is not a valid version-^version^ file (^check^)",
            &[
                ("path", &shown(path)),
                ("version", &openrigcue_rigpose::layout::VERSION.to_string()),
                ("check", &format!("{check:?}")),
            ],
        ),
        RigPoseError::TooShort { path, len } => crate::i18n::sentence(
            "rig-pose file ^path^ is too short (^bytes^ bytes)",
            &[("path", &shown(path)), ("bytes", &len.to_string())],
        ),
        RigPoseError::Io { path, source } => crate::i18n::sentence(
            "rig-pose file ^path^: ^error^",
            &[("path", &shown(path)), ("error", &source.to_string())],
        ),
        RigPoseError::Busy => error.to_string(),
    }
}

fn pause(period: Duration, stop: &AtomicBool) {
    let end = Instant::now() + period;
    while !stop.load(Ordering::SeqCst) {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        std::thread::sleep(left.min(Duration::from_millis(20)));
    }
}

struct Publisher {
    context: PublisherContext,
    writer: Option<RigPoseWriter>,
    stream: PoseStream,
    imu: Option<ImuSamples>,
    last_sequence: u64,
}

impl Publisher {
    fn new(context: PublisherContext) -> Publisher {
        Publisher { context, writer: None, stream: PoseStream::new(), imu: None, last_sequence: 0 }
    }

    fn run(mut self, stop: &AtomicBool) {
        while !stop.load(Ordering::SeqCst) {
            if self.writer.is_none() && !self.open_writer() {
                pause(REOPEN_PERIOD, stop);
                continue;
            }
            let wait = self.pass();
            if let Some(wait) = wait {
                pause(wait, stop);
            }
        }
        // Dropping the writer republishes the last sample with `enabled = false`.
        self.writer = None;
        self.context.vr.note_running(false);
    }

    /// Opens the file, continuing the generation found in it.
    fn open_writer(&mut self) -> bool {
        let vr = &self.context.vr;
        let path = vr.rig_pose_path().to_path_buf();
        let previous = RigPoseReader::open(&path).and_then(|r| r.read()).map(|s| s.sample).ok();
        match RigPoseWriter::create(&path) {
            Ok(writer) => {
                vr.continue_file(previous);
                vr.note_writer(Ok(()));
                tracing::info!(path = %path.display(), "publishing the rig pose");
                self.writer = Some(writer);
                true
            }
            Err(error) => {
                vr.note_writer(Err(rig_pose_failure(&error)));
                false
            }
        }
    }

    /// The IMU reading stream of the connected sensor, picked up again after a reconnect.
    fn imu_samples(&mut self) -> Option<ImuSamples> {
        if self.imu.as_ref().is_none_or(ImuSamples::closed) {
            let slot = self.context.accessories.imu.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            self.imu = slot.connection().device().map(Imu::samples);
            self.last_sequence = 0;
        }
        self.imu.clone()
    }

    /// One pass; returns the pause before the next one (`None`: wait for the next IMU reading at once).
    fn pass(&mut self) -> Option<Duration> {
        let sensor = self.context.state.config.read().settings().compensation.sensor;
        let (reading, wait) = match sensor {
            CompensationSensor::Witmotion => match self.imu_samples() {
                None => (Reading::without_sensor(monotonic_now_ns()), Some(IDLE_PERIOD)),
                Some(samples) => {
                    let sample = samples.wait_newer(self.last_sequence, IMU_WAIT)?;
                    self.last_sequence = sample.sequence;
                    (Reading::from_imu(&sample, monotonic_now_ns(), Instant::now()), None)
                }
            },
            CompensationSensor::Virtual => {
                let rate = i64::from(self.context.state.config.read().settings().compensation.refresh_rate);
                (Reading::without_sensor(monotonic_now_ns()), Some(refresh_pause(rate).unwrap_or(ERROR_PAUSE)))
            }
        };
        let vr = self.context.vr.clone();
        let stream = &mut self.stream;
        let result = self.context.feed.with_compensation_inputs(|inputs| {
            let framing = vr.framing(inputs.settings.height);
            refresh_pause(inputs.settings.refresh_rate).and_then(|_| stream.next(&reading, inputs, framing))
        });
        match result {
            Ok(sample) => {
                self.publish(sample);
                vr.note_error(None);
                wait
            }
            Err(error) => {
                vr.note_error(Some(error.to_string()));
                let disabled = RigSample {
                    enabled: false,
                    timestamp_ns: monotonic_now_ns(),
                    cor_offset: vr.last_framed_cor(),
                    recenter_generation: vr.recenter_generation(),
                    ..RigSample::default()
                };
                self.publish(disabled);
                Some(ERROR_PAUSE)
            }
        }
    }

    fn publish(&mut self, sample: RigSample) {
        let Some(writer) = self.writer.as_mut() else { return };
        match writer.publish(&sample) {
            Ok(()) => self.context.vr.note_published(sample, Instant::now()),
            Err(error) => {
                tracing::warn!(%error, "cannot write the rig pose; reopening");
                self.context.vr.note_writer(Err(rig_pose_failure(&error)));
                self.writer = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accessories::{PortCache, POSE_EXPORT_FILE};
    use crate::compensation::VrSettings;
    use crate::motion::testing::OfflineHost;
    use crate::motion::MotionHandle;
    use crate::state::OriginalPlatform;
    use crate::store::ConfigStore;
    use openrigcue_config::settings::Configuration;
    use openrigcue_effects::accessories::dispatcher::AccessoryFeed;
    use openrigcue_rigpose::{CorOffset, Snapshot};

    struct Rig {
        _tmp: tempfile::TempDir,
        state: ServiceState,
        vr: VrCompensation,
        feed: Arc<ServiceFeed>,
        accessories: Arc<Accessories>,
    }

    fn rig() -> Rig {
        let tmp = tempfile::tempdir().unwrap();
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        state.config.write().update(|c| {
            c.settings.compensation.sensor = CompensationSensor::Virtual;
            c.settings.compensation.height = 800.0;
        });
        let motion = MotionHandle::for_settings(state.config.read().settings(), Box::new(OfflineHost::default()));
        let accessories = Arc::new(Accessories::new(Instant::now()));
        let vr = VrCompensation::new(None, tmp.path().join("rig-pose"));
        let feed = Arc::new(
            ServiceFeed::new(
                state.clone(),
                motion,
                Arc::clone(&accessories),
                PortCache::in_memory(),
                Some(tmp.path().to_path_buf()),
            )
            .with_vr(vr.clone()),
        );
        Rig { _tmp: tmp, state, vr, feed, accessories }
    }

    fn start(rig: &Rig) -> RigPoseThread {
        RigPoseThread::start(PublisherContext {
            vr: rig.vr.clone(),
            state: rig.state.clone(),
            feed: Arc::clone(&rig.feed),
            accessories: Arc::clone(&rig.accessories),
        })
        .unwrap()
    }

    /// Polls the rig-pose file until `done` holds (5 s at most).
    fn wait_for(rig: &Rig, done: impl Fn(&Snapshot) -> bool) -> Snapshot {
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(snapshot) = RigPoseReader::open(rig.vr.rig_pose_path()).and_then(|r| r.read()) {
                if done(&snapshot) {
                    return snapshot;
                }
            }
            assert!(Instant::now() < end, "the rig pose never reached the expected state");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// IntelliComp ('V') passes every 1 / refresh_rate: enabled follows mc_switch, the CoR offset comes from
    /// the VR settings (down from Seated Height), re-centres raise the generation, and stopping the thread
    /// leaves the pose disabled for the consumers.
    #[test]
    fn intellicomp_stream_end_to_end() {
        let rig = rig();
        rig.vr.set_settings(VrSettings { cor_forward_mm: 100.0, ..VrSettings::default() }).unwrap();
        let thread = start(&rig);
        let first = wait_for(&rig, |s| s.sample.enabled);
        assert_eq!(first.sample.cor_offset, CorOffset { forward: 0.1, down: 0.8, right: 0.0 });
        assert_eq!(first.sample.recenter_generation, 0, "a new file: the first CoR is no move");
        assert_eq!(first.writer_pid, std::process::id());
        rig.vr.recenter();
        wait_for(&rig, |s| s.sample.recenter_generation == 1);
        // Seated Height moves the CoR while "below the eyes" follows it: the consumers re-centre.
        rig.state.config.write().update(|c| c.settings.compensation.height = 900.0);
        let moved = wait_for(&rig, |s| s.sample.cor_offset.down == 0.9);
        assert_eq!(moved.sample.recenter_generation, 2);
        rig.state.controls.write().switches.compensation = false;
        wait_for(&rig, |s| !s.sample.enabled);
        rig.state.controls.write().switches.compensation = true;
        wait_for(&rig, |s| s.sample.enabled);
        assert!(rig.vr.publisher_view(Instant::now()).rate_hz > 0.0);
        thread.stop();
        let last = RigPoseReader::open(rig.vr.rig_pose_path()).unwrap().read().unwrap();
        assert!(!last.sample.enabled, "the writer's drop disables the pose");
        assert!(!rig.vr.publisher_view(Instant::now()).running);
    }

    /// 'W' without a connected IMU: no session, neutral disabled samples; a held writer lock (for example
    /// openrigcue-rigpose-sim) is reported and retried; taking the file over continues its generation, and
    /// because the sim's CoR (0) differs from ours (Seated Height 800 mm) the consumers are re-centred once.
    #[test]
    fn sensor_mode_without_imu_and_a_busy_file() {
        let rig = rig();
        rig.state.config.write().update(|c| c.settings.compensation.sensor = CompensationSensor::Witmotion);
        let mut sim = RigPoseWriter::create(rig.vr.rig_pose_path()).unwrap();
        sim.publish(&RigSample {
            timestamp_ns: monotonic_now_ns(),
            recenter_generation: 5,
            enabled: true,
            ..RigSample::default()
        })
        .unwrap();
        let thread = start(&rig);
        let end = Instant::now() + Duration::from_secs(5);
        while rig.vr.publisher_view(Instant::now()).error.is_none() {
            assert!(Instant::now() < end);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(rig.vr.publisher_view(Instant::now()).error.unwrap().contains("already being written"));
        drop(sim);
        let snapshot = wait_for(&rig, |s| {
            s.writer_pid == std::process::id()
                && s.sample.timestamp_ns > 0
                && !s.sample.enabled
                && s.sample.recenter_generation == 6
        });
        assert_eq!(snapshot.sample.values, openrigcue_rigpose::RigValues::ZERO);
        thread.stop();
    }

    /// The SRS-compatible export for the bridge is written by the sensor pass only while it is switched on.
    #[test]
    fn srs_export_follows_its_switch() {
        let rig = rig();
        let export = rig.vr.rig_pose_path().with_file_name(POSE_EXPORT_FILE);
        rig.feed.compensation(&mut |_| Ok(Duration::ZERO)).unwrap();
        assert_eq!(std::fs::read(&export).unwrap().len(), 48);
        std::fs::remove_file(&export).unwrap();
        rig.vr.set_settings(VrSettings { srs_export: false, ..VrSettings::default() }).unwrap();
        rig.feed.compensation(&mut |_| Ok(Duration::ZERO)).unwrap();
        assert!(!export.exists());
    }
}
