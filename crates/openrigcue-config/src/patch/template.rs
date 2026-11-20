//! Settings files replaced by a library template with `^port^` filled in: KartKraft, The Crew 1/2, V-Racer,
//! Condor 2/3, FSX / MSFS 2020 / MSFS 2024 / Prepar3D, DCS.

use std::fs;
use std::path::PathBuf;

use super::env::{PatchEnvironment, PortKey, UserFolder, UserPath};
use super::files::backup_path;
use super::text::{self, Encoding, Newline};
use super::winpath;
use super::PatchError;

/// Placeholder replaced by the configured port.
const PORT_PLACEHOLDER: &str = "^port^";

/// Where the game keeps its settings folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallDir {
    /// A folder below a Windows user folder.
    User(UserPath),
    /// The `InstallDir` value of an `HKEY_CURRENT_USER` registry key.
    Registry { key: &'static str, value: &'static str },
}

/// What `installed()` checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstalledWhen {
    InstallDirExists,
    SettingsFileExists,
}

/// When missing folders between the install folder and the settings file are created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateFolders {
    Never,
    /// Before the backup (DCS: `Scripts`, then `Scripts\Hooks`).
    BeforeBackup,
    /// After the move and the template read, just before writing (classic DCS export; a failure is an error).
    BeforeWrite,
}

/// One template-based settings file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TemplateGame {
    pub install_dir: InstallDir,
    /// Folders between the install folder and the file (Windows separators, may be empty).
    pub settings_dir: &'static str,
    pub file: &'static str,
    /// Template file of the asset library.
    pub template: &'static str,
    pub port: PortKey,
    /// Encoding of the settings file, and of the template when `patched()` reads it.
    pub encoding: Encoding,
    /// Encoding `patch()` reads the template with (differs from `encoding` only for `dcsbeta`).
    pub patch_template_encoding: Encoding,
    pub installed_when: InstalledWhen,
    pub create_folders: CreateFolders,
}

impl TemplateGame {
    fn install_dir(&self, env: &PatchEnvironment<'_>) -> Option<PathBuf> {
        let prefix = env.prefix?;
        match self.install_dir {
            InstallDir::User(path) => Some(path.resolve(prefix)),
            InstallDir::Registry { key, value } => prefix.unix_path(prefix.hive().string_value(key, value)?),
        }
    }

    fn settings_file(&self, env: &PatchEnvironment<'_>) -> Option<PathBuf> {
        let dir = winpath::resolve(&self.install_dir(env)?, self.settings_dir);
        Some(winpath::resolve(&dir, self.file))
    }

    /// `installed()` of the module.
    pub fn installed(&self, env: &PatchEnvironment<'_>) -> bool {
        match self.installed_when {
            InstalledWhen::InstallDirExists => self.install_dir(env).is_some_and(|dir| dir.is_dir()),
            InstalledWhen::SettingsFileExists => self.settings_file(env).is_some_and(|file| file.is_file()),
        }
    }

    /// The settings file exists and its text equals the filled-in template (read in text mode).
    pub fn patched(&self, env: &PatchEnvironment<'_>) -> Result<bool, PatchError> {
        let Some(file) = self.settings_file(env).filter(|file| file.is_file()) else {
            return Ok(false);
        };
        let current = text::read_text(&file, self.encoding)?;
        Ok(current == self.filled_template(env, self.encoding)?)
    }

    /// Moves the current file to its backup and writes the filled-in template. `Ok(false)` for
    /// a failed folder creation (`dcs`, `dcsbeta`), move or write; a template that cannot be read is an error.
    pub fn patch(&self, env: &PatchEnvironment<'_>) -> Result<bool, PatchError> {
        let file = self.settings_file(env).ok_or(PatchError::NotInstalled)?;
        if self.create_folders == CreateFolders::BeforeBackup && self.create_settings_dirs(env).is_err() {
            return Ok(false);
        }
        if file.is_file() && fs::rename(&file, backup_path(&file, self.file)).is_err() {
            return Ok(false);
        }
        // The template is read after the move: a failure here leaves the game's file moved aside.
        let content = self.filled_template(env, self.patch_template_encoding)?;
        if self.create_folders == CreateFolders::BeforeWrite {
            self.create_settings_dirs(env)?;
        }
        // Written under the module's own file name, whatever the case of the file just moved aside.
        let file = self.settings_file(env).ok_or(PatchError::NotInstalled)?;
        Ok(text::write_text(&file, &content, self.encoding, Newline::Windows).is_ok())
    }

