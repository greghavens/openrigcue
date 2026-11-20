//! `config.ini`, retaining its raw INI values alongside typed views for existing consumers.
//!
//! Keys of an imported SRS `config.ini` that are not used: the sales-gating keys (`trial_subscription_started`,
//! `trial_started`, `show_deactivate_license_button`; OpenRigCue has no such concept), cloud backup (`enable_cloud_backup`), analytics
//! (`uuid`) and marketing feeds (`marketing_notifications`). Migration bookkeeping remains in the
//! INI file. Launcher flags are kept in the separate `registry.db`.

mod configuration;
mod joysticks;
mod leds;
mod metadata;
mod native;
mod notifications;
pub mod paths;
mod sections;
mod shortcuts;

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

pub use configuration::{
    reset_config_files, Configuration, Opened, StartFailure, RESET_FILE, RESET_NOTICE_TEXT, RESET_NOTICE_TITLE,
};
pub use joysticks::{AxisMapping, JoystickProfiles};
pub use leds::{LedSegment, LedSettings, SegmentKind};
pub use notifications::Dismissals;
pub use sections::*;
pub use shortcuts::{Binding, Shortcuts};

use crate::import::de::convert_lenient;
use crate::import::value::{Dict, LegacyValue};
use crate::import::ImportWarning;
use crate::StoreError;

/// Version of the typed settings view (not the INI file's migration cursor).
pub const SETTINGS_VERSION: u32 = 1;

/// All global settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub version: u32,
    pub app: AppSettings,
    pub shakers: ShakerSettings,
    pub shortcuts: Shortcuts,
    pub wind: WindSettings,
    pub motion: MotionSettings,
    pub dof_reality: DofRealitySettings,
    pub prosimu: ProsimuSettings,
    pub yaw_vr: YawVrSettings,
    pub pt_actuator: PtActuatorSettings,
    pub thanos: ThanosSettings,
    pub vnm: VnmSettings,
    pub simtech: SimtechSettings,
    pub network_ports: NetworkPorts,
    /// Listener id → `ip:port` targets its UDP packets are forwarded to (`[PORT_FORWARDING]`).
    pub port_forwarding: BTreeMap<String, Vec<String>>,
    pub features: CustomFeatures,
    pub leds: LedSettings,
    /// Monitor index → `x + y + width + height` at the last save (`[MONITORS]`).
    pub monitors: BTreeMap<String, i64>,
    pub compensation: MotionCompensationSettings,
    pub belt: BeltSettings,
    pub hud: HudSettings,
    pub iflag: IflagSettings,
    pub sim3d: Sim3dSettings,
    pub hapticr: HapticrSettings,
    pub magicbox: MagicboxSettings,
    pub symprojects: SymprojectsSettings,
    #[serde(skip)]
    native: native::NativeSettings,
}

impl PartialEq for Settings {
    fn eq(&self, other: &Self) -> bool {
        self.version == other.version
            && self.app == other.app
            && self.shakers == other.shakers
            && self.shortcuts == other.shortcuts
            && self.wind == other.wind
            && self.motion == other.motion
            && self.dof_reality == other.dof_reality
            && self.prosimu == other.prosimu
            && self.yaw_vr == other.yaw_vr
            && self.pt_actuator == other.pt_actuator
            && self.thanos == other.thanos
            && self.vnm == other.vnm
            && self.simtech == other.simtech
            && self.network_ports == other.network_ports
            && self.port_forwarding == other.port_forwarding
            && self.features == other.features
            && self.leds == other.leds
            && self.monitors == other.monitors
            && self.compensation == other.compensation
            && self.belt == other.belt
            && self.hud == other.hud
            && self.iflag == other.iflag
            && self.sim3d == other.sim3d
            && self.hapticr == other.hapticr
            && self.magicbox == other.magicbox
            && self.symprojects == other.symprojects
    }
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            version: SETTINGS_VERSION,
            app: AppSettings::default(),
            shakers: ShakerSettings::default(),
            shortcuts: Shortcuts::default(),
            wind: WindSettings::default(),
            motion: MotionSettings::default(),
            dof_reality: DofRealitySettings::default(),
            prosimu: ProsimuSettings::default(),
            yaw_vr: YawVrSettings::default(),
            pt_actuator: PtActuatorSettings::default(),
            thanos: ThanosSettings::default(),
            vnm: VnmSettings::default(),
            simtech: SimtechSettings::default(),
            network_ports: NetworkPorts::default(),
            port_forwarding: BTreeMap::new(),
            features: CustomFeatures::default(),
            leds: LedSettings::default(),
            monitors: BTreeMap::new(),
            compensation: MotionCompensationSettings::default(),
            belt: BeltSettings::default(),
            hud: HudSettings::default(),
            iflag: IflagSettings::default(),
            sim3d: Sim3dSettings::default(),
            hapticr: HapticrSettings::default(),
            magicbox: MagicboxSettings::default(),
            symprojects: SymprojectsSettings::default(),
            native: Default::default(),
        }
    }
}

