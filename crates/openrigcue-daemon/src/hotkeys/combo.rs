//! Keyboard hotkeys in the text syntax the settings store (`ctrl+shift+f1`), resolved to
//! Linux evdev key codes.
//!
//! Covers parsing a hotkey, testing whether it is held, and naming a captured hotkey for the shortcut
//! editor. A key name stands for the key at that position of a US layout,
//! which is what evdev codes are.

use std::collections::BTreeSet;

/// One key code with its name; the generic modifier names (`ctrl`, `shift`, `alt`, `windows`) cover
/// both sides, `left …`/`right …` one side.
const KEYS: &[(&str, &[u16])] = &[
    ("esc", &[1]),
    ("1", &[2, 79]),
    ("2", &[3, 80]),
    ("3", &[4, 81]),
    ("4", &[5, 75]),
    ("5", &[6, 76]),
    ("6", &[7, 77]),
    ("7", &[8, 71]),
    ("8", &[9, 72]),
    ("9", &[10, 73]),
    ("0", &[11, 82]),
    ("-", &[12, 74]),
    ("=", &[13, 117]),
    ("backspace", &[14]),
    ("tab", &[15]),
    ("q", &[16]),
    ("w", &[17]),
    ("e", &[18]),
    ("r", &[19]),
    ("t", &[20]),
    ("y", &[21]),
    ("u", &[22]),
    ("i", &[23]),
    ("o", &[24]),
    ("p", &[25]),
    ("[", &[26]),
    ("]", &[27]),
    ("enter", &[28, 96]),
    ("ctrl", &[29, 97]),
    ("left ctrl", &[29]),
    ("right ctrl", &[97]),
    ("a", &[30]),
    ("s", &[31]),
    ("d", &[32]),
    ("f", &[33]),
    ("g", &[34]),
    ("h", &[35]),
    ("j", &[36]),
    ("k", &[37]),
    ("l", &[38]),
    (";", &[39]),
    ("'", &[40]),
    ("`", &[41]),
    ("shift", &[42, 54]),
    ("left shift", &[42]),
    ("right shift", &[54]),
    ("\\", &[43, 86]),
    ("z", &[44]),
    ("x", &[45]),
    ("c", &[46]),
    ("v", &[47]),
    ("b", &[48]),
    ("n", &[49]),
    ("m", &[50]),
    (",", &[51, 121]),
    (".", &[52, 83]),
    ("/", &[53, 98]),
    ("*", &[55, 9]),
    ("alt", &[56, 100]),
    ("left alt", &[56]),
    ("right alt", &[100]),
    ("alt gr", &[100]),
    ("space", &[57]),
    ("caps lock", &[58]),
    ("f1", &[59]),
    ("f2", &[60]),
    ("f3", &[61]),
    ("f4", &[62]),
    ("f5", &[63]),
    ("f6", &[64]),
    ("f7", &[65]),
    ("f8", &[66]),
    ("f9", &[67]),
    ("f10", &[68]),
    ("num lock", &[69]),
    ("scroll lock", &[70]),
    ("plus", &[78, 13]),
    ("f11", &[87]),
    ("f12", &[88]),
    ("print screen", &[99]),
    ("home", &[102]),
    ("up", &[103]),
    ("page up", &[104]),
    ("left", &[105]),
    ("right", &[106]),
    ("end", &[107]),
    ("down", &[108]),
    ("page down", &[109]),
    ("insert", &[110]),
    ("delete", &[111]),
    ("volume mute", &[113]),
    ("volume down", &[114]),
    ("volume up", &[115]),
    ("pause", &[119]),
    ("windows", &[125, 126]),
    ("left windows", &[125]),
    ("right windows", &[126]),
    ("menu", &[127]),
    ("next track", &[163]),
    ("play/pause media", &[164]),
    ("previous track", &[165]),
    ("stop media", &[166]),
    ("f13", &[183]),
    ("f14", &[184]),
    ("f15", &[185]),
    ("f16", &[186]),
    ("f17", &[187]),
    ("f18", &[188]),
    ("f19", &[189]),
    ("f20", &[190]),
    ("f21", &[191]),
    ("f22", &[192]),
    ("f23", &[193]),
    ("f24", &[194]),
];

/// Alias names: other words for a key above.
const ALIASES: &[(&str, &str)] = &[
    ("control", "ctrl"),
    ("left control", "left ctrl"),
    ("right control", "right ctrl"),
    ("altgr", "alt gr"),
    ("option", "alt"),
    ("win", "windows"),
    ("left win", "left windows"),
    ("right win", "right windows"),
    ("command", "windows"),
    ("cmd", "windows"),
    ("super", "windows"),
    ("escape", "esc"),
    ("return", "enter"),
    ("del", "delete"),
    ("ins", "insert"),
    ("pgup", "page up"),
    ("pgdn", "page down"),
    ("pgdown", "page down"),
    ("prior", "page up"),
    ("next", "page down"),
    ("prtsc", "print screen"),
    ("prtscn", "print screen"),
    ("print", "print screen"),
    ("snapshot", "print screen"),
    ("break", "pause"),
    ("apps", "menu"),
    ("application", "menu"),
    ("spacebar", "space"),
    ("capslock", "caps lock"),
    ("numlock", "num lock"),
    ("scrolllock", "scroll lock"),
    ("left arrow", "left"),
    ("right arrow", "right"),
    ("up arrow", "up"),
    ("down arrow", "down"),
    ("back", "backspace"),
    ("minus", "-"),
    ("asterisk", "*"),
];

