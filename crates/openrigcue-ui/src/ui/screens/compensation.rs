//! Hardware → Motion Compensation: the sensor and its axes (enable, direction, factor, calibration), the rig's
//! settings, the VR status (rig-pose writer, consumer, driver, sensor, published pose and rates, the live axis
//! values, Recenter and the VR component set-up with install / uninstall), the "OpenVR setup" dialog (centre of
//! rotation and pose export), "Calibrate sensor", the page's shortcuts and the shortcut actions.
//!
//! Every value is a `MOTION_COMPENSATION` setting; the VR status and settings are the service's
//! `compensation` document (`compensation/vr` for the settings), the component set-up is
//! `compensation/setup`. The live parts are read by [`rules`] on every main-loop pass while the screen is open.

use std::collections::BTreeSet;

use openrigcue_model::native_value::LegacyValue;
use serde_json::{json, Value};

use super::super::components::{self, RowState};
use super::super::dialogs_hardware::{self, Dialog as Shared};
use super::super::html::{esc, t, tf, tr};
use super::super::{dialogs, notices, read, themes, Region, Screen};
use crate::app::MyApp;
use crate::backend::{native_field_owned, python_float, Backend, BackendExt};
use crate::spec::{self, Control, Widget};

/// The axes in the order the shortcut actions are applied.
const LOOP_EFFECTS: [&str; 6] = ["pitch", "roll", "sway", "surge", "yaw", "heave"];

/// The axes in the order the status lists their values.
const AXES: [(&str, &str); 6] =
    [("pitch", "Pitch"), ("roll", "Roll"), ("yaw", "Yaw"), ("heave", "Heave"), ("sway", "Sway"), ("surge", "Surge")];

/// The VR component set-up: key, group, name.
const COMPONENTS: [(&str, &str, &str); 3] = [
    ("openxr", "openxr", "OpenXR"),
    ("steamvr", "steamvr", "SteamVR runtime"),
    ("steamvr_compensation", "steamvr", "SteamVR"),
];

/// The index of the VR status group in the group picker (after the page's Sensor and Settings groups).
pub const STATUS_GROUP: usize = 2;

/// What the screen keeps between passes (`app.ui.devices`).
#[derive(Debug, Default)]
pub struct State {
    /// The `compensation` document (VR status and settings), or why it could not be read.
    pub status: Option<Result<Value, String>>,
    /// The `compensation/setup` document, or why it could not be read.
    pub setup: Option<Result<Value, String>>,
    /// Each axis's live sensor value as text.
    pub sensor: Vec<(String, String)>,
    /// The components whose install / uninstall runs.
    pub busy: BTreeSet<String>,
}

/// The dialogs of this screen.
#[derive(Debug, Clone, PartialEq)]
pub enum Dialog {
    /// Calibrate an axis (`pitch`, `roll`, `yaw`) or all three (`all`).
    Calibrate { axis: String },
    /// The VR settings: the `compensation/vr` settings as last read.
    Vr { settings: Value },
}

fn detail(failure: crate::backend::Failure) -> String {
    failure.detail
}

/// The motion model's catalog entry.
fn model(app: &MyApp) -> Option<&'static crate::catalog::ModelFlags> {
    match app.backend.native_setting("MOTIONBOX", "model") {
        Ok(LegacyValue::Str(name)) => crate::catalog::model(&name),
        _ => None,
    }
}

fn model_name(app: &MyApp) -> String {
    match app.backend.native_setting("MOTIONBOX", "model") {
        Ok(LegacyValue::Str(name)) => name,
        _ => String::new(),
    }
}

/// The stored sensor kind: `W` (WITMotion) or `V` (IntelliComp).
fn sensor_kind(app: &MyApp) -> String {
    match app.backend.native_setting("MOTION_COMPENSATION", "type") {
        Ok(LegacyValue::Str(kind)) => kind,
        _ => match app.setting(&["compensation", "sensor"]).as_str() {
            Some("witmotion") => "W".into(),
            _ => "V".into(),
        },
    }
}

/// What the rows depend on, read once per render.
struct Rules {
    model: String,
    witmotion: bool,
    dof_reality: bool,
    /// The model offers the IntelliComp sensor; without it the sensor is WITMotion and cannot be changed.
    intellicomp: bool,
    surge: bool,
}

impl Rules {
    fn read(app: &MyApp) -> Rules {
        let flags = model(app);
        Rules {
            model: model_name(app),
            witmotion: sensor_kind(app) == "W",
            dof_reality: flags.is_some_and(|m| m.manufacturer == "DOF REALITY"),
            intellicomp: flags.is_none_or(|m| m.enable_intellicomp),
            surge: flags.is_some_and(|m| m.port_mapping.iter().any(|port| port == "Surge")),
        }
    }

