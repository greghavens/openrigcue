//! The screens: the page heading with its page selector, each screen's body, and the routing of their value
//! changes and buttons. See [`crate::ui`] for adding a screen.

pub mod app;
pub mod belt;
pub mod compensation;
pub mod debug;
pub mod games;
pub mod hapticr;
pub mod hardware;
pub mod home;
pub mod iflag;
pub mod joystick;
pub mod led;
pub mod library;
pub mod motion;
pub mod reload;
pub mod routines;
pub mod settings;
pub mod shake;
pub mod shortcuts;
pub mod sim3d;
pub mod tach;
pub mod telemetry;
pub mod tuning;
pub mod wind;

use openrigcue_config::catalog::registry::Registry;
use openrigcue_model::native_value::LegacyValue;

use super::components::{self, RowState};
use super::html::{esc, t, tr};
use super::{read, themes, Screen};
use crate::app::MyApp;
use crate::spec::{self, Page};

/// The typed page description a screen is built from (also what the search looks through).
pub fn page(app: &MyApp, screen: Screen) -> Option<Page> {
    let registry = Registry::builtin();
    Some(match screen {
        Screen::Motion => spec::motion_page(registry, motion::smoothing_step(app)),
        Screen::Shake => spec::shake_page(registry),
        Screen::Wind => spec::wind_page(),
        Screen::Belt => spec::belt_page(),
        Screen::HapticR => spec::haptic_page(registry),
        Screen::Sim3d => spec::sim3d_page(registry),
        Screen::IFlag => spec::iflag_page(registry),
        Screen::Led => spec::led_page(),
        Screen::Routines => spec::routines_page(),
        Screen::Telemetry => spec::telemetry_page(),
        Screen::Hardware => spec::hardware_page(),
        Screen::Joystick => spec::joystick_page(),
        Screen::Compensation => spec::compensation_page(motion::six_dof(app)),
        Screen::App => self::app::page(),
        Screen::Debug => spec::debug_page(),
        _ => return None,
    })
}

/// The page heading: on the start screen the theme's own; elsewhere the section, the screen title, its line
/// and the page selector.
pub fn heading(app: &MyApp) -> String {
    let screen = app.ui.screen;
    if screen == Screen::Home {
        return themes::home_heading(app);
    }
    let group = screen.group();
    let options: Vec<(String, String)> =
        Screen::of_group(group, app).into_iter().map(|s| (s.key().to_owned(), tr(app, s.key()))).collect();
    format!(
        "<section class=\"page-heading\"><div><div class=\"eyebrow\">{}</div><h1>{}</h1><p>{}</p></div><div class=\"profile screen-picker\"><label class=\"eyebrow\" for=\"screen-picker\">{}</label>{}</div></section>",
        t(app, group.key()),
        themes::title(app, screen.key()),
        t(app, screen.subtitle()),
        t(app, group.key()),
        components::select("screen-picker", &tr(app, group.key()), &options, screen.key(), "screen", "")
    )
}

/// The settings group picker of a panel: the groups by index and title.
pub fn section_picker(app: &MyApp, sections: &[(usize, String)], selected: usize) -> String {
    if sections.len() < 2 {
        return String::new();
    }
    let options: Vec<(String, String)> =
        sections.iter().map(|(index, title)| (index.to_string(), title.clone())).collect();
    format!(
        "<div class=\"section-picker\">{}</div>",
        components::select(
            "section-picker",
            &tr(app, "Settings group"),
            &options,
            &selected.to_string(),
            "section",
            ""
        )
    )
}

/// The body of the open screen.
pub fn main(app: &MyApp) -> String {
    match app.ui.screen {
        Screen::Home => home::main(app),
        Screen::Motion => motion::main(app),
        Screen::Profiles => super::profiles::main(app),
        Screen::App => self::app::main(app),
        Screen::Shortcuts => shortcuts::main(app),
        Screen::Debug => debug::main(app),
        Screen::Help => settings::help_main(app),
        Screen::Pc | Screen::Console => games::main(app),
        Screen::Telemetry => telemetry::main(app),
        Screen::Hardware => hardware::main(app),
        Screen::Joystick => joystick::main(app),
        Screen::Compensation => compensation::main(app),
        Screen::Wind => wind::main(app),
        Screen::Shake => shake::main(app),
        Screen::Belt => belt::main(app),
        Screen::Sim3d => sim3d::main(app),
        Screen::HapticR => hapticr::main(app),
        Screen::IFlag => iflag::main(app),
        Screen::Led => led::main(app),
        Screen::Tach => tach::main(app),
        Screen::Routines => routines::main(app),
        Screen::DownloadedProfiles => library::main(app),
    }
}

