//! Tuning → Shake: the engine type and every shaker effect (Common, Racing, Flight) with its switch, filter,
//! shortcut and test buttons; the running game's feature rule; the stand-by lock; the header actions
//! (shortcuts, hardware set-up, downloaded profiles); the effect's filter dialog; the Shake hot keys; and the
//! default engine type of a loaded profile.

use openrigcue_config::catalog::registry::Registry;
use openrigcue_config::synced_profiles::Category;
use openrigcue_model::native_value::LegacyValue;
use serde_json::Value;

use super::super::components::{self, RowState};
use super::super::html::{esc, num, t, tf, tr};
use super::super::notices::{self, Severity};
use super::super::read;
use super::super::Screen;
use super::tuning::{self, Dialog};
use crate::app::MyApp;
use crate::spec::{self, Action, Control, Page, Widget};

/// The page description.
pub fn page() -> Page {
    spec::shake_page(Registry::builtin())
}

/// The game feature column of an effect.
pub fn feature_key(effect: &str) -> &str {
    match effect {
        "acceleration" | "deceleration" | "impact" => "surge",
        "lateral_acceleration" => "sway",
        "road_bump" => "heave",
        other => other,
    }
}

fn is_effect(id: &str) -> bool {
    Registry::builtin().shaker_effects.iter().any(|e| e.id == id)
}

fn locked(app: &MyApp) -> bool {
    tuning::locked(app, true, false)
}

/// Whether the effects' Test buttons are offered (`enable_test_effect` of the shaker settings).
fn tests_offered(app: &MyApp) -> bool {
    app.backend
        .native_setting("SHAKEBOX", "enable_test_effect")
        .ok()
        .and_then(|value| crate::backend::native_bool(&value).ok())
        .unwrap_or(false)
}

/// The state of a row, `None` when the rules hide it.
fn row_state(app: &MyApp, control: &Control, locked: bool, tests: bool) -> Option<RowState> {
    let mut state = tuning::base_state(app, control, !locked);
    if is_effect(&control.id) {
        let (visible, enabled) = tuning::feature(app, feature_key(&control.id));
        if !visible {
            return None;
        }
        if !enabled {
            state.enabled = false;
            state.row_enabled = false;
            state.hint = Some(tuning::unused_hint(app));
        }
        if !tests {
            let kept = control
                .buttons
                .iter()
                .enumerate()
                .filter(|(_, b)| !matches!(b, Widget::Button { run: Action::TestShakerEffect { .. }, .. }))
                .map(|(i, _)| i)
                .collect();
            state.buttons = Some(kept);
        }
    }
    Some(state)
}

/// The body.
pub fn main(app: &MyApp) -> String {
    let locked = locked(app);
    let tests = tests_offered(app);
    tuning::panel(
        app,
        Screen::Shake,
        &page(),
        locked,
        "",
        |control| row_state(app, control, locked, tests),
        |_| Some((true, None)),
        &[tuning::library_extra()],
        "",
    )
}

/// The values Compare looks at.
pub fn compared(app: &MyApp) -> Vec<(String, Vec<String>)> {
    tuning::compared(app, &page())
}

/// A value of the screen changed: the stored value the Shake page's handler wrote.
pub fn set(app: &mut MyApp, control: &str, part: &str, value: &str) -> Result<(), String> {
    if !accepts(app, control, part) {
        return Ok(());
    }
    match (control, part) {
        ("engine_type", "") => {
            let kind = match value {
                "0" => "O",
                "1" => "E",
                other => return Err(format!("KeyError: {other:?}")),
            };
            app.set_tune_native_checked(&["shaker_engine_type"], &LegacyValue::Str(kind.into()))
        }
        (effect, "toggle") if is_effect(effect) => {
            app.set_shaker_value_checked("shaker_switch", effect, &LegacyValue::Bool(read::checked(value)))
        }
        (effect, "") if is_effect(effect) => {
            app.set_shaker_value_checked("shaker_effects", effect, &read::number(value)?)
        }
        _ => Err(format!("no Shake value {control:?}")),
    }
}

