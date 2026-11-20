//! The logic of the Joystick page that does not depend on where the
//! profiles are stored: the rows and their texts, the live bars, axis capture, profile names and the Magic Wheel
//! battery.

use std::collections::BTreeMap;

use super::ControllerView;
use crate::udp_flight::python::map_range;

/// The assignment rows: profile key → row label, in page order.
pub const ASSIGNMENTS: [(&str, &str); 7] = [
    ("left", "Left"),
    ("right", "Right"),
    ("front", "Front"),
    ("back", "Back"),
    ("rotate_left", "Rotate Left"),
    ("rotate_right", "Rotate Right"),
    ("speed", "Speed RPM"),
];

/// The two entries of the mode dropdown.
pub const SINGLE_AXIS: &str = "Single Axis";
pub const DUAL_AXIS: &str = "Dual Axis";
/// The example text shown for each mode.
pub const SINGLE_AXIS_EXAMPLE: &str = "Example: Pedal, Gamepad Trigger, etc";
pub const DUAL_AXIS_EXAMPLE: &str = "Example: Wheel, Gamepad Thumbstick, Hotas, etc";
/// Shown once a joystick is selected.
pub const CAPTURE_INSTRUCTION: &str =
    "MOVE the axis to its MAX position, continue to HOLD it in the MAX position and then click CONFIRM at the SAME time";
/// Shown when the assign button of a configured row is clicked.
pub const CURRENT_SETTINGS: &str = "Current Settings";
/// The joystick list's first entry, and the only one without controllers.
pub const NO_JOYSTICK_SELECTED: &str = "None";
pub const NO_JOYSTICK_FOUND: &str = "No joystick found";
/// The text of a row without assignment.
pub const NOT_CONFIGURED: &str = "Not configured";
/// Alert shown when adding or duplicating a profile is refused.
pub const NAME_EXISTS: &str = "A profile with this name exists already; choose another name.";
/// Alert shown when removing a profile is refused.
pub const REMOVE_QUESTION: &str = "Please confirm that this profile should be deleted: ";
/// The profile that cannot be removed and is selected after a removal.
pub const DEFAULT_PROFILE: &str = "Default";
/// Longest profile name the name input accepts.
pub const NAME_MAX_LENGTH: usize = 30;

/// Rows whose new assignment starts in single-axis mode (`front`, `back`, `speed`).
pub fn default_mode(key: &str) -> &'static str {
    if matches!(key, "front" | "back" | "speed") {
        SINGLE_AXIS
    } else {
        DUAL_AXIS
    }
}

/// For a mode: the example text and whether the invert switch is visible.
pub fn mode_example(mode: &str) -> (&'static str, bool) {
    if mode == DUAL_AXIS {
        (DUAL_AXIS_EXAMPLE, false)
    } else {
        (SINGLE_AXIS_EXAMPLE, true)
    }
}

/// One stored assignment as the page reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry<'a> {
    pub joystick: &'a str,
    pub kind: &'a str,
    pub trigger: &'a str,
    pub mode: &'a str,
    pub invert_axis: bool,
}

impl Entry<'_> {
    /// The row's text.
    pub fn status(&self) -> String {
        let mut status = self.joystick.to_owned();
        if self.kind == "button" {
            status.push_str(" - button ");
            status.push_str(self.trigger);
        } else if self.mode == SINGLE_AXIS {
            status.push_str(" - single axis ");
            status.push_str(self.trigger);
        } else {
            status.push_str(" - dual ");
            let kind =
                self.kind.replace('>', &format!("{} >", self.trigger)).replace('<', &format!("{} <", self.trigger));
            status.push_str(&kind);
        }
        status
    }

    /// The settings button's tooltip (`&#010;` is a line break in the title attribute).
    pub fn tooltip(&self) -> String {
        format!("Joystick: {}\nType: {}\nAxis: {}\nMode: {}", self.joystick, self.kind, self.trigger, self.mode)
    }

    /// One assignment's pass over the controller states: the bar value (0..100) the
    /// last matching input sets, `None` when no input matches. A button event comes before the axes of the same
    /// update, so an axis with the same index decides.
    ///
    /// `Err` when the stored trigger is not an integer.
    pub fn progress(
        &self,
        controllers: &BTreeMap<String, ControllerView>,
    ) -> Result<Option<i64>, std::num::ParseIntError> {
        let trigger: i64 = self.trigger.trim().parse()?;
        let Some(state) = controllers.get(self.joystick) else {
            return Ok(None);
        };
        let Ok(index) = u32::try_from(trigger) else {
            return Ok(None);
        };
        let mut progress = None;
        if let Some(&pressed) = state.buttons.get(&index) {
            if self.kind == "button" {
                progress = Some(if pressed { 100 } else { 0 });
            }
        }
        if let Some(&raw) = state.axes.get(&index) {
            let mut value = f64::from(raw);
            if self.mode == SINGLE_AXIS {
                if self.invert_axis {
                    value *= -1.0;
                }
                progress = Some(map_range(value, -32768.0, 32767.0, 0.0, 100.0) as i64);
            } else if self.mode == DUAL_AXIS {
                if self.kind == "axis >" && value > 0.0 {
                    progress = Some(map_range(value, 0.0, 32767.0, 0.0, 100.0) as i64);
                } else if self.kind == "axis >" && value < 0.0 {
                    progress = Some(0);
                } else if self.kind == "axis <" && value < 0.0 {
                    progress = Some(100 - map_range(value, -32768.0, 0.0, 0.0, 100.0) as i64);
                } else if self.kind == "axis <" && value > 0.0 {
                    progress = Some(0);
                }
            }
        }
        Ok(progress)
    }
}

