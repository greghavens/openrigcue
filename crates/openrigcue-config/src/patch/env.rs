//! What a patch module needs to know about the machine: the game's Windows user folders and registry, the Steam
//! libraries, the asset library and the configured UDP ports.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use super::prefix::WinePrefix;
use super::steam::SteamLibraries;
use super::winpath;

/// `config.ini` `[NETWORK_PORT]` keys used by the patch modules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PortKey {
    F1,
    F12018,
    F12019,
    F12020,
    F12021,
    F12022,
    F12023,
    F12024,
    F12025,
    Dirt,
    Grid,
    Lfs,
    KartKraft,
    TheCrew,
    VRacer,
    Condor,
    Fsx,
    P3d,
    Dcs,
    Il2,
    Wrc2023,
    Wreckfest2,
    PmRacing,
    Tdusc,
}

impl PortKey {
    /// Every key, in declaration order.
    pub const ALL: [PortKey; 24] = [
        PortKey::F1,
        PortKey::F12018,
        PortKey::F12019,
        PortKey::F12020,
        PortKey::F12021,
        PortKey::F12022,
        PortKey::F12023,
        PortKey::F12024,
        PortKey::F12025,
        PortKey::Dirt,
        PortKey::Grid,
        PortKey::Lfs,
        PortKey::KartKraft,
        PortKey::TheCrew,
        PortKey::VRacer,
        PortKey::Condor,
        PortKey::Fsx,
        PortKey::P3d,
        PortKey::Dcs,
        PortKey::Il2,
        PortKey::Wrc2023,
        PortKey::Wreckfest2,
        PortKey::PmRacing,
        PortKey::Tdusc,
    ];

    /// The key name in `[NETWORK_PORT]`.
    pub fn config_key(self) -> &'static str {
        match self {
            PortKey::F1 => "f1",
            PortKey::F12018 => "f12018",
            PortKey::F12019 => "f12019",
            PortKey::F12020 => "f12020",
            PortKey::F12021 => "f12021",
            PortKey::F12022 => "f12022",
            PortKey::F12023 => "f12023",
            PortKey::F12024 => "f12024",
            PortKey::F12025 => "f12025",
            PortKey::Dirt => "dirt",
            PortKey::Grid => "grid",
            PortKey::Lfs => "lfs",
            PortKey::KartKraft => "kartkraft",
            PortKey::TheCrew => "thecrew",
            PortKey::VRacer => "vracer",
            PortKey::Condor => "condor",
            PortKey::Fsx => "fsx",
            PortKey::P3d => "p3d",
            PortKey::Dcs => "dcs",
            PortKey::Il2 => "il2",
            PortKey::Wrc2023 => "wrc2023",
            PortKey::Wreckfest2 => "wreckfest2",
            PortKey::PmRacing => "pmracing",
            PortKey::Tdusc => "tdusc",
        }
    }
}

/// Source of the configured UDP ports (`[NETWORK_PORT]`).
pub trait PortLookup {
    /// The port configured for a key, `None` when the key is absent.
    fn port(&self, key: PortKey) -> Option<u16>;

    /// The port's text form, for patches that write the configured text unchanged.
    /// Numeric-only callers keep their existing decimal substitution.
    fn port_text(&self, key: PortKey) -> Option<String> {
        self.port(key).map(|port| port.to_string())
    }

    /// The raw scalar, which the WRC patcher converts to an integer itself.
    /// Numeric-only adapters retain their existing bounded integer values.
    fn port_value(&self, key: PortKey) -> Option<openrigcue_model::native_value::LegacyValue> {
        self.port(key).map(|port| i64::from(port).into())
    }
}

impl<F: Fn(PortKey) -> Option<u16>> PortLookup for F {
    fn port(&self, key: PortKey) -> Option<u16> {
        self(key)
    }
}

impl PortLookup for crate::settings::NetworkPorts {
    fn port(&self, key: PortKey) -> Option<u16> {
        self.get(key.config_key())
    }
}

impl PortLookup for crate::settings::Settings {
    fn port(&self, key: PortKey) -> Option<u16> {
        self.network_port(key.config_key())
    }

    fn port_text(&self, key: PortKey) -> Option<String> {
        self.native_snapshot().get("NETWORK_PORT")?.as_dict()?.get(key.config_key())?.py_str()
    }

    fn port_value(&self, key: PortKey) -> Option<openrigcue_model::native_value::LegacyValue> {
        self.native_snapshot().get("NETWORK_PORT")?.as_dict()?.get(key.config_key()).cloned()
    }
}

/// A patch module that compares against the port it loaded itself rather than the configured one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PortModule {
    /// A Codemasters / EA module, identified by its settings file below Documents.
    Codemasters(&'static str),
    Lfs,
    Il2,
}

