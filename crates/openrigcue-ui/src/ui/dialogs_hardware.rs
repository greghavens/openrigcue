//! The dialogs of the Games and Hardware screens: the game set-up, the joystick assignment, the VR set-up and
//! every device dialog of Hardware → Hardware. Each screen module keeps its own dialog type and draws and runs
//! it; this module routes the shared dialog region and its actions to it.
//!
//! Actions inside these dialogs use the shared names: `dialog-field` (a field changed: name, value),
//! `dialog-button` (a button inside the dialog: its argument), `dialog-ok` (the confirm button) and
//! `dialog-close` (Cancel, Escape or ×).
//!
//! The Hardware → Hardware dialogs are written here ([`Device`]): the output test, the speaker layout copy and
//! removal, the speaker matrix, the motion controllers' dialogs (Axis Assignment, Yaw VR, Thanos / PT-ACTUATOR
//! with the Thanos4U settings and the maximum-angle calculator, PTA Motion System, VNM, DOF Reality) and the
//! MagicBox controller set-up.

use std::collections::BTreeMap;

use openrigcue_model::native_value::LegacyValue;
use openrigcue_model::python_compare::native_equal;
use serde_json::json;

use super::components;
use super::dialogs::{self, Dialog as Shared};
use super::html::{esc, t, tf, tr};
use super::screens::{compensation, games, hardware, joystick};
use super::{notices, read};
use crate::app::MyApp;
use crate::backend::{native_bool, native_field_owned, native_int, BackendExt};
use crate::catalog::ModelFlags;

/// The number box of a UDP port (the game port, its forwarding targets, the PTA destinations).
pub const PORT: read::NumberBox = read::NumberBox::new(1.0, 49152.0, 1.0);
/// The Max Angle calculator's number boxes: the distance between the L/R actuators and the vertical stroke (mm).
const LATERAL: read::NumberBox = read::NumberBox::new(0.0, 5000.0, 1.0);
const VERTICAL: read::NumberBox = read::NumberBox::new(0.0, 500.0, 1.0);

/// An open dialog of the Games or Hardware screens.
#[derive(Debug, Clone, PartialEq)]
pub enum Dialog {
    Games(games::Dialog),
    Joystick(joystick::Dialog),
    Compensation(compensation::Dialog),
    Hardware(Device),
}

/// What the Games and Hardware screens keep between passes (`app.ui.devices`).
#[derive(Debug, Default)]
pub struct State {
    pub games: games::State,
    pub telemetry: super::screens::telemetry::State,
    pub joystick: joystick::State,
    pub compensation: compensation::State,
    pub hardware: hardware::State,
}

/// Opens a dialog (replacing an open one).
pub fn open(app: &mut MyApp, dialog: Dialog) {
    dialogs::open(app, Shared::Hardware(dialog));
}

/// The open dialog of these screens, in front or held behind a Downloaded Profiles review (what it runs goes
/// on there).
pub fn current(app: &MyApp) -> Option<&Dialog> {
    dialogs::open_dialogs(app).find_map(|(dialog, _)| match dialog {
        Shared::Hardware(dialog) => Some(dialog),
        _ => None,
    })
}

/// [`current`] to change what it holds, and whether it is in front.
fn open_mut(app: &mut MyApp) -> Option<(&mut Dialog, bool)> {
    dialogs::open_dialogs_mut(app).find_map(|(dialog, front)| match dialog {
        Shared::Hardware(dialog) => Some((dialog, front)),
        _ => None,
    })
}

/// [`current`], to change what it holds.
pub fn current_mut(app: &mut MyApp) -> Option<&mut Dialog> {
    open_mut(app).map(|(dialog, _)| dialog)
}

/// Replaces the open dialog's content and draws it again when it is in front; a held one shows it when it
/// comes back (nothing happens once it was closed).
pub fn update(app: &mut MyApp, dialog: Dialog) {
    if let Some((shown, front)) = open_mut(app) {
        *shown = dialog;
        if front {
            super::refresh(app, &[super::Region::Dialog]);
        }
    }
}

/// The dialog's markup.
pub fn render(app: &MyApp, dialog: &Dialog) -> String {
    match dialog {
        Dialog::Games(dialog) => games::dialog_render(app, dialog),
        Dialog::Joystick(dialog) => joystick::dialog_render(app, dialog),
        Dialog::Compensation(dialog) => compensation::dialog_render(app, dialog),
        Dialog::Hardware(dialog) => render_device(app, dialog),
    }
}

/// An action of the open dialog.
pub fn act(app: &mut MyApp, dialog: Dialog, act: &str, arg: &str, value: &str) -> Result<(), String> {
    match dialog {
        Dialog::Games(dialog) => games::dialog_act(app, dialog, act, arg, value),
        Dialog::Joystick(dialog) => joystick::dialog_act(app, dialog, act, arg, value),
        Dialog::Compensation(dialog) => compensation::dialog_act(app, dialog, act, arg, value),
        Dialog::Hardware(dialog) => act_device(app, dialog, act, arg, value),
    }
}

/// A button inside a dialog: `dialog-button` with `arg`.
pub fn button(label: &str, arg: &str, class: &str, enabled: bool) -> String {
    format!(
        "<button type=\"button\"{} data-act=\"dialog-button\" data-arg=\"{}\"{}>{label}</button>",
        if class.is_empty() { String::new() } else { format!(" class=\"{class}\"") },
        super::html::esc(arg),
        if enabled { "" } else { " disabled" }
    )
}

/// A text or number field inside a dialog: its changes arrive as `dialog-field` with `name`.
/// `kind` is the input type, `extra` further attributes, each with a leading space (` min="1"`, …).
pub fn input(id: &str, label: &str, name: &str, kind: &str, value: &str, extra: &str) -> String {
    format!(
        "<input id=\"{id}\" class=\"field-input\" type=\"{kind}\" value=\"{}\" aria-label=\"{}\" data-act=\"dialog-field\" data-arg=\"{}\"{extra}>",
        super::html::esc(value),
        super::html::esc(label),
        super::html::esc(name),
    )
}

/// A switch inside a dialog: its changes arrive as `dialog-field` with `name` and `1`/`0`.
pub fn switch(id: &str, name: &str, on: bool, text: &str, enabled: bool) -> String {
    format!(
        "<label class=\"switch\"><input id=\"{id}\" type=\"checkbox\" data-act=\"dialog-field\" data-arg=\"{}\"{}{}><span>{text}</span></label>",
        super::html::esc(name),
        if on { " checked" } else { "" },
        if enabled { "" } else { " disabled" }
    )
}

/// A slider inside a dialog with its value shown beside the label: changes arrive as `dialog-field`.
#[allow(clippy::too_many_arguments)]
pub fn slider(
    id: &str,
    label: &str,
    name: &str,
    (min, max, step): (f64, f64, f64),
    value: f64,
    template: &str,
    enabled: bool,
    hint: &str,
) -> String {
    let shown = template.replace("{}", &super::html::num(value));
    slider_shown(id, label, name, (min, max, step), value, template, &shown, enabled, hint)
}

/// [`slider`] with the value text shown until the slider moves (the template follows the slider while it
/// moves).
#[allow(clippy::too_many_arguments)]
fn slider_shown(
    id: &str,
    label: &str,
    name: &str,
    (min, max, step): (f64, f64, f64),
    value: f64,
    template: &str,
    shown: &str,
    enabled: bool,
    hint: &str,
) -> String {
    let num = super::html::num;
    format!(
        "<div class=\"control-row{}\"><div class=\"control-label\"><label for=\"{id}\"><strong>{label}</strong></label><output id=\"{id}-out\" for=\"{id}\">{}</output></div>{}<input id=\"{id}\" type=\"range\" min=\"{}\" max=\"{}\" step=\"{}\" value=\"{}\" data-act=\"dialog-field\" data-arg=\"{}\" data-output=\"{id}-out\" data-template=\"{}\"{}><div class=\"range-endpoints\"><span>{}</span><span>{}</span></div></div>",
        if enabled { "" } else { " is-disabled" },
        super::html::esc(shown),
        if hint.is_empty() { String::new() } else { format!("<small>{hint}</small>") },
        num(min),
        num(max),
        num(step),
        num(value),
        super::html::esc(name),
        super::html::esc(template),
        if enabled { "" } else { " disabled" },
        num(min),
        num(max)
    )
}

/// A dialog's frame with its own confirm text, or no confirm button (`None`).
pub fn frame(app: &MyApp, title: &str, body: &str, confirm: Option<(&str, bool)>) -> String {
    dialogs::frame(app, title, body, confirm)
}

/// A labelled select inside a dialog: the entries' indices as values, changes as `dialog-field` with `name`.
fn choice(app: &MyApp, id: &str, label: &str, name: &str, options: &[String], selected: Option<usize>) -> String {
    let mut list: Vec<(String, String)> = Vec::new();
    if selected.is_none() {
        list.push((String::new(), "—".into()));
    }
    list.extend(options.iter().enumerate().map(|(index, shown)| (index.to_string(), shown.clone())));
    let selected = selected.map(|index| index.to_string()).unwrap_or_default();
    components::field(
        &t(app, label),
        &components::select(id, &tr(app, label), &list, &selected, "dialog-field", name),
        "",
    )
}

