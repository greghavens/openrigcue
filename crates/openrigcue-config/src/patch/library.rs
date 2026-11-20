//! Embedded game configuration templates and OpenRigCue telemetry plugins.
//! Configuration actions read the embedded bytes directly, then copy or fill
//! in the files required by the selected game.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::text::{self, Encoding};

/// One library file: the name the patch modules ask for and its contents.
struct LibraryFile {
    name: &'static str,
    contents: &'static [u8],
    modified: u64,
}

macro_rules! library_bytes {
    ($name:literal) => {
        include_bytes!(concat!("builtin/", $name))
    };
    ($name:literal, $source:literal) => {
        include_bytes!(concat!("builtin/", $source))
    };
}

macro_rules! library {
    ($($name:literal $(from $source:literal)? => $modified:literal),* $(,)?) => {
        &[$(LibraryFile { name: $name, contents: library_bytes!($name $(, $source)?), modified: $modified }),*]
    };
}

/// Game-compatible filenames are kept when a game or an existing configuration
/// refers to them. DLLs are built from Rust; DCS scripts and Farming Simulator
/// scripts, descriptors and artwork are authored OpenRigCue assets:
/// - `SimConnect.xml` (FSX, Prepar3D v4/v5/v6, Flight Simulator 2020/2024), `UDP.ini` (Condor 2/3), `Game.ini`
///   (KartKraft), `ExtraConfig.xml` (The Crew 1/2), `telemetry.json` (V-Racer Hoverbike), `SRSExport.lua` (DCS
///   World, DCS World Beta hook), `Export.lua` (classic DCS export): templates with `^port^`.
/// - `wrc_srs.json` (EA Sports WRC), `CustomPluginVariables.JSON` (Le Mans Ultimate), `smc.cfg` (Warplanes WW1 /
///   Battles over Pacific): copied as they are.
/// - `SRS_Rfactor1.dll` (rFactor 1 and the games built on it), `rFactor2SharedMemoryMapPlugin64.dll` (rFactor 2,
///   Le Mans Ultimate), `ets2-telemetry.dll` (Euro Truck Simulator 2, American Truck Simulator;
///   `ets2-telemetry_32.dll` is also embedded), `Aerofly_FS_2_GamePlugin_Telemetry.dll`
///   (Aerofly FS 2), `FS19_TelemetryMod.zip` / `FS22_TelemetryMod.zip` / `FS25_TelemetryMod.zip` (Farming
///   Simulator 19/22/25): game-side telemetry plugins.
const FILES: &[LibraryFile] = library!(
    "SimConnect.xml" => 1781490864,
    "UDP.ini" => 1781490864,
    "Game.ini" => 1781490864,
    "ExtraConfig.xml" => 1781490864,
    "telemetry.json" => 1781490864,
    "SRSExport.lua" from "openrigcue-dcs-hook.lua" => 1781490864,
    "Export.lua" from "openrigcue-dcs-export.lua" => 1781490864,
    "wrc_srs.json" => 1781490864,
    "CustomPluginVariables.JSON" => 1781490864,
    "smc.cfg" => 1781490864,
    "SRS_Rfactor1.dll" => 1781490862,
    "rFactor2SharedMemoryMapPlugin64.dll" => 1781490862,
    "ets2-telemetry.dll" => 1781490862,
    "ets2-telemetry_32.dll" => 1781490862,
    "Aerofly_FS_2_GamePlugin_Telemetry.dll" => 1781490862,
    "FS19_TelemetryMod.zip" => 1781490864,
    "FS22_TelemetryMod.zip" => 1781490864,
    "FS25_TelemetryMod.zip" => 1781490864,
);

/// Where the patch modules read library files from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Library {
    /// The files built into OpenRigCue.
    Builtin,
    /// A folder holding the files (tests that need other library contents).
    #[cfg(test)]
    Folder(PathBuf),
}

/// The built-in library.
pub const BUILTIN: Library = Library::Builtin;