/// Ports the Codemasters, LFS and IL-2 modules loaded: each reads the configured port once at
/// application start and again only when its `patch()` (F1 2015: `patched()`) runs; their other function
/// compares or writes with that loaded port. Create one per application run, before the user can change ports.
#[derive(Debug, Default)]
pub struct ModulePorts {
    startup: HashMap<PortKey, CachedPort>,
    reloaded: Mutex<HashMap<PortModule, CachedPort>>,
}

#[derive(Debug)]
struct CachedPort {
    /// Numeric view; the modules themselves cache text without a socket-range check.
    number: Option<u16>,
    text: String,
}

/// The `[NETWORK_PORT]` keys the patch modules load at application start
/// (F1 2016…F1 25, LFS, the Codemasters modules, F1 2015, GRID 2, IL-2),
/// in load order.
pub const IMPORT_PORT_KEYS: [PortKey; 13] = [
    PortKey::F1,
    PortKey::F12018,
    PortKey::F12019,
    PortKey::F12020,
    PortKey::F12021,
    PortKey::F12022,
    PortKey::F12023,
    PortKey::F12024,
    PortKey::F12025,
    PortKey::Lfs,
    PortKey::Dirt,
    PortKey::Grid,
    PortKey::Il2,
];

impl ModulePorts {
    /// Loads the module ports at application start: fails with the first key of
    /// [`IMPORT_PORT_KEYS`] missing from the configuration (which ends the application's start).
    pub fn imported(ports: &dyn PortLookup) -> Result<Self, super::PatchError> {
        if let Some(missing) = IMPORT_PORT_KEYS.iter().find(|key| ports.port_text(**key).is_none()) {
            return Err(super::PatchError::MissingPort(*missing));
        }
        Ok(Self::at_startup(ports))
    }

    /// The ports every module loads at application start.
    pub fn at_startup(ports: &dyn PortLookup) -> Self {
        let startup = PortKey::ALL
            .into_iter()
            .filter_map(|key| {
                let text = ports.port_text(key)?;
                Some((key, CachedPort { number: ports.port(key), text }))
            })
            .collect();
        ModulePorts { startup, reloaded: Mutex::default() }
    }

    /// The port the module last loaded.
    pub fn loaded(&self, module: PortModule, key: PortKey) -> Result<u16, super::PatchError> {
        let reloaded = self.reloaded.lock().unwrap_or_else(PoisonError::into_inner);
        reloaded
            .get(&module)
            .or_else(|| self.startup.get(&key))
            .and_then(|port| port.number)
            .ok_or(super::PatchError::MissingPort(key))
    }

    /// The exact text of the raw value the module loaded.
    pub fn loaded_text(&self, module: PortModule, key: PortKey) -> Result<String, super::PatchError> {
        let reloaded = self.reloaded.lock().unwrap_or_else(PoisonError::into_inner);
        reloaded
            .get(&module)
            .or_else(|| self.startup.get(&key))
            .map(|port| port.text.clone())
            .ok_or(super::PatchError::MissingPort(key))
    }

    /// Loads the configured port into the module; a missing key leaves the loaded port unchanged.
    pub fn reload(&self, module: PortModule, key: PortKey, ports: &dyn PortLookup) -> Result<u16, super::PatchError> {
        let port = ports.port(key).ok_or(super::PatchError::MissingPort(key))?;
        self.reloaded
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(module, CachedPort { number: Some(port), text: port.to_string() });
        Ok(port)
    }

    /// A required lookup that preserves the preceding cache on failure.
    pub fn reload_text(
        &self,
        module: PortModule,
        key: PortKey,
        ports: &dyn PortLookup,
    ) -> Result<String, super::PatchError> {
        let text = ports.port_text(key).ok_or(super::PatchError::MissingPort(key))?;
        let number = ports.port(key);
        self.reloaded
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(module, CachedPort { number, text: text.clone() });
        Ok(text)
    }
}

/// Everything a patch module reads besides the game's own files.
pub struct PatchEnvironment<'a> {
    /// Wine prefix holding the game's Windows user folders and registry.
    pub prefix: Option<&'a WinePrefix>,
    /// Steam libraries, for the modules that locate games through Steam.
    pub steam: Option<&'a SteamLibraries>,
    /// The templates and plugin files.
    pub library: &'a super::library::Library,
    /// The configured ports (`[NETWORK_PORT]`) as they are now.
    pub ports: &'a dyn PortLookup,
    /// The ports the Codemasters, LFS and IL-2 modules loaded.
    pub loaded_ports: &'a ModulePorts,
}