/// A sub-heading inside a dialog.
fn heading(text: &str) -> String {
    format!("<div class=\"eyebrow\">{text}</div>")
}

// ---- Hardware → Hardware ------------------------------------------------------------------------------------

/// A device test.
#[derive(Debug, Clone, PartialEq)]
pub enum Test {
    Wind,
    Led,
    Tach,
    Iflag,
    /// A HapticR wheel (1–4).
    Haptic(i64),
    /// A SIM3D port (1–4).
    Rumble(i64),
    /// A speaker's test sound (by its upper-case name).
    Speaker(String),
}

/// One actuator row of a controller dialog: its name, its port (or CAN id) and, on the rows that have one,
/// the port whose direction its Reverse switch sets.
#[derive(Debug, Clone, PartialEq)]
pub struct Actuator {
    pub name: String,
    pub port: String,
    pub reverse: Option<i64>,
}

/// A value of the Yaw VR dialog.
#[derive(Debug, Clone, PartialEq)]
pub enum YawField {
    /// A limit: its range's top, its value within `0..=max`, and the text shown beside it (the platform's own
    /// value until the slider moves).
    Slider {
        max: i64,
        value: i64,
        shown: String,
    },
    Switch(bool),
}

/// The Thanos4U settings of the controller dialog (each the value shown; the edits are kept as pending
/// configuration until OK).
#[derive(Debug, Clone, PartialEq)]
pub struct Thanos4u {
    /// Each actuator's port and stroke (mm).
    pub strokes: Vec<(i64, i64)>,
    /// Each actuator's lead screw, homing and ratio: port, variable, map key.
    pub choices: Vec<(i64, &'static str, i64)>,
    /// The parking, standby and spike-filter values: configuration key and value.
    pub sliders: Vec<(&'static str, i64)>,
    /// The kill switch, e-stop, offline, filter and platform-check modes: variable and map key.
    pub modes: Vec<(&'static str, i64)>,
}

/// The Thanos / PT-ACTUATOR controller dialog.
#[derive(Debug, Clone, PartialEq)]
pub struct Thanos {
    pub title: String,
    pub pt: bool,
    pub actuators: Vec<Actuator>,
    /// The Reverse settings of ports 5–8 (port, reversed).
    pub reverse: Vec<(i64, bool)>,
    pub thanos4u: Option<Thanos4u>,
    /// The calculator's distance and stroke as typed.
    pub lateral: String,
    pub vertical: String,
    pub max_angle: i64,
    pub spike: i64,
    /// The pending configuration of the main and the secondary controller.
    pub pending: Vec<(String, i64)>,
    pub pending2: Vec<(String, i64)>,
    /// The apply-to-all confirmation that is open: `Some(true)` for the spike filter, `Some(false)` for the
    /// maximum angle.
    pub confirm: Option<bool>,
}

/// A Hardware → Hardware dialog.
#[derive(Debug, Clone, PartialEq)]
pub enum Device {
    /// The output test of a device (Start test / Stop).
    Test(Test),
    /// Make a copy of this profile: the copied layout, the name and the refusal shown.
    Duplicate {
        source: String,
        name: String,
        error: Option<String>,
    },
    /// Delete this profile?
    Remove {
        name: String,
    },
    /// A speaker's effect levels (0–10 each).
    Matrix {
        speaker: String,
        levels: LegacyValue,
    },
    /// The actuator of each assignment option: the option's label and the chosen entry, and the entries (the
    /// value read back and the text shown).
    AxisAssignment {
        rows: Vec<(String, usize)>,
        actuators: Vec<(String, String)>,
    },
    /// The Yaw VR limits and options, in the dialog's order.
    YawVr {
        fields: Vec<(&'static str, &'static str, YawField)>,
    },
    Thanos(Box<Thanos>),
    /// The PTA Motion System's UDP destinations: the address and port being added, the list and whether the
    /// setting exists (then each change is stored at once).
    Pta {
        ip: String,
        port: String,
        list: LegacyValue,
        attached: bool,
    },
    /// DOF Reality's Auto Set KI and KP, and whether it was changed.
    Dof {
        update_kp: bool,
        changed: bool,
    },
    /// VNM's actuators with the Reverse switches of ports 5–9.
    Vnm {
        actuators: Vec<Actuator>,
        reverse: Vec<(i64, bool)>,
    },
    /// The MagicBox: the Magic Wheel ID, and the restart (`Some(true)`) or shutdown (`Some(false)`) being
    /// confirmed.
    Controller {
        wheel_id: String,
        confirm: Option<bool>,
    },
}

/// Opens a Hardware → Hardware dialog.
pub fn open_device(app: &mut MyApp, device: Device) {
    open(app, Dialog::Hardware(device));
}

fn current_device(app: &MyApp) -> Option<&Device> {
    match current(app) {
        Some(Dialog::Hardware(device)) => Some(device),
        _ => None,
    }
}

fn update_device(app: &mut MyApp, device: Device) {
    update(app, Dialog::Hardware(device));
}

/// Opens the output test of a device.
pub fn open_test(app: &mut MyApp, test: Test) {
    open_device(app, Device::Test(test));
}

/// Whether a device's test may start now. The iFlag, HapticR and SIM3D tests need their device connected
/// (HapticR enabled, a SIM3D model set) and no game running; the iFlag and HapticR tests also need no test of
/// their device running, started here or elsewhere: their dialog may have opened before a game or such a test
/// started, or the device went away.
fn test_allowed(app: &MyApp, test: &Test) -> bool {
    let connected = |name: &str| {
        app.backend
            .accessory_status(name)
            .is_ok_and(|status| native_equal(&status, &LegacyValue::Str("Connected".into())))
    };
    let ready = match test {
        Test::Iflag => connected("iflag") && !hardware::test_running(app, "iflag_test"),
        Test::Haptic(_) => {
            hardware::hapticr_enabled(app) && connected("hapticr") && !hardware::test_running(app, "haptic_test")
        }
        Test::Rumble(_) => {
            app.backend.native_setting("SIM3D", "model").is_ok_and(|model| model != LegacyValue::Str("None".into()))
                && connected("sim3d")
        }
        _ => return true,
    };
    ready && !hardware::game_running(app)
}

/// The configured motion model.
fn configured_model(app: &MyApp) -> Result<&'static ModelFlags, String> {
    let name = hardware::model_name(app);
    crate::catalog::model(&name).ok_or_else(|| format!("KeyError: {}", openrigcue_model::python_repr::repr_str(&name)))
}

fn parameter(app: &MyApp, name: &str) -> Result<LegacyValue, String> {
    app.backend.motion_parameter(name).map_err(|error| error.detail)
}

fn setting(app: &MyApp, section: &str, key: &str) -> Result<LegacyValue, String> {
    app.backend.native_setting(section, key).map_err(|error| error.detail)
}

/// Whether the platform has two controllers (more than one motion port).
fn two_controllers(app: &MyApp) -> Result<bool, String> {
    let ports = parameter(app, "motion_com_port")?;
    Ok(ports.py_len().ok_or_else(|| format!("TypeError: object of type '{}' has no len()", ports.type_name()))? > 1)
}

/// The actuator rows of a model: `first` is the number of the first port, `secondary` from which row the
/// Reverse switch and the secondary controller begin (rows beyond it, when the model has more than four
/// actuators).
fn actuator_rows(app: &MyApp, model: &ModelFlags, first: i64, secondary: i64) -> Result<Vec<Actuator>, String> {
    let many = model.port_mapping.len() > 4;
    let two = many && two_controllers(app)?;
    let mut rows = Vec::new();
    for (row, name) in (0i64..).zip(&model.port_mapping) {
        let index = row + if first == 0 { 0 } else { 1 };
        let port = index + if first == 0 { 1 } else { 0 };
        let (shown, reverse) = if many && index > secondary {
            let shown = if two {
                tr(app, "Secondary - ^port^").replace("^port^", &(port - 4).to_string())
            } else {
                port.to_string()
            };
            (shown, Some(port))
        } else if two {
            (tr(app, "Main - ^port^").replace("^port^", &port.to_string()), None)
        } else {
            (port.to_string(), None)
        };
        rows.push(Actuator { name: name.clone(), port: shown, reverse: if many { reverse } else { None } });
    }
    Ok(rows)
}

/// The actuator table of a controller dialog, with the Reverse switches.
fn actuator_table(app: &MyApp, actuators: &[Actuator], port_label: &str, reverse: &[(i64, bool)]) -> String {
    let with_reverse = actuators.iter().any(|a| a.reverse.is_some());
    let rows: String = actuators
        .iter()
        .map(|actuator| {
            let switch_cell = match actuator.reverse {
                Some(port) => {
                    let on = reverse.iter().any(|(p, on)| *p == port && *on);
                    format!(
                        "<td>{}</td>",
                        switch(
                            &format!("thanos-reverse-{port}"),
                            &format!("reverse:{port}"),
                            on,
                            &t(app, if on { "Reversed" } else { "Normal" }),
                            true
                        )
                    )
                }
                None if with_reverse => "<td></td>".into(),
                None => String::new(),
            };
            format!(
                "<tr><th scope=\"row\">{}</th><td class=\"mono\">{}</td>{switch_cell}</tr>",
                t(app, &actuator.name),
                esc(&actuator.port)
            )
        })
        .collect();
    format!(
        "{}<table class=\"data-table\"><thead><tr><th>{}</th><th>{}</th>{}</tr></thead><tbody>{rows}</tbody></table>",
        heading(&t(app, "Actuators")),
        t(app, "Actuator"),
        t(app, port_label),
        if with_reverse { format!("<th>{}</th>", t(app, "Reverse")) } else { String::new() }
    )
}

// ---- Speaker layouts and the matrix ----

/// A free name for a copy: the name without its bracketed part, then ` (n)` with the first unused `n`.
pub fn propose_name(app: &MyApp, table: &str, original: &str) -> Result<String, String> {
    let base = openrigcue_sources::api::joystick::page::remove_text_inside_brackets(original);
    let base = base.trim_end_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c));
    let names: Vec<String> = app
        .backend
        .metadata_rows(table)
        .map_err(|failure| failure.detail)?
        .map(|row| row.map(|(key, _)| key).map_err(|failure| failure.detail))
        .collect::<Result<_, _>>()?;
    (1..).map(|index| format!("{base} ({index})")).find(|name| !names.contains(name)).ok_or_else(String::new)
}

