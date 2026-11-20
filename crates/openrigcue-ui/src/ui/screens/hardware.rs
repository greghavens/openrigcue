//! Hardware → Hardware: the output configuration of every device group (Intellibox, Shakers, Motion, Motion
//! Compensation, Belt, SIM3D, HUD, IFlag, HapticR), with the rules that show, hide and enable rows by device,
//! connection, motion model and running game; the device tests; the motion and belt tests; the per-model
//! controller dialogs (see [`super::super::dialogs_hardware`]); and the main loop's Hardware work (start-up
//! writes, the per-pass test rules and the sound device refresh).
//!
//! The settings rows come from the typed page description (`spec::hardware_page`); the lists the description
//! leaves empty (ports, sound devices, speaker layouts, manufacturers, models) are filled here from the service.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use openrigcue_model::native_value::LegacyValue;
use openrigcue_model::python_compare::native_equal;
use serde_json::{json, Value};

use super::super::components::{self, RowState};
use super::super::dialogs_hardware::{self as device_dialogs, Device, Test};
use super::super::html::{esc, t, tf, tr};
use super::super::read;
use super::super::{notices, themes, Region, Screen};
use crate::app::MyApp;
use crate::backend::{native_bool, native_int, text, Backend, BackendExt};
use crate::catalog::ModelFlags;
use crate::spec::{self, Action, Choice, Control, Widget};

/// The wind types: name and stored code.
pub const WIND_TYPES: [(&str, &str); 3] =
    [("Fan", "F"), ("Blower (discontinued)", "B"), ("Turbo (Fan + Blower) (discontinued)", "T")];

/// The SIM3D models and their number of ports.
const RUMBLE_MODELS: [(&str, i64); 3] = [("None", 0), ("3 Ports", 3), ("4 Ports", 4)];

/// The models with a second (horizontal) PRS port.
const TWO_PRS_MODELS: [&str; 5] =
    ["T1000-5M PRS200 (Upgrade)", "P5MP PRS200 (Upgrade)", "T5PRO (Upgrade)", "T1000-5M SV", "T1000-6M SV RKP5MP SV"];

/// The models listed only with the `enable_dyadic_surge` custom feature.
const DYADIC_MODELS: [&str; 2] = ["T1000-6M PRS200 RK", "T1000-6M SV RK"];

/// The motion test sliders' keys, in the description's order.
const MOTION_TEST_KEYS: [&str; 8] =
    ["pitch", "roll", "yaw", "sway", "surge", "heave", "traction_loss", "traction_loss_front"];

/// The speaker rows' keys.
const SPEAKERS: [&str; 6] = ["fl", "fr", "ce", "sub", "rl", "rr"];

/// The speaker layout chosen when a custom one is deleted (a built-in layout).
pub const DEFAULT_LAYOUT: &str = "SRS ShakeKit or SRS Shakebox Elite";

/// The devices with an Enabled switch, by settings group: the content key, its configuration section and
/// whether it has a Com Port row.
const DEVICES: [(&str, &str, bool); 8] = [
    ("INTELLIBOX", "INTELLIBOX", true),
    ("MOTIONBOX", "MOTIONBOX", false),
    ("MOTION_COMPENSATION", "MOTION_COMPENSATION", true),
    ("BELT_TENSIONER", "BELT_TENSIONER", true),
    ("SIM3D", "SIM3D", true),
    ("HUD", "HUD", false),
    ("IFLAG", "IFLAG", true),
    ("HAPTICR", "HAPTICR", true),
];

/// How long the device tests keep their outputs on.
const INTELLIBOX_TEST: Duration = Duration::from_secs(7);
const SHORT_TEST: Duration = Duration::from_millis(1500);

/// What the screen keeps between passes (`app.ui.devices.hardware`).
#[derive(Debug, Default)]
pub struct State {
    /// The service's device list (`GET hardware`: sound devices and serial ports), read when the screen opens,
    /// after a motion model change and on a sound device refresh.
    pub listing: Option<Value>,
    /// The device test that runs.
    pub test: Option<RunningTest>,
    /// Counts the tests, so that a test's timer ends only its own test.
    pub generation: u64,
    /// The path typed into the SRS installer import row.
    pub srs_path: String,
    /// The motion measurement sweep that runs.
    pub sweep: Option<Sweep>,
    /// The configured sound device did not open: the Sound Device row shows None (the setting keeps it).
    pub sound_device_failed: bool,
}

/// A device test that runs: what it tests and the outputs it turned on (restored at its end).
#[derive(Debug, Clone, PartialEq)]
pub struct RunningTest {
    pub target: Test,
    pub generation: u64,
    pub previous: Vec<(String, LegacyValue)>,
}

/// The motion measurement sweep: progress, the file it writes and its stop request.
#[derive(Debug, Clone)]
pub struct Sweep {
    pub row: usize,
    pub total: usize,
    pub path: String,
    pub stop: Arc<AtomicBool>,
}

// ---- What the rows depend on ------------------------------------------------------------------------------

/// The state the rules read, once per render.
struct Rules {
    model_name: String,
    model: Option<&'static ModelFlags>,
    motion_connected: bool,
    game_running: bool,
    motion_test: bool,
    belt_test: bool,
    belt_homing: i64,
    belt_connected: bool,
    iflag_connected: bool,
    hapticr_connected: bool,
    sim3d_connected: bool,
    sim3d_ports: i64,
    sound_card: String,
    layout: String,
    /// Each speaker's use in the chosen layout (by lower-case key).
    channels: Vec<(String, bool)>,
    hapticr: bool,
    thanos_locked: bool,
    testing: bool,
    /// An iFlag or HapticR test the service runs (started here or elsewhere): its value is not 0.
    iflag_running: bool,
    haptic_running: bool,
}

/// Whether a test value of the service is anything but the number 0 (a test runs).
pub(crate) fn test_running(app: &MyApp, key: &str) -> bool {
    app.backend.control_value(&["tests", key]).map_or(true, |value| !native_equal(&value, &LegacyValue::Int(0)))
}

fn is_str(value: &LegacyValue, expected: &str) -> bool {
    native_equal(value, &LegacyValue::Str(expected.into()))
}

/// Whether a game other than stand-by and the test mode runs.
pub fn game_running(app: &MyApp) -> bool {
    app.backend.game_value().is_ok_and(|game| !is_str(&game, "none") && !is_str(&game, "test"))
}

fn accessory_connected(app: &MyApp, name: &str) -> bool {
    app.backend.accessory_status(name).is_ok_and(|status| is_str(&status, "Connected"))
}

fn motion_connected(app: &MyApp) -> bool {
    app.backend.motion_parameter("motion_status").is_ok_and(|status| is_str(&status, "Connected"))
}

fn setting_text(app: &MyApp, section: &str, key: &str) -> String {
    match app.backend.native_setting(section, key) {
        Ok(LegacyValue::Str(value)) => value,
        Ok(other) => openrigcue_model::python_repr::str_value(&other),
        Err(_) => String::new(),
    }
}

fn setting_flag(app: &MyApp, section: &str, key: &str) -> bool {
    app.backend.native_setting(section, key).ok().and_then(|value| native_bool(&value).ok()).unwrap_or(false)
}

fn control_int(app: &MyApp, path: &[&str]) -> i64 {
    app.backend.control_value(path).ok().and_then(|value| native_int(&value).ok()).unwrap_or(0)
}

/// Whether HapticR is enabled in the settings.
pub fn hapticr_enabled(app: &MyApp) -> bool {
    setting_flag(app, "APP", "enable_hapticr")
}

/// The configured motion model's name.
pub fn model_name(app: &MyApp) -> String {
    setting_text(app, "MOTIONBOX", "model")
}

/// The number of SIM3D ports of the configured model.
fn rumble_port_count(app: &MyApp) -> i64 {
    let model = setting_text(app, "SIM3D", "model");
    RUMBLE_MODELS.iter().find(|(name, _)| *name == model).map_or(0, |(_, ports)| *ports)
}

/// Whether a speaker layout is one of the built-in ones.
fn builtin_layout(name: &str) -> bool {
    openrigcue_config::catalog::registry::Registry::builtin().builtin_shaker_layouts.iter().any(|b| b == name)
}

/// Each speaker's `enable` in the loaded layout, by lower-case key.
fn layout_channels(app: &MyApp) -> Vec<(String, bool)> {
    let Ok(layout) = app.backend.shaker_effects() else { return Vec::new() };
    let Some(fields) = layout.as_dict() else { return Vec::new() };
    fields
        .iter()
        .map(|(key, value)| {
            let enabled = value.item("enable").map(LegacyValue::is_truthy).unwrap_or(false);
            (key.to_text().to_lowercase(), enabled)
        })
        .collect()
}