/// `config.ini` keys that are deliberately not carried over.
const NOT_PORTED: [(&str, &str); 9] = [
    ("APP", "uuid"),
    ("APP", "trial_subscription_started"),
    ("APP", "trial_started"),
    ("APP", "enable_cloud_backup"),
    ("APP", "marketing_notifications"),
    ("APP", "show_deactivate_license_button"),
    ("APP", "config_file_version"),
    ("APP", "first_time_installation"),
    ("INTELLIBOX", "config_file_version"),
];

/// Keys `template.ini` holds that nothing reads (`APP.window_with`: the window width is
/// `window_width`, which step v87 sets).
const UNREAD: [(&str, &str); 1] = [("APP", "window_with")];

impl Settings {
    /// Converts a migrated `config.ini` (version 412 shape). Values that do not convert keep the
    /// default and are reported, as are keys openrigcue does not carry over.
    pub fn from_legacy(config: &Dict) -> (Settings, Vec<ImportWarning>) {
        let mut warnings = Vec::new();
        let section = |name: &str| config.get(name).and_then(LegacyValue::as_dict);
        let (shortcuts, unmapped) = match section("SHORTCUTS") {
            Some(s) => Shortcuts::from_legacy(s),
            None => (Shortcuts::default(), Vec::new()),
        };
        let mut settings = Settings {
            version: SETTINGS_VERSION,
            app: convert_lenient(section("APP"), "APP", &mut warnings),
            shakers: convert_lenient(section("SHAKEBOX"), "SHAKEBOX", &mut warnings),
            shortcuts,
            wind: convert_lenient(section("INTELLIBOX"), "INTELLIBOX", &mut warnings),
            motion: convert_lenient(section("MOTIONBOX"), "MOTIONBOX", &mut warnings),
            dof_reality: convert_lenient(section("DOF REALITY"), "DOF REALITY", &mut warnings),
            prosimu: convert_lenient(section("PROSIMU"), "PROSIMU", &mut warnings),
            yaw_vr: convert_lenient(section("Yaw VR"), "Yaw VR", &mut warnings),
            pt_actuator: convert_lenient(section("PT-ACTUATOR"), "PT-ACTUATOR", &mut warnings),
            thanos: convert_lenient(section("THANOS"), "THANOS", &mut warnings),
            vnm: convert_lenient(section("VNM SIMULATION"), "VNM SIMULATION", &mut warnings),
            simtech: convert_lenient(section("SIMTECHREVOLUTION"), "SIMTECHREVOLUTION", &mut warnings),
            network_ports: convert_lenient(section("NETWORK_PORT"), "NETWORK_PORT", &mut warnings),
            port_forwarding: convert_lenient(section("PORT_FORWARDING"), "PORT_FORWARDING", &mut warnings),
            features: convert_lenient(section("CUSTOM_FEATURES"), "CUSTOM_FEATURES", &mut warnings),
            leds: convert_lenient::<leds::LegacyLed>(section("LED"), "LED", &mut warnings).into(),
            monitors: convert_lenient(section("MONITORS"), "MONITORS", &mut warnings),
            compensation: convert_lenient(section("MOTION_COMPENSATION"), "MOTION_COMPENSATION", &mut warnings),
            belt: convert_lenient(section("BELT_TENSIONER"), "BELT_TENSIONER", &mut warnings),
            hud: convert_lenient(section("HUD"), "HUD", &mut warnings),
            iflag: convert_lenient(section("IFLAG"), "IFLAG", &mut warnings),
            sim3d: convert_lenient(section("SIM3D"), "SIM3D", &mut warnings),
            hapticr: convert_lenient(section("HAPTICR"), "HAPTICR", &mut warnings),
            magicbox: convert_lenient(section("MAGICBOX"), "MAGICBOX", &mut warnings),
            symprojects: convert_lenient(section("SYMPROJECTS"), "SYMPROJECTS", &mut warnings),
            native: Default::default(),
        };
        settings.app.allowed_serial_ports.retain(|p| !p.is_empty());
        for u in unmapped {
            warnings.push(ImportWarning::new(format!("SHORTCUTS.{}", u.key), u.reason));
        }
        for (section_name, key) in NOT_PORTED {
            if section(section_name).is_some_and(|s| s.contains(key)) {
                warnings.push(ImportWarning::not_ported(format!("{section_name}.{key}")));
            }
        }
        for (section_name, key) in UNREAD {
            if section(section_name).is_some_and(|s| s.contains(key)) {
                warnings.push(ImportWarning::unread(format!("{section_name}.{key}")));
            }
        }
        settings.native = native::NativeSettings::retain(config.clone(), &settings);
        (settings, warnings)
    }