    fn filled_template(&self, env: &PatchEnvironment<'_>, encoding: Encoding) -> Result<String, PatchError> {
        let template = env.library.read_text(self.template, encoding)?;
        // These sixteen modules substitute the port's raw text, after the whole read.
        // The migration-only Export.lua and other templates substitute the numeric port.
        let port = if LIVE_TEXT_PORT_GAMES.contains(self) {
            env.port_text(self.port)?
        } else {
            env.port(self.port)?.to_string()
        };
        Ok(template.replace(PORT_PLACEHOLDER, &port))
    }

    /// One folder creation per missing level.
    fn create_settings_dirs(&self, env: &PatchEnvironment<'_>) -> Result<(), PatchError> {
        let mut dir = self.install_dir(env).ok_or(PatchError::NotInstalled)?;
        for part in winpath::components(self.settings_dir) {
            dir = winpath::resolve(&dir, part);
            if !dir.is_dir() {
                fs::create_dir(&dir)?;
            }
        }
        Ok(())
    }
}

const fn user(
    folder: UserFolder,
    dir: &'static str,
    file: &'static str,
    template: &'static str,
    port: PortKey,
    installed_when: InstalledWhen,
) -> TemplateGame {
    TemplateGame {
        install_dir: InstallDir::User(UserPath::new(folder, dir)),
        settings_dir: "",
        file,
        template,
        port,
        encoding: Encoding::Ansi,
        patch_template_encoding: Encoding::Ansi,
        installed_when,
        create_folders: CreateFolders::Never,
    }
}

const fn simconnect(folder: UserFolder, dir: &'static str, port: PortKey) -> TemplateGame {
    user(folder, dir, "SimConnect.xml", "SimConnect.xml", port, InstalledWhen::InstallDirExists)
}

const fn condor(key: &'static str) -> TemplateGame {
    TemplateGame {
        install_dir: InstallDir::Registry { key, value: "InstallDir" },
        settings_dir: "Settings",
        file: "UDP.ini",
        template: "UDP.ini",
        port: PortKey::Condor,
        encoding: Encoding::Ansi,
        patch_template_encoding: Encoding::Ansi,
        installed_when: InstalledWhen::SettingsFileExists,
        create_folders: CreateFolders::Never,
    }
}

const fn dcs(dir: &'static str, encoding: Encoding, patch_template_encoding: Encoding) -> TemplateGame {
    TemplateGame {
        install_dir: InstallDir::User(UserPath::new(UserFolder::Profile, dir)),
        settings_dir: "Scripts\\Hooks",
        file: "SRSExport.lua",
        template: "SRSExport.lua",
        port: PortKey::Dcs,
        encoding,
        patch_template_encoding,
        installed_when: InstalledWhen::InstallDirExists,
        create_folders: CreateFolders::BeforeBackup,
    }
}

pub const KARTKRAFT: TemplateGame = user(
    UserFolder::LocalAppData,
    "project_k\\Saved\\Config\\WindowsNoEditor",
    "Game.ini",
    "Game.ini",
    PortKey::KartKraft,
    InstalledWhen::InstallDirExists,
);
pub const THE_CREW: TemplateGame = user(
    UserFolder::Documents,
    "The Crew",
    "ExtraConfig.xml",
    "ExtraConfig.xml",
    PortKey::TheCrew,
    InstalledWhen::InstallDirExists,
);
pub const THE_CREW_2: TemplateGame = user(
    UserFolder::Documents,
    "The Crew 2",
    "ExtraConfig.xml",
    "ExtraConfig.xml",
    PortKey::TheCrew,
    InstalledWhen::InstallDirExists,
);
pub const VRACER: TemplateGame = user(
    UserFolder::AppDataParent,
    "LocalLow\\VertexBreakers\\vracer_hoverbike",
    "telemetry.json",
    "telemetry.json",
    PortKey::VRacer,
    InstalledWhen::InstallDirExists,
);
/// The registry path has a doubled backslash.
pub const CONDOR2: TemplateGame = condor("Software\\\\Condor2");
pub const CONDOR3: TemplateGame = condor("Software\\\\Condor3");
pub const FSX: TemplateGame = simconnect(UserFolder::Profile, "APPDATA\\Roaming\\Microsoft\\FSX", PortKey::Fsx);
pub const FS2020_STEAM: TemplateGame =
    simconnect(UserFolder::Profile, "APPDATA\\Roaming\\Microsoft Flight Simulator", PortKey::Fsx);
