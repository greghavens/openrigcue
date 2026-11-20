//! Tuning → SIM3D: the pedal and seat haptics (Throttle, Brake, Clutch, Seat) with their switches and the
//! advanced engine, wheel slip / lock and traction loss settings; the running game's feature rule (an effect
//! and the advanced rows of its kind); the stand-by lock; the header actions (shortcuts, hardware set-up,
//! downloaded profiles); and the SIM3D hot keys. Tuning → HapticR has the same rows in its own tables
//! ([`Kind`]).

use openrigcue_config::catalog::registry::{GroupedEffectInfo, Registry};
use openrigcue_config::synced_profiles::Category;
use openrigcue_model::native_value::LegacyValue;

use super::super::components::{self, RowState};
use super::super::html::t;
use super::super::read;
use super::super::Screen;
use super::tuning;
use crate::app::MyApp;
use crate::spec::{self, Action, Control, Page};

/// What tells SIM3D and HapticR apart: the screen, its page description, its tables in the tune, its
/// effects, the key of its RPM-start row, the prefix of its hot keys and its library category.
pub struct Kind {
    pub screen: Screen,
    pub page: fn() -> Page,
    pub effects_key: &'static str,
    pub switch_key: &'static str,
    pub effects: fn() -> &'static [GroupedEffectInfo],
    pub rpm_start_row: &'static str,
    pub prefix: &'static str,
    pub category: Category,
    /// The tooltip of the page's Shortcuts action.
    pub shortcuts_hint: &'static str,
}

/// SIM3D.
pub const SIM3D: Kind = Kind {
    screen: Screen::Sim3d,
    page,
    effects_key: "rumble_effects",
    switch_key: "rumble_switch",
    effects: rumble_effects,
    rpm_start_row: "rpm_start_rumble_perc",
    prefix: "rumble_",
    category: Category::Rumble,
    shortcuts_hint: "Shows the keyboard, gamepad, joystick and buttonbox shortcuts that switch SIM3D off or to auto, and lets you change them.",
};

/// The page description.
pub fn page() -> Page {
    spec::sim3d_page(Registry::builtin())
}

fn rumble_effects() -> &'static [GroupedEffectInfo] {
    &Registry::builtin().rumble_effects
}

impl Kind {
    fn is_effect(&self, id: &str) -> bool {
        (self.effects)().iter().any(|e| e.id == id)
    }

