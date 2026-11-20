//! Hardware → Joystick: the Status switch, the stored joystick profiles (choose, create, copy, delete), the seven
//! assignment rows with their live input meters, the assignment dialog with its axis capture, the Magic Wheel
//! battery and the page's shortcut.
//!
//! The profiles are the `joystick` table of the service's metadata (`joystick/db`), the selected one is
//! `joystick/profile`, the devices and their live inputs are `joystick/devices`. The live meters and the profile
//! rows are read by the screen's live pass ([`rules_shared`], on its own thread every [`live_period`]) while the
//! screen is open.

use std::collections::BTreeMap;
use std::sync::{Mutex, Weak};
use std::time::Duration;

use openrigcue_model::native_value::LegacyValue;
use openrigcue_model::python_compare::native_equal;
use openrigcue_sources::api::joystick::page as logic;
use openrigcue_sources::api::joystick::ControllerView;
use serde_json::{json, Value};

use super::super::components::{self, RowState};
use super::super::dialogs_hardware::{self, Dialog as Shared};
use super::super::html::{esc, t, tf, tr};
use super::super::{dialogs, read, themes, Region, Screen};
use crate::app::MyApp;
use crate::backend::{Backend, BackendExt, Method};
use crate::spec::{self, Widget};

/// How often the axis capture looks at the devices.
const CAPTURE_PERIOD: Duration = Duration::from_millis(100);

/// What the screen keeps between passes (`app.ui.devices`).
#[derive(Debug, Default)]
pub struct State {
    /// The stored profiles, in the service's order.
    pub profiles: Vec<String>,
    /// The selected profile.
    pub selected: String,
    /// The selected profile's assignments (row key → `{joystick, type, trigger, mode, invert_axis}`).
    pub assignments: Value,
    /// Each assigned row's live input (0..100).
    pub levels: BTreeMap<String, i64>,
    /// The Magic Wheel battery charge in percent, while one is connected (outside the Windows rules).
    pub battery: Option<f64>,
    /// After a removal the Delete button stays enabled, also for the default profile.
    pub default_removable: bool,
    /// The last read failed: its text.
    pub error: Option<String>,
    /// The joystick status the last pass found (`switches.joystick_status` on or off).
    pub status_seen: Option<bool>,
}

/// The assignment dialog.
#[derive(Debug, Clone, PartialEq)]
pub struct Assign {
    /// The row's key (`left` … `speed`).
    pub key: String,
    /// The row has an assignment: its current settings are shown and no capture runs until "Capture again".
    pub configured: bool,
    /// The joysticks offered (the stored one alone for a configured row).
    pub joysticks: Vec<String>,
    pub joystick: String,
    pub mode: String,
    pub invert: bool,
    /// The captured input type (`axis >`, `axis <`, `button`) and its index.
    pub kind: String,
    pub trigger: String,
    /// The capture runs.
    pub capturing: bool,
    /// Identifies the capture thread of this opening (an older thread stops).
    pub generation: u64,
}

/// What a name dialog makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameFor {
    /// An empty profile.
    New,
    /// A copy of the selected profile.
    Copy,
}

/// The dialogs of this screen.
#[derive(Debug, Clone, PartialEq)]
pub enum Dialog {
    Assign(Assign),
    /// The name of a new profile or of a copy; `exists` when the name is taken.
    Name {
        purpose: NameFor,
        name: String,
        exists: bool,
    },
    /// Delete the profile?
    Remove {
        profile: String,
    },
}

fn call(result: Result<Value, crate::backend::Failure>) -> Result<Value, String> {
    result.map_err(|failure| failure.detail)
}

/// The text of a stored value as the rows show it.
fn text_of(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => crate::backend::text(other),
    }
}

/// The selected profile's name.
fn selected_profile(app: &MyApp) -> Result<String, String> {
    selected_profile_of(&*app.backend)
}

fn selected_profile_of(backend: &dyn Backend) -> Result<String, String> {
    Ok(text_of(&call(backend.get("joystick/profile"))?["name"]))
}

/// The stored assignments of `name` (an object; nothing for a missing profile).
fn profile_assignments(app: &MyApp, name: &str) -> Result<Value, String> {
    profile_assignments_of(&*app.backend, name)
}

