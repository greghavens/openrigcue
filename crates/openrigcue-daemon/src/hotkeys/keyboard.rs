//! Keyboard hotkeys: the test of the keyboard bindings on every key event,
//! and the shortcut editor's capture of a hotkey.
//!
//! The held keys are one table for every keyboard, keyed by key code.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use super::combo::{hotkey_name, key_name, ComboError, KeyCombo};
use super::CANNED_EFFECTS;

/// Value of a key event: release, press, or auto-repeat (a repeat counts as another key-down event).
pub const KEY_UP: i32 = 0;
pub const KEY_DOWN: i32 = 1;
pub const KEY_REPEAT: i32 = 2;

/// The capture pauses this long after each captured hotkey before it reads the next one.
pub const CAPTURE_PAUSE: Duration = Duration::from_secs(1);

/// One key event of any keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
    pub code: u16,
    pub value: i32,
}

/// What one key event did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KeyPass {
    /// Actions that fired, in binding order.
    pub fired: Vec<String>,
    /// Canned-effect writes (name, 1 or 0) for the canned-effect bindings.
    pub canned: Vec<(String, u8)>,
    /// The binding that could not be tested, with the reason: the pass ends there (the failure is
    /// reported and the next event is handled normally), so the bindings after it are not
    /// tested for this event.
    pub invalid: Option<(String, ComboError)>,
}

/// The keyboard side of the hotkeys.
#[derive(Debug, Default)]
pub struct KeyboardHotkeys {
    held: BTreeSet<u16>,
    capture: Option<Capture>,
    /// The editor session [`Self::capture`] belongs to.
    session: Option<u64>,
}

/// The editor's keyboard capture (`list_shortcuts` on).
#[derive(Debug, Clone, Default)]
struct Capture {
    /// The last hotkey read (the editor's keyboard field), `None` before the first.
    hotkey: Option<String>,
    /// How many hotkeys were read (each read sets the field again, even to the same text).
    reads: u64,
    /// The pause after a read: key releases before this instant are not read.
    pause_until: Option<Instant>,
}

impl KeyboardHotkeys {
    /// The key codes held now.
    pub fn held(&self) -> &BTreeSet<u16> {
        &self.held
    }

    /// Starts or ends the editor's keyboard capture (`list_shortcuts`).
    pub fn set_capture(&mut self, on: bool) {
        match (on, self.capture.is_some()) {
            (true, false) => self.capture = Some(Capture::default()),
            (false, true) => self.capture = None,
            _ => {}
        }
    }

    /// Follows the editor's session (`None`: closed). A new session (the editor was closed and opened again,
    /// even with no key event in between) starts a fresh capture.
    pub fn set_capture_session(&mut self, session: Option<u64>) {
        if self.session != session {
            self.capture = session.map(|_| Capture::default());
            self.session = session;
        }
    }

    /// The hotkey the capture read last.
    pub fn captured(&self) -> Option<&str> {
        self.capture.as_ref().and_then(|c| c.hotkey.as_deref())
    }

    /// How many hotkeys the running capture has read.
    pub fn captures(&self) -> u64 {
        self.capture.as_ref().map_or(0, |c| c.reads)
    }

