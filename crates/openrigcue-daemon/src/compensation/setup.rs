//! Per-user installation of the bundled Linux VR consumers. No compiler or download is needed.
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use serde_json::json;

#[derive(Clone, Debug)]
enum Bundle {
    Packaged(PathBuf),
    Source { root: PathBuf, target: PathBuf },
}

#[derive(Clone, Debug)]
pub(crate) struct SetupPaths {
    bundles: Vec<Bundle>,
    data: PathBuf,
    config: PathBuf,
    registry_override: Option<PathBuf>,
    driver_config_override: Option<PathBuf>,
    registrar_timeout: std::time::Duration,
    steamvr: Option<PathBuf>,
    proc_root: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Component {
    OpenXr,
    SteamVr,
}
impl Component {
    pub fn name(self) -> &'static str {
        match self {
            Self::OpenXr => "openxr",
            Self::SteamVr => "steamvr",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "openxr" => Some(Self::OpenXr),
            "steamvr" => Some(Self::SteamVr),
            _ => None,
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct ComponentStatus {
    available: bool,
    installed: bool,
    registered: bool,
    active: bool,
    restart_needed: bool,
    error: Option<String>,
    update_available: bool,
    status: String,
    action: Option<&'static str>,
    path: PathBuf,
}
#[derive(Debug, Serialize)]
pub(crate) struct SetupStatus {
    pub(super) openxr: ComponentStatus,
    pub(super) steamvr: ComponentStatus,
    steamvr_runtime: Option<PathBuf>,
}

impl SetupStatus {
    pub(super) fn set_activity(&mut self, kind: Option<openrigcue_rigpose::ConsumerKind>) {
        for (component, status) in [(Component::OpenXr, &mut self.openxr), (Component::SteamVr, &mut self.steamvr)] {
            status.active = kind
                == Some(match component {
                    Component::OpenXr => openrigcue_rigpose::ConsumerKind::XR_LAYER,
                    Component::SteamVr => openrigcue_rigpose::ConsumerKind::STEAMVR_DRIVER,
                });
            status.restart_needed = status.registered && !status.active && component == Component::SteamVr;
        }
    }

    pub(super) fn set_error(&mut self, component: Component, error: String) {
        let status = match component {
            Component::OpenXr => &mut self.openxr,
            Component::SteamVr => &mut self.steamvr,
        };
        status.status = format!("Setup failed: {}", crate::i18n::plain(&error));
        status.error = Some(error);
        if status.registered {
            status.action = Some("Uninstall");
        }
    }

    /// Puts the failures into the language `translate` gives (a sentence with values is looked up and filled
    /// in): the status keeps its `Setup failed: ` start, which the page reads, and `error` is the shown text.
    pub(super) fn translate_errors(&mut self, translate: impl Fn(&str) -> String) {
        for status in [&mut self.openxr, &mut self.steamvr] {
            if let Some(error) = &mut status.error {
                *error = translate(error);
                status.status = format!("Setup failed: {error}");
            }
        }
    }
}

impl SetupPaths {
    pub fn discover() -> Self {
        let absolute = |path: PathBuf| {
            if path.is_absolute() {
                path
            } else {
                std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")).join(path)
            }
        };
        let home = absolute(std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default());
        let user_dir = |key: &str, default: &str| {
            std::env::var_os(key)
                .filter(|v| !v.is_empty())
                .map(|v| absolute(PathBuf::from(v)))
                .unwrap_or_else(|| home.join(default))
        };
        let mut bundles = Vec::new();
        if let Some(root) = std::env::var_os("OPENRIGCUE_VR_BUNDLE").filter(|v| !v.is_empty()) {
            bundles.push(Bundle::Packaged(root.into()));
        }
        if let Some(appdir) = std::env::var_os("APPDIR") {
            bundles.push(Bundle::Packaged(PathBuf::from(appdir).join("usr/share/openrigcue/vr")));
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(bin) = exe.parent() {
                bundles.push(Bundle::Packaged(bin.join("../share/openrigcue/vr")));
            }
        }
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let target = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from).unwrap_or_else(|| root.join("target"));
        bundles.push(Bundle::Source { root, target });
        let steamvr = std::env::var_os("STEAMVR")
            .map(|value| absolute(PathBuf::from(value)))
            .filter(|p| p.join("bin/linux64/vrpathreg").is_file())
            .or_else(|| {
                openrigcue_config::patch::SteamLibraries::discover(&home).and_then(|libs| {
                    libs.search_roots
                        .iter()
                        .flat_map(|root| [root.join("SteamVR"), root.join("steamapps/common/SteamVR")])
                        .find(|p| p.join("bin/linux64/vrpathreg").is_file())
                })
            });
        Self {
            bundles,
            data: user_dir("XDG_DATA_HOME", ".local/share"),
            config: user_dir("XDG_CONFIG_HOME", ".config"),
            registry_override: std::env::var_os("VR_PATHREG_OVERRIDE")
                .filter(|v| !v.is_empty())
                .map(|v| absolute(PathBuf::from(v))),
            driver_config_override: std::env::var_os("OPENRIGCUE_STEAMVR_CONFIG")
                .filter(|v| !v.is_empty())
                .map(|v| absolute(PathBuf::from(v))),
            registrar_timeout: std::time::Duration::from_secs(3),
            steamvr,
            proc_root: PathBuf::from("/proc"),
        }
    }

    fn driver_dir(&self) -> PathBuf {
        self.data.join("openrigcue/steamvr-driver/openrigcue")
    }
    fn layer_lib(&self) -> PathBuf {
        self.data.join("openrigcue/xr-layer/libopenrigcue_xr_layer.so")
    }
    fn layer_manifest(&self) -> PathBuf {
        self.data.join("openxr/1/api_layers/implicit.d/openrigcue_xr_layer.json")
    }
    fn registry(&self) -> PathBuf {
        self.registry_override.clone().unwrap_or_else(|| self.config.join("openvr/openvrpaths.vrpath"))
    }

    fn disabled_marker(&self, component: Component) -> PathBuf {
        self.config.join("openrigcue").join(format!("vr-{}-disabled", component.name()))
    }

    fn driver_config(&self) -> PathBuf {
        self.driver_config_override.clone().unwrap_or_else(|| self.config.join("openrigcue/steamvr-driver.toml"))
    }

    /// Source/destination pairs; only files of the selected native product are copied.
    fn artifacts(&self, component: Component) -> Option<Vec<(PathBuf, PathBuf)>> {
        self.bundles.iter().find_map(|bundle| {
            let files = match (bundle, component) {
                (Bundle::Packaged(root), Component::SteamVr) => {
                    let source = root.join("SteamVR/drivers/openrigcue");
                    [
                        "driver.vrdrivermanifest",
                        "bin/linux64/driver_openrigcue.so",
                        "resources/settings/default.vrsettings",
                        "resources/steamvr-driver.example.toml",
                    ]
                    .map(|name| (source.join(name), self.driver_dir().join(name)))
                    .to_vec()
                }
                (Bundle::Source { root, target }, Component::SteamVr) => {
                    let source = root.join("crates/openrigcue-steamvr-driver/driver/openrigcue");
                    let mut files = [
                        "driver.vrdrivermanifest",
                        "resources/settings/default.vrsettings",
                        "resources/steamvr-driver.example.toml",
                    ]
                    .map(|name| (source.join(name), self.driver_dir().join(name)))
                    .to_vec();
                    files.push((
                        target.join("release/libopenrigcue_steamvr_driver.so"),
                        self.driver_dir().join("bin/linux64/driver_openrigcue.so"),
                    ));
                    files
                }
                (Bundle::Packaged(root), Component::OpenXr) => vec![
                    (root.join("OpenXR/libopenrigcue_xr_layer.so"), self.layer_lib()),
                    (root.join("OpenXR/openrigcue_xr_layer.json.in"), self.layer_manifest()),
                ],
                (Bundle::Source { root, target }, Component::OpenXr) => vec![
                    (target.join("release/libopenrigcue_xr_layer.so"), self.layer_lib()),
                    (
                        root.join("crates/openrigcue-xr-layer/manifest/openrigcue_xr_layer.json.in"),
                        self.layer_manifest(),
                    ),
                ],
            };
            files.iter().all(|(source, _)| source.is_file()).then_some(files)
        })
    }

    fn prepared(&self, component: Component) -> Result<Vec<(PathBuf, Vec<u8>)>, String> {
        let files = self
            .artifacts(component)
            .ok_or("The Linux VR component is missing from this package. Use a complete OpenRigCue build.")?;
        files
            .into_iter()
            .map(|(source, destination)| {
                let mut bytes = fs::read(&source).map_err(|e| {
                    failed("Cannot read ^path^: ^error^", &[("path", &source.display()), ("error", &e)])
                })?;
                if matches!(component, Component::OpenXr) && destination == self.layer_manifest() {
                    let mut value: serde_json::Value = serde_json::from_slice(&bytes)
                        .map_err(|e| failed("Invalid OpenXR manifest: ^error^", &[("error", &e)]))?;
                    value["api_layer"]["library_path"] = json!(self.layer_lib());
                    // Native application setup is always on. The loader still honors
                    // disable_environment, and explicit UI opt-outs remove this manifest.
                    value["api_layer"].as_object_mut().expect("template api_layer").remove("enable_environment");
                    bytes = serde_json::to_vec_pretty(&value).map_err(|e| e.to_string())?;
                }
                Ok((destination, bytes))
            })
            .collect()
    }

    fn registered(&self) -> bool {
        fs::read(self.registry())
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|value| value["external_drivers"].as_array().cloned())
            .is_some_and(|paths| paths.iter().filter_map(|p| p.as_str()).any(|p| Path::new(p) == self.driver_dir()))
    }