/// Make a copy of this profile: the dialog with a free name for the copy of the chosen layout.
pub fn open_duplicate(app: &mut MyApp) -> Result<(), String> {
    let source = match setting(app, "SHAKEBOX", "profile")? {
        LegacyValue::Str(name) => name,
        other => openrigcue_model::python_repr::str_value(&other),
    };
    let name = propose_name(app, "shakers", &source)?;
    open_device(app, Device::Duplicate { source, name, error: None });
    Ok(())
}

/// A speaker's matrix: its effect levels from the loaded layout.
pub fn open_matrix(app: &mut MyApp, speaker: &str) -> Result<(), String> {
    let loaded = app.backend.shaker_effects().map_err(|error| error.detail)?;
    let levels = native_field_owned(&loaded, &[speaker, "sfx"])?;
    if !matches!(levels, LegacyValue::Dict(_)) {
        return Err(format!("AttributeError: '{}' object has no attribute 'copy'", levels.type_name()));
    }
    open_device(app, Device::Matrix { speaker: speaker.to_owned(), levels });
    Ok(())
}

fn render_matrix(app: &MyApp, speaker: &str, levels: &LegacyValue) -> String {
    let registry = openrigcue_config::catalog::registry::Registry::builtin();
    let mut body = String::new();
    for parent in crate::CHANNEL_PARENTS {
        let effects: Vec<_> =
            registry.shaker_channel_effects.iter().filter(|e| e.parent == parent && e.visible).collect();
        if effects.is_empty() {
            continue;
        }
        if !parent.is_empty() {
            body.push_str(&heading(&t(app, parent)));
        }
        for effect in effects {
            let value = levels.item(&effect.id).ok().and_then(|v| native_int(v).ok()).unwrap_or(0);
            body.push_str(&slider(
                &format!("matrix-{}", effect.id),
                &t(app, &effect.name),
                &format!("level:{}", effect.id),
                (0.0, 10.0, 1.0),
                value as f64,
                "{}",
                true,
                "",
            ));
        }
    }
    frame(
        app,
        &tf(app, "Speaker ^speaker^", &[("speaker", speaker)]),
        &format!(
            "<p class=\"dialog-text\">{}</p>{}",
            t(app, "Advanced Shaker Profile Settings"),
            components::more(app, &[body], "matrix")
        ),
        Some((&t(app, "Apply"), true)),
    )
}

// ---- Axis Assignment ----

/// The Axis Assignment dialog: for each assignment option of the model (up to its actuator count) the scanned
/// actuator, preset from the stored assignment (an entry matching its axis and serial number).
pub fn open_axis_assignment(app: &mut MyApp) -> Result<(), String> {
    let mut actuators = vec![("None".to_owned(), tr(app, "None"))];
    let scanned = parameter(app, "actuators")?;
    let rows = scanned
        .as_dict()
        .ok_or_else(|| format!("AttributeError: '{}' object has no attribute 'items'", scanned.type_name()))?;
    for (key, actuator) in rows.iter() {
        let axis = crate::backend::native_str(&native_field_owned(actuator, &["axis"])?)?;
        let field = |name: &str| -> Result<String, String> {
            let value = native_field_owned(actuator, &[name])?;
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("TypeError: can only concatenate str (not \"{}\") to str", value.type_name()))
        };
        let (port, serial) = (field("com_port")?, field("sn")?);
        let value = format!("Actuator {} - Axis {axis} - {port} - S/N {serial}", key.to_text());
        let shown = tr(app, "Actuator ^number^ - Axis ^axis^ - ^port^ - S/N ^serial^")
            .replace("^number^", &key.to_text())
            .replace("^axis^", &axis)
            .replace("^port^", &port)
            .replace("^serial^", &serial);
        actuators.push((value, shown));
    }
    let model = configured_model(app)?;
    if model.assignment_options.is_empty() {
        return Err("KeyError: 'assignment_options'".into());
    }
    let stored = setting(app, "MOTIONBOX", "axis_assignment").ok();
    let mut options = Vec::new();
    for (index, option) in model.assignment_options.iter().enumerate() {
        let entry = stored
            .as_ref()
            .and_then(|assignment| assignment.item(&index.to_string()).ok().cloned())
            .unwrap_or_else(|| LegacyValue::Str("None".into()));
        let mut selected = 0;
        if entry.as_str() != Some("None") {
            let entry = entry
                .as_str()
                .ok_or_else(|| format!("AttributeError: '{}' object has no attribute 'split'", entry.type_name()))?;
            let mut parts = entry.split(',');
            let axis = parts.next().unwrap_or_default();
            let serial = parts.next().ok_or("IndexError: list index out of range")?;
            let (mut found_axis, mut found_serial) = (false, false);
            for (position, (value, _)) in actuators.iter().enumerate() {
                for part in value.split(" - ") {
                    found_axis |= part.replace("Axis ", "") == axis;
                    found_serial |= part.replace("S/N ", "") == serial;
                }
                if found_axis && found_serial {
                    selected = position;
                    break;
                }
            }
        }
        options.push((option.clone(), selected));
        if model.motors as usize == index + 1 {
            break;
        }
    }
    open_device(app, Device::AxisAssignment { rows: options, actuators });
    Ok(())
}

/// The assignment: `<axis>,<sn>` for each option in order (an option left at None repeats the previous one),
/// sent to the platform, which reconnects.
fn confirm_axis_assignment(
    app: &mut MyApp,
    rows: &[(String, usize)],
    actuators: &[(String, String)],
) -> Result<(), String> {
    let mut assignment = BTreeMap::new();
    let (mut axis, mut serial): (Option<String>, Option<String>) = (None, None);
    let mut index = 0;
    for (_, selected) in rows {
        let value = actuators.get(*selected).map_or("None", |(value, _)| value.as_str());
        if value != "None" {
            for part in value.split(" - ") {
                if part.starts_with("Axis") {
                    axis = Some(part.replace("Axis ", ""));
                } else if part.starts_with("S/N") {
                    serial = Some(part.replace("S/N ", ""));
                }
            }
        }
        if let (Some(axis), Some(serial)) = (&axis, &serial) {
            assignment.insert(index.to_string(), format!("{axis},{serial}"));
            index += 1;
        }
    }
    app.backend.post("motion/axis-assignment", json!({ "assignment": assignment })).map_err(|error| error.detail)?;
    Ok(())
}

// ---- Yaw VR ----

/// The Yaw VR dialog: the platform's limits (pitch and roll maxima by device model), the vibration and the
/// options stored in the settings. Without the platform's limits it does not open.
pub fn open_yaw_vr(app: &mut MyApp) -> Result<(), String> {
    let parameters = parameter(app, "Yaw VR")?;
    let field = |key: &str| native_field_owned(&parameters, &[key]);
    let (roll, pitch_forward, pitch_backward) =
        if field("device_model")? == LegacyValue::Str("YAWDEVICE".into()) { (30, 35, 35) } else { (20, 15, 40) };
    // The platform's value, kept within the slider's range and stored as its whole part (a fraction dropped toward
    // zero); the text beside it is the value as the platform gave it.
    let slider = |max: i64, value: LegacyValue| -> Result<YawField, String> {
        let number = value.number().ok_or_else(|| {
            format!("TypeError: '>' not supported between instances of '{}' and 'int'", value.type_name())
        })?;
        let shown = crate::backend::native_str(&value)?;
        Ok(YawField::Slider { max, value: (number.trunc() as i64).clamp(0, max), shown })
    };
    let flag = |value: LegacyValue| YawField::Switch(value.is_truthy());
    let stored = |key: &str| -> Result<bool, String> { native_bool(&setting(app, "Yaw VR", key)?) };
    let fields = vec![
        ("power", "Power", slider(100, field("power")?)?),
        ("pitch_fwd_limit", "Pitch Forward", slider(pitch_forward, field("pitch_fwd_limit")?)?),
        ("pitch_bwd_limit", "Pitch Backward", slider(pitch_backward, field("pitch_bwd_limit")?)?),
        ("roll_limit", "Roll", slider(roll, field("roll_limit")?)?),
        ("yaw_limit", "Yaw", slider(180, field("yaw_limit")?)?),
        ("yaw_limit_enabled", "Limit Yaw", flag(field("yaw_limit_enabled")?)),
        (
            "vibration_power",
            "Vibration Power",
            slider(100, LegacyValue::Int(native_int(&setting(app, "Yaw VR", "vibration_power")?)?))?,
        ),
        ("vibration_enabled", "Enable Vibration", flag(field("vibration_enabled")?)),
        ("auto_park", "Auto Park?", YawField::Switch(stored("auto_park")?)),
        ("center_before_start_alert", "Alert to Center Yaw VR", YawField::Switch(stored("center_before_start_alert")?)),
        ("6dof_mode", "6DOF Mode MC", YawField::Switch(stored("6dof_mode")?)),
    ];
    open_device(app, Device::YawVr { fields });
    Ok(())
}