/// Work when a screen opens.
pub fn on_enter(app: &mut MyApp, screen: Screen) {
    if screen == Screen::Profiles {
        super::profiles::load(app);
    }
    match screen {
        Screen::Pc | Screen::Console => games::on_enter(app, screen),
        Screen::Telemetry => telemetry::on_enter(app),
        Screen::Hardware => hardware::on_enter(app),
        Screen::Joystick => joystick::on_enter(app),
        Screen::Compensation => compensation::on_enter(app),
        Screen::DownloadedProfiles => library::on_enter(app),
        _ => {}
    }
}

/// A value of a screen's control changed (`part`: empty for the main value, or `toggle`, `max`, `high`). A
/// number box of the screen's page description takes the typed text first (see [`number_box`]).
pub fn set(app: &mut MyApp, screen: Screen, control: &str, part: &str, value: &str) -> Result<(), String> {
    match number_box(app, screen, control, part) {
        Some((number_box, shown)) => {
            let typed = LegacyValue::Str(value.to_owned());
            let whole = number_box.take_whole(&typed, &LegacyValue::Str(shown))?;
            route(app, screen, control, part, &whole.to_string())
        }
        None => route(app, screen, control, part, value),
    }
}

/// Puts a stored value into a number box of a screen's page description, as a change of the box: the box takes
/// it ([`read::NumberBox::take`]) and the row stores it, whether or not the row is shown or enabled (a Motion row
/// is written as its change writes it, see [`motion::write`]). A control that is no number box takes the
/// value's text.
pub fn set_number(app: &mut MyApp, screen: Screen, control: &str, value: &LegacyValue) -> Result<(), String> {
    let text = crate::backend::native_str(value)?;
    let text = match number_box(app, screen, control, "") {
        // The box shows the value put into it before it takes it.
        Some((number_box, _)) => number_box.take_whole(value, &LegacyValue::Str(text))?.to_string(),
        None => text,
    };
    match screen {
        Screen::Motion => motion::write(app, control, "", &text),
        _ => route(app, screen, control, "", &text),
    }
}

/// The number box a control's part is, with the text it shows: a number row (`part` empty) or a Max Telemetry box
/// (`part` `max`) of the screen's page description. Each of these rows stores a whole number, so what the box
/// takes is passed on as one (a fraction dropped toward zero).
pub fn number_box(app: &MyApp, screen: Screen, control: &str, part: &str) -> Option<(read::NumberBox, String)> {
    let page = page(app, screen)?;
    let control = page.control(control)?;
    let shown = |value: &serde_json::Value| {
        let value = components::number(value);
        super::html::num(if control.read == spec::Read::IntOfFloat { value.trunc() } else { value })
    };
    match (&control.widget, part, &control.max_telemetry) {
        (spec::Widget::Number { min, max, step }, "", _) => {
            let value = control.field.as_ref().map(|field| read::field(app, field)).unwrap_or_default();
            Some((read::NumberBox::new(*min, *max, *step), shown(&value)))
        }
        (_, "max", Some(limits)) => Some((
            read::NumberBox::new(limits.min, limits.max, limits.step),
            shown(&motion::max_value(app, &control.id)),
        )),
        _ => None,
    }
}