/// Whether the row `control` takes a change of `part` now (not in stand-by, nor while the game lacks the
/// effect).
fn accepts(app: &MyApp, control: &str, part: &str) -> bool {
    let locked = locked(app);
    let tests = tests_offered(app);
    tuning::accepts(&page(), control, part, |row| row_state(app, row, locked, tests))
}

/// The state of the row `id` as this screen draws it, for the start screen's Shake tab; `None` when the rules
/// hide it or the page has no such row.
pub fn shown_state(app: &MyApp, id: &str) -> Option<RowState> {
    row_state(app, page().control(id)?, locked(app), tests_offered(app))
}

/// Whether the row `control` takes a click of its button `index` now.
fn accepts_button(app: &MyApp, control: &str, index: Option<usize>) -> bool {
    let locked = locked(app);
    let tests = tests_offered(app);
    tuning::accepts_button(&page(), control, index, |row| row_state(app, row, locked, tests))
}

/// A button of the screen.
pub fn action(app: &mut MyApp, control: &str, index: Option<usize>) -> Result<(), String> {
    if control == "@library" {
        tuning::open_library(app, Category::Shakers);
        return Ok(());
    }
    if control != "@header" && !accepts_button(app, control, index) {
        return Ok(());
    }
    let run = tuning::button(&page(), control, index).ok_or_else(|| format!("no Shake button {control:?}"))?;
    if tuning::run_common(app, &run)? {
        return Ok(());
    }
    match run {
        Action::ShakerFilter { effect } => open_filter(app, &effect),
        Action::TestShakerEffect { effect } => test(app, &effect),
        other => Err(format!("unsupported Shake action {other:?}")),
    }
}

/// An effect's test: its test sound plays on the shakers' sound device, with one selected.
pub fn test(app: &mut MyApp, effect: &str) -> Result<(), String> {
    let soundcard = app.backend.native_setting("SHAKEBOX", "soundcard").map_err(|error| error.detail)?;
    if soundcard == LegacyValue::Str("None".into()) {
        let mut notice = notices::notice(
            Severity::Warning,
            &tr(app, "Test"),
            &tr(app, "Choose the shakers' sound device on the Hardware screen first."),
        );
        notice.actions = vec![("Hardware setup".into(), notices::Action::Open { screen: Screen::Hardware })];
        notices::raise(app, notice);
        return Ok(());
    }
    let current = app.backend.session_value("listener_running").map_err(|error| error.detail)?;
    let mut listener = current.clone();
    if listener == LegacyValue::Str("none".into()) {
        listener = "test".into();
    }
    app.backend.test_shaker_effect(effect, &current, &listener).map_err(|error| error.detail)?;
    let label = effect_label(app, effect);
    let message = tf(app, "Playing the test of ^effect^.", &[("effect", &label)]);
    notices::confirmation(app, "Test", &message);
    Ok(())
}

fn effect_label(app: &MyApp, effect: &str) -> String {
    Registry::builtin()
        .shaker_effects
        .iter()
        .find(|e| e.id == effect)
        .map_or_else(|| effect.to_owned(), |e| tr(app, &e.label))
}

/// The Shake hot keys: `shake_box_<effect>_up` / `_down` move the effect one step.
pub fn apply_hotkeys(app: &mut MyApp, pending: &dyn Fn(&str) -> bool) -> Result<bool, String> {
    let steps: Vec<(String, String)> = Registry::builtin()
        .shaker_effects
        .iter()
        .map(|effect| (format!("shake_box_{}", effect.id), effect.id.clone()))
        .collect();
    tuning::step_hotkeys(app, pending, &page(), &steps, write_step)
}

/// A hot-key step: written even in stand-by-locked screens (the keys only work while a game runs).
fn write_step(app: &mut MyApp, control: &str, _part: &str, value: &str) -> Result<(), String> {
    app.set_shaker_value_checked("shaker_effects", control, &read::number(value)?)
}

// ---- Filter dialog ----------------------------------------------------------------------------------------

/// An effect's filter dialog: the effect and its five values when it opened (Cancel restores them).
#[derive(Debug, Clone, PartialEq)]
pub struct Filter {
    pub effect: String,
    /// The cutoff, resonance and filter switch of the effect, then the engine fade-off start and end.
    pub saved: Vec<Value>,
}