fn profile_assignments_of(backend: &dyn Backend, name: &str) -> Result<Value, String> {
    let row = backend.metadata_row("joystick", name).map_err(|failure| failure.detail)?;
    match row {
        Some(value) => crate::backend::native_json(&value),
        None => Ok(json!({})),
    }
}

/// The stored profile names.
fn profile_names(backend: &dyn Backend) -> Result<Vec<String>, String> {
    let mut names = Vec::new();
    for row in backend.metadata_rows("joystick").map_err(|failure| failure.detail)? {
        names.push(row.map_err(|failure| failure.detail)?.0);
    }
    Ok(names)
}

/// The controllers' inputs of `joystick/devices` (`state`: name → `axis` / `button` → index → value).
pub fn controllers(devices: &Value) -> BTreeMap<String, ControllerView> {
    let mut views = BTreeMap::new();
    let Some(state) = devices["state"].as_object() else { return views };
    for (name, kinds) in state {
        let mut view = ControllerView::default();
        for (index, value) in kinds["axis"].as_object().into_iter().flatten() {
            if let (Ok(index), Some(value)) = (index.parse::<u32>(), value.as_f64()) {
                view.axes.insert(index, value.clamp(-32768.0, 32767.0) as i32);
            }
        }
        for (index, value) in kinds["button"].as_object().into_iter().flatten() {
            if let Ok(index) = index.parse::<u32>() {
                view.buttons.insert(index, read::truth(value));
            }
        }
        views.insert(name.clone(), view);
    }
    views
}

/// The connected joysticks' names, in the service's order.
pub fn joystick_names(devices: &Value) -> Vec<String> {
    devices["joysticks"].as_object().into_iter().flatten().map(|(_, joystick)| text_of(&joystick["name"])).collect()
}

/// One assignment as the shared row logic reads it.
fn entry(config: &Value) -> Option<(String, String, String, String, bool)> {
    config.as_object()?;
    Some((
        text_of(&config["joystick"]),
        text_of(&config["type"]),
        text_of(&config["trigger"]),
        text_of(&config["mode"]),
        read::truth(&config["invert_axis"]),
    ))
}

/// Each assigned row's live input for the devices' current state.
pub fn levels(assignments: &Value, devices: &Value) -> BTreeMap<String, i64> {
    let views = controllers(devices);
    let mut levels = BTreeMap::new();
    for (key, config) in assignments.as_object().into_iter().flatten() {
        let Some((joystick, kind, trigger, mode, invert_axis)) = entry(config) else { continue };
        let row = logic::Entry { joystick: &joystick, kind: &kind, trigger: &trigger, mode: &mode, invert_axis };
        if let Ok(Some(level)) = row.progress(&views) {
            levels.insert(key.clone(), level.clamp(0, 100));
        }
    }
    levels
}

/// Reads the profiles, the selected one's assignments, the live inputs and the Magic Wheel battery.
pub fn load(app: &mut MyApp) {
    let backend = app.backend.clone();
    let read = read_screen(&*backend);
    take_screen(app, read);
}

/// The screen's reads: the profiles, the selected one, its assignments and the devices' inputs.
type ScreenRead = Result<(Vec<String>, String, Value, Value), String>;

fn read_screen(backend: &dyn Backend) -> ScreenRead {
    let profiles = profile_names(backend)?;
    let selected = selected_profile_of(backend)?;
    let assignments = profile_assignments_of(backend, &selected)?;
    let devices = call(backend.get("joystick/devices"))?;
    Ok((profiles, selected, assignments, devices))
}

fn take_screen(app: &mut MyApp, read: ScreenRead) {
    let result = (|| -> Result<(), String> {
        let (profiles, selected, assignments, devices) = read?;
        let state = &mut app.ui.devices.joystick;
        // The meters start again at 0 for changed assignments.
        if state.assignments != assignments {
            state.levels.clear();
        }
        let live = levels(&assignments, &devices);
        state.levels.extend(live);
        state.profiles = profiles;
        state.selected = selected;
        state.assignments = assignments;
        let battery = (!app.win32())
            .then(|| logic::magic_wheel_level(&controllers(&devices)).map(logic::battery_percent))
            .flatten();
        app.ui.devices.joystick.battery = battery;
        app.ui.devices.joystick.error = None;
        Ok(())
    })();
    if let Err(error) = result {
        app.ui.devices.joystick.error = Some(error);
    }
}

