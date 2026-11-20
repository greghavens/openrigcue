//! The start screen's tuning panel: Motion, Shake and Wind tabs with the four main cues of each, and the tool
//! that opens the tab's complete screen. The session instruments, rig status and compare note are the
//! theme's (see [`crate::ui::themes`]).

use openrigcue_config::catalog::registry::Registry;

use super::super::components::{self, RowState};
use super::super::html::{icon, t};
use super::super::read;
use super::super::themes;
use crate::app::MyApp;
use crate::slider::SliderScale;
use crate::spec::{self, Control, Field, Write};

/// The tabs.
pub const TABS: [&str; 3] = ["Motion", "Shake", "Wind"];

/// The shaker effects of the Shake tab.
const SHAKE_EFFECTS: [(&str, &str); 4] = [
    ("engine", "Feel engine vibration through the seat."),
    ("gear", "A distinct cue with each gear change."),
    ("road_bump", "Texture from the track surface."),
    ("traction_loss", "Feedback as the tires lose traction."),
];

/// The controls of a tab, with their hints.
fn controls(app: &MyApp, tab: usize) -> Vec<(Control, &'static str)> {
    let percent = SliderScale::new(0.0, 100.0, 1.0);
    match tab {
        0 => {
            let page = spec::motion_page(Registry::builtin(), super::motion::smoothing_step(app));
            [
                ("motion_smoothing_factor", "Soften sudden transitions between motion cues."),
                ("roll", "Lateral tilt and banking response."),
                ("pitch", "Fore-aft tilt and attitude response."),
                ("heave", "Vertical motion and surface or turbulence cues."),
            ]
            .into_iter()
            .filter_map(|(id, hint)| page.control(id).cloned().map(|c| (c, hint)))
            .collect()
        }
        1 => {
            let registry = Registry::builtin();
            SHAKE_EFFECTS
                .into_iter()
                .filter_map(|(id, hint)| {
                    let effect = registry.shaker_effects.iter().find(|e| e.id == id)?;
                    Some((
                        Control::slider(
                            id,
                            &effect.label,
                            SliderScale::new(0.0, 20.0, 1.0),
                            Field::tune(&["shaker_effects", id]),
                            Write::Number,
                        ),
                        hint,
                    ))
                })
                .collect()
        }
        _ => vec![
            (
                Control::slider(
                    "wind_min",
                    "Minimum power",
                    percent,
                    Field::tune(&["power_wind_min_speed_perc"]),
                    Write::Number,
                )
                .showing("{}%"),
                "Fan output at the bottom of the power range.",
            ),
            (
                Control::slider(
                    "wind_max",
                    "Maximum power",
                    percent,
                    Field::tune(&["power_wind_max_speed_perc"]),
                    Write::Number,
                )
                .showing("{}%"),
                "Limit the upper end of the fan output.",
            ),
            (
                Control::slider(
                    "default_max_speed",
                    "Max speed",
                    SliderScale::new(0.0, 2000.0, 1.0),
                    Field::tune(&["default_max_speed_kmh"]),
                    Write::SpeedFromUnit,
                )
                .showing("{}{unit}"),
                "Speed at which the fans reach maximum output.",
            ),
            (
                Control::slider(
                    "curving_effect",
                    "Curving Effect",
                    SliderScale::new(1.0, 10.0, 1.0),
                    Field::tune(&["curving_effect"]),
                    Write::Int,
                ),
                "Shape the response through a corner.",
            ),
        ],
    }
}

/// The Wind screen's row a control of the Wind tab stands for (the two power sliders are the ends of its fan
/// power range).
fn wind_row(control: &str) -> &str {
    match control {
        "wind_min" | "wind_max" => "fan_power_perc",
        other => other,
    }
}

/// Whether the tab is locked: the stand-by lock of its full screen (Wind stays open while a fan output is
/// Always On).
fn locked(app: &MyApp, tab: usize) -> bool {
    super::tuning::locked(app, true, tab == 2)
}

/// A row's state as the tab's full screen draws it (its stand-by lock, the running game's feature rule, the
/// per-control switch and its own rules); `None` when the full screen hides it. The tab shows the main value
/// only, with its own hint unless the full screen gives one.
fn row_state(app: &MyApp, tab: usize, control: &Control, hint: &str) -> Option<RowState> {
    let mut state = match tab {
        0 => super::motion::shown_state(app, &control.id)?,
        1 => super::shake::shown_state(app, &control.id)?,
        _ => {
            let mut state = super::wind::shown_state(app, wind_row(&control.id))?;
            if wind_row(&control.id) != control.id {
                state.value = control.field.as_ref().map(|f| read::field(app, f)).unwrap_or_default();
                state.high = serde_json::Value::Null;
            }
            state
        }
    };
    state.toggle = None;
    state.max = None;
    state.buttons = Some(Vec::new());
    state.hint = state.hint.or_else(|| Some(t(app, hint)));
    Some(state)
}

/// The tuning panel.
pub fn main(app: &MyApp) -> String {
    let tab = app.ui.tab.min(2);
    let locked = locked(app, tab);
    let unit = if super::wind::kmh(app) { "Km/h" } else { "Mph" };
    let rows: String = controls(app, tab)
        .iter()
        .filter_map(|(control, hint)| {
            let state = row_state(app, tab, control, hint)?;
            Some(components::spec_row(app, "Tuning", control, &state, unit))
        })
        .collect();
    let note = if locked {
        super::note(&super::super::html::tr(app, "Tuning is unavailable in Stand By mode; start a game to tune."), "")
    } else {
        String::new()
    };
    let (title, subtitle) = themes::tuning_head(app);
    format!(
        "<section class=\"panel tuning-panel\"><div class=\"panel-head\"><div><h2>{title}</h2><small>{subtitle}</small></div><button type=\"button\" class=\"quiet panel-tool\" data-act=\"open-tuning\" aria-label=\"{open}\" title=\"{open}\">{}</button></div>{}<div id=\"controls\" role=\"tabpanel\" aria-labelledby=\"tab-{tab}\">{note}{rows}</div></section>",
        icon("tune"),
        components::tabs(app, "Tuning category", &TABS, tab, "tab"),
        open = t(app, "Open all tuning controls"),
    )
}

/// The controls Compare looks at: the open tab's.
pub fn compared(app: &MyApp) -> Vec<(String, Vec<String>)> {
    controls(app, app.ui.tab.min(2))
        .into_iter()
        .filter_map(|(control, _)| control.field.map(|field| (super::super::html::tr(app, &control.label), field.path)))
        .collect()
}

/// A value of the tuning panel changed: the full screen's change of the row, with its guards (a row the full
/// screen hides or disables takes nothing).
pub fn set(app: &mut MyApp, control: &str, part: &str, value: &str) -> Result<(), String> {
    match (app.ui.tab.min(2), control) {
        (0, _) => super::motion::set(app, control, part, value),
        (1, _) => super::shake::set(app, control, part, value),
        (_, "wind_min") => super::wind::set(app, "fan_power_perc", "", value),
        (_, "wind_max") => super::wind::set(app, "fan_power_perc", "high", value),
        (_, "default_max_speed" | "curving_effect") => super::wind::set(app, control, part, value),
        _ => Err(format!("no tuning value {control:?}")),
    }
}

#[cfg(test)]
#[path = "home_tests.rs"]
mod tests;
