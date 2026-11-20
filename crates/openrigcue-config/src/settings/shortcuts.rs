//! `[SHORTCUTS]`: keyboard and controller bindings of the hotkey actions.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::import::value::{str2bool, Dict, LegacyValue};

/// The bindings of one action: a hotkey string (`ctrl+shift+f1`) and a controller
/// button as `<joystick name>^<button>`. Empty means unbound.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Binding {
    pub keys: String,
    pub buttons: String,
}

/// All shortcut bindings, keyed by action name (the `config.ini` key without `_keys`/`_btns`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Shortcuts {
    pub bindings: BTreeMap<String, Binding>,
    /// Wind mode switch skips "always on" (tpl 0).
    pub skip_power_wind_always_on: bool,
    pub skip_power_wind_auto: bool,
    pub skip_power_wind_off: bool,
    /// Joysticks the input layer ignores.
    pub ignore_joysticks: String,
    /// Every other key of the section, kept as written, such as keys with a trailing
    /// space (`ejection_seat_btns `). Only keys ending exactly in `_keys` or
    /// `_btns` are bindings, so these never trigger anything.
    pub other_keys: BTreeMap<String, String>,
}

impl Default for Shortcuts {
    /// The bindings of a fresh installation: the template's, then every action the migration steps
    /// add (effect, smoothing, belt, rumble, haptic, compensation … actions).
    fn default() -> Self {
        static DEFAULT: OnceLock<Shortcuts> = OnceLock::new();
        DEFAULT.get_or_init(|| crate::import::first_run().settings.shortcuts.clone()).clone()
    }
}

/// What `from_legacy` could not map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnmappedShortcut {
    pub key: String,
    pub reason: String,
}

impl Shortcuts {
    pub fn binding(&self, action: &str) -> Option<&Binding> {
        self.bindings.get(action)
    }

    /// Converts the `config.ini` section. `<action>_keys` and `<action>_btns` pairs become bindings; any other
    /// key (such as one with a trailing space, `ejection_seat_btns `) is kept in
    /// [`Shortcuts::other_keys`] and binds nothing.
    pub fn from_legacy(section: &Dict) -> (Shortcuts, Vec<UnmappedShortcut>) {
        let mut shortcuts = Shortcuts {
            bindings: BTreeMap::new(),
            skip_power_wind_always_on: false,
            skip_power_wind_auto: false,
            skip_power_wind_off: false,
            ignore_joysticks: String::new(),
            other_keys: BTreeMap::new(),
        };
        let mut unmapped = Vec::new();
        for (key, value) in section.iter() {
            let key = key.to_text();
            let Some(text) = value.py_str() else {
                unmapped.push(UnmappedShortcut { key, reason: format!("{} is not a scalar", value.type_name()) });
                continue;
            };
            match key.as_str() {
                "skip_power_wind_always_on" | "skip_power_wind_auto" | "skip_power_wind_off" => {
                    let Some(flag) = legacy_flag(value) else {
                        unmapped.push(UnmappedShortcut { key, reason: format!("{text:?} is not a boolean") });
                        continue;
                    };
                    match key.as_str() {
                        "skip_power_wind_always_on" => shortcuts.skip_power_wind_always_on = flag,
                        "skip_power_wind_auto" => shortcuts.skip_power_wind_auto = flag,
                        _ => shortcuts.skip_power_wind_off = flag,
                    }
                }
                "ignore_joysticks" => shortcuts.ignore_joysticks = text,
                _ => {
                    if let Some(action) = key.strip_suffix("_keys") {
                        shortcuts.bindings.entry(action.to_owned()).or_default().keys = text;
                    } else if let Some(action) = key.strip_suffix("_btns") {
                        shortcuts.bindings.entry(action.to_owned()).or_default().buttons = text;
                    } else {
                        shortcuts.other_keys.insert(key, text);
                    }
                }
            }
        }
        (shortcuts, unmapped)
    }
}