    fn six_dof(&self) -> bool {
        matches!(self.model.as_str(), "H6" | "P6")
    }

    /// Whether an axis's rows show: yaw only on six-DOF rigs, heave on H6 / P6 / H4 / P4.
    fn axis_shown(&self, axis: &str) -> bool {
        match axis {
            "yaw" => self.six_dof(),
            "heave" => matches!(self.model.as_str(), "H6" | "P6" | "H4" | "P4"),
            _ => true,
        }
    }
}

/// The page description as shown.
fn page(app: &MyApp) -> spec::Page {
    spec::compensation_page(super::motion::six_dof(app))
}

fn axis_label(axis: &str) -> &'static str {
    AXES.iter().find(|(key, _)| *key == axis).map_or("", |(_, label)| label)
}

/// The state of a row, `None` when the rules hide it.
fn row_state(app: &MyApp, rules: &Rules, control: &Control) -> Option<RowState> {
    let id = control.id.as_str();
    let mut state = RowState::default();
    if let Some(field) = &control.field {
        state.value = read::field(app, field);
    }
    if let Some(axis) = AXES.iter().map(|(key, _)| *key).find(|axis| {
        id == *axis || id.strip_suffix("_reverse") == Some(axis) || id.strip_suffix("_factor") == Some(axis)
    }) {
        if !rules.axis_shown(axis) {
            return None;
        }
        if id == axis {
            // Calibration needs the WITMotion sensor.
            state.buttons = Some(if rules.witmotion { vec![0] } else { Vec::new() });
        } else if id.ends_with("_reverse") {
            state.label = Some(tf(app, "^axis^ direction", &[("axis", &tr(app, axis_label(axis)))]));
        } else {
            state.label = Some(tf(app, "^axis^ factor", &[("axis", &tr(app, axis_label(axis)))]));
        }
    }
    match id {
        "sensor_type" => state.enabled = rules.intellicomp,
        "surge_horizontal_factor" | "surge_stroke" if !rules.surge => return None,
        "surge_horizontal_reverse" => {
            if !rules.surge {
                return None;
            }
            state.label = Some(t(app, "Horizontal direction"));
        }
        "installation_mode" if !rules.witmotion => return None,
        "actuator_distance_left_right" | "actuator_distance_front_back" if rules.witmotion || rules.dof_reality => {
            return None
        }
        "vertical_actuators_stroke" if !rules.witmotion && rules.dof_reality => return None,
        _ => {}
    }
    Some(state)
}

/// The body: the chosen group's rows (the VR status group after the page's own), its tools and the actions.
pub fn main(app: &MyApp) -> String {
    let rules = Rules::read(app);
    let page = page(app);
    let mut groups: Vec<(usize, String)> =
        page.sections.iter().enumerate().map(|(i, s)| (i, tr(app, &s.title))).collect();
    groups.push((STATUS_GROUP, tr(app, "VR status")));
    let selected = if app.ui.section <= STATUS_GROUP { app.ui.section } else { 0 };
    let (title, mut body) = if selected == STATUS_GROUP {
        ("VR status", status_body(app))
    } else {
        let section = &page.sections[selected];
        let rows = super::rows(app, Screen::Compensation, &section.controls, "", |c| row_state(app, &rules, c));
        let mut body = components::more(app, &rows, &format!("compensation:{selected}"));
        if selected == 0 {
            body.push_str(&components::field(
                &t(app, "Runtime"),
                &format!(
                    "<div class=\"device action-row\">{}{}</div>",
                    components::button(&t(app, "OpenVR setup"), "action", "Motion Compensation:vr-setup:", ""),
                    components::button(&t(app, "Calibrate sensor"), "action", "Motion Compensation:calibrate-all:", "")
                ),
                "",
            ));
        }
        (section.title.as_str(), body)
    };
    let header: String = page
        .header
        .iter()
        .enumerate()
        .filter_map(|(i, button)| match button {
            Widget::Button { text: label, .. } => {
                Some(components::button(&t(app, label), "action", &format!("Motion Compensation:@header:{i}"), ""))
            }
            _ => None,
        })
        .collect();
    body.push_str(&components::actions(app, &header, "compensation"));
    components::panel(&themes::title(app, title), &super::section_picker(app, &groups, selected), &body, "")
}

fn number(value: &Value) -> String {
    value.as_f64().map(|value| format!("{value:.4}")).unwrap_or_else(|| "—".to_owned())
}

