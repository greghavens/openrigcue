//! Controller-button hotkeys: the shortcut part of the controller polling pass
//! (one pass every 10 ms on Windows, 25 ms elsewhere), with its cache of the bindings.
//!
//! Button states live in the gamepad table, keyed `'<controller name>^BTN_<n>'`; a binding
//! (`<action>_btns`) is a `+`-joined list of such keys and fires when the set of pressed keys equals it exactly.

use std::collections::{BTreeMap, BTreeSet};

use openrigcue_config::settings::Shortcuts;
use openrigcue_sources::api::JoystickEvent;

use super::CANNED_EFFECTS;

/// Passes between two reads of the bindings.
pub const CACHE_PASSES: u32 = 100;

/// The `gamepad` key of a controller button.
pub fn button_key(controller: &str, index: u32) -> String {
    format!("{controller}^BTN_{index}")
}

/// The bindings as last read from the settings.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cache {
    /// The non-empty `_btns` bindings: action → the keys it needs.
    triggers: Vec<Trigger>,
    /// Controllers whose presses count.
    white_list: BTreeSet<String>,
    /// Whether the shortcut editor was open when the cache was built.
    list_shortcuts: bool,
}

impl Cache {
    fn build(shortcuts: &Shortcuts, devices: &[String], list_shortcuts: bool) -> Cache {
        let mut triggers = Vec::new();
        let mut white_list = BTreeSet::new();
        for (action, binding) in &shortcuts.bindings {
            if binding.buttons.is_empty() {
                continue;
            }
            triggers.push(Trigger::new(action, &binding.buttons));
            white_list.insert(binding.buttons.split('^').next().unwrap_or_default().to_owned());
        }
        if list_shortcuts {
            white_list = devices.iter().cloned().collect();
        }
        for ignored in shortcuts.ignore_joysticks.split(',') {
            white_list.remove(ignored);
        }
        Cache { triggers, white_list, list_shortcuts }
    }
}

/// One binding: the keys of its `+`-joined list.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Trigger {
    action: String,
    /// The keys in the binding's own order, a repeated key kept at its first place.
    /// The canned-effect loop walks them in this order, so the last-listed key wins.
    ordered: Vec<String>,
    /// The same keys as a set, for the comparison with the pressed keys.
    keys: BTreeSet<String>,
}

impl Trigger {
    fn new(action: &str, buttons: &str) -> Trigger {
        let mut ordered: Vec<String> = Vec::new();
        for key in buttons.split('+') {
            if !ordered.iter().any(|k| k == key) {
                ordered.push(key.to_owned());
            }
        }
        let keys = ordered.iter().cloned().collect();
        Trigger { action: action.to_owned(), ordered, keys }
    }
}

/// What one pass did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ButtonPass {
    /// Actions that fired, in order (an action can appear more than once).
    pub fired: Vec<String>,
    /// Canned-effect writes (name, value), in order.
    pub canned: Vec<(String, u8)>,
}

/// The state the controller pass keeps between passes.
#[derive(Debug)]
pub struct ButtonHotkeys {
    /// `APP.enable_sdl2`, read once at start-up: when off, a pass returns at once.
    enabled: bool,
    gamepad: BTreeMap<String, u8>,
    /// The gamepad keys in the order they were first inserted.
    order: Vec<String>,
    cache_tick: u32,
    cache: Option<Cache>,
}

impl Default for ButtonHotkeys {
    fn default() -> ButtonHotkeys {
        ButtonHotkeys::new(true)
    }
}

impl ButtonHotkeys {
    /// Button hotkeys with controller input enabled or disabled.
    pub fn new(enabled: bool) -> ButtonHotkeys {
        ButtonHotkeys { enabled, gamepad: BTreeMap::new(), order: Vec::new(), cache_tick: 0, cache: None }
    }

    /// The gamepad table: every button that sent an event, 1 while pressed by a listed controller.
    pub fn gamepad(&self) -> &BTreeMap<String, u8> {
        &self.gamepad
    }

    /// The gamepad table's entries in first-insertion order.
    pub fn gamepad_in_order(&self) -> Vec<(String, u8)> {
        self.order.iter().map(|key| (key.clone(), self.gamepad.get(key).copied().unwrap_or_default())).collect()
    }