pub const FS2020_STORE: TemplateGame =
    simconnect(UserFolder::LocalAppData, "Packages\\Microsoft.FlightSimulator_8wekyb3d8bbwe\\LocalCache", PortKey::Fsx);
pub const FS2024_STEAM: TemplateGame =
    simconnect(UserFolder::Profile, "APPDATA\\Roaming\\Microsoft Flight Simulator 2024", PortKey::Fsx);
pub const FS2024_STORE: TemplateGame = simconnect(
    UserFolder::Profile,
    "APPDATA\\Local\\Packages\\Microsoft.Limitless_8wekyb3d8bbwe\\LocalCache",
    PortKey::Fsx,
);
pub const P3D_V4: TemplateGame = simconnect(UserFolder::AppData, "Lockheed Martin\\Prepar3D v4", PortKey::P3d);
pub const P3D_V5: TemplateGame = simconnect(UserFolder::AppData, "Lockheed Martin\\Prepar3D v5", PortKey::P3d);
pub const P3D_V6: TemplateGame =
    simconnect(UserFolder::Profile, "APPDATA\\Roaming\\Lockheed Martin\\Prepar3D v6", PortKey::P3d);
pub const DCS: TemplateGame = dcs("Saved Games\\DCS", Encoding::Ansi, Encoding::Ansi);
/// DCS Beta: `patch()` reads the template in the ANSI code page and writes UTF-8; `patched()` reads both as UTF-8.
pub const DCS_BETA: TemplateGame = dcs("Saved Games\\DCS.openbeta", Encoding::Utf8, Encoding::Ansi);
/// Classic DCS export (not on the Games page; used by configuration migration v214).
pub const DCS_LEGACY: TemplateGame = TemplateGame {
    install_dir: InstallDir::User(UserPath::new(UserFolder::Profile, "Saved Games\\DCS")),
    settings_dir: "Scripts",
    file: "Export.lua",
    template: "Export.lua",
    port: PortKey::Dcs,
    encoding: Encoding::Utf8,
    patch_template_encoding: Encoding::Utf8,
    installed_when: InstalledWhen::InstallDirExists,
    create_folders: CreateFolders::BeforeWrite,
};

