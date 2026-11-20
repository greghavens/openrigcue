//! The OBD2HUD thread, fed from the service state. The BLE stack is BlueZ through
//! [`openrigcue_effects::accessories::hud::bluez`].

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use openrigcue_effects::accessories::dispatcher::Accessories;
use openrigcue_effects::accessories::hud::{firmware_path, HudHost, HudStatus, HudThread};
use openrigcue_effects::shakers::nsis::{NsisArchive, NsisError};
use openrigcue_model::Telemetry;

use crate::api::ui_state::UiServices;
use crate::state::ServiceState;

/// The firmware's install-relative path inside the SRS installer.
const INSTALLER_FIRMWARE: &str = "firmware/obd2hud/firmware.bin";

/// How often a long wait looks at the stop flag.
const STOP_POLL: Duration = Duration::from_millis(50);

/// The service state as the HUD driver reads and writes it.
pub struct ServiceHud {
    state: ServiceState,
    accessories: Arc<Accessories>,
    ui: Arc<UiServices>,
    /// Explicit firmware import; otherwise use the packaged Rust firmware.
    srs_install: Option<PathBuf>,
    stop: Arc<AtomicBool>,
}

impl ServiceHud {
    pub fn new(
        state: ServiceState,
        accessories: Arc<Accessories>,
        ui: Arc<UiServices>,
        srs_install: Option<PathBuf>,
        stop: Arc<AtomicBool>,
    ) -> ServiceHud {
        ServiceHud { state, accessories, ui, srs_install, stop }
    }
}

impl HudHost for ServiceHud {
    fn refresh_rate(&self) -> u32 {
        self.state.config.read().settings().hud.refresh_rate
    }

    fn model(&self) -> String {
        self.state.config.read().settings().hud.model.clone()
    }

    fn hud_switch(&self) -> bool {
        self.state.controls.read().switches.hud
    }

    fn game_running(&self) -> String {
        self.state.session.read().game_key().to_owned()
    }

    fn telemetry(&self) -> Telemetry {
        self.state.telemetry.snapshot()
    }

    fn set_status(&self, status: HudStatus) {
        self.accessories.hud.set(status);
    }

    fn translate(&self, text: &str) -> String {
        self.ui.translate(text)
    }

    /// The HUD firmware image. `--srs-install` may be the user's SRS installation folder or installer, whose
    /// `firmware/obd2hud/firmware.bin` entry is extracted (never executed).
    fn firmware_image(&self) -> std::io::Result<Vec<u8>> {
        read_firmware_image(
            self.srs_install.as_deref(),
            std::env::var_os("APPDIR").as_deref().map(Path::new),
            std::env::current_exe().ok().as_deref(),
        )
    }

    fn sleep(&self, duration: Duration) -> bool {
        let end = Instant::now() + duration;
        loop {
            if self.stop.load(Ordering::SeqCst) {
                return false;
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return true;
            }
            std::thread::sleep(left.min(STOP_POLL));
        }
    }
}

fn read_firmware_image(
    source: Option<&Path>,
    appdir: Option<&Path>,
    executable: Option<&Path>,
) -> std::io::Result<Vec<u8>> {
    match source {
        Some(install) if install.is_file() => {
            let archive = NsisArchive::open(std::fs::read(install)?).map_err(std::io::Error::other)?;
            archive.read(INSTALLER_FIRMWARE).map_err(|error| match error {
                NsisError::Missing(_) => std::io::Error::new(std::io::ErrorKind::NotFound, error),
                error => std::io::Error::other(error),
            })
        }
        Some(install) => std::fs::read(firmware_path(install)),
        None => {
            // The bundled image is built from this repository and has NOT been tested on a real OBD2HUD yet.
            let root = openrigcue_effects::accessories::firmware::bundled_firmware_root(
                appdir,
                executable,
                Path::new(INSTALLER_FIRMWARE),
            )
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "OBD2HUD firmware is not bundled; import an SRS installation to supply it",
                )
            })?;
            std::fs::read(firmware_path(&root))
        }
    }
}