/// An axis held past 60 % of its travel on the selected joystick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub device: String,
    pub trigger: u32,
    pub value: i32,
    /// `'axis >'`, `'axis <'` (or `'axis'` for 0, which the 60 % test excludes).
    pub kind: String,
}

/// Lower bound of a captured axis: 60 % of 32768.
const CAPTURE_THRESHOLD: f64 = 19660.8;
/// Full scale: an axis at its very end (magnitude 32767 or 32768) is not captured.
const CAPTURE_LIMIT: i32 = 32767;

/// The axes of `device` between 60 % and full travel, in index order.
pub fn candidates(device: &str, controllers: &BTreeMap<String, ControllerView>) -> Vec<Candidate> {
    let Some(state) = controllers.get(device) else {
        return Vec::new();
    };
    state
        .axes
        .iter()
        .filter(|(_, value)| {
            value.unsigned_abs() < CAPTURE_LIMIT.unsigned_abs() && f64::from(value.unsigned_abs()) > CAPTURE_THRESHOLD
        })
        .map(|(&trigger, &value)| Candidate {
            device: device.to_owned(),
            trigger,
            value,
            kind: match value.signum() {
                1 => "axis >".to_owned(),
                0 => "axis".to_owned(),
                _ => "axis <".to_owned(),
            },
        })
        .collect()
}

/// The axis capture loop: each pass (every 0.1 s) the strongest candidate that was not
/// already a candidate in the previous pass ("this axis is stuck") is proposed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Capture {
    previous: Option<Vec<Candidate>>,
}

impl Capture {
    /// One pass: the `(type, axis)` to show, if a candidate qualifies.
    pub fn step(&mut self, device: &str, controllers: &BTreeMap<String, ControllerView>) -> Option<(String, String)> {
        let current = candidates(device, controllers);
        let mut best: Option<(u32, usize)> = None;
        for (index, candidate) in current.iter().enumerate() {
            let stuck = self.previous.as_ref().is_some_and(|previous| {
                previous
                    .iter()
                    .any(|p| p.device == candidate.device && p.trigger == candidate.trigger && p.kind == candidate.kind)
            });
            let strength = candidate.value.unsigned_abs();
            if !stuck && best.is_none_or(|(value, _)| strength > value) {
                best = Some((strength, index));
            }
        }
        let proposal = best.map(|(_, index)| (current[index].kind.clone(), current[index].trigger.to_string()));
        self.previous = Some(current);
        proposal
    }
}

/// Removes every `(...)` and `[...]` group, nested ones included, from `text`.
pub fn remove_text_inside_brackets(text: &str) -> String {
    const BRACKETS: [char; 4] = ['(', ')', '[', ']'];
    let mut count = [0i32; 2];
    let mut saved = String::new();
    for character in text.chars() {
        match BRACKETS.iter().position(|&b| b == character) {
            Some(i) => {
                let (kind, is_close) = (i / 2, i % 2);
                count[kind] += if is_close == 1 { -1 } else { 1 };
                if count[kind] < 0 {
                    count[kind] = 0;
                    // A closing bracket ends the scan only when the count stays
                    // non-negative; a stray closing bracket falls through to the character test.
                    if !count.iter().any(|&c| c != 0) {
                        saved.push(character);
                    }
                }
            }
            None => {
                if !count.iter().any(|&c| c != 0) {
                    saved.push(character);
                }
            }
        }
    }
    saved
}