impl Rules {
    fn read(app: &MyApp) -> Rules {
        let model_name = model_name(app);
        let model = crate::catalog::model(&model_name);
        let game_running = game_running(app);
        let motion_test = control_int(app, &["tests", "motion_test"]) == 1;
        let controller = app.backend.motion_parameter("thanos_controller").unwrap_or(LegacyValue::None);
        let thanos4u = is_str(&controller, "Thanos4U(S)") || is_str(&controller, "Thanos4U(M)");
        Rules {
            model,
            motion_connected: motion_connected(app),
            game_running,
            motion_test,
            belt_test: control_int(app, &["tests", "belt_test"]) == 1,
            belt_homing: control_int(app, &["tests", "belt_homing"]),
            belt_connected: accessory_connected(app, "belt"),
            iflag_connected: accessory_connected(app, "iflag"),
            hapticr_connected: accessory_connected(app, "hapticr"),
            sim3d_connected: accessory_connected(app, "sim3d"),
            sim3d_ports: rumble_port_count(app),
            sound_card: setting_text(app, "SHAKEBOX", "soundcard"),
            layout: setting_text(app, "SHAKEBOX", "profile"),
            channels: layout_channels(app),
            hapticr: hapticr_enabled(app),
            thanos_locked: thanos4u && (game_running || motion_test),
            testing: app.ui.devices.hardware.test.is_some(),
            iflag_running: test_running(app, "iflag_test"),
            haptic_running: test_running(app, "haptic_test"),
            model_name,
        }
    }

    fn manufacturer(&self) -> &str {
        self.model.map_or("", |m| m.manufacturer.as_str())
    }

    fn no_card(&self) -> bool {
        self.sound_card == "None"
    }

    /// The motion test can run: a model is set, the platform is connected and no game runs.
    fn motion_test_allowed(&self) -> bool {
        self.model_name != "None" && self.motion_connected && !self.game_running
    }

    /// A speaker's row is usable: a sound device is set and the layout uses the speaker.
    fn channel_used(&self, key: &str) -> bool {
        !self.no_card() && self.channels.iter().any(|(channel, on)| channel == key && *on)
    }

    /// The iFlag test can run: connected, no game, no other test here and no iFlag test running elsewhere.
    fn iflag_ready(&self) -> bool {
        self.iflag_connected && !self.game_running && !self.testing && !self.iflag_running
    }

    /// The HapticR tests can run: enabled, connected, no game, no other test here and no HapticR test running
    /// elsewhere.
    fn haptic_ready(&self) -> bool {
        self.hapticr && self.hapticr_connected && !self.game_running && !self.testing && !self.haptic_running
    }

    /// The SIM3D tests can run: a model is set, connected, no game and no other test.
    fn rumble_ready(&self) -> bool {
        self.sim3d_ports > 0 && self.sim3d_connected && !self.game_running && !self.testing
    }

    fn two_tl(&self) -> bool {
        self.model_name.contains("2 TL")
    }

    fn two_prs(&self) -> bool {
        TWO_PRS_MODELS.contains(&self.model_name.as_str())
    }

    /// The manual port's settings section and key for the model's manufacturer.
    fn port_setting(&self) -> (&'static str, &'static str) {
        port_setting(self.manufacturer())
    }
}

/// The manual motion port's settings section and key of a manufacturer.
fn port_setting(manufacturer: &str) -> (&'static str, &'static str) {
    match manufacturer {
        "VNM SIMULATION" => ("VNM SIMULATION", "vnm_com_port"),
        "PT-ACTUATOR" => ("PT-ACTUATOR", "pt_com_port"),
        _ => ("PROSIMU", "prs200_com_port1"),
    }
}

// ---- The lists the description leaves empty ----------------------------------------------------------------

/// The Com Port options: Auto Search (stored as an empty text), then COM1…COM99.
fn com_ports() -> Vec<Choice> {
    let mut ports = vec![Choice::new("Auto Search", "")];
    ports.extend((1..100).map(|port| Choice::new(&format!("COM{port}"), format!("COM{port}"))));
    ports
}

/// The sound devices: by stable ID with the system's name, or by name (without `Unknown`).
fn sound_devices(app: &MyApp) -> Vec<Choice> {
    let Some(listing) = app.ui.devices.hardware.listing.as_ref() else { return Vec::new() };
    if let Some(devices) = listing["sound_devices"].as_array() {
        return devices
            .iter()
            .filter_map(|device| Some(Choice::new(device["name"].as_str()?, device["id"].as_str()?)))
            .collect();
    }
    listing["sound_cards"]
        .as_array()
        .into_iter()
        .flatten()
        .map(text)
        .filter(|name| name != "Unknown")
        .map(|name| Choice::new(&name, name.clone()))
        .collect()
}

/// The stored speaker layouts, sorted.
fn layouts(app: &MyApp) -> Vec<Choice> {
    let mut names: Vec<String> =
        app.backend.metadata_keys("shakers").map(|keys| keys.filter_map(Result::ok).collect()).unwrap_or_default();
    names.sort();
    names.into_iter().map(|name| Choice::new(&name, name.clone())).collect()
}

/// The models of a manufacturer, in catalog order (the two dyadic models only with their custom feature).
fn models_of(app: &MyApp, manufacturer: &str) -> Vec<&'static ModelFlags> {
    let dyadic = setting_flag(app, "CUSTOM_FEATURES", "enable_dyadic_surge");
    crate::catalog::models()
        .iter()
        .filter(|model| model.manufacturer == manufacturer)
        .filter(|model| dyadic || !DYADIC_MODELS.contains(&model.name.as_str()))
        .collect()
}

/// The manufacturer shown: the configured model's, or the `None` model's.
fn shown_manufacturer(rules: &Rules) -> String {
    rules.model.or_else(|| crate::catalog::model("None")).map(|model| model.manufacturer.clone()).unwrap_or_default()
}

/// The serial ports the manual motion port can use: not held by an accessory or the traction-loss board;
/// PROSIMU boards only on FTDI 0403:6001 adapters. Each with whether the horizontal port lists it too.
fn motion_ports(app: &MyApp, rules: &Rules) -> Vec<(String, bool)> {
    let Some(listing) = app.ui.devices.hardware.listing.as_ref() else { return Vec::new() };
    let held: Vec<LegacyValue> = ["compensation_port", "belt_port", "intellibox_port", "iflag_port", "sim3d_port"]
        .iter()
        .filter_map(|key| app.backend.accessory_status(key).ok())
        .collect();
    let tl_port = app.backend.motion_parameter("prs200_snc6_tl_com_port").unwrap_or(LegacyValue::None);
    let direct = matches!(rules.manufacturer(), "VNM SIMULATION" | "PT-ACTUATOR");
    let mut ports = Vec::new();
    for device in listing["serial_ports"].as_array().into_iter().flatten() {
        let port = text(&device["device"]);
        let value = LegacyValue::Str(port.clone());
        if held.iter().any(|held| native_equal(held, &value)) || native_equal(&tl_port, &value) {
            continue;
        }
        if direct {
            ports.push((port, false));
        } else if text(&device["hwid"]).contains("0403:6001") {
            ports.push((port, rules.two_prs()));
        }
    }
    ports
}

// ---- The page as shown -------------------------------------------------------------------------------------

fn set_options(control: &mut Control, options: Vec<Choice>) {
    if let Widget::Select { options: list } = &mut control.widget {
        *list = options;
    }
}

fn set_button_text(control: &mut Control, shown: &str) {
    if let Widget::Button { text: label, .. } = &mut control.widget {
        *label = shown.to_owned();
    }
}

/// The page description with the lists filled in and the toggle buttons' texts.
fn page(app: &MyApp, rules: &Rules) -> spec::Page {
    let mut page = spec::hardware_page();
    let manufacturer = shown_manufacturer(rules);
    for control in page.sections.iter_mut().flat_map(|s| s.controls.iter_mut()) {
        match control.id.as_str() {
            id if id.starts_with("com_port_") => set_options(control, com_ports()),
            "soundcard" => set_options(control, sound_devices(app)),
            "shakers_profile" => set_options(control, layouts(app)),
            "motion_manufacturer" => set_options(
                control,
                crate::catalog::manufacturers().iter().map(|m| Choice::new(&m.name, m.name.clone())).collect(),
            ),
            "motion_model" => set_options(
                control,
                models_of(app, &manufacturer).iter().map(|m| Choice::new(&m.name, m.name.clone())).collect(),
            ),
            "motion_port" | "motion_port2" => {
                let second = control.id == "motion_port2";
                let mut options = vec![Choice::new("", "")];
                options.extend(
                    motion_ports(app, rules)
                        .into_iter()
                        .filter(|(_, both)| !second || *both)
                        .map(|(port, _)| Choice::new(&port, port.clone())),
                );
                set_options(control, options);
                if !second && matches!(rules.manufacturer(), "VNM SIMULATION" | "PT-ACTUATOR") {
                    control.label = "Com Port".into();
                }
            }
            "motion_test" => {
                set_button_text(control, if rules.motion_test { "Stop Test" } else { "Start Test" });
            }
            "belt_test" => set_button_text(control, if rules.belt_test { "Stop Test" } else { "Start Test" }),
            _ => {}
        }
    }
    page
}

/// The device content key of an Enabled or Com Port row.
fn device_of(id: &str) -> Option<(&'static str, &'static str, bool)> {
    let key = id.strip_prefix("auto_search_").or_else(|| id.strip_prefix("com_port_"))?;
    DEVICES.iter().find(|(k, ..)| *k == key).copied()
}