    fn installed(&self, component: Component) -> bool {
        match component {
            Component::SteamVr => {
                [
                    "driver.vrdrivermanifest",
                    "bin/linux64/driver_openrigcue.so",
                    "resources/settings/default.vrsettings",
                    "resources/steamvr-driver.example.toml",
                ]
                .iter()
                .all(|name| self.driver_dir().join(name).is_file())
                    && fs::read(self.driver_dir().join("driver.vrdrivermanifest"))
                        .ok()
                        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                        .is_some_and(|manifest| manifest["name"] == "openrigcue")
            }
            Component::OpenXr => self.layer_lib().is_file(),
        }
    }

    fn installed_openxr_manifest(&self) -> Option<serde_json::Value> {
        let value: serde_json::Value = serde_json::from_slice(&fs::read(self.layer_manifest()).ok()?).ok()?;
        let layer = value.get("api_layer")?.as_object()?;
        (layer.get("name")?.as_str()? == "XR_APILAYER_OPENRIGCUE_motion_compensation"
            && Path::new(layer.get("library_path")?.as_str()?) == self.layer_lib()
            && layer.get("enable_environment").is_none_or(|key| key == "OPENRIGCUE_XR_LAYER_ENABLE")
            && layer.get("disable_environment").is_some_and(|key| key == "OPENRIGCUE_XR_LAYER_DISABLE"))
        .then_some(value)
    }