/// The six values of a pose as one line.
fn pose(app: &MyApp, value: &Value) -> String {
    if value.is_null() {
        return tr(app, "No sample");
    }
    ["sway", "surge", "heave", "yaw", "roll", "pitch"].into_iter().fold(
        tr(app, "sway: ^sway^  surge: ^surge^  heave: ^heave^  yaw: ^yaw^  roll: ^roll^  pitch: ^pitch^"),
        |line, key| line.replace(&format!("^{key}^"), &number(&value[key])),
    )
}

/// The VR status lines: writer, consumer, driver, sensor, published pose and rates.
pub fn status_lines(app: &MyApp, status: &Value) -> [String; 6] {
    let writer = &status["rig_pose"];
    let consumer = &status["consumer"];
    let imu = &status["imu"];
    let published = &writer["published"];
    let tx = |text: &str| tr(app, text);
    let mut writer_text = format!(
        "{}; {:.1} Hz; {}{}",
        tx(if writer["writing"] == true {
            "Writing"
        } else if writer["running"] == true {
            "Waiting"
        } else {
            "Stopped"
        }),
        writer["rate_hz"].as_f64().unwrap_or(0.0),
        writer["path"].as_str().unwrap_or(""),
        writer["error"].as_str().map(|error| format!("; {}", tx(error))).unwrap_or_default(),
    );
    if let Some(enabled) = published["enabled"].as_bool() {
        writer_text.push_str(&format!("; {}", tx(if enabled { "enabled" } else { "disabled" })));
    }
    if let Some(age) = published["age_ms"].as_f64() {
        writer_text.push_str(&format!("; {}", tx("sample ^age^ ms old").replace("^age^", &format!("{age:.1}"))));
    }
    let consumer_text = if consumer.is_null() {
        tx("None")
    } else {
        format!(
            "{}; PID {}; {}",
            consumer["kind"].as_str().map_or_else(|| tx("Unknown"), str::to_owned),
            consumer["pid"],
            tx(if consumer["live"] == true { "Live" } else { "Stale" }),
        )
    };
    let driver_text = status["steamvr_driver"]
        .as_object()
        .map(|fields| {
            fields
                .iter()
                .map(|(key, value)| format!("{key}: {}", value.as_str().unwrap_or("")))
                .collect::<Vec<_>>()
                .join("; ")
        })
        .unwrap_or_else(|| tx("No status file"));
    let mut sensor_text = format!(
        "{} {}; X: {}  Y: {}  Z: {}",
        tx(imu["connection"].as_str().unwrap_or("Unknown")),
        imu["port"].as_str().unwrap_or(""),
        number(&imu["angles"][0]),
        number(&imu["angles"][1]),
        number(&imu["angles"][2]),
    );
    if imu["rates"].is_array() {
        let gyro = tx("gyro X: ^x^  Y: ^y^  Z: ^z^ °/s")
            .replace("^x^", &number(&imu["rates"][0]))
            .replace("^y^", &number(&imu["rates"][1]))
            .replace("^z^", &number(&imu["rates"][2]));
        sensor_text.push_str(&format!("; {gyro}"));
    } else {
        sensor_text.push_str(&format!("; {}", tx("no gyro packets")));
    }
    if let Some(age) = imu["age_ms"].as_f64() {
        sensor_text.push_str(&format!("; {}", tx("^age^ ms old").replace("^age^", &format!("{age:.1}"))));
    }
    [
        writer_text,
        consumer_text,
        driver_text,
        sensor_text,
        pose(app, &published["values"]),
        pose(app, &published["rates"]),
    ]
}

/// A component's status text and its action (`Install`, `Uninstall` or none).
fn component(app: &MyApp, key: &str) -> (String, Option<&'static str>) {
    let setup = match &app.ui.devices.compensation.setup {
        None => return (tr(app, "Checking installation"), None),
        Some(Err(_)) => return (tr(app, "Status unavailable"), None),
        Some(Ok(setup)) => setup,
    };
    if key == "steamvr" {
        return (tr(app, if setup["steamvr_runtime"].is_string() { "Installed" } else { "Not installed" }), None);
    }
    let part = if key == "steamvr_compensation" { "steamvr" } else { "openxr" };
    let value = setup[part]["status"].as_str().unwrap_or("Status unavailable");
    // A value that carries a reason is one translated sentence with the reason filled in.
    let shown =
        [("Setup failed: ", "Setup failed: ^error^", "^error^"), ("Invalid: ", "Invalid: ^tracker^", "^tracker^")]
            .into_iter()
            .find_map(|(prefix, template, name)| {
                let rest = value.strip_prefix(prefix)?;
                Some(tr(app, template).replace(name, &tr(app, rest)))
            })
            .unwrap_or_else(|| tr(app, value));
    let action = match setup[part]["registered"].as_bool() {
        Some(true) => Some("Uninstall"),
        Some(false) if setup[part]["action"] == "Install" => Some("Install"),
        _ => None,
    };
    (shown, action)
}

