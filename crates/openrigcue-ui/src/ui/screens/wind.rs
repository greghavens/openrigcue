//! Tuning → Wind: the fans' power ranges (left/right and centre) with their actual power and shortcut buttons,
//! the default maximum speed in the chosen unit, wind curving (two fans only), the speed factor and, for
//! games other than racing games, the ejection seat; the stand-by lock (lifted while a fan output is Always
//! On); the header actions (shortcuts, hardware set-up); and the rule a loaded profile follows for one fan.

use openrigcue_model::native_value::LegacyValue;
use serde_json::json;

use super::super::components::{self, RowState};
use super::super::read;
use super::super::Screen;
use super::tuning;
use crate::app::MyApp;
use crate::spec::{self, Control, Page, Widget};

/// The page description as shown: the actual-power rows' shortcut buttons say what they set.
pub fn page() -> Page {
    let mut page = spec::wind_page();
    for section in &mut page.sections {
        for control in &mut section.controls {
            for button in &mut control.buttons {
                if let Widget::Button { text, .. } = button {
                    *text = match text.as_str() {
                        "-" => "Lower-power shortcut".into(),
                        "+" => "Raise-power shortcut".into(),
                        other => other.to_owned(),
                    };
                }
            }
        }
    }
    page
}

/// Whether the fans show their speeds in km/h (the `measurement_unit` of the App settings).
pub fn kmh(app: &MyApp) -> bool {
    app.backend.native_setting("APP", "measurement_unit").map_or(true, |unit| unit == LegacyValue::Str("K".into()))
}

/// Whether two fans are set up (wind curving needs them).
pub fn two_fans(app: &MyApp) -> bool {
    app.backend.native_setting("INTELLIBOX", "nbr_fans").is_ok_and(|fans| fans == LegacyValue::Str("2".into()))
}

fn locked(app: &MyApp) -> bool {
    tuning::locked(app, true, true)
}

/// The state of a row, `None` when the rules hide it.
fn row_state(app: &MyApp, control: &Control, locked: bool) -> Option<RowState> {
    let mut state = tuning::base_state(app, control, !locked);
    match control.id.as_str() {
        "fan_actual_perc" | "fan_center_actual_perc" => {
            let key = if control.id == "fan_actual_perc" {
                "power_wind_max_speed_perc"
            } else {
                "power_center_wind_max_speed_perc"
            };
            state.output = Some(format!("{}%", components::number(app.tune(&[key])).trunc()));
            state.enabled = true;
        }
        "default_max_speed" if !kmh(app) => {
            state.value = json!((components::number(&state.value) * 0.621_371).trunc());
        }
        "wind_curving" | "curving_effect" if !two_fans(app) => {
            state.value = if control.id == "wind_curving" { json!(0) } else { json!(1) };
            state.enabled = false;
            state.hint = Some(super::super::html::t(app, "Needs 2 fans (Hardware)"));
        }
        "ejection_seat" if app.ui.session.racing => return None,
        _ => {}
    }
    Some(state)
}

/// The state of the row `id` as this screen draws it, for the start screen's Wind tab; `None` when the rules
/// hide it or the page has no such row.
pub fn shown_state(app: &MyApp, id: &str) -> Option<RowState> {
    row_state(app, page().control(id)?, locked(app))
}