impl PatchEnvironment<'_> {
    /// Configured port of a key.
    pub fn port(&self, key: PortKey) -> Result<u16, super::PatchError> {
        self.ports.port(key).ok_or(super::PatchError::MissingPort(key))
    }

    pub fn port_text(&self, key: PortKey) -> Result<String, super::PatchError> {
        self.ports.port_text(key).ok_or(super::PatchError::MissingPort(key))
    }

    pub fn port_value(&self, key: PortKey) -> Result<openrigcue_model::native_value::LegacyValue, super::PatchError> {
        self.ports.port_value(key).ok_or(super::PatchError::MissingPort(key))
    }

    /// The port a module last loaded ([`ModulePorts::loaded`]).
    pub fn loaded_port(&self, module: PortModule, key: PortKey) -> Result<u16, super::PatchError> {
        self.loaded_ports.loaded(module, key)
    }

    /// Loads the configured port into a module and returns it ([`ModulePorts::reload`]).
    pub fn reload_port(&self, module: PortModule, key: PortKey) -> Result<u16, super::PatchError> {
        self.loaded_ports.reload(module, key, self.ports)
    }

    pub fn loaded_port_text(&self, module: PortModule, key: PortKey) -> Result<String, super::PatchError> {
        self.loaded_ports.loaded_text(module, key)
    }

    pub fn reload_port_text(&self, module: PortModule, key: PortKey) -> Result<String, super::PatchError> {
        self.loaded_ports.reload_text(module, key, self.ports)
    }

    /// The root of the prefix's `C:` drive, used as the fallback drive root.
    pub fn drive_root(&self) -> Option<PathBuf> {
        self.prefix.and_then(|prefix| prefix.unix_path("C:\\"))
    }
}

#[cfg(test)]
mod native_cache_tests {
    use super::*;

    #[test]
    fn native_cached_import_checks_presence_without_socket_range_normalization() {
        for text in ["70000", "-1"] {
            let mut settings = crate::settings::Settings::default();
            settings.edit_native(|raw| {
                raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert("f1", text.into());
            });
            assert!(settings.network_port("f1").is_none());
            let cache = ModulePorts::imported(&settings).expect("str(raw) import has no bind-range check");
            assert_eq!(cache.loaded_text(PortModule::Codemasters("fixture"), PortKey::F1).unwrap(), text);
        }
    }

    #[test]
    fn native_cached_text_reload_keeps_failed_prefix_and_numeric_compatibility() {
        let mut settings = crate::settings::Settings::default();
        settings.edit_native(|raw| {
            raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert("f1", "001337".into());
        });
        let cache = ModulePorts::imported(&settings).unwrap();
        let module = PortModule::Codemasters("fixture");
        assert_eq!(cache.loaded_text(module, PortKey::F1).unwrap(), "001337");
        assert_eq!(cache.loaded(module, PortKey::F1).unwrap(), 1337);
        let mut missing = settings.clone();
        missing.edit_native(|raw| {
            raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().remove("f1");
        });
        assert!(matches!(
            cache.reload_text(module, PortKey::F1, &missing),
            Err(super::super::PatchError::MissingPort(PortKey::F1))
        ));
        assert_eq!(cache.loaded_text(module, PortKey::F1).unwrap(), "001337");
        let mut wide = settings;
        wide.edit_native(|raw| {
            raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert("f1", "70000".into());
        });
        assert_eq!(cache.reload_text(module, PortKey::F1, &wide).unwrap(), "70000");
        assert!(cache.loaded(module, PortKey::F1).is_err());
        assert!(cache.reload(module, PortKey::F1, &wide).is_err());
        assert_eq!(cache.loaded_text(module, PortKey::F1).unwrap(), "70000");
        assert_eq!(cache.reload(module, PortKey::F1, &|_| Some(1338)).unwrap(), 1338);
        assert_eq!(cache.loaded_text(module, PortKey::F1).unwrap(), "1338");
    }
}

/// A Windows user folder of the prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserFolder {
    /// The Documents folder.
    Documents,
    /// The user's home folder.
    Profile,
    /// `%APPDATA%`.
    AppData,
    /// `%LOCALAPPDATA%`.
    LocalAppData,
    /// The parent folder of `%APPDATA%`.
    AppDataParent,
}

/// A path below a user folder, written with Windows separators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserPath {
    pub folder: UserFolder,
    pub relative: &'static str,
}

impl UserPath {
    pub const fn new(folder: UserFolder, relative: &'static str) -> Self {
        UserPath { folder, relative }
    }

    /// The Linux path inside the prefix.
    pub fn resolve(&self, prefix: &WinePrefix) -> PathBuf {
        let base = match self.folder {
            UserFolder::Documents => prefix.documents(),
            UserFolder::Profile => prefix.profile(),
            UserFolder::AppData => prefix.app_data(),
            UserFolder::LocalAppData => prefix.local_app_data(),
            UserFolder::AppDataParent => {
                let app_data = prefix.app_data();
                app_data.parent().map(Path::to_path_buf).unwrap_or(app_data)
            }
        };
        winpath::resolve(&base, self.relative)
    }
}