    /// One key event: the held-key table is updated, then
    /// every non-empty `<action>_keys` binding is tested: a binding whose keys are all held fires its action (and sets its
    /// canned effect to 1); a canned-effect binding that is not held sets its effect to 0. So a held combo
    /// fires again on every further key event, auto-repeat included.
    pub fn event<'a>(
        &mut self,
        event: KeyEvent,
        bindings: impl IntoIterator<Item = (&'a str, &'a str)>,
        now: Instant,
    ) -> KeyPass {
        match event.value {
            KEY_DOWN | KEY_REPEAT => {
                self.held.insert(event.code);
            }
            KEY_UP => {
                self.read_hotkey(event.code, now);
                self.held.remove(&event.code);
            }
            _ => return KeyPass::default(),
        }
        let mut pass = KeyPass::default();
        for (action, keys) in bindings {
            if keys.is_empty() {
                continue;
            }
            let combo = match KeyCombo::parse(keys) {
                Ok(combo) => combo,
                Err(error) => {
                    pass.invalid = Some((action.to_owned(), error));
                    break;
                }
            };
            let canned = CANNED_EFFECTS.contains(&action);
            if combo.is_pressed(&self.held) {
                pass.fired.push(action.to_owned());
                if canned {
                    pass.canned.push((action.to_owned(), 1));
                }
            } else if canned {
                pass.canned.push((action.to_owned(), 0));
            }
        }
        pass
    }

    /// Reads a hotkey for the capture: at the first key release, the names of the keys still held plus the released one.
    fn read_hotkey(&mut self, released: u16, now: Instant) {
        let Some(capture) = self.capture.as_mut() else { return };
        if capture.pause_until.is_some_and(|until| now < until) {
            return;
        }
        let names = self.held.iter().chain(std::iter::once(&released)).filter_map(|code| key_name(*code));
        capture.hotkey = Some(hotkey_name(names));
        capture.reads += 1;
        capture.pause_until = Some(now + CAPTURE_PAUSE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CTRL: u16 = 29;
    const SHIFT: u16 = 42;
    const F1: u16 = 59;
    const F2: u16 = 60;
    const A: u16 = 30;

    fn down(code: u16) -> KeyEvent {
        KeyEvent { code, value: KEY_DOWN }
    }

    fn up(code: u16) -> KeyEvent {
        KeyEvent { code, value: KEY_UP }
    }

    fn bindings() -> Vec<(&'static str, &'static str)> {
        vec![("power_wind_switch", "ctrl+shift+f1"), ("ejection_seat", "ctrl+shift+f2"), ("led_switch", "")]
    }

    /// A combo fires when its last key goes down, again on auto-repeat and on any other key event while held,
    /// and not on the releases that break it.
    #[test]
    fn held_combo_fires_on_every_key_event() {
        let mut k = KeyboardHotkeys::default();
        let now = Instant::now();
        let fired = |k: &mut KeyboardHotkeys, e| k.event(e, bindings(), now).fired;
        assert!(fired(&mut k, down(CTRL)).is_empty());
        assert!(fired(&mut k, down(SHIFT)).is_empty());
        assert_eq!(fired(&mut k, down(F1)), ["power_wind_switch"]);
        assert_eq!(fired(&mut k, KeyEvent { code: F1, value: KEY_REPEAT }), ["power_wind_switch"]);
        assert_eq!(fired(&mut k, down(A)), ["power_wind_switch"], "another key while the combo is held");
        assert_eq!(fired(&mut k, up(A)), ["power_wind_switch"]);
        assert!(fired(&mut k, up(F1)).is_empty());
        assert!(fired(&mut k, up(SHIFT)).is_empty());
        assert!(fired(&mut k, KeyEvent { code: CTRL, value: 7 }).is_empty(), "not a key event");
    }

    /// Canned effects are level-triggered: 1 while their combo is held, 0 on every other key event.
    #[test]
    fn canned_effects_follow_the_combo() {
        let mut k = KeyboardHotkeys::default();
        let now = Instant::now();
        let pass = k.event(down(CTRL), bindings(), now);
        assert_eq!(pass.canned, [("ejection_seat".to_owned(), 0)]);
        k.event(down(SHIFT), bindings(), now);
        let pass = k.event(down(F2), bindings(), now);
        assert_eq!(pass.fired, ["ejection_seat"]);
        assert_eq!(pass.canned, [("ejection_seat".to_owned(), 1)]);
        let pass = k.event(up(F2), bindings(), now);
        assert_eq!(pass.canned, [("ejection_seat".to_owned(), 0)]);
    }

    /// A binding that cannot be tested ends the pass: the bindings before it were tested, the ones after it are not.
    #[test]
    fn invalid_binding_ends_the_pass() {
        let mut k = KeyboardHotkeys::default();
        let pass = k.event(down(F1), vec![("a", "f1"), ("b", "hyper+f1"), ("c", "f1")], Instant::now());
        assert_eq!(pass.fired, ["a"]);
        assert_eq!(pass.invalid.map(|(action, _)| action).as_deref(), Some("b"));
    }

    /// Capture: the first release gives the held keys plus the released one; the capture then pauses
    /// one second before the next read.
    #[test]
    fn capture_reads_the_combo_at_the_first_release() {
        let mut k = KeyboardHotkeys::default();
        let start = Instant::now();
        k.event(down(CTRL), [], start);
        k.event(up(CTRL), [], start);
        assert_eq!(k.captured(), None, "no capture running");
        k.set_capture(true);
        for code in [CTRL, SHIFT, F2] {
            k.event(down(code), [], start);
        }
        k.event(up(SHIFT), [], start);
        assert_eq!(k.captured(), Some("ctrl+shift+f2"));
        k.event(up(CTRL), [], start + Duration::from_millis(500));
        assert_eq!(k.captured(), Some("ctrl+shift+f2"), "within the pause");
        k.event(down(A), [], start + Duration::from_secs(2));
        k.event(up(F2), [], start + Duration::from_secs(2));
        assert_eq!(k.captured(), Some("a+f2"));
        assert_eq!(k.captures(), 2);
        k.set_capture(false);
        assert_eq!(k.captured(), None);
    }
}
