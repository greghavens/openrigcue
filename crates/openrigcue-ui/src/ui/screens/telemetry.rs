//! Games → Telemetry: the Capture and Export switches (Reset of the minimum and maximum under Options), the
//! live status, the channel to plot and its history chart, every telemetry channel with its current, minimum
//! and maximum value, and the page's actions (shortcuts, CSV snapshot).
//!
//! The main loop reads the live values (`GET telemetry/table`) and the plotted channel's history
//! (`GET telemetry/history`) each pass while capture runs with a game and the screen is open
//! ([`rules`]); after a reset while capture is off it clears the minimum and maximum and the reset flag.

use std::collections::BTreeMap;

use openrigcue_model::native_value::LegacyValue;
use serde_json::{json, Value};

use super::super::components::{self, RowState};
use super::super::html::{esc, num, t, tr};
use super::super::notices::{self, Severity};
use super::super::{read, themes, Screen};
use crate::app::MyApp;
use crate::backend::{num as json_num, text, Backend, BackendExt};
use crate::spec::{self, Action, Widget};

/// One channel's texts: value, minimum, maximum.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reading {
    pub value: String,
    pub min: String,
    pub max: String,
}

/// What the screen keeps between passes (`app.ui.devices`).
#[derive(Debug, Default)]
pub struct State {
    /// Every channel is listed, not only the displayed ones.
    pub all_channels: bool,
    /// Capture runs while a game runs.
    pub active: bool,
    /// The channels' latest readings, by channel name.
    pub readings: BTreeMap<String, Reading>,
    /// The plotted channel's history, while it has values.
    pub history: Option<Vec<f64>>,
}

/// The dialogs of this screen (its results are notices).
#[derive(Debug, Clone, PartialEq)]
pub enum Dialog {}

/// The channels the screen lists.
fn channels(app: &MyApp) -> Vec<crate::catalog::ChannelRow> {
    let all = app.ui.devices.telemetry.all_channels;
    crate::catalog::channels().into_iter().filter(|channel| all || channel.display).collect()
}

/// The state of the page description's rows.
fn row_state(app: &MyApp, control: &spec::Control) -> Option<RowState> {
    if control.id == "export_telemetry" && !app.win32() {
        return None;
    }
    let mut state = RowState::default();
    if let Some(field) = &control.field {
        state.value = read::field(app, field);
    }
    Some(state)
}

/// The history chart: the values as a line from the minimum to the maximum.
fn chart(app: &MyApp, history: &[f64]) -> String {
    let (low, high) = history.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(l, h), v| (l.min(*v), h.max(*v)));
    let span = if high > low { high - low } else { 1.0 };
    let step = if history.len() > 1 { 300.0 / (history.len() - 1) as f64 } else { 0.0 };
    let points: Vec<String> = history
        .iter()
        .enumerate()
        .map(|(i, v)| format!("{:.1},{:.1}", i as f64 * step, 96.0 - (v - low) / span * 92.0))
        .collect();
    format!(
        "<svg viewBox=\"0 0 300 100\" width=\"100%\" height=\"140\" preserveAspectRatio=\"none\" role=\"img\" aria-label=\"{}\"><path d=\"M0 98H300\" stroke=\"currentColor\" opacity=\".25\" fill=\"none\"/><polyline points=\"{}\" fill=\"none\" stroke=\"currentColor\" stroke-width=\"2\" vector-effect=\"non-scaling-stroke\"/></svg><small>{} {} · {} {}</small>",
        t(app, "History of the plotted channel"),
        points.join(" "),
        t(app, "Min"),
        esc(&num(low)),
        t(app, "Max"),
        esc(&num(high))
    )
}