    fn read_ini(path: &Path) -> Result<Option<Settings>, StoreError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(StoreError::io(path, error)),
        };
        let raw = crate::import::configobj::parse(&text).map_err(|error| StoreError::parse(path, error))?;
        let mut settings = Settings::from_legacy(&raw).0;
        settings.remember_ini_layout(&text);
        Ok(Some(settings))
    }

    /// INI-only access for UI callbacks: no launcher-flag reads.
    pub fn load_ini_only(path: &Path) -> Result<Settings, StoreError> {
        Ok(Self::read_ini(path)?.unwrap_or_default())
    }

    /// Opens one INI file for editing: an absent file is an empty dictionary.
    pub fn load_ini_for_native_edit(path: &Path) -> Result<Settings, StoreError> {
        Ok(Self::read_ini(path)?.unwrap_or_else(|| {
            let mut empty = Self::from_legacy(&Dict::new()).0;
            empty.native.layout = Some(crate::import::configobj::Layout::default());
            empty
        }))
    }

    /// Read-only recovery of the TOML-to-INI integer spelling defect.
    /// Only an older, version-1 TOML file with the same explicit old value can
    /// establish provenance; arbitrary INI text is never normalized.
    pub fn recover_legacy_typed_integers(&mut self, ini: &Path, legacy: &Path) -> bool {
        let older = (|| {
            let ini_time = std::fs::metadata(ini).ok()?.modified().ok()?;
            let old_time = std::fs::metadata(legacy).ok()?.modified().ok()?;
            (old_time < ini_time).then_some(())
        })()
        .is_some();
        if !older {
            return false;
        }
        let Some(document) = std::fs::read_to_string(legacy).ok().and_then(|text| text.parse::<toml::Value>().ok())
        else {
            return false;
        };
        if document.get("version").and_then(toml::Value::as_integer) != Some(1) {
            return false;
        }
        native::recover_typed_integer_fields(self, &document)
    }

    /// Reads `config.ini`; accepts old `settings.toml` files for migration.
    pub fn load(path: &Path) -> Result<Settings, StoreError> {
        if path.extension().is_some_and(|extension| extension == "toml") {
            return Ok(paths::read_toml(path)?.unwrap_or_default());
        }
        let Some(mut settings) = Self::read_ini(path)? else { return Ok(Settings::default()) };
        // Launcher flags live outside resettable configuration/metadata. Folders in the
        // older layout remain readable before their first registry write upgrades storage.
        let root = path.parent().unwrap_or_else(|| Path::new("."));
        let registry = crate::import::registry::read_path(root);
        if registry.is_file() {
            for (key, field) in [
                ("StartWithWindows", &mut settings.app.start_with_login),
                ("ShowSplashScreen", &mut settings.app.show_splash_screen),
            ] {
                if let Ok(Some(value)) = crate::import::registry::read_item(root, key) {
                    if let Some(value) = value.as_str().and_then(crate::import::value::str2bool) {
                        *field = value;
                    }
                }
            }
        }
        Ok(settings)
    }

    /// Writes only the INI file.
    pub fn write_ini_only(&self, path: &Path) -> Result<(), StoreError> {
        let layout =
            self.native.layout.clone().unwrap_or_else(|| {
                crate::import::configobj::Layout::capture(include_str!("../../data/srs/template.ini"))
            });
        let text = layout.encode(&self.native_snapshot()).map_err(|error| {
            if let Some(source) = error.native {
                StoreError::Native { path: path.to_owned(), source: Box::new(source) }
            } else {
                StoreError::serialize(path, error)
            }
        })?;
        paths::write_in_place(path, text.as_bytes())
    }

    /// Explicit snapshot save, including the launcher flags.
    /// UI writes use write_ini_only and persist flags through their own callbacks.
    pub fn save(&self, path: &Path) -> Result<(), StoreError> {
        if path.extension().is_some_and(|extension| extension == "toml") {
            return paths::write_toml(path, self);
        }
        self.write_ini_only(path)?;
        let root = path.parent().unwrap_or_else(|| Path::new("."));
        let registry = crate::import::registry::path(root);
        for (key, enabled) in
            [("StartWithWindows", self.app.start_with_login), ("ShowSplashScreen", self.app.show_splash_screen)]
        {
            let before = if crate::import::registry::read_path(root).exists() {
                crate::import::registry::read_item(root, key).map_err(|error| StoreError::parse(&registry, error))?
            } else {
                None
            };
            let value = LegacyValue::from(if enabled { "1" } else { "0" });
            if before.as_ref() != Some(&value) {
                crate::import::registry::set_item(root, key, &value)
                    .map_err(|error| StoreError::parse(&registry, error))?;
            }
        }
        Ok(())
    }

    pub(crate) fn remember_ini_layout(&mut self, text: &str) {
        self.native.layout = Some(crate::import::configobj::Layout::capture(text));
    }

    pub(crate) fn retain_ini_layout(&mut self, before: &Settings) {
        self.native.layout = before.native.layout.clone();
    }

    /// The INI dictionary, retaining raw values, unknown keys and ordering except for edited fields.
    pub fn native_snapshot(&self) -> Dict {
        self.native.snapshot(self)
    }

    /// In-process INI assignments (for example the window's Save
    /// callback), preserving value types, unknown keys, layout and registry flags.
    pub fn edit_native(&mut self, change: impl FnOnce(&mut Dict)) {
        let mut raw = self.native_snapshot();
        change(&mut raw);
        let mut edited = Self::from_legacy(&raw).0;
        edited.retain_ini_layout(self);
        edited.app.start_with_login = self.app.start_with_login;
        edited.app.show_splash_screen = self.app.show_splash_screen;
        *self = edited;
    }

    /// A raw network port value at an import/bind boundary. Missing and malformed values do not fall back
    /// to the typed view. The result is limited to ports the socket API can bind.
    pub fn network_port(&self, key: &str) -> Option<u16> {
        use crate::import::value::parse_py_int;
        let native = self.native_snapshot();
        let raw = native.get("NETWORK_PORT")?.as_dict()?.get(key)?;
        let port = match raw {
            LegacyValue::Int(n) => *n,
            LegacyValue::Float(n) if n.is_finite() && n.trunc() >= 0.0 && n.trunc() < 65536.0 => n.trunc() as i64,
            LegacyValue::Bool(b) => i64::from(*b),
            LegacyValue::Str(s) => parse_py_int(s)?,
            _ => return None,
        };
        u16::try_from(port).ok()
    }

    /// The UI's field names over canonical INI values, preserving absent and malformed fields.
    pub fn ui_view(&self) -> Result<serde_json::Value, crate::import::de::ConvertError> {
        native::ui_view(self)
    }

    /// Page-handler assignments, mapped to INI keys without deserializing/replacing the whole typed view.
    pub fn apply_ui_patch(&self, patch: &serde_json::Value) -> Result<Settings, String> {
        native::apply_ui_patch(self, patch)
    }

    /// Retains the raw INI source when an API JSON patch replaces the typed settings view.
    pub fn retain_source(&mut self, before: &Settings) {
        self.native = before.native.clone();
    }
}

#[cfg(test)]
mod tests;