impl Library {
    /// The bytes of a library file; a file the library lacks is a not-found error.
    fn contents(&self, name: &str) -> io::Result<std::borrow::Cow<'static, [u8]>> {
        match self {
            Library::Builtin => file(name)
                .map(std::borrow::Cow::Borrowed)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no library file {name}"))),
            #[cfg(test)]
            Library::Folder(dir) => fs::read(dir.join(name)).map(std::borrow::Cow::Owned),
        }
    }

    /// The text of a library file in the given encoding.
    pub fn read_text(&self, name: &str, encoding: Encoding) -> io::Result<String> {
        text::decode_text(&self.contents(name)?, &display_path(name), encoding)
    }

    /// Copies a library file to `destination`: the library file is opened first (a missing one fails
    /// before the destination is touched), then the destination is created or emptied and written.
    pub fn copy_to(&self, name: &str, destination: &Path) -> io::Result<()> {
        let contents = self.contents(name)?;
        fs::write(destination, contents)
    }

    /// Shallow comparison of a game file with a library file: both are statted (errors fail), unequal unless both
    /// are regular files. Equal type/size/mtime signatures skip content reads; otherwise sizes and bytes decide.
    /// The embedded library records a UTC modification time per file.
    pub fn same_as(&self, game_file: &Path, name: &str) -> io::Result<bool> {
        match self {
            Library::Builtin => {
                let game = crate::filecmp::signature(game_file)?;
                let item = FILES
                    .iter()
                    .find(|file| file.name == name)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no library file {name}")))?;
                crate::filecmp::compare_embedded(game_file, game, name, item.contents, item.modified)
            }
            #[cfg(test)]
            Library::Folder(dir) => crate::filecmp::cmp(game_file, &dir.join(name), true),
        }
    }
}

/// The Windows-style path of a library file, for error messages.
fn display_path(name: &str) -> PathBuf {
    Path::new("libraries").join(name)
}

/// Contents of a built-in library file.
pub fn file(name: &str) -> Option<&'static [u8]> {
    FILES.iter().find(|file| file.name == name).map(|file| file.contents)
}