/// The indices of the motion model row's buttons that apply: Axis Assignment, Yaw VR, Controller Settings
/// (Thanos / PT), Move platform to center, Controller Settings (VNM), Controller Settings (DOF Reality).
fn model_buttons(rules: &Rules) -> Vec<usize> {
    if !rules.motion_connected {
        return Vec::new();
    }
    let mut kept = Vec::new();
    if rules.model.is_some_and(|m| m.axis_assignment) {
        kept.push(0);
    }
    match rules.manufacturer() {
        "Yaw VR" => kept.extend([1, 3]),
        "PT-ACTUATOR" | "THANOS" => kept.push(2),
        "VNM SIMULATION" => kept.push(4),
        "DOF REALITY" => kept.push(5),
        _ => {}
    }
    kept.sort_unstable();
    kept
}

/// The state of a row, `None` when the rules hide it.
fn row_state(app: &MyApp, rules: &Rules, control: &Control) -> Option<RowState> {
    let id = control.id.as_str();
    let mut state = RowState::default();
    if let Some(field) = &control.field {
        state.value = read::field(app, field);
    }
    if let Some((_, section, _)) = device_of(id) {
        if id.starts_with("com_port_") {
            // The ports are Windows port names: the row is offered only where this program runs on Windows, and
            // only while the device's automatic search is on.
            if !(app.win32() && cfg!(windows)) || !setting_flag(app, section, "auto_search") {
                return None;
            }
            state.value = Value::String(setting_text(app, section, "com_port"));
        } else {
            state.value = Value::Bool(setting_flag(app, section, "auto_search"));
        }
        return Some(state);
    }
    match id {
        "soundcard" => {
            let shown = if app.ui.devices.hardware.sound_device_failed { "None" } else { rules.sound_card.as_str() };
            state.value = Value::String(shown.to_owned());
        }
        "shakers_profile" => {
            state.value = Value::String(rules.layout.clone());
            state.enabled = !rules.no_card();
        }
        "fl" | "fr" | "ce" | "sub" | "rl" | "rr" => state.enabled = rules.channel_used(id),
        "motion_manufacturer" => state.value = Value::String(shown_manufacturer(rules)),
        "motion_model" => {
            state.value = Value::String(rules.model_name.clone());
            state.buttons = Some(model_buttons(rules));
        }
        "motion_port" => {
            if !rules.model.is_some_and(|m| m.manual_com_port) {
                return None;
            }
            let (section, key) = rules.port_setting();
            state.value = Value::String(setting_text(app, section, key));
        }
        "motion_port2" => {
            if !rules.model.is_some_and(|m| m.manual_com_port) || !rules.two_prs() {
                return None;
            }
            state.value = Value::String(setting_text(app, "PROSIMU", "prs200_com_port2"));
        }
        "auto_smoothing_low_speed" => state.value = Value::Bool(setting_flag(app, "MOTIONBOX", id)),
        "invert_tl" => {
            if !rules.model.is_some_and(|m| m.allow_change_tl_dir) {
                return None;
            }
            state.value = Value::Bool(setting_flag(app, "PROSIMU", "invert_tl"));
        }
        "motion_test" => state.enabled = rules.motion_test_allowed(),
        "traction_loss_front" if !rules.two_tl() => return None,
        "pitch" | "roll" | "yaw" | "sway" | "surge" | "heave" | "traction_loss" | "traction_loss_front" => {
            state.enabled = rules.motion_test && rules.motion_test_allowed();
        }
        "belt_test" => state.enabled = rules.belt_connected && !rules.game_running,
        "belt_test_slider" => state.enabled = rules.belt_homing == 1 && rules.belt_test,
        id if id.starts_with("rumble_port") => {
            let port: i64 = id.trim_start_matches("rumble_port").parse().unwrap_or(0);
            if port > rules.sim3d_ports {
                return None;
            }
        }
        "iflag_test" => state.enabled = rules.iflag_ready(),
        "haptic_test_front" | "haptic_test_rear" => state.enabled = rules.haptic_ready(),
        _ => {}
    }
    Some(state)
}

/// The row buttons the rules disable (by their action argument): every test while one runs, the tests of a
/// device that is not ready, the layout buttons without a sound device or for a built-in layout, the speaker
/// buttons of an unused speaker, and the Thanos4U controller dialog while it is locked.
fn disabled_buttons(rules: &Rules) -> Vec<String> {
    let mut disabled = Vec::new();
    let mut off = |control: &str, index: usize| disabled.push(format!("Hardware:{control}:{index}"));
    if rules.testing {
        for control in ["wind_type", "nbr_leds", "shift_light_dim_perc"] {
            off(control, 0);
        }
    }
    if rules.no_card() {
        off("shakers_profile", 0);
    }
    if builtin_layout(&rules.layout) {
        off("shakers_profile", 1);
    }
    for key in SPEAKERS {
        if !rules.channel_used(key) || rules.testing {
            off(key, 0);
        }
        if !rules.channel_used(key) || builtin_layout(&rules.layout) {
            off(key, 1);
        }
    }
    if !rules.rumble_ready() {
        for port in 1..5 {
            off(&format!("rumble_port{port}"), 0);
        }
    }
    if !rules.haptic_ready() {
        off("haptic_test_front", 0);
        off("haptic_test_rear", 0);
    }
    if rules.thanos_locked {
        off("motion_model", 2);
    }
    disabled
}

/// The rows whose list holds names (ports, sound devices, speaker layouts): shown as named, not translated.
const NAMED_LISTS: [&str; 5] = ["soundcard", "shakers_profile", "motion_port", "motion_port2", "com_port_"];

fn named_list(id: &str) -> bool {
    NAMED_LISTS.iter().any(|named| if named.ends_with('_') { id.starts_with(named) } else { id == *named })
}

/// One row: the description's row, with a named list's entries as named (a built-in layout and Auto Search
/// translated).
fn row(app: &MyApp, control: &Control, state: &RowState) -> String {
    let Widget::Select { options } = &control.widget else {
        return components::spec_row(app, Screen::Hardware.key(), control, state, "");
    };
    if !named_list(&control.id) {
        return components::spec_row(app, Screen::Hardware.key(), control, state, "");
    }
    let mut bare = control.clone();
    bare.widget = Widget::Select { options: Vec::new() };
    let html = components::spec_row(app, Screen::Hardware.key(), &bare, state, "");
    let current = options.iter().position(|choice| read::same(&choice.value, &state.value));
    let mut list = String::new();
    if current.is_none() {
        list.push_str("<option value=\"\" selected disabled>—</option>");
    }
    for (index, choice) in options.iter().enumerate() {
        let shown = if builtin_layout(&choice.label) || choice.label == "Auto Search" || choice.label == "None" {
            tr(app, &choice.label)
        } else {
            choice.label.clone()
        };
        list.push_str(&format!(
            "<option value=\"{index}\"{}>{}</option>",
            if Some(index) == current { " selected" } else { "" },
            esc(&shown)
        ));
    }
    html.replacen("<option value=\"\" selected disabled>—</option>", &list, 1)
}

/// Marks the buttons with these arguments disabled.
fn disable(html: &str, arguments: &[String]) -> String {
    arguments.iter().fold(html.to_owned(), |html, argument| {
        html.replace(&format!("data-arg=\"{argument}\">"), &format!("data-arg=\"{argument}\" disabled>"))
    })
}

/// The settings groups shown: HapticR only while it is enabled; the Motion Compensation group under the
/// Windows rules.
fn group_shown(app: &MyApp, rules: &Rules, section: &spec::Section) -> bool {
    match section.id.as_deref() {
        Some("hapticr_container") => rules.hapticr,
        Some("mc_container") => app.win32(),
        _ => true,
    }
}

/// The body: the settings group picker, the group's rows and its extra rows.
pub fn main(app: &MyApp) -> String {
    let rules = Rules::read(app);
    let page = page(app, &rules);
    let shown: Vec<(usize, &spec::Section)> =
        page.sections.iter().enumerate().filter(|(_, section)| group_shown(app, &rules, section)).collect();
    let selected = shown.iter().position(|(index, _)| *index == app.ui.section).unwrap_or(0);
    let Some((index, section)) = shown.get(selected).copied() else { return String::new() };
    let groups: Vec<(usize, String)> = shown.iter().map(|(i, s)| (*i, tr(app, &s.title))).collect();
    let disabled = disabled_buttons(&rules);
    let mut rows: Vec<String> = section
        .controls
        .iter()
        .filter_map(|control| {
            row_state(app, &rules, control).map(|state| disable(&row(app, control, &state), &disabled))
        })
        .collect();
    if section.id.as_deref() == Some("motion_container") {
        if let Some(sweep) = &app.ui.devices.hardware.sweep {
            rows.insert(
                rows.iter().position(|row| row.contains("data-control=\"motion_test\"")).map_or(0, |i| i + 1),
                components::field(
                    &t(app, "Measurement"),
                    &format!(
                        "<span class=\"mono\">{}</span> {}",
                        tf(
                            app,
                            "Measuring ^row^ / ^total^",
                            &[("row", &sweep.row.to_string()), ("total", &sweep.total.to_string())]
                        ),
                        components::button(&t(app, "Stop"), "action", "Hardware:sweep_stop:", "")
                    ),
                    &esc(&sweep.path),
                ),
            );
        }
        rows.push(components::field(
            &t(app, "Axis directions"),
            &format!(
                "<img class=\"axis-art\" src=\"/res:images/openrigcue/6dof.png\" width=\"320\" height=\"240\" alt=\"{}\">",
                t(app, "The motion axes of the seat")
            ),
            "",
        ));
    }
    if section.id.as_deref() == Some("shakers_container") {
        rows.push(srs_import_row(app));
    }
    let mut body = components::more(app, &rows, &format!("hardware:{index}"));
    if index == 0 && !app.win32() {
        body.push_str(&format!(
            "<div class=\"device action-row\">{}</div>",
            components::button(&t(app, "Controller setup"), "action", "Hardware:controller:", "")
        ));
    }
    components::panel(&themes::title(app, &section.title), &super::section_picker(app, &groups, index), &body, "")
}

