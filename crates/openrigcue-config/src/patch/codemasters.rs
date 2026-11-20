//! Codemasters / EA `hardware_settings_config*.xml` telemetry settings.

use std::fs;
use std::path::Path;

use super::env::{PatchEnvironment, PortKey, PortModule, UserFolder, UserPath};
use super::files::backup_path;
use super::text::{self, py_strip, Encoding, Newline};
use super::winpath;
use super::xml::{self, Element};
use super::PatchError;

/// Attributes ignored by the modern comparison (written, but the game may change them).
const IGNORED_BY_COMPARISON: [&str; 2] = ["yourTelemetry", "onlineNames"];
/// Closing tag the VR files must end with.
const DOCUMENT_END: &str = "</hardware_settings_config>";
/// Last line kept when a VR file is repaired.
const REPAIR_MARKER: &str = "ca enabled=";

/// How a game's settings are located and rewritten.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// A `<udp…>` child of the last `<tag>` element, identified by
    /// its port. `numbered` names a new child `udpN` (N = number of `udp*` children with a port); `repair` first
    /// fixes a truncated VR file.
    Udp { numbered: bool, repair: bool },
    /// The attributes of the `<tag>` element itself.
    Classic,
    /// F1 2015: like [`Layout::Udp`] but every attribute is compared and the first
    /// `udp*` child is rewritten whatever its port.
    F12015,
}

/// One game's settings file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodemastersGame {
    /// Path below Documents.
    pub file: &'static str,
    /// Element holding the telemetry settings.
    pub tag: &'static str,
    pub layout: Layout,
    pub port: PortKey,
    /// Attributes after `port`, in the written order.
    pub settings: &'static [(&'static str, &'static str)],
}

impl CodemastersGame {
    fn path(&self, env: &PatchEnvironment<'_>) -> Result<std::path::PathBuf, PatchError> {
        let prefix = env.prefix.ok_or(PatchError::NotInstalled)?;
        Ok(UserPath::new(UserFolder::Documents, self.file).resolve(prefix))
    }

    fn module(&self) -> PortModule {
        PortModule::Codemasters(self.file)
    }

    /// The port `patched()` compares with: the module's loaded port, which the F1 2015 layout reloads first.
    fn port_for_patched(&self, env: &PatchEnvironment<'_>) -> Result<String, PatchError> {
        match self.layout {
            Layout::F12015 => env.reload_port_text(self.module(), self.port),
            Layout::Udp { .. } | Layout::Classic => env.loaded_port_text(self.module(), self.port),
        }
    }

    /// The port `patch()` writes: reloaded first, except for the F1 2015 layout, which writes the
    /// port its last `patched()` (or application start) loaded.
    fn port_for_patch(&self, env: &PatchEnvironment<'_>) -> Result<String, PatchError> {
        match self.layout {
            Layout::F12015 => env.loaded_port_text(self.module(), self.port),
            Layout::Udp { .. } | Layout::Classic => env.reload_port_text(self.module(), self.port),
        }
    }

    fn attributes(&self, port: String) -> Vec<(String, String)> {
        let rest = self.settings.iter().map(|(name, value)| (name.to_string(), value.to_string()));
        std::iter::once(("port".to_string(), port)).chain(rest).collect()
    }

    /// The settings file exists (`installed`).
    pub fn installed(&self, env: &PatchEnvironment<'_>) -> bool {
        self.path(env).is_ok_and(|path| path.is_file())
    }

    /// The file already carries the settings with the module's loaded port; an unreadable file is not patched.
    pub fn patched(&self, env: &PatchEnvironment<'_>) -> Result<bool, PatchError> {
        let port = self.port_for_patched(env)?;
        let Ok(path) = self.path(env) else { return Ok(false) };
        let attributes = self.attributes(port);
        let Ok(root) = fs::read(&path).map_err(PatchError::from).and_then(|bytes| Ok(xml::parse_bytes(&bytes)?)) else {
            return Ok(false);
        };
        Ok(match self.layout {
            Layout::Udp { .. } => udp_patched(&root, self.tag, &attributes, &IGNORED_BY_COMPARISON),
            Layout::F12015 => udp_patched(&root, self.tag, &attributes, &[]),
            Layout::Classic => classic_patched(&root, self.tag, &attributes),
        })
    }