    fn component_status(&self, component: Component) -> ComponentStatus {
        let available = self.artifacts(component).is_some();
        let installed = self.installed(component);
        let registered = match component {
            Component::OpenXr => self.installed_openxr_manifest().is_some(),
            Component::SteamVr => self.registered(),
        };
        let update_available = installed
            && self
                .prepared(component)
                .is_ok_and(|files| files.iter().any(|(path, bytes)| fs::read(path).ok().as_ref() != Some(bytes)));
        let runtime_missing = matches!(component, Component::SteamVr) && self.steamvr.is_none();
        let status = if registered {
            "Installed"
        } else if !available {
            "Component unavailable"
        } else if runtime_missing {
            "SteamVR not found"
        } else {
            "Not installed"
        };
        ComponentStatus {
            available,
            installed,
            registered,
            active: false,
            restart_needed: component == Component::SteamVr && registered,
            error: None,
            update_available,
            status: status.into(),
            action: if registered { Some("Uninstall") } else { (available && !runtime_missing).then_some("Install") },
            path: match component {
                Component::SteamVr => self.driver_dir(),
                Component::OpenXr => self.layer_lib(),
            },
        }
    }

    pub fn status(&self) -> SetupStatus {
        SetupStatus {
            openxr: self.component_status(Component::OpenXr),
            steamvr: self.component_status(Component::SteamVr),
            steamvr_runtime: self.steamvr.clone(),
        }
    }

    /// Keep the bundled consumers ready independently of external runtime registration.
    pub fn startup(&self) -> Vec<(Component, String)> {
        let mut errors = Vec::new();
        for component in [Component::OpenXr, Component::SteamVr] {
            // Earlier builds offered uninstall. The current product keeps its own components present.
            if let Err(error) =
                remove_if_present(&self.disabled_marker(component)).and_then(|_| self.deploy_internal(component))
            {
                errors.push((component, error));
            }
        }
        errors
    }

    /// Install into the selected third-party runtime using the already-bundled component.
    pub fn install(&self, component: Component) -> Result<SetupStatus, String> {
        self.deploy_internal(component)?;
        match component {
            Component::OpenXr => {
                let (_, manifest) = self
                    .prepared(component)?
                    .into_iter()
                    .find(|(path, _)| *path == self.layer_manifest())
                    .ok_or("OpenXR registration is missing from this package")?;
                write_atomic(&self.layer_manifest(), &manifest)?;
                if self.installed_openxr_manifest().is_none() {
                    return Err("OpenXR registration could not be verified".into());
                }
            }
            Component::SteamVr => {
                if self.steamvr.is_none() {
                    return Err("SteamVR was not found".into());
                }
                if !self.registered() {
                    self.register_driver("adddriver")?;
                }
                if !self.registered() {
                    return Err("SteamVR did not confirm the driver registration".into());
                }
            }
        }
        if component == Component::SteamVr {
            self.refresh_steamvr()?;
        }
        Ok(self.status())
    }