/// The page description of the screen (its Status switch and Shortcuts action).
fn page() -> spec::Page {
    spec::joystick_page()
}

/// The status and tooltip of a row.
fn row_texts(app: &MyApp, config: Option<&Value>) -> (String, String) {
    let Some((joystick, kind, trigger, mode, _)) = config.and_then(entry) else {
        return (tr(app, logic::NOT_CONFIGURED), tr(app, logic::NOT_CONFIGURED));
    };
    let tooltip = tr(app, "Joystick: ^joystick^&#010;Type: ^type^&#010;Axis: ^axis^&#010;Mode: ^mode^")
        .replace("&#010;", "\n")
        .replace("^joystick^", &joystick)
        .replace("^type^", &kind)
        .replace("^axis^", &trigger)
        .replace("^mode^", &tr(app, &mode));
    let (template, input) = if kind == "button" {
        ("^joystick^ - button ^trigger^", trigger.clone())
    } else if mode == logic::SINGLE_AXIS {
        ("^joystick^ - single axis ^trigger^", trigger.clone())
    } else {
        (
            "^joystick^ - dual ^trigger^",
            kind.replace('>', &format!("{trigger} >")).replace('<', &format!("{trigger} <")),
        )
    };
    (tr(app, template).replace("^joystick^", &joystick).replace("^trigger^", &input), tooltip)
}

/// Whether Delete applies to the selected profile.
fn removable(app: &MyApp) -> bool {
    let state = &app.ui.devices.joystick;
    state.default_removable || state.selected != logic::DEFAULT_PROFILE
}

/// The body: Status, the profile choice and its buttons, the seven rows and the actions.
pub fn main(app: &MyApp) -> String {
    let state = &app.ui.devices.joystick;
    let page = page();
    let mut body = String::new();
    if let Some(error) = &state.error {
        body.push_str(&super::note(&tf(app, "Status unavailable: ^error^", &[("error", &tr(app, error))]), ""));
    }
    if let Some(percent) = state.battery {
        let shown = format!("{}{}%", tr(app, "Magicwheel battery charge: "), percent.trunc() as i64);
        body.push_str(&components::field(
            &t(app, "Magic Wheel"),
            &format!("<span class=\"pill\" title=\"{}\">{}</span>", esc(&shown), esc(&shown)),
            "",
        ));
    } else if !app.win32() {
        body.push_str(&components::field(
            &t(app, "Magic Wheel"),
            &format!("<span class=\"pill\">{}</span>", t(app, "No Magicwheel connected")),
            "",
        ));
    }
    if let Some(status) = page.control("status") {
        let on = read::truth(app.control(&["switches", "joystick_status"]));
        let row = RowState { value: json!(on), ..RowState::default() };
        body.push_str(&components::spec_row(app, Screen::Joystick.key(), status, &row, ""));
    }
    let options: Vec<(String, String)> = state.profiles.iter().map(|name| (name.clone(), name.clone())).collect();
    body.push_str(&components::field(
        &format!("<label for=\"joystick-profile\">{}</label>", t(app, "Profile")),
        &components::select(
            "joystick-profile",
            &tr(app, "Profile"),
            &options,
            &state.selected,
            "set",
            "Joystick:profile",
        ),
        "",
    ));
    body.push_str(&components::field(
        &t(app, "Profiles"),
        &format!(
            "<div class=\"device action-row\">{}{}{}</div>",
            components::button(&t(app, "New…"), "action", "Joystick:profile-new:", ""),
            components::button(&t(app, "Save as…"), "action", "Joystick:profile-copy:", ""),
            if removable(app) {
                components::button(&t(app, "Delete"), "action", "Joystick:profile-delete:", "")
            } else {
                format!("<button type=\"button\" disabled>{}</button>", t(app, "Delete"))
            }
        ),
        "",
    ));
    let rows: Vec<String> = logic::ASSIGNMENTS
        .iter()
        .map(|(key, label)| {
            let config = state.assignments.get(*key);
            let (status, tooltip) = row_texts(app, config);
            let level = state.levels.get(*key).copied().unwrap_or(0);
            format!(
                "<div class=\"control-row\" data-control=\"{key}\"><div class=\"control-label\"><strong>{}</strong><output id=\"joystick-{key}-level\">{level}</output></div><small id=\"joystick-{key}-status\">{}</small><div class=\"control-body\"><meter min=\"0\" max=\"100\" value=\"{level}\" aria-labelledby=\"joystick-{key}-status\"></meter><button type=\"button\" title=\"{}\" data-act=\"action\" data-arg=\"Joystick:assign.{key}:\">{}</button></div></div>",
                t(app, label),
                esc(&status),
                esc(&tooltip),
                t(app, "Assign")
            )
        })
        .collect();
    body.push_str(&components::more(app, &rows, "joystick:rows"));
    let header: String = page
        .header
        .iter()
        .enumerate()
        .filter_map(|(i, button)| match button {
            Widget::Button { text: label, .. } => {
                Some(components::button(&t(app, label), "action", &format!("Joystick:@header:{i}"), ""))
            }
            _ => None,
        })
        .collect();
    body.push_str(&components::actions(app, &header, "joystick"));
    components::panel(&themes::title(app, "Joystick"), "", &body, "")
}