/// The SRS installer import row: the path of the user's installer or installation folder and Import.
fn srs_import_row(app: &MyApp) -> String {
    format!(
        "<div class=\"control-row\" data-control=\"srs_import\"><div class=\"control-label\"><label for=\"c-hardware-srs_path\"><strong>{}</strong></label></div><small>{}</small><div class=\"control-body\"><input id=\"c-hardware-srs_path\" class=\"field-input\" type=\"text\" value=\"{}\" placeholder=\"{}\" aria-label=\"{}\" data-act=\"set\" data-arg=\"Hardware:srs_path\"><div class=\"device action-row\">{}</div></div></div>",
        t(app, "SRS Installer"),
        t(app, "Copies the sounds and firmware files from your SRS installer or installation folder."),
        esc(&app.ui.devices.hardware.srs_path),
        t(app, "Path of the SRS installer (.exe) or installation folder"),
        t(app, "SRS Installer"),
        components::button(&t(app, "Import"), "action", "Hardware:srs_import:", "")
    )
}

/// Work when the screen opens: the service's device list.
pub fn on_enter(app: &mut MyApp) {
    load_listing(app);
}

/// Reads the service's device list (sound devices and serial ports).
pub fn load_listing(app: &mut MyApp) {
    if let Err(error) = try_load_listing(app) {
        tracing::warn!(%error, "device list");
    }
}

/// Reads the service's device list; a failure keeps the list shown before.
fn try_load_listing(app: &mut MyApp) -> Result<(), String> {
    let listing = app.backend.get("hardware").map_err(|failure| failure.detail)?;
    app.ui.devices.hardware.listing = Some(listing);
    Ok(())
}

/// The tune values of the screen Compare looks at: the speaker volumes and the shift light brightness.
pub fn compared(app: &MyApp) -> Vec<(String, Vec<String>)> {
    let mut rows: Vec<(String, Vec<String>)> = spec::hardware_page()
        .controls()
        .filter_map(|control| match &control.field {
            Some(spec::Field { store: spec::Store::Tune, path }) => Some((tr(app, &control.label), path.clone())),
            _ => None,
        })
        .collect();
    rows.dedup();
    rows
}

// ---- Writes -------------------------------------------------------------------------------------------------

/// The choice a select sent (by its index in the options shown).
fn chosen(app: &MyApp, control: &str, value: &str) -> Result<Value, String> {
    let rules = Rules::read(app);
    let page = page(app, &rules);
    let control = page.control(control).ok_or_else(|| format!("no Hardware row {control:?}"))?;
    let Widget::Select { options } = &control.widget else { return Err(format!("{:?} is no list", control.id)) };
    let index: usize = value.parse().map_err(|_| format!("ValueError: {value:?}"))?;
    options.get(index).map(|choice| choice.value.clone()).ok_or_else(|| format!("IndexError: {value:?}"))
}

fn str_value(value: &Value) -> LegacyValue {
    LegacyValue::Str(match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    })
}

/// A value of the screen changed: the stored value each row stands for.
pub fn set(app: &mut MyApp, control: &str, part: &str, value: &str) -> Result<(), String> {
    if !part.is_empty() {
        return Err(format!("no Hardware value {control:?}:{part}"));
    }
    // The brightness and belt calibration values must be whole numbers as typed.
    let int = |value: &str| read::exact_int(value).map(LegacyValue::Int);
    if let Some((_, section, _)) = device_of(control) {
        return if control.starts_with("auto_search_") {
            app.set_setting_native_checked(section, "auto_search", &LegacyValue::Bool(read::checked(value)))
        } else {
            let port = chosen(app, control, value)?;
            app.set_setting_native_checked(section, "com_port", &str_value(&port))
        };
    }
    match control {
        "wind_type" => {
            let code = chosen(app, control, value)?;
            wind_type(app, &text(&code))
        }
        "nbr_fans" => {
            let count = chosen(app, control, value)?;
            fan_count(app, &text(&count))
        }
        "led_model" => {
            let model = chosen(app, control, value)?;
            app.set_setting_native_checked("LED", "model", &str_value(&model))?;
            app.backend.reload_led_colors().map_err(|error| error.detail)
        }
        "nbr_leds" => led_count(app, read::int(value)?),
        "brightness" => app.set_setting_native_checked("LED", "brightness", &int(value)?),
        "shift_light_dim_perc" => app.set_tune_native_checked(&["gear_shift_dim_perc"], &read::number(value)?),
        "soundcard" => {
            let device = chosen(app, control, value)?;
            sound_device(app, &text(&device))
        }
        "shakers_profile" => {
            let layout = chosen(app, control, value)?;
            choose_layout(app, &text(&layout))
        }
        "fl" | "fr" | "ce" | "sub" | "rl" | "rr" => {
            app.set_shaker_value_checked("shaker_volume", control, &read::number(value)?)
        }
        "use_srs_sounds" => use_srs_sounds(app, read::checked(value)),
        "srs_path" => {
            app.ui.devices.hardware.srs_path = value.to_owned();
            Ok(())
        }
        "motion_manufacturer" => {
            let manufacturer = chosen(app, control, value)?;
            choose_manufacturer(app, &text(&manufacturer))
        }
        "motion_model" => {
            let model = chosen(app, control, value)?;
            choose_model(app, &text(&model))
        }
        "motion_port" => {
            let port = chosen(app, control, value)?;
            let (section, key) = port_setting(Rules::read(app).manufacturer());
            app.set_setting_native_checked(section, key, &str_value(&port))
        }
        "motion_port2" => {
            let port = chosen(app, control, value)?;
            app.set_setting_native_checked("PROSIMU", "prs200_com_port2", &str_value(&port))
        }
        "auto_smoothing_low_speed" => app.set_motion_setting_native_checked(
            "auto_smoothing_low_speed",
            &LegacyValue::Int(i64::from(read::checked(value))),
        ),
        "invert_tl" => {
            app.set_setting_native_checked("PROSIMU", "invert_tl", &LegacyValue::Int(i64::from(read::checked(value))))
        }
        "pitch" | "roll" | "yaw" | "sway" | "surge" | "heave" | "traction_loss" | "traction_loss_front" => {
            let number = read::number(value)?;
            let projection = components::number(&json!(value));
            app.backend.assign_motion_test_value(control, &number, projection).map_err(|error| error.detail)?;
            refresh_controls(app);
            Ok(())
        }
        "belt_test_slider" => {
            let number = read::number(value)?;
            let projection = components::number(&json!(value));
            app.backend.assign_belt_test_value(&number, projection).map_err(|error| error.detail)?;
            refresh_controls(app);
            Ok(())
        }
        "belt_calibration_power_slider" => {
            app.set_setting_native_checked("BELT_TENSIONER", "belt_calibration_factor", &int(value)?)
        }
        "belt_calibration_offset_slider" => {
            app.set_setting_native_checked("BELT_TENSIONER", "belt_calibration_retraction", &int(value)?)
        }
        "rumble_model" => {
            let model = chosen(app, control, value)?;
            app.set_setting_native_checked("SIM3D", "model", &str_value(&model))
        }
        port if port.starts_with("rumble_port") => {
            let group = chosen(app, control, value)?;
            let key = port.trim_start_matches("rumble_");
            app.set_setting_native_checked("SIM3D", key, &str_value(&group))
        }
        "hud_model" => {
            let code = chosen(app, control, value)?;
            app.set_setting_native_checked("HUD", "model", &str_value(&code))
        }
        "iflag_dim_perc" => app.set_setting_native_checked("IFLAG", "brightness", &int(value)?),
        "iflag_mirror" => app.set_setting_native_checked("IFLAG", "mirror", &LegacyValue::Bool(read::checked(value))),
        "iflag_orientation" => {
            let degrees = chosen(app, control, value)?;
            app.set_setting_native_checked("IFLAG", "rotation", &LegacyValue::Int(degrees.as_i64().unwrap_or(0)))
        }
        _ => Err(format!("no Hardware value {control:?}")),
    }
}