fn render_yaw_vr(app: &MyApp, fields: &[(&'static str, &'static str, YawField)]) -> String {
    let limited = fields.iter().any(|(key, _, f)| *key == "yaw_limit_enabled" && *f == YawField::Switch(true));
    let mut rows: Vec<String> = Vec::new();
    for (key, label, value) in fields {
        rows.push(match value {
            YawField::Slider { max, value, shown } => slider_shown(
                &format!("yaw-{key}"),
                &t(app, label),
                &format!("yaw:{key}"),
                (0.0, *max as f64, 1.0),
                *value as f64,
                "{}",
                shown,
                *key != "yaw_limit" || limited,
                "",
            ),
            YawField::Switch(on) => components::field(
                &t(app, label),
                &switch(
                    &format!("yaw-{key}"),
                    &format!("yaw:{key}"),
                    *on,
                    &t(app, if *on { "On" } else { "Off" }),
                    true,
                ),
                if matches!(*key, "auto_park" | "center_before_start_alert") {
                    t(app, "Stored as soon as it is changed.")
                } else {
                    String::new()
                }
                .as_str(),
            ),
        });
    }
    frame(app, "Yaw VR", &components::more(app, &rows, "yawvr"), Some((&t(app, "Confirm"), true)))
}

/// A Yaw VR value changed: Auto Park and the centre alert are stored at once.
fn yaw_field(
    app: &mut MyApp,
    mut fields: Vec<(&'static str, &'static str, YawField)>,
    key: &str,
    value: &str,
) -> Result<(), String> {
    for (field_key, _, field) in fields.iter_mut().filter(|(k, ..)| *k == key) {
        match field {
            YawField::Slider { max, value: current, shown } => {
                *current = read::int(value)?.clamp(0, *max);
                *shown = current.to_string();
            }
            YawField::Switch(on) => {
                *on = read::checked(value);
                if matches!(*field_key, "auto_park" | "center_before_start_alert") {
                    app.set_setting_native_checked("Yaw VR", field_key, &LegacyValue::Bool(*on))?;
                }
            }
        }
    }
    update_device(app, Device::YawVr { fields });
    Ok(())
}

/// The Yaw VR dialog confirmed: the vibration power, Auto Park, the centre alert and the 6DOF mode become
/// settings, the other values go to the platform.
fn confirm_yaw_vr(app: &mut MyApp, fields: &[(&'static str, &'static str, YawField)]) -> Result<(), String> {
    for (key, _, field) in fields {
        let value = match field {
            YawField::Slider { value, .. } => *value,
            YawField::Switch(on) => i64::from(*on),
        };
        match *key {
            "vibration_power" | "auto_park" | "center_before_start_alert" | "6dof_mode" => {
                app.set_setting_native_checked("Yaw VR", key, &LegacyValue::Int(value))?
            }
            _ => app.backend.assign_yaw_limit(key, value).map_err(|error| error.detail)?,
        }
    }
    Ok(())
}

// ---- Thanos / PT-ACTUATOR ----

/// The value maps of the Thanos4U settings: map key and text.
const LEAD_SCREW: [(i64, &str); 4] = [(2, "5mm/rev"), (1, "4mm/rev"), (3, "10mm/rev"), (4, "25mm/rev")];
const HOMING: [(i64, &str); 2] = [(1, "Retract"), (2, "Expand")];
const RATIO: [(i64, &str); 3] = [(1, "1:1"), (2, "1:1.5"), (3, "1:2")];
const KILL_SWITCH: [(i64, &str); 2] = [(1, "Momentary Switch"), (2, "Immediate Activate")];
const ESTOP: [(i64, &str); 2] = [(1, "Hold Powered"), (2, "Cut Power")];
const OFFLINE: [(i64, &str); 2] = [(1, "Standby Position"), (2, "Parking Position")];
const FILTER: [(i64, &str); 6] =
    [(0, "No Filter"), (1, "Hard"), (2, "Semi Hard"), (3, "Semi Soft"), (4, "Soft"), (5, "Very Soft")];
const PLATFORM_CHECK: [(i64, &str); 2] = [(1, "Disable"), (2, "Enabled")];

/// A value map: map key and text.
type ValueMap = &'static [(i64, &'static str)];

/// The per-actuator maps: variable, label and map.
const ACTUATOR_MAPS: [(&str, &str, ValueMap); 3] =
    [("lead_screw", "Lead Screw", &LEAD_SCREW), ("inline", "Homing", &HOMING), ("ratio", "Ratio", &RATIO)];

/// The controller-wide maps: variable, label, group and map.
const MODE_MAPS: [(&str, &str, &str, ValueMap); 5] = [
    ("klm_mode", "Kill Motor Switch", "E-STOP", &KILL_SWITCH),
    ("estop_mode", "E-Stop Mode", "E-STOP", &ESTOP),
    ("force_offline_mode", "Offline Mode", "E-STOP", &OFFLINE),
    ("filter_factor", "Actuator Smoothing", "FILTER", &FILTER),
    ("platform_check", "Check", "PLATFORM", &PLATFORM_CHECK),
];

/// The sliders: configuration key, label, group, range and whether the value is in seconds.
const THANOS_SLIDERS: [(&str, &str, &str, i64, i64, bool); 8] = [
    ("park_position", "Parking Position", "PARKING", 1, 254, false),
    ("park_move_speed", "Parking Speed", "PARKING", 1, 254, false),
    ("park_move_timeout", "Parking Timeout", "PARKING", 1, 90, true),
    ("6dof_start_pos", "Standby Position", "STANDBY", 1, 254, false),
    ("start_move_step", "Standby Speed", "STANDBY", 5, 254, false),
    ("start_move_timeout", "Standby Timeout", "STANDBY", 1, 90, true),
    ("spike_move_speed", "Esc Speed", "SPIKE FILTER", 1, 254, false),
    ("spike_fine_move_speed", "Fine Speed", "SPIKE FILTER", 1, 254, false),
];

/// A value on its range as a percentage, then the value: `<perc>% <value>`.
pub fn slider_text(value: i64, min: i64, max: i64) -> String {
    let mapped = (value as f64 - min as f64) * 100.0 / (max as f64 - min as f64) + 0.0;
    let upper = if mapped <= 100.0 { mapped } else { 100.0 };
    let percent = if 0.0 <= upper { upper } else { 0.0 } as i64;
    format!("{percent}% {value}")
}

/// Sets a pending configuration entry (an existing key keeps its position).
fn put(entries: &mut Vec<(String, i64)>, key: &str, value: i64) {
    match entries.iter_mut().find(|(k, _)| k == key) {
        Some(entry) => entry.1 = value,
        None => entries.push((key.to_owned(), value)),
    }
}

fn title_text(app: &MyApp, key: &str) -> Result<String, String> {
    let value = parameter(app, key)?;
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("TypeError: can only concatenate str (not \"{}\") to str", value.type_name()))
}

/// Controller Settings: the PTA Motion System dialog for that model; otherwise the controller dialog — the
/// actuator table with the Reverse switches of ports 5–8, for a Thanos4U the controller's settings, and for a
/// Thanos the maximum-angle calculator.
pub fn open_thanos(app: &mut MyApp) -> Result<(), String> {
    if hardware::model_name(app) == "PTA Motion System" {
        return open_pta(app);
    }
    let model = configured_model(app)?;
    let pt = model.manufacturer == "PT-ACTUATOR";
    let title = if pt {
        "PT-ACTUATOR".to_owned()
    } else {
        format!("Thanos - {} - v{}", title_text(app, "thanos_controller")?, title_text(app, "thanos_firmware")?)
    };
    let mut reverse = Vec::new();
    for port in 5..9 {
        reverse.push((
            port,
            native_bool(&setting(app, &model.manufacturer, &format!("reverse_controller_port_{port}"))?)?,
        ));
    }
    let actuators = if pt { actuator_rows(app, model, 0, 3)? } else { actuator_rows(app, model, 1, 4)? };
    let mut dialog = Thanos {
        title,
        pt,
        actuators,
        reverse,
        thanos4u: None,
        lateral: String::new(),
        vertical: String::new(),
        max_angle: 0,
        spike: 0,
        pending: Vec::new(),
        pending2: Vec::new(),
        confirm: None,
    };
    if !pt {
        let controller = parameter(app, "thanos_controller")?;
        if matches!(controller.as_str(), Some("Thanos4U(S)" | "Thanos4U(M)")) {
            dialog.thanos4u = Some(read_thanos4u(app, model)?);
        }
        let wide = |key: &str| -> Result<String, String> {
            Ok(crate::backend::native_int_wide(&setting(app, &model.manufacturer, key)?)?.to_string())
        };
        dialog.lateral = wide("lateral_distance_actuators")?;
        dialog.vertical = wide("vertical_actuators_stroke")?;
        recommend(app, &mut dialog)?;
    }
    open_device(app, Device::Thanos(Box::new(dialog)));
    Ok(())
}