    fn deploy_internal(&self, component: Component) -> Result<(), String> {
        for (path, bytes) in self.prepared(component)? {
            if component == Component::OpenXr && path == self.layer_manifest() {
                continue;
            }
            write_atomic(&path, &bytes)?;
        }
        if component == Component::SteamVr {
            let config = self.driver_config();
            if !config.exists() {
                let example = fs::read(self.driver_dir().join("resources/steamvr-driver.example.toml"))
                    .map_err(|e| e.to_string())?;
                write_atomic(&config, &example)?;
            }
            self.set_driver_enabled(true)?;
        }
        if !self.installed(component) {
            return Err("Bundled VR component could not be verified".into());
        }
        Ok(())
    }

    /// Remove only the selected runtime registration; bundled components remain ready.
    pub fn uninstall(&self, component: Component) -> Result<SetupStatus, String> {
        match component {
            Component::OpenXr => remove_if_present(&self.layer_manifest())?,
            Component::SteamVr if self.registered() => {
                self.register_driver("removedriver")?;
                if self.registered() {
                    return Err("SteamVR did not remove the driver registration".into());
                }
            }
            Component::SteamVr => {}
        }
        if component == Component::SteamVr {
            self.refresh_steamvr()?;
        }
        Ok(self.status())
    }

    fn register_driver(&self, operation: &str) -> Result<(), String> {
        let registrar = self
            .steamvr
            .as_ref()
            .ok_or("SteamVR was not found; registration cannot be changed yet.")?
            .join("bin/linux64/vrpathreg");
        // Valve's shell wrapper has unquoted paths. Invoke the native utility.
        let library_dir = registrar.parent().expect("native registrar directory");
        let mut libraries = vec![library_dir.to_path_buf(), library_dir.join("qt/lib")];
        if let Some(existing) = std::env::var_os("LD_LIBRARY_PATH") {
            libraries.extend(std::env::split_paths(&existing));
        }
        let library_path = std::env::join_paths(libraries).map_err(|e| e.to_string())?;
        let mut command = Command::new(&registrar);
        command
            .arg(operation)
            .arg(self.driver_dir())
            .env("LD_LIBRARY_PATH", library_path)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("VR_PATHREG_OVERRIDE", self.registry());
        self.run_external(command, "registration")
    }

    /// Valve's own dashboard invokes vrmonitor://restartsystem. Send the same
    /// command to this runtime only when its matching vrserver is already running.
    fn refresh_steamvr(&self) -> Result<(), String> {
        let Some(runtime) = &self.steamvr else { return Ok(()) };
        let server = runtime.join("bin/linux64/vrserver");
        let running = fs::read_dir(&self.proc_root).ok().into_iter().flatten().flatten().any(|entry| {
            if !entry.file_name().to_string_lossy().bytes().all(|byte| byte.is_ascii_digit()) {
                return false;
            }
            let Ok(executable) = fs::read_link(entry.path().join("exe")) else {
                return false;
            };
            executable == server
                || fs::canonicalize(&executable)
                    .ok()
                    .zip(fs::canonicalize(&server).ok())
                    .is_some_and(|(first, second)| first == second)
        });
        if !running {
            return Ok(());
        }
        let monitor = runtime.join("bin/vrmonitor.sh");
        if !monitor.is_file() {
            return Err("SteamVR restart command is unavailable".into());
        }
        let mut command = Command::new(&monitor);
        command
            .arg("vrmonitor://restartsystem")
            .current_dir(runtime)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("VR_PATHREG_OVERRIDE", self.registry());
        self.run_external(command, "restart")
    }

