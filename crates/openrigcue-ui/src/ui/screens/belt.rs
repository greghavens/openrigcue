//! Tuning → Belt: the surge, sway and heave groups (status, reverse, strength and smoothing); the stand-by
//! lock; the header actions (calibration, shortcuts, hardware set-up, downloaded profiles); the calibration
//! question and the calibration result; the Belt hot keys; and the belt switch that follows the game when a
//! profile loads.

use std::sync::{Mutex, Weak};
use std::time::Duration;

use openrigcue_config::synced_profiles::Category;
use openrigcue_model::native_value::LegacyValue;
use openrigcue_model::python_compare::native_equal;
use serde_json::json;

use super::super::components::RowState;
use super::super::html::t;
use super::super::notices;
use super::super::read;
use super::super::Screen;
use super::tuning::{self, Dialog};
use crate::app::MyApp;
use crate::backend::BackendExt;
use crate::spec::{self, Action, Control, Page, Widget};

/// The page description.
pub fn page() -> Page {
    spec::belt_page()
}

/// The hot-key actions and the rows they move.
const HOTKEYS: [(&str, &str); 6] = [
    ("belt_surge", "belt_surge"),
    ("belt_surge_smoothing", "belt_surge_smoothing"),
    ("belt_heave", "belt_heave"),
    ("belt_heave_smoothing", "belt_heave_smoothing"),
    ("belt_sway_curving", "belt_curving"),
    ("belt_sway_smoothing", "belt_sway_smoothing"),
];

fn locked(app: &MyApp) -> bool {
    tuning::locked(app, true, false)
}

/// Whether the belt is connected (its calibration needs it).
fn connected(app: &MyApp) -> bool {
    app.backend
        .accessory_status("belt")
        .is_ok_and(|status| native_equal(&status, &LegacyValue::Str("Connected".into())))
}

/// The body.
pub fn main(app: &MyApp) -> String {
    let locked = locked(app);
    let connected = connected(app);
    tuning::panel(
        app,
        Screen::Belt,
        &page(),
        locked,
        "",
        |control: &Control| -> Option<RowState> { Some(tuning::base_state(app, control, !locked)) },
        |run| match run {
            Action::BeltCalibrate if !connected => Some((false, Some(t(app, "Connect the belt to calibrate it.")))),
            _ => Some((true, None)),
        },
        &[tuning::library_extra()],
        "",
    )
}

/// The values Compare looks at.
pub fn compared(app: &MyApp) -> Vec<(String, Vec<String>)> {
    tuning::compared(app, &page())
}

/// A value of the screen changed: the stored value the Belt page's handler wrote (switches as 0 or 1, the
/// sliders as whole numbers).
pub fn set(app: &mut MyApp, control: &str, part: &str, value: &str) -> Result<(), String> {
    if locked(app) {
        return Ok(());
    }
    write(app, control, part, value)
}

fn write(app: &mut MyApp, control: &str, part: &str, value: &str) -> Result<(), String> {
    let page = page();
    let row = page.control(control).filter(|_| part.is_empty()).ok_or_else(|| format!("no Belt value {control:?}"))?;
    let key = row.field.as_ref().and_then(|f| f.path.first()).ok_or_else(|| format!("no Belt value {control:?}"))?;
    let stored = match row.widget {
        Widget::Switch { .. } => LegacyValue::Int(i64::from(read::checked(value))),
        _ => LegacyValue::Int(read::int(value)?),
    };
    app.set_tune_native_checked(&[key], &stored)
}

/// A button of the screen.
pub fn action(app: &mut MyApp, control: &str, index: Option<usize>) -> Result<(), String> {
    if control == "@library" {
        tuning::open_library(app, Category::Belt);
        return Ok(());
    }
    let run = tuning::button(&page(), control, index).ok_or_else(|| format!("no Belt button {control:?}"))?;
    if tuning::run_common(app, &run)? {
        return Ok(());
    }
    match run {
        Action::BeltCalibrate => {
            tuning::open_dialog(app, Dialog::BeltCalibrate);
            Ok(())
        }
        other => Err(format!("unsupported Belt action {other:?}")),
    }
}