/// The Thanos4U settings as configured on the controller.
fn read_thanos4u(app: &MyApp, model: &ModelFlags) -> Result<Thanos4u, String> {
    let config = |table: &str, key: &str| native_field_owned(&parameter(app, table)?, &[key]);
    let map_key = |value: &LegacyValue, map: &[(i64, &str)]| -> Result<i64, String> {
        map.iter()
            .find(|(key, _)| native_equal(value, &LegacyValue::Int(*key)))
            .map(|(key, _)| *key)
            .ok_or_else(|| format!("KeyError: {}", openrigcue_model::python_repr::repr(value)))
    };
    let mut strokes = Vec::new();
    let mut choices = Vec::new();
    for (port, _) in (1i64..).zip(&model.port_mapping) {
        let (table, number) = if port >= 5 { ("thanos_config2", port - 4) } else { ("thanos_config", port) };
        let stroke = config(table, &format!("stroke{number}"))?;
        strokes.push((port, native_int(&stroke)?.saturating_mul(10)));
        for (variable, _, map) in ACTUATOR_MAPS {
            choices.push((port, variable, map_key(&config(table, &format!("{variable}{number}"))?, map)?));
        }
    }
    let mut sliders = Vec::new();
    for (key, ..) in THANOS_SLIDERS {
        sliders.push((key, native_int(&config("thanos_config", key)?)?));
    }
    let mut modes = Vec::new();
    for (variable, _, _, map) in MODE_MAPS {
        modes.push((variable, map_key(&config("thanos_config", variable)?, map)?));
    }
    Ok(Thanos4u { strokes, choices, sliders, modes })
}

/// The recommended maximum angle (`distance / stroke * 10`, at most 100) and spike filter
/// (`350 * distance / stroke`, times the controller factor, at most 64530).
fn recommend(app: &MyApp, dialog: &mut Thanos) -> Result<(), String> {
    let int = |text: &str| crate::backend::native_int_wide(&LegacyValue::Str(text.to_owned()));
    let divide = |numerator: i128, denominator: i128| {
        openrigcue_model::python_numeric::int_true_divide(numerator, denominator)
            .ok_or_else(|| "ZeroDivisionError: division by zero".to_owned())
    };
    let vertical = int(&dialog.vertical)?;
    let (angle, spike) = if vertical == 0 {
        (0, 0)
    } else {
        let controller = parameter(app, "thanos_controller")?;
        let factor =
            if matches!(controller.as_str(), Some("AMC-AASD15A" | "Thanos4U(M)" | "Thanos4U(S)")) { 1 } else { 10 };
        let lateral = int(&dialog.lateral)?;
        let angle = crate::backend::native_int_wide(&LegacyValue::Float(divide(lateral, vertical)? * 10.0))?;
        let numerator = lateral.checked_mul(350).ok_or("OverflowError: integer exceeds supported arithmetic range")?;
        let spike = crate::backend::native_int_wide(&LegacyValue::Float(divide(numerator, vertical)?))?
            .checked_mul(factor)
            .ok_or("OverflowError: integer exceeds supported arithmetic range")?;
        (angle.min(100), spike.min(64530))
    };
    dialog.max_angle = i64::try_from(angle).unwrap_or(i64::MAX);
    dialog.spike = i64::try_from(spike).unwrap_or(i64::MAX);
    Ok(())
}

fn map_options(app: &MyApp, map: &[(i64, &str)]) -> Vec<String> {
    map.iter().map(|(_, shown)| tr(app, shown)).collect()
}

fn render_thanos(app: &MyApp, d: &Thanos) -> String {
    if let Some(spike) = d.confirm {
        let (title, question) = if spike {
            ("Spike Filter Confirmation", "Confirm to apply this spike filter to each of your tunings")
        } else {
            ("Max Angle Confirmation", "Confirm to apply this max angle to each of your tunings")
        };
        return frame(
            app,
            &t(app, title),
            &format!("<p class=\"dialog-text\">{}</p>", t(app, question)),
            Some((&t(app, "Confirm"), true)),
        );
    }
    let mut body = actuator_table(app, &d.actuators, if d.pt { "Can ID" } else { "Port" }, &d.reverse);
    if let Some(s) = &d.thanos4u {
        let mut rows = vec![components::field(
            &t(app, "Defaults Settings"),
            &format!(
                "{} {}",
                button(&t(app, "PT-Actuator"), "defaults:P", "", true),
                button(&t(app, "Others"), "defaults:S", "", true)
            ),
            "",
        )];
        for (port, mm) in &s.strokes {
            let name = d.actuators.get((*port - 1) as usize).map_or(String::new(), |a| tr(app, &a.name));
            rows.push(heading(&esc(&tr(app, "ACTUATOR ^name^").replace("^name^", &name))));
            rows.push(slider(
                &format!("thanos-stroke-{port}"),
                &t(app, "Stroke"),
                &format!("stroke:{port}"),
                (10.0, 500.0, 10.0),
                *mm as f64,
                "{}mm",
                true,
                "",
            ));
            for (variable, label, map) in ACTUATOR_MAPS {
                let key = s.choices.iter().find(|(p, v, _)| p == port && *v == variable).map(|(.., k)| *k);
                rows.push(choice(
                    app,
                    &format!("thanos-{variable}-{port}"),
                    label,
                    &format!("var:{port}:{variable}"),
                    &map_options(app, map),
                    key.and_then(|key| map.iter().position(|(k, _)| *k == key)),
                ));
            }
            if *port == 1 {
                rows.push(format!(
                    "<div class=\"device action-row\">{}</div>",
                    button(&t(app, "Copy settings to all vertical actuators"), "copy", "", true)
                ));
            }
        }
        let mut group = "";
        for (key, label, section, min, max, seconds) in THANOS_SLIDERS {
            if section != group {
                rows.push(heading(&t(app, section)));
                group = section;
            }
            let value = s.sliders.iter().find(|(k, _)| *k == key).map_or(min, |(_, v)| *v);
            let (template, shown) =
                if seconds { ("{}s", format!("{value}s")) } else { ("{}", slider_text(value, min, max)) };
            rows.push(slider_shown(
                &format!("thanos-{key}"),
                &t(app, label),
                &format!("slider:{key}"),
                (min as f64, max as f64, 1.0),
                value as f64,
                template,
                &shown,
                true,
                "",
            ));
            if key == "start_move_timeout" {
                for (variable, label, section, map) in MODE_MAPS {
                    if section != group {
                        rows.push(heading(&t(app, section)));
                        group = section;
                    }
                    let key = s.modes.iter().find(|(v, _)| *v == variable).map(|(_, k)| *k);
                    rows.push(choice(
                        app,
                        &format!("thanos-{variable}"),
                        label,
                        &format!("mode:{variable}"),
                        &map_options(app, map),
                        key.and_then(|key| map.iter().position(|(k, _)| *k == key)),
                    ));
                }
            }
        }
        body.push_str(&heading(&t(app, "Thanos4U")));
        body.push_str(&components::more(app, &rows, "thanos4u"));
    }
    if !d.pt {
        let enabled = d.max_angle > 0;
        body.push_str(&heading(&t(app, "MAX ANGLE")));
        body.push_str(&components::field(
            &t(app, "Distance Between L/R Actuators (mm)"),
            &input(
                "thanos-lateral",
                &tr(app, "Distance Between L/R Actuators (mm)"),
                "lateral",
                "number",
                &d.lateral,
                &LATERAL.attributes(),
            ),
            "",
        ));
        body.push_str(&components::field(
            &t(app, "Vertical Actuators Stroke (mm)"),
            &input(
                "thanos-vertical",
                &tr(app, "Vertical Actuators Stroke (mm)"),
                "vertical",
                "number",
                &d.vertical,
                &VERTICAL.attributes(),
            ),
            "",
        ));
        body.push_str(&format!(
            "<div class=\"device action-row\">{}</div>",
            button(&t(app, "Calculate"), "calculate", "", true)
        ));
        body.push_str(&components::field(
            &t(app, "Recommended Max Angle"),
            &format!(
                "<span class=\"mono\" id=\"thanos-max-angle\">{}</span> {}",
                d.max_angle,
                button(&t(app, "Apply Max Angle to all Games / Vehicles"), "apply:angle", "", enabled)
            ),
            "",
        ));
        body.push_str(&components::field(
            &t(app, "Recommended Spike Filter"),
            &format!(
                "<span class=\"mono\" id=\"thanos-spike\">{}</span> {}",
                d.spike,
                button(&t(app, "Apply Spike Filter to all Games / Vehicles"), "apply:spike", "", enabled)
            ),
            "",
        ));
    }
    frame(app, &esc(&d.title), &body, Some((&t(app, "OK"), true)))
}