/// Shifted symbols of a US layout: such a name stands for the key that types it.
const SHIFTED: &[(char, &str)] = &[
    ('!', "1"),
    ('@', "2"),
    ('#', "3"),
    ('$', "4"),
    ('%', "5"),
    ('^', "6"),
    ('&', "7"),
    ('(', "9"),
    (')', "0"),
    ('_', "-"),
    ('{', "["),
    ('}', "]"),
    ('|', "\\"),
    (':', ";"),
    ('"', "'"),
    ('~', "`"),
    ('<', ","),
    ('>', "."),
    ('?', "/"),
    ('+', "plus"),
];

/// Why a binding string is not a hotkey that can be tested.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ComboError {
    /// The hotkey has more than one step; only one-step hotkeys can be tested as held.
    #[error("multi-step hotkey {0:?}")]
    MultiStep(String),
    /// A key name that names no known key.
    #[error("key {0:?} is not mapped to any known key")]
    UnknownKey(String),
}

/// Normalises a key name: lower case for names longer than one character, `_` as a space,
/// then the canonical name.
fn normalize(name: &str) -> String {
    let mut name = if name.chars().count() > 1 { name.to_lowercase() } else { name.to_owned() };
    if name != "_" && name.contains('_') {
        name = name.replace('_', " ");
    }
    match ALIASES.iter().find(|(alias, _)| *alias == name) {
        Some((_, canonical)) => (*canonical).to_owned(),
        None => name,
    }
}

/// The key codes a key name stands for.
fn codes_of(name: &str) -> Option<Vec<u16>> {
    let normalized = normalize(name);
    let mut chars = normalized.chars();
    let lookup = match (chars.next(), chars.next()) {
        // A single letter names its key whatever the case ('A' is the same key as 'a').
        (Some(c), None) if c.is_ascii_alphabetic() => c.to_ascii_lowercase().to_string(),
        (Some(c), None) => SHIFTED.iter().find(|(s, _)| *s == c).map_or(normalized.clone(), |(_, k)| (*k).to_owned()),
        _ => normalized,
    };
    KEYS.iter().find(|(key, _)| *key == lookup).map(|(_, codes)| codes.to_vec())
}

/// A parsed one-step hotkey: every group must have one of its codes held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyCombo {
    groups: Vec<Vec<u16>>,
}

impl KeyCombo {
    /// Parses a hotkey text: a one-character hotkey is that key (so `,` and
    /// `+` work alone); otherwise steps are split on `,` (optionally followed by a space), keys on `+` with an
    /// optional space on either side. A comma inside a longer hotkey (`ctrl+,`) therefore makes two steps.
    pub fn parse(hotkey: &str) -> Result<KeyCombo, ComboError> {
        if hotkey.chars().count() == 1 {
            let codes = codes_of(hotkey).ok_or_else(|| ComboError::UnknownKey(hotkey.to_owned()))?;
            return Ok(KeyCombo { groups: vec![codes] });
        }
        let steps = split_steps(hotkey);
        if steps.len() > 1 {
            return Err(ComboError::MultiStep(hotkey.to_owned()));
        }
        let groups = split_keys(steps[0])
            .into_iter()
            .map(|name| codes_of(name).ok_or_else(|| ComboError::UnknownKey(name.to_owned())))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(KeyCombo { groups })
    }

    /// Whether the hotkey is held: every key of the step is held (other keys may be held too).
    pub fn is_pressed(&self, held: &BTreeSet<u16>) -> bool {
        self.groups.iter().all(|codes| codes.iter().any(|code| held.contains(code)))
    }
}

/// Splits a hotkey into steps at each `,` and one optional whitespace character after it.
fn split_steps(hotkey: &str) -> Vec<&str> {
    let mut steps = Vec::new();
    let mut rest = hotkey;
    while let Some(at) = rest.find(',') {
        steps.push(&rest[..at]);
        rest = &rest[at + 1..];
        if let Some(stripped) = rest.strip_prefix(char::is_whitespace) {
            rest = stripped;
        }
    }
    steps.push(rest);
    steps
}

/// Splits a step into keys at each `+` with one optional whitespace character on either side.
fn split_keys(step: &str) -> Vec<&str> {
    let mut keys = Vec::new();
    let mut rest = step;
    while let Some(at) = rest.find('+') {
        let key = &rest[..at];
        keys.push(key.strip_suffix(char::is_whitespace).unwrap_or(key));
        rest = &rest[at + 1..];
        if let Some(stripped) = rest.strip_prefix(char::is_whitespace) {
            rest = stripped;
        }
    }
    keys.push(rest);
    keys
}