/// Starts the HUD thread on BlueZ.
pub fn start(host: ServiceHud) -> std::io::Result<Option<HudThread>> {
    use openrigcue_effects::accessories::hud::bluez::BluezCentral;
    let stop = Arc::clone(&host.stop);
    HudThread::start(BluezCentral::new(Arc::clone(&stop)), host, stop).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrigcue_config::settings::paths::ConfigDir;

    fn host(srs_install: Option<PathBuf>) -> (ServiceHud, ServiceState, Arc<Accessories>, Arc<AtomicBool>) {
        let tmp = tempfile::tempdir().unwrap();
        let store = crate::store::ConfigStore::load(ConfigDir::at(tmp.path().join("cfg"))).unwrap();
        let state = ServiceState::new(store, crate::state::OriginalPlatform::Windows);
        let accessories = Arc::new(Accessories::new(Instant::now()));
        let stop = Arc::new(AtomicBool::new(false));
        let hud = ServiceHud::new(
            state.clone(),
            Arc::clone(&accessories),
            Arc::new(UiServices::off()),
            srs_install,
            Arc::clone(&stop),
        );
        (hud, state, accessories, stop)
    }

    /// The `HUD` settings, the HUD switch, the running game ('none' in stand-by) and the telemetry are read;
    /// the HUD status lands in the accessory view the sidebar reads.
    #[test]
    fn reads_and_publishes_the_service_state() {
        let (hud, state, accessories, _) = host(None);
        assert_eq!((hud.refresh_rate(), hud.model().as_str(), hud.hud_switch()), (5, "H", true));
        assert_eq!(hud.game_running(), "none");
        state.session.write().game = Some("dirtrally2".into());
        state.controls.write().switches.hud = false;
        state.telemetry.write().rpm = 4321.0;
        assert_eq!((hud.game_running().as_str(), hud.hud_switch(), hud.telemetry().rpm), ("dirtrally2", false, 4321.0));
        assert_eq!(crate::accessories::accessory_status(&accessories).hud, "Not Connected");
        hud.set_status(HudStatus::Flashing {
            prefix: hud.translate("Installing new firmware - progress: "),
            percent: 39,
        });
        assert_eq!(crate::accessories::accessory_status(&accessories).hud, "Installing new firmware - progress: 39%");
        hud.set_status(HudStatus::Connected);
        assert_eq!(crate::accessories::accessory_status(&accessories).hud, "Connected");
    }

    /// `firmware/obd2hud/firmware.bin` of an imported installation; an explicit import remains authoritative.
    #[test]
    fn firmware_comes_from_the_srs_installation() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("firmware").join("obd2hud");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("firmware.bin"), [1, 2, 3]).unwrap();
        assert_eq!(host(Some(tmp.path().to_path_buf())).0.firmware_image().unwrap(), [1, 2, 3]);
        // An installation folder without the file: the read fails.
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(
            host(Some(empty.path().to_path_buf())).0.firmware_image().unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
    }

    #[test]
    fn bundled_firmware_fallback_and_explicit_import_precedence() {
        let tmp = tempfile::tempdir().unwrap();
        let appdir = tmp.path().join("AppDir");
        let packaged = appdir.join("usr/bin").join(INSTALLER_FIRMWARE);
        std::fs::create_dir_all(packaged.parent().unwrap()).unwrap();
        std::fs::write(packaged, [1, 2, 3]).unwrap();
        assert_eq!(read_firmware_image(None, Some(&appdir), None).unwrap(), [1, 2, 3]);
        let native = tmp.path().join("native/bin");
        let native_image = native.join(INSTALLER_FIRMWARE);
        std::fs::create_dir_all(native_image.parent().unwrap()).unwrap();
        std::fs::write(&native_image, [4, 5]).unwrap();
        let executable = native.join("openrigcue");
        assert_eq!(read_firmware_image(None, None, Some(&executable)).unwrap(), [4, 5]);
        let imported = tmp.path().join("import");
        let image = imported.join(INSTALLER_FIRMWARE);
        std::fs::create_dir_all(image.parent().unwrap()).unwrap();
        std::fs::write(&image, [6, 7]).unwrap();
        assert_eq!(read_firmware_image(Some(&imported), Some(&appdir), Some(&executable)).unwrap(), [6, 7]);
        std::fs::remove_file(image).unwrap();
        assert_eq!(
            read_firmware_image(Some(&imported), Some(&appdir), Some(&executable)).unwrap_err().kind(),
            std::io::ErrorKind::NotFound,
            "a broken explicit import must not silently flash different firmware"
        );
        assert_eq!(read_firmware_image(None, None, None).unwrap_err().kind(), std::io::ErrorKind::NotFound);
    }

    /// The user's own installer (`OPENRIGCUE_SRS_INSTALLER`) ships the HUD firmware: 609088 bytes unpacked
    /// (289981 LZMA-packed, the size `7z l` lists), an ESP32 image starting with its 0xE9 magic byte.
    #[test]
    #[ignore = "needs a local SRS installer"]
    fn real_installer_ships_the_firmware() {
        let path = std::env::var("OPENRIGCUE_SRS_INSTALLER").expect("OPENRIGCUE_SRS_INSTALLER");
        let image = host(Some(PathBuf::from(path))).0.firmware_image().unwrap();
        assert_eq!((image.len(), image[0]), (609_088, 0xE9));
    }

    /// Waits end early once the service stops.
    #[test]
    fn sleeps_observe_the_stop_flag() {
        let (hud, _, _, stop) = host(None);
        assert!(hud.sleep(Duration::from_millis(1)));
        stop.store(true, Ordering::SeqCst);
        let started = Instant::now();
        assert!(!hud.sleep(Duration::from_secs(5)));
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