fn refresh_controls(app: &mut MyApp) {
    if let Ok(controls) = app.backend.get("controls") {
        app.docs.controls = controls;
    }
}

/// The wind type: stored; a Blower limits the Wind power range to 35–90 %, the other types open it to
/// 0–100 %.
pub fn wind_type(app: &mut MyApp, code: &str) -> Result<(), String> {
    if !WIND_TYPES.iter().any(|(_, c)| *c == code) {
        return Err(format!("KeyError: {}", openrigcue_model::python_repr::repr_str(code)));
    }
    app.set_setting_native_checked("INTELLIBOX", "wind_type", &LegacyValue::Str(code.into()))?;
    let stored = setting_text(app, "INTELLIBOX", "wind_type");
    let (low, high) = if stored == "B" { (35.0, 90.0) } else { (0.0, 100.0) };
    app.set_tune_native_checked(&["power_wind_max_speed_perc"], &LegacyValue::Float(high))?;
    app.set_tune_native_checked(&["power_wind_min_speed_perc"], &LegacyValue::Float(low))
}

/// The fan count (`2+` stores `2`): one fan turns Wind Curving off with a curving effect of 1 (Tuning → Wind
/// then offers neither).
pub fn fan_count(app: &mut MyApp, count: &str) -> Result<(), String> {
    let count = count.replace('+', "");
    app.set_setting_native_checked("INTELLIBOX", "nbr_fans", &LegacyValue::Str(count))?;
    one_fan_rule(app)
}

fn one_fan_rule(app: &mut MyApp) -> Result<(), String> {
    if setting_text(app, "INTELLIBOX", "nbr_fans") == "1" {
        app.set_tune_native_checked(&["wind_curving"], &LegacyValue::Int(0))?;
        app.set_tune_native_checked(&["curving_effect"], &LegacyValue::Int(1))?;
    }
    Ok(())
}

/// The LED count: each Tuning → LED segment is kept within 1…count, then the count is stored.
pub fn led_count(app: &mut MyApp, count: i64) -> Result<(), String> {
    for index in 1..5 {
        clamp_segment(app, index, count)?;
    }
    app.set_setting_native_checked("LED", "nbr_leds", &LegacyValue::Int(count))
}

/// A LED segment's start and end put into its range (1…`count`) again, as at start ([`super::reload::range`]):
/// the start is kept within 1 and the end, the end within the start and `count` (so a start beyond the count
/// stays, above the end).
fn clamp_segment(app: &mut MyApp, index: usize, count: i64) -> Result<(), String> {
    let read = |app: &MyApp, field: &str| -> Result<i64, String> {
        let value =
            app.backend.native_setting("LED", &format!("segment{index}_{field}")).map_err(|error| error.detail)?;
        native_int(&value)
    };
    let (start, end) = (read(app, "start")?, read(app, "end")?);
    let scale = crate::slider::RangeScale::new(1.0, count as f64, 1.0);
    let (low, high) = super::reload::range(scale, &LegacyValue::Int(start), &LegacyValue::Int(end))?;
    let (low, high) = (low as i64, high as i64);
    if low != start {
        app.set_setting_native_checked("LED", &format!("segment{index}_start"), &LegacyValue::Int(low))?;
    }
    if high != end {
        app.set_setting_native_checked("LED", &format!("segment{index}_end"), &LegacyValue::Int(high))?;
    }
    Ok(())
}

/// A sound device was chosen: stored and opened; a failure is reported.
pub fn sound_device(app: &mut MyApp, device: &str) -> Result<(), String> {
    app.set_setting_native_checked("SHAKEBOX", "soundcard", &LegacyValue::Str(device.into()))?;
    open_sound_device(app);
    Ok(())
}

/// Opens the configured sound device; a failure is a warning with the sound system's reason.
fn open_sound_device(app: &mut MyApp) {
    let result = app.backend.apply_shaker_soundcard().map_err(|failure| failure.detail);
    sound_device_opened(app, result);
}

/// The outcome of opening the sound device: a failure is a warning, and the Sound Device row shows None until a
/// device opens.
fn sound_device_opened(app: &mut MyApp, result: Result<(), String>) {
    app.ui.devices.hardware.sound_device_failed = result.is_err();
    if let Err(detail) = result {
        let message = app
            .backend
            .translate("Could not open the selected sound device: ^error^")
            .replace("^error^", &app.backend.translate(&detail));
        notices::warning(app, "Alert", &message);
    }
}

/// A speaker layout was chosen: stored, its effects loaded and the surround set-up checked again.
pub fn choose_layout(app: &mut MyApp, name: &str) -> Result<(), String> {
    app.set_setting_native_checked("SHAKEBOX", "profile", &LegacyValue::Str(name.into()))?;
    app.backend.load_shaker_effects().map_err(|error| error.detail)?;
    app.backend.recheck_shakers_surround().map_err(|error| error.detail)
}

/// The sound sample choice; a refused change is reported (the switch shows the stored choice).
fn use_srs_sounds(app: &mut MyApp, enabled: bool) -> Result<(), String> {
    match app.backend.patch("settings", json!({"shakers": {"use_srs_sounds": enabled}})) {
        Ok(settings) => {
            crate::backend::set_at(&mut app.docs.settings, &["shakers", "use_srs_sounds"], json!(enabled));
            if settings.is_object() {
                app.docs.settings = settings;
            }
        }
        Err(failure) => notices::warning(app, "Alert", &failure.detail),
    }
    Ok(())
}

/// The SRS installer import: the service copies the sounds and firmware files from the given path.
fn srs_import(app: &mut MyApp) {
    let path = app.ui.devices.hardware.srs_path.trim().to_owned();
    if path.is_empty() {
        let message = tr(app, "Enter the path of the SRS installer (.exe) or installation folder.");
        notices::warning(app, "Alert", &message);
        return;
    }
    match app.backend.post("srs-import", json!({"path": path})) {
        Ok(_) => notices::success(
            app,
            "SRS Installer",
            "The SRS files were imported. The sound samples can be used now; voice prompts and firmware files are used from the next start of OpenRigCue.",
        ),
        Err(failure) => {
            let reason = app.backend.translate(&failure.detail);
            let message = format!("{}{reason}", tr(app, "Could not import the SRS files: "));
            let notice = notices::notice(notices::Severity::Error, &tr(app, "SRS Installer"), &message);
            notices::raise(app, notice);
        }
    }
}

/// A manufacturer was chosen: the Motion screen's refresh rate takes the manufacturer's default, then the
/// manufacturer's first model is chosen.
pub fn choose_manufacturer(app: &mut MyApp, manufacturer: &str) -> Result<(), String> {
    let row = crate::catalog::manufacturers()
        .into_iter()
        .find(|m| m.name == manufacturer)
        .ok_or_else(|| format!("KeyError: {}", openrigcue_model::python_repr::repr_str(manufacturer)))?;
    let first = crate::catalog::models().iter().find(|model| model.manufacturer == manufacturer);
    app.set_motion_setting_native_checked("refresh_rate", &LegacyValue::Int(i64::from(row.default_refresh_rate)))?;
    match first {
        Some(model) => choose_model(app, &model.name),
        None => Ok(()),
    }
}

/// A motion model was chosen: stored, then the rules it brings.
pub fn choose_model(app: &mut MyApp, model: &str) -> Result<(), String> {
    app.set_motion_setting_native_checked("model", &LegacyValue::Str(model.into()))?;
    model_rules(app)?;
    // The ports of the new model: a failed listing is reported.
    try_load_listing(app)
}

/// What the configured model sets elsewhere: a "2 TL" model offers the front traction-loss effect (others
/// zero its activation and vertical-actuator share and turn it off); a model without IntelliComp keeps the
/// Motion Compensation sensor at WITMotion.
pub fn model_rules(app: &mut MyApp) -> Result<(), String> {
    let name = model_name(app);
    let model = crate::catalog::model(&name)
        .ok_or_else(|| format!("KeyError: {}", openrigcue_model::python_repr::repr_str(&name)))?;
    app.backend.native_setting("MOTIONBOX", "axis_assignment").map_err(|error| error.detail)?;
    let two_tl = name.contains("2 TL");
    if two_tl {
        // The front activation threshold is put again into its number box, within the box's limits.
        let tune = app.backend.native_tune().map_err(|error| error.detail)?;
        let activation = crate::backend::native_field_owned(&tune, &["min_traction_loss_front_activation"])?;
        super::set_number(app, Screen::Motion, "min_traction_loss_front_activation", &activation)?;
    } else {
        app.set_tune_native_checked(&["min_traction_loss_front_activation"], &LegacyValue::Int(0))?;
        app.set_tune_native_checked(&["motion_main_actuators_traction_loss_front"], &LegacyValue::Int(0))?;
    }
    app.set_tune_native_checked(&["motion_switch", "traction_loss_front"], &LegacyValue::Bool(two_tl))?;
    if app.win32() && name != "None" && !model.enable_intellicomp {
        app.set_setting_native_checked("MOTION_COMPENSATION", "type", &LegacyValue::Str("W".into()))?;
    }
    Ok(())
}

// ---- Buttons --------------------------------------------------------------------------------------------------