/// A value-map choice: the pending configuration takes the map key under `<variable><port>`; with two
/// controllers the secondary configuration takes it under the port less 4.
fn thanos_choice(app: &MyApp, d: &mut Thanos, port: Option<i64>, variable: &str, map_key: i64) -> Result<(), String> {
    if two_controllers(app)? {
        let port = port.map(|p| (p - 4).to_string()).unwrap_or_default();
        put(&mut d.pending2, &format!("{variable}{port}"), map_key);
    } else {
        let port = port.map(|p| p.to_string()).unwrap_or_default();
        put(&mut d.pending, &format!("{variable}{port}"), map_key);
    }
    Ok(())
}

/// An actuator's stroke (mm): pending in cm under `stroke<n>` (ports 5+ on the secondary controller).
fn thanos_stroke(d: &mut Thanos, port: i64, mm: i64) {
    let cm = openrigcue_model::python_numeric::int_true_divide(i128::from(mm), 10).unwrap_or(0.0) as i64;
    if let Some(s) = d.thanos4u.as_mut() {
        if let Some(entry) = s.strokes.iter_mut().find(|(p, _)| *p == port) {
            entry.1 = mm;
        }
    }
    if port >= 5 {
        put(&mut d.pending2, &format!("stroke{}", port - 4), cm);
    } else {
        put(&mut d.pending, &format!("stroke{port}"), cm);
    }
}

/// A parking, standby or spike-filter value: pending on both controllers.
fn thanos_slider(d: &mut Thanos, key: &str, value: i64) {
    if let Some(s) = d.thanos4u.as_mut() {
        if let Some(entry) = s.sliders.iter_mut().find(|(k, _)| *k == key) {
            entry.1 = value;
        }
    }
    put(&mut d.pending, key, value);
    put(&mut d.pending2, key, value);
}

fn set_actuator_choice(
    app: &MyApp,
    d: &mut Thanos,
    port: i64,
    variable: &'static str,
    map_key: i64,
) -> Result<(), String> {
    if let Some(s) = d.thanos4u.as_mut() {
        if let Some(entry) = s.choices.iter_mut().find(|(p, v, _)| *p == port && *v == variable) {
            entry.2 = map_key;
        }
    }
    thanos_choice(app, d, Some(port), variable, map_key)
}

/// The factory values (`P`: PT-Actuator strokes of 150 mm, otherwise 100 mm).
fn thanos_defaults(app: &MyApp, d: &mut Thanos, pt_actuators: bool) -> Result<(), String> {
    for (key, value) in [
        ("park_move_speed", 2),
        ("park_position", 1),
        ("park_move_timeout", 1),
        ("start_move_step", 5),
        ("6dof_start_pos", 127),
        ("start_move_timeout", 5),
        ("spike_move_speed", 24),
        ("spike_fine_move_speed", 11),
    ] {
        thanos_slider(d, key, value);
    }
    for (variable, value) in
        [("klm_mode", 1), ("estop_mode", 2), ("force_offline_mode", 2), ("filter_factor", 0), ("platform_check", 1)]
    {
        if let Some(s) = d.thanos4u.as_mut() {
            if let Some(entry) = s.modes.iter_mut().find(|(v, _)| *v == variable) {
                entry.1 = value;
            }
        }
        thanos_choice(app, d, None, variable, value)?;
    }
    let ports: Vec<i64> = d.thanos4u.as_ref().map(|s| s.strokes.iter().map(|(p, _)| *p).collect()).unwrap_or_default();
    for port in ports.into_iter().filter(|port| *port < 8) {
        thanos_stroke(d, port, if pt_actuators { 150 } else { 100 });
        set_actuator_choice(app, d, port, "ratio", 1)?;
        set_actuator_choice(app, d, port, "inline", 1)?;
        set_actuator_choice(app, d, port, "lead_screw", 2)?;
    }
    Ok(())
}

/// Actuator 1's stroke, ratio, homing and lead screw on actuators 2–7.
fn thanos_copy(app: &MyApp, d: &mut Thanos) -> Result<(), String> {
    let Some(s) = d.thanos4u.clone() else { return Ok(()) };
    let Some((_, stroke)) = s.strokes.iter().find(|(p, _)| *p == 1).copied() else { return Ok(()) };
    for port in s.strokes.iter().map(|(p, _)| *p).filter(|p| (2..8).contains(p)) {
        thanos_stroke(d, port, stroke);
        for variable in ["ratio", "inline", "lead_screw"] {
            if let Some((.., key)) = s.choices.iter().find(|(p, v, _)| *p == 1 && *v == variable) {
                set_actuator_choice(app, d, port, variable, *key)?;
            }
        }
    }
    Ok(())
}

/// The controller dialog confirmed: both pending configurations, then the motor counts, then each Reverse
/// setting.
fn confirm_thanos(app: &mut MyApp, d: &Thanos) -> Result<(), String> {
    app.backend.update_thanos_pending(false, true, &d.pending).map_err(|error| error.detail)?;
    app.backend.update_thanos_pending(true, true, &d.pending2).map_err(|error| error.detail)?;
    if two_controllers(app)? {
        // The main controller drives four motors; that is stored before the model gives the secondary's count.
        app.backend.update_thanos_pending(false, false, &[("motor_number".into(), 4)]).map_err(|error| error.detail)?;
        let motors = i64::from(configured_model(app)?.motors) - 4;
        app.backend
            .update_thanos_pending(true, false, &[("motor_number".into(), motors)])
            .map_err(|error| error.detail)?;
    } else {
        let motors = i64::from(configured_model(app)?.motors) - 4;
        app.backend
            .update_thanos_pending(false, false, &[("motor_number".into(), motors)])
            .map_err(|error| error.detail)?;
    }
    for (port, on) in &d.reverse {
        let model = configured_model(app)?;
        app.set_setting_native_checked(
            &model.manufacturer,
            &format!("reverse_controller_port_{port}"),
            &LegacyValue::Bool(*on),
        )?;
    }
    Ok(())
}

/// The recommended value written into every game and vehicle tune (the spike filter skips the emulator).
fn apply_to_all(app: &mut MyApp, d: &Thanos, spike: bool) -> Result<(), String> {
    let value = if spike { d.spike } else { d.max_angle };
    let backend = app.backend.clone();
    backend.apply_all_tunes(spike, &mut || Ok(value)).map_err(|error| error.detail)?;
    notices::success(
        app,
        "Alert",
        if spike {
            "Every game / vehicle now uses the new spike filter"
        } else {
            "Every game / vehicle now uses the new max angle"
        },
    );
    Ok(())
}

fn thanos_act(app: &mut MyApp, mut d: Thanos, act: &str, arg: &str, value: &str) -> Result<(), String> {
    match act {
        "dialog-close" if d.confirm.is_some() => d.confirm = None,
        "dialog-close" => {
            dialogs::close(app);
            return Ok(());
        }
        "dialog-ok" => match d.confirm.take() {
            Some(spike) => apply_to_all(app, &d, spike)?,
            None => {
                dialogs::close(app);
                return confirm_thanos(app, &d);
            }
        },
        "dialog-field" => {
            let mut parts = arg.split(':');
            match (parts.next(), parts.next(), parts.next()) {
                (Some("reverse"), Some(port), _) => {
                    let port: i64 = port.parse().map_err(|_| format!("ValueError: {port:?}"))?;
                    match d.reverse.iter_mut().find(|(p, _)| *p == port) {
                        Some(entry) => entry.1 = read::checked(value),
                        None => d.reverse.push((port, read::checked(value))),
                    }
                }
                (Some("stroke"), Some(port), _) => {
                    thanos_stroke(&mut d, port.parse().unwrap_or(0), read::int(value)?);
                }
                (Some("slider"), Some(key), _) => {
                    let key = THANOS_SLIDERS
                        .iter()
                        .find(|s| s.0 == key)
                        .map(|s| s.0)
                        .ok_or_else(|| format!("KeyError: {}", openrigcue_model::python_repr::repr_str(key)))?;
                    thanos_slider(&mut d, key, read::int(value)?);
                }
                (Some("var"), Some(port), Some(variable)) => {
                    let (variable, _, map) =
                        ACTUATOR_MAPS.iter().find(|(v, ..)| *v == variable).copied().ok_or_else(|| {
                            format!("KeyError: {}", openrigcue_model::python_repr::repr_str(variable))
                        })?;
                    let index: usize = value.parse().map_err(|_| format!("ValueError: {value:?}"))?;
                    let (key, _) = map.get(index).ok_or_else(|| format!("ValueError: {value:?} is not in list"))?;
                    set_actuator_choice(app, &mut d, port.parse().unwrap_or(0), variable, *key)?;
                }
                (Some("mode"), Some(variable), _) => {
                    let (variable, _, _, map) =
                        MODE_MAPS.iter().find(|(v, ..)| *v == variable).copied().ok_or_else(|| {
                            format!("KeyError: {}", openrigcue_model::python_repr::repr_str(variable))
                        })?;
                    let index: usize = value.parse().map_err(|_| format!("ValueError: {value:?}"))?;
                    let (key, _) = map.get(index).ok_or_else(|| format!("ValueError: {value:?} is not in list"))?;
                    if let Some(s) = d.thanos4u.as_mut() {
                        if let Some(entry) = s.modes.iter_mut().find(|(v, _)| *v == variable) {
                            entry.1 = *key;
                        }
                    }
                    thanos_choice(app, &mut d, None, variable, *key)?;
                }
                (Some(field @ ("lateral" | "vertical")), ..) => {
                    // The box takes the typed value within its limits; the recommendation follows it, then the
                    // value is stored as the box took it (a whole number as one). The box keeps what it took
                    // when the recommendation or the store fails.
                    let (number_box, shown, key) = if field == "lateral" {
                        (LATERAL, &mut d.lateral, "lateral_distance_actuators")
                    } else {
                        (VERTICAL, &mut d.vertical, "vertical_actuators_stroke")
                    };
                    let taken =
                        number_box.take(&LegacyValue::Str(value.to_owned()), &LegacyValue::Str(shown.clone()))?;
                    *shown = read::box_text(&taken);
                    let stored = recommend(app, &mut d)
                        .and_then(|()| app.set_setting_native_checked("PT-ACTUATOR", key, &taken));
                    update_device(app, Device::Thanos(Box::new(d)));
                    return stored;
                }
                _ => {}
            }
        }
        "dialog-button" => match arg {
            "defaults:P" => thanos_defaults(app, &mut d, true)?,
            "defaults:S" => thanos_defaults(app, &mut d, false)?,
            "copy" => thanos_copy(app, &mut d)?,
            "calculate" => recommend(app, &mut d)?,
            "apply:angle" if d.max_angle > 0 => d.confirm = Some(false),
            "apply:spike" if d.max_angle > 0 => d.confirm = Some(true),
            _ => {}
        },
        _ => return Ok(()),
    }
    update_device(app, Device::Thanos(Box::new(d)));
    Ok(())
}

