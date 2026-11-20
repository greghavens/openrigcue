//! The two background threads of the Yaw VR driver: discovery (UDP broadcast, fills the device list) and
//! feedback (UDP reports: battery, motion-compensation yaw, link loss).
//!
//! Both start with the service, feedback first, then discovery, and are never stopped.
//! Discovery loops for the whole run, but only broadcasts while the selected model's manufacturer
//! is Yaw VR, pausing 5 s after each round. Feedback binds `[Yaw VR] udp_port` once and reads it for the whole
//! run; when the bind fails the thread ends with the error.
//!
//! Start-up also creates the zeroed `YawVRGEFile` block for the whole run
//! ([`open_game_engine_export`]); the feedback thread fills it.

use std::io;
use std::sync::Arc;
use std::thread::JoinHandle;

use openrigcue_devices::motion::yaw_vr::export::GameEngineExport;
use openrigcue_devices::motion::yaw_vr::{net, YawShared, YawVrSettings};
use openrigcue_devices::motion::{motion_model, LiveSettings};

use super::drivers::DriverSettings;

/// Manufacturer whose models the discovery serves.
const YAW_VR: &str = "Yaw VR";

/// The discovery's check: the selected model's manufacturer is Yaw VR.
pub fn yaw_vr_selected(model: &str) -> bool {
    motion_model(model, false).is_some_and(|m| m.manufacturer == YAW_VR)
}

/// Creates the zeroed `YawVRGEFile` block in the
/// bridge's mirror directory `dir`, for the whole run. `None` without a directory, or when the file cannot be
/// created (logged; the Yaw VR driver works without it).
pub fn open_game_engine_export(dir: Option<&std::path::Path>) -> Option<Arc<GameEngineExport>> {
    let dir = dir?;
    match GameEngineExport::create(GameEngineExport::path_in(dir)) {
        Ok(export) => Some(Arc::new(export)),
        Err(error) => {
            tracing::warn!(%error, "cannot create the YawVRGEFile export");
            None
        }
    }
}

/// Starts the threads' bodies. The default runs the driver's real sockets; tests use fakes. `Err` only when a
/// thread cannot be created.
pub trait YawNet {
    /// Feedback: binds the feedback port on its thread, then reads it for the whole run.
    fn feedback(
        &mut self,
        settings: LiveSettings<YawVrSettings>,
        shared: LiveSettings<YawShared>,
        selected: Box<dyn Fn() -> bool + Send>,
    ) -> io::Result<JoinHandle<()>>;
    /// Discovery: a discovery round every 5 s while `selected()`, for the whole run.
    fn discovery(
        &mut self,
        settings: LiveSettings<YawVrSettings>,
        shared: LiveSettings<YawShared>,
        selected: Box<dyn Fn() -> bool + Send>,
    ) -> io::Result<JoinHandle<()>>;
}

/// The Yaw VR driver's own threads ([`net::start_read_response`], [`net::start_call_devices`]); the feedback thread
/// stores each report in the `YawVRGEFile` block when there is one.
#[derive(Debug, Default)]
pub struct SystemYawNet {
    export: Option<Arc<GameEngineExport>>,
}

impl SystemYawNet {
    pub fn with_export(export: Option<Arc<GameEngineExport>>) -> SystemYawNet {
        SystemYawNet { export }
    }
}

impl YawNet for SystemYawNet {
    fn feedback(
        &mut self,
        settings: LiveSettings<YawVrSettings>,
        shared: LiveSettings<YawShared>,
        selected: Box<dyn Fn() -> bool + Send>,
    ) -> io::Result<JoinHandle<()>> {
        net::start_read_response(settings, shared, self.export.clone(), selected)
    }

    fn discovery(
        &mut self,
        settings: LiveSettings<YawVrSettings>,
        shared: LiveSettings<YawShared>,
        selected: Box<dyn Fn() -> bool + Send>,
    ) -> io::Result<JoinHandle<()>> {
        net::start_call_devices(settings, shared, selected)
    }
}