/// The VR status group: the status lines, the live axis values, Recenter and the component set-up.
fn status_body(app: &MyApp) -> String {
    let state = &app.ui.devices.compensation;
    let titles = [
        "Rig-pose Writer",
        "Active Consumer",
        "SteamVR Driver",
        "Sensor Reading (°)",
        "Published Pose (m / rad)",
        "Rates (m/s / rad/s)",
    ];
    let lines: Vec<String> = match &state.status {
        None => titles.iter().map(|_| tr(app, "Waiting for status")).collect(),
        Some(Err(error)) => {
            let mut lines = vec![tr(app, "Status unavailable: ^error^").replace("^error^", &tr(app, error))];
            lines.extend(titles.iter().skip(1).map(|_| tr(app, "Waiting for status")));
            lines
        }
        Some(Ok(status)) => status_lines(app, status).to_vec(),
    };
    let mut rows: Vec<String> = titles
        .iter()
        .zip(&lines)
        .map(|(title, line)| {
            components::field(&t(app, title), &format!("<span class=\"mono\">{}</span>", esc(line)), "")
        })
        .collect();
    let values: String = AXES
        .iter()
        .map(|(key, label)| {
            let value = state.sensor.iter().find(|(k, _)| k == key).map_or("0", |(_, v)| v.as_str());
            format!("{} {}", tr(app, label), value)
        })
        .collect::<Vec<_>>()
        .join(" · ");
    rows.push(components::field(&t(app, "Axis values"), &format!("<span class=\"mono\">{}</span>", esc(&values)), ""));
    rows.push(components::field(
        &t(app, "Recenter"),
        &components::button(&t(app, "Recenter"), "action", "Motion Compensation:recenter:", ""),
        "",
    ));
    let mut body = rows.concat();
    for (group, heading) in [("openxr", "OpenXR"), ("steamvr", "SteamVR")] {
        body.push_str(&format!("<div class=\"control-row note-row\"><small>{}</small></div>", t(app, heading)));
        for (key, _, name) in COMPONENTS.iter().filter(|(_, g, _)| *g == group) {
            let (value, action) = component(app, key);
            let busy = state.busy.contains(*key);
            let button = match action {
                Some(label) => format!(
                    "<button type=\"button\" data-act=\"action\" data-arg=\"Motion Compensation:{}.{key}:\"{}>{}</button>",
                    if label == "Install" { "install" } else { "uninstall" },
                    if busy { " disabled aria-busy=\"true\"" } else { "" },
                    t(app, label)
                ),
                None => String::new(),
            };
            body.push_str(&components::field(&t(app, name), &format!("<span>{}</span> {button}", esc(&value)), ""));
        }
    }
    body.push_str(&format!(
        "<div class=\"device action-row\">{}</div>",
        components::button(&t(app, "Check again"), "action", "Motion Compensation:check:", "")
    ));
    body
}

/// Work when the screen opens: the live parts are read.
pub fn on_enter(app: &mut MyApp) {
    refresh_live(app);
}

/// The VR status, the component set-up and the axis values, as one read found them.
pub struct Live {
    status: Result<Value, String>,
    setup: Result<Value, String>,
    sensor: Vec<(String, String)>,
}

/// Reads the VR status, the component set-up and the axis values from the service.
pub fn read_live(backend: &dyn Backend) -> Live {
    let status = backend.get("compensation").map_err(detail);
    let setup = backend.get("compensation/setup").map_err(detail);
    let sensor = AXES
        .iter()
        .map(|(key, _)| {
            let value =
                backend.motion_sensor_value(key).map(|value| sensor_text(&value)).unwrap_or_else(|_| "0".into());
            ((*key).to_owned(), value)
        })
        .collect();
    Live { status, setup, sensor }
}

/// Takes a live read: the screen shows it, and an open VR dialog follows the stored centre of rotation and
/// export when they changed elsewhere.
pub fn apply_live(app: &mut MyApp, live: Live) {
    let settings = live.status.as_ref().ok().map(|status| status["settings"].clone()).filter(Value::is_object);
    let mut redraw = false;
    // The VR dialog follows them in front or held behind a Downloaded Profiles review (drawn when it comes back).
    let open = dialogs::open_dialogs_mut(app).find_map(|(dialog, front)| match dialog {
        dialogs::Dialog::Hardware(Shared::Compensation(Dialog::Vr { settings: shown })) => Some((shown, front)),
        _ => None,
    });
    if let (Some(settings), Some((shown, front))) = (settings, open) {
        if *shown != settings {
            *shown = settings;
            redraw = front;
        }
    }
    if redraw {
        super::super::refresh(app, &[Region::Dialog]);
    }
    let state = &mut app.ui.devices.compensation;
    state.status = Some(live.status);
    state.setup = Some(live.setup);
    state.sensor = live.sensor;
}