/// `skip_power_wind_*` hold `'0'`/`'1'` (validation type `bool`).
fn legacy_flag(value: &LegacyValue) -> Option<bool> {
    match value {
        LegacyValue::Bool(b) => Some(*b),
        LegacyValue::Int(i) => Some(*i != 0),
        LegacyValue::Str(s) => str2bool(s),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::value::Key;

    fn section(entries: &[(&str, &str)]) -> Dict {
        entries.iter().map(|(k, v)| (Key::from(*k), LegacyValue::Str((*v).into()))).collect()
    }

    /// A typo key (`ejection_seat_btns `) binds nothing and is kept as written.
    #[test]
    fn pairs_become_bindings_and_typo_keys_are_kept() {
        let (s, unmapped) = Shortcuts::from_legacy(&section(&[
            ("power_wind_switch_keys", "ctrl+shift+f1"),
            ("power_wind_switch_btns", "Xbox One Elite Controller"),
            ("ejection_seat_btns ", ""),
            ("ejection_seat_keys", "ctrl+e"),
            ("cannon_firing_btns ", "stale"),
            ("cannon_firing_btns", "Pad^3"),
            ("skip_power_wind_auto", "1"),
            ("ignore_joysticks", "vJoy"),
            ("stray", "x"),
        ]));
        assert_eq!(s.binding("power_wind_switch").unwrap().keys, "ctrl+shift+f1");
        assert_eq!(s.binding("power_wind_switch").unwrap().buttons, "Xbox One Elite Controller");
        assert_eq!(s.binding("ejection_seat").unwrap(), &Binding { keys: "ctrl+e".into(), buttons: String::new() });
        assert_eq!(s.binding("cannon_firing").unwrap().buttons, "Pad^3");
        assert!(s.skip_power_wind_auto);
        assert!(!s.skip_power_wind_off);
        assert_eq!(s.ignore_joysticks, "vJoy");
        assert!(unmapped.is_empty());
        let other: Vec<(&str, &str)> = s.other_keys.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(other, [("cannon_firing_btns ", "stale"), ("ejection_seat_btns ", ""), ("stray", "x")]);

        let (s, _) = Shortcuts::from_legacy(&section(&[("ejection_seat_btns ", "Pad^7")]));
        assert!(s.binding("ejection_seat").is_none(), "the typo key never fires");
        assert_eq!(s.other_keys.get("ejection_seat_btns ").map(String::as_str), Some("Pad^7"));
    }

    /// Fresh-install bindings: template hotkeys plus the keys set by v97, v116, v133, v163, v237, v239.
    #[test]
    fn default_bindings_match_fresh_install() {
        let s = Shortcuts::default();
        let keys = |a: &str| s.binding(a).unwrap().keys.clone();
        assert_eq!(keys("power_wind_switch"), "ctrl+shift+f1");
        assert_eq!(keys("power_wind_up"), "ctrl+shift+up");
        assert_eq!(keys("power_wind_down"), "ctrl+shift+down");
        assert_eq!(keys("shake_box_switch"), "ctrl+shift+f5");
        assert_eq!(keys("motion_box_switch"), "ctrl+shift+f3");
        assert_eq!(keys("led_switch"), "ctrl+shift+f4");
        assert_eq!(keys("motion_box_homing"), "ctrl+shift+f6");
        assert_eq!(keys("telemetry_capture"), "ctrl+shift+f7");
        assert_eq!(keys("telemetry_reset"), "ctrl+shift+f8");
        assert_eq!(keys("telemetry_snapshot"), "ctrl+shift+f9");
        assert_eq!(keys("telemetry_export"), "ctrl+shift+f10");
        assert_eq!(keys("belt_switch"), "ctrl+shift+f11");
        assert_eq!(keys("belt_homing"), "ctrl+shift+f12");
        assert_eq!(s.binding("power_wind_switch").unwrap().buttons, "", "template.ini leaves it empty");
        assert!(s.binding("export_csv").is_none(), "renamed at v163");
        assert!(s.binding("joystick_switch").is_none(), "only a validation rule, never created");
        for action in
            ["motion_box_rumble_strips_up", "rumble_seat_engine_down", "haptic_wheel_gear_up", "ejection_seat"]
        {
            assert!(s.binding(action).is_some(), "{action}");
        }
        assert_eq!(s.bindings.len(), 202);
    }
}