    /// Writes the settings, pretty-printed, after copying the file to its backup; a failed copy or write is
    /// `Ok(false)`, the rest is an error.
    pub fn patch(&self, env: &PatchEnvironment<'_>) -> Result<bool, PatchError> {
        let attributes = self.attributes(self.port_for_patch(env)?);
        let path = self.path(env)?;
        if let Layout::Udp { repair: true, .. } = self.layout {
            repair_truncated(&path)?;
        }
        let mut root = xml::parse_bytes(&fs::read(&path)?)?;
        match self.layout {
            Layout::Udp { numbered, .. } => set_udp_by_port(&mut root, self.tag, attributes, numbered)?,
            Layout::F12015 => set_first_udp(&mut root, self.tag, attributes)?,
            Layout::Classic => set_classic(&mut root, self.tag, attributes),
        }
        xml::indent(&mut root);
        if fs::copy(&path, backup_path(&path, winpath::file_name(self.file))).is_err() {
            return Ok(false);
        }
        Ok(text::write_text(&path, &xml::document_text(&root), Encoding::Utf8, Newline::Windows).is_ok())
    }
}

fn is_udp(element: &Element) -> bool {
    element.tag.starts_with("udp")
}

fn last_with_tag<'e>(root: &'e Element, tag: &str) -> Option<&'e Element> {
    root.children.iter().rev().find(|child| child.tag == tag)
}

/// The first `udp*` child (with a port) carrying our port decides: every compared attribute must be equal.
fn udp_patched(root: &Element, tag: &str, attributes: &[(String, String)], ignored: &[&str]) -> bool {
    let Some(section) = last_with_tag(root, tag) else {
        return false;
    };
    let port = &attributes[0].1;
    let Some(udp) = section.children.iter().filter(|c| is_udp(c)).find(|c| c.attribute("port") == Some(port.as_str()))
    else {
        return false;
    };
    attributes.iter().all(|(name, value)| {
        udp.attribute(name) == Some(value.as_str())
            || (udp.attribute(name).is_some() && ignored.contains(&name.as_str()))
    })
}

/// The first `<tag>` child of the root must carry every attribute.
fn classic_patched(root: &Element, tag: &str, attributes: &[(String, String)]) -> bool {
    root.children
        .iter()
        .find(|child| child.tag == tag)
        .is_some_and(|section| attributes.iter().all(|(name, value)| section.attribute(name) == Some(value.as_str())))
}

fn last_with_tag_mut<'e>(root: &'e mut Element, tag: &str) -> Result<&'e mut Element, PatchError> {
    root.children.iter_mut().rev().find(|child| child.tag == tag).ok_or(PatchError::MissingElement)
}

/// Replaces the attributes of every `udp*` child with our port, or appends a new child.
fn set_udp_by_port(
    root: &mut Element,
    tag: &str,
    attributes: Vec<(String, String)>,
    numbered: bool,
) -> Result<(), PatchError> {
    let section = last_with_tag_mut(root, tag)?;
    let port = attributes[0].1.clone();
    let mut found = false;
    let mut counted = 0;
    for child in section.children.iter_mut().filter(|child| is_udp(child)) {
        let Some(child_port) = child.attribute("port") else {
            continue;
        };
        if child_port == port {
            child.attributes = attributes.clone();
            found = true;
        }
        counted += 1;
    }
    if !found {
        let name = if numbered { format!("udp{counted}") } else { "udp".to_string() };
        section.children.push(Element::new(&name, attributes));
    }
    Ok(())
}