/// Reads the VR status, the component set-up and the axis values.
pub fn refresh_live(app: &mut MyApp) {
    let backend = app.backend.clone();
    let live = read_live(&*backend);
    apply_live(app, live);
}

/// An axis's sensor value rounded to four decimals, as text (`0` when it is not a number).
pub fn sensor_text(value: &LegacyValue) -> String {
    match value {
        LegacyValue::Int(value) => value.to_string(),
        LegacyValue::Bool(value) => if *value { "1" } else { "0" }.into(),
        LegacyValue::Float(x) if x.is_finite() => {
            let rounded: f64 = format!("{x:.4}").parse().unwrap_or(*x);
            crate::remi::py_float_str(rounded)
        }
        LegacyValue::Float(x) => crate::remi::py_float_str(*x),
        _ => "0".to_owned(),
    }
}

fn refresh_settings(app: &mut MyApp) {
    if let Ok(settings) = app.backend.get("settings") {
        app.docs.settings = settings;
    }
}

/// One field of an axis.
fn assign_axis(app: &mut MyApp, axis: &str, field: &str, value: &LegacyValue) -> Result<(), String> {
    app.backend.assign_mc_value(axis, field, value).map_err(detail)?;
    refresh_settings(app);
    Ok(())
}

/// One `MOTION_COMPENSATION` setting.
fn assign(app: &mut MyApp, key: &str, value: &LegacyValue) -> Result<(), String> {
    app.set_setting_native_checked("MOTION_COMPENSATION", key, value)
}

/// A value of the screen changed: the setting the row edits.
pub fn set(app: &mut MyApp, control: &str, part: &str, value: &str) -> Result<(), String> {
    if !part.is_empty() {
        return Err(format!("no Motion Compensation value {control:?}:{part}"));
    }
    // A spin box's or slider's number is stored as its whole part (a fraction is dropped, toward zero).
    let int = |value: &str| read::whole(value).map(LegacyValue::Int);
    let checked = LegacyValue::Bool(read::checked(value));
    if AXES.iter().any(|(axis, _)| *axis == control) {
        return assign_axis(app, control, "enabled", &checked);
    }
    if let Some(axis) = control.strip_suffix("_reverse").filter(|axis| AXES.iter().any(|(a, _)| a == axis)) {
        return assign_axis(app, axis, "reverse", &checked);
    }
    match control {
        "sensor_type" => {
            if !Rules::read(app).intellicomp {
                return Ok(());
            }
            let kind = match value {
                "0" => "W",
                "1" => "V",
                other => return Err(format!("KeyError: {other:?}")),
            };
            assign(app, "type", &LegacyValue::Str(kind.into()))
        }
        "pitch_factor"
        | "roll_factor"
        | "yaw_factor"
        | "sway_factor"
        | "surge_factor"
        | "surge_horizontal_factor"
        | "height" => assign(app, control, &int(value)?),
        "surge_horizontal_reverse" => assign(app, control, &checked),
        "swap" => {
            assign(app, "swap_roll_pitch", &LegacyValue::Str(if read::checked(value) { "yes" } else { "no" }.into()))
        }
        "installation_mode" => {
            let kind = match value {
                "0" => "H",
                "1" => "V",
                other => return Err(format!("KeyError: {other:?}")),
            };
            assign(app, "installation", &LegacyValue::Str(kind.into()))
        }
        "actuator_distance_left_right" => assign(app, "mm_distance_left_right", &int(value)?),
        "actuator_distance_front_back" => assign(app, "mm_distance_front_back", &int(value)?),
        "vertical_actuators_stroke" => assign(app, "vertical_actuators_stroke", &int(value)?),
        "surge_stroke" => assign(app, "surge_horizontal_stroke", &int(value)?),
        _ => Err(format!("no Motion Compensation value {control:?}")),
    }
}

/// Calibrates an axis (its offset becomes the negated raw sensor value) or, for `all`, yaw, roll and pitch.
pub fn calibrate(app: &mut MyApp, axis: &str) -> Result<(), String> {
    if axis == "all" {
        for axis in ["yaw", "roll", "pitch"] {
            calibrate(app, axis)?;
        }
        return Ok(());
    }
    app.backend.post(&format!("compensation/actions/{axis}_calibrate"), json!({})).map(|_| ()).map_err(detail)
}

