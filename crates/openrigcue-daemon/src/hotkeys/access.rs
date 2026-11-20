//! Which input devices are keyboards, for the keyboard reader.
//!
//! Devices are listed from `/dev/input/event*` and classified from sysfs (`/sys/class/input/<node>/device/name`,
//! `capabilities/key`, `capabilities/abs`).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// `KEY_ESC` … `KEY_MAX` of the keyboard part of the key space (below `BTN_MISC`, 0x100).
const KEYBOARD_KEYS: std::ops::Range<u16> = 1..0x100;
/// `KEY_Q`, `KEY_A`, `KEY_Z`: a device with letter keys is a keyboard.
const LETTER_KEYS: [u16; 3] = [16, 30, 44];
/// Codes that mark mice, touchpads and pens (`BTN_LEFT`, `BTN_TOOL_PEN`, `BTN_TOUCH`).
const POINTER_KEYS: [u16; 3] = [0x110, 0x140, 0x14a];

/// What a device is, for the hotkeys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    /// Letter keys: read for keyboard hotkeys.
    Keyboard,
    /// Other keys of the keyboard space (media keys, macro pads, power buttons): read for keyboard hotkeys.
    Keys,
    /// A joystick, gamepad, wheel or button box: its buttons are controller hotkeys.
    Controller,
    /// Anything else (mice, touchpads, switches): not used.
    Other,
}

impl DeviceKind {
    /// Whether the keyboard reader opens devices of this kind.
    pub fn has_keyboard_keys(self) -> bool {
        matches!(self, DeviceKind::Keyboard | DeviceKind::Keys)
    }
}

/// The joystick reader's controller test.
fn is_controller(keys: &BTreeSet<u16>, axes: &BTreeSet<u16>) -> bool {
    openrigcue_sources::api::joystick::evdev::is_controller(keys, axes)
}

/// Classifies a device by its key and absolute-axis codes.
pub fn classify(keys: &BTreeSet<u16>, axes: &BTreeSet<u16>) -> DeviceKind {
    if is_controller(keys, axes) {
        return DeviceKind::Controller;
    }
    if POINTER_KEYS.iter().any(|k| keys.contains(k)) {
        return DeviceKind::Other;
    }
    if LETTER_KEYS.iter().all(|k| keys.contains(k)) {
        DeviceKind::Keyboard
    } else if keys.range(KEYBOARD_KEYS).next().is_some() {
        DeviceKind::Keys
    } else {
        DeviceKind::Other
    }
}

/// A sysfs capability bitmap (`capabilities/key`): hexadecimal words of `BITS_PER_LONG` bits, most significant
/// word first.
pub fn parse_bitmap(text: &str) -> BTreeSet<u16> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let bits = usize::BITS as usize;
    let mut codes = BTreeSet::new();
    for (position, word) in words.iter().rev().enumerate() {
        let Ok(value) = u64::from_str_radix(word, 16) else { continue };
        for bit in 0..bits.min(64) {
            if value & (1 << bit) != 0 {
                if let Ok(code) = u16::try_from(position * bits + bit) {
                    codes.insert(code);
                }
            }
        }
    }
    codes
}

/// Where the devices and their sysfs descriptions are.
#[derive(Debug, Clone)]
pub struct InputRoots {
    pub dev: PathBuf,
    pub sys: PathBuf,
}

impl InputRoots {
    pub fn system() -> InputRoots {
        InputRoots { dev: PathBuf::from("/dev/input"), sys: PathBuf::from("/sys/class/input") }
    }
}

fn read_trimmed(path: &Path) -> String {
    std::fs::read_to_string(path).map(|t| t.trim().to_owned()).unwrap_or_default()
}

/// A device node's name and classification, from sysfs.
pub fn describe(roots: &InputRoots, node: &str) -> (String, DeviceKind) {
    let device = roots.sys.join(node).join("device");
    let keys = parse_bitmap(&read_trimmed(&device.join("capabilities/key")));
    let axes = parse_bitmap(&read_trimmed(&device.join("capabilities/abs")));
    (read_trimmed(&device.join("name")), classify(&keys, &axes))
}

