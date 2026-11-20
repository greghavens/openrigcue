//! Settings → App: every application setting, one settings group at a time: General (version, theme), Startup
//! Options (startup page, quit on close, start minimized, start at login, splash screen), Alerts /
//! Notifications (voice notifications, shift sound), Network (the address and the mobile companion), Logging
//! (level), Measurement (units), Reset (factory settings, with its confirmation) and Credits. The startup page
//! also opens at start ([`open_startup_page`]).

use serde_json::{json, Value};

use super::super::components::{self, RowState};
use super::super::dialogs::{self, Dialog};
use super::super::html::{t, tr};
use super::super::read;
use super::super::{notices, themes, Screen, Theme};
use crate::app::MyApp;
use crate::backend::{text, BackendExt};
use crate::spec::{self, Control, Field, Widget, Write};

/// The stored names of the measurement units, in the order of the Units choices (km/h, mph).
const UNITS: [&str; 2] = ["K", "M"];

/// The choices of a select of the page, by control id.
fn choices(id: &str) -> Vec<Value> {
    match page().control(id).map(|control| &control.widget) {
        Some(Widget::Select { options }) => options.iter().map(|choice| choice.value.clone()).collect(),
        _ => Vec::new(),
    }
}

/// The start-up switches that live with the launcher: control id (also the settings field), label and
/// launcher flag.
const LAUNCHER_ROWS: [(&str, &str, &str); 2] = [
    ("start_with_login", "Start at Login", "StartWithWindows"),
    ("show_splash_screen", "Show Splash Screen", "ShowSplashScreen"),
];

/// The App page as shown: its description, with the launcher's start-up switches after the Startup Options
/// rows of the description.
pub fn page() -> spec::Page {
    let mut page = spec::app_page();
    if let Some(startup) = page.sections.iter_mut().find(|section| section.title == "Startup Options") {
        for (id, label, _) in LAUNCHER_ROWS {
            startup.controls.push(Control::switch(id, label, Field::settings(&["app", id]), Write::Bool));
        }
    }
    page
}

/// The state of one row from the stored settings.
fn row_state(app: &MyApp, control: &Control) -> RowState {
    let mut state = RowState::default();
    if let Some(field) = &control.field {
        state.value = read::field(app, field);
    }
    match control.id.as_str() {
        "version" => state.output = Some(env!("CARGO_PKG_VERSION").to_owned()),
        "theme" => state.value = json!(app.ui.theme.id()),
        "ip" => state.output = Some(text(app.status(&["host_ip"]))),
        // A level that is none of the choices shows as the last one (Debug).
        "log_level" => {
            let levels = choices("log_level");
            if !levels.iter().any(|level| read::same(level, &state.value)) {
                state.value = levels.last().cloned().unwrap_or(Value::Null);
            }
        }
        // Any unit but km/h shows as mph.
        "measurement_unit" if state.value != json!("kmh") => state.value = json!("mph"),
        _ => {}
    }
    state
}

/// The body: the selected settings group's rows.
pub fn main(app: &MyApp) -> String {
    let page = page();
    let index = app.ui.section.min(page.sections.len().saturating_sub(1));
    let section = &page.sections[index];
    let groups: Vec<(usize, String)> = page.sections.iter().enumerate().map(|(i, s)| (i, tr(app, &s.title))).collect();
    let mut rows = super::rows(app, Screen::App, &section.controls, "", |control| Some(row_state(app, control)));
    if section.title == "Network" {
        rows.push(components::field(
            &t(app, "OpenRigCue Mobile Companion"),
            &components::button(&t(app, "Show QR code"), "companion", "", ""),
            &t(app, "Open OpenRigCue on your phone or tablet."),
        ));
    }
    // The first group is the application's own: its panel reads "Application", its group "General".
    let title = if index == 0 { "Application" } else { section.title.as_str() };
    components::panel(
        &themes::title(app, title),
        &super::section_picker(app, &groups, index),
        &components::more(app, &rows, &format!("app:{index}")),
        "",
    )
}

/// The position the browser sent for a choice.
fn choice(value: &str, count: usize) -> Result<usize, String> {
    let index: usize = value.parse().map_err(|_| format!("ValueError: {value:?}"))?;
    if index < count {
        Ok(index)
    } else {
        Err(format!("KeyError: {value:?}"))
    }
}