/// A free profile name: `<name without bracketed text, right-stripped> (n)` for the first free `n` from 1.
pub fn propose_name(original: &str, exists: impl Fn(&str) -> bool) -> String {
    let base = remove_text_inside_brackets(original);
    let base = base.trim_end();
    (1..).map(|n| format!("{base} ({n})")).find(|name| !exists(name)).expect("an unused name exists")
}

/// [`propose_name`] with a lookup that can fail: the first failed lookup ends the search with its error. The
/// name is right-stripped of whitespace and of the separator characters U+001C…U+001F as well.
pub fn try_propose_name<E>(original: &str, mut exists: impl FnMut(&str) -> Result<bool, E>) -> Result<String, E> {
    let base = remove_text_inside_brackets(original);
    let base = base.trim_end_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c));
    for n in 1.. {
        let name = format!("{base} ({n})");
        if !exists(&name)? {
            return Ok(name);
        }
    }
    unreachable!("an unused name exists")
}

/// The Magic Wheel's battery (non-Windows only): axis 1 of a controller named `MAGIC WHEEL …`.
pub const MAGIC_WHEEL_PREFIX: &str = "MAGIC WHEEL ";

/// Maps the raw battery level from 2117..2455 to 0..100.
pub fn battery_percent(level: i32) -> f64 {
    map_range(f64::from(level), 2117.0, 2455.0, 0.0, 100.0)
}

/// The battery icon name for a percentage.
pub fn battery_icon(percent: f64) -> &'static str {
    if percent < 5.0 {
        "battery-0"
    } else if percent < 26.0 {
        "battery-25"
    } else if percent < 51.0 {
        "battery-50"
    } else if percent < 76.0 {
        "battery-75"
    } else {
        "battery-100"
    }
}