/// A failed VR or calibration action: a warning with the service's reason.
fn alert(app: &mut MyApp, error: &str) {
    let message = tr(app, error);
    notices::warning(app, "Alert", &message);
}

/// A button of the screen.
pub fn action(app: &mut MyApp, control: &str, index: Option<usize>) -> Result<(), String> {
    if let Some(key) = control.strip_prefix("install.") {
        return setup_operation(app, key, false);
    }
    if let Some(key) = control.strip_prefix("uninstall.") {
        return setup_operation(app, key, true);
    }
    match control {
        "@header" => {
            let page = page(app);
            match index.and_then(|i| page.header.get(i)) {
                Some(Widget::Button { run: spec::Action::Shortcuts { actions }, .. }) => {
                    super::super::shortcuts::open(app, actions)
                }
                _ => Err(format!("no Motion Compensation header button {index:?}")),
            }
        }
        "pitch" | "roll" | "yaw" => {
            dialogs_hardware::open(app, Shared::Compensation(Dialog::Calibrate { axis: control.to_owned() }));
            Ok(())
        }
        "calibrate-all" => {
            dialogs_hardware::open(app, Shared::Compensation(Dialog::Calibrate { axis: "all".into() }));
            Ok(())
        }
        "vr-setup" => {
            let settings = match app.backend.get("compensation") {
                Ok(status) => status["settings"].clone(),
                Err(failure) => {
                    alert(app, &failure.detail);
                    Value::Null
                }
            };
            dialogs_hardware::open(app, Shared::Compensation(Dialog::Vr { settings }));
            Ok(())
        }
        "recenter" => {
            if let Err(failure) = app.backend.post("compensation/actions/recenter", json!({})) {
                alert(app, &failure.detail);
            }
            Ok(())
        }
        "check" => {
            refresh_live(app);
            Ok(())
        }
        _ => Err(format!("no Motion Compensation button {control:?}")),
    }
}

/// Installs or uninstalls a VR component on its own thread; the button is busy meanwhile.
fn setup_operation(app: &mut MyApp, key: &str, uninstall: bool) -> Result<(), String> {
    let part = match key {
        "openxr" => "openxr",
        "steamvr_compensation" => "steamvr",
        _ => return Err(format!("no VR component {key:?}")),
    };
    let expected = if uninstall { "Uninstall" } else { "Install" };
    if app.ui.devices.compensation.busy.contains(key) || component(app, key).1 != Some(expected) {
        return Ok(());
    }
    app.ui.devices.compensation.busy.insert(key.to_owned());
    let key = key.to_owned();
    let path = format!("compensation/setup/{part}{}", if uninstall { "/uninstall" } else { "" });
    let backend = app.backend.clone();
    app.start_thread(
        "compensation_setup",
        Box::new(move |weak| {
            let result = backend.post(&path, json!({}));
            if let Some(shared) = weak.upgrade() {
                finish_operation(&mut crate::host::lock(&shared), &key, result);
            }
        }),
    );
    Ok(())
}

/// An install / uninstall ended: the answer is the new set-up; a failure reads it again and says why.
pub fn finish_operation(app: &mut MyApp, key: &str, result: Result<Value, crate::backend::Failure>) {
    app.ui.devices.compensation.busy.remove(key);
    match result {
        Ok(setup) if setup.is_object() => app.ui.devices.compensation.setup = Some(Ok(setup)),
        Ok(_) => {
            let setup = app.backend.get("compensation/setup").map_err(detail);
            app.ui.devices.compensation.setup = Some(setup);
        }
        Err(failure) => {
            let setup = app.backend.get("compensation/setup").map_err(detail);
            app.ui.devices.compensation.setup = Some(setup);
            alert(app, &failure.detail);
        }
    }
    super::super::refresh(app, &[Region::Main]);
}

/// The seated height as the centre of rotation's "Below" distance; it must be finite.
fn seated_height(app: &MyApp) -> Result<Value, String> {
    let stored = app.setting(&["compensation", "height"]);
    let value = python_float(stored)?;
    if !value.is_finite() {
        return Err("Seated Height must be a finite distance in millimetres.".into());
    }
    Ok(if stored.is_number() { stored.clone() } else { json!(value) })
}