    fn run_external(&self, mut command: Command, operation: &str) -> Result<(), String> {
        // A stalled native utility must not hold application startup indefinitely.
        // A temporary regular file avoids a filled stderr pipe deadlocking the child.
        static NEXT_LOG: AtomicU64 = AtomicU64::new(0);
        let log_path = std::env::temp_dir().join(format!(
            "openrigcue-vr-register-{}-{}",
            std::process::id(),
            NEXT_LOG.fetch_add(1, Ordering::Relaxed)
        ));
        let log = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&log_path)
            .map_err(|e| failed("Cannot capture registration output: ^error^", &[("error", &e)]))?;
        fs::remove_file(&log_path)
            .map_err(|e| failed("Cannot unlink registration output: ^error^", &[("error", &e)]))?;
        let mut child =
            command.stdout(Stdio::null()).stderr(log.try_clone().map_err(|e| e.to_string())?).spawn().map_err(|e| {
                failed("Cannot run SteamVR ^operation^: ^error^", &[("operation", &operation), ("error", &e)])
            })?;
        let deadline = std::time::Instant::now() + self.registrar_timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10))
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(failed("SteamVR ^operation^ timed out", &[("operation", &operation)]));
                }
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(failed(
                        "Cannot wait for SteamVR ^operation^: ^error^",
                        &[("operation", &operation), ("error", &error)],
                    ));
                }
            }
        };
        if !status.success() {
            use std::io::{Read, Seek};
            let mut log = log;
            log.rewind().map_err(|e| e.to_string())?;
            let mut detail = String::new();
            log.take(8192).read_to_string(&mut detail).map_err(|e| e.to_string())?;
            return Err(failed(
                "SteamVR ^operation^ failed (^status^): ^output^",
                &[("operation", &operation), ("status", &status), ("output", &detail.trim())],
            ));
        }
        Ok(())
    }

    fn set_driver_enabled(&self, enabled: bool) -> Result<(), String> {
        let path = self.driver_config();
        let mut value: toml::Value = match fs::read_to_string(&path) {
            Ok(text) => {
                toml::from_str(&text).map_err(|e| failed("Cannot read driver settings: ^error^", &[("error", &e)]))?
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => toml::Value::Table(Default::default()),
            Err(e) => return Err(failed("Cannot read ^path^: ^error^", &[("path", &path.display()), ("error", &e)])),
        };
        if value.get("enabled").and_then(toml::Value::as_bool) == Some(enabled) {
            return Ok(());
        }
        value.as_table_mut().ok_or("Driver settings must be a TOML table")?.insert("enabled".into(), enabled.into());
        write_atomic(&path, toml::to_string_pretty(&value).map_err(|e| e.to_string())?.as_bytes())
    }
}

fn remove_if_present(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(failed("Cannot remove ^path^: ^error^", &[("path", &path.display()), ("error", &e)])),
    }
}