/// Work when the screen opens: the profiles and rows are read.
pub fn on_enter(app: &mut MyApp) {
    load(app);
}

/// A value of the screen changed: the Status switch or the profile choice.
pub fn set(app: &mut MyApp, control: &str, part: &str, value: &str) -> Result<(), String> {
    match (control, part) {
        ("status", "") => {
            // The status becomes 1 or 0, then the listener switch takes the value.
            let on = read::checked(value);
            app.set_control_checked(&["switches", "joystick_status"], json!(i64::from(on)))?;
            app.ui.devices.joystick.status_seen = Some(on);
            app.set_control_checked(&["switches", "joystick"], json!(on))
        }
        ("profile", "") => {
            app.ui.devices.joystick.default_removable = false;
            call(app.backend.put("joystick/profile", json!({ "name": value })))?;
            load(app);
            Ok(())
        }
        _ => Err(format!("no Joystick value {control:?}")),
    }
}

/// A button of the screen: an Assign button, the profile buttons or the Shortcuts action.
pub fn action(app: &mut MyApp, control: &str, index: Option<usize>) -> Result<(), String> {
    if let Some(key) = control.strip_prefix("assign.") {
        return open_assign(app, key);
    }
    match control {
        "@header" => {
            let page = page();
            match index.and_then(|i| page.header.get(i)) {
                Some(Widget::Button { run: spec::Action::Shortcuts { actions }, .. }) => {
                    super::super::shortcuts::open(app, actions)
                }
                _ => Err(format!("no Joystick header button {index:?}")),
            }
        }
        "profile-new" => {
            dialogs_hardware::open(
                app,
                Shared::Joystick(Dialog::Name { purpose: NameFor::New, name: String::new(), exists: false }),
            );
            Ok(())
        }
        "profile-copy" => {
            let selected = app.ui.devices.joystick.selected.clone();
            let backend = app.backend.clone();
            // A failed read of the stored profiles ends the action with its error (no name is proposed).
            let name = logic::try_propose_name(&selected, |name| {
                backend.metadata_contains("joystick", name).map_err(|failure| failure.detail)
            })?;
            dialogs_hardware::open(app, Shared::Joystick(Dialog::Name { purpose: NameFor::Copy, name, exists: false }));
            Ok(())
        }
        "profile-delete" => {
            if !removable(app) {
                return Ok(());
            }
            let profile = app.ui.devices.joystick.selected.clone();
            dialogs_hardware::open(app, Shared::Joystick(Dialog::Remove { profile }));
            Ok(())
        }
        _ => Err(format!("no Joystick button {control:?}")),
    }
}

/// The next capture generation.
fn next_generation(app: &MyApp) -> u64 {
    match dialogs_hardware::current(app) {
        Some(Shared::Joystick(Dialog::Assign(assign))) => assign.generation + 1,
        _ => std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_nanos() as u64),
    }
}

