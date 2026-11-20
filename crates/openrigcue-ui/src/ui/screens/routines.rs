//! Tuning → Routines: one row per routine (Fanalab, Simucube 2 True Drive Paddock, CrewChief) with its status
//! and its Options, which read the controller's profiles on a thread of their own and open the routine's
//! configuration (status and profile); the disclaimer; the stand-by lock; and the start of the enabled
//! routines when a profile loads. The controllers drive the other programs through the desktop
//! ([`desktop`]).

pub mod crewchief;
pub mod desktop;
pub mod fanatec;
pub mod simecube;

use std::sync::{Mutex, Weak};

use openrigcue_model::native_value::{Dict, LegacyValue};
use serde_json::json;

use self::desktop::{ActivateError, Desktop, SystemDesktop};
use super::super::components;
use super::super::html::{esc, t, tr};
use super::super::{read, themes, Region};
use super::tuning::{self, Dialog};
use crate::app::MyApp;
use crate::host::{lock, UiApp};

/// The controller of a routine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Controller {
    Fanatec,
    Simecube,
    Crewchief,
}

/// A controller's failure: one it reports with a message (`Rejected`), or one that ends the work
/// (`Raised`).
pub(super) enum RoutineFailure {
    Rejected(String),
    Raised(String),
}

impl RoutineFailure {
    fn message(self) -> String {
        match self {
            Self::Rejected(message) | Self::Raised(message) => message,
        }
    }
}

type RoutineResult<T> = Result<T, RoutineFailure>;

impl Controller {
    /// The controller's profiles, or a failure message.
    pub fn get_list_profiles(self, desktop: &dyn Desktop) -> Result<Vec<String>, String> {
        match self {
            Controller::Fanatec => fanatec::get_list_profiles(desktop),
            Controller::Simecube => simecube::get_list_profiles(desktop),
            Controller::Crewchief => crewchief::get_list_profiles(desktop),
        }
    }

    /// Activates a profile in the controller: success or failure, with a message.
    pub fn activate_profile(self, desktop: &dyn Desktop, profile: Option<&str>) -> Result<String, String> {
        match self {
            Controller::Fanatec => fanatec::activate_profile(desktop, profile),
            Controller::Simecube => simecube::activate_profile(desktop, profile),
            Controller::Crewchief => crewchief::activate_profile(desktop, profile),
        }
    }

    fn list_result(self, desktop: &dyn Desktop) -> RoutineResult<Vec<String>> {
        match self {
            Self::Fanatec => fanatec::get_list_profiles(desktop).map_err(RoutineFailure::Rejected),
            Self::Simecube => simecube::list_result(desktop),
            Self::Crewchief => crewchief::list_result(desktop),
        }
    }

    fn activation_result(self, desktop: &dyn Desktop, profile: Option<&str>) -> RoutineResult<String> {
        match self {
            Self::Fanatec => fanatec::activation_result(desktop, profile),
            Self::Simecube => simecube::activation_result(desktop, profile),
            Self::Crewchief => crewchief::activation_result(desktop, profile),
        }
    }
}

/// The routines in order: name, controller, description.
pub const ROUTINES: [(&str, Controller, &str); 3] = [
    (
        "Fanalab",
        Controller::Fanatec,
        "On entering the game or vehicle, this routine switches the FANALAB app to the profile chosen below automatically",
    ),
    (
        "Simucube 2 True Drive Paddock",
        Controller::Simecube,
        "On entering the game or vehicle, this routine switches the Simucube 2 True Drive Paddock to the profile chosen below automatically",
    ),
    ("CrewChief", Controller::Crewchief, ""),
];

/// The warning instead of a routine's options in a Wayland session in which the app window cannot be
/// reached.
const NEEDS_X11_SESSION: &str = "Routine options need an X11 session. In a Wayland session OpenRigCue cannot reach its own window; please log in to an X11 session and try again.";

/// The warning when a game starts with routines enabled in a Wayland session in which no window can be
/// reached.
const ROUTINES_NEED_X11_SESSION: &str = "Routines need an X11 session. In a Wayland session OpenRigCue cannot reach other windows, so the enabled routines were not started.";

/// A routine's stored field (`None` when it is missing, distinct from a stored none).
fn routine_value(backend: &dyn crate::backend::Backend, routine: &str, field: &str) -> Option<LegacyValue> {
    let native = backend.native_tune().ok()?;
    native.item_owned("routines").ok()?.item_owned(routine).ok()?.item_owned(field).ok()
}

/// Whether a routine is enabled: the stored value's truth, the same rule that decides whether it starts.
pub fn enabled(app: &MyApp, routine: &str) -> bool {
    routine_value(&*app.backend, routine, "enabled").is_some_and(|value| value.is_truthy())
}

fn locked(app: &MyApp) -> bool {
    tuning::locked(app, true, false)
}

