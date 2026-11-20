//! Intellibox firmware flashing through avrdude.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use tracing::debug;

use super::link::Clock;

/// The fixed system location of the image (`/usr/local/bin/firmware/intellibox/intellibox.hex`).
/// [`installed_hex_path`] gives the location inside an application folder.
pub const DEFAULT_HEX_PATH: &str = "/usr/local/bin/firmware/intellibox/intellibox.hex";

/// The system avrdude program.
pub const AVRDUDE_PROGRAM: &str = "/usr/bin/avrdude";

/// The image inside an application folder (`firmware/intellibox/intellibox.hex`).
pub fn installed_hex_path(srs_app_path: &Path) -> PathBuf {
    srs_app_path.join("firmware").join("intellibox").join("intellibox.hex")
}

/// Find a packaged firmware root. Explicit user imports are selected by the caller
/// before this fallback. A missing image never selects another device's firmware.
pub fn bundled_firmware_root(appdir: Option<&Path>, executable: Option<&Path>, relative: &Path) -> Option<PathBuf> {
    appdir
        .map(|root| root.join("usr/bin"))
        .into_iter()
        .chain(executable.and_then(Path::parent).map(Path::to_path_buf))
        .find(|root| root.join(relative).is_file())
}

/// The avrdude configuration inside an application folder (`avr_helpers/avrdude.conf`).
pub fn installed_conf_path(srs_app_path: &Path) -> PathBuf {
    srs_app_path.join("avr_helpers").join("avrdude.conf")
}

/// The log avrdude writes and the flasher reads back
/// (`avrdude.log` in the documents folder).
pub fn log_path(srs_my_documents: &Path) -> PathBuf {
    srs_my_documents.join("avrdude.log")
}

/// The bootloader speed starts at 115200; after every attempt it alternates with 57600.
const INITIAL_BAUD_RATE: u32 = 115_200;

/// The status before the avrdude call; it stays so when the call fails outright.
const STATUS_UNSET: &str = "will_see_what_happens";

/// Result of one flashing attempt, by status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlashOutcome {
    /// `'success'`.
    Success,
    /// `'wrong_baud_rate'`: the bootloader did not answer at this speed.
    WrongBaudRate,
    /// Any other status: `'access_denied'`, `'firmware_not_found'`, or `'will_see_what_happens'` when the
    /// avrdude call failed outright.
    Failed(String),
}

impl FlashOutcome {
    fn from_status(status: &str) -> FlashOutcome {
        match status {
            "success" => FlashOutcome::Success,
            "wrong_baud_rate" => FlashOutcome::WrongBaudRate,
            other => FlashOutcome::Failed(other.to_owned()),
        }
    }
}

/// One avrdude call: programmer `arduino`, part `atmega328p`, the given speed and port, writing the
/// image without erasing first (`-D`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvrdudeInvocation {
    pub programmer: &'static str,
    pub part: &'static str,
    pub baud_rate: String,
    pub port: String,
    pub hex: PathBuf,
    /// `-C`: the avrdude configuration file.
    pub conf: PathBuf,
    /// `-l`: the log the result is read from.
    pub log: PathBuf,
    /// Extra flags (`-D`: do not erase before writing).
    pub extra_flags: Vec<String>,
}

impl AvrdudeInvocation {
    /// The command line after the program (`-c`, `-b`, `-p`, `-P`, `-U flash:w:<abs>:i`, `-C`,
    /// `-l`, then the extra flags).
    pub fn arguments(&self) -> Vec<String> {
        let mut flags = vec![
            "-c".into(),
            self.programmer.into(),
            "-b".into(),
            self.baud_rate.clone(),
            "-p".into(),
            self.part.into(),
            "-P".into(),
            self.port.clone(),
            "-U".into(),
            format!("flash:w:{}:i", self.hex.display()),
            "-C".into(),
            self.conf.display().to_string(),
            "-l".into(),
            self.log.display().to_string(),
        ];
        flags.extend(self.extra_flags.iter().cloned());
        flags
    }
}

/// Runs avrdude: `Ok(status)` when a result was read, `Err` (with the error text) when the call
/// fails outright.
pub trait Avrdude {
    fn flash(&mut self, invocation: &AvrdudeInvocation) -> Result<String, String>;
}