/// The dialog's markup.
pub fn dialog_render(app: &MyApp, dialog: &Dialog) -> String {
    match dialog {
        Dialog::Calibrate { axis } => {
            let title = if axis == "all" {
                t(app, "Calibrate sensor")
            } else {
                tf(app, "Calibrate ^axis^", &[("axis", &tr(app, axis_label(axis)))])
            };
            dialogs_hardware::frame(
                app,
                &title,
                &format!(
                    "<p class=\"dialog-text\">{}</p>",
                    t(app, "Hold the rig at its neutral position: the current sensor reading becomes the zero.")
                ),
                Some((&t(app, "Calibrate"), true)),
            )
        }
        Dialog::Vr { settings } => {
            let use_seated = settings["cor_down_mm"].is_null();
            let mut body =
                format!("<div class=\"control-row note-row\"><small>{}</small></div>", t(app, "Centre of Rotation"));
            for (key, label) in
                [("cor_forward_mm", "Forward (mm)"), ("cor_down_mm", "Below (mm)"), ("cor_right_mm", "Right (mm)")]
            {
                let value = if key == "cor_down_mm" && use_seated {
                    seated_height(app).ok().and_then(|v| v.as_f64())
                } else {
                    settings[key].as_f64()
                };
                let id = format!("vr-{key}");
                let disabled = if key == "cor_down_mm" && use_seated { " disabled" } else { "" };
                body.push_str(&components::field(
                    &format!("<label for=\"{id}\">{}</label>", t(app, label)),
                    &dialogs_hardware::input(
                        &id,
                        &tr(app, label),
                        key,
                        "number",
                        &value.map(|v| v.to_string()).unwrap_or_default(),
                        &format!(" step=\"any\"{disabled}"),
                    ),
                    "",
                ));
            }
            body.push_str(&components::field(
                &t(app, "Use Seated Height"),
                &dialogs_hardware::switch(
                    "vr-seated",
                    "seated",
                    use_seated,
                    &t(app, if use_seated { "On" } else { "Off" }),
                    true,
                ),
                &t(app, "Below follows the seated height of the Settings group."),
            ));
            let export = settings["srs_export"].as_bool().unwrap_or(true);
            body.push_str(&components::field(
                &t(app, "SRS Pose Export"),
                &dialogs_hardware::switch(
                    "vr-export",
                    "export",
                    export,
                    &t(app, if export { "On" } else { "Off" }),
                    true,
                ),
                "",
            ));
            dialogs_hardware::frame(app, &t(app, "OpenVR setup"), &body, None)
        }
    }
}

/// Writes VR settings; the answer is the new settings.
fn patch_vr(app: &mut MyApp, patch: Value) {
    match app.backend.patch("compensation/vr", patch.clone()) {
        Ok(settings) => {
            if let Some(Shared::Compensation(Dialog::Vr { settings: shown })) = dialogs_hardware::current_mut(app) {
                if settings.is_object() {
                    *shown = settings;
                } else if let (Some(shown), Some(patch)) = (shown.as_object_mut(), patch.as_object()) {
                    shown.extend(patch.clone());
                } else {
                    *shown = patch;
                }
            }
            super::super::refresh(app, &[Region::Dialog]);
        }
        Err(failure) => alert(app, &failure.detail),
    }
}