/// The Assign button of row `key`: a configured row shows its settings; another clears the last device state,
/// lists the joysticks and starts the capture in the row's default mode.
pub fn open_assign(app: &mut MyApp, key: &str) -> Result<(), String> {
    if !logic::ASSIGNMENTS.iter().any(|(k, _)| *k == key) {
        return Err(format!("no Joystick row {key:?}"));
    }
    let selected = selected_profile(app)?;
    let assignments = profile_assignments(app, &selected)?;
    let generation = next_generation(app);
    let assign = if let Some(config) = assignments.get(key).filter(|c| c.is_object()) {
        Assign {
            key: key.to_owned(),
            configured: true,
            joysticks: vec![text_of(&config["joystick"])],
            joystick: text_of(&config["joystick"]),
            mode: config.get("mode").map(text_of).unwrap_or_else(|| logic::default_mode(key).to_owned()),
            invert: config.get("invert_axis").is_some_and(read::truth),
            kind: text_of(&config["type"]),
            trigger: text_of(&config["trigger"]),
            capturing: false,
            generation,
        }
    } else {
        let mut assign = Assign {
            key: key.to_owned(),
            configured: false,
            joysticks: Vec::new(),
            joystick: logic::NO_JOYSTICK_SELECTED.to_owned(),
            mode: logic::default_mode(key).to_owned(),
            invert: false,
            kind: String::new(),
            trigger: String::new(),
            capturing: true,
            generation,
        };
        reset_capture(app, &mut assign)?;
        assign
    };
    let capturing = assign.capturing;
    dialogs_hardware::open(app, Shared::Joystick(Dialog::Assign(assign)));
    if capturing {
        start_capture(app, generation);
    }
    Ok(())
}

/// A new capture: the device's last state is cleared, the joysticks listed and the captured input cleared.
fn reset_capture(app: &mut MyApp, assign: &mut Assign) -> Result<(), String> {
    call(app.backend.delete("joystick/devices/last-state"))?;
    let devices = call(app.backend.get("joystick/devices"))?;
    assign.joysticks = joystick_names(&devices);
    assign.joystick = if assign.joysticks.is_empty() {
        logic::NO_JOYSTICK_FOUND.to_owned()
    } else {
        logic::NO_JOYSTICK_SELECTED.to_owned()
    };
    assign.kind.clear();
    assign.trigger.clear();
    assign.configured = false;
    assign.capturing = true;
    Ok(())
}

/// Starts the capture thread of the assignment dialog opened as `generation`.
fn start_capture(app: &mut MyApp, generation: u64) {
    app.start_thread("joystick_capture", Box::new(move |weak| capture_thread(weak, generation)));
}

/// The open assignment dialog while it captures for `generation`: in front, or held behind a Downloaded
/// Profiles review, where the capture goes on.
fn capturing(app: &MyApp, generation: u64) -> Option<Assign> {
    dialogs::open_dialogs(app).find_map(|(dialog, _)| match dialog {
        dialogs::Dialog::Hardware(Shared::Joystick(Dialog::Assign(assign)))
            if assign.capturing && assign.generation == generation =>
        {
            Some(assign.clone())
        }
        _ => None,
    })
}

/// Shows a capture pass in the assignment dialog of `generation`: drawn in front, kept for when a held one
/// comes back.
fn show_capture(app: &mut MyApp, generation: u64, assign: Assign) {
    let open = dialogs::open_dialogs_mut(app).find_map(|(dialog, front)| match dialog {
        dialogs::Dialog::Hardware(Shared::Joystick(Dialog::Assign(shown))) if shown.generation == generation => {
            Some((shown, front))
        }
        _ => None,
    });
    if let Some((shown, front)) = open {
        *shown = assign;
        if front {
            super::super::refresh(app, &[Region::Dialog]);
        }
    }
}

/// The capture: every 0.1 s, while the dialog captures, the devices are read and applied.
fn capture_thread(weak: Weak<Mutex<MyApp>>, generation: u64) {
    let mut capture = logic::Capture::default();
    let mut previous: Option<Value> = None;
    loop {
        let Some(shared) = weak.upgrade() else { return };
        let backend = {
            let app = crate::host::lock(&shared);
            if capturing(&app, generation).is_none() {
                return;
            }
            app.backend.clone()
        };
        let devices = match backend.get("joystick/devices") {
            Ok(devices) => devices,
            Err(error) => {
                tracing::error!(%error, "joystick capture");
                return;
            }
        };
        capture_pass(&mut crate::host::lock(&shared), generation, &devices, &mut capture, &mut previous);
        drop(shared);
        std::thread::sleep(CAPTURE_PERIOD);
    }
}