/// The status a log line reports (tested after removing `.exe` from the line).
pub fn log_line_status(line: &str) -> Option<&'static str> {
    let line = line.replace(".exe", "");
    if line.contains("avrdude: stk500_recv(): programmer is not responding") {
        Some("wrong_baud_rate")
    } else if line.contains("avrdude: ser_open(): can") {
        Some("access_denied")
    } else if line.contains("avrdude: can't open input file") {
        Some("firmware_not_found")
    } else if line.contains("avrdude done.  Thank you") {
        Some("success")
    } else {
        None
    }
}

/// Runs the avrdude program ([`AVRDUDE_PROGRAM`]) and reads its result from the log.
#[derive(Debug, Clone)]
pub struct SystemAvrdude {
    program: PathBuf,
}

impl Default for SystemAvrdude {
    fn default() -> Self {
        let executable = std::env::current_exe().ok();
        let appdir = std::env::var_os("APPDIR").map(PathBuf::from);
        let path = std::env::var_os("PATH").unwrap_or_default();
        SystemAvrdude {
            program: resolve_avrdude(appdir.as_deref(), executable.as_deref(), &path)
                .unwrap_or_else(|| PathBuf::from(AVRDUDE_PROGRAM)),
        }
    }
}

fn bundle_paths(appdir: Option<&Path>, executable: Option<&Path>, relative: &str) -> Vec<PathBuf> {
    appdir
        .map(|root| root.join("usr").join(relative))
        .into_iter()
        .chain(executable.and_then(Path::parent).and_then(Path::parent).map(|prefix| prefix.join(relative)))
        .collect()
}

fn executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else { return false };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Prefer the AppImage's matching native helper, then a normal system/PATH install.
pub fn resolve_avrdude(appdir: Option<&Path>, executable: Option<&Path>, path: &OsStr) -> Option<PathBuf> {
    bundle_paths(appdir, executable, "lib/openrigcue/avrdude")
        .into_iter()
        .chain(std::env::split_paths(path).map(|dir| dir.join("avrdude")))
        .find(|path| executable_file(path))
        .and_then(|path| std::path::absolute(path).ok())
}

/// Always pass -C explicitly; native helpers must not depend on a host /etc file.
pub fn bundled_avrdude_conf(appdir: Option<&Path>, executable: Option<&Path>) -> Option<PathBuf> {
    bundle_paths(appdir, executable, "share/openrigcue/avrdude.conf")
        .into_iter()
        .find(|path| path.is_file())
        .and_then(|path| std::path::absolute(path).ok())
}

impl SystemAvrdude {
    pub fn new(program: &Path) -> Self {
        SystemAvrdude { program: program.to_path_buf() }
    }
}

impl Avrdude for SystemAvrdude {
    /// Without the program the attempt fails with "avrdude not installed". Otherwise avrdude runs with
    /// the image's folder as working directory: the log is removed,
    /// avrdude started without waiting, then the log is read again and again (1 s apart while it cannot be
    /// opened) until a line reports a status; avrdude is then terminated, and 1 s later the log removed.
    /// An avrdude process which exits without a known result, or runs longer than two minutes,
    /// fails the attempt instead of leaving the serial-device scan stuck indefinitely.
    fn flash(&mut self, invocation: &AvrdudeInvocation) -> Result<String, String> {
        self.run(invocation, Duration::from_secs(1), Duration::from_secs(120))
    }
}