// ---- PTA Motion System, DOF Reality, VNM ----

/// The PTA Motion System dialog: the UDP destinations (`PT-ACTUATOR.udp_config`).
pub fn open_pta(app: &mut MyApp) -> Result<(), String> {
    let (list, attached) = match setting(app, "PT-ACTUATOR", "udp_config") {
        Ok(value) => (value, true),
        Err(_) => (LegacyValue::List(Vec::new()), false),
    };
    open_device(app, Device::Pta { ip: "255.255.255.255".into(), port: "7408".into(), list, attached });
    Ok(())
}

fn destinations(list: &LegacyValue) -> Vec<String> {
    match &list.resolved() {
        LegacyValue::List(values) | LegacyValue::Tuple(values) | LegacyValue::Set(values) => {
            values.iter().map(openrigcue_model::python_repr::str_value).collect()
        }
        _ => Vec::new(),
    }
}

fn render_pta(app: &MyApp, ip: &str, port: &str, list: &LegacyValue) -> String {
    let rows: String = destinations(list)
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let (address, port) = item.split_once(':').unwrap_or((item.as_str(), ""));
            format!(
                "<tr><td class=\"mono\">{}</td><td class=\"mono\">{}</td><td>{}</td></tr>",
                esc(address),
                esc(port),
                button(&t(app, "Delete"), &format!("delete:{index}"), "", true)
            )
        })
        .collect();
    let body = format!(
        "{}{}<div class=\"device action-row\">{}</div><table class=\"data-table\"><thead><tr><th>{}</th><th>{}</th><th>{}</th></tr></thead><tbody>{rows}</tbody></table>",
        components::field(&t(app, "IP"), &input("pta-ip", &tr(app, "IP"), "ip", "text", ip, ""), ""),
        components::field(&t(app, "Port"), &input("pta-port", &tr(app, "Port"), "port", "number", port, &PORT.attributes()), ""),
        button(&t(app, "Add"), "add", "", true),
        t(app, "IP"),
        t(app, "Port"),
        t(app, "Delete")
    );
    frame(app, "PTA Motion System", &body, Some((&t(app, "OK"), true)))
}

fn pta_act(
    app: &mut MyApp,
    (mut ip, mut port, mut list, attached): (String, String, LegacyValue, bool),
    act: &str,
    arg: &str,
    value: &str,
) -> Result<(), String> {
    match (act, arg) {
        ("dialog-close", _) => {
            dialogs::close(app);
            return Ok(());
        }
        ("dialog-ok", _) => {
            dialogs::close(app);
            app.set_setting_native_checked("PT-ACTUATOR", "udp_config", &list)?;
            app.backend.post("motion/disconnect", json!({})).map_err(|error| error.detail)?;
            notices::confirmation(app, "PTA Motion System", "Motion platform reconnecting");
            return Ok(());
        }
        ("dialog-field", "ip") => ip = value.to_owned(),
        ("dialog-field", "port") => port = PORT.take_text(value, &port)?,
        ("dialog-button", "add") => {
            if !crate::net::is_ip_address(&ip) {
                let message = format!("{ip}{}", tr(app, " is not a correctly formed IP address"));
                notices::warning(app, "Alert", &message);
            } else {
                let destination = format!("{ip}:{port}");
                if destinations(&list).contains(&destination) {
                    let message = tr(app, "This IP and Port pair is a destination already");
                    notices::warning(app, "Alert", &message);
                } else if let LegacyValue::List(values) = &mut list {
                    values.push(LegacyValue::Str(destination));
                    if attached {
                        app.set_setting_native_checked("PT-ACTUATOR", "udp_config", &list)?;
                    }
                } else {
                    return Err(format!("AttributeError: '{}' object has no attribute 'append'", list.type_name()));
                }
            }
        }
        ("dialog-button", delete) if delete.starts_with("delete:") => {
            let index: usize = delete.trim_start_matches("delete:").parse().unwrap_or(usize::MAX);
            if let LegacyValue::List(values) | LegacyValue::Set(values) = &mut list {
                if index < values.len() {
                    values.remove(index);
                }
            }
            if attached {
                app.set_setting_native_checked("PT-ACTUATOR", "udp_config", &list)?;
            }
        }
        _ => return Ok(()),
    }
    update_device(app, Device::Pta { ip, port, list, attached });
    Ok(())
}

/// The DOF Reality dialog: Auto Set KI and KP.
pub fn open_dof(app: &mut MyApp) -> Result<(), String> {
    let update_kp = native_bool(&setting(app, "DOF REALITY", "update_kp")?)?;
    open_device(app, Device::Dof { update_kp, changed: false });
    Ok(())
}

/// The VNM dialog: the actuators with the Reverse switches of ports 5–9.
pub fn open_vnm(app: &mut MyApp) -> Result<(), String> {
    let mut reverse = Vec::new();
    for port in 5..10 {
        reverse
            .push((port, native_bool(&setting(app, "VNM SIMULATION", &format!("reverse_controller_port_{port}"))?)?));
    }
    let model = configured_model(app)?;
    let actuators = actuator_rows(app, model, 1, 4)?;
    open_device(app, Device::Vnm { actuators, reverse });
    Ok(())
}

// ---- MagicBox ----

fn render_controller(app: &MyApp, wheel_id: &str, confirm: Option<bool>) -> String {
    if let Some(reboot) = confirm {
        let (title, info) = if reboot {
            ("Reboot?", "The Magicbox is going to reboot. Rebooting can last as long as 3 minutes, and the Magicbox does not respond until it is done. Press Confirm to go ahead.")
        } else {
            ("Shutdown?", "The Magicbox is going to shut down. To start it again afterwards, pull its plug from the electrical outlet and plug it back in. Press Confirm to go ahead.")
        };
        return frame(
            app,
            &t(app, title),
            &format!("<p class=\"dialog-text\">{}</p>", t(app, info)),
            Some((&t(app, "Confirm"), true)),
        );
    }
    let body = format!(
        "{}{}",
        components::field(
            &t(app, "Magic Wheel Id"),
            &input("magic-wheel-id", &tr(app, "Magic Wheel Id"), "wheel", "text", wheel_id, " maxlength=\"4\""),
            ""
        ),
        components::field(
            &t(app, "Controller"),
            &format!(
                "{} {}",
                button(&t(app, "Reboot"), "reboot", "", true),
                button(&t(app, "Shutdown"), "shutdown", "", true)
            ),
            ""
        )
    );
    frame(app, &t(app, "Controller setup"), &body, None)
}

/// A confirmed restart or shutdown: the notice that stays, then the system command on this computer.
fn magicbox_command(app: &mut MyApp, reboot: bool) {
    let (title, message) = if reboot {
        ("Reboot", "The Magicbox is rebooting now. It can need as long as 3 minutes to rejoin your network.")
    } else {
        ("Shutdown", "The shutdown has begun")
    };
    let message = tr(app, message);
    notices::warning(app, title, &message);
    app.start_thread(
        if reboot { "reboot_confirmation_operation" } else { "shutdown_confirmation_operation" },
        Box::new(move |_| {
            let command = if reboot { "shutdown -r now" } else { "shutdown now" };
            let _ = std::process::Command::new("sh").arg("-c").arg(command).status();
        }),
    );
}

// ---- Drawing and running the Hardware dialogs ----