/// A button of the screen: a row's button `index`, or the row's own button (`None`).
pub fn action(app: &mut MyApp, control: &str, index: Option<usize>) -> Result<(), String> {
    match control {
        "srs_import" => {
            srs_import(app);
            return Ok(());
        }
        "controller" => {
            device_dialogs::open_device(app, Device::Controller { wheel_id: magic_wheel_id(app), confirm: None });
            return Ok(());
        }
        "sweep_stop" => {
            if app.ui.devices.hardware.sweep.is_some() {
                end_sweep(app);
                reset_test_sliders(app)?;
            }
            return Ok(());
        }
        _ => {}
    }
    let rules = Rules::read(app);
    let page = page(app, &rules);
    let row = page.control(control).ok_or_else(|| format!("no Hardware button {control:?}"))?;
    let button = match index {
        Some(index) => row.buttons.get(index).cloned(),
        None => Some(row.widget.clone()),
    };
    let Some(Widget::Button { run, .. }) = button else { return Err(format!("no Hardware button {control:?}")) };
    run_action(app, &run)
}

/// What a Hardware button does.
fn run_action(app: &mut MyApp, run: &Action) -> Result<(), String> {
    match run {
        Action::HardwareTest { target } => {
            let test = match target.as_str() {
                "wind" => Test::Wind,
                "led" => Test::Led,
                "tach" => Test::Tach,
                other => return Err(format!("KeyError: {}", openrigcue_model::python_repr::repr_str(other))),
            };
            device_dialogs::open_test(app, test);
        }
        Action::IflagTest => device_dialogs::open_test(app, Test::Iflag),
        Action::HapticTest { wheel } => device_dialogs::open_test(app, Test::Haptic(i64::from(*wheel))),
        Action::RumbleTest { port } => device_dialogs::open_test(app, Test::Rumble(i64::from(*port))),
        Action::TestSpeaker { speaker } => device_dialogs::open_test(app, Test::Speaker(speaker.clone())),
        Action::SpeakerMatrix { speaker } => device_dialogs::open_matrix(app, speaker)?,
        Action::DuplicateLayout => device_dialogs::open_duplicate(app)?,
        Action::RemoveLayout => {
            let name = setting_text(app, "SHAKEBOX", "profile");
            device_dialogs::open_device(app, Device::Remove { name });
        }
        Action::MotionTestToggle => motion_test_toggle(app)?,
        Action::BeltTestToggle => belt_test_toggle(app)?,
        Action::AxisAssignment => device_dialogs::open_axis_assignment(app)?,
        Action::YawVrLimits => device_dialogs::open_yaw_vr(app)?,
        Action::YawVrCenter => super::motion::center(app)?,
        Action::ThanosSettings => device_dialogs::open_thanos(app)?,
        Action::VnmReverse => device_dialogs::open_vnm(app)?,
        Action::DofRealityKp => device_dialogs::open_dof(app)?,
        other => return Err(format!("unsupported Hardware action {other:?}")),
    }
    Ok(())
}

/// The Magic Wheel ID stored in the settings.
fn magic_wheel_id(app: &MyApp) -> String {
    text(app.setting(&["magicbox", "wheel_scan_code"]))
}

/// The motion test: on (with the measurement sweep when that custom feature is on) or off, which ends a running
/// measurement; every test slider goes back to 0 (with a measurement, once it has written its rows).
pub fn motion_test_toggle(app: &mut MyApp) -> Result<(), String> {
    let mode = app.backend.control_value(&["tests", "motion_test"]).map_err(|error| error.detail)?;
    let starting = native_equal(&mode, &LegacyValue::Int(0));
    app.set_control_checked(&["tests", "motion_test"], json!(starting))?;
    if !starting {
        end_sweep(app);
    } else if setting_flag(app, "CUSTOM_FEATURES", "enable_motion_measurement_mode") {
        // The sliders go back to 0 once the measurement has written its rows.
        return start_sweep(app, MEASUREMENT);
    }
    reset_test_sliders(app)
}

/// The belt test: Start turns the belt and its test on; Stop turns both off and clears the calibration. The
/// test slider goes back to 0.
pub fn belt_test_toggle(app: &mut MyApp) -> Result<(), String> {
    if control_int(app, &["tests", "belt_test"]) == 1 {
        app.set_control_checked(&["tests", "belt_homing"], json!(0))?;
        app.set_control_checked(&["switches", "belt"], json!(false))?;
        app.set_control_checked(&["tests", "belt_test"], json!(false))?;
    } else {
        app.set_control_checked(&["switches", "belt"], json!(true))?;
        app.set_control_checked(&["tests", "belt_test"], json!(true))?;
    }
    app.backend.assign_belt_test_value(&LegacyValue::Int(0), 0.0).map_err(|error| error.detail)?;
    refresh_controls(app);
    Ok(())
}

// ---- Device tests -----------------------------------------------------------------------------------------

/// The output switches of an Intellibox test.
fn intellibox_outputs(test: &Test) -> &'static [&'static str] {
    match test {
        Test::Wind => &["wind", "wind_center"],
        Test::Led => &["led"],
        Test::Tach => &["tach"],
        _ => &[],
    }
}

/// A stored output mode as the controls take it.
fn output_mode(value: &LegacyValue) -> Result<Value, String> {
    match value {
        LegacyValue::Int(0) => Ok(json!("off")),
        LegacyValue::Int(1) => Ok(json!("auto")),
        LegacyValue::Int(2) => Ok(json!("always_on")),
        value => crate::backend::native_json(value),
    }
}

/// Starts a device test: its outputs on, and a timer that ends it (seven seconds for the Intellibox outputs,
/// 1.5 s for the others). A speaker test plays its sound once.
pub fn start_test(app: &mut MyApp, test: &Test) -> Result<(), String> {
    if app.ui.devices.hardware.test.is_some() {
        return Ok(());
    }
    let mut previous = Vec::new();
    let duration = match test {
        Test::Wind | Test::Led | Test::Tach => {
            for switch in intellibox_outputs(test) {
                let value = app.backend.control_value(&["switches", switch]).map_err(|error| error.detail)?;
                previous.push(((*switch).to_owned(), value));
                app.set_control_checked(&["switches", switch], json!("always_on"))?;
            }
            INTELLIBOX_TEST
        }
        Test::Iflag => {
            app.set_control_checked(&["tests", "iflag_test"], json!(true))?;
            SHORT_TEST
        }
        Test::Haptic(wheel) => {
            app.set_control_checked(&["tests", "haptic_test"], json!(wheel))?;
            SHORT_TEST
        }
        Test::Rumble(port) => {
            app.set_control_checked(&["tests", "rumble_test"], json!(port))?;
            SHORT_TEST
        }
        Test::Speaker(speaker) => {
            if setting_text(app, "SHAKEBOX", "soundcard") != "None" {
                app.backend.test_shaker_speaker(speaker).map_err(|error| error.detail)?;
            }
            return Ok(());
        }
    };
    let state = &mut app.ui.devices.hardware;
    state.generation += 1;
    let generation = state.generation;
    state.test = Some(RunningTest { target: test.clone(), generation, previous });
    app.start_thread(
        "hardware_test",
        Box::new(move |weak: Weak<Mutex<MyApp>>| {
            std::thread::sleep(duration);
            if let Some(shared) = weak.upgrade() {
                if let Err(error) = finish_test_shared(&shared, generation) {
                    tracing::error!(%error, "device test");
                }
            }
        }),
    );
    super::super::refresh(app, &[Region::Main, Region::Dialog]);
    Ok(())
}

/// The run-time switches that end a device test, in order: its outputs back to what they were.
fn restore_writes(test: &RunningTest) -> Result<Vec<(Vec<String>, Value)>, String> {
    let path = |a: &str, b: &str| vec![a.to_owned(), b.to_owned()];
    Ok(match &test.target {
        Test::Wind | Test::Led | Test::Tach => test
            .previous
            .iter()
            .map(|(switch, value)| Ok((path("switches", switch), output_mode(value)?)))
            .collect::<Result<_, String>>()?,
        Test::Iflag => vec![(path("tests", "iflag_test"), json!(false))],
        Test::Haptic(_) => vec![(path("tests", "haptic_test"), json!(0))],
        Test::Rumble(_) => vec![(path("tests", "rumble_test"), json!(0))],
        Test::Speaker(_) => Vec::new(),
    })
}

/// Ends the running device test: its outputs back to what they were.
pub fn finish_test(app: &mut MyApp) -> Result<(), String> {
    let Some(test) = app.ui.devices.hardware.test.take() else { return Ok(()) };
    let result = (|| -> Result<(), String> {
        for (path, value) in restore_writes(&test)? {
            let path: Vec<&str> = path.iter().map(String::as_str).collect();
            app.set_control_checked(&path, value)?;
        }
        Ok(())
    })();
    super::super::refresh(app, &[Region::Main, Region::Dialog]);
    result
}