    fn set_button(&mut self, key: String, value: u8) {
        if !self.gamepad.contains_key(&key) {
            self.order.push(key.clone());
        }
        self.gamepad.insert(key, value);
    }

    /// One pass over the events queued since the previous one.
    ///
    /// The bindings are re-read every [`CACHE_PASSES`] passes, when the editor opens or closes
    /// (`list_shortcuts`), and on the pass after a controller was added or removed. A press of a listed controller sets
    /// its key to 1 and arms the trigger test, a press of any other controller sets it to 0 and disarms it, a
    /// release sets it to 0 and arms it. After every event, each button event updates the canned effects
    /// from the keys of their bindings, and while the trigger is armed (it stays armed for the rest of the
    /// pass, so a later axis or hat event of the same pass tests again) every binding equal to the set of
    /// pressed keys fires. With controller input disabled the pass does nothing.
    pub fn update(
        &mut self,
        events: &[JoystickEvent],
        shortcuts: &Shortcuts,
        devices: &[String],
        list_shortcuts: bool,
    ) -> ButtonPass {
        if !self.enabled {
            return ButtonPass::default();
        }
        self.cache_tick += 1;
        let stale = self.cache.as_ref().is_none_or(|cache| cache.list_shortcuts != list_shortcuts);
        if self.cache_tick >= CACHE_PASSES || stale {
            self.cache_tick = 0;
            self.cache = Some(Cache::build(shortcuts, devices, list_shortcuts));
        }
        let Some(cache) = self.cache.take() else { return ButtonPass::default() };
        let mut pass = ButtonPass::default();
        let mut trigger = false;
        for event in events {
            let mut button_event = false;
            match event {
                JoystickEvent::Added { .. } | JoystickEvent::Removed { .. } => self.cache_tick = CACHE_PASSES,
                JoystickEvent::Axis { .. } | JoystickEvent::Hat { .. } | JoystickEvent::Other { .. } => {}
                JoystickEvent::Button { controller, index, pressed } => {
                    button_event = true;
                    let key = button_key(controller, *index);
                    if !pressed {
                        self.set_button(key, 0);
                        trigger = true;
                    } else if cache.white_list.contains(controller) {
                        self.set_button(key, 1);
                        trigger = true;
                    } else {
                        self.set_button(key, 0);
                        trigger = false;
                    }
                }
            }
            if button_event {
                for trigger in &cache.triggers {
                    if !CANNED_EFFECTS.contains(&trigger.action.as_str()) {
                        continue;
                    }
                    for key in &trigger.ordered {
                        if let Some(value) = self.gamepad.get(key) {
                            pass.canned.push((trigger.action.clone(), *value));
                        }
                    }
                }
            }
            if trigger {
                let pressed: BTreeSet<&String> =
                    self.gamepad.iter().filter(|(_, v)| **v == 1).map(|(k, _)| k).collect();
                for trigger in &cache.triggers {
                    let keys = &trigger.keys;
                    if keys.len() == pressed.len() && keys.iter().all(|k| pressed.contains(k)) {
                        pass.fired.push(trigger.action.clone());
                    }
                }
            }
        }
        self.cache = Some(cache);
        pass
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrigcue_config::settings::Binding;

    const WHEEL: &str = "Wheel";
    const BOX: &str = "Button Box";

    fn shortcuts(entries: &[(&str, &str)]) -> Shortcuts {
        let mut s = Shortcuts::default();
        s.bindings.clear();
        for (action, buttons) in entries {
            s.bindings.insert((*action).into(), Binding { keys: String::new(), buttons: (*buttons).into() });
        }
        s
    }

    fn press(controller: &str, index: u32, pressed: bool) -> JoystickEvent {
        JoystickEvent::Button { controller: controller.into(), index, pressed }
    }

    fn axis(controller: &str) -> JoystickEvent {
        JoystickEvent::Axis { controller: controller.into(), index: 0, value: 100 }
    }

    fn devices() -> Vec<String> {
        vec![WHEEL.into(), BOX.into()]
    }

    /// A bound button fires on press only; its release arms the test but the pressed set (empty) matches
    /// nothing. The gamepad keeps every button that sent an event.
    #[test]
    fn press_fires_release_does_not() {
        let s = shortcuts(&[("motion_box_switch", "Wheel^BTN_3")]);
        let mut b = ButtonHotkeys::default();
        assert_eq!(b.update(&[press(WHEEL, 3, true)], &s, &devices(), false).fired, ["motion_box_switch"]);
        assert!(b.update(&[press(WHEEL, 3, false)], &s, &devices(), false).fired.is_empty());
        assert_eq!(b.gamepad().get("Wheel^BTN_3"), Some(&0));
    }

    /// The match is exact: another pressed button (of any controller) stops the binding from firing.
    #[test]
    fn exact_match_of_pressed_buttons() {
        let s = shortcuts(&[("a", "Wheel^BTN_1"), ("b", "Wheel^BTN_1+Wheel^BTN_2"), ("c", "Button Box^BTN_0")]);
        let mut b = ButtonHotkeys::default();
        let pass = b.update(&[press(WHEEL, 1, true), press(WHEEL, 2, true)], &s, &devices(), false);
        assert_eq!(pass.fired, ["a", "b"], "BTN_1 alone, then BTN_1 + BTN_2");
        let pass = b.update(&[press(BOX, 0, true)], &s, &devices(), false);
        assert!(pass.fired.is_empty(), "three buttons held");
    }

    /// Only controllers named by a binding count; a press of another one sets its key to 0 and disarms the
    /// trigger for the rest of the pass. `ignore_joysticks` removes names; with the editor open every
    /// connected controller counts.
    #[test]
    fn white_list() {
        let mut s = shortcuts(&[("a", "Wheel^BTN_1")]);
        let mut b = ButtonHotkeys::default();
        let pass = b.update(&[press(WHEEL, 1, true), press(BOX, 4, true)], &s, &devices(), false);
        assert_eq!(pass.fired, ["a"]);
        assert_eq!(b.gamepad().get("Button Box^BTN_4"), Some(&0));
        let pass = b.update(&[press(WHEEL, 1, false), press(BOX, 4, true), axis(WHEEL)], &s, &devices(), false);
        assert!(pass.fired.is_empty(), "the later unlisted press disarmed the test");
        b.update(&[press(BOX, 4, false)], &s, &devices(), false);
        s.ignore_joysticks = "Wheel".into();
        let mut b = ButtonHotkeys::default();
        assert!(b.update(&[press(WHEEL, 1, true)], &s, &devices(), false).fired.is_empty());
        let mut b = ButtonHotkeys::default();
        b.update(&[press(BOX, 7, true)], &s, &devices(), true);
        assert_eq!(b.gamepad().get("Button Box^BTN_7"), Some(&1), "editor open: every controller listed");
    }

    /// The trigger stays armed for the rest of the pass: an axis or hat event after a press fires the
    /// binding again in the same pass.
    #[test]
    fn later_events_of_the_pass_fire_again() {
        let s = shortcuts(&[("led_switch", "Wheel^BTN_0")]);
        let mut b = ButtonHotkeys::default();
        let events = [
            press(WHEEL, 0, true),
            axis(WHEEL),
            JoystickEvent::Hat { controller: WHEEL.into(), index: 0 },
            JoystickEvent::Other { controller: WHEEL.into() },
        ];
        assert_eq!(
            b.update(&events, &s, &devices(), false).fired,
            ["led_switch", "led_switch", "led_switch", "led_switch"]
        );
        assert!(b.update(&[axis(WHEEL)], &s, &devices(), false).fired.is_empty(), "next pass starts disarmed");
    }

    #[test]
    fn added_device_enters_editor_white_list_on_the_following_pass() {
        let s = shortcuts(&[("led_switch", "Wheel^BTN_0")]);
        let mut b = ButtonHotkeys::default();
        let events = [JoystickEvent::Added { controller: WHEEL.into() }, press(WHEEL, 0, true)];
        assert!(b.update(&events, &s, &[], true).fired.is_empty());
        assert_eq!(b.gamepad().get("Wheel^BTN_0"), Some(&0));
        assert_eq!(b.update(&[press(WHEEL, 0, true)], &s, &devices(), true).fired, ["led_switch"]);
    }

    /// Canned effects follow the gamepad value of their binding's keys on every button event.
    #[test]
    fn canned_effects_follow_their_buttons() {
        let s = shortcuts(&[("cannon_firing", "Wheel^BTN_5"), ("led_switch", "Wheel^BTN_6")]);
        let mut b = ButtonHotkeys::default();
        let pass = b.update(&[press(WHEEL, 6, true)], &s, &devices(), false);
        assert!(pass.canned.is_empty(), "BTN_5 has not sent an event yet");
        let pass = b.update(&[press(WHEEL, 5, true), press(WHEEL, 5, false)], &s, &devices(), false);
        assert_eq!(pass.canned, [("cannon_firing".to_owned(), 1), ("cannon_firing".to_owned(), 0)]);
    }

    /// The canned loop walks a binding's keys in their written order,
    /// so the last-listed key decides, not the last in sorted order.
    #[test]
    fn canned_effects_follow_the_written_key_order() {
        let s = shortcuts(&[("cannon_firing", "Wheel^BTN_2+Wheel^BTN_10")]);
        let mut b = ButtonHotkeys::default();
        b.update(&[press(WHEEL, 10, false)], &s, &devices(), false);
        let pass = b.update(&[press(WHEEL, 2, true)], &s, &devices(), false);
        assert_eq!(pass.canned, [("cannon_firing".to_owned(), 1), ("cannon_firing".to_owned(), 0)]);
        let s = shortcuts(&[("cannon_firing", "Wheel^BTN_10+Wheel^BTN_2+Wheel^BTN_10")]);
        let mut b = ButtonHotkeys::default();
        b.update(&[press(WHEEL, 10, false)], &s, &devices(), false);
        let pass = b.update(&[press(WHEEL, 2, true)], &s, &devices(), false);
        assert_eq!(pass.canned, [("cannon_firing".to_owned(), 0), ("cannon_firing".to_owned(), 1)], "first place kept");
        let pass = b.update(&[press(WHEEL, 10, true)], &s, &devices(), false);
        assert_eq!(pass.fired, ["cannon_firing"], "exact match ignores the order and the repeat");
    }

    /// The gamepad keeps the order in which buttons first sent an event.
    #[test]
    fn gamepad_keeps_insertion_order() {
        let s = shortcuts(&[("a", "Wheel^BTN_1")]);
        let mut b = ButtonHotkeys::default();
        b.update(&[press(WHEEL, 2, true), press(WHEEL, 10, true), press(WHEEL, 2, false)], &s, &devices(), false);
        assert_eq!(b.gamepad_in_order(), [("Wheel^BTN_2".to_owned(), 0), ("Wheel^BTN_10".to_owned(), 1)]);
    }

    /// Controller input disabled: the pass returns at once, nothing fires and the gamepad stays empty.
    #[test]
    fn stub_joystick_without_sdl2() {
        let s = shortcuts(&[("motion_box_switch", "Wheel^BTN_3"), ("cannon_firing", "Wheel^BTN_4")]);
        let mut b = ButtonHotkeys::new(false);
        let pass = b.update(&[press(WHEEL, 3, true), press(WHEEL, 4, true)], &s, &devices(), true);
        assert_eq!(pass, ButtonPass::default());
        assert!(b.gamepad().is_empty());
        assert!(b.gamepad_in_order().is_empty());
    }

    /// Bindings are re-read every 100 passes, on a change of `list_shortcuts`, and on the pass after a device
    /// was added or removed.
    #[test]
    fn bindings_cache() {
        let old = shortcuts(&[("a", "Wheel^BTN_1")]);
        let new = shortcuts(&[("b", "Wheel^BTN_1")]);
        let mut b = ButtonHotkeys::default();
        b.update(&[], &old, &devices(), false);
        for _ in 0..(CACHE_PASSES - 2) {
            b.update(&[], &new, &devices(), false);
        }
        assert_eq!(b.update(&[press(WHEEL, 1, true)], &new, &devices(), false).fired, ["a"], "pass 99: cached");
        assert_eq!(b.update(&[press(WHEEL, 1, false)], &new, &devices(), false).fired, Vec::<String>::new());
        assert_eq!(b.update(&[press(WHEEL, 1, true)], &new, &devices(), false).fired, ["b"], "re-read");
        let mut b = ButtonHotkeys::default();
        b.update(&[JoystickEvent::Added { controller: WHEEL.into() }], &old, &devices(), false);
        assert_eq!(b.update(&[press(WHEEL, 1, true)], &new, &devices(), false).fired, ["b"], "after a hot-plug");
    }
}