/// The body: the routines with their status and Options, and the disclaimer.
pub fn main(app: &MyApp) -> String {
    let locked = locked(app);
    let mut body = String::new();
    if locked {
        body.push_str(&super::note(&tr(app, "Tuning is unavailable in Stand By mode; start a game to tune."), ""));
    }
    for (index, (name, _, description)) in ROUTINES.iter().enumerate() {
        let busy = app.ui.tuning.routine_busy.as_deref() == Some(*name);
        let status = t(app, if enabled(app, name) { "Enabled" } else { "Disabled" });
        let button = format!(
            "<button type=\"button\" id=\"c-routines-{index}\" data-act=\"action\" data-arg=\"Routines:{}:\"{}>{}</button>",
            esc(name),
            if busy { " disabled aria-busy=\"true\"" } else if locked { " disabled" } else { "" },
            t(app, if busy { "Loading…" } else { "Options" })
        );
        body.push_str(&components::field(
            &esc(name),
            &format!("<div class=\"action-row\"><span class=\"mono\">{status}</span>{button}</div>"),
            &if description.is_empty() { String::new() } else { t(app, description) },
        ));
    }
    body.push_str(&format!(
        "<div class=\"control-row note-row\"><small>{}</small></div>",
        t(app, "Routines are an experimental part of OpenRigCue and might be switched off in a later version")
    ));
    components::panel(&themes::title(app, "Routines"), "", &body, "tuning-panel")
}

/// The values Compare looks at: each routine's status and profile.
pub fn compared(app: &MyApp) -> Vec<(String, Vec<String>)> {
    ROUTINES
        .iter()
        .flat_map(|(name, ..)| {
            [
                (
                    format!("{name} · {}", tr(app, "Status")),
                    vec!["routines".to_owned(), (*name).to_owned(), "enabled".to_owned()],
                ),
                (
                    format!("{name} · {}", tr(app, "Profile")),
                    vec!["routines".to_owned(), (*name).to_owned(), "profile".to_owned()],
                ),
            ]
        })
        .collect()
}

/// A routine's Options button: its options are prepared on a thread of their own.
pub fn action(app: &mut MyApp, control: &str, _index: Option<usize>) -> Result<(), String> {
    let (name, ..) =
        ROUTINES.iter().find(|(name, ..)| *name == control).ok_or_else(|| format!("no routine {control:?}"))?;
    if locked(app) || app.ui.tuning.routine_busy.is_some() {
        return Ok(());
    }
    app.ui.tuning.routine_busy = Some((*name).to_owned());
    super::super::refresh(app, &[Region::Main]);
    let window_title = app.title();
    let routine: &'static str = name;
    app.start_thread(
        "config_Button",
        Box::new(move |shared| prepare_options(&shared, &SystemDesktop::default(), routine, &window_title)),
    );
    Ok(())
}

/// The work behind a routine's Options, on its own thread: in a Wayland session a warning; otherwise the
/// controller's profiles are read and the app window brought to the front, then the configuration opens, or
/// a warning with the controller's message. `window_title` is the app window's title.
pub fn prepare_options(shared: &Weak<Mutex<MyApp>>, desktop: &dyn Desktop, routine: &str, window_title: &str) {
    let finish = |outcome: Result<Config, String>| {
        let Some(shared) = shared.upgrade() else { return };
        let mut app = lock(&shared);
        app.ui.tuning.routine_busy = None;
        super::super::refresh(&mut app, &[Region::Main]);
        match outcome {
            Ok(config) => tuning::open_dialog(&mut app, Dialog::Routine(config)),
            Err(message) => app.alert(&message),
        }
    };
    let Some((_, controller, description)) = ROUTINES.iter().find(|(name, ..)| *name == routine) else {
        return finish(Err(format!("KeyError: {routine:?}")));
    };
    if desktop::window_needs_x11_session(desktop, window_title) {
        return finish(Err(NEEDS_X11_SESSION.to_owned()));
    }
    let options = match controller.list_result(desktop) {
        Ok(options) => options,
        Err(failure) => return finish(Err(failure.message())),
    };
    match desktop.activate(window_title) {
        Ok(()) => {}
        Err(ActivateError::NoWindow) => tracing::debug!("no app window to bring to the front"),
        Err(ActivateError::Failed) => tracing::debug!("failed to activate window"),
    }
    let backend = match shared.upgrade() {
        Some(shared) => lock(&shared).backend.clone(),
        None => return,
    };
    let enabled = routine_value(&*backend, routine, "enabled").is_some_and(|value| value.is_truthy());
    let stored = routine_value(&*backend, routine, "profile").and_then(|value| value.as_str().map(str::to_owned));
    let profile = stored.filter(|p| options.contains(p)).or_else(|| options.first().cloned());
    finish(Ok(Config {
        routine: routine.to_owned(),
        description: (*description).to_owned(),
        enabled,
        profile,
        options,
    }))
}

/// A routine's configuration: its status and the controller's profile it switches to.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub routine: String,
    pub description: String,
    pub enabled: bool,
    pub profile: Option<String>,
    pub options: Vec<String>,
}