/// [`finish_test`] from the test's own thread, for the test numbered `generation` only: the service writes run
/// without holding the application, so the browser and the timers are not held up by the device.
fn finish_test_shared(shared: &Mutex<MyApp>, generation: u64) -> Result<(), String> {
    let (test, backend) = {
        let mut app = crate::host::lock(shared);
        if !app.ui.devices.hardware.test.as_ref().is_some_and(|test| test.generation == generation) {
            return Ok(());
        }
        let test = app.ui.devices.hardware.test.take().expect("the running test");
        (test, app.backend.clone())
    };
    let mut written = Vec::new();
    let result = (|| -> Result<(), String> {
        for (path, value) in restore_writes(&test)? {
            let parts: Vec<&str> = path.iter().map(String::as_str).collect();
            let controls = backend
                .patch("controls", crate::backend::patch_for(&parts, value.clone()))
                .map_err(|failure| failure.detail)?;
            written.push((path, value, controls));
        }
        Ok(())
    })();
    let mut app = crate::host::lock(shared);
    for (path, value, controls) in written {
        if controls.is_object() {
            app.docs.controls = controls;
        } else {
            let parts: Vec<&str> = path.iter().map(String::as_str).collect();
            crate::backend::set_at(&mut app.docs.controls, &parts, value);
        }
    }
    super::super::refresh(&mut app, &[Region::Main, Region::Dialog]);
    result
}

// ---- The measurement sweep ----------------------------------------------------------------------------------

/// What the measurement sweep goes through: the yaw / roll / pitch values (every combination), how long each
/// combination is held before its row is measured, and the laser reading of a row.
#[derive(Clone, Copy)]
struct SweepPlan {
    values: &'static [i64],
    hold: Duration,
    laser: fn() -> String,
}

/// The measurement: every combination of −100…100 in steps of 20, each held for 0.7 s.
const MEASUREMENT: SweepPlan = SweepPlan {
    values: &[-100, -80, -60, -40, -20, 0, 20, 40, 60, 80, 100],
    hold: Duration::from_millis(700),
    laser: laser_reading,
};

/// Every motion test slider goes back to 0.
fn reset_test_sliders(app: &mut MyApp) -> Result<(), String> {
    for key in MOTION_TEST_KEYS {
        app.set_control_checked(&["tests", "motion_sliders", key], json!(0))?;
    }
    Ok(())
}

/// Whether `sweep` is the measurement the screen shows (a stopped one no longer is).
fn current_sweep(app: &MyApp, stop: &Arc<AtomicBool>) -> bool {
    app.ui.devices.hardware.sweep.as_ref().is_some_and(|sweep| Arc::ptr_eq(&sweep.stop, stop))
}

/// Ends the running measurement, if one runs: it stops before it moves the sliders again or writes another row,
/// and a new one can start at once.
fn end_sweep(app: &mut MyApp) {
    if let Some(sweep) = app.ui.devices.hardware.sweep.take() {
        sweep.stop.store(true, Ordering::SeqCst);
    }
}

/// Starts the measurement sweep on its own thread: every yaw / roll / pitch combination, each row logged with
/// the actuator positions and a laser reading to `data_<timestamp>.csv` in the documents folder. A measurement
/// that still runs is ended first: one runs at a time.
fn start_sweep(app: &mut MyApp, plan: SweepPlan) -> Result<(), String> {
    use std::io::Write;
    end_sweep(app);
    let documents = text(app.status(&["documents"]));
    let path = std::path::Path::new(&documents).join(format!("data_{}.csv", local_timestamp()));
    let mut headers: Vec<String> = ["yaw", "roll", "pitch"].iter().map(|k| format!("test_slider_{k}")).collect();
    headers.extend((0..6).map(|i| format!("m{i}")));
    headers.push("bosch_cm".to_owned());
    let mut file = std::fs::File::create(&path).map_err(|error| format!("OSError: {error}: {}", path.display()))?;
    file.write_all(format!("{}\n", headers.join(",")).as_bytes()).map_err(|error| format!("OSError: {error}"))?;
    let stop = Arc::new(AtomicBool::new(false));
    let total = plan.values.len().pow(3);
    let path = path.display().to_string();
    app.ui.devices.hardware.sweep = Some(Sweep { row: 0, total, path: path.clone(), stop: stop.clone() });
    app.start_thread("motion_measurement", Box::new(move |weak| sweep(weak, file, stop, path, plan)));
    Ok(())
}

/// How a measurement ended.
#[derive(Debug, PartialEq, Eq)]
enum SweepEnd {
    /// Every row was written.
    Finished,
    /// The test or the measurement was stopped.
    Stopped,
    /// A slider or a row could not be written (why).
    Failed(String),
}

/// The measurement thread. Each combination is set on the yaw, roll and pitch test sliders, held, then its row
/// is written and synced to the disk. Whether it was stopped is looked at while the application is held, before
/// the sliders move and before a row is written, so a stopped measurement changes nothing after the stop. Once
/// every row is written, the test sliders go back to 0.
fn sweep(weak: Weak<Mutex<MyApp>>, mut file: std::fs::File, stop: Arc<AtomicBool>, path: String, plan: SweepPlan) {
    use std::io::Write;
    let total = plan.values.len().pow(3);
    let mut row = 0;
    let stopped = || stop.load(Ordering::SeqCst);
    let end = 'sweep: {
        for &yaw in plan.values {
            for &roll in plan.values {
                for &pitch in plan.values {
                    let combo = [yaw, roll, pitch];
                    {
                        let Some(shared) = weak.upgrade() else { return };
                        let mut app = crate::host::lock(&shared);
                        if stopped() {
                            break 'sweep SweepEnd::Stopped;
                        }
                        for (key, value) in ["yaw", "roll", "pitch"].iter().zip(combo) {
                            if let Err(error) = app.set_control_checked(&["tests", "motion_sliders", key], json!(value))
                            {
                                tracing::error!(%error, "measurement");
                                break 'sweep SweepEnd::Failed(error);
                            }
                        }
                    }
                    std::thread::sleep(plan.hold);
                    let laser = (plan.laser)();
                    let Some(shared) = weak.upgrade() else { return };
                    let mut app = crate::host::lock(&shared);
                    if stopped() {
                        break 'sweep SweepEnd::Stopped;
                    }
                    let motors = motor_positions(&app);
                    let mut line: Vec<String> = combo.iter().map(ToString::to_string).collect();
                    line.extend(motors);
                    line.push(laser);
                    let written = file
                        .write_all(format!("{}\n", line.join(",")).as_bytes())
                        .and_then(|()| file.flush())
                        .and_then(|()| file.sync_all());
                    if let Err(error) = written {
                        tracing::error!(%error, "measurement");
                        break 'sweep SweepEnd::Failed(format!("OSError: {error}"));
                    }
                    row += 1;
                    if let Some(sweep) = app.ui.devices.hardware.sweep.as_mut().filter(|s| Arc::ptr_eq(&s.stop, &stop))
                    {
                        sweep.row = row;
                    }
                    super::super::refresh(&mut app, &[Region::Main]);
                }
            }
        }
        SweepEnd::Finished
    };
    let Some(shared) = weak.upgrade() else { return };
    let mut app = crate::host::lock(&shared);
    if current_sweep(&app, &stop) {
        app.ui.devices.hardware.sweep = None;
        if end == SweepEnd::Finished {
            if let Err(error) = reset_test_sliders(&mut app) {
                tracing::error!(%error, "measurement");
            }
        }
    }
    if let SweepEnd::Failed(error) = &end {
        // A failed measurement reports its error as a failed action does, and no success.
        let message = app.backend.translate("An internal error occurred: ^detail^").replace("^detail^", error);
        notices::warning(&mut app, "Alert", &message);
    } else {
        let message = tf(
            &app,
            "^rows^ of ^total^ rows were written to ^path^",
            &[("rows", &row.to_string()), ("total", &total.to_string()), ("path", &path)],
        );
        let notice = notices::notice(notices::Severity::Success, &tr(&app, "Measurement"), &message);
        notices::raise(&mut app, notice);
    }
    super::super::refresh(&mut app, &[Region::Main]);
}

/// The actuator positions of one sweep row (0 when missing).
fn motor_positions(app: &MyApp) -> Vec<String> {
    let actuators = app.backend.motion_parameter("actuators_position").unwrap_or(LegacyValue::None);
    (0..6)
        .map(|index| {
            actuators
                .as_dict()
                .and_then(|fields| fields.get(&format!("m{index}")))
                .map_or_else(|| "0".to_owned(), openrigcue_model::python_repr::str_value)
        })
        .collect()
}

/// One reading of the laser distance meter's local service (`GET` to UDP 127.0.0.1:5005, 4 s timeout).
fn laser_reading() -> String {
    let reading = || -> std::io::Result<String> {
        let socket = std::net::UdpSocket::bind("0.0.0.0:0")?;
        socket.set_read_timeout(Some(Duration::from_secs(4)))?;
        socket.send_to(b"GET", "127.0.0.1:5005")?;
        let mut data = [0u8; 1024];
        let (len, _) = socket.recv_from(&mut data)?;
        Ok(String::from_utf8_lossy(&data[..len]).into_owned())
    };
    reading().unwrap_or_else(|error| {
        tracing::warn!(%error, "laser reading");
        "0.0".to_owned()
    })
}