impl SystemAvrdude {
    fn run(&mut self, invocation: &AvrdudeInvocation, poll: Duration, limit: Duration) -> Result<String, String> {
        if !self.program.exists() {
            return Err("avrdude not installed".into());
        }
        let _ = std::fs::remove_file(&invocation.log);
        let mut command = Command::new(&self.program);
        command.args(invocation.arguments());
        if let Some(dir) = invocation.hex.parent() {
            command.current_dir(dir);
        }
        debug!("{} {}", self.program.display(), invocation.arguments().join(" "));
        let mut child = command.spawn().map_err(|e| e.to_string())?;
        let started = Instant::now();
        let status = loop {
            let text = std::fs::read_to_string(&invocation.log).unwrap_or_default();
            if let Some(status) = text.lines().find_map(log_line_status) {
                break Ok(status.to_owned());
            }
            match child.try_wait() {
                Ok(Some(exit)) => {
                    // The final log write may land between the read and try_wait.
                    let text = std::fs::read_to_string(&invocation.log).unwrap_or(text);
                    if let Some(status) = text.lines().find_map(log_line_status) {
                        break Ok(status.to_owned());
                    }
                    let detail = text.lines().last().unwrap_or("no result log");
                    break Err(format!("avrdude exited ({exit}): {detail}"));
                }
                Err(error) => break Err(error.to_string()),
                Ok(None) => {}
            }
            if started.elapsed() >= limit {
                break Err("avrdude update timed out".into());
            }
            std::thread::sleep(poll);
        };
        if child.kill().is_err() {
            debug!("cannot terminate");
        }
        let _ = child.wait();
        std::thread::sleep(poll);
        if let Err(e) = std::fs::remove_file(&invocation.log) {
            debug!("error while deleting file {e}");
        }
        status
    }
}

/// Flasher state that outlives a single attempt: the image, the avrdude configuration and the bootloader speed
/// to use next (toggled after every attempt).
#[derive(Debug, Clone)]
pub struct IntelliboxFlasher {
    hex: PathBuf,
    conf: PathBuf,
    installer: Option<PathBuf>,
    native_conf: Option<PathBuf>,
    baud_rate: u32,
}

impl IntelliboxFlasher {
    /// The flasher of the application installed at `srs_app_path` ([`installed_hex_path`],
    /// [`installed_conf_path`]).
    pub fn installed(srs_app_path: &Path) -> Self {
        IntelliboxFlasher {
            hex: installed_hex_path(srs_app_path),
            conf: installed_conf_path(srs_app_path),
            installer: None,
            native_conf: None,
            baud_rate: INITIAL_BAUD_RATE,
        }
    }

    /// The flasher for the image `hex`; the avrdude configuration is found
    /// relative to the application folder holding the image (`<app>/firmware/intellibox/intellibox.hex`,
    /// `<app>/avr_helpers/avrdude.conf`).
    pub fn new(hex: &Path) -> Self {
        let app = hex.ancestors().nth(3).unwrap_or(Path::new(""));
        IntelliboxFlasher {
            hex: hex.to_path_buf(),
            conf: installed_conf_path(app),
            installer: None,
            native_conf: None,
            baud_rate: INITIAL_BAUD_RATE,
        }
    }

    /// The approved import accepts an installed folder or the SRS installer.
    /// Installer files are read only when a mismatched device reaches flashing;
    /// only the image/configuration are extracted, never an executable.
    pub fn from_source(source: &Path, cache: &Path) -> Self {
        if !source.is_file() {
            return Self::installed(&std::path::absolute(source).unwrap_or_else(|_| source.to_path_buf()));
        }
        let cache = if cache.is_absolute() {
            cache.to_path_buf()
        } else {
            std::env::current_dir().map(|cwd| cwd.join(cache)).unwrap_or_else(|_| cache.to_path_buf())
        };
        let mut flasher = Self::installed(&cache);
        flasher.installer = Some(std::path::absolute(source).unwrap_or_else(|_| source.to_path_buf()));
        flasher
    }

    pub fn with_native_conf(mut self, path: Option<PathBuf>) -> Self {
        self.native_conf = path;
        self
    }