/// The name of the key with this code, without its `left `/`right ` side.
/// Key pad keys are named like the keys they type.
pub fn key_name(code: u16) -> Option<&'static str> {
    KEYS.iter()
        .filter(|(name, _)| !name.starts_with("left ") && !name.starts_with("right ") && *name != "alt gr")
        .find(|(_, codes)| codes.contains(&code))
        .map(|(name, _)| *name)
}

/// The hotkey text for a set of key names: the names without sides, `+` as `plus`, deduplicated, modifiers first in
/// the order ctrl, alt, shift, windows, the rest alphabetically, joined by `+`.
pub fn hotkey_name<'a>(names: impl IntoIterator<Item = &'a str>) -> String {
    const MODIFIERS: [&str; 4] = ["ctrl", "alt", "shift", "windows"];
    let clean: BTreeSet<String> =
        names.into_iter().map(|n| n.replace("left ", "").replace("right ", "").replace('+', "plus")).collect();
    let mut sorted: Vec<String> = clean.into_iter().collect();
    sorted.sort_by_key(|name| (MODIFIERS.iter().position(|m| m == name).unwrap_or(5), name.clone()));
    sorted.join("+")
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEFT_CTRL: u16 = 29;
    const RIGHT_CTRL: u16 = 97;
    const LEFT_SHIFT: u16 = 42;
    const F1: u16 = 59;

    fn held(codes: &[u16]) -> BTreeSet<u16> {
        codes.iter().copied().collect()
    }

    /// The template's default bindings parse, and the held test accepts either side of a generic modifier and
    /// extra held keys.
    #[test]
    fn default_combo_is_pressed() {
        let combo = KeyCombo::parse("ctrl+shift+f1").unwrap();
        assert!(combo.is_pressed(&held(&[LEFT_CTRL, LEFT_SHIFT, F1])));
        assert!(combo.is_pressed(&held(&[RIGHT_CTRL, 54, F1, 30])), "right side, extra key");
        assert!(!combo.is_pressed(&held(&[LEFT_CTRL, F1])));
        let left = KeyCombo::parse("left ctrl+f1").unwrap();
        assert!(!left.is_pressed(&held(&[RIGHT_CTRL, F1])));
        for default in
            ["ctrl+shift+up", "ctrl+shift+down", "ctrl+shift+f5", "ctrl+shift+f10", "ctrl+shift+f11", "ctrl+shift+f12"]
        {
            KeyCombo::parse(default).unwrap();
        }
    }

    /// Parsing details: spaces around `+`, upper case, `_` for spaces, canonical aliases, single
    /// upper-case letters, shifted symbols; multi-step and unknown keys are errors.
    #[test]
    fn parse_like_the_keyboard_package() {
        assert_eq!(KeyCombo::parse("Ctrl + Page_Up").unwrap(), KeyCombo::parse("ctrl+page up").unwrap());
        assert_eq!(KeyCombo::parse("control+escape").unwrap(), KeyCombo::parse("ctrl+esc").unwrap());
        assert_eq!(KeyCombo::parse("shift+A").unwrap(), KeyCombo::parse("shift+a").unwrap());
        assert_eq!(KeyCombo::parse("shift+!").unwrap(), KeyCombo::parse("shift+1").unwrap());
        assert_eq!(KeyCombo::parse("ctrl+plus").unwrap().groups, vec![vec![29, 97], vec![78, 13]]);
        assert_eq!(KeyCombo::parse("ctrl+a, b"), Err(ComboError::MultiStep("ctrl+a, b".into())));
        assert_eq!(KeyCombo::parse("ctrl+hyper"), Err(ComboError::UnknownKey("hyper".into())));
        assert_eq!(KeyCombo::parse("ctrl+"), Err(ComboError::UnknownKey(String::new())));
        assert_eq!(KeyCombo::parse(",").unwrap().groups, vec![vec![51, 121]], "one character: the key itself");
        assert_eq!(KeyCombo::parse("+").unwrap().groups, vec![vec![78, 13]]);
        assert_eq!(KeyCombo::parse("ctrl+,"), Err(ComboError::MultiStep("ctrl+,".into())));
    }

    /// Hotkey naming: sides removed, modifiers in ctrl/alt/shift/windows order, then the other names.
    #[test]
    fn hotkey_names() {
        assert_eq!(hotkey_name(["f1", "right shift", "left ctrl"]), "ctrl+shift+f1");
        assert_eq!(hotkey_name(["windows", "b", "alt", "a", "shift"]), "alt+shift+windows+a+b");
        assert_eq!(hotkey_name(["ctrl", "+", "ctrl"]), "ctrl+plus");
        assert_eq!(key_name(RIGHT_CTRL), Some("ctrl"));
        assert_eq!(key_name(79), Some("1"), "key pad 1 types 1");
        assert_eq!(key_name(100), Some("alt"));
        assert_eq!(key_name(0x120), None);
    }

    /// A captured single key can be bound again: its name parses back to a combo it satisfies.
    #[test]
    fn captured_names_round_trip() {
        for code in 1..=255u16 {
            if let Some(name) = key_name(code) {
                let combo = KeyCombo::parse(&hotkey_name([name])).unwrap();
                assert!(combo.is_pressed(&held(&[code])), "{name}");
            }
        }
    }
}