/// The calibration question.
pub fn calibrate_dialog(app: &MyApp) -> String {
    super::super::dialogs::frame(
        app,
        &t(app, "Do you have the belt on?"),
        &format!("<p class=\"dialog-text\">{}</p>", t(app, "Put the belt on before starting the calibration")),
        Some((&t(app, "Calibrate"), true)),
    )
}

/// The calibration was confirmed: the belt calibrates when it is connected and switched on.
pub fn request_calibration(app: &mut MyApp) -> Result<(), String> {
    let status = app.backend.accessory_status("belt").map_err(|error| error.detail)?;
    if !native_equal(&status, &LegacyValue::Str("Connected".into())) {
        return Ok(());
    }
    let enabled = app.backend.control_value(&["switches", "belt"]).map_err(|error| error.detail)?;
    if native_equal(&enabled, &LegacyValue::Int(1)) {
        app.backend.post("belt/homing", json!({})).map_err(|error| error.detail)?;
    }
    Ok(())
}

/// The calibration started (`belt_homing` became 3): its result is awaited on a thread of its own.
pub fn start_check_calibration(app: &mut MyApp) {
    app.start_thread("check_calibration", Box::new(check_calibration));
}

/// The calibration check, on its own thread: waits while the calibration runs (0.3 s steps), then reports
/// its result.
fn check_calibration(app: Weak<Mutex<MyApp>>) {
    let homing = || {
        let Some(shared) = app.upgrade() else { return Ok(None) };
        let backend = crate::host::lock(&shared).backend.clone();
        backend.control_value(&["tests", "belt_homing"]).map(Some).map_err(|error| error.detail)
    };
    let outcome = match calibration_outcome(homing, std::thread::sleep) {
        Ok(Some(outcome)) => outcome,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(%error, "belt calibration monitor failed");
            return;
        }
    };
    let Some(shared) = app.upgrade() else { return };
    let mut app = crate::host::lock(&shared);
    report(&mut app, outcome);
}

/// The calibration's result as a notice.
pub fn report(app: &mut MyApp, finished: bool) {
    if finished {
        notices::alert(app, "Success", "Calibration has finished", "green");
    } else {
        notices::alert(app, "Failed", "Calibration did not succeed", "red");
    }
}

/// Waits while the calibration runs (`belt_homing` 2 or 3), then whether it ended in success (1).
pub fn calibration_outcome(
    mut read: impl FnMut() -> Result<Option<LegacyValue>, String>,
    mut sleep: impl FnMut(Duration),
) -> Result<Option<bool>, String> {
    loop {
        let Some(current) = read()? else { return Ok(None) };
        if !native_equal(&current, &LegacyValue::Int(2)) && !native_equal(&current, &LegacyValue::Int(3)) {
            break;
        }
        sleep(Duration::from_millis(300));
    }
    Ok(read()?.map(|value| native_equal(&value, &LegacyValue::Int(1))))
}

/// The Belt hot keys: each `belt_<row>_up` / `_down` moves its row one step.
pub fn apply_hotkeys(app: &mut MyApp, pending: &dyn Fn(&str) -> bool) -> Result<bool, String> {
    let steps: Vec<(String, String)> = HOTKEYS.iter().map(|(a, r)| ((*a).to_owned(), (*r).to_owned())).collect();
    tuning::step_hotkeys(app, pending, &page(), &steps, write)
}

/// A loaded profile: the belt is switched on while a game runs and off in stand-by.
pub fn on_profile_load(app: &mut MyApp) -> Result<(), String> {
    let game = app.backend.game_value().map_err(|error| error.detail)?;
    app.assign_belt_switch(!native_equal(&game, &LegacyValue::Str("none".into())))
}

#[cfg(test)]
#[path = "belt_tests.rs"]
mod tests;