fn render_device(app: &MyApp, device: &Device) -> String {
    match device {
        Device::Test(test) => {
            let running = app.ui.devices.hardware.test.as_ref().is_some_and(|r| &r.target == test);
            let busy = app.ui.devices.hardware.test.is_some();
            let status =
                if running { "Testing…" } else { "Ready. The device runs its test output while the test runs." };
            let title = match test {
                Test::Wind => tr(app, "Wind"),
                Test::Led => tr(app, "LED"),
                Test::Tach => tr(app, "Tach"),
                Test::Iflag => tr(app, "IFlag"),
                Test::Haptic(wheel) => tf(app, "HapticR ^wheel^", &[("wheel", &wheel.to_string())]),
                Test::Rumble(port) => tf(app, "Port ^port^", &[("port", &port.to_string())]),
                Test::Speaker(speaker) => speaker.clone(),
            };
            let stop = !matches!(test, Test::Speaker(_));
            frame(
                app,
                &format!("{} · {}", t(app, "Output test"), esc(&title)),
                &format!(
                    "{}{}",
                    components::field(
                        &t(app, "Output test"),
                        &format!("<p id=\"test-status\" aria-live=\"polite\">{}</p>", t(app, status)),
                        ""
                    ),
                    components::field(
                        &t(app, "Controls"),
                        &format!(
                            "{} {}",
                            button(&t(app, "Start test"), "start", "primary", !busy && test_allowed(app, test)),
                            if stop { button(&t(app, "Stop"), "stop", "", running) } else { String::new() }
                        ),
                        ""
                    )
                ),
                None,
            )
        }
        Device::Duplicate { name, error, .. } => frame(
            app,
            &t(app, "Make a copy of this profile"),
            &format!(
                "{}{}",
                components::field(
                    &t(app, "Name"),
                    &input(
                        "layout-name",
                        &tr(app, "Name"),
                        "name",
                        "text",
                        name,
                        " maxlength=\"30\" required data-autofocus"
                    ),
                    ""
                ),
                error
                    .as_ref()
                    .map_or(String::new(), |e| format!("<p class=\"warning-text\" role=\"alert\">{}</p>", t(app, e)))
            ),
            Some((&t(app, "Confirm"), true)),
        ),
        Device::Remove { name } => frame(
            app,
            &t(app, "Delete this profile?"),
            &format!(
                "<p class=\"dialog-text\">{}</p>",
                tf(app, "Please confirm that this profile should be deleted: ^name^", &[("name", name)])
            ),
            Some((&t(app, "Delete"), true)),
        ),
        Device::Matrix { speaker, levels } => render_matrix(app, speaker, levels),
        Device::AxisAssignment { rows, actuators } => {
            let shown: Vec<String> = actuators.iter().map(|(_, shown)| shown.clone()).collect();
            let body: Vec<String> = rows
                .iter()
                .enumerate()
                .map(|(index, (label, selected))| {
                    choice(app, &format!("axis-{index}"), label, &format!("axis:{index}"), &shown, Some(*selected))
                })
                .collect();
            frame(
                app,
                &t(app, "Axis Assignment"),
                &components::more(app, &body, "axis-assignment"),
                Some((&t(app, "Confirm"), true)),
            )
        }
        Device::YawVr { fields } => render_yaw_vr(app, fields),
        Device::Thanos(dialog) => render_thanos(app, dialog),
        Device::Pta { ip, port, list, .. } => render_pta(app, ip, port, list),
        Device::Dof { update_kp, .. } => frame(
            app,
            "DOF REALITY",
            &components::field(
                &t(app, "Auto Set KI and KP"),
                &switch("dof-update-kp", "update_kp", *update_kp, &t(app, if *update_kp { "On" } else { "Off" }), true),
                "",
            ),
            Some((&t(app, "OK"), true)),
        ),
        Device::Vnm { actuators, reverse } => {
            frame(app, "VNM SIMULATION", &actuator_table(app, actuators, "Port", reverse), Some((&t(app, "OK"), true)))
        }
        Device::Controller { wheel_id, confirm } => render_controller(app, wheel_id, *confirm),
    }
}

fn act_device(app: &mut MyApp, device: Device, act: &str, arg: &str, value: &str) -> Result<(), String> {
    if let Device::Thanos(dialog) = device {
        return thanos_act(app, *dialog, act, arg, value);
    }
    if let Device::Pta { ip, port, list, attached } = device {
        return pta_act(app, (ip, port, list, attached), act, arg, value);
    }
    match (act, device) {
        ("dialog-close", Device::Controller { wheel_id, confirm: Some(_) }) => {
            update_device(app, Device::Controller { wheel_id, confirm: None });
        }
        ("dialog-close", _) => dialogs::close(app),
        ("dialog-button", Device::Test(test)) => match arg {
            "start" if test_allowed(app, &test) => hardware::start_test(app, &test)?,
            "stop" => hardware::finish_test(app)?,
            _ => {}
        },
        ("dialog-field", Device::Duplicate { source, .. }) if arg == "name" => {
            update_device(app, Device::Duplicate { source, name: value.to_owned(), error: None });
        }
        ("dialog-ok", Device::Duplicate { source, name, .. }) => {
            // The name is taken as typed (an empty one too); only a layout that exists already is refused.
            if app.backend.metadata_contains("shakers", &name).map_err(|failure| failure.detail)? {
                let error = Some("A profile with this name exists already; choose another name.".to_owned());
                update_device(app, Device::Duplicate { source, name, error });
                return Ok(());
            }
            dialogs::close(app);
            app.backend.copy_shaker_profile(&source, &name).map_err(|error| error.detail)?;
            app.backend.load_shaker_effects().map_err(|error| error.detail)?;
            app.docs.settings = app.backend.assign_shaker_profile(&name).map_err(|error| error.detail)?;
            notices::confirmation(app, "Make a copy of this profile", &name);
        }
        ("dialog-ok", Device::Remove { name }) => {
            dialogs::close(app);
            app.backend.delete_shaker_profile(&name).map_err(|error| error.detail)?;
            app.docs.settings =
                app.backend.assign_shaker_profile(hardware::DEFAULT_LAYOUT).map_err(|error| error.detail)?;
            app.backend.load_shaker_effects().map_err(|error| error.detail)?;
            hardware::choose_layout(app, hardware::DEFAULT_LAYOUT)?;
        }
        ("dialog-field", Device::Matrix { speaker, mut levels }) => {
            if let Some(effect) = arg.strip_prefix("level:") {
                levels
                    .assign_item(effect, LegacyValue::Int(read::int(value)?))
                    .map_err(|error| format!("TypeError: {error}"))?;
            }
            update_device(app, Device::Matrix { speaker, levels });
        }
        ("dialog-ok", Device::Matrix { speaker, levels }) => {
            dialogs::close(app);
            app.backend.confirm_shaker_levels(Some(&speaker), &levels).map_err(|error| error.detail)?;
        }
        ("dialog-field", Device::AxisAssignment { mut rows, actuators }) => {
            let index: usize = arg.trim_start_matches("axis:").parse().unwrap_or(usize::MAX);
            if let (Some(row), Ok(selected)) = (rows.get_mut(index), value.parse()) {
                row.1 = selected;
            }
            update_device(app, Device::AxisAssignment { rows, actuators });
        }
        ("dialog-ok", Device::AxisAssignment { rows, actuators }) => {
            dialogs::close(app);
            confirm_axis_assignment(app, &rows, &actuators)?;
        }
        ("dialog-field", Device::YawVr { fields }) => {
            yaw_field(app, fields, arg.trim_start_matches("yaw:"), value)?;
        }
        ("dialog-ok", Device::YawVr { fields }) => {
            dialogs::close(app);
            confirm_yaw_vr(app, &fields)?;
        }
        ("dialog-field", Device::Dof { .. }) if arg == "update_kp" => {
            update_device(app, Device::Dof { update_kp: read::checked(value), changed: true });
        }
        ("dialog-ok", Device::Dof { update_kp, changed }) => {
            dialogs::close(app);
            if changed {
                app.set_setting_native_checked("DOF REALITY", "update_kp", &LegacyValue::Bool(update_kp))?;
            }
        }
        ("dialog-field", Device::Vnm { actuators, mut reverse }) => {
            let port: i64 = arg.trim_start_matches("reverse:").parse().unwrap_or(0);
            if let Some(entry) = reverse.iter_mut().find(|(p, _)| *p == port) {
                entry.1 = read::checked(value);
            }
            update_device(app, Device::Vnm { actuators, reverse });
        }
        ("dialog-ok", Device::Vnm { reverse, .. }) => {
            dialogs::close(app);
            for (port, on) in reverse {
                app.set_setting_native_checked(
                    "VNM SIMULATION",
                    &format!("reverse_controller_port_{port}"),
                    &LegacyValue::Bool(on),
                )?;
            }
        }
        ("dialog-field", Device::Controller { confirm, .. }) if arg == "wheel" => {
            app.set_setting(&["magicbox", "wheel_scan_code"], serde_json::Value::String(value.to_owned()));
            update_device(app, Device::Controller { wheel_id: value.to_owned(), confirm });
        }
        ("dialog-button", Device::Controller { wheel_id, .. }) if matches!(arg, "reboot" | "shutdown") => {
            update_device(app, Device::Controller { wheel_id, confirm: Some(arg == "reboot") });
        }
        ("dialog-ok", Device::Controller { confirm: Some(reboot), .. }) => {
            dialogs::close(app);
            magicbox_command(app, reboot);
        }
        _ => {}
    }
    Ok(())
}

/// The open Hardware dialog, for tests.
pub fn device(app: &MyApp) -> Option<&Device> {
    current_device(app)
}

#[cfg(test)]
#[path = "dialogs_hardware_tests.rs"]
mod tests;