/// The value of the choice the browser sent for a select of the page.
fn chosen(id: &str, value: &str) -> Result<Value, String> {
    let options = choices(id);
    Ok(options[choice(value, options.len())?].clone())
}

/// Stores an APP setting by its stored name (`startup_page`, `x_button_exit`, `shift_sound`, `log_level`,
/// `measurement_unit`, ...); the settings the screen reads are the service's answer.
fn assign(app: &mut MyApp, key: &str, value: Value) -> Result<(), String> {
    app.docs.settings = app.backend.assign_app_setting(key, value).map_err(|error| error.detail)?;
    Ok(())
}

/// A value of Settings → App changed.
pub fn set(app: &mut MyApp, control: &str, _part: &str, value: &str) -> Result<(), String> {
    if let Some((_, _, flag)) = LAUNCHER_ROWS.iter().find(|(id, ..)| *id == control) {
        // The launcher keeps the switch where it can; a failed write leaves the stored value as it was.
        if let Err(error) = app.backend.write_launcher_flag(flag, read::checked(value)) {
            tracing::debug!(%error, flag, "launcher flag not written");
        }
        if let Ok(settings) = app.backend.get("settings") {
            app.docs.settings = settings;
        }
        return Ok(());
    }
    match control {
        "theme" => {
            let theme = Theme::ALL[choice(value, Theme::ALL.len())?];
            super::super::set_theme(app, theme.id())
        }
        "startup_page" | "log_level" => {
            let chosen = chosen(control, value)?;
            assign(app, control, chosen)
        }
        "x_button_exit" | "start_minimized" | "voice_notifications" | "shift_sound" => {
            assign(app, control, json!(read::checked(value)))
        }
        "measurement_unit" => {
            let unit = UNITS[choice(value, UNITS.len())?];
            let was_kmh = super::wind::kmh(app);
            assign(app, "measurement_unit", json!(unit))?;
            // The Wind max speed follows the unit (see `wind::units_changed`).
            super::wind::units_changed(app, was_kmh)
        }
        _ => Err(format!("no App value {control:?}")),
    }
}

/// A button of Settings → App: Restore Factory Settings asks first.
pub fn action(app: &mut MyApp, control: &str, _index: Option<usize>) -> Result<(), String> {
    match control {
        "reset" => {
            dialogs::open(app, Dialog::Settings(SettingsDialog::FactoryReset));
            Ok(())
        }
        _ => Err(format!("no App button {control:?}")),
    }
}

/// The dialogs of the Settings screens.
#[derive(Debug, Clone, PartialEq)]
pub enum SettingsDialog {
    /// Return to Factory Settings?
    FactoryReset,
}

/// The dialog's markup.
pub fn dialog(app: &MyApp, dialog: &SettingsDialog) -> String {
    match dialog {
        SettingsDialog::FactoryReset => dialogs::frame(
            app,
            &t(app, "Return to Factory Settings"),
            &format!(
                "<p class=\"dialog-text\">{}</p>",
                t(app, "OpenRigCue is going to be returned to its factory settings. This erases your tuning profiles and your hardware setup for good.")
            ),
            Some((&t(app, "Reset"), true)),
        ),
    }
}

/// The dialog's confirm button: the service returns to its factory settings and starts again.
pub fn dialog_ok(app: &mut MyApp, dialog: &SettingsDialog) -> Result<(), String> {
    match dialog {
        SettingsDialog::FactoryReset => {
            dialogs::close(app);
            if let Err(failure) = app.backend.post("factory-reset", json!({})) {
                let title = tr(app, "Return to Factory Settings");
                notices::raise(app, notices::notice(notices::Severity::Warning, &title, &failure.detail));
            }
            Ok(())
        }
    }
}

/// At start: the startup page chosen under Startup Options opens, Wind (the first choice and the default)
/// as well as any other. A stored name that is none of the choices opens the first choice; a page that is not
/// offered (HapticR while it is off) and a missing setting leave the start screen.
pub fn open_startup_page(app: &mut MyApp) {
    let stored = app.setting(&["app", "start_page"]);
    if stored.is_null() {
        return;
    }
    let stored = text(stored);
    let choices = choices("startup_page");
    let name = if choices.contains(&json!(stored)) { stored } else { choices.first().map(text).unwrap_or_default() };
    if let Some(screen) = Screen::from_key(&name).filter(|screen| screen.available(app)) {
        super::super::navigate(app, screen, 0);
    }
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod tests;