/// Replaces the attributes of the first `udp*` child, or appends `<udp>`.
fn set_first_udp(root: &mut Element, tag: &str, attributes: Vec<(String, String)>) -> Result<(), PatchError> {
    let section = last_with_tag_mut(root, tag)?;
    match section.children.iter_mut().find(|child| is_udp(child)) {
        Some(udp) => udp.attributes = attributes,
        None => section.children.push(Element::new("udp", attributes)),
    }
    Ok(())
}

/// Replaces the attributes of every `<tag>` child of the root, or appends one.
fn set_classic(root: &mut Element, tag: &str, attributes: Vec<(String, String)>) {
    let mut found = false;
    for section in root.children.iter_mut().filter(|child| child.tag == tag) {
        section.attributes = attributes.clone();
        found = true;
    }
    if !found {
        root.children.push(Element::new(tag, attributes));
    }
}

/// Repairs a file whose last line is not the closing tag: keeps the lines up to the first one containing
/// `ca enabled=` (all but the last line otherwise) and appends the closing tag.
fn repair_truncated(path: &Path) -> Result<(), PatchError> {
    let content = text::read_text(path, Encoding::Ansi)?;
    let lines = text::lines_with_ends(&content);
    let Some((last, kept)) = lines.split_last() else {
        return Err(PatchError::MissingElement);
    };
    if py_strip(last) == DOCUMENT_END {
        return Ok(());
    }
    let end = kept.iter().position(|line| line.contains(REPAIR_MARKER)).map_or(kept.len(), |index| index + 1);
    let repaired = format!("{}{DOCUMENT_END}\n", kept[..end].concat());
    text::write_text(path, &repaired, Encoding::Ansi, Newline::Windows)?;
    Ok(())
}

const F1_CLASSIC_2013: &[(&str, &str)] =
    &[("enabled", "true"), ("ip", "127.0.0.1"), ("delay", "10"), ("extradata", "3")];
const F1_CLASSIC_2011: &[(&str, &str)] =
    &[("enabled", "true"), ("ip", "127.0.0.1"), ("delay", "1"), ("extradata", "1")];
const DIRT_CLASSIC: &[(&str, &str)] = &[("enabled", "true"), ("ip", "127.0.0.1"), ("delay", "10"), ("extradata", "1")];
const MOTION_PLATFORM_DIRT: &[(&str, &str)] =
    &[("enabled", "true"), ("extradata", "3"), ("delay", "10"), ("ip", "127.0.0.1")];
const MOTION_PLATFORM_GRID: &[(&str, &str)] =
    &[("enabled", "true"), ("ip", "127.0.0.1"), ("extradata", "3"), ("delay", "1")];
const F1_2016: &[(&str, &str)] = &[("enabled", "true"), ("ip", "127.0.0.1")];
const F1_2017: &[(&str, &str)] =
    &[("enabled", "true"), ("broadcast", "false"), ("ip", "127.0.0.1"), ("sendRate", "60")];

macro_rules! modern_f1 {
    ($year:literal, $rate:literal) => {
        &[
            ("enabled", "true"),
            ("broadcast", "false"),
            ("ip", "127.0.0.1"),
            ("sendRate", $rate),
            ("format", $year),
            ("yourTelemetry", "restricted"),
            ("onlineNames", "off"),
        ]
    };
}

const UDP: Layout = Layout::Udp { numbered: false, repair: false };
const UDP_REPAIRED: Layout = Layout::Udp { numbered: false, repair: true };

const fn game(
    file: &'static str,
    tag: &'static str,
    layout: Layout,
    port: PortKey,
    settings: &'static [(&'static str, &'static str)],
) -> CodemastersGame {
    CodemastersGame { file, tag, layout, port, settings }
}