/// The local time as `YYYYMMDD_HHMMSS`.
fn local_timestamp() -> String {
    match std::process::Command::new("date").arg("+%Y%m%d_%H%M%S").output() {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        _ => std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs().to_string())
            .unwrap_or_default(),
    }
}

// ---- The main loop ----------------------------------------------------------------------------------------

/// Once, before the main loop's first pass: the configured sound device is opened, the speaker layout's
/// effects are loaded, a missing motion model becomes the first listed one and the model's rules apply, the
/// belt calibration sliders take their settings again, the Motion refresh rate is kept within its range
/// ([`super::motion::start`]), the LED segments are kept within the LED count, the
/// one-fan rule applies, then the iFlag and LED brightness sliders take their settings again.
pub fn start(app: &mut MyApp) -> Result<(), String> {
    open_sound_device(app);
    app.backend.load_shaker_effects().map_err(|error| error.detail)?;
    app.backend.recheck_shakers_surround().map_err(|error| error.detail)?;
    let model = (|| -> Result<(), String> {
        if model_name(app) == "None" {
            let manufacturer = crate::catalog::model("None").map(|m| m.manufacturer.clone()).unwrap_or_default();
            if let Some(first) = models_of(app, &manufacturer).first() {
                app.set_motion_setting_native_checked("model", &LegacyValue::Str(first.name.clone()))?;
            }
        }
        model_rules(app)
    })();
    if let Err(error) = model {
        // A configured model whose rules fail (an unknown name, a value that is no text) is replaced by `None`.
        tracing::warn!(%error, "motion model at start");
        let fallback = app
            .set_motion_setting_native_checked("model", &LegacyValue::Str("None".into()))
            .and_then(|()| model_rules(app));
        if let Err(error) = fallback {
            tracing::warn!(%error, "motion model None at start");
        }
    }
    // The belt calibration power and retraction, each put again into its slider as a whole number.
    slider_at_start(app, "belt_calibration_power_slider", "BELT_TENSIONER", "belt_calibration_factor")?;
    slider_at_start(app, "belt_calibration_offset_slider", "BELT_TENSIONER", "belt_calibration_retraction")?;
    // The Motion screen's refresh rate, kept within its range.
    super::motion::start(app)?;
    // The LED segment count, then the LED count, each put again into its number box, so a stored count outside the
    // box's limits is stored within them. A segment count that cannot be read or stored is logged and left.
    if let Err(error) = segment_count_at_start(app) {
        tracing::warn!(%error, "LED segment count at start");
    }
    let count = app.backend.native_setting("LED", "nbr_leds").map_err(|error| error.detail)?;
    super::set_number(app, Screen::Hardware, "nbr_leds", &LegacyValue::Int(native_int(&count)?))?;
    one_fan_rule(app)?;
    // The iFlag brightness, then the LED brightness, each put again into its slider as a whole number.
    slider_at_start(app, "iflag_dim_perc", "IFLAG", "brightness")?;
    slider_at_start(app, "brightness", "LED", "brightness")
}

/// A stored setting put again into its slider at start as a whole number, so a value outside the slider's range
/// or off its step is stored as the slider holds it.
fn slider_at_start(app: &mut MyApp, control: &str, section: &str, key: &str) -> Result<(), String> {
    let stored = app.backend.native_setting(section, key).map_err(|error| error.detail)?;
    super::reload::put_into_slider(app, Screen::Hardware, control, native_int(&stored)?)
}

/// The stored LED segment count put into its number box (Tuning → LED) again and stored as the box takes it.
fn segment_count_at_start(app: &mut MyApp) -> Result<(), String> {
    let stored = app.backend.native_setting("LED", "nbr_segments").map_err(|error| error.detail)?;
    let count = native_int(&stored)?;
    let (number_box, _) = super::number_box(app, Screen::Led, "nbr_segments", "").ok_or("no LED segment count box")?;
    let count = number_box.take_whole(&LegacyValue::Int(count), &LegacyValue::Str(count.to_string()))?;
    app.set_setting_native_checked("LED", "nbr_segments", &LegacyValue::Int(count))
}

/// Each main-loop pass: a requested sound device refresh, and the test rules — the motion test stops (its
/// sliders at 0) while it cannot run, a running game stops the belt test, and the belt switched off ends its
/// test and calibration.
pub fn rules(app: &mut MyApp) -> Result<(), String> {
    let backend = app.backend.clone();
    if let Some(refresh) = read_sound_refresh(&*backend)? {
        take_sound_refresh(app, refresh)?;
    }
    test_rules(app)
}

/// A requested sound device refresh as the service answered it: the device list, then the outcome of opening the
/// configured device again. A list that cannot be read leaves the request for the next pass.
pub struct SoundRefresh {
    listing: Result<Value, String>,
    opened: Option<Result<(), String>>,
}

fn read_sound_refresh(backend: &dyn Backend) -> Result<Option<SoundRefresh>, String> {
    if !backend.soundcard_refresh_requested().map_err(|error| error.detail)? {
        return Ok(None);
    }
    let listing = backend.get("hardware").map_err(|failure| failure.detail);
    let opened = listing.is_ok().then(|| backend.apply_shaker_soundcard().map_err(|failure| failure.detail));
    Ok(Some(SoundRefresh { listing, opened }))
}

fn take_sound_refresh(app: &mut MyApp, refresh: SoundRefresh) -> Result<(), String> {
    match refresh.listing {
        Ok(listing) => app.ui.devices.hardware.listing = Some(listing),
        Err(error) => {
            tracing::warn!(%error, "sound device refresh: device list");
            return Ok(());
        }
    }
    if let Some(opened) = refresh.opened {
        sound_device_opened(app, opened);
    }
    app.backend.delete("shakers/refresh-soundcard").map_err(|error| error.detail)?;
    Ok(())
}

/// The test rules of each pass (see [`rules`]).
fn test_rules(app: &mut MyApp) -> Result<(), String> {
    let allowed = model_name(app) != "None" && motion_connected(app) && !game_running(app);
    if !allowed {
        // The test stops, and a measurement with it.
        end_sweep(app);
        for key in MOTION_TEST_KEYS {
            let value = app.backend.control_value(&["tests", "motion_sliders", key]).unwrap_or(LegacyValue::Int(0));
            if !native_equal(&value, &LegacyValue::Int(0)) {
                app.backend.assign_motion_test_value(key, &LegacyValue::Int(0), 0.0).map_err(|error| error.detail)?;
            }
        }
        app.set_control_checked(&["tests", "motion_test"], json!(false))?;
    }
    if game_running(app) {
        app.set_control_checked(&["tests", "belt_test"], json!(false))?;
        let value = app.backend.control_value(&["tests", "belt_test_value"]).unwrap_or(LegacyValue::Int(0));
        if !native_equal(&value, &LegacyValue::Int(0)) {
            app.backend.assign_belt_test_value(&LegacyValue::Int(0), 0.0).map_err(|error| error.detail)?;
        }
    }
    if control_int(app, &["switches", "belt"]) == 0 {
        app.set_control_checked(&["tests", "belt_test"], json!(false))?;
        app.set_control_checked(&["tests", "belt_homing"], json!(0))?;
    }
    Ok(())
}

// ---- The main loop's work for the Games and Hardware screens ---------------------------------------------

/// Once, before the main loop's first pass: the start-up work of every Games and Hardware screen (Motion
/// Compensation's first).
pub fn main_loop_start(app: &mut MyApp) -> Result<(), String> {
    super::compensation::start(app)?;
    start(app)?;
    super::games::start(app)?;
    super::telemetry::start(app)?;
    super::joystick::start(app)
}

/// Each main-loop pass, before the queued shortcut actions are cleared: the per-pass rules, writes and hot keys
/// of every Games and Hardware screen.
pub fn main_loop_rules(app: &mut MyApp) -> Result<(), String> {
    rules(app)?;
    super::games::rules(app)?;
    super::telemetry::rules(app)?;
    super::joystick::rules(app)?;
    super::compensation::rules(app)
}

/// [`main_loop_start`] on the main loop's shared application.
pub(crate) fn main_loop_start_shared(shared: &Mutex<MyApp>) -> Result<(), String> {
    main_loop_start(&mut crate::host::lock(shared))
}

/// [`main_loop_rules`] on the main loop's shared application.
pub(crate) fn main_loop_rules_shared(shared: &Mutex<MyApp>) -> Result<(), String> {
    // The sound device list and the device's reopening are read without holding the application.
    let backend = crate::host::lock(shared).backend.clone();
    let refresh = read_sound_refresh(&*backend)?;
    {
        let mut app = crate::host::lock(shared);
        if let Some(refresh) = refresh {
            take_sound_refresh(&mut app, refresh)?;
        }
        test_rules(&mut app)?;
        super::games::rules(&mut app)?;
    }
    super::telemetry::rules_shared(shared)?;
    // The joystick's live pass runs on its own thread (see `joystick::start`).
    super::compensation::rules_shared(shared)
}

#[cfg(test)]
#[path = "hardware_tests.rs"]
mod tests;