/// The body.
pub fn main(app: &MyApp) -> String {
    let page = spec::telemetry_page();
    let state = &app.ui.devices.telemetry;
    let mut body = String::new();
    for section in &page.sections {
        body.push_str(&super::rows(app, Screen::Telemetry, &section.controls, "", |c| row_state(app, c)).concat());
    }
    body.push_str(&components::field(
        &t(app, "Status"),
        &format!(
            "<span class=\"pill\">{}</span>",
            t(app, if state.active { "Live telemetry" } else { "No live telemetry" })
        ),
        "",
    ));
    let shown = channels(app);
    let plotted = text(app.control(&["telemetry", "plot_channel"]));
    let mut options: Vec<(String, String)> = Vec::new();
    if !shown.iter().any(|c| c.name == plotted) {
        options.push((String::new(), "—".to_owned()));
    }
    options.extend(shown.iter().map(|c| (c.name.to_owned(), tr(app, c.label))));
    body.push_str(&components::field(
        &t(app, "Plot"),
        &components::select("telemetry-plot", &tr(app, "Plot"), &options, &plotted, "set", "Telemetry:plot"),
        &t(app, "The chart shows this channel while capture runs."),
    ));
    if let (true, Some(history)) = (state.active, &state.history) {
        body.push_str(&components::field(&t(app, "History"), &chart(app, history), ""));
    }
    let rows: Vec<String> = shown
        .iter()
        .map(|channel| {
            let reading = state.readings.get(channel.name).cloned().unwrap_or_default();
            let dash = |text: &str| if text.is_empty() { "—".to_owned() } else { text.to_owned() };
            format!(
                "<div class=\"control-row\" data-channel=\"{}\"><div class=\"control-label\"><strong>{}</strong><output class=\"mono\">{}</output></div><small>{} {} · {} {}</small></div>",
                esc(channel.name),
                t(app, channel.label),
                esc(&dash(&reading.value)),
                t(app, "Min"),
                esc(&dash(&reading.min)),
                t(app, "Max"),
                esc(&dash(&reading.max))
            )
        })
        .collect();
    body.push_str(&components::more(app, &rows, "telemetry:channels"));
    let header: String = page
        .header
        .iter()
        .enumerate()
        .filter_map(|(i, button)| match button {
            Widget::Button { text: label, run } => {
                if matches!(run, Action::TelemetrySnapshot) && !app.win32() {
                    return None;
                }
                let hint = page.header_hints.get(label).map(|h| format!(" title=\"{}\"", t(app, h))).unwrap_or_default();
                let disabled = matches!(run, Action::TelemetrySnapshot) && !state.active;
                Some(format!(
                    "<button type=\"button\" data-act=\"action\" data-arg=\"Telemetry:@header:{i}\"{hint}{}>{}</button>",
                    if disabled { " disabled" } else { "" },
                    t(app, label)
                ))
            }
            _ => None,
        })
        .collect();
    body.push_str(&components::actions(app, &header, "telemetry"));
    components::panel(&themes::title(app, "Telemetry"), "", &body, "")
}

/// Work when the screen opens: the readings of this pass are read at once.
pub fn on_enter(app: &mut MyApp) {
    if let Err(error) = read_live(app, true) {
        tracing::warn!(error, "telemetry readings");
    }
}

/// A value of the screen changed: Capture (the reset flag cleared, the chart emptied), Export, or the plotted
/// channel (its history starts again).
pub fn set(app: &mut MyApp, control: &str, part: &str, value: &str) -> Result<(), String> {
    match (control, part) {
        ("capture_telemetry", "") => {
            app.backend.set_telemetry_reset(false).map_err(|error| error.detail)?;
            app.set_control_checked(&["telemetry", "capture"], Value::Bool(read::checked(value)))?;
            app.ui.devices.telemetry.history = None;
            Ok(())
        }
        ("export_telemetry", "") => {
            app.set_control_checked(&["telemetry", "export"], Value::Bool(read::checked(value)))
        }
        ("plot", "") => {
            let channel = crate::catalog::channels()
                .into_iter()
                .find(|c| c.name == value)
                .ok_or_else(|| format!("KeyError: {value:?}"))?;
            app.backend.assign_telemetry_plot(channel.name).map_err(|error| error.detail)?;
            app.ui.devices.telemetry.history = None;
            app.backend.clear_telemetry_history().map_err(|error| error.detail)?;
            if let Ok(controls) = app.backend.get("controls") {
                app.docs.controls = controls;
            }
            Ok(())
        }
        _ => Err(format!("no Telemetry value {control:?}")),
    }
}

/// A button of the screen: Reset (under Capture's Options) or an action (Shortcuts, Save CSV Snapshot).
pub fn action(app: &mut MyApp, control: &str, index: Option<usize>) -> Result<(), String> {
    let page = spec::telemetry_page();
    let button = if control == "@header" {
        index.and_then(|i| page.header.get(i)).cloned()
    } else {
        page.control(control).and_then(|c| index.and_then(|i| c.buttons.get(i)).cloned())
    };
    let Some(Widget::Button { run, .. }) = button else { return Err(format!("no Telemetry button {control:?}")) };
    match run {
        Action::TelemetryReset => app.backend.reset_telemetry().map_err(|error| error.detail),
        Action::TelemetrySnapshot => snapshot(app),
        Action::Shortcuts { actions } => super::super::shortcuts::open(app, &actions),
        other => Err(format!("unsupported Telemetry action {other:?}")),
    }
}