/// Starts the feedback thread, then the discovery thread; both run for
/// the whole process (without a stop). `selected` is the manufacturer check both make.
/// A feedback thread whose port cannot be bound ends with the error on its own thread.
pub(super) fn start(
    net: &mut impl YawNet,
    drivers: &DriverSettings,
    selected: impl Fn() -> bool + Clone + Send + 'static,
) -> io::Result<Vec<JoinHandle<()>>> {
    Ok(vec![
        net.feedback(drivers.yaw_vr.clone(), drivers.yaw_shared.clone(), Box::new(selected.clone()))?,
        net.discovery(drivers.yaw_vr.clone(), drivers.yaw_shared.clone(), Box::new(selected))?,
    ])
}

/// Yaw VR loops that open no socket and end at once (tests of the other motion threads).
#[cfg(test)]
pub(crate) struct NoYawNet;

#[cfg(test)]
impl YawNet for NoYawNet {
    fn feedback(
        &mut self,
        _: LiveSettings<YawVrSettings>,
        _: LiveSettings<YawShared>,
        _: Box<dyn Fn() -> bool + Send>,
    ) -> io::Result<JoinHandle<()>> {
        Ok(std::thread::spawn(|| {}))
    }

    fn discovery(
        &mut self,
        _: LiveSettings<YawVrSettings>,
        _: LiveSettings<YawShared>,
        _: Box<dyn Fn() -> bool + Send>,
    ) -> io::Result<JoinHandle<()>> {
        Ok(std::thread::spawn(|| {}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;
    use std::time::Duration;

    /// Records starts and runs loops that end once the test raises `stop` (no socket is opened).
    #[derive(Clone, Default)]
    struct FakeNet {
        starts: Arc<Mutex<Vec<&'static str>>>,
        stop: Arc<AtomicBool>,
    }

    impl FakeNet {
        fn spawn(&self, what: &'static str) -> JoinHandle<()> {
            self.starts.lock().unwrap().push(what);
            let stop = Arc::clone(&self.stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(1));
                }
            })
        }

        fn starts(&self) -> Vec<&'static str> {
            self.starts.lock().unwrap().clone()
        }
    }

    impl YawNet for FakeNet {
        fn feedback(
            &mut self,
            _: LiveSettings<YawVrSettings>,
            _: LiveSettings<YawShared>,
            _: Box<dyn Fn() -> bool + Send>,
        ) -> io::Result<JoinHandle<()>> {
            Ok(self.spawn("read_response"))
        }

        fn discovery(
            &mut self,
            _: LiveSettings<YawVrSettings>,
            _: LiveSettings<YawShared>,
            _: Box<dyn Fn() -> bool + Send>,
        ) -> io::Result<JoinHandle<()>> {
            Ok(self.spawn("call_devices"))
        }
    }

    /// Discovery broadcasts only while the selected model's manufacturer is Yaw VR.
    #[test]
    fn selection_follows_the_yaw_vr_manufacturer() {
        for model in ["Yaw3", "Yaw2 PRO", "Yaw2 STANDARD", "Yaw2 ARCADE", "Yaw VR PRO", "Yaw VR STANDARD"] {
            assert!(yaw_vr_selected(model), "{model}");
        }
        for model in ["None", "P3", "6 Actuators [2 Front + 2 Rear + TL + Surge]", "unknown"] {
            assert!(!yaw_vr_selected(model), "{model}");
        }
    }

    /// Start-up starts feedback, then discovery, whatever model is selected; they run until the
    /// program ends.
    #[test]
    fn both_threads_start_at_import_and_run() {
        let mut net = FakeNet::default();
        let handles = start(&mut net, &DriverSettings::default(), || false).unwrap();
        assert_eq!(net.starts(), ["read_response", "call_devices"]);
        assert!(handles.iter().all(|h| !h.is_finished()));
        net.stop.store(true, Ordering::SeqCst);
        for handle in handles {
            handle.join().unwrap();
        }
    }

    /// The export is created at start-up: the zeroed 17-byte block exists from the start, whatever model is
    /// selected.
    #[test]
    fn game_engine_block_exists_from_the_start() {
        let dir = tempfile::tempdir().unwrap();
        assert!(open_game_engine_export(None).is_none());
        let export = open_game_engine_export(Some(dir.path())).unwrap();
        assert_eq!(export.path(), dir.path().join("YawVRGEFile"));
        assert_eq!(std::fs::read(export.path()).unwrap(), [0; 17]);
    }
}