/// The configuration dialog.
pub fn config_dialog(app: &MyApp, config: &Config) -> String {
    let options: Vec<(String, String)> =
        config.options.iter().enumerate().map(|(i, o)| (i.to_string(), o.clone())).collect();
    let selected =
        config.profile.as_ref().and_then(|p| config.options.iter().position(|o| o == p)).map(|i| i.to_string());
    let body = [
        components::field(&t(app, "Routine"), &format!("<p>{}</p>", esc(&config.routine)), ""),
        if config.description.is_empty() {
            String::new()
        } else {
            components::field(&t(app, "Description"), &format!("<p>{}</p>", t(app, &config.description)), "")
        },
        components::field(
            &t(app, "Status"),
            &format!(
                "<label class=\"switch\"><input id=\"routine-enabled\" type=\"checkbox\" data-act=\"dialog-field\" data-arg=\"enabled\"{}><span>{}</span></label>",
                if config.enabled { " checked" } else { "" },
                t(app, if config.enabled { "On" } else { "Off" })
            ),
            "",
        ),
        components::field(
            &t(app, "Profile"),
            &components::select(
                "routine-profile",
                &tr(app, "Profile"),
                &options,
                selected.as_deref().unwrap_or(""),
                "dialog-field",
                "profile",
            ),
            "",
        ),
    ]
    .concat();
    super::super::dialogs::frame(app, &t(app, "Routine Configuration"), &body, Some((&t(app, "Confirm"), true)))
}

/// A field of the configuration dialog changed.
pub fn config_field(app: &mut MyApp, mut config: Config, arg: &str, value: &str) -> Result<(), String> {
    match arg {
        "enabled" => config.enabled = read::checked(value),
        "profile" => {
            let index: usize = value.parse().map_err(|_| format!("ValueError: {value:?}"))?;
            config.profile = config.options.get(index).cloned();
        }
        other => return Err(format!("no routine field {other:?}")),
    }
    tuning::replace_dialog(app, Dialog::Routine(config));
    Ok(())
}

/// The configuration was confirmed: the routine's entry in the tune takes its status and profile (the
/// routines table is created first when the tune has none).
pub fn config_confirmed(app: &mut MyApp, config: &Config) -> Result<(), String> {
    let present = app
        .backend
        .native_tune()
        .map_err(|error| error.detail)
        .and_then(|native| crate::backend::native_field_owned(&native, &["routines"]).map(|_| ()));
    if present.is_err() {
        app.set_tune_native_checked(&["routines"], &LegacyValue::from(json!({})))?;
    }
    let mut entry = Dict::new();
    entry.insert("enabled", LegacyValue::Bool(config.enabled));
    entry.insert("profile", config.profile.clone().map_or(LegacyValue::None, LegacyValue::Str));
    app.set_tune_native_checked(&["routines", &config.routine], &LegacyValue::Dict(entry))?;
    super::super::refresh(app, &[Region::Main, Region::Save]);
    Ok(())
}

/// A loaded profile: the enabled routines start on a thread of their own.
pub fn on_profile_load(app: &mut MyApp) {
    let window_title = app.title();
    app.start_thread(
        "launch_routine",
        Box::new(move |shared| launch_routine(&shared, &SystemDesktop::default(), &window_title)),
    );
}

/// Starts the routines: every enabled routine's profile is activated (a routine that fails is recorded in
/// Activity), then the game's window is brought to the front, else the app's window.
pub fn launch_routine(shared: &Weak<Mutex<MyApp>>, desktop: &dyn Desktop, window_title: &str) {
    let needs_x11 = desktop::window_needs_x11_session(desktop, window_title);
    for (key, controller, _) in ROUTINES {
        let profile = {
            let Some(shared) = shared.upgrade() else { return };
            let backend = lock(&shared).backend.clone();
            if !routine_value(&*backend, key, "enabled").is_some_and(|value| value.is_truthy()) {
                continue;
            }
            if needs_x11 {
                lock(&shared).alert(ROUTINES_NEED_X11_SESSION);
                return;
            }
            // An entry without a profile ends the start before the remaining routines.
            match routine_value(&*backend, key, "profile") {
                Some(profile) => profile.as_str().map(str::to_owned),
                None => return,
            }
        };
        match controller.activation_result(desktop, profile.as_deref()) {
            Err(RoutineFailure::Raised(message)) => {
                tracing::error!(%message, "routine profile activation");
                return;
            }
            Err(RoutineFailure::Rejected(message)) => {
                let Some(shared) = shared.upgrade() else { return };
                let mut app = lock(&shared);
                let title = tr(&app, "Routine not started: ^routine^").replace("^routine^", key);
                let message = tr(&app, &message);
                let notice = super::super::notices::notice(super::super::notices::Severity::Warning, &title, &message);
                super::super::notices::raise(&mut app, notice);
            }
            Ok(_) => {}
        }
    }
    let game_window = {
        let Some(shared) = shared.upgrade() else { return };
        let backend = lock(&shared).backend.clone();
        match backend.game_value() {
            Ok(LegacyValue::Str(game)) => {
                openrigcue_config::catalog::Catalog::builtin().game(&game).and_then(|g| g.window_name.clone())
            }
            _ => None,
        }
    };
    if let Some(window_name) = game_window {
        if desktop.activate(&window_name).is_ok() {
            return;
        }
    }
    let _ = desktop.activate(window_title);
}

#[cfg(test)]
#[path = "routines_tests.rs"]
mod tests;