/// One capture pass: the strongest axis of the chosen joystick held past 60 % that was not a candidate in the
/// previous pass becomes the captured input; when the joystick list changed, the list is shown again and the
/// captured input cleared.
pub fn capture_pass(
    app: &mut MyApp,
    generation: u64,
    devices: &Value,
    capture: &mut logic::Capture,
    previous: &mut Option<Value>,
) {
    let Some(mut assign) = capturing(app, generation) else { return };
    let mut changed = false;
    if let Some((kind, trigger)) = capture.step(&assign.joystick, &controllers(devices)) {
        if assign.kind != kind || assign.trigger != trigger {
            assign.kind = kind;
            assign.trigger = trigger;
            changed = true;
        }
    }
    let joysticks = devices["joysticks"].clone();
    if previous.as_ref().is_some_and(|before| *before != joysticks) {
        assign.kind.clear();
        assign.trigger.clear();
        assign.joysticks = joystick_names(devices);
        if !assign.joysticks.contains(&assign.joystick) {
            assign.joystick = if assign.joysticks.is_empty() {
                logic::NO_JOYSTICK_FOUND.to_owned()
            } else {
                logic::NO_JOYSTICK_SELECTED.to_owned()
            };
        }
        changed = true;
    }
    *previous = Some(joysticks);
    if changed {
        show_capture(app, generation, assign);
    }
}

/// The dialog's markup.
pub fn dialog_render(app: &MyApp, dialog: &Dialog) -> String {
    match dialog {
        Dialog::Assign(assign) => assign_render(app, assign),
        Dialog::Name { purpose, name, exists } => {
            let title = match purpose {
                NameFor::New => t(app, "New Profile"),
                NameFor::Copy => t(app, "Make a copy of this profile"),
            };
            let hint = if *exists {
                format!("<p class=\"warning-text\" role=\"alert\">{}</p>", t(app, logic::NAME_EXISTS))
            } else {
                String::new()
            };
            let input = dialogs_hardware::input(
                "joystick-name",
                &tr(app, "Name"),
                "name",
                "text",
                name,
                &format!(
                    " maxlength=\"{}\" data-autofocus{}",
                    logic::NAME_MAX_LENGTH,
                    if *exists { " aria-invalid=\"true\"" } else { "" }
                ),
            );
            dialogs_hardware::frame(
                app,
                &title,
                &format!(
                    "{}{hint}",
                    components::field(&format!("<label for=\"joystick-name\">{}</label>", t(app, "Name")), &input, "")
                ),
                Some((&t(app, "Save"), true)),
            )
        }
        Dialog::Remove { profile } => dialogs_hardware::frame(
            app,
            &t(app, "Delete this profile?"),
            &format!("<p class=\"dialog-text\">{}{}</p>", t(app, logic::REMOVE_QUESTION), esc(profile)),
            Some((&t(app, "Delete"), true)),
        ),
    }
}

fn assign_render(app: &MyApp, assign: &Assign) -> String {
    let label = logic::ASSIGNMENTS.iter().find(|(k, _)| *k == assign.key).map_or(assign.key.as_str(), |(_, l)| l);
    let mut devices: Vec<(String, String)> = Vec::new();
    if assign.configured {
        devices.push((assign.joystick.clone(), assign.joystick.clone()));
    } else if assign.joysticks.is_empty() {
        devices.push((logic::NO_JOYSTICK_FOUND.to_owned(), tr(app, logic::NO_JOYSTICK_FOUND)));
    } else {
        devices.push((logic::NO_JOYSTICK_SELECTED.to_owned(), tr(app, logic::NO_JOYSTICK_SELECTED)));
        devices.extend(assign.joysticks.iter().map(|name| (name.clone(), name.clone())));
    }
    let instruction = if assign.configured {
        format!("<p class=\"dialog-text\">{}</p>", t(app, logic::CURRENT_SETTINGS))
    } else if assign.joystick != logic::NO_JOYSTICK_SELECTED && assign.joystick != logic::NO_JOYSTICK_FOUND {
        format!("<p class=\"warning-text\" role=\"status\">{}</p>", t(app, logic::CAPTURE_INSTRUCTION))
    } else {
        String::new()
    };
    let modes: Vec<(String, String)> =
        [logic::SINGLE_AXIS, logic::DUAL_AXIS].iter().map(|m| ((*m).to_owned(), tr(app, m))).collect();
    let (example, reverse_shown) = logic::mode_example(&assign.mode);
    let mut body = instruction;
    body.push_str(&components::field(
        &format!("<label for=\"joystick-device\">{}</label>", t(app, "Device")),
        &components::select(
            "joystick-device",
            &tr(app, "Device"),
            &devices,
            &assign.joystick,
            "dialog-field",
            "joystick",
        ),
        "",
    ));
    body.push_str(&components::field(
        &format!("<label for=\"joystick-mode\">{}</label>", t(app, "Mode")),
        &components::select("joystick-mode", &tr(app, "Mode"), &modes, &assign.mode, "dialog-field", "mode"),
        &t(app, example),
    ));
    if reverse_shown {
        body.push_str(&components::field(
            &t(app, "Reverse this axis?"),
            &dialogs_hardware::switch(
                "joystick-reverse",
                "invert",
                assign.invert,
                &t(app, if assign.invert { "On" } else { "Off" }),
                true,
            ),
            "",
        ));
    }
    body.push_str(&components::field(
        &t(app, "Type"),
        &format!("<span class=\"mono\" id=\"joystick-type\">{}</span>", esc(&assign.kind)),
        "",
    ));
    body.push_str(&components::field(
        &t(app, "Axis"),
        &format!("<span class=\"mono\" id=\"joystick-axis\">{}</span>", esc(&assign.trigger)),
        "",
    ));
    if assign.configured {
        body.push_str(&format!(
            "<div class=\"device action-row\">{}</div>",
            dialogs_hardware::button(&t(app, "Capture again"), "capture", "", true)
        ));
    }
    dialogs_hardware::frame(
        app,
        &tf(app, "Assign ^axis^", &[("axis", &tr(app, label))]),
        &body,
        Some((&t(app, "Apply"), true)),
    )
}

