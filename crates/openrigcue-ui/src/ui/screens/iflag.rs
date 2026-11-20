//! Tuning → IFlag: the iFlag's switches (RPM, start lights, flags, pits, spotter) and its credits; the flags
//! the running game does not support are disabled and, when a profile loads, switched off; the stand-by lock;
//! and the hardware set-up action.

use openrigcue_config::catalog::registry::Registry;
use openrigcue_model::native_value::LegacyValue;

use super::super::components::RowState;
use super::super::read;
use super::super::Screen;
use super::tuning;
use crate::app::MyApp;
use crate::spec::{self, Control, Page, Widget};

/// The page description.
pub fn page() -> Page {
    spec::iflag_page(Registry::builtin())
}

fn is_flag(id: &str) -> bool {
    Registry::builtin().flags.iter().any(|f| f.group != "none" && f.name == id)
}

fn locked(app: &MyApp) -> bool {
    tuning::locked(app, true, false)
}

fn row_state(app: &MyApp, control: &Control, locked: bool) -> Option<RowState> {
    let mut state = tuning::base_state(app, control, !locked);
    if is_flag(&control.id) && app.game_feature(&control.id) == Some(false) {
        state.enabled = false;
        state.value = serde_json::Value::Bool(false);
        state.hint = Some(tuning::unused_hint(app));
    }
    Some(state)
}

/// The body.
pub fn main(app: &MyApp) -> String {
    let locked = locked(app);
    tuning::panel(
        app,
        Screen::IFlag,
        &page(),
        locked,
        "",
        |control| row_state(app, control, locked),
        |_| Some((true, None)),
        &[],
        "",
    )
}

/// The values Compare looks at.
pub fn compared(app: &MyApp) -> Vec<(String, Vec<String>)> {
    tuning::compared(app, &page())
}

/// A flag's switch changed: the flag's entry in the tune takes it.
pub fn set(app: &mut MyApp, control: &str, part: &str, value: &str) -> Result<(), String> {
    if locked(app) {
        return Ok(());
    }
    match (control, part) {
        (flag, "") if is_flag(flag) && app.game_feature(flag) != Some(false) => {
            app.set_tune_native_checked(&["iflag", flag], &LegacyValue::Bool(read::checked(value)))
        }
        (flag, "") if is_flag(flag) => Ok(()),
        _ => Err(format!("no IFlag value {control:?}")),
    }
}

/// A button of the screen.
pub fn action(app: &mut MyApp, control: &str, index: Option<usize>) -> Result<(), String> {
    let run = tuning::button(&page(), control, index).ok_or_else(|| format!("no IFlag button {control:?}"))?;
    if tuning::run_common(app, &run)? {
        Ok(())
    } else {
        Err(format!("unsupported IFlag action {run:?}"))
    }
}

/// A loaded profile: every flag the running game does not support is switched off.
pub fn on_profile_load(app: &mut MyApp) -> Result<(), String> {
    for control in page().controls() {
        if !matches!(control.widget, Widget::Switch { .. }) || app.game_feature(&control.id) != Some(false) {
            continue;
        }
        if read::truth(app.tune(&["iflag", &control.id])) {
            app.set_tune_native_checked(&["iflag", &control.id], &LegacyValue::Bool(false))?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "iflag_tests.rs"]
mod tests;