/// Save CSV Snapshot: the service writes the current values as CSV and says where (or why not).
pub fn snapshot(app: &mut MyApp) -> Result<(), String> {
    let response = app.backend.post("telemetry/snapshot", json!({})).map_err(|error| error.detail)?;
    let parts = response.as_array().ok_or_else(|| {
        format!("TypeError: '{}' object is not subscriptable", LegacyValue::from(response.clone()).type_name())
    })?;
    let result = parts.first().ok_or("IndexError: tuple index out of range")?;
    let detail = parts.get(1).ok_or("IndexError: tuple index out of range")?;
    let title = tr(app, "Save CSV Snapshot");
    let notice = if result.as_str() == Some("OK") {
        let destination = detail.as_str().ok_or("TypeError: can only concatenate str to str")?;
        notices::notice(Severity::Success, &title, &format!("{}{destination}", tr(app, "Telemetry export written to ")))
    } else {
        notices::notice(Severity::Warning, &title, detail.as_str().unwrap_or_default())
    };
    notices::raise(app, notice);
    Ok(())
}

/// The dialog's markup (the screen has none).
pub fn dialog_render(app: &MyApp, dialog: &Dialog) -> String {
    let _ = app;
    match *dialog {}
}

/// An action of the open dialog (the screen has none).
pub fn dialog_act(app: &mut MyApp, dialog: Dialog, act: &str, arg: &str, value: &str) -> Result<(), String> {
    let _ = (app, act, arg, value);
    match dialog {}
}

/// Once, before the main loop's first pass: nothing.
pub fn start(app: &mut MyApp) -> Result<(), String> {
    let _ = app;
    Ok(())
}

/// Each main-loop pass: the live readings and history while the screen is open, and the reset of the minimum
/// and maximum while capture is off.
pub fn rules(app: &mut MyApp) -> Result<(), String> {
    let shown = app.ui.screen == Screen::Telemetry;
    read_live(app, shown)
}

/// What one pass reads from the service: whether capture is live, the channel list rule, the table and the
/// history while the screen is shown, and the reset flag while capture is off.
struct Live {
    active: bool,
    all_channels: Option<bool>,
    rows: Option<Vec<Value>>,
    reset: bool,
}

/// What a pass does after taking its reads.
enum Next {
    /// The readings were taken: the history is read and drawn next.
    History,
    /// Capture is off and a reset was asked for: the reset flag is cleared.
    ClearReset,
    Nothing,
}

/// Takes a history read: a history that cannot be read or summed keeps the chart drawn before.
fn apply_history(app: &mut MyApp, history: Result<Option<Vec<f64>>, ()>) {
    if let Ok(history) = history {
        app.ui.devices.telemetry.history = history;
    }
}

fn read_pass(backend: &dyn Backend, shown: bool) -> Result<Live, String> {
    let capture = backend.control_value(&["telemetry", "capture"]).map_err(|error| error.detail)?.is_truthy();
    // The running game is asked only while capture is on.
    let active = capture && backend.game_value().map_err(|error| error.detail)? != LegacyValue::Str("none".into());
    let all_channels = shown.then(|| backend.telemetry_all_channels());
    let mut live = Live { active, all_channels, rows: None, reset: false };
    if active {
        if shown {
            let rows = backend.get("telemetry/table").map_err(|error| error.to_string())?;
            live.rows = Some(rows.as_array().cloned().unwrap_or_default());
        }
    } else {
        live.reset = backend.control_value(&["telemetry", "reset_min_max"]).map_err(|error| error.detail)?.is_truthy();
    }
    Ok(live)
}

/// Takes a pass's reads (a row error ends the pass here, before the history is read).
fn apply_pass(app: &mut MyApp, live: Live) -> Result<Next, String> {
    app.ui.devices.telemetry.active = live.active;
    if let Some(all) = live.all_channels {
        app.ui.devices.telemetry.all_channels = all;
    }
    if live.active {
        return Ok(match live.rows {
            Some(rows) => {
                readings(app, &rows)?;
                Next::History
            }
            None => Next::Nothing,
        });
    }
    app.ui.devices.telemetry.history = None;
    if live.reset {
        for reading in app.ui.devices.telemetry.readings.values_mut() {
            reading.min.clear();
            reading.max.clear();
        }
        return Ok(Next::ClearReset);
    }
    Ok(Next::Nothing)
}