/// Names of the built-in library files.
pub fn file_names() -> impl Iterator<Item = &'static str> {
    FILES.iter().map(|file| file.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(name: &str) -> &'static str {
        std::str::from_utf8(file(name).unwrap()).unwrap()
    }

    /// The SimConnect configuration has six server blocks; only the local IPv4 one on the
    /// telemetry port is enabled. The file keeps its trailing spaces and has no final newline.
    #[test]
    fn simconnect_enables_one_local_ipv4_server() {
        let text = text("SimConnect.xml");
        assert_eq!(file("SimConnect.xml").unwrap().len(), 1926);
        assert_eq!(text.matches("<SimConnect.Comm>").count(), 6);
        assert_eq!(text.matches("<Disabled>True</Disabled>").count(), 5);
        assert!(text.contains(
            "<Disabled>False</Disabled> \n    <Protocol>IPv4</Protocol> \n    <Scope>local</Scope> \n    \
             <MaxClients>64</MaxClients> \n    <Address>127.0.0.1</Address> \n    <Port>^port^</Port>\n"
        ));
        assert!(text.ends_with("</SimBase.Document> "));
    }

    /// Every file a patch module fills in has the port placeholder; the copied files have none.
    #[test]
    fn port_placeholders() {
        for name in ["SimConnect.xml", "UDP.ini", "Game.ini", "ExtraConfig.xml", "telemetry.json", "Export.lua"] {
            assert_eq!(text(name).matches("^port^").count(), 1, "{name}");
        }
        assert!(text("SRSExport.lua").contains("port = tonumber(\"^port^\")"));
        assert!(text("Export.lua").contains("port = tonumber(\"^port^\")"));
        for name in ["wrc_srs.json", "CustomPluginVariables.JSON", "smc.cfg"] {
            assert!(!text(name).contains("^port^"), "{name}");
        }
    }

    /// Settings the games read from the template files.
    #[test]
    fn template_settings() {
        assert_eq!(
            text("UDP.ini"),
            "[General]\nEnabled=1\n\n[Connection]\nHost=127.0.0.1\nPort=^port^\n\n[Misc]\nSendIntervalMs=1\n\
             ExtendedData=0\nLogToFile=0"
        );
        assert!(text("Game.ini").starts_with(
            "[/Script/project_k.UDPManager]\nbConfigOverride=True\nOutputEndpoints=127.0.0.1:^port^\n\
             bEnableOutputStandard=True\n"
        ));
        assert!(text("ExtraConfig.xml").contains("phys_out_sim=\"127.0.0.1:^port^\""));
        assert!(text("ExtraConfig.xml").contains("phys_out_sim_rate=\"3\""));
        assert!(text("ExtraConfig.xml").contains("phys_out_nitro_lean=\"0.5\""));
        assert_eq!(text("smc.cfg"), "ip=127.0.0.1\nport=50000\nmmf=ww1\n");
    }

    /// The JSON files parse to the expected values: the EA WRC structure (51 session channels) and the Le Mans
    /// Ultimate plugin entry with rFactor 2's `" Enabled"` spelling.
    #[test]
    fn json_files() {
        let wrc: serde_json::Value = serde_json::from_str(text("wrc_srs.json")).unwrap();
        assert_eq!(wrc["id"], "wrc_srs");
        assert_eq!(wrc["versions"], serde_json::json!({"schema": 1, "data": 3}));
        assert_eq!(wrc["header"]["channels"], serde_json::json!([]));
        let packet = &wrc["packets"][0];
        assert_eq!(packet["id"], "session_update");
        let channels = packet["channels"].as_array().unwrap();
        assert_eq!(channels.len(), 51);
        assert_eq!(channels[0], "vehicle_gear_index");
        assert_eq!(channels[50], "stage_length");
        let variables: serde_json::Value = serde_json::from_str(text("CustomPluginVariables.JSON")).unwrap();
        assert_eq!(
            variables,
            serde_json::json!({"rFactor2SharedMemoryMapPlugin64.dll": {
                " Enabled": 1, "DebugISIInternals": 0, "DebugOutputLevel": 0,
                "DedicatedServerMapGlobally": 0, "EnableStockCarRulesPlugin": 0}})
        );
    }

    /// Both authored DCS scripts preserve the datagram layout and public callbacks.
    #[test]
    fn dcs_scripts() {
        let layout = "\"%.2f,%.2f,%.2f,%.5f,%.5f,%.5f,%.5f,%.5f,%.5f,%.5f,%.5f,%.5f,%s,%s,%s,%.5f\"";
        for name in ["SRSExport.lua", "Export.lua"] {
            let script = text(name);
            assert!(script.contains(layout), "{name}");
            assert!(script.contains("host = \"127.0.0.1\""), "{name}");
        }
        let hook = text("SRSExport.lua");
        assert!(hook.ends_with("DCS.setUserCallbacks(callbacks)\n"));
        assert!(hook.contains("[\"AJS37\"] = {139,100}"));
        let export = text("Export.lua");
        for function in [
            "LuaExportStart",
            "LuaExportBeforeNextFrame",
            "LuaExportAfterNextFrame",
            "LuaExportStop",
            "LuaExportActivityNextEvent",
        ] {
            assert!(
                export.contains(&format!("function {function}(")) || export.contains(&format!("{function} = ")),
                "{function}"
            );
        }
    }

    /// Delivered plugins have their recorded sizes and container formats (Rust DLLs and rebuilt ZIPs).
    #[test]
    fn plugin_binaries() {
        for (name, size, magic) in [
            ("SRS_Rfactor1.dll", 207_872, &b"MZ"[..]),
            ("rFactor2SharedMemoryMapPlugin64.dll", 287_232, b"MZ"),
            ("ets2-telemetry.dll", 16_896, b"MZ"),
            ("ets2-telemetry_32.dll", 13_824, b"MZ"),
            ("Aerofly_FS_2_GamePlugin_Telemetry.dll", 51_200, b"MZ"),
            ("FS19_TelemetryMod.zip", 4_793, b"PK"),
            ("FS22_TelemetryMod.zip", 4_793, b"PK"),
            ("FS25_TelemetryMod.zip", 4_986, b"PK"),
        ] {
            let contents = file(name).unwrap();
            assert_eq!(contents.len(), size, "{name}");
            assert!(contents.starts_with(magic), "{name}");
        }
        assert_eq!(file_names().count(), 18);
    }

    /// Comparison against a library file: a missing game file is an error, so is a file the library
    /// lacks; a folder is not equal; otherwise size and bytes decide. Copying writes the library bytes.
    #[test]
    fn compare_and_copy() {
        let dir = crate::patch::testing::scratch("library-compare");
        let game_file = dir.join("FS25_TelemetryMod.zip");
        assert!(BUILTIN.same_as(&game_file, "FS25_TelemetryMod.zip").is_err());
        fs::write(&game_file, b"PK").unwrap();
        assert!(!BUILTIN.same_as(&game_file, "FS25_TelemetryMod.zip").unwrap());
        assert!(BUILTIN.same_as(&game_file, "unknown.dll").is_err());
        assert!(!BUILTIN.same_as(&dir, "FS25_TelemetryMod.zip").unwrap());
        BUILTIN.copy_to("FS25_TelemetryMod.zip", &game_file).unwrap();
        assert_eq!(fs::read(&game_file).unwrap(), file("FS25_TelemetryMod.zip").unwrap());
        assert!(BUILTIN.same_as(&game_file, "FS25_TelemetryMod.zip").unwrap());
        let mut changed = fs::read(&game_file).unwrap();
        changed[10] ^= 1;
        fs::write(&game_file, changed).unwrap();
        assert!(!BUILTIN.same_as(&game_file, "FS25_TelemetryMod.zip").unwrap());
        let missing = dir.join("not-written");
        assert!(BUILTIN.copy_to("unknown.dll", &missing).is_err());
        assert!(!missing.exists());
        assert_eq!(BUILTIN.read_text("UDP.ini", Encoding::Ansi).unwrap(), text("UDP.ini"));
    }

    #[test]
    fn builtin_shallow_signature_uses_the_installer_modification_time() {
        use std::fs::{File, FileTimes};
        use std::time::{Duration, UNIX_EPOCH};
        let dir = crate::patch::testing::scratch("library-shallow-builtin");
        let game = dir.join("UDP.ini");
        let mut changed = file("UDP.ini").unwrap().to_vec();
        changed[0] ^= 1;
        fs::write(&game, changed).unwrap();
        let handle = File::options().write(true).open(&game).unwrap();
        let original_mtime = UNIX_EPOCH + Duration::from_secs(1_781_490_864);
        handle.set_times(FileTimes::new().set_modified(original_mtime)).unwrap();
        assert!(BUILTIN.same_as(&game, "UDP.ini").unwrap(), "equal stat signatures skip different bytes");
        handle.set_times(FileTimes::new().set_modified(original_mtime + Duration::from_secs(1))).unwrap();
        assert!(!BUILTIN.same_as(&game, "UDP.ini").unwrap(), "a different time requires content comparison");
        fs::write(&game, file("UDP.ini").unwrap()).unwrap();
        assert!(BUILTIN.same_as(&game, "UDP.ini").unwrap());
    }

    #[test]
    fn library_comparison_reuses_an_outcome_when_both_stat_signatures_stay_unchanged() {
        use std::fs::{File, FileTimes};
        use std::time::{Duration, UNIX_EPOCH};
        let dir = crate::patch::testing::scratch("library-comparison-cache");
        let game = dir.join("UDP.ini");
        let original = file("UDP.ini").unwrap();
        fs::write(&game, original).unwrap();
        let time = UNIX_EPOCH + Duration::from_secs(1_781_490_865);
        let set_time =
            || File::options().write(true).open(&game).unwrap().set_times(FileTimes::new().set_modified(time)).unwrap();
        set_time();
        assert!(BUILTIN.same_as(&game, "UDP.ini").unwrap());
        let mut changed = original.to_vec();
        changed[0] ^= 1;
        fs::write(&game, changed).unwrap();
        set_time();
        assert!(BUILTIN.same_as(&game, "UDP.ini").unwrap(), "the original cache still holds the prior true result");
    }

    #[test]
    fn folder_reference_uses_both_stat_signatures_before_reading() {
        use std::fs::{File, FileTimes};
        use std::time::{Duration, UNIX_EPOCH};
        let dir = crate::patch::testing::scratch("library-shallow-folder");
        let game = dir.join("game.ini");
        let reference = dir.join("reference.ini");
        fs::write(&game, b"abc").unwrap();
        fs::write(&reference, b"xyz").unwrap();
        let time = UNIX_EPOCH + Duration::from_secs(123_456);
        for path in [&game, &reference] {
            File::options().write(true).open(path).unwrap().set_times(FileTimes::new().set_modified(time)).unwrap();
        }
        let library = Library::Folder(dir.clone());
        assert!(library.same_as(&game, "reference.ini").unwrap());
        fs::create_dir(dir.join("directory")).unwrap();
        assert!(!library.same_as(&game, "directory").unwrap(), "a non-regular reference is false, not read");
        assert!(
            library.same_as(&dir, "missing.ini").is_err(),
            "the second stat still raises after a non-file first stat"
        );
    }
}