/// The first Magic Wheel's battery level (its axis 1; 0 when it has no axis 1).
pub fn magic_wheel_level(controllers: &BTreeMap<String, ControllerView>) -> Option<i32> {
    controllers
        .iter()
        .find(|(name, _)| name.starts_with(MAGIC_WHEEL_PREFIX))
        .map(|(_, state)| state.axes.get(&1).copied().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(axes: &[(u32, i32)], buttons: &[(u32, bool)]) -> BTreeMap<String, ControllerView> {
        BTreeMap::from([(
            "Pad".to_owned(),
            ControllerView { axes: axes.iter().copied().collect(), buttons: buttons.iter().copied().collect() },
        )])
    }

    fn entry<'a>(kind: &'a str, trigger: &'a str, mode: &'a str, invert_axis: bool) -> Entry<'a> {
        Entry { joystick: "Pad", kind, trigger, mode, invert_axis }
    }

    /// Row texts: `' - button '`, `' - single axis '`, `' - dual '` with the trigger put before `>`/`<`.
    #[test]
    fn load_events_texts() {
        assert_eq!(entry("button", "3", DUAL_AXIS, false).status(), "Pad - button 3");
        assert_eq!(entry("axis >", "2", SINGLE_AXIS, false).status(), "Pad - single axis 2");
        assert_eq!(entry("axis <", "0", DUAL_AXIS, false).status(), "Pad - dual axis 0 <");
        assert_eq!(
            entry("axis >", "1", DUAL_AXIS, false).tooltip(),
            "Joystick: Pad\nType: axis >\nAxis: 1\nMode: Dual Axis"
        );
    }

    /// Bar values: single axis over the full range (inverted when asked), dual halves, buttons; the value is
    /// truncated to an integer.
    #[test]
    fn update_sliders_bars() {
        let pad = view(&[(0, 0), (1, -32768), (2, 16384)], &[(3, true)]);
        assert_eq!(entry("axis >", "0", SINGLE_AXIS, false).progress(&pad).unwrap(), Some(50), "map(0) = 50.0007…");
        assert_eq!(entry("axis >", "1", SINGLE_AXIS, true).progress(&pad).unwrap(), Some(100));
        assert_eq!(entry("axis >", "2", DUAL_AXIS, false).progress(&pad).unwrap(), Some(50));
        assert_eq!(entry("axis >", "1", DUAL_AXIS, false).progress(&pad).unwrap(), Some(0));
        assert_eq!(entry("axis <", "1", DUAL_AXIS, false).progress(&pad).unwrap(), Some(100));
        assert_eq!(entry("axis <", "2", DUAL_AXIS, false).progress(&pad).unwrap(), Some(0));
        assert_eq!(entry("axis <", "0", DUAL_AXIS, false).progress(&pad).unwrap(), None, "a centred axis sets nothing");
        assert_eq!(entry("button", "3", DUAL_AXIS, false).progress(&pad).unwrap(), Some(100));
        assert_eq!(
            entry("axis >", "3", DUAL_AXIS, false).progress(&pad).unwrap(),
            None,
            "button state needs type button"
        );
        assert_eq!(entry("axis >", "9", SINGLE_AXIS, false).progress(&pad).unwrap(), None);
        assert!(entry("axis >", "x", SINGLE_AXIS, false).progress(&pad).is_err(), "int(config['trigger']) raises");
        let other = Entry { joystick: "Other", ..entry("axis >", "0", SINGLE_AXIS, false) };
        assert_eq!(other.progress(&pad).unwrap(), None);
    }

    /// Axis capture: only axes with 19660.8 < |value| < 32767; the strongest wins; a candidate already
    /// present in the previous pass is stuck.
    #[test]
    fn hook_joystick_config_capture() {
        let mut capture = Capture::default();
        assert_eq!(capture.step("Pad", &view(&[(0, 32767), (1, -32768), (2, 19660)], &[])), None);
        assert_eq!(capture.step("Pad", &view(&[(0, 20000), (1, -25000)], &[])), Some(("axis <".into(), "1".into())));
        assert_eq!(capture.step("Pad", &view(&[(0, 20000), (1, -25000)], &[])), None, "both stuck now");
        assert_eq!(
            capture.step("Pad", &view(&[(0, 20000), (1, 30000)], &[])),
            Some(("axis >".into(), "1".into())),
            "new direction"
        );
        assert_eq!(capture.step("Other", &view(&[(0, 30000)], &[])), None, "only the selected joystick");
        let mut tie = Capture::default();
        assert_eq!(
            tie.step("Pad", &view(&[(4, 21000), (5, -21000)], &[])),
            Some(("axis >".into(), "4".into())),
            "first wins a tie"
        );
    }

    /// Proposed names and bracket removal.
    #[test]
    fn propose_name_numbers() {
        let existing = ["Default", "Default (1)", "Wheel"];
        let exists = |name: &str| existing.contains(&name);
        assert_eq!(propose_name("Default", exists), "Default (2)");
        assert_eq!(propose_name("Default (1)", exists), "Default (2)");
        assert_eq!(propose_name("Wheel [old] ", exists), "Wheel (1)");
        let found = |name: &str| Ok::<_, String>(existing.contains(&name));
        assert_eq!(try_propose_name("Default", found), Ok("Default (2)".to_owned()));
        assert_eq!(try_propose_name("Wheel\x1f [old]\x1c", found), Ok("Wheel (1)".to_owned()));
        // A lookup that fails ends the search with its error, at the first failure.
        let mut asked = Vec::new();
        let failing = |name: &str| {
            asked.push(name.to_owned());
            if name == "Default (2)" {
                Err("service down")
            } else {
                Ok(existing.contains(&name))
            }
        };
        assert_eq!(try_propose_name("Default", failing), Err("service down"));
        assert_eq!(asked, ["Default (1)", "Default (2)"]);
        assert_eq!(remove_text_inside_brackets("a(b)c[d]e"), "ace");
        assert_eq!(remove_text_inside_brackets("a)b"), "a)b", "a stray closing bracket is kept");
        assert_eq!(remove_text_inside_brackets("a(b"), "a");
        assert_eq!(remove_text_inside_brackets("a([b)c]d"), "ad");
    }

    #[test]
    fn modes_and_battery() {
        assert_eq!(default_mode("speed"), SINGLE_AXIS);
        assert_eq!(default_mode("left"), DUAL_AXIS);
        assert_eq!(mode_example(DUAL_AXIS), (DUAL_AXIS_EXAMPLE, false));
        assert_eq!(mode_example("anything"), (SINGLE_AXIS_EXAMPLE, true));
        assert_eq!(battery_percent(2455), 100.0);
        assert_eq!(battery_icon(battery_percent(2117)), "battery-0");
        assert_eq!(battery_icon(25.9), "battery-25");
        assert_eq!(battery_icon(75.0), "battery-75");
        let wheel = BTreeMap::from([("MAGIC WHEEL 1".to_owned(), ControllerView::default())]);
        assert_eq!(magic_wheel_level(&wheel), Some(0));
        assert_eq!(magic_wheel_level(&view(&[], &[])), None);
    }
}