/// An action of the open dialog.
pub fn dialog_act(app: &mut MyApp, dialog: Dialog, act: &str, arg: &str, value: &str) -> Result<(), String> {
    match (dialog, act) {
        // Closing stops the capture (its thread sees the dialog gone).
        (_, "dialog-close") => {
            dialogs::close(app);
            Ok(())
        }
        (Dialog::Assign(mut assign), "dialog-field") => {
            match arg {
                "joystick" => assign.joystick = value.to_owned(),
                "mode" => assign.mode = value.to_owned(),
                "invert" => assign.invert = read::checked(value),
                _ => {}
            }
            dialogs_hardware::update(app, Shared::Joystick(Dialog::Assign(assign)));
            Ok(())
        }
        (Dialog::Assign(mut assign), "dialog-button") if arg == "capture" => {
            reset_capture(app, &mut assign)?;
            assign.generation += 1;
            let generation = assign.generation;
            dialogs_hardware::update(app, Shared::Joystick(Dialog::Assign(assign)));
            start_capture(app, generation);
            Ok(())
        }
        (Dialog::Assign(assign), "dialog-ok") => {
            dialogs::close(app);
            store_assignment(app, &assign)
        }
        (Dialog::Name { .. }, "dialog-field") if arg == "name" => {
            // Kept without drawing the dialog again, so that the Save click that ended the typing still lands.
            if let Some(Shared::Joystick(Dialog::Name { name, exists, .. })) = dialogs_hardware::current_mut(app) {
                *name = value.chars().take(logic::NAME_MAX_LENGTH).collect();
                *exists = false;
            }
            Ok(())
        }
        (Dialog::Name { purpose, name, .. }, "dialog-ok") => {
            if app.backend.metadata_contains("joystick", &name).map_err(|failure| failure.detail)? {
                dialogs_hardware::update(app, Shared::Joystick(Dialog::Name { purpose, name, exists: true }));
                return Ok(());
            }
            dialogs::close(app);
            let value = match purpose {
                NameFor::New => json!({}),
                NameFor::Copy => profile_assignments(app, &selected_profile(app)?)?,
            };
            call(app.backend.put("joystick/db", json!({ "key": name, "value": value })))?;
            call(app.backend.put("joystick/profile", json!({ "name": name })))?;
            app.ui.devices.joystick.default_removable = false;
            load(app);
            super::super::refresh(app, &[Region::Main]);
            Ok(())
        }
        (Dialog::Remove { profile }, "dialog-ok") => {
            dialogs::close(app);
            call(app.backend.call(Method::Delete, "joystick/db", Some(json!({ "key": profile }))))?;
            call(app.backend.put("joystick/profile", json!({ "name": logic::DEFAULT_PROFILE })))?;
            app.ui.devices.joystick.default_removable = true;
            load(app);
            super::super::refresh(app, &[Region::Main]);
            Ok(())
        }
        _ => Ok(()),
    }
}