/// The modules that substitute raw port text: the Game.ini, ExtraConfig.xml, telemetry.json, UDP.ini,
/// SimConnect.xml and SRSExport.lua users. The list is explicit; other modules are not included.
const LIVE_TEXT_PORT_GAMES: [TemplateGame; 16] = [
    KARTKRAFT,
    THE_CREW,
    THE_CREW_2,
    VRACER,
    CONDOR2,
    CONDOR3,
    FSX,
    FS2020_STEAM,
    FS2020_STORE,
    FS2024_STEAM,
    FS2024_STORE,
    P3D_V4,
    P3D_V5,
    P3D_V6,
    DCS,
    DCS_BETA,
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::library::{Library, BUILTIN};
    use crate::patch::prefix::WinePrefix;
    use crate::patch::testing::{no_module_ports, ports, prefix_fixture, scratch};

    const UDP_INI: &str = "[General]\nEnabled=1\n\n[Connection]\nHost=127.0.0.1\nPort=^port^\n\n[Misc]\nSendIntervalMs=1\nExtendedData=0\nLogToFile=0";

    fn library(tag: &str) -> Library {
        let dir = scratch(tag);
        fs::write(dir.join("UDP.ini"), UDP_INI).unwrap();
        fs::write(dir.join("ExtraConfig.xml"), "<ExtraConfig\nphys_out_sim=\"127.0.0.1:^port^\"\n/>").unwrap();
        fs::write(dir.join("SRSExport.lua"), "local port = tonumber(\"^port^\")\n").unwrap();
        Library::Folder(dir)
    }

    /// Template modules read the configured port on every call; they never consult loaded module ports.
    fn environment<'a>(
        prefix: &'a WinePrefix,
        library: &'a Library,
        lookup: &'a dyn super::super::env::PortLookup,
    ) -> PatchEnvironment<'a> {
        PatchEnvironment { prefix: Some(prefix), steam: None, library, ports: lookup, loaded_ports: no_module_ports() }
    }

    #[test]
    fn dcsbeta_native_utf8_read_error_precedes_template_and_port_lookup() {
        use crate::patch::Utf8ReadDecodeError;
        use std::cell::Cell;
        let prefix = prefix_fixture("dcsbeta-native-utf8");
        let folder = prefix.profile().join("Saved Games/DCS.openbeta/Scripts/Hooks");
        fs::create_dir_all(&folder).unwrap();
        let file = folder.join("SRSExport.lua");
        let original = b"-- user edit\r\n\xffTAIL";
        fs::write(&file, original).unwrap();
        let library_dir = scratch("dcsbeta-native-utf8-lib");
        let library = Library::Folder(library_dir.clone());
        let reads = Cell::new(0);
        let lookup = |_: PortKey| {
            reads.set(reads.get() + 1);
            None
        };
        let env = environment(&prefix, &library, &lookup);
        let error = DCS_BETA.patched(&env).unwrap_err();
        let PatchError::Io(io) = &error else { panic!("target text error") };
        let native = io.get_ref().unwrap().downcast_ref::<Utf8ReadDecodeError>().unwrap();
        assert_eq!(native.input(), original);
        assert_eq!((native.start(), native.end()), (14, 15));
        assert_eq!(reads.get(), 0, "invalid target precedes missing library and port");
        assert_eq!(fs::read(&file).unwrap(), original);

        fs::write(&file, b"valid target\n").unwrap();
        fs::write(library_dir.join("SRSExport.lua"), b"port=^port^\r\n\xe2\x82").unwrap();
        let error = DCS_BETA.patched(&env).unwrap_err();
        assert_eq!(
            error.native_exception(),
            Some((
                "UnicodeDecodeError",
                "'utf-8' codec can't decode bytes in position 13-14: unexpected end of data".into()
            ))
        );
        assert_eq!(reads.get(), 0, "invalid template precedes port lookup");
        fs::write(library_dir.join("SRSExport.lua"), b"port=^port^\n").unwrap();
        assert!(matches!(DCS_BETA.patched(&env), Err(PatchError::MissingPort(PortKey::Dcs))));
        assert_eq!(reads.get(), 1);
    }

    #[test]
    fn kartkraft_native_whole_read_errors_keep_target_template_and_port_order() {
        use std::cell::Cell;

        let prefix = prefix_fixture("kartkraft-native-read");
        let file = prefix.local_app_data().join("project_k/Saved/Config/WindowsNoEditor/Game.ini");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        let library_dir = scratch("kartkraft-native-read-lib");
        let library = Library::Folder(library_dir.clone());
        let reads = Cell::new(0);
        let lookup = |_: PortKey| {
            reads.set(reads.get() + 1);
            Some(10000)
        };
        let env = environment(&prefix, &library, &lookup);

        // A target decode error wins over the missing template and skips its port lookup.
        fs::write(&file, b"target\r\n\x81TAIL").unwrap();
        let error = KARTKRAFT.patched(&env).unwrap_err();
        assert_eq!(
            error.native_exception(),
            Some((
                "UnicodeDecodeError",
                "'charmap' codec can't decode byte 0x81 in position 8: character maps to <undefined>".into()
            ))
        );
        assert_eq!(reads.get(), 0);

        let valid_target = b"port=10000\r\ncaf\xe9";
        fs::write(&file, valid_target).unwrap();
        fs::write(library_dir.join("Game.ini"), b"template\r\n\x8dTAIL").unwrap();
        let error = KARTKRAFT.patched(&env).unwrap_err();
        assert_eq!(
            error.native_exception(),
            Some((
                "UnicodeDecodeError",
                "'charmap' codec can't decode byte 0x8d in position 10: character maps to <undefined>".into()
            ))
        );
        assert_eq!(reads.get(), 0);

        fs::write(library_dir.join("Game.ini"), b"port=^port^\ncaf\xe9").unwrap();
        assert!(KARTKRAFT.patched(&env).unwrap());
        assert_eq!(reads.get(), 1);
        assert_eq!(fs::read(&file).unwrap(), valid_target);
    }

    /// The Crew patch / patched: backup by move, template with the port, CRLF on disk, text-mode comparison.
    #[test]
    fn native_crew_patched_retains_validated_ini_port_text() {
        for (game, folder) in [(THE_CREW, "The Crew"), (THE_CREW_2, "The Crew 2")] {
            let prefix = prefix_fixture("native-crew-port-compare");
            let library = library("native-crew-port-compare-lib");
            let dir = prefix.documents().join(folder);
            fs::create_dir_all(&dir).unwrap();
            for (ini, expected) in [("001337", "001337"), ("+001337", "+001337"), ("' 001337 '", " 001337 ")] {
                let raw = crate::import::configobj::parse(&format!("[NETWORK_PORT]\nthecrew = {ini}\n")).unwrap();
                let (settings, _) = crate::settings::Settings::from_legacy(&raw);
                assert_eq!(settings.network_port("thecrew"), Some(1337));
                let content = format!("<ExtraConfig\r\nphys_out_sim=\"127.0.0.1:{expected}\"\r\n/>");
                fs::write(dir.join("ExtraConfig.xml"), content).unwrap();
                assert!(game.patched(&environment(&prefix, &library, &settings)).unwrap(), "{folder} {ini}");
            }
        }
    }

    #[test]
    fn native_crew_patch_retains_raw_port_text_after_backup() {
        for (game, folder) in [(THE_CREW, "The Crew"), (THE_CREW_2, "The Crew 2")] {
            let prefix = prefix_fixture("native-crew-port-patch");
            let library = library("native-crew-port-patch-lib");
            let dir = prefix.documents().join(folder);
            fs::create_dir_all(&dir).unwrap();
            for (ini, expected) in [("001337", "001337"), ("+001337", "+001337"), ("' 001337 '", " 001337 ")] {
                let raw = crate::import::configobj::parse(&format!("[NETWORK_PORT]\nthecrew = {ini}\n")).unwrap();
                let (settings, _) = crate::settings::Settings::from_legacy(&raw);
                fs::write(dir.join("ExtraConfig.xml"), b"previous settings").unwrap();
                let env = environment(&prefix, &library, &settings);
                assert!(game.patch(&env).unwrap());
                assert_eq!(fs::read(dir.join("ExtraConfigsrs_bck.xml")).unwrap(), b"previous settings");
                assert_eq!(
                    fs::read_to_string(dir.join("ExtraConfig.xml")).unwrap(),
                    format!("<ExtraConfig\r\nphys_out_sim=\"127.0.0.1:{expected}\"\r\n/>")
                );
                assert!(game.patched(&env).unwrap());
            }
        }
    }

    #[test]
    fn native_template_port_text_covers_all_live_paths_and_encodings() {
        let games = [
            KARTKRAFT,
            THE_CREW,
            THE_CREW_2,
            VRACER,
            CONDOR2,
            CONDOR3,
            FSX,
            FS2020_STEAM,
            FS2020_STORE,
            FS2024_STEAM,
            FS2024_STORE,
            P3D_V4,
            P3D_V5,
            P3D_V6,
            DCS,
            DCS_BETA,
        ];
        for game in games {
            let mut prefix = prefix_fixture("native-template-family");
            fs::write(prefix.root().join("user.reg"), "[Software\\\\Condor2] 1\n\"InstallDir\"=\"C:\\\\Condor2\"\n[Software\\\\Condor3] 1\n\"InstallDir\"=\"C:\\\\Condor3\"\n").unwrap();
            prefix = WinePrefix::open(prefix.root().to_owned());
            let raw =
                crate::import::configobj::parse(&format!("[NETWORK_PORT]\n{} = +001337\n", game.port.config_key()))
                    .unwrap();
            let (settings, _) = crate::settings::Settings::from_legacy(&raw);
            let env = environment(&prefix, &BUILTIN, &settings);
            let file = game.settings_file(&env).unwrap();
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(&file, b"previous settings").unwrap();
            let expected =
                BUILTIN.read_text(game.template, game.patch_template_encoding).unwrap().replace("^port^", "+001337");
            assert!(expected.contains("+001337"));
            assert!(game.patch(&env).unwrap(), "{}", file.display());
            assert_eq!(fs::read(backup_path(&file, game.file)).unwrap(), b"previous settings");
            assert_eq!(text::read_text(&file, game.encoding).unwrap(), expected, "{}", file.display());
            assert!(game.patched(&env).unwrap(), "{}", file.display());
        }
    }

    #[test]
    fn native_crew_template_read_precedes_text_port_lookup() {
        use crate::patch::PortLookup;
        use std::cell::Cell;
        struct Lookup(Cell<usize>);
        impl PortLookup for Lookup {
            fn port(&self, _: PortKey) -> Option<u16> {
                panic!("Crew must use the text accessor")
            }
            fn port_text(&self, key: PortKey) -> Option<String> {
                assert_eq!(key, PortKey::TheCrew);
                self.0.set(self.0.get() + 1);
                Some("001337".to_owned())
            }
        }
        for (game, folder) in [(THE_CREW, "The Crew"), (THE_CREW_2, "The Crew 2")] {
            let prefix = prefix_fixture("native-crew-text-read-order");
            let library = Library::Folder(scratch("native-crew-text-read-order-lib"));
            let Library::Folder(library_dir) = &library else { unreachable!() };
            let dir = prefix.documents().join(folder);
            fs::create_dir_all(&dir).unwrap();
            let file = dir.join("ExtraConfig.xml");
            let lookup = Lookup(Cell::new(0));
            let env = environment(&prefix, &library, &lookup);
            fs::write(&file, b"\x81").unwrap();
            assert!(game.patched(&env).unwrap_err().native_exception().is_some());
            assert_eq!(lookup.0.get(), 0);
            fs::write(&file, b"previous").unwrap();
            assert!(game.patched(&env).is_err(), "missing template precedes text lookup");
            assert_eq!(lookup.0.get(), 0);
            assert!(game.patch(&env).is_err());
            assert_eq!(fs::read(dir.join("ExtraConfigsrs_bck.xml")).unwrap(), b"previous");
            assert!(!file.exists());
            assert_eq!(lookup.0.get(), 0);
            fs::write(library_dir.join("ExtraConfig.xml"), b"port=^port^\x81").unwrap();
            assert!(game.patch(&env).unwrap_err().native_exception().is_some());
            assert_eq!(lookup.0.get(), 0);
            fs::write(library_dir.join("ExtraConfig.xml"), b"port=^port^\n").unwrap();
            assert!(game.patch(&env).unwrap());
            assert_eq!(lookup.0.get(), 1);
            assert_eq!(fs::read(&file).unwrap(), b"port=001337\r\n");
            assert!(game.patched(&env).unwrap());
            assert_eq!(lookup.0.get(), 2);
        }
    }

    #[test]
    fn native_template_text_accessor_keeps_legacy_and_numeric_fallbacks() {
        use crate::patch::PortLookup;
        struct Lookup;
        impl PortLookup for Lookup {
            fn port(&self, _: PortKey) -> Option<u16> {
                Some(4321)
            }
            fn port_text(&self, _: PortKey) -> Option<String> {
                panic!("not a source-audited live template")
            }
        }
        let prefix = prefix_fixture("native-crew-other-template");
        let library = Library::Folder(scratch("native-crew-other-template-lib"));
        let Library::Folder(dir) = &library else { unreachable!() };
        fs::write(dir.join("Export.lua"), b"port=^port^").unwrap();
        fs::write(dir.join("unverified.ini"), b"port=^port^").unwrap();
        let env = environment(&prefix, &library, &Lookup);
        assert_eq!(DCS_LEGACY.filled_template(&env, Encoding::Ansi).unwrap(), "port=4321");
        let unverified = TemplateGame { template: "unverified.ini", ..KARTKRAFT };
        assert_eq!(unverified.filled_template(&env, Encoding::Ansi).unwrap(), "port=4321");
        let numeric = ports(4321);
        let env = environment(&prefix, &BUILTIN, &numeric);
        for game in LIVE_TEXT_PORT_GAMES {
            let filled = game.filled_template(&env, game.patch_template_encoding).unwrap();
            assert!(filled.contains("4321"));
            assert!(!filled.contains("^port^"));
        }
    }

    /// The Crew patch / patched: backup by move, template with the port, CRLF on disk, text-mode comparison.
    #[test]
    fn the_crew() {
        let prefix = prefix_fixture("tpl-crew");
        let library = library("tpl-crew-lib");
        let lookup = ports(1234);
        let env = environment(&prefix, &library, &lookup);
        assert!(!THE_CREW.installed(&env));
        let dir = prefix.documents().join("The Crew");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("ExtraConfig.xml"), "old").unwrap();
        assert!(THE_CREW.installed(&env));
        assert!(!THE_CREW.patched(&env).unwrap());
        THE_CREW.patch(&env).unwrap();
        assert_eq!(fs::read_to_string(dir.join("ExtraConfigsrs_bck.xml")).unwrap(), "old");
        assert_eq!(
            fs::read_to_string(dir.join("ExtraConfig.xml")).unwrap(),
            "<ExtraConfig\r\nphys_out_sim=\"127.0.0.1:1234\"\r\n/>"
        );
        assert!(THE_CREW.patched(&env).unwrap());
        let other_port = ports(9);
        assert!(!THE_CREW.patched(&environment(&prefix, &library, &other_port)).unwrap());
    }

    /// Condor installed / patch: install folder from the registry (doubled backslash in the key).
    #[test]
    fn condor_from_registry() {
        let prefix_root = scratch("tpl-condor");
        fs::create_dir_all(prefix_root.join("drive_c/Condor2/Settings")).unwrap();
        fs::write(prefix_root.join("drive_c/Condor2/Settings/UDP.ini"), "x").unwrap();
        fs::write(prefix_root.join("user.reg"), "[Software\\\\Condor2] 1\n\"InstallDir\"=\"C:\\\\Condor2\"\n").unwrap();
        let prefix = WinePrefix::open(prefix_root.clone());
        let library = library("tpl-condor-lib");
        let lookup = ports(55278);
        let env = environment(&prefix, &library, &lookup);
        assert!(CONDOR2.installed(&env));
        assert!(!CONDOR3.installed(&env));
        CONDOR2.patch(&env).unwrap();
        let written = fs::read_to_string(prefix_root.join("drive_c/Condor2/Settings/UDP.ini")).unwrap();
        assert!(written.contains("Port=55278\r\n"));
        assert!(CONDOR2.patched(&env).unwrap());
        assert!(matches!(CONDOR3.patch(&env), Err(PatchError::NotInstalled)));
    }

    /// DCS patch: Scripts and Hooks created before the backup; patched needs the Hooks file.
    #[test]
    fn dcs_creates_hooks() {
        let prefix = prefix_fixture("tpl-dcs");
        let library = library("tpl-dcs-lib");
        let lookup = ports(41230);
        let env = environment(&prefix, &library, &lookup);
        let saved = prefix.profile().join("Saved Games/DCS");
        fs::create_dir_all(&saved).unwrap();
        assert!(DCS.installed(&env));
        assert!(!DCS.patched(&env).unwrap());
        DCS.patch(&env).unwrap();
        assert_eq!(
            fs::read_to_string(saved.join("Scripts/Hooks/SRSExport.lua")).unwrap(),
            "local port = tonumber(\"41230\")\r\n"
        );
        assert!(DCS.patched(&env).unwrap());
    }

    /// The DCS Beta patch reads the library SRSExport.lua as cp1252 and writes it as
    /// UTF-8; its patched check reads both as UTF-8. A UTF-8 "é" in the template is written as "Ã©"
    /// (bytes C3 83 C2 A9), so patched() stays false; a byte cp1252 leaves undefined (0x81) fails the patch.
    #[test]
    fn dcs_beta_double_encodes_the_template() {
        let prefix = prefix_fixture("tpl-dcsbeta");
        let dir = scratch("tpl-dcsbeta-lib");
        fs::write(dir.join("SRSExport.lua"), "-- caf\u{e9} ^port^\n").unwrap();
        let library = Library::Folder(dir.clone());
        let lookup = ports(41230);
        let env = environment(&prefix, &library, &lookup);
        let saved = prefix.profile().join("Saved Games/DCS.openbeta");
        fs::create_dir_all(&saved).unwrap();
        DCS_BETA.patch(&env).unwrap();
        let written = saved.join("Scripts/Hooks/SRSExport.lua");
        assert_eq!(fs::read(&written).unwrap(), b"-- caf\xc3\x83\xc2\xa9 41230\r\n");
        assert!(!DCS_BETA.patched(&env).unwrap());

        fs::write(dir.join("SRSExport.lua"), "-- ^port^\n").unwrap();
        DCS_BETA.patch(&env).unwrap();
        assert!(DCS_BETA.patched(&env).unwrap(), "ASCII templates are unaffected");

        fs::write(dir.join("SRSExport.lua"), b"-- \x81 ^port^\n").unwrap();
        assert!(DCS_BETA.patch(&env).is_err());
    }

    /// A template that cannot be read fails the patch after the game's file was moved to its backup;
    /// the classic DCS export creates `Scripts` only after the read.
    #[test]
    fn unreadable_template_after_the_move() {
        let prefix = prefix_fixture("tpl-missing");
        let library = Library::Folder(scratch("tpl-missing-lib"));
        let lookup = ports(1);
        let env = environment(&prefix, &library, &lookup);
        let dir = prefix.documents().join("The Crew 2");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("ExtraConfig.xml"), "old").unwrap();
        assert!(THE_CREW_2.patch(&env).is_err());
        assert!(!dir.join("ExtraConfig.xml").exists());
        assert_eq!(fs::read_to_string(dir.join("ExtraConfigsrs_bck.xml")).unwrap(), "old");

        let saved = prefix.profile().join("Saved Games/DCS");
        fs::create_dir_all(&saved).unwrap();
        assert!(DCS_LEGACY.patch(&env).is_err());
        assert!(!saved.join("Scripts").exists());
    }

    /// A move that fails is `Ok(false)`, not an error; the game's file stays.
    #[test]
    fn failed_move_returns_false() {
        let prefix = prefix_fixture("tpl-move");
        let lookup = ports(1);
        let env = environment(&prefix, &BUILTIN, &lookup);
        let dir = prefix.documents().join("The Crew");
        fs::create_dir_all(dir.join("ExtraConfigsrs_bck.xml/occupied")).unwrap();
        fs::write(dir.join("ExtraConfig.xml"), "old").unwrap();
        assert!(!THE_CREW.patch(&env).unwrap());
        assert_eq!(fs::read_to_string(dir.join("ExtraConfig.xml")).unwrap(), "old");
    }

    /// Every template module with OpenRigCue's built-in library and no SRS installation: the game file is the
    /// filled-in built-in template (CRLF on disk), the old file is the `srs_bck` backup, and `patched()` holds.
    #[test]
    fn builtin_library_patches_every_template_game() {
        let library = BUILTIN;
        let games = [
            KARTKRAFT,
            THE_CREW,
            THE_CREW_2,
            VRACER,
            FSX,
            FS2020_STEAM,
            FS2020_STORE,
            FS2024_STEAM,
            FS2024_STORE,
            P3D_V4,
            P3D_V5,
            P3D_V6,
            DCS,
            DCS_BETA,
            DCS_LEGACY,
        ];
        for (index, game) in games.iter().enumerate() {
            let prefix = prefix_fixture(&format!("tpl-builtin-{index}"));
            let lookup = ports(4321);
            let env = environment(&prefix, &library, &lookup);
            let InstallDir::User(dir) = game.install_dir else { unreachable!() };
            fs::create_dir_all(dir.resolve(&prefix)).unwrap();
            assert!(game.installed(&env), "{}", game.file);
            assert!(!game.patched(&env).unwrap(), "{}", game.file);
            game.patch(&env).unwrap();
            assert!(game.patched(&env).unwrap(), "{}", game.file);
            let file = game.settings_file(&env).unwrap();
            let written = fs::read_to_string(&file).unwrap();
            let builtin = std::str::from_utf8(crate::patch::library::file(game.template).unwrap()).unwrap();
            assert_eq!(written, builtin.replace("^port^", "4321").replace('\n', "\r\n"), "{}", game.file);
            game.patch(&env).unwrap();
            let backup = backup_path(&file, game.file);
            assert_eq!(fs::read_to_string(backup).unwrap(), written, "{}", game.file);
        }
    }
}