    fn prepare_import(&self) -> Result<PathBuf, String> {
        let Some(installer) = &self.installer else {
            return Ok(if self.conf.is_file() {
                self.conf.clone()
            } else {
                self.native_conf.clone().unwrap_or_else(|| self.conf.clone())
            });
        };
        use crate::shakers::nsis::NsisArchive;
        let archive = NsisArchive::open(std::fs::read(installer).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?;
        // Resolve both before writing either; a partial archive never reaches avrdude.
        let hex = archive.read("firmware/intellibox/intellibox.hex").map_err(|error| error.to_string())?;
        let conf = match archive.read("avr_helpers/avrdude.conf") {
            Ok(conf) => Some(conf),
            Err(crate::shakers::nsis::NsisError::Missing(_)) if self.native_conf.is_some() => None,
            Err(error) => return Err(error.to_string()),
        };
        let config = if conf.is_some() {
            self.conf.clone()
        } else {
            self.native_conf.clone().expect("missing archive config requires native fallback")
        };
        let files = std::iter::once((&self.hex, hex)).chain(conf.map(|conf| (&self.conf, conf)));
        for (path, bytes) in files {
            if std::fs::read(path).ok().as_deref() == Some(bytes.as_slice()) {
                continue;
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            std::fs::write(path, bytes).map_err(|error| error.to_string())?;
        }
        Ok(config)
    }

    /// Flash the box on `port`, reporting the sidebar status texts as they change; `log` is avrdude's log
    /// ([`log_path`]).
    ///
    /// The sequence: `New Firmware Found`, then
    /// `Flashing new firmware` and one avrdude attempt at the current speed (an outright failure is logged and
    /// leaves the status `will_see_what_happens`); then the status/pause sequence for `success` (`Flashing completed`,
    /// 1.5 s, `Reconnecting`, 1.5 s), for `wrong_baud_rate` (`1/2 steps completed`, 2 s, `Reconnecting`, 1 s)
    /// or for any other status (the translated `Failed to flash firmware (` followed by the status and `)`, 1.5 s,
    /// `Trying again`, 1.5 s); finally the speed becomes 115200 when it was 57600, else 57600.
    pub fn flash(
        &mut self,
        port: &str,
        avrdude: &mut dyn Avrdude,
        log: &Path,
        clock: &dyn Clock,
        translate: &dyn Fn(&str) -> String,
        status: &mut dyn FnMut(&str),
    ) -> FlashOutcome {
        status("Firmware update available");
        debug!("Trying to flash firmware on the intellibox");
        status("Installing new firmware");
        let mut invocation = AvrdudeInvocation {
            programmer: "arduino",
            part: "atmega328p",
            baud_rate: self.baud_rate.to_string(),
            port: port.to_string(),
            hex: self.hex.clone(),
            conf: self.conf.clone(),
            log: log.to_path_buf(),
            extra_flags: vec!["-D".into()],
        };
        let flashed = match self.prepare_import().and_then(|conf| {
            invocation.conf = conf;
            avrdude.flash(&invocation)
        }) {
            Ok(flashed) => flashed,
            Err(e) => {
                debug!("error while trying to flash firmware{e}");
                STATUS_UNSET.to_owned()
            }
        };
        let outcome = FlashOutcome::from_status(&flashed);
        let secs = Duration::from_secs_f64;
        match &outcome {
            FlashOutcome::Success => {
                status("Firmware installation finished");
                clock.sleep(secs(1.5));
                status("Reconnecting");
                clock.sleep(secs(1.5));
            }
            FlashOutcome::WrongBaudRate => {
                status("Step 1 of 2 done");
                clock.sleep(secs(2.0));
                status("Reconnecting");
                clock.sleep(secs(1.0));
            }
            FlashOutcome::Failed(reason) => {
                status(&format!("{}{reason})", translate("Firmware could not be flashed (")));
                clock.sleep(secs(1.5));
                status("Retrying");
                clock.sleep(secs(1.5));
            }
        }
        self.baud_rate = if self.baud_rate == 57_600 { 115_200 } else { 57_600 };
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::super::link::fakes::ManualClock;
    use super::*;

    #[test]
    fn bundled_firmware_lookup_checks_the_requested_image_and_install_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let appdir = tmp.path().join("appimage");
        let native = tmp.path().join("native/bin");
        let relative = Path::new("firmware/intellibox/intellibox.hex");
        let app_image = appdir.join("usr/bin").join(relative);
        let native_image = native.join(relative);
        std::fs::create_dir_all(app_image.parent().unwrap()).unwrap();
        std::fs::create_dir_all(native_image.parent().unwrap()).unwrap();
        std::fs::write(&native_image, ":00000001FF\n").unwrap();
        let executable = native.join("openrigcue");
        assert_eq!(bundled_firmware_root(Some(&appdir), Some(&executable), relative), Some(native.clone()));
        std::fs::write(&app_image, ":00000001FF\n").unwrap();
        assert_eq!(bundled_firmware_root(Some(&appdir), Some(&executable), relative), Some(appdir.join("usr/bin")));
        assert_eq!(
            bundled_firmware_root(Some(&appdir), Some(&executable), Path::new("firmware/obd2hud/firmware.bin")),
            None
        );
        std::fs::remove_file(app_image).unwrap();
        std::fs::remove_file(native_image).unwrap();
        assert_eq!(bundled_firmware_root(Some(&appdir), Some(&executable), relative), None);
    }

    struct Scripted(Vec<Result<String, String>>, Vec<AvrdudeInvocation>);

    impl Avrdude for Scripted {
        fn flash(&mut self, invocation: &AvrdudeInvocation) -> Result<String, String> {
            self.1.push(invocation.clone());
            self.0.remove(0)
        }
    }

    fn statuses(results: &[&str]) -> Vec<Result<String, String>> {
        results.iter().map(|s| Ok((*s).to_owned())).collect()
    }

    fn installer(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut strings = vec![0u16];
        let mut entries = Vec::new();
        let mut payload = Vec::new();
        for (path, bytes) in files {
            let (directory, name) = path.rsplit_once('/').unwrap();
            let directory_at = strings.len() as u32;
            strings.extend([3, 0x8095]); // NSIS $INSTDIR
            strings.extend(format!("\\{}", directory.replace('/', "\\")).encode_utf16().chain([0]));
            let name_at = strings.len() as u32;
            strings.extend(name.encode_utf16().chain([0]));
            for (op, parameters) in
                [(11u32, [directory_at, 1, 0, 0, 0, 0]), (20, [0, name_at, payload.len() as u32, 0, 0, 0])]
            {
                entries.extend(std::iter::once(op).chain(parameters).flat_map(u32::to_le_bytes));
            }
            payload.extend((bytes.len() as u32).to_le_bytes());
            payload.extend_from_slice(bytes);
        }
        let strings: Vec<u8> = strings.into_iter().flat_map(u16::to_le_bytes).collect();
        let entries_at = 68u32;
        let strings_at = entries_at + entries.len() as u32;
        let end = strings_at + strings.len() as u32;
        let mut header = vec![0; 4];
        for (offset, count) in [
            (0, 0),
            (0, 0),
            (entries_at, (files.len() * 2) as u32),
            (strings_at, 0),
            (end, 0),
            (end, 0),
            (end, 0),
            (end, 0),
        ] {
            header.extend(u32::to_le_bytes(offset));
            header.extend(u32::to_le_bytes(count));
        }
        header.extend(entries);
        header.extend(strings);
        let mut image = vec![b'M'; 512];
        image.extend(0u32.to_le_bytes());
        image.extend(0xDEAD_BEEFu32.to_le_bytes());
        image.extend(b"NullsoftInst");
        image.extend((header.len() as u32).to_le_bytes());
        image.extend((28 + 4 + header.len() as u32 + payload.len() as u32).to_le_bytes());
        image.extend((header.len() as u32).to_le_bytes());
        image.extend(header);
        image.extend(payload);
        image
    }

    #[test]
    fn installer_import_provides_both_files_to_the_reached_fake_flash() {
        let temp = tempfile::tempdir().unwrap();
        let setup = temp.path().join("SRS setup.exe");
        let hex = b":00000001FF\r\n";
        let conf = b"# fake avrdude configuration\n";
        std::fs::write(
            &setup,
            installer(&[("firmware/intellibox/intellibox.hex", hex), ("avr_helpers/avrdude.conf", conf)]),
        )
        .unwrap();
        let cache = temp.path().join("import cache");
        let mut flasher = IntelliboxFlasher::from_source(&setup, &cache);
        assert!(!cache.exists(), "constructing a scanner does not extract files");
        let mut runner = Scripted(statuses(&["success"]), vec![]);
        let clock = ManualClock::default();
        let result = flasher.flash(
            "/fake/serial",
            &mut runner,
            &temp.path().join("avrdude.log"),
            &clock,
            &str::to_owned,
            &mut |_| {},
        );
        assert_eq!(result, FlashOutcome::Success);
        let invocation = &runner.1[0];
        assert_eq!(std::fs::read(&invocation.hex).unwrap(), hex);
        assert_eq!(std::fs::read(&invocation.conf).unwrap(), conf);
        assert!(!invocation.hex.starts_with(&setup), "the installer is not a directory");
    }

    #[test]
    fn installer_failure_retains_status_baud_and_retry_before_fake_invocation() {
        let temp = tempfile::tempdir().unwrap();
        let setup = temp.path().join("setup.exe");
        let hex = b":00000001FF\n";
        std::fs::write(&setup, installer(&[("firmware/intellibox/intellibox.hex", hex)])).unwrap();
        let cache = temp.path().join("cache");
        let mut flasher = IntelliboxFlasher::from_source(&setup, &cache);
        let mut runner = Scripted(statuses(&["success"]), vec![]);
        let clock = ManualClock::default();
        let mut seen = Vec::new();
        let log = temp.path().join("avrdude.log");
        assert_eq!(
            flasher.flash("/fake/serial", &mut runner, &log, &clock, &str::to_owned, &mut |s| seen.push(s.to_owned())),
            FlashOutcome::Failed(STATUS_UNSET.into())
        );
        assert!(runner.1.is_empty());
        assert!(!cache.exists(), "missing configuration fails before cache or runner mutation");
        assert_eq!(
            seen,
            [
                "Firmware update available",
                "Installing new firmware",
                "Firmware could not be flashed (will_see_what_happens)",
                "Retrying"
            ]
        );
        assert_eq!(clock.elapsed(), Duration::from_secs(3));
        std::fs::write(
            &setup,
            installer(&[("firmware/intellibox/intellibox.hex", hex), ("avr_helpers/avrdude.conf", b"valid config")]),
        )
        .unwrap();
        assert_eq!(
            flasher.flash("/fake/serial", &mut runner, &log, &clock, &str::to_owned, &mut |_| {}),
            FlashOutcome::Success
        );
        assert_eq!(runner.1[0].baud_rate, "57600");
    }

    #[test]
    fn native_config_fallback_never_reuses_a_stale_imported_config() {
        let temp = tempfile::tempdir().unwrap();
        let setup = temp.path().join("setup.exe");
        let cache = temp.path().join("cache");
        let native = temp.path().join("native.conf");
        std::fs::write(&native, b"native config").unwrap();
        let files: &[(&str, &[u8])] = &[
            ("firmware/intellibox/intellibox.hex", b":00000001FF\n"),
            ("avr_helpers/avrdude.conf", b"imported config"),
        ];
        std::fs::write(&setup, installer(files)).unwrap();
        let mut flasher = IntelliboxFlasher::from_source(&setup, &cache).with_native_conf(Some(native.clone()));
        let mut runner = Scripted(statuses(&["success", "success"]), vec![]);
        let clock = ManualClock::default();
        flasher.flash("/fake/serial", &mut runner, &temp.path().join("log"), &clock, &str::to_owned, &mut |_| {});
        assert_eq!(std::fs::read(&runner.1[0].conf).unwrap(), b"imported config");
        std::fs::write(&setup, installer(&files[..1])).unwrap();
        flasher.flash("/fake/serial", &mut runner, &temp.path().join("log"), &clock, &str::to_owned, &mut |_| {});
        assert_eq!(runner.1[1].conf, native);
    }

    #[test]
    fn installed_folder_keeps_its_configuration_and_never_uses_import_cache() {
        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path().join("original installed folder");
        let hex = installed_hex_path(&folder);
        let conf = installed_conf_path(&folder);
        std::fs::create_dir_all(hex.parent().unwrap()).unwrap();
        std::fs::create_dir_all(conf.parent().unwrap()).unwrap();
        std::fs::write(&hex, b":00000001FF\n").unwrap();
        std::fs::write(&conf, b"custom imported config").unwrap();
        let native = temp.path().join("native.conf");
        std::fs::write(&native, b"native fallback").unwrap();
        let cache = temp.path().join("unused cache");
        let mut flasher = IntelliboxFlasher::from_source(&folder, &cache).with_native_conf(Some(native));
        let mut runner = Scripted(statuses(&["success"]), vec![]);
        flasher.flash(
            "/fake/serial",
            &mut runner,
            &temp.path().join("log"),
            &ManualClock::default(),
            &str::to_owned,
            &mut |_| {},
        );
        assert_eq!(runner.1[0].hex, hex);
        assert_eq!(runner.1[0].conf, conf);
        assert!(!cache.exists());
    }

    #[cfg(unix)]
    #[test]
    fn helper_and_config_discovery_prefers_bundle_then_installed_then_path() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let appdir = temp.path().join("App Image mount");
        let prefix = temp.path().join("installed prefix");
        let system = temp.path().join("system bin");
        let bundled = appdir.join("usr/lib/openrigcue/avrdude");
        let installed = prefix.join("lib/openrigcue/avrdude");
        let path = system.join("avrdude");
        for file in [&bundled, &installed, &path] {
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, b"fake helper").unwrap();
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let executable = prefix.join("bin/openrigcue");
        let search = std::env::join_paths([&system]).unwrap();
        assert_eq!(resolve_avrdude(Some(&appdir), Some(&executable), &search), Some(bundled.clone()));
        std::fs::remove_file(&bundled).unwrap();
        assert_eq!(
            resolve_avrdude(Some(&appdir), Some(&executable), &search).unwrap().canonicalize().unwrap(),
            installed.canonicalize().unwrap()
        );
        std::fs::remove_file(installed).unwrap();
        assert_eq!(resolve_avrdude(Some(&appdir), Some(&executable), &search), Some(path));
        let config = appdir.join("usr/share/openrigcue/avrdude.conf");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(&config, b"native config").unwrap();
        assert_eq!(bundled_avrdude_conf(Some(&appdir), Some(&executable)), Some(config));
    }

    #[cfg(unix)]
    #[test]
    fn exited_fake_helper_reports_unrecognized_error_without_waiting_forever() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let helper = temp.path().join("avrdude");
        std::fs::write(&helper, b"#!/bin/sh\nwhile [ \"$1\" != \"-l\" ]; do shift; done\nshift\nprintf 'bad fake configuration\\n' > \"$1\"\nexit 3\n").unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        let invocation = AvrdudeInvocation {
            programmer: "arduino",
            part: "atmega328p",
            baud_rate: "115200".into(),
            port: "/fake/no-serial".into(),
            hex: temp.path().join("image.hex"),
            conf: temp.path().join("config"),
            log: temp.path().join("log"),
            extra_flags: vec![],
        };
        let started = Instant::now();
        let result = SystemAvrdude::new(&helper).flash(&invocation);
        assert!(result.unwrap_err().contains("bad fake configuration"));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(!invocation.log.exists());
    }