/// The five stored values a filter dialog edits, in order.
fn filter_paths(effect: &str) -> [[String; 2]; 5] {
    let p = |a: &str, b: &str| [a.to_owned(), b.to_owned()];
    [
        p("shaker_cutoff", effect),
        p("shaker_resonance", effect),
        p("shaker_dsp_type", effect),
        p("shaker_cutoff", "engine_fadeoff_start"),
        p("shaker_cutoff", "engine_fadeoff_end"),
    ]
}

/// Opens an effect's filter dialog; its values change as they are moved. Opening it first stores the engine
/// fade-off range as its control holds it ([`tidy_fadeoff`]); the filter switch, the cutoff and the resonance are
/// only shown, not stored. Cancel restores the values from before.
pub fn open_filter(app: &mut MyApp, effect: &str) -> Result<(), String> {
    if !accepts(app, effect, "button") {
        return Ok(());
    }
    let saved = filter_paths(effect).iter().map(|[a, b]| app.tune(&[a, b]).clone()).collect();
    tidy_fadeoff(app)?;
    tuning::open_dialog(app, Dialog::Filter(Filter { effect: effect.to_owned(), saved }));
    Ok(())
}

/// The engine fade-off range (whichever effect the dialog is for) put into its control: both ends rounded to
/// whole numbers, ordered and kept within 0…100, then stored as whole numbers (a value equal in number to what is
/// stored is not written again). When either end is not a number the range is left as it is and the dialog
/// still opens.
fn tidy_fadeoff(app: &mut MyApp) -> Result<(), String> {
    use super::reload::{put, stored};
    let end = |app: &MyApp, key: &str| stored(app, &["shaker_cutoff", key]).ok().and_then(|value| value.number());
    let (Some(low), Some(high)) = (end(app, "engine_fadeoff_start"), end(app, "engine_fadeoff_end")) else {
        return Ok(());
    };
    let (low, high) = crate::slider::RangeScale::new(0.0, 100.0, 1.0).snap(low, high);
    put(app, &["shaker_cutoff", "engine_fadeoff_start"], LegacyValue::Int(low as i64))?;
    put(app, &["shaker_cutoff", "engine_fadeoff_end"], LegacyValue::Int(high as i64))
}

/// A slider row of a dialog: its label and value, the slider (sending `dialog-field` with `arg`) and its ends.
fn dialog_slider(label: &str, id: &str, arg: &str, (min, max): (f64, f64), value: f64, enabled: bool) -> String {
    format!(
        "<div class=\"control-row{}\"><div class=\"control-label\"><label for=\"{id}\"><strong>{label}</strong></label><output id=\"{id}-out\" for=\"{id}\">{value}</output></div><input id=\"{id}\" type=\"range\" min=\"{min}\" max=\"{max}\" step=\"1\" value=\"{value}\" data-act=\"dialog-field\" data-arg=\"{arg}\" data-output=\"{id}-out\" data-template=\"{{}}\"{}><div class=\"range-endpoints\"><span>{min}</span><span>{max}</span></div></div>",
        if enabled { "" } else { " is-disabled" },
        if enabled { "" } else { " disabled" },
        min = num(min),
        max = num(max),
        value = num(value),
    )
}