pub const F12011: CodemastersGame = game(
    "My Games\\FormulaOne2011\\hardwaresettings\\hardware_settings_config.xml",
    "motion",
    Layout::Classic,
    PortKey::Dirt,
    F1_CLASSIC_2011,
);
pub const F12012: CodemastersGame = game(
    "My Games\\FormulaOne2012\\hardwaresettings\\hardware_settings_config.xml",
    "motion",
    Layout::Classic,
    PortKey::Dirt,
    F1_CLASSIC_2011,
);
pub const F12013: CodemastersGame = game(
    "My Games\\FormulaOne2013\\hardwaresettings\\hardware_settings_config.xml",
    "motion",
    Layout::Classic,
    PortKey::F1,
    F1_CLASSIC_2013,
);
pub const F12014: CodemastersGame = game(
    "My Games\\FormulaOne2014\\hardwaresettings\\hardware_settings_config.xml",
    "motion",
    Layout::Classic,
    PortKey::F1,
    F1_CLASSIC_2013,
);
pub const F12015: CodemastersGame = game(
    "My Games\\F1 2015\\hardwaresettings\\hardware_settings_config.xml",
    "motion",
    Layout::F12015,
    PortKey::F1,
    F1_2016,
);
pub const F12016: CodemastersGame = game(
    "My Games\\F1 2016\\hardwaresettings\\hardware_settings_config.xml",
    "motion",
    Layout::Udp { numbered: true, repair: false },
    PortKey::F1,
    F1_2016,
);
pub const F12017: CodemastersGame =
    game("My Games\\F1 2017\\hardwaresettings\\hardware_settings_config.xml", "motion", UDP, PortKey::F1, F1_2017);