    /// The game feature column of a row: an effect's kind (its key after the group), or the kind an advanced
    /// row belongs to.
    fn feature_column<'a>(&self, id: &'a str) -> Option<&'a str> {
        if self.is_effect(id) {
            return Some(id.split_once('_').map_or(id, |(_, rest)| rest));
        }
        match id {
            "rpm_fading_perc" => Some("engine"),
            row if row == self.rpm_start_row => Some("engine"),
            "wheel_slip_min_activation_perc" | "wheel_lock_min_activation_perc" => Some("wheel_slip"),
            "traction_loss_min_activation_perc" => Some("traction_loss"),
            _ => None,
        }
    }

    fn row_state(&self, app: &MyApp, control: &Control, locked: bool) -> Option<RowState> {
        let mut state = tuning::base_state(app, control, !locked);
        if let Some(column) = self.feature_column(&control.id) {
            let (visible, enabled) = tuning::feature(app, column);
            if !visible {
                return None;
            }
            if !enabled {
                state.enabled = false;
                state.row_enabled = false;
                state.hint = Some(tuning::unused_hint(app));
            }
        }
        Some(state)
    }

    /// Whether the row `control` takes a change of `part` now (not in stand-by, nor while the game lacks
    /// its kind).
    fn accepts(&self, app: &MyApp, control: &str, part: &str) -> bool {
        let locked = tuning::locked(app, true, false);
        tuning::accepts(&(self.page)(), control, part, |row| self.row_state(app, row, locked))
    }

    /// The body.
    pub fn main(&self, app: &MyApp) -> String {
        let locked = tuning::locked(app, true, false);
        tuning::panel(
            app,
            self.screen,
            &(self.page)(),
            locked,
            "",
            |control| self.row_state(app, control, locked),
            |run| match run {
                Action::Shortcuts { .. } => Some((true, Some(t(app, self.shortcuts_hint)))),
                _ => Some((true, None)),
            },
            &[tuning::library_extra()],
            "",
        )
    }

    /// A value of the screen changed.
    pub fn set(&self, app: &mut MyApp, control: &str, part: &str, value: &str) -> Result<(), String> {
        if !self.accepts(app, control, part) {
            return Ok(());
        }
        self.write(app, control, part, value)
    }

    /// The stored value a row's handler wrote: effects as whole numbers, their switches as booleans, the
    /// advanced percentages as given and the RPM fade range's ends as whole numbers.
    fn write(&self, app: &mut MyApp, control: &str, part: &str, value: &str) -> Result<(), String> {
        let effects = self.effects_key;
        match (control, part) {
            (effect, "toggle") if self.is_effect(effect) => {
                app.set_tune_native_checked(&[self.switch_key, effect], &LegacyValue::Bool(read::checked(value)))
            }
            (effect, "") if self.is_effect(effect) => {
                app.set_tune_native_checked(&[effects, effect], &LegacyValue::Int(read::int(value)?))
            }
            ("rpm_fading_perc", end) => {
                let given = read::int(value)?;
                if end == "high" {
                    let low = components::number(app.tune(&[effects, "rpm_start_fading"])).trunc() as i64;
                    app.set_tune_native_checked(&[effects, "rpm_stop_fading"], &LegacyValue::Int(given.max(low)))
                } else {
                    let high = components::number(app.tune(&[effects, "rpm_stop_fading"])).trunc() as i64;
                    app.set_tune_native_checked(&[effects, "rpm_start_fading"], &LegacyValue::Int(given.min(high)))
                }
            }
            (row, "") => {
                let page = (self.page)();
                let key = page
                    .control(row)
                    .and_then(|c| c.field.as_ref())
                    .and_then(|f| f.path.get(1))
                    .ok_or_else(|| format!("no {} value {row:?}", self.screen.key()))?;
                app.set_tune_native_checked(&[effects, key], &read::number(value)?)
            }
            _ => Err(format!("no {} value {control:?}", self.screen.key())),
        }
    }

    /// A button of the screen.
    pub fn action(&self, app: &mut MyApp, control: &str, index: Option<usize>) -> Result<(), String> {
        if control == "@library" {
            tuning::open_library(app, self.category);
            return Ok(());
        }
        let key = self.screen.key();
        let run =
            tuning::button(&(self.page)(), control, index).ok_or_else(|| format!("no {key} button {control:?}"))?;
        if tuning::run_common(app, &run)? {
            Ok(())
        } else {
            Err(format!("unsupported {key} action {run:?}"))
        }
    }

    /// The hot keys: `<prefix><effect>_up` / `_down` move the effect one step.
    pub fn apply_hotkeys(&self, app: &mut MyApp, pending: &dyn Fn(&str) -> bool) -> Result<bool, String> {
        let steps: Vec<(String, String)> =
            (self.effects)().iter().map(|e| (format!("{}{}", self.prefix, e.id), e.id.clone())).collect();
        let page = (self.page)();
        let mut moved = false;
        for (action, row) in steps {
            for (suffix, delta) in [("up", 1.0), ("down", -1.0)] {
                if !pending(&format!("{action}_{suffix}")) {
                    continue;
                }
                let Some(crate::spec::Widget::Slider { scale }) = page.control(&row).map(|c| &c.widget) else {
                    continue;
                };
                let current = components::number(app.tune(&[self.effects_key, &row])).trunc();
                self.write(app, &row, "", &super::super::html::num(scale.snap(current + delta)))?;
                moved = true;
            }
        }
        Ok(moved)
    }
}

/// The body.
pub fn main(app: &MyApp) -> String {
    SIM3D.main(app)
}

/// The values Compare looks at.
pub fn compared(app: &MyApp) -> Vec<(String, Vec<String>)> {
    tuning::compared(app, &page())
}

/// A value of the screen changed.
pub fn set(app: &mut MyApp, control: &str, part: &str, value: &str) -> Result<(), String> {
    SIM3D.set(app, control, part, value)
}

/// A button of the screen.
pub fn action(app: &mut MyApp, control: &str, index: Option<usize>) -> Result<(), String> {
    SIM3D.action(app, control, index)
}

/// The SIM3D hot keys.
pub fn apply_hotkeys(app: &mut MyApp, pending: &dyn Fn(&str) -> bool) -> Result<bool, String> {
    SIM3D.apply_hotkeys(app, pending)
}

#[cfg(test)]
#[path = "sim3d_tests.rs"]
mod tests;