/// The filter dialog.
pub fn filter_dialog(app: &MyApp, filter: &Filter) -> String {
    let effect = filter.effect.as_str();
    let value = |table: &str, key: &str| components::number(app.tune(&[table, key])).trunc();
    let on = components::number(app.tune(&["shaker_dsp_type", effect])) != 0.0;
    let switch = components::field(
        &t(app, "Enabled"),
        &format!(
            "<label class=\"switch\"><input id=\"filter-enabled\" type=\"checkbox\" data-act=\"dialog-field\" data-arg=\"enabled\"{}><span>{}</span></label>",
            if on { " checked" } else { "" },
            t(app, if on { "On" } else { "Off" })
        ),
        "",
    );
    let rows = if effect == "engine" {
        let (low, high) =
            (value("shaker_cutoff", "engine_fadeoff_start"), value("shaker_cutoff", "engine_fadeoff_end"));
        let fade = format!(
            "<div class=\"control-row{}\"><div class=\"control-label\"><label for=\"filter-fade\"><strong>{}</strong></label><output id=\"filter-fade-out\">{} - {}</output></div><small>{}</small><div class=\"range-pair\">{}{}</div></div>",
            if on { "" } else { " is-disabled" },
            t(app, "Fade Off"),
            num(low),
            num(high),
            t(app, "Cutoff"),
            pair_input(app, "filter-fade", "", "fade_start", low, on),
            pair_input(app, "filter-fade", "-high", "fade_end", high, on),
        );
        dialog_slider(
            &t(app, "Speed"),
            "filter-resonance",
            "resonance",
            (1.0, 100.0),
            value("shaker_resonance", effect),
            on,
        ) + &fade
    } else {
        dialog_slider(&t(app, "Filter"), "filter-cutoff", "cutoff", (30.0, 500.0), value("shaker_cutoff", effect), on)
            + &dialog_slider(
                &t(app, "Strength"),
                "filter-resonance",
                "resonance",
                (1.0, 100.0),
                value("shaker_resonance", effect),
                on,
            )
    };
    let title = format!("{} · {}", t(app, "Filter"), esc(&effect_label(app, effect)));
    super::super::dialogs::frame(app, &title, &format!("{switch}{rows}"), Some((&t(app, "Confirm"), true)))
}

/// One end of the fade-off range of the filter dialog.
fn pair_input(app: &MyApp, id: &str, part: &str, arg: &str, value: f64, enabled: bool) -> String {
    format!(
        "<input id=\"{id}{part}\" type=\"range\" min=\"0\" max=\"100\" step=\"1\" value=\"{}\" data-act=\"dialog-field\" data-arg=\"{arg}\" data-output=\"{id}-out\" data-template=\"{{lo}} - {{hi}}\" data-pair=\"{id}\" data-bound=\"{}\" aria-label=\"{} · {}\"{}>",
        num(value),
        if part.is_empty() { "low" } else { "high" },
        t(app, "Fade Off"),
        t(app, if part.is_empty() { "Minimum" } else { "Maximum" }),
        if enabled { "" } else { " disabled" }
    )
}

/// A value of the filter dialog changed: it is stored at once.
pub fn filter_field(app: &mut MyApp, filter: Filter, arg: &str, value: &str) -> Result<(), String> {
    let effect = filter.effect.as_str();
    match arg {
        "enabled" => app.set_tune_native_checked(
            &["shaker_dsp_type", effect],
            &LegacyValue::Int(i64::from(read::checked(value))),
        )?,
        "cutoff" => app.set_tune_native_checked(&["shaker_cutoff", effect], &read::number(value)?)?,
        "resonance" => app.set_tune_native_checked(&["shaker_resonance", effect], &read::number(value)?)?,
        "fade_start" => {
            let end = components::number(app.tune(&["shaker_cutoff", "engine_fadeoff_end"])) as i64;
            let start = read::int(value)?.min(end);
            app.set_tune_native_checked(&["shaker_cutoff", "engine_fadeoff_start"], &LegacyValue::Int(start))?
        }
        "fade_end" => {
            let start = components::number(app.tune(&["shaker_cutoff", "engine_fadeoff_start"])) as i64;
            let end = read::int(value)?.max(start);
            app.set_tune_native_checked(&["shaker_cutoff", "engine_fadeoff_end"], &LegacyValue::Int(end))?
        }
        other => return Err(format!("no filter value {other:?}")),
    }
    tuning::replace_dialog(app, Dialog::Filter(filter));
    Ok(())
}

/// The filter dialog was cancelled: its five values are restored.
pub fn filter_cancel(app: &mut MyApp, filter: &Filter) -> Result<(), String> {
    for ([table, key], value) in filter_paths(&filter.effect).iter().zip(&filter.saved) {
        if read::same(value, app.tune(&[table, key])) {
            continue;
        }
        app.set_tune_native_checked(&[table, key], &LegacyValue::from(value.clone()))?;
    }
    super::super::refresh(app, &[super::super::Region::Main, super::super::Region::Save]);
    Ok(())
}

#[cfg(test)]
#[path = "shake_tests.rs"]
mod tests;