    #[cfg(unix)]
    #[test]
    fn stalled_fake_helper_is_bounded_and_known_results_keep_priority() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let helper = temp.path().join("avrdude");
        std::fs::write(&helper, b"#!/bin/sh\nexec sleep 60\n").unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        let invocation = AvrdudeInvocation {
            programmer: "arduino",
            part: "atmega328p",
            baud_rate: "115200".into(),
            port: "/fake/no-serial".into(),
            hex: temp.path().join("image.hex"),
            conf: temp.path().join("config"),
            log: temp.path().join("log"),
            extra_flags: vec![],
        };
        let mut runner = SystemAvrdude::new(&helper);
        let started = Instant::now();
        assert_eq!(
            runner.run(&invocation, Duration::from_millis(2), Duration::from_millis(20)),
            Err("avrdude update timed out".into())
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        std::fs::write(&helper, b"#!/bin/sh\nwhile [ \"$1\" != \"-l\" ]; do shift; done\nshift\nprintf 'avrdude done.  Thank you.\\n' > \"$1\"\nexit 1\n").unwrap();
        assert_eq!(runner.run(&invocation, Duration::from_millis(2), Duration::from_secs(2)), Ok("success".into()));
        assert!(!invocation.log.exists());
    }

    /// Flashing: statuses and pauses of each branch, the alternating bootloader speed
    /// (115200 first) and the avrdude command line.
    #[test]
    fn flash_sequence_and_baud_toggle() {
        let clock = ManualClock::default();
        let mut runner = Scripted(statuses(&["access_denied", "success", "wrong_baud_rate"]), vec![]);
        let app = Path::new("/opt/srs");
        let mut flasher = IntelliboxFlasher::installed(app);
        let log = log_path(Path::new("/home/u/Documents/SimRacingStudio 2.0"));
        let mut seen = Vec::new();
        let english = |text: &str| text.to_owned();
        flasher.flash("/dev/ttyUSB3", &mut runner, &log, &clock, &english, &mut |s| seen.push(s.to_string()));
        assert_eq!(
            seen,
            vec![
                "Firmware update available",
                "Installing new firmware",
                "Firmware could not be flashed (access_denied)",
                "Retrying"
            ]
        );
        assert_eq!(clock.elapsed(), Duration::from_secs(3));
        seen.clear();
        flasher.flash("/dev/ttyUSB3", &mut runner, &log, &clock, &english, &mut |s| seen.push(s.to_string()));
        assert_eq!(&seen[2..], ["Firmware installation finished", "Reconnecting"]);
        assert_eq!(clock.elapsed(), Duration::from_secs(6));
        seen.clear();
        flasher.flash("/dev/ttyUSB3", &mut runner, &log, &clock, &english, &mut |s| seen.push(s.to_string()));
        assert_eq!(&seen[2..], ["Step 1 of 2 done", "Reconnecting"]);
        assert_eq!(clock.elapsed(), Duration::from_secs(9));
        let bauds: Vec<&str> = runner.1.iter().map(|i| i.baud_rate.as_str()).collect();
        assert_eq!(bauds, ["115200", "57600", "115200"]);
        assert_eq!(
            runner.1[0].arguments().join(" "),
            "-c arduino -b 115200 -p atmega328p -P /dev/ttyUSB3 -U flash:w:/opt/srs/firmware/intellibox/intellibox.hex:i \
             -C /opt/srs/avr_helpers/avrdude.conf -l /home/u/Documents/SimRacingStudio 2.0/avrdude.log -D"
        );
    }