/// A failure as a sentence with its values ([`crate::i18n::sentence`]): the UI shows the sentence in the selected
/// language with the path, the program's output or the system's error text filled in as they are.
fn failed(text: &str, values: &[(&str, &dyn std::fmt::Display)]) -> String {
    let values: Vec<(&str, String)> = values.iter().map(|(name, value)| (*name, value.to_string())).collect();
    let values: Vec<(&str, &str)> = values.iter().map(|(name, value)| (*name, value.as_str())).collect();
    crate::i18n::sentence(text, &values)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    if fs::read(path).is_ok_and(|existing| existing == bytes) {
        return Ok(());
    }
    let parent = path.parent().ok_or("Installation path has no parent")?;
    fs::create_dir_all(parent)
        .map_err(|e| failed("Cannot create ^path^: ^error^", &[("path", &parent.display()), ("error", &e)]))?;
    let temp =
        parent.join(format!(".openrigcue-install-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    let result = (|| -> io::Result<()> {
        let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(0o644).open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.map_err(|e| failed("Cannot install ^path^: ^error^", &[("path", &path.display()), ("error", &e)]))
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    pub(crate) fn fixture(root: &Path) -> SetupPaths {
        let bundle = root.join("app mount with spaces/usr/share/openrigcue/vr");
        for (name, bytes) in [
            ("SteamVR/drivers/openrigcue/driver.vrdrivermanifest", b"{\"name\":\"openrigcue\"}".as_slice()),
            ("SteamVR/drivers/openrigcue/bin/linux64/driver_openrigcue.so", b"native driver"),
            ("SteamVR/drivers/openrigcue/resources/settings/default.vrsettings", b"{}"),
            ("SteamVR/drivers/openrigcue/resources/steamvr-driver.example.toml", b"enabled = true"),
            ("OpenXR/libopenrigcue_xr_layer.so", b"native layer"),
            (
                "OpenXR/openrigcue_xr_layer.json.in",
                include_bytes!("../../../openrigcue-xr-layer/manifest/openrigcue_xr_layer.json.in").as_slice(),
            ),
        ] {
            let file = bundle.join(name);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, bytes).unwrap();
        }
        let paths = SetupPaths {
            bundles: vec![Bundle::Packaged(bundle)],
            data: root.join("persistent user data & quoted"),
            config: root.join("config"),
            registry_override: None,
            driver_config_override: None,
            registrar_timeout: std::time::Duration::from_secs(3),
            steamvr: Some(root.join("Steam library with spaces/SteamVR")),
            proc_root: root.join("proc"),
        };
        let registrar = paths.steamvr.as_ref().unwrap().join("bin/linux64/vrpathreg");
        fs::create_dir_all(registrar.parent().unwrap()).unwrap();
        fs::write(
            &registrar,
            r##"#!/bin/sh
set -eu
mkdir -p "$(dirname "$VR_PATHREG_OVERRIDE")"
printf '%s\n' "$1" >> "$(dirname "$VR_PATHREG_OVERRIDE")/calls"
case "$1" in
adddriver) printf '{"external_drivers":["%s"]}' "$2" > "$VR_PATHREG_OVERRIDE" ;;
removedriver) printf '{"external_drivers":[]}' > "$VR_PATHREG_OVERRIDE" ;;
*) exit 2 ;;
esac
"##,
        )
        .unwrap();
        fs::set_permissions(registrar, fs::Permissions::from_mode(0o755)).unwrap();
        paths
    }

    #[test]
    fn native_install_is_persistent_registered_and_preserves_user_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = fixture(tmp.path());
        assert!(!paths.status().steamvr.installed);
        assert!(!paths.status().steamvr.registered);
        paths.install(Component::SteamVr).unwrap();
        paths.install(Component::OpenXr).unwrap();
        assert!(paths.status().steamvr.installed && paths.status().openxr.installed);
        let manifest: serde_json::Value = serde_json::from_slice(&fs::read(paths.layer_manifest()).unwrap()).unwrap();
        assert_eq!(manifest["api_layer"]["library_path"], json!(paths.layer_lib()));
        assert!(manifest["api_layer"].get("enable_environment").is_none());
        let config = paths.config.join("openrigcue/steamvr-driver.toml");
        fs::write(&config, "enabled = false\ncor_down_m = 0.63\n").unwrap();
        let Bundle::Packaged(bundle) = &paths.bundles[0] else { unreachable!() };
        fs::write(bundle.join("SteamVR/drivers/openrigcue/bin/linux64/driver_openrigcue.so"), b"updated driver")
            .unwrap();
        assert!(paths.status().steamvr.update_available);
        paths.install(Component::SteamVr).unwrap();
        let preserved: toml::Value = toml::from_str(&fs::read_to_string(config).unwrap()).unwrap();
        assert_eq!(preserved["enabled"].as_bool(), Some(true));
        assert_eq!(preserved["cor_down_m"].as_float(), Some(0.63));
        assert!(!paths.status().steamvr.update_available);
        fs::remove_dir_all(bundle).unwrap();
        assert!(paths.status().steamvr.installed && paths.status().openxr.installed);
    }

    #[test]
    fn registration_failure_never_reports_installed_and_can_be_retried() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = fixture(tmp.path());
        let registrar = paths.steamvr.as_ref().unwrap().join("bin/linux64/vrpathreg");
        let good = fs::read(&registrar).unwrap();
        fs::write(&registrar, "#!/bin/sh\necho 'registration denied' >&2\nexit 9\n").unwrap();
        assert!(paths.install(Component::SteamVr).unwrap_err().contains("registration denied"));
        assert!(paths.status().steamvr.installed);
        assert!(!paths.status().steamvr.registered);
        fs::write(&registrar, good).unwrap();
        paths.install(Component::SteamVr).unwrap();
        assert!(paths.status().steamvr.installed);
    }

    #[test]
    fn missing_runtime_does_not_block_independent_openxr_installation() {
        let tmp = tempfile::tempdir().unwrap();
        let mut paths = fixture(tmp.path());
        paths.steamvr = None;
        assert!(paths.startup().is_empty());
        assert!(paths.install(Component::SteamVr).unwrap_err().contains("SteamVR was not found"));
        assert!(paths.status().steamvr.installed);
        assert!(!paths.status().steamvr.registered);
        paths.install(Component::OpenXr).unwrap();
        assert!(paths.status().openxr.installed);
    }

    #[test]
    fn source_build_artifacts_use_the_same_persistent_install_contract() {
        let tmp = tempfile::tempdir().unwrap();
        let mut paths = fixture(tmp.path());
        let source = tmp.path().join("source");
        let target = tmp.path().join("target");
        let Bundle::Packaged(bundle) = &paths.bundles[0] else { unreachable!() };
        for (from, to) in [
            (bundle.join("OpenXR/libopenrigcue_xr_layer.so"), target.join("release/libopenrigcue_xr_layer.so")),
            (
                bundle.join("OpenXR/openrigcue_xr_layer.json.in"),
                source.join("crates/openrigcue-xr-layer/manifest/openrigcue_xr_layer.json.in"),
            ),
        ] {
            fs::create_dir_all(to.parent().unwrap()).unwrap();
            fs::copy(from, to).unwrap();
        }
        paths.bundles = vec![Bundle::Source { root: source, target }];
        paths.install(Component::OpenXr).unwrap();
        assert!(paths.status().openxr.installed);
    }

    #[test]
    fn registry_override_and_partial_install_are_checked_without_user_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let mut paths = fixture(tmp.path());
        paths.registry_override = Some(tmp.path().join("separate registry/paths.json"));
        paths.install(Component::SteamVr).unwrap();
        assert!(paths.registry().is_file());
        assert!(!paths.config.join("openvr/openvrpaths.vrpath").exists());
        fs::remove_file(paths.driver_dir().join("resources/settings/default.vrsettings")).unwrap();
        assert!(!paths.status().steamvr.installed);
        assert!(paths.status().steamvr.registered);
        assert_eq!(paths.status().steamvr.action, Some("Uninstall"));
        paths.install(Component::SteamVr).unwrap();
        assert!(paths.status().steamvr.installed && paths.status().steamvr.registered);
        paths.install(Component::OpenXr).unwrap();
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(paths.layer_manifest()).unwrap()).unwrap();
        manifest["api_layer"]["library_path"] = json!("/old/appimage/mount/lib.so");
        fs::write(paths.layer_manifest(), serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(paths.status().openxr.installed, "invalid external registration does not remove the bundled library");
        assert!(!paths.status().openxr.registered);
        paths.install(Component::OpenXr).unwrap();
        assert!(paths.status().openxr.installed);
    }

    #[test]
    fn openxr_update_preserves_the_existing_always_on_choice() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = fixture(tmp.path());
        paths.install(Component::OpenXr).unwrap();
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(paths.layer_manifest()).unwrap()).unwrap();
        assert!(manifest["api_layer"].get("enable_environment").is_none(), "fresh installs are always on");
        manifest["api_layer"].as_object_mut().unwrap().remove("enable_environment");
        fs::write(paths.layer_manifest(), serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
        assert!(paths.status().openxr.installed, "the supported CLI --always-on manifest is installed");
        assert!(!paths.status().openxr.update_available, "the activation choice alone is not an update");
        let Bundle::Packaged(bundle) = &paths.bundles[0] else { unreachable!() };
        fs::write(bundle.join("OpenXR/libopenrigcue_xr_layer.so"), b"new layer version").unwrap();
        assert!(paths.status().openxr.update_available);
        paths.install(Component::OpenXr).unwrap();
        let updated: serde_json::Value = serde_json::from_slice(&fs::read(paths.layer_manifest()).unwrap()).unwrap();
        assert!(updated["api_layer"].get("enable_environment").is_none());
        assert_eq!(updated["api_layer"]["disable_environment"], "OPENRIGCUE_XR_LAYER_DISABLE");
        assert_eq!(fs::read(paths.layer_lib()).unwrap(), b"new layer version");
        assert!(!paths.status().openxr.update_available);
    }
    #[test]
    fn fresh_native_setup_is_active_without_game_launch_options() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = fixture(tmp.path());
        paths.install(Component::OpenXr).unwrap();
        let manifest: serde_json::Value = serde_json::from_slice(&fs::read(paths.layer_manifest()).unwrap()).unwrap();
        assert!(
            manifest["api_layer"].get("enable_environment").is_none(),
            "native setup must activate without per-game environment options"
        );
    }

    #[test]
    fn startup_prepares_components_and_external_deployment_stays_explicit() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = fixture(tmp.path());
        for component in [Component::OpenXr, Component::SteamVr] {
            write_atomic(&paths.disabled_marker(component), b"legacy disabled").unwrap();
        }
        assert!(paths.startup().is_empty());
        assert!(paths.installed(Component::OpenXr) && paths.installed(Component::SteamVr));
        assert!(!paths.layer_manifest().exists() && !paths.registered());
        assert!(!paths.disabled_marker(Component::OpenXr).exists());
        assert!(!paths.disabled_marker(Component::SteamVr).exists());
        for component in [Component::OpenXr, Component::SteamVr] {
            paths.install(component).unwrap();
        }
        assert!(paths.registered() && paths.installed_openxr_manifest().is_some());
        let calls = paths.registry().parent().unwrap().join("calls");
        let before = fs::read_to_string(&calls).unwrap();
        assert!(paths.startup().is_empty());
        assert_eq!(fs::read_to_string(&calls).unwrap(), before);
        for component in [Component::OpenXr, Component::SteamVr] {
            paths.uninstall(component).unwrap();
        }
        assert!(!paths.registered() && !paths.layer_manifest().exists());
        assert!(paths.layer_lib().exists() && paths.driver_dir().exists());
        let config: toml::Value = toml::from_str(&fs::read_to_string(paths.driver_config()).unwrap()).unwrap();
        assert_eq!(config["enabled"].as_bool(), Some(true));
        assert!(paths.startup().is_empty());
        assert!(
            !paths.registered() && !paths.layer_manifest().exists(),
            "restart must not redeploy external integration"
        );
        for component in [Component::OpenXr, Component::SteamVr] {
            paths.install(component).unwrap();
        }
        assert!(paths.registered() && paths.installed_openxr_manifest().is_some());
    }

    #[test]
    fn action_tracks_external_registration_not_bundled_library_presence() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = fixture(tmp.path());
        assert!(paths.startup().is_empty());
        for component in [Component::OpenXr, Component::SteamVr] {
            let status = paths.component_status(component);
            assert!(status.installed && !status.registered);
            assert_eq!(status.action, Some("Install"));
            paths.install(component).unwrap();
            let installed = paths.component_status(component);
            assert_eq!(installed.action, Some("Uninstall"));
            assert_eq!(installed.status, "Installed");
            let mut failed = paths.status();
            failed.set_error(component, "deployment changed but refresh failed".into());
            let status = match component {
                Component::OpenXr => failed.openxr,
                Component::SteamVr => failed.steamvr,
            };
            assert_eq!(status.action, Some("Uninstall"), "error must preserve actual registration state");
            paths.uninstall(component).unwrap();
            let status = paths.component_status(component);
            assert!(status.installed && !status.registered);
            assert_eq!(status.action, Some("Install"));
        }
        let mut missing = paths.clone();
        missing.steamvr = None;
        let missing = missing.component_status(Component::SteamVr);
        assert_eq!(missing.status, "SteamVR not found");
        assert_eq!(missing.action, None);
    }

    #[test]
    fn external_registration_failure_keeps_the_bundled_components_ready() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = fixture(tmp.path());
        assert!(paths.startup().is_empty());
        let registrar = paths.steamvr.as_ref().unwrap().join("bin/linux64/vrpathreg");
        let original = fs::read(&registrar).unwrap();
        fs::write(&registrar, "#!/bin/sh\necho register-failed >&2\nexit 9\n").unwrap();
        assert!(paths.install(Component::SteamVr).unwrap_err().contains("register-failed"));
        assert!(paths.layer_lib().exists() && paths.driver_dir().exists());
        assert!(!paths.registered());
        fs::write(&registrar, original).unwrap();
        paths.install(Component::SteamVr).unwrap();
        assert!(paths.registered());
    }

    #[test]
    fn external_changes_restart_only_the_selected_running_runtime() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let paths = fixture(tmp.path());
        let runtime = paths.steamvr.as_ref().unwrap();
        let server = runtime.join("bin/linux64/vrserver");
        fs::write(&server, b"fixture server; never executed").unwrap();
        let monitor = runtime.join("bin/vrmonitor.sh");
        let calls = runtime.join("restart-calls");
        fs::write(&monitor, "#!/bin/sh\nprintf '%s\\n' \"$1\" >> restart-calls\n").unwrap();
        fs::set_permissions(&monitor, fs::Permissions::from_mode(0o755)).unwrap();
        fs::create_dir_all(paths.proc_root.join("123")).unwrap();
        symlink(tmp.path().join("other-runtime/vrserver"), paths.proc_root.join("123/exe")).unwrap();
        paths.install(Component::SteamVr).unwrap();
        assert!(!calls.exists(), "another runtime must not be restarted");
        fs::remove_file(paths.proc_root.join("123/exe")).unwrap();
        symlink(&server, paths.proc_root.join("123/exe")).unwrap();
        paths.install(Component::SteamVr).unwrap();
        paths.uninstall(Component::SteamVr).unwrap();
        assert_eq!(fs::read_to_string(&calls).unwrap(), "vrmonitor://restartsystem\nvrmonitor://restartsystem\n");
        assert!(paths.installed(Component::SteamVr));
        assert!(!paths.registered());
        fs::write(&monitor, "#!/bin/sh\necho restart-rejected >&2\nexit 9\n").unwrap();
        assert!(paths.install(Component::SteamVr).unwrap_err().contains("restart-rejected"));
        assert!(paths.registered(), "the registered-but-not-refreshed state stays truthful");
    }

    #[test]
    fn stalled_registrar_returns_bounded_error_after_deploying_libraries() {
        let tmp = tempfile::tempdir().unwrap();
        let mut paths = fixture(tmp.path());
        paths.registrar_timeout = std::time::Duration::from_millis(80);
        let registrar = paths.steamvr.as_ref().unwrap().join("bin/linux64/vrpathreg");
        fs::write(registrar, "#!/bin/sh\nexec sleep 10\n").unwrap();
        let start = std::time::Instant::now();
        assert!(paths.startup().is_empty());
        let error = paths.install(Component::SteamVr).unwrap_err();
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
        assert!(error.contains("timed out"));
        assert!(paths.status().openxr.installed && paths.status().steamvr.installed);
        assert!(!paths.status().steamvr.registered && !paths.status().steamvr.active);
        let vr = super::super::VrCompensation::new(None, tmp.path().join("rig-pose"));
        *vr.0.setup.lock().unwrap() = Some(paths);
        vr.initialize_native_integrations();
        assert!(vr.configure_integration(Component::SteamVr, false).is_err());
        let status = vr.setup_view();
        assert!(status.steamvr.error.as_ref().unwrap().contains("timed out"));
        assert_eq!(status.steamvr.action, Some("Install"));
        assert!(!status.steamvr.active);
    }
}