/// An action of the open dialog.
pub fn dialog_act(app: &mut MyApp, dialog: Dialog, act: &str, arg: &str, value: &str) -> Result<(), String> {
    match (dialog, act) {
        (_, "dialog-close") => {
            dialogs::close(app);
            Ok(())
        }
        (Dialog::Calibrate { axis }, "dialog-ok") => {
            dialogs::close(app);
            if let Err(error) = calibrate(app, &axis) {
                alert(app, &error);
                return Err(error);
            }
            notices::confirmation(app, "Calibration complete", "The sensor's current position is now its zero.");
            Ok(())
        }
        (Dialog::Vr { .. }, "dialog-field") => {
            match arg {
                "cor_forward_mm" | "cor_down_mm" | "cor_right_mm" => match value.trim().parse::<f64>() {
                    Ok(number) if number.is_finite() => patch_vr(app, json!({ arg: number })),
                    _ => {
                        alert(app, "Enter a finite distance in millimetres.");
                        super::super::refresh(app, &[Region::Dialog]);
                    }
                },
                "seated" => {
                    let down = if read::checked(value) {
                        Value::Null
                    } else {
                        match seated_height(app) {
                            Ok(height) => height,
                            Err(error) => {
                                alert(app, &error);
                                super::super::refresh(app, &[Region::Dialog]);
                                return Ok(());
                            }
                        }
                    };
                    patch_vr(app, json!({ "cor_down_mm": down }));
                }
                "export" => patch_vr(app, json!({ "srs_export": read::checked(value) })),
                _ => {}
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// The Settings number boxes put again at start-up, in this order: each row's control and its
/// `MOTION_COMPENSATION` setting.
const START_NUMBERS: [(&str, &str); 5] = [
    ("height", "height"),
    ("surge_stroke", "surge_horizontal_stroke"),
    ("vertical_actuators_stroke", "vertical_actuators_stroke"),
    ("actuator_distance_left_right", "mm_distance_left_right"),
    ("actuator_distance_front_back", "mm_distance_front_back"),
];

/// The factor sliders put again at start-up, after [`START_NUMBERS`], in this order; the last three only on a
/// six-DOF platform (H6 or P6), which alone has them.
const START_FACTORS: [&str; 6] =
    ["surge_factor", "surge_horizontal_factor", "sway_factor", "yaw_factor", "roll_factor", "pitch_factor"];

/// Once, before the main loop's first pass (under the Windows rules): each Settings number box, then each factor
/// slider, takes its stored setting as a whole number again, so a stored value outside the control's limits (or
/// off the slider's step) is stored as the control holds it. A setting that cannot be read, is no whole number
/// or cannot be stored is logged and left as it is; the start goes on.
pub fn start(app: &mut MyApp) -> Result<(), String> {
    if !app.win32() {
        return Ok(());
    }
    for (control, key) in START_NUMBERS {
        let corrected = app
            .backend
            .native_setting("MOTION_COMPENSATION", key)
            .map_err(detail)
            .and_then(|stored| crate::backend::native_int(&stored))
            .and_then(|value| super::set_number(app, Screen::Compensation, control, &LegacyValue::Int(value)));
        if let Err(error) = corrected {
            tracing::warn!(%error, key, "Motion Compensation setting at start");
        }
    }
    let six_dof = super::motion::six_dof(app);
    for (index, key) in START_FACTORS.into_iter().enumerate() {
        if index >= 3 && !six_dof {
            continue;
        }
        let corrected = app
            .backend
            .native_setting("MOTION_COMPENSATION", key)
            .map_err(detail)
            .and_then(|stored| crate::backend::native_int(&stored))
            .and_then(|value| super::reload::put_into_slider(app, Screen::Compensation, key, value));
        if let Err(error) = corrected {
            tracing::warn!(%error, key, "Motion Compensation factor at start");
        }
    }
    Ok(())
}

/// Each main-loop pass: the shortcut actions (a pending pitch / roll / yaw / all calibration calibrates, a
/// pending axis switch toggles that axis), then, while the screen is open, the live parts are read again.
pub fn rules(app: &mut MyApp) -> Result<(), String> {
    let backend = app.backend.clone();
    apply_shortcuts(app, &|action| backend.ui_action_pending(action))?;
    if app.ui.mounted && app.ui.screen == Screen::Compensation {
        refresh_live(app);
    }
    Ok(())
}

/// [`rules`] on the main loop's shared application: the live status is read without holding it, so a slow VR
/// service does not hold up the browser's events or the timers.
pub(crate) fn rules_shared(shared: &std::sync::Mutex<MyApp>) -> Result<(), String> {
    let (shown, backend) = {
        let mut app = crate::host::lock(shared);
        let backend = app.backend.clone();
        apply_shortcuts(&mut app, &|action| backend.ui_action_pending(action))?;
        (app.ui.mounted && app.ui.screen == Screen::Compensation, backend)
    };
    if shown {
        let live = read_live(&*backend);
        let mut app = crate::host::lock(shared);
        if app.ui.screen == Screen::Compensation {
            apply_live(&mut app, live);
        }
    }
    Ok(())
}

/// The shortcut actions with `pending` telling which are queued.
pub fn apply_shortcuts(app: &mut MyApp, pending: &dyn Fn(&str) -> bool) -> Result<(), String> {
    for axis in ["pitch", "roll", "yaw"] {
        if pending(&format!("motion_compensation_{axis}_calibrate")) || pending("motion_compensation_all_calibrate") {
            // A failed calibration is reported; the later calibrations of the pass are not made.
            if let Err(error) = calibrate(app, axis) {
                alert(app, &error);
                break;
            }
        }
    }
    for axis in LOOP_EFFECTS {
        if pending(&format!("motion_compensation_{axis}_switch")) {
            // The stored `enabled` is tested for truth as it is stored.
            let stored = app.backend.native_setting("MOTION_COMPENSATION", axis).map_err(detail)?;
            let enabled = !native_field_owned(&stored, &["enabled"])?.is_truthy();
            assign_axis(app, axis, "enabled", &LegacyValue::Bool(enabled))?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "compensation_tests.rs"]
mod tests;