    /// An outright avrdude failure leaves the status 'will_see_what_happens'; only the
    /// prefix is translated, the status is appended as it is.
    #[test]
    fn raised_avrdude_and_translated_prefix() {
        let clock = ManualClock::default();
        let mut runner = Scripted(vec![Err("avrdude not installed".into())], vec![]);
        let mut flasher = IntelliboxFlasher::new(Path::new("/opt/srs/firmware/intellibox/intellibox.hex"));
        let german = |text: &str| {
            if text == "Firmware could not be flashed (" {
                "Flashen der Firmware fehlgeschlagen (".to_owned()
            } else {
                text.to_owned()
            }
        };
        let mut seen = Vec::new();
        flasher.flash("/dev/ttyUSB3", &mut runner, Path::new("/tmp/x.log"), &clock, &german, &mut |s| {
            seen.push(s.to_string())
        });
        assert_eq!(seen[2], "Flashen der Firmware fehlgeschlagen (will_see_what_happens)");
        assert_eq!(seen[3], "Retrying");
        assert_eq!(runner.1[0].conf, Path::new("/opt/srs/avr_helpers/avrdude.conf"));
    }

    /// Log status detection: the four log messages, `.exe` removed first, other lines ignored.
    #[test]
    fn log_statuses() {
        assert_eq!(
            log_line_status("avrdude.exe: stk500_recv(): programmer is not responding"),
            Some("wrong_baud_rate")
        );
        assert_eq!(log_line_status("avrdude: ser_open(): can't open device \"\\\\.\\COM3\""), Some("access_denied"));
        assert_eq!(log_line_status("avrdude: can't open input file x.hex"), Some("firmware_not_found"));
        assert_eq!(log_line_status("avrdude done.  Thank you."), Some("success"));
        assert_eq!(log_line_status("avrdude: stk500_getsync() attempt 1 of 10: not in sync"), None);
    }

    /// Without the program the attempt fails with "avrdude not installed".
    #[test]
    fn missing_avrdude_raises() {
        let dir = std::env::temp_dir().join(format!("openrigcue-avrdude-{}", std::process::id()));
        let mut avrdude = SystemAvrdude::new(&dir.join("avrdude"));
        let flasher = IntelliboxFlasher::installed(&dir);
        let invocation = AvrdudeInvocation {
            programmer: "arduino",
            part: "atmega328p",
            baud_rate: "115200".into(),
            port: "/dev/null".into(),
            hex: flasher.hex.clone(),
            conf: flasher.conf.clone(),
            log: dir.join("avrdude.log"),
            extra_flags: vec![],
        };
        assert_eq!(avrdude.flash(&invocation), Err("avrdude not installed".to_owned()));
    }
}