/// The screen's own handling of a value change.
fn route(app: &mut MyApp, screen: Screen, control: &str, part: &str, value: &str) -> Result<(), String> {
    match screen {
        Screen::Home => home::set(app, control, part, value),
        Screen::Motion => motion::set(app, control, part, value),
        Screen::App => self::app::set(app, control, part, value),
        Screen::Shortcuts => shortcuts::set(app, control, part, value),
        Screen::Pc | Screen::Console => games::set(app, screen, control, part, value),
        Screen::Telemetry => telemetry::set(app, control, part, value),
        Screen::Hardware => hardware::set(app, control, part, value),
        Screen::Joystick => joystick::set(app, control, part, value),
        Screen::Compensation => compensation::set(app, control, part, value),
        Screen::Wind => wind::set(app, control, part, value),
        Screen::Shake => shake::set(app, control, part, value),
        Screen::Belt => belt::set(app, control, part, value),
        Screen::Sim3d => sim3d::set(app, control, part, value),
        Screen::HapticR => hapticr::set(app, control, part, value),
        Screen::IFlag => iflag::set(app, control, part, value),
        Screen::Led => led::set(app, control, part, value),
        Screen::Tach => tach::set(app, control, part, value),
        Screen::DownloadedProfiles => library::set(app, control, part, value),
        _ => Err(format!("no value {control:?} on {}", screen.key())),
    }
}

/// A button of a screen: a control's row button `index`, or the control's own button (`None`).
pub fn action(app: &mut MyApp, screen: Screen, control: &str, index: Option<usize>) -> Result<(), String> {
    match screen {
        Screen::Motion => motion::action(app, control, index),
        Screen::App => self::app::action(app, control, index),
        Screen::Shortcuts => shortcuts::action(app, control, index),
        Screen::Debug => debug::action(app, control, index),
        Screen::Pc | Screen::Console => games::action(app, screen, control, index),
        Screen::Telemetry => telemetry::action(app, control, index),
        Screen::Hardware => hardware::action(app, control, index),
        Screen::Joystick => joystick::action(app, control, index),
        Screen::Compensation => compensation::action(app, control, index),
        Screen::Wind => wind::action(app, control, index),
        Screen::Shake => shake::action(app, control, index),
        Screen::Belt => belt::action(app, control, index),
        Screen::Sim3d => sim3d::action(app, control, index),
        Screen::HapticR => hapticr::action(app, control, index),
        Screen::IFlag => iflag::action(app, control, index),
        Screen::Led => led::action(app, control, index),
        Screen::Tach => tach::action(app, control, index),
        Screen::Routines => routines::action(app, control, index),
        Screen::DownloadedProfiles => library::action(app, control, index),
        _ => Err(format!("no button {control:?} on {}", screen.key())),
    }
}

/// The values of the compared scope that differ from the saved tune: `Label: saved → current`.
pub fn compare_rows(app: &MyApp) -> Vec<String> {
    let Some(snapshot) = app.ui.snapshot.as_ref() else { return Vec::new() };
    let controls: Vec<(String, Vec<String>)> = match app.ui.screen {
        Screen::Home => home::compared(app),
        Screen::Motion => motion::compared(app),
        Screen::Hardware => hardware::compared(app),
        Screen::Wind => wind::compared(app),
        Screen::Shake => shake::compared(app),
        Screen::Belt => belt::compared(app),
        Screen::Sim3d => sim3d::compared(app),
        Screen::HapticR => hapticr::compared(app),
        Screen::IFlag => iflag::compared(app),
        Screen::Led => led::compared(app),
        Screen::Tach => tach::compared(app),
        Screen::Routines => routines::compared(app),
        _ => Vec::new(),
    };
    controls
        .into_iter()
        .filter_map(|(label, path)| {
            let path: Vec<&str> = path.iter().map(String::as_str).collect();
            let saved = crate::backend::at(snapshot, &path);
            let current = app.tune(&path);
            (!super::read::same(saved, current))
                .then(|| format!("{label}: {} → {}", value_text(saved), value_text(current)))
        })
        .collect()
}

fn value_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "—".into(),
        serde_json::Value::Number(n) => super::html::num(n.as_f64().unwrap_or(0.0)),
        other => crate::backend::text(other),
    }
}

/// The rows of a page description's section, each with the state `state` gives (`None` hides the row).
pub fn rows(
    app: &MyApp,
    screen: Screen,
    controls: &[spec::Control],
    unit: &str,
    state: impl Fn(&spec::Control) -> Option<RowState>,
) -> Vec<String> {
    controls
        .iter()
        .filter_map(|control| state(control).map(|s| components::spec_row(app, screen.key(), control, &s, unit)))
        .collect()
}

/// A note at the top of a screen's panel.
pub fn note(text: &str, extra: &str) -> String {
    format!("<div class=\"screen-note\"><p>{}</p>{extra}</div>", esc(text))
}