/// The assignment dialog was confirmed: without a captured axis the row's assignment goes, otherwise the
/// joystick, type, axis, mode and reverse switch are stored.
fn store_assignment(app: &mut MyApp, assign: &Assign) -> Result<(), String> {
    let profile = selected_profile(app)?;
    let mut assignments = profile_assignments(app, &profile)?;
    let map = assignments.as_object_mut().ok_or("TypeError: the joystick profile is not a dictionary")?;
    if assign.trigger.is_empty() {
        map.shift_remove(&assign.key);
    } else {
        map.insert(
            assign.key.clone(),
            json!({
                "joystick": assign.joystick,
                "type": assign.kind,
                "trigger": assign.trigger,
                "mode": assign.mode,
                "invert_axis": assign.invert,
            }),
        );
    }
    call(app.backend.put("joystick/db", json!({ "key": profile, "value": assignments })))?;
    load(app);
    super::super::refresh(app, &[Region::Main]);
    Ok(())
}

/// Once, before the main loop's first pass: the screen's live pass starts on its own thread
/// (`joystick_meters`), which runs [`rules_shared`] again and again, waiting [`live_period`] between passes.
pub fn start(app: &mut MyApp) -> Result<(), String> {
    app.start_thread("joystick_meters", Box::new(live));
    Ok(())
}

/// The wait between two live passes: 0.1 s while the screen is open under the Windows rules (0.2 s otherwise),
/// 1 s while another screen is open.
pub fn live_period(app: &MyApp) -> Duration {
    if app.ui.mounted && app.ui.screen == Screen::Joystick {
        Duration::from_millis(if app.win32() { 100 } else { 200 })
    } else {
        Duration::from_secs(1)
    }
}

/// The live pass thread: it ends once the application is gone.
fn live(weak: Weak<Mutex<MyApp>>) {
    loop {
        let Some(shared) = weak.upgrade() else { return };
        if let Err(error) = rules_shared(&shared) {
            tracing::warn!(%error, "joystick live pass");
        }
        let period = live_period(&crate::host::lock(&shared));
        drop(shared);
        std::thread::sleep(period);
    }
}

/// One live pass while the screen is open: the profiles, the rows and the live meters are read again. The
/// product's live pass runs on its own thread ([`start`]); the main loop of an application without that thread
/// runs this pass.
pub fn rules(app: &mut MyApp) -> Result<(), String> {
    follow_status(app);
    if app.ui.mounted && app.ui.screen == Screen::Joystick {
        load(app);
    }
    Ok(())
}

/// [`rules`] on the main loop's shared application: the screen's reads run without holding it.
pub(crate) fn rules_shared(shared: &Mutex<MyApp>) -> Result<(), String> {
    let (shown, backend) = {
        let mut app = crate::host::lock(shared);
        follow_status(&mut app);
        (app.ui.mounted && app.ui.screen == Screen::Joystick, app.backend.clone())
    };
    if shown {
        let read = read_screen(&*backend);
        let mut app = crate::host::lock(shared);
        if app.ui.screen == Screen::Joystick {
            take_screen(&mut app, read);
        }
    }
    Ok(())
}

/// The joystick listener follows the joystick status, as the Status switch does: the status is on unless it is
/// false or the number 0, and the switch starts off. When the status differs from the switch (also at the first
/// pass, when a stored status is on), the switch takes it, the status is stored as 1 or 0 and the listener switch
/// (`switches.joystick`) takes it. A failed write is logged; the switch keeps its new state.
fn follow_status(app: &mut MyApp) {
    let Ok(status) = app.backend.control_value(&["switches", "joystick_status"]) else { return };
    let on = !native_equal(&status, &LegacyValue::Int(0));
    let seen = app.ui.devices.joystick.status_seen.replace(on);
    if seen.unwrap_or(false) == on {
        return;
    }
    let written = app
        .set_control_checked(&["switches", "joystick_status"], json!(i64::from(on)))
        .and_then(|()| app.set_control_checked(&["switches", "joystick"], json!(on)));
    if let Err(error) = written {
        tracing::warn!(%error, "joystick listener");
    }
}

#[cfg(test)]
#[path = "joystick_tests.rs"]
mod tests;
