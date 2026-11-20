//! Tuning → Tach: the tachometer's shift-light brightness (part of the profile), its output test (the
//! tachometer Always On for seven seconds), and the ways to its output configuration (Hardware → Intellibox)
//! and to the shift lighting (Tuning → LED).

use super::super::components::{self, RowState};
use super::super::dialogs_hardware::Test;
use super::super::html::{t, tr};
use super::super::{read, themes, Screen};
use super::tuning;
use crate::app::MyApp;
use crate::spec::{self, Control};

/// The brightness row (the shift-light dim percentage of the profile), from the hardware page description.
fn brightness() -> Option<Control> {
    spec::hardware_page().control("shift_light_dim_perc").cloned()
}

/// The body. The brightness is not locked in stand-by (as on the Hardware screen).
pub fn main(app: &MyApp) -> String {
    let mut rows = String::new();
    if let Some(control) = brightness() {
        let state = RowState { buttons: Some(Vec::new()), ..tuning::base_state(app, &control, true) };
        rows.push_str(&components::spec_row(app, Screen::Tach.key(), &control, &state, ""));
    }
    // One device test runs at a time, this one or one of the Hardware screen's.
    let running = app.ui.devices.hardware.test.as_ref().map(|test| &test.target);
    let testing = running == Some(&Test::Tach);
    rows.push_str(&components::field(
        &t(app, "Output test"),
        &format!(
            "<button type=\"button\" id=\"c-tach-test\" data-act=\"action\" data-arg=\"Tach:test:\"{}>{}</button>",
            if testing {
                " disabled aria-busy=\"true\""
            } else if running.is_some() {
                " disabled"
            } else {
                ""
            },
            t(app, if testing { "Testing…" } else { "Test" })
        ),
        &t(app, "The tachometer lights Always On for 7 seconds."),
    ));
    rows.push_str(&components::field(
        &t(app, "Output configuration"),
        &components::button(&t(app, "Configure tachometer"), "action", "Tach:hardware:", ""),
        &t(app, "Tachometer output is configured with Intellibox hardware."),
    ));
    rows.push_str(&components::field(
        &t(app, "Shift lighting"),
        &components::button(&t(app, "Configure LEDs"), "action", "Tach:led:", ""),
        "",
    ));
    components::panel(&themes::title(app, "Tachometer"), "", &rows, "tuning-panel")
}

/// The values Compare looks at.
pub fn compared(app: &MyApp) -> Vec<(String, Vec<String>)> {
    brightness().map(|c| vec![(tr(app, &c.label), vec!["gear_shift_dim_perc".to_owned()])]).unwrap_or_default()
}

/// The brightness changed: the profile's shift-light dim percentage takes it.
pub fn set(app: &mut MyApp, control: &str, part: &str, value: &str) -> Result<(), String> {
    match (control, part) {
        ("shift_light_dim_perc", "") => app.set_tune_native_checked(&["gear_shift_dim_perc"], &read::number(value)?),
        _ => Err(format!("no Tach value {control:?}")),
    }
}

/// A button of the screen.
pub fn action(app: &mut MyApp, control: &str, _index: Option<usize>) -> Result<(), String> {
    match control {
        "test" => start_test(app)?,
        "hardware" => super::super::navigate(app, Screen::Hardware, 0),
        "led" => super::super::navigate(app, Screen::Led, 0),
        _ => return Err(format!("no Tach button {control:?}")),
    }
    Ok(())
}

/// The output test: the Hardware screen's tachometer test (the tachometer Always On for seven seconds, then
/// back to its mode); while any device test runs, another waits for it.
pub fn start_test(app: &mut MyApp) -> Result<(), String> {
    super::hardware::start_test(app, &Test::Tach)
}

#[cfg(test)]
#[path = "tach_tests.rs"]
mod tests;