pub const F12018: CodemastersGame = game(
    "My Games\\F1 2018\\hardwaresettings\\hardware_settings_config.xml",
    "motion",
    UDP,
    PortKey::F12018,
    &[("enabled", "true"), ("broadcast", "false"), ("ip", "127.0.0.1"), ("sendRate", "60"), ("format", "2018")],
);
pub const F12019: CodemastersGame = game(
    "My Games\\F1 2019\\hardwaresettings\\hardware_settings_config.xml",
    "motion",
    UDP,
    PortKey::F12019,
    &[("enabled", "true"), ("broadcast", "false"), ("ip", "127.0.0.1"), ("sendRate", "60"), ("format", "2019")],
);
pub const F12020: CodemastersGame = game(
    "My Games\\F1 2020\\hardwaresettings\\hardware_settings_config.xml",
    "motion",
    UDP,
    PortKey::F12020,
    &[
        ("enabled", "true"),
        ("broadcast", "false"),
        ("ip", "127.0.0.1"),
        ("sendRate", "60"),
        ("format", "2020"),
        ("yourTelemetry", "restricted"),
    ],
);
pub const F12021: CodemastersGame = game(
    "My Games\\F1 2021\\hardwaresettings\\hardware_settings_config.xml",
    "motion",
    UDP,
    PortKey::F12021,
    &[
        ("enabled", "true"),
        ("broadcast", "false"),
        ("ip", "127.0.0.1"),
        ("sendRate", "20hz"),
        ("format", "2021"),
        ("yourTelemetry", "restricted"),
    ],
);
pub const F12022: CodemastersGame = game(
    "My Games\\F1 22\\hardwaresettings\\hardware_settings_config.xml",
    "motion",
    UDP,
    PortKey::F12022,
    modern_f1!("2022", "20"),
);
pub const F12022_VR: CodemastersGame = game(
    "My Games\\F1 22\\hardwaresettings\\hardware_settings_config_VR.xml",
    "motion",
    UDP,
    PortKey::F12022,
    modern_f1!("2022", "20"),
);
pub const F12023: CodemastersGame = game(
    "My Games\\F1 23\\hardwaresettings\\hardware_settings_config.xml",
    "motion",
    UDP,
    PortKey::F12023,
    modern_f1!("2023", "20"),
);
pub const F12023_VR: CodemastersGame = game(
    "My Games\\F1 23\\hardwaresettings\\hardware_settings_config_VR.xml",
    "motion",
    UDP_REPAIRED,
    PortKey::F12023,
    modern_f1!("2023", "20"),
);
pub const F12024: CodemastersGame = game(
    "My Games\\F1 24\\hardwaresettings\\hardware_settings_config.xml",
    "motion",
    UDP,
    PortKey::F12024,
    modern_f1!("2024", "20"),
);
pub const F12024_VR: CodemastersGame = game(
    "My Games\\F1 24\\hardwaresettings\\hardware_settings_config_vr.xml",
    "motion",
    UDP_REPAIRED,
    PortKey::F12024,
    modern_f1!("2024", "20"),
);
pub const F12025: CodemastersGame = game(
    "My Games\\F1 25\\hardwaresettings\\hardware_settings_config.xml",
    "motion",
    UDP,
    PortKey::F12025,
    modern_f1!("2025", "20"),
);
pub const F12025_VR: CodemastersGame = game(
    "My Games\\F1 25\\hardwaresettings\\hardware_settings_config_vr.xml",
    "motion",
    UDP_REPAIRED,
    PortKey::F12025,
    modern_f1!("2025", "20"),
);
pub const DIRT2: CodemastersGame = game(
    "My Games\\DiRT2\\hardwaresettings\\hardware_settings_config.xml",
    "motion",
    Layout::Classic,
    PortKey::Dirt,
    DIRT_CLASSIC,
);
pub const DIRT3: CodemastersGame = game(
    "My Games\\DiRT3\\hardwaresettings\\hardware_settings_config.xml",
    "motion",
    Layout::Classic,
    PortKey::Dirt,
    DIRT_CLASSIC,
);
pub const DIRT_SHOWDOWN: CodemastersGame = game(
    "My Games\\DiRT Showdown\\hardwaresettings\\hardware_settings_config.xml",
    "motion",
    Layout::Classic,
    PortKey::Dirt,
    DIRT_CLASSIC,
);
pub const GRID2: CodemastersGame = game(
    "My Games\\GRID 2\\hardwaresettings\\hardware_settings_config.xml",
    "motion",
    Layout::Classic,
    PortKey::Grid,
    F1_CLASSIC_2013,
);
pub const DIRT4: CodemastersGame = game(
    "My Games\\DiRT 4\\hardwaresettings\\hardware_settings_config.xml",
    "motion_platform",
    UDP,
    PortKey::Dirt,
    MOTION_PLATFORM_DIRT,
);
pub const DIRT_RALLY: CodemastersGame = game(
    "My Games\\DiRT Rally\\hardwaresettings\\hardware_settings_config.xml",
    "motion_platform",
    UDP,
    PortKey::Dirt,
    MOTION_PLATFORM_DIRT,
);
pub const DIRT_RALLY_VR: CodemastersGame = game(
    "My Games\\DiRT Rally VR\\hardwaresettings\\hardware_settings_config.xml",
    "motion_platform",
    UDP,
    PortKey::Dirt,
    MOTION_PLATFORM_DIRT,
);
pub const DIRT_RALLY2_STEAM: CodemastersGame = game(
    "My Games\\DiRT Rally 2.0\\hardwaresettings\\hardware_settings_config.xml",
    "motion_platform",
    UDP,
    PortKey::Dirt,
    MOTION_PLATFORM_DIRT,
);
pub const DIRT_RALLY2_OCULUS: CodemastersGame = game(
    "My Games\\DiRT Rally 2.0 Oculus\\hardwaresettings\\hardware_settings_config.xml",
    "motion_platform",
    UDP,
    PortKey::Dirt,
    MOTION_PLATFORM_DIRT,
);
pub const DIRT_RALLY2_VR_STEAM: CodemastersGame = game(
    "My Games\\DiRT Rally 2.0\\hardwaresettings\\hardware_settings_config_vr.xml",
    "motion_platform",
    UDP,
    PortKey::Dirt,
    MOTION_PLATFORM_DIRT,
);
pub const DIRT_RALLY2_VR_OCULUS: CodemastersGame = game(
    "My Games\\DiRT Rally 2.0 Oculus\\hardwaresettings\\hardware_settings_config_vr.xml",
    "motion_platform",
    UDP,
    PortKey::Dirt,
    MOTION_PLATFORM_DIRT,
);
pub const GRID2019: CodemastersGame = game(
    "My Games\\GRID (2019)\\hardwaresettings\\hardware_settings_config.xml",
    "motion_platform",
    UDP,
    PortKey::Grid,
    MOTION_PLATFORM_GRID,
);
pub const GRID_LEGENDS: CodemastersGame = game(
    "My Games\\GRID Legends\\hardwaresettings\\hardware_settings_config.xml",
    "motion_platform",
    UDP,
    PortKey::Grid,
    MOTION_PLATFORM_GRID,
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::env::{ModulePorts, PortLookup};
    use crate::patch::prefix::WinePrefix;
    use crate::patch::testing::{ports, prefix_fixture};

    fn setup(tag: &str, game: &CodemastersGame, content: &str) -> (WinePrefix, std::path::PathBuf) {
        let prefix = prefix_fixture(tag);
        let path = UserPath::new(UserFolder::Documents, game.file).resolve(&prefix);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
        (prefix, path)
    }

    fn env<'a>(prefix: &'a WinePrefix, lookup: &'a dyn PortLookup, loaded: &'a ModulePorts) -> PatchEnvironment<'a> {
        PatchEnvironment {
            prefix: Some(prefix),
            steam: None,
            library: &crate::patch::library::BUILTIN,
            ports: lookup,
            loaded_ports: loaded,
        }
    }

    const F1_FILE: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\r\n<hardware_settings_config>\r\n  <motion>\r\n    <udp enabled=\"false\" port=\"20777\" ip=\"127.0.0.1\" />\r\n  </motion>\r\n  <ca enabled=\"true\" />\r\n</hardware_settings_config>\r\n";

    fn native_settings(key: &str, text: &str) -> crate::settings::Settings {
        let parsed = crate::import::configobj::parse(&format!("[NETWORK_PORT]\n{key} = {text}\n")).unwrap();
        let value = parsed.get("NETWORK_PORT").unwrap().as_dict().unwrap().get(key).unwrap().clone();
        let mut settings = crate::settings::Settings::default();
        settings.edit_native(|raw| {
            raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert(key, value);
        });
        settings
    }

    #[test]
    fn native_cached_codemasters_text_survives_import_and_independent_reload() {
        let (prefix, path) = setup(
            "cm-native-cache",
            &DIRT_RALLY2_STEAM,
            "<hardware_settings_config><motion_platform /></hardware_settings_config>",
        );
        let startup = native_settings("dirt", "001337");
        let loaded = ModulePorts::imported(&startup).unwrap();
        DIRT_RALLY2_STEAM.patch(&env(&prefix, &startup, &loaded)).unwrap();
        assert!(fs::read_to_string(&path).unwrap().contains("port=\"001337\""));
        let changed = native_settings("dirt", "+001338");
        let changed_env = env(&prefix, &changed, &loaded);
        assert!(DIRT_RALLY2_STEAM.patched(&changed_env).unwrap(), "patched reads its imported value");
        DIRT_RALLY2_STEAM.patch(&changed_env).unwrap();
        assert!(fs::read_to_string(&path).unwrap().contains("port=\"+001338\""));
        assert!(DIRT_RALLY2_STEAM.patched(&changed_env).unwrap());
        assert_eq!(changed_env.loaded_port(DIRT_RALLY2_STEAM.module(), PortKey::Dirt).unwrap(), 1338);
        assert_eq!(changed_env.loaded_port(DIRT4.module(), PortKey::Dirt).unwrap(), 1337);
    }

    #[test]
    fn native_cached_f12015_reloads_only_when_patched_checks() {
        let (prefix, path) = setup("cm-native-f12015", &F12015, F1_FILE);
        let startup = native_settings("f1", "005555");
        let loaded = ModulePorts::imported(&startup).unwrap();
        let changed = native_settings("f1", "+006666");
        let changed_env = env(&prefix, &changed, &loaded);
        F12015.patch(&changed_env).unwrap();
        assert!(fs::read_to_string(&path).unwrap().contains("port=\"005555\""));
        assert!(!F12015.patched(&changed_env).unwrap());
        F12015.patch(&changed_env).unwrap();
        assert!(fs::read_to_string(&path).unwrap().contains("port=\"+006666\""));
        assert!(F12015.patched(&changed_env).unwrap());
    }

    /// Patch (F1 22): existing <udp> with our port rewritten in config order, backup, CRLF.
    #[test]
    fn modern_patch_rewrites_matching_udp() {
        let (prefix, path) = setup("cm-modern", &F12022, F1_FILE);
        let lookup = ports(20777);
        let loaded = ModulePorts::at_startup(&lookup);
        let env = env(&prefix, &lookup, &loaded);
        assert!(F12022.installed(&env));
        assert!(!F12022.patched(&env).unwrap());
        F12022.patch(&env).unwrap();
        assert_eq!(fs::read_to_string(path.with_file_name("hardware_settings_configsrs_bck.xml")).unwrap(), F1_FILE);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "<?xml version='1.0' encoding='UTF-8'?>\r\n<hardware_settings_config>\r\n    <motion>\r\n        \
<udp port=\"20777\" enabled=\"true\" broadcast=\"false\" ip=\"127.0.0.1\" sendRate=\"20\" format=\"2022\" \
yourTelemetry=\"restricted\" onlineNames=\"off\" />\r\n    </motion>\r\n    <ca enabled=\"true\" />\r\n\
</hardware_settings_config>\r\n"
        );
        assert!(F12022.patched(&env).unwrap());
    }

    /// The patched check ignores yourTelemetry/onlineNames values but not their absence; port identifies.
    #[test]
    fn modern_patched_rules() {
        let changed = F1_FILE.replace(
            "<udp enabled=\"false\" port=\"20777\" ip=\"127.0.0.1\" />",
            "<udp port=\"1\" /><udp port=\"20777\" enabled=\"true\" broadcast=\"false\" ip=\"127.0.0.1\" sendRate=\"20\" format=\"2022\" yourTelemetry=\"public\" onlineNames=\"on\" />",
        );
        let (prefix, _) = setup("cm-rules", &F12022, &changed);
        let lookup = ports(20777);
        let loaded = ModulePorts::at_startup(&lookup);
        assert!(F12022.patched(&env(&prefix, &lookup, &loaded)).unwrap());
        let other_port = ports(20778);
        let other_loaded = ModulePorts::at_startup(&other_port);
        assert!(!F12022.patched(&env(&prefix, &other_port, &other_loaded)).unwrap());
    }

    /// Numbered patch for F1 2016: a new child named udpN after a port change.
    #[test]
    fn numbered_udp_on_port_change() {
        let (prefix, path) = setup("cm-2016", &F12016, F1_FILE);
        let lookup = ports(30000);
        let loaded = ModulePorts::at_startup(&lookup);
        F12016.patch(&env(&prefix, &lookup, &loaded)).unwrap();
        let written = fs::read_to_string(&path).unwrap();
        assert!(written.contains("<udp enabled=\"false\" port=\"20777\" ip=\"127.0.0.1\" />"));
        assert!(written.contains("<udp1 port=\"30000\" enabled=\"true\" ip=\"127.0.0.1\" />"));
    }

    /// Classic layout (Dirt 2): attributes of <motion> replaced, created when missing.
    #[test]
    fn classic_layout() {
        let file = "<hardware_settings_config><audio/></hardware_settings_config>";
        let (prefix, path) = setup("cm-classic", &DIRT2, file);
        let lookup = ports(20777);
        let loaded = ModulePorts::at_startup(&lookup);
        let env = env(&prefix, &lookup, &loaded);
        assert!(!DIRT2.patched(&env).unwrap());
        DIRT2.patch(&env).unwrap();
        assert!(fs::read_to_string(&path)
            .unwrap()
            .contains("<motion port=\"20777\" enabled=\"true\" ip=\"127.0.0.1\" delay=\"10\" extradata=\"1\" />"));
        assert!(DIRT2.patched(&env).unwrap());
    }

    /// The F1 2015 patch rewrites the first udp* child whatever its port; its patched check compares every attribute.
    #[test]
    fn f1_2015_layout() {
        let (prefix, path) = setup("cm-2015", &F12015, F1_FILE);
        let lookup = ports(5555);
        let loaded = ModulePorts::at_startup(&lookup);
        let env = env(&prefix, &lookup, &loaded);
        F12015.patch(&env).unwrap();
        assert!(fs::read_to_string(&path).unwrap().contains("<udp port=\"5555\" enabled=\"true\" ip=\"127.0.0.1\" />"));
        assert!(F12015.patched(&env).unwrap());
    }

    /// VR file repair (F1 23 VR): truncated file cut after the "ca enabled=" line and closed.
    #[test]
    fn vr_file_repair() {
        let truncated = "<hardware_settings_config>\n  <motion>\n  </motion>\n  <ca enabled=\"true\" />\n  <garbage\n";
        let (prefix, path) = setup("cm-repair", &F12023_VR, truncated);
        let lookup = ports(20777);
        let loaded = ModulePorts::at_startup(&lookup);
        F12023_VR.patch(&env(&prefix, &lookup, &loaded)).unwrap();
        assert!(fs::read_to_string(&path).unwrap().contains("<ca enabled=\"true\" />\r\n</hardware_settings_config>"));
    }

    /// A patch without the <motion> element fails.
    #[test]
    fn missing_section_fails() {
        let (prefix, _) = setup("cm-missing", &F12017, "<hardware_settings_config/>");
        let lookup = ports(20777);
        let loaded = ModulePorts::at_startup(&lookup);
        assert!(F12017.patch(&env(&prefix, &lookup, &loaded)).is_err());
        assert!(!F12017.patched(&env(&prefix, &lookup, &loaded)).unwrap());
    }

    /// DiRT Rally 2.0 (Steam): patched() compares with the port loaded at application start; a port changed on the
    /// Games page is used only once patch() reloads it.
    #[test]
    fn patched_uses_the_port_loaded_at_startup() {
        let (prefix, _) = setup(
            "cm-stale",
            &DIRT_RALLY2_STEAM,
            "<hardware_settings_config><motion_platform /></hardware_settings_config>",
        );
        let startup = ports(20777);
        let loaded = ModulePorts::at_startup(&startup);
        DIRT_RALLY2_STEAM.patch(&env(&prefix, &startup, &loaded)).unwrap();
        let changed = ports(20999);
        let after_change = env(&prefix, &changed, &loaded);
        assert!(DIRT_RALLY2_STEAM.patched(&after_change).unwrap(), "still compared with 20777");
        DIRT_RALLY2_STEAM.patch(&after_change).unwrap();
        assert!(DIRT_RALLY2_STEAM.patched(&after_change).unwrap());
        assert_eq!(after_change.loaded_port(DIRT_RALLY2_STEAM.module(), PortKey::Dirt).unwrap(), 20999);
        assert_eq!(
            after_change.loaded_port(DIRT4.module(), PortKey::Dirt).unwrap(),
            20777,
            "every module keeps its own loaded port"
        );
    }

    /// The F1 2015 patched check reloads the port before comparing; its patch writes the port the last
    /// patched() (or application start) loaded.
    #[test]
    fn f1_2015_reloads_in_patched() {
        let (prefix, path) = setup("cm-2015-stale", &F12015, F1_FILE);
        let startup = ports(5555);
        let loaded = ModulePorts::at_startup(&startup);
        let changed = ports(6666);
        let env = env(&prefix, &changed, &loaded);
        F12015.patch(&env).unwrap();
        assert!(fs::read_to_string(&path).unwrap().contains("port=\"5555\""));
        assert!(!F12015.patched(&env).unwrap());
        F12015.patch(&env).unwrap();
        assert!(fs::read_to_string(&path).unwrap().contains("port=\"6666\""));
        assert!(F12015.patched(&env).unwrap());
    }
}