/// The `event*` nodes, in number order.
pub fn event_nodes(roots: &InputRoots) -> Vec<String> {
    let mut nodes: Vec<(u32, String)> = std::fs::read_dir(&roots.dev)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let number = name.strip_prefix("event")?.parse().ok()?;
            Some((number, name))
        })
        .collect();
    nodes.sort();
    nodes.into_iter().map(|(_, name)| name).collect()
}

/// The nodes whose keys the keyboard reader reads, with their names.
pub fn keyboards(roots: &InputRoots) -> Vec<(PathBuf, String)> {
    event_nodes(roots)
        .into_iter()
        .filter_map(|node| {
            let (name, kind) = describe(roots, &node);
            kind.has_keyboard_keys().then(|| (roots.dev.join(&node), name))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(codes: &[u16]) -> BTreeSet<u16> {
        codes.iter().copied().collect()
    }

    /// A fake `/dev/input` and `/sys/class/input` in a temporary directory: nothing real is opened.
    fn fake_device(root: &Path, node: &str, name: &str, key: &str, abs: &str) {
        std::fs::create_dir_all(root.join("dev")).unwrap();
        std::fs::write(root.join("dev").join(node), "").unwrap();
        let caps = root.join("sys").join(node).join("device/capabilities");
        std::fs::create_dir_all(&caps).unwrap();
        std::fs::write(caps.join("key"), format!("{key}\n")).unwrap();
        std::fs::write(caps.join("abs"), format!("{abs}\n")).unwrap();
        std::fs::write(root.join("sys").join(node).join("device/name"), format!("{name}\n")).unwrap();
    }

    /// Bitmaps as sysfs prints them: the last word holds codes 0…63.
    #[test]
    fn bitmaps() {
        assert_eq!(parse_bitmap("3"), set(&[0, 1]));
        assert_eq!(parse_bitmap("1 0"), set(&[64]));
        assert_eq!(parse_bitmap("0"), set(&[]));
        // A gamepad: BTN_SOUTH (0x130) and BTN_EAST (0x131) in the fifth word (codes 256…319).
        assert_eq!(parse_bitmap("3000000000000 0 0 0 0"), set(&[0x130, 0x131]));
    }

    #[test]
    fn classification() {
        let letters = set(&[1, 16, 28, 30, 44, 59]);
        assert_eq!(classify(&letters, &set(&[])), DeviceKind::Keyboard);
        assert_eq!(classify(&set(&[116]), &set(&[])), DeviceKind::Keys, "power button");
        assert_eq!(classify(&set(&[0x120, 0x121]), &set(&[0, 1])), DeviceKind::Controller);
        assert_eq!(classify(&set(&[0x110, 0x111]), &set(&[])), DeviceKind::Other, "mouse");
        assert_eq!(classify(&set(&[]), &set(&[])), DeviceKind::Other);
    }

    /// The keyboard list holds the nodes with keyboard keys, in number order; controllers and mice are left out.
    #[test]
    fn keyboard_nodes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // Keyboard: KEY_ESC..KEY_Z bits set in the last word (codes 1..=44 and more).
        fake_device(root, "event10", "AT Keyboard", "fffffffffffffffe", "0");
        fake_device(root, "event2", "Wheel", "ffff00000000 0 0 0 0", "3");
        fake_device(root, "event3", "Mouse", "1f0000 0 0 0 0", "0");
        fake_device(root, "event4", "Media Keys", "100000 0", "0");
        std::fs::write(root.join("dev/mouse0"), "").unwrap();
        let roots = InputRoots { dev: root.join("dev"), sys: root.join("sys") };
        let names: Vec<_> = keyboards(&roots).into_iter().map(|(_, name)| name).collect();
        assert_eq!(names, ["Media Keys", "AT Keyboard"]);
    }
}