/// The body.
pub fn main(app: &MyApp) -> String {
    let locked = locked(app);
    let unit = if kmh(app) { "Km/h" } else { "Mph" };
    tuning::panel(
        app,
        Screen::Wind,
        &page(),
        locked,
        unit,
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

/// A value of the screen changed: the stored value the Wind page's handler wrote.
pub fn set(app: &mut MyApp, control: &str, part: &str, value: &str) -> Result<(), String> {
    let locked = locked(app);
    if locked || !tuning::accepts(&page(), control, part, |row| row_state(app, row, locked)) {
        return Ok(());
    }
    write(app, control, part, value)
}

/// The stored value a row's change writes (for the screen and the profile load alike).
pub fn write(app: &mut MyApp, control: &str, part: &str, value: &str) -> Result<(), String> {
    let int = |value: &str| read::int(value).map(LegacyValue::Int);
    match (control, part) {
        ("fan_power_perc" | "fan_power_center_perc", end) => {
            let prefix = if control == "fan_power_perc" { "power_wind" } else { "power_center_wind" };
            let low_key = format!("{prefix}_min_speed_perc");
            let high_key = format!("{prefix}_max_speed_perc");
            let given = read::int(value)?;
            if end == "high" {
                let low = components::number(app.tune(&[&low_key])) as i64;
                app.set_tune_native_checked(&[&high_key], &LegacyValue::Int(given.max(low)))
            } else {
                let high = components::number(app.tune(&[&high_key])) as i64;
                app.set_tune_native_checked(&[&low_key], &LegacyValue::Int(given.min(high)))
            }
        }
        ("default_max_speed", "") => {
            let shown = read::int(value)?;
            let stored = if kmh(app) { shown } else { (shown as f64 * 1.60934).trunc() as i64 };
            app.set_tune_native_checked(&["default_max_speed_kmh"], &LegacyValue::Int(stored))
        }
        ("wind_curving", "") => {
            app.set_tune_native_checked(&["wind_curving"], &LegacyValue::Int(i64::from(read::checked(value))))
        }
        ("curving_effect" | "speed_factor", "") => app.set_tune_native_checked(&[control], &int(value)?),
        ("ejection_seat", "") => {
            app.set_tune_native_checked(&["enable_ejection_seat"], &LegacyValue::Int(i64::from(read::checked(value))))
        }
        _ => Err(format!("no Wind value {control:?}")),
    }
}

/// A button of the screen: the header actions and the rows' shortcut buttons.
pub fn action(app: &mut MyApp, control: &str, index: Option<usize>) -> Result<(), String> {
    let locked = locked(app);
    if control != "@header" && !tuning::accepts_button(&page(), control, index, |row| row_state(app, row, locked)) {
        return Ok(());
    }
    let run = tuning::button(&page(), control, index).ok_or_else(|| format!("no Wind button {control:?}"))?;
    if tuning::run_common(app, &run)? {
        Ok(())
    } else {
        Err(format!("unsupported Wind action {run:?}"))
    }
}

/// The default max speed as its slider shows it in a unit: the stored km/h, or them in mph (multiplied, then
/// truncated), then rounded to the slider's whole steps within 0..2000.
fn shown_speed(stored: f64, kmh: bool) -> f64 {
    let scale = crate::slider::SliderScale::new(0.0, 2000.0, 1.0);
    scale.snap(if kmh { stored } else { (stored * 0.621_371).trunc() })
}

/// The measurement unit changed (`was_kmh`: the unit before): the default max speed is shown in the new
/// unit, and when that moves its slider the value is stored again through the slider's conversion (the
/// whole number in km/h; in mph the whole number times 1.60934, truncated), so a km/h → mph change can lose
/// a little, as it always has.
pub fn units_changed(app: &mut MyApp, was_kmh: bool) -> Result<(), String> {
    let now_kmh = kmh(app);
    let tune = app.backend.native_tune().map_err(|error| error.detail)?;
    let stored = crate::backend::native_field_owned(&tune, &["default_max_speed_kmh"])?;
    // A number; in km/h, where it is shown without a conversion, also a number written as text.
    let text_number = match &stored {
        LegacyValue::Str(text) if now_kmh => text.trim().parse::<f64>().ok().filter(|v| v.is_finite()),
        _ => None,
    };
    let stored = stored
        .number()
        .or(text_number)
        .ok_or_else(|| format!("TypeError: unsupported operand type(s) for *: '{}' and 'float'", stored.type_name()))?;
    let (before, after) = (shown_speed(stored, was_kmh), shown_speed(stored, now_kmh));
    if before == after {
        return Ok(());
    }
    let written = if now_kmh { after.trunc() } else { (after * 1.60934).trunc() };
    app.set_tune_native_checked(&["default_max_speed_kmh"], &LegacyValue::Int(written as i64))
}

#[cfg(test)]
#[path = "wind_tests.rs"]
mod tests;