/// [`rules`] on the main loop's shared application: the service is read and the reset flag written without
/// holding the application.
pub(crate) fn rules_shared(shared: &std::sync::Mutex<MyApp>) -> Result<(), String> {
    let (shown, backend) = {
        let app = crate::host::lock(shared);
        (app.ui.screen == Screen::Telemetry, app.backend.clone())
    };
    let live = read_pass(&*backend, shown)?;
    let next = apply_pass(&mut crate::host::lock(shared), live)?;
    match next {
        Next::History => {
            let history = history(&*backend);
            apply_history(&mut crate::host::lock(shared), history);
        }
        Next::ClearReset => backend.set_telemetry_reset(false).map_err(|error| error.detail)?,
        Next::Nothing => {}
    }
    Ok(())
}

/// The text of a telemetry value: six decimals for a fraction, the plain text otherwise.
pub fn format_value(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Number(n) if n.is_f64() => format!("{:.6}", n.as_f64().unwrap_or(0.0)),
        Value::Number(n) => n.to_string(),
        _ => "0".to_owned(),
    }
}

/// Reads whether capture is live; while it is and `shown`, the channel readings and the history. After a reset
/// while capture is off, the minimum and maximum are cleared and so is the reset flag.
fn read_live(app: &mut MyApp, shown: bool) -> Result<(), String> {
    let backend = app.backend.clone();
    let live = read_pass(&*backend, shown)?;
    match apply_pass(app, live)? {
        Next::History => {
            let history = history(&*backend);
            apply_history(app, history);
        }
        Next::ClearReset => backend.set_telemetry_reset(false).map_err(|error| error.detail)?,
        Next::Nothing => {}
    }
    Ok(())
}

/// Each listed channel's reading from the service's table (by the channel's label; a channel without a row
/// reads 0). Roll and pitch show their direction; a value the service could not format is blank. A row the
/// service reports an error for ends the pass with that error.
fn readings(app: &mut MyApp, rows: &[Value]) -> Result<(), String> {
    let mut readings = BTreeMap::new();
    for channel in channels(app) {
        let found = rows.iter().find(|r| text(&r["label"]) == channel.label);
        if let Some(error) = found.and_then(|row| row.get("error")).and_then(Value::as_str) {
            app.ui.devices.telemetry.readings.extend(readings);
            return Err(error.to_owned());
        }
        let zero = Value::from(0);
        let (value, min, max) = found
            .map_or((zero.clone(), zero.clone(), zero), |r| (r["value"].clone(), r["min"].clone(), r["max"].clone()));
        let failed = found.and_then(|row| row.get("value_format_failed")).and_then(Value::as_bool).unwrap_or(false);
        let positive = if failed || !matches!(channel.name, "roll" | "pitch") {
            false
        } else {
            match found.and_then(|row| row.get("direction_positive")) {
                Some(Value::Bool(positive)) => *positive,
                Some(_) => return Err("invalid native attitude comparison".into()),
                None => json_num(&value) > 0.0,
            }
        };
        let direction = match (channel.name, positive) {
            ("roll", true) => " →",
            ("roll", false) => " ←",
            ("pitch", true) => " ↑",
            ("pitch", false) => " ↓",
            _ => "",
        };
        let value = if failed { String::new() } else { format!("{}{direction}", format_value(&value)) };
        readings.insert(channel.name.to_owned(), Reading { value, min: format_value(&min), max: format_value(&max) });
    }
    app.ui.devices.telemetry.readings = readings;
    Ok(())
}

/// The plotted channel's history, rounded to four decimals: `Ok(None)` (no chart) while it sums to zero or is
/// no list; `Err` when it cannot be read, summed or rounded, and the chart drawn before stays.
fn history(backend: &dyn Backend) -> Result<Option<Vec<f64>>, ()> {
    let history = backend.telemetry_history().map_err(|_| ())?;
    let (LegacyValue::List(items) | LegacyValue::Tuple(items)) = &history else { return Ok(None) };
    let sum = openrigcue_model::python_numeric::sum_values(items).map_err(|_| ())?;
    if sum.is_zero() {
        return Ok(None);
    }
    items
        .iter()
        .map(|value| value.number().map(|v| format!("{v:.4}").parse().unwrap_or(v)))
        .collect::<Option<_>>()
        .map(Some)
        .ok_or(())
}

#[cfg(test)]
#[path = "telemetry_tests.rs"]
mod tests;
