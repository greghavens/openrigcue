//! Tuning → Motion, the reference screen: every row of the Motion page description, the rules that show,
//! hide and enable rows by motion model, connection and game, the stand-by lock, the header actions
//! (platform centring, shortcuts, hardware set-up), the row buttons (help, Autotune, telemetry, per-effect
//! shortcuts), the Autotune wizard and the motion hot keys.

use std::sync::{Mutex, Weak};
use std::time::Duration;

use openrigcue_config::catalog::registry::Registry;
use openrigcue_model::native_value::LegacyValue;
use openrigcue_model::python_compare::native_equal;
use serde_json::Value;

use super::super::components::{self, RowState};
use super::super::dialogs::{self, Dialog};
use super::super::html::{esc, num, t, tr};
use super::super::read;
use super::super::{notices, themes, Region, Screen};
use crate::app::MyApp;
use crate::backend::{at, native_field_owned, native_int, num as json_num, text, Backend, Method};
use crate::catalog::ModelFlags;
use crate::spec::{self, Action, Control, Field, Widget, Write};

/// The driving-style preferences of the Max Telemetry row.
const PREFERENCES: [&str; 3] = ["Soft", "Medium", "Aggressive"];

/// The key in [`super::tuning::State::motion_hidden`] of the Max Telemetry row's driving-style choice.
pub const PREFERENCE_PICKER: &str = "intellitune_preference";

/// The driving-style preferences: name, `effect_min`, `effect_max`, `smoothing`, in order.
pub const MOTION_PREFERENCE: [(&str, i64, i64, i64); 3] =
    [("Soft", 10, 12, 20), ("Medium", 14, 16, 15), ("Aggressive", 17, 19, 10)];

/// The telemetry channel Autotune samples for each effect.
pub const MOTION_MAPPING_VARS: [(&str, &str); 9] = [
    ("pitch", "pitch"),
    ("roll", "roll"),
    ("yaw", "yaw"),
    ("sway", "lateral_acceleration"),
    ("surge_acceleration", "longitudinal_acceleration"),
    ("surge_deceleration", "longitudinal_acceleration"),
    ("heave", "vertical_acceleration"),
    ("traction_loss", "lateral_velocity"),
    ("traction_loss_front", "lateral_velocity"),
];

/// A random integer in `a..=b`.
pub fn randint(a: i64, b: i64) -> i64 {
    use openrigcue_cueing::EffectRng;
    openrigcue_cueing::SharedRandom.randint(i128::from(a), i128::from(b)) as i64
}

/// The rounded mean (ties to even) of the samples. Float summation uses Neumaier compensation; integer items in
/// that path are added directly.
pub fn rounded_mean(samples: &LegacyValue) -> Result<i64, String> {
    let values: Vec<LegacyValue> = match samples {
        LegacyValue::List(values) | LegacyValue::Tuple(values) | LegacyValue::Set(values) => values.clone(),
        LegacyValue::Str(value) => value.chars().map(|value| LegacyValue::Str(value.to_string())).collect(),
        LegacyValue::Bytes(value) => value.iter().map(|value| LegacyValue::Int(i64::from(*value))).collect(),
        LegacyValue::Dict(values) => values.keys().map(|key| key.to_value()).collect(),
        value => return Err(format!("TypeError: '{}' object is not iterable", value.type_name())),
    };
    let mean = match openrigcue_model::python_numeric::sum_values(&values)? {
        openrigcue_model::python_numeric::NumericSum::Float(total) => total / values.len() as f64,
        openrigcue_model::python_numeric::NumericSum::Int(integer) => {
            openrigcue_model::python_numeric::int_true_divide(integer, values.len() as i128)
                .ok_or("ZeroDivisionError: division by zero")?
        }
    };
    native_int(&LegacyValue::Float(mean.round_ties_even()))
}

/// The models whose overrides hide the main actuators and a list of effects.
const STR_MODELS: [&str; 2] = ["STR-SCN6150", "STR-SCN6150-TL"];

/// The effects (and their smoothing rows) a STR-SCN6150 platform has no use for.
pub const STR_HIDDEN: [&str; 12] = [
    "yaw",
    "heave",
    "traction_loss",
    "traction_loss_front",
    "wheel_slip",
    "rumble_strips",
    "afterburner",
    "cannon_firing",
    "canopy",
    "landing_gear",
    "missile_firing",
    "abs_lock",
];

/// The effects a STR-SCN6150-TL platform has no use for.
pub const STR_TL_HIDDEN: [&str; 10] = [
    "heave",
    "traction_loss_front",
    "wheel_slip",
    "rumble_strips",
    "afterburner",
    "cannon_firing",
    "canopy",
    "landing_gear",
    "missile_firing",
    "abs_lock",
];

/// The `smoothing_slider_step` of the custom features (1 when unset).
pub fn smoothing_step(app: &MyApp) -> f64 {
    app.backend
        .native_setting("CUSTOM_FEATURES", "smoothing_slider_step")
        .ok()
        .and_then(|value| native_int(&value).ok())
        .filter(|step| *step > 0)
        .map_or(1.0, |step| step as f64)
}

/// Whether the motion model is a six-DOF platform (H6 or P6).
pub fn six_dof(app: &MyApp) -> bool {
    matches!(
        app.backend.native_setting("MOTIONBOX", "model"),
        Ok(LegacyValue::Str(model)) if model == "H6" || model == "P6"
    )
}

/// What the rows depend on, read once per render.
struct Rules {
    model: Option<&'static ModelFlags>,
    model_name: String,
    /// The tune is locked: no game runs and editing without one is off.
    locked: bool,
    /// The stand-by notice was hidden for good.
    notice_hidden: bool,
    racing: bool,
    /// A Yaw VR is connected (its Center action shows).
    yaw_connected: bool,
    /// A Yaw VR whose yaw limit is off: the yaw rows are disabled.
    yaw_limited: bool,
    /// The Thanos controller has a configuration.
    thanos_configured: bool,
}

impl Rules {
    fn read(app: &MyApp) -> Rules {
        let model = crate::catalog::current_model(&*app.backend).ok();
        let model_name = match app.backend.native_setting("MOTIONBOX", "model") {
            Ok(LegacyValue::Str(name)) => name,
            _ => String::new(),
        };
        let standby = app.ui.session.game_key == "none";
        let editable = app
            .backend
            .native_setting("CUSTOM_FEATURES", "allow_edit_emulator")
            .ok()
            .and_then(|value| crate::backend::native_bool(&value).ok())
            .unwrap_or(false);
        let notice_hidden = app
            .backend
            .native_setting("APP", "disable_tuning_alert_standby")
            .ok()
            .and_then(|value| crate::backend::native_bool(&value).ok())
            .unwrap_or(false);
        let manufacturer = model.map(|m| m.manufacturer.as_str()).unwrap_or("");
        let status = app.backend.motion_parameter("motion_status").unwrap_or(LegacyValue::None);
        let yaw = manufacturer == "Yaw VR";
        let yaw_connected = yaw && native_equal(&status, &LegacyValue::Str("Connected".into()));
        let yaw_limited = yaw
            && native_equal(
                &app.backend.motion_parameter("yaw_limit_enabled").unwrap_or(LegacyValue::Int(1)),
                &LegacyValue::Int(0),
            );
        let thanos_configured = !native_equal(
            &app.backend.motion_parameter("thanos_config").unwrap_or(LegacyValue::Dict(Default::default())),
            &LegacyValue::Dict(Default::default()),
        );
        Rules {
            model,
            model_name,
            locked: standby && !editable,
            notice_hidden,
            racing: app.ui.session.racing,
            yaw_connected,
            yaw_limited,
            thanos_configured,
        }
    }

    fn manufacturer(&self) -> &str {
        self.model.map(|m| m.manufacturer.as_str()).unwrap_or("")
    }

    fn str_model(&self) -> bool {
        STR_MODELS.contains(&self.model_name.as_str())
    }

    fn str_hidden(&self, effect: &str) -> bool {
        match self.model_name.as_str() {
            "STR-SCN6150" => STR_HIDDEN.contains(&effect),
            "STR-SCN6150-TL" => STR_TL_HIDDEN.contains(&effect),
            _ => false,
        }
    }

    fn canbus(&self) -> bool {
        self.model_name.contains("CANBus")
    }

    fn main_actuators(&self) -> bool {
        self.model.is_some_and(|m| m.main_actuators) && !self.str_model()
    }

    /// The game feature rule of a row: `(visible, enabled)` for the running game's support of `column` (see
    /// [`super::tuning::feature`]).
    fn feature(&self, app: &MyApp, column: &str) -> (bool, bool) {
        super::tuning::feature(app, column)
    }
}

/// The actuator speed rows of a SIMTECHREVOLUTION platform (not in the page description).
fn actuator_rows() -> [Control; 2] {
    [
        Control::slider(
            "scn6_speed",
            "Actuator Speed",
            crate::slider::SliderScale::new(1.0, 10000.0, 1.0),
            Field::tune(&["scn6_speed"]),
            Write::Int,
        ),
        Control::slider(
            "scn6_acceleration",
            "Actuator Acceleration",
            crate::slider::SliderScale::new(1.0, 250.0, 1.0),
            Field::tune(&["scn6_acceleration"]),
            Write::Int,
        ),
    ]
}

/// The page description as shown: the actuator rows of a SIMTECHREVOLUTION platform follow Overall Smoothing.
fn page(app: &MyApp, rules: &Rules) -> spec::Page {
    let mut page = spec::motion_page(Registry::builtin(), smoothing_step(app));
    if app.ui.tuning.motion_hidden.contains(PREFERENCE_PICKER) {
        // Without a stored driving style the Max Telemetry row offers no choice, only its buttons.
        if let Some(row) = page.sections.iter_mut().flat_map(|s| s.controls.iter_mut()).find(|c| c.id == "intellitune")
        {
            row.widget = Widget::Readout;
        }
    }
    if rules.manufacturer() == "SIMTECHREVOLUTION" {
        if let Some(platform) = page.sections.first_mut() {
            let at = platform.controls.iter().position(|c| c.id == "motion_smoothing_factor").map_or(0, |i| i + 1);
            for (offset, control) in actuator_rows().into_iter().enumerate() {
                platform.controls.insert(at + offset, control);
            }
        }
    }
    page
}

/// The Max Telemetry key of an effect: pitch and roll follow their rate maximum while they follow the rate.
pub fn max_key(app: &MyApp, effect: &str) -> String {
    if matches!(effect, "pitch" | "roll") && text(app.tune(&["motion_effects", &format!("{effect}_type")])) == "rate" {
        format!("{effect}_rate")
    } else {
        effect.to_owned()
    }
}

/// The stored Max Telemetry value an effect's box shows.
pub fn max_value(app: &MyApp, effect: &str) -> serde_json::Value {
    app.tune(&["max_telemetry_vars", &max_key(app, effect)]).clone()
}

fn is_effect(id: &str) -> bool {
    Registry::builtin().motion_effects.iter().any(|e| e.id == id)
}

/// The state of a row, `None` when the rules hide it.
fn row_state(app: &MyApp, rules: &Rules, control: &Control) -> Option<RowState> {
    let id = control.id.as_str();
    let mut state = RowState { enabled: !rules.locked, row_enabled: !rules.locked, ..RowState::default() };
    if let Some(field) = &control.field {
        state.value = read::field(app, field);
    }
    if let Widget::Range { high, .. } = &control.widget {
        state.high = read::field(app, high);
    }
    match id {
        "motion_max_horizontal" if !rules.main_actuators() => return None,
        "motion_smoothing_factor" if rules.canbus() || rules.str_model() => return None,
        "reaction_speed" => {
            if !rules.model.is_some_and(|m| m.show_reaction_speed) {
                return None;
            }
            if rules.canbus() {
                state.label = Some(t(app, "Action Mode"));
                state.endpoints = Some((t(app, "Smooth"), t(app, "Vivid")));
            }
        }
        "intellitune" if !rules.racing => {
            // Autotune is for racing games; the row's other buttons stay.
            let kept = control
                .buttons
                .iter()
                .enumerate()
                .filter(|(_, b)| !matches!(b, Widget::Button { run: Action::Intellitune, .. }))
                .map(|(i, _)| i)
                .collect();
            state.buttons = Some(kept);
        }
        "refresh_rate" if !rules.model.is_some_and(|m| m.display_motion_rate) => return None,
        "min_traction_loss_activation" | "min_traction_loss_front_activation" if !rules.racing || rules.str_model() => {
            return None
        }
        "gear_smoothing_factor" if !rules.racing => return None,
        "belt_tensioner" if !rules.model.is_some_and(|m| m.belt_tensioner_motor) => return None,
        "roll_type" | "pitch_type" => {
            if app.ui.tuning.motion_hidden.contains(id) {
                return None;
            }
            // Anything but the angle reads as the rate.
            if state.value != Value::String("angle".into()) {
                state.value = Value::String("rate".into());
            }
        }
        "enhanced_gforces" => {
            if !rules.model.is_some_and(|m| m.enhanced_gforces) {
                return None;
            }
        }
        "thanos_spike_filter_vertical" | "thanos_spike_filter_horizontal" | "thanos_alert" => {
            if rules.manufacturer() != "THANOS" {
                return None;
            }
            if id == "thanos_alert" && rules.thanos_configured {
                return None;
            }
            state.enabled &= rules.thanos_configured;
            state.row_enabled &= rules.thanos_configured;
            if id != "thanos_alert" {
                state.output = Some(spike_millimetres(app, &state.value));
            }
        }
        _ => {}
    }
    if let Some(axis) = id.strip_prefix("motion_main_actuators_") {
        if !rules.main_actuators() {
            return None;
        }
        let (visible, enabled) = rules.feature(app, axis);
        if !visible {
            return None;
        }
        state.enabled &= enabled;
        state.row_enabled &= enabled;
    }
    if let Some(axis) = id.strip_prefix("motion_smoothing_") {
        if rules.str_hidden(axis) {
            return None;
        }
        if axis != "factor" {
            let (visible, enabled) = rules.feature(app, axis);
            if !visible {
                return None;
            }
            state.row_enabled &= enabled;
            state.enabled &= enabled && !(axis == "yaw" && rules.yaw_limited);
        }
    }
    if is_effect(id) {
        if rules.str_hidden(id) {
            return None;
        }
        let (visible, enabled) = rules.feature(app, id);
        if !visible {
            return None;
        }
        let on = read::truth(app.tune(&["motion_switch", id]));
        state.toggle = Some(on);
        if control.max_telemetry.is_some() {
            state.max = Some(app.tune(&["max_telemetry_vars", &max_key(app, id)]).clone());
        }
        state.row_enabled &= enabled;
        state.enabled &= enabled && on && !(id == "yaw" && rules.yaw_limited);
    }
    if id == "intellitune" {
        // The value is the stored preference's position; a profile without one shows no selection.
        state.value = app.tune(&["intellitune_motion_preference"]).clone();
        if app.ui.tuning.motion_hidden.contains(PREFERENCE_PICKER) {
            // The driving-style choice is not offered (see `page`); the row's buttons stay.
            state.output = Some(String::new());
        }
    }
    Some(state)
}

/// The spike filter in millimetres of the vertical stroke, when the controller reports its stroke.
fn spike_millimetres(app: &MyApp, value: &Value) -> String {
    let manufacturer = app
        .backend
        .native_setting("MOTIONBOX", "model")
        .ok()
        .and_then(|value| match value {
            LegacyValue::Str(model) => crate::catalog::model(&model).map(|model| model.manufacturer.clone()),
            _ => None,
        })
        .unwrap_or_else(|| "PT-ACTUATOR".to_owned());
    let stroke = app
        .backend
        .native_setting(&manufacturer, "vertical_actuators_stroke")
        .ok()
        .and_then(|v| native_int(&v).ok())
        .unwrap_or(0);
    if stroke == 0 {
        return String::new();
    }
    let millimetres = components::number(value) / 65535.0 * stroke as f64;
    format!("{millimetres:.2} mm")
}

/// The body: the stand-by note, the selected settings group's rows and the page's actions.
pub fn main(app: &MyApp) -> String {
    let rules = Rules::read(app);
    let page = page(app, &rules);
    let unit = "";
    let visible: Vec<(usize, &spec::Section, Vec<String>)> = page
        .sections
        .iter()
        .enumerate()
        .filter_map(|(index, section)| {
            let rows = super::rows(app, Screen::Motion, &section.controls, unit, |c| row_state(app, &rules, c));
            (!rows.is_empty()).then_some((index, section, rows))
        })
        .collect();
    let Some(selected) = visible.iter().position(|(index, ..)| *index == app.ui.section).or(Some(0)) else {
        return String::new();
    };
    let Some((index, section, rows)) = visible.get(selected) else { return String::new() };
    let groups: Vec<(usize, String)> = visible.iter().map(|(i, s, _)| (*i, tr(app, section_title(&s.title)))).collect();
    let mut body = String::new();
    if rules.locked && !rules.notice_hidden {
        body.push_str(&format!(
            "<div class=\"screen-note\"><p>{}</p><button type=\"button\" class=\"text-button\" data-act=\"standby-hide\">{}</button></div>",
            t(app, "Tuning is unavailable in Stand By mode; start a game to tune."),
            t(app, "Hide this notice from now on")
        ));
    }
    body.push_str(&components::more(app, rows, &format!("motion:{index}")));
    let mut header: String = page
        .header
        .iter()
        .enumerate()
        .filter(|(_, button)| !matches!(button, Widget::Button { run: Action::YawVrCenter, .. }) || rules.yaw_connected)
        .filter_map(|(i, button)| match button {
            Widget::Button { text: label, .. } => {
                let hint =
                    page.header_hints.get(label).map(|h| format!(" title=\"{}\"", t(app, h))).unwrap_or_default();
                Some(format!(
                    "<button type=\"button\" data-act=\"action\" data-arg=\"Motion:@header:{i}\"{hint}>{}</button>",
                    t(app, label)
                ))
            }
            _ => None,
        })
        .collect();
    header.push_str(&format!(
        "<button type=\"button\" data-act=\"action\" data-arg=\"Motion:@library:\">{}</button>",
        t(app, "Downloaded profiles")
    ));
    body.push_str(&components::actions(app, &header, "motion"));
    components::panel(
        &themes::title(app, section_title(&section.title)),
        &super::section_picker(app, &groups, *index),
        &body,
        "tuning-panel",
    )
}

/// The heading of a settings group (the lower-case vertical-actuator group reads as a title).
fn section_title(title: &str) -> &str {
    match title {
        "effects on the vertical actuators" => "Effects on the vertical actuators",
        "CONTROLLER" => "Controller",
        other => other,
    }
}

/// The controls Compare looks at: every tune value of the page.
pub fn compared(app: &MyApp) -> Vec<(String, Vec<String>)> {
    let rules = Rules::read(app);
    page(app, &rules)
        .controls()
        .filter_map(|control| match &control.field {
            Some(Field { store: spec::Store::Tune, path }) => Some((tr(app, &control.label), path.clone())),
            _ => None,
        })
        .collect()
}

/// At start: the platform's refresh rate is stored as a whole number within 30..=1000.
pub fn start(app: &mut MyApp) -> Result<(), String> {
    let stored = app.backend.native_setting("MOTIONBOX", "refresh_rate").map_err(|error| error.detail)?;
    let Ok(rate) = native_int(&stored) else { return Ok(()) };
    let rate = LegacyValue::Int(rate.clamp(30, 1000));
    if stored != rate {
        app.set_motion_setting_native_checked("refresh_rate", &rate)?;
    }
    Ok(())
}

/// Whether the row `control` takes a change of `part` now: it is shown, and neither the stand-by lock, the
/// running game's feature rule nor its own rules disable that part.
fn accepts(app: &MyApp, control: &str, part: &str) -> bool {
    if control == "intellitune" && part.is_empty() && app.ui.tuning.motion_hidden.contains(PREFERENCE_PICKER) {
        return false;
    }
    let rules = Rules::read(app);
    super::tuning::accepts(&page(app, &rules), control, part, |row| row_state(app, &rules, row))
}

/// The state of the row `id` as this screen draws it, for the start screen's Motion tab; `None` when the rules
/// hide it or the page has no such row.
pub fn shown_state(app: &MyApp, id: &str) -> Option<RowState> {
    let rules = Rules::read(app);
    row_state(app, &rules, page(app, &rules).control(id)?)
}

/// A value of the screen changed: the stored value its row stands for, when the row takes it.
pub fn set(app: &mut MyApp, control: &str, part: &str, value: &str) -> Result<(), String> {
    if !accepts(app, control, part) {
        return Ok(());
    }
    write(app, control, part, value)
}

/// The range of the Max Telemetry boxes (as the page has them).
pub const MAX_TELEMETRY_RANGE: (i64, i64) = (-1800, 1800);
/// What a Max Telemetry box stores for a value put into it: a whole number within [`MAX_TELEMETRY_RANGE`]. A
/// value that is no number is an error.
fn max_telemetry(value: &LegacyValue) -> Result<i64, String> {
    let (min, max) = MAX_TELEMETRY_RANGE;
    let shown = LegacyValue::Str(crate::backend::native_str(value)?);
    read::NumberBox::new(min as f64, max as f64, 1.0).take_whole(value, &shown)
}

/// The range of the traction loss activation thresholds (as the page has them).
pub const ACTIVATION_RANGE: (i64, i64) = (0, 1800);
/// The range of the spike filters (as the page has them).
pub const SPIKE_FILTER_RANGE: (i64, i64) = (255, 64530);

/// The stored value a row's change writes (for the screen, the hot keys and the profile load alike). The
/// number boxes' values arrive kept within their limits ([`super::set`]).
pub fn write(app: &mut MyApp, control: &str, part: &str, value: &str) -> Result<(), String> {
    let int = |value: &str| read::int(value).map(LegacyValue::Int);
    match (control, part) {
        (effect, "toggle") if is_effect(effect) => {
            app.set_tune_native_checked(&["motion_switch", effect], &LegacyValue::Bool(read::checked(value)))
        }
        (effect, "max") if is_effect(effect) => {
            let key = max_key(app, effect);
            app.set_tune_native_checked(&["max_telemetry_vars", &key], &int(value)?)
        }
        ("motion_max_power" | "motion_max_horizontal" | "motion_smoothing_factor" | "motion_boost_factor", "") => {
            app.set_tune_native_checked(&[control], &read::number(value)?)
        }
        ("scn6_speed" | "scn6_acceleration", "") => app.set_tune_native_checked(&[control], &int(value)?),
        ("reaction_speed", "") => {
            app.set_tune_native_checked(&["motion_reaction_speed"], &int(value)?)?;
            app.set_motion_setting_native_checked("reaction_speed", &int(value)?)
        }
        ("intellitune", "") => {
            let index: usize = value.parse().map_err(|_| format!("ValueError: {value:?}"))?;
            let preference = PREFERENCES.get(index).ok_or_else(|| format!("KeyError: {value:?}"))?;
            driving_style(app, preference)
        }
        (effect, "") if is_effect(effect) => {
            app.set_tune_native_checked(&["motion_effects", effect], &read::number(value)?)
        }
        ("refresh_rate", "") => app.set_motion_setting_native_checked("refresh_rate", &int(value)?),
        (
            "min_traction_loss_activation"
            | "min_traction_loss_front_activation"
            | "gear_smoothing_factor"
            | "thanos_spike_filter_vertical"
            | "thanos_spike_filter_horizontal",
            "",
        ) => app.set_tune_native_checked(&[control], &int(value)?),
        ("belt_tensioner", "") => app.set_tune_native_checked(&["motion_belt_tensioner"], &int(value)?),
        ("roll_type" | "pitch_type", "") => {
            let axis = control.trim_end_matches("_type");
            let kind = match value {
                "0" => "angle",
                "1" => "rate",
                other => return Err(format!("KeyError: {other:?}")),
            };
            app.set_tune_native_checked(&["motion_effects", control], &LegacyValue::Str(kind.into()))?;
            // The axis's Max Telemetry box now takes the maximum of the chosen kind (it must exist) and stores it
            // as the box holds it.
            let key = if kind == "rate" { format!("{axis}_rate") } else { axis.to_owned() };
            let tune = app.backend.native_tune().map_err(|error| error.detail)?;
            let max = max_telemetry(&native_field_owned(&tune, &["max_telemetry_vars", &key])?)?;
            app.set_tune_native_checked(&["max_telemetry_vars", &key], &LegacyValue::Int(max))
        }
        ("enhanced_gforces", "") => app
            .set_tune_native_checked(&["motion_enhanced_gforces"], &LegacyValue::Int(i64::from(read::checked(value)))),
        (axis, "") if axis.starts_with("motion_smoothing_") => {
            let axis = axis.trim_start_matches("motion_smoothing_");
            app.set_tune_native_checked(&["motion_effect_smoothing_factor", axis], &int(value)?)
        }
        (row, "") if row.starts_with("motion_main_actuators_") => app.set_tune_native_checked(&[row], &int(value)?),
        _ => Err(format!("no Motion value {control:?}")),
    }
}

/// A driving style was chosen: it is stored, the effects take random values within its range (yaw 0) and the
/// smoothing rows its smoothing.
pub fn driving_style(app: &mut MyApp, preference: &str) -> Result<(), String> {
    app.set_tune_native_checked(&["intellitune_motion_preference"], &LegacyValue::Str(preference.into()))?;
    let &(_, low, high, smoothing) = MOTION_PREFERENCE
        .iter()
        .find(|(name, ..)| *name == preference)
        .ok_or_else(|| format!("KeyError: {preference:?}"))?;
    for effect in ["surge_acceleration", "surge_deceleration", "sway", "traction_loss", "roll", "heave", "pitch", "yaw"]
    {
        let value = if effect == "yaw" { 0 } else { randint(low, high) };
        app.set_tune_native_checked(&["motion_effects", effect], &LegacyValue::Int(value))?;
    }
    // The smoothing sliders hold the style's smoothing rounded to their step.
    let scale = crate::slider::SliderScale::new(0.0, 100.0, smoothing_step(app));
    let smoothing = scale.snap(smoothing as f64).trunc() as i64;
    for axis in ["surge", "sway", "traction_loss", "roll", "heave", "pitch", "yaw"] {
        app.set_tune_native_checked(&["motion_effect_smoothing_factor", axis], &LegacyValue::Int(smoothing))?;
    }
    Ok(())
}

/// A button of the screen: a header action (`@header`) or a row's button.
pub fn action(app: &mut MyApp, control: &str, index: Option<usize>) -> Result<(), String> {
    if control == "@library" {
        super::tuning::open_library(app, openrigcue_config::synced_profiles::Category::Motion);
        return Ok(());
    }
    let rules = Rules::read(app);
    let page = page(app, &rules);
    let button = if control == "@header" {
        index.and_then(|i| page.header.get(i)).cloned()
    } else if !super::tuning::accepts_button(&page, control, index, |row| row_state(app, &rules, row)) {
        return Ok(());
    } else {
        page.control(control).and_then(|c| index.and_then(|i| c.buttons.get(i)).cloned())
    };
    let Some(Widget::Button { run, .. }) = button else { return Err(format!("no Motion button {control:?}")) };
    run_action(app, &run)
}

/// What a Motion button does.
fn run_action(app: &mut MyApp, run: &Action) -> Result<(), String> {
    match run {
        // Tuning Help opens the help.
        Action::Goto { left, .. } if left == "App" => super::super::navigate(app, Screen::Help, 0),
        Action::Goto { left, .. } if left == "Telemetry" => super::super::navigate(app, Screen::Telemetry, 0),
        Action::Goto { left, .. } => super::super::navigate(app, Screen::from_key(left).unwrap_or(Screen::Hardware), 0),
        Action::Intellitune => start_wizard(app)?,
        Action::Shortcuts { actions } => super::super::shortcuts::open(app, actions)?,
        Action::YawVrCenter => center(app)?,
        other => return Err(format!("unsupported Motion action {other:?}")),
    }
    Ok(())
}

/// Move platform to center: a connected Yaw VR is told to centre.
pub fn center(app: &mut MyApp) -> Result<(), String> {
    let status = app.backend.motion_parameter("motion_status").map_err(|error| error.detail)?;
    if status == LegacyValue::Str("Connected".into()) {
        app.backend.request_yaw_center().map_err(|error| error.detail)?;
    }
    notices::success(app, "Success", "YawVR was told to center");
    Ok(())
}

/// The motion hot keys: each `motion_box_<row>_up` / `_down` action moves its value by one step, within the
/// row's range, through the same writes as the screen. The main loop calls it while a game runs (its hot-key
/// block checks the game once for every page's keys).
pub fn apply_hotkeys(app: &mut MyApp, pending: &dyn Fn(&str) -> bool) -> Result<(), String> {
    let page = spec::motion_page(Registry::builtin(), smoothing_step(app));
    let mut steps: Vec<(String, String)> = Vec::new();
    for effect in &Registry::builtin().motion_effects {
        steps.push((format!("motion_box_{}", effect.id), effect.id.clone()));
    }
    for key in [
        "motion_max_power",
        "motion_max_horizontal",
        "motion_smoothing_factor",
        "reaction_speed",
        "motion_boost_factor",
    ] {
        steps.push((format!("motion_box_{key}"), key.to_owned()));
    }
    for axis in spec_axes() {
        steps.push((format!("motion_box_smoothing_{axis}"), format!("motion_smoothing_{axis}")));
    }
    for (action, row) in steps {
        for (suffix, delta) in [("up", 1.0), ("down", -1.0)] {
            if !pending(&format!("{action}_{suffix}")) {
                continue;
            }
            let Some(control) = page.control(&row) else { continue };
            let (Widget::Slider { scale }, Some(field)) = (&control.widget, &control.field) else { continue };
            let current = components::number(&read::field(app, field));
            let next = scale.snap(current + delta);
            write(app, &row, "", &num(next))?;
        }
    }
    super::super::refresh(app, &[Region::Main, Region::Save]);
    Ok(())
}

/// The smoothing axes of the page.
fn spec_axes() -> [&'static str; 8] {
    ["pitch", "roll", "yaw", "sway", "surge", "heave", "traction_loss", "traction_loss_front"]
}

/// [`apply_hotkeys`] for the main loop's shared application.
pub(crate) fn apply_hotkeys_shared(shared: &Mutex<crate::app::MyApp>) -> Result<(), String> {
    let backend = crate::host::lock(shared).backend.clone();
    let pending = |action: &str| backend.ui_action_pending(action);
    apply_hotkeys(&mut crate::host::lock(shared), &pending)
}

// ---- Autotune --------------------------------------------------------------------------------------------

/// Where the Autotune wizard is.
#[derive(Debug, Clone, PartialEq)]
pub enum WizardStep {
    /// Step 1: what Autotune does.
    Intro,
    /// Step 2 before the capture: the count-down's seconds (`None` while the starting prompt plays).
    Countdown(Option<i64>),
    /// Step 2 during the capture: the seconds left.
    Capture(i64),
    /// Step 3: each effect's current and suggested maximum, and the preference.
    Review { rows: Vec<(String, String, i64)>, preference: usize },
}

/// Plays a voice clip in the background.
fn clip(backend: &dyn Backend, name: &str, wait: bool) {
    let path = if wait { format!("voice/clips/{name}?wait=true") } else { format!("voice/clips/{name}") };
    if let Err(error) = backend.call(Method::Post, &path, None) {
        tracing::debug!(%error, name, "voice clip");
    }
}

/// Autotune: the welcome prompt and step 1. The running game must be in the catalogue.
pub fn start_wizard(app: &mut MyApp) -> Result<(), String> {
    clip(&*app.backend, "intellitune_welcome", false);
    let game = match app.backend.game_value().map_err(|error| error.detail)? {
        LegacyValue::Str(game) => game,
        other => return Err(format!("KeyError: {}", openrigcue_model::python_repr::repr(&other))),
    };
    if openrigcue_config::catalog::Catalog::builtin().game(&game).is_none() {
        return Err(format!("KeyError: {}", openrigcue_model::python_repr::repr(&LegacyValue::Str(game))));
    }
    dialogs::open(app, Dialog::Wizard(WizardStep::Intro));
    Ok(())
}

/// Whether the wizard is still open (the timer thread stops when it is not).
fn wizard_open(app: &MyApp) -> bool {
    let running = |step: Option<&WizardStep>| matches!(step, Some(WizardStep::Countdown(_) | WizardStep::Capture(_)));
    let shown = match &app.ui.dialog {
        Some(Dialog::Wizard(step)) => Some(step),
        _ => None,
    };
    running(shown) || running(app.ui.tuning.wizard_behind.as_ref())
}

/// The wizard moves on to `step`, where it is: open, or behind another dialog.
fn set_step(app: &mut MyApp, step: WizardStep) {
    if let Some(behind) = app.ui.tuning.wizard_behind.as_mut() {
        *behind = step;
        return;
    }
    app.ui.dialog = Some(Dialog::Wizard(step));
    super::super::refresh(app, &[Region::Dialog]);
}

/// The wizard ends, where it is: open, or behind another dialog.
fn close_wizard(app: &mut MyApp) {
    if app.ui.tuning.wizard_behind.take().is_none() {
        dialogs::close(app);
    }
}

/// Step 1 confirmed: step 2 and its timer: the starting prompt, a ten-second spoken count-down, then the
/// capture for `intelli_tune_capture_secs`; then step 3 opens.
pub fn wizard_next(app: &mut MyApp) -> Result<(), String> {
    match app.ui.dialog.clone() {
        Some(Dialog::Wizard(WizardStep::Intro)) => {
            set_step(app, WizardStep::Countdown(None));
            app.start_thread("autotune_timer", Box::new(wizard_timer));
            Ok(())
        }
        Some(Dialog::Wizard(WizardStep::Review { rows, preference })) => {
            // Each suggestion goes into its Max Telemetry box, which holds it within its limits.
            for (effect, _, suggested) in &rows {
                let key = max_key(app, effect);
                let max = max_telemetry(&LegacyValue::Int(*suggested))?;
                app.set_tune_native_checked(&["max_telemetry_vars", &key], &LegacyValue::Int(max))?;
            }
            driving_style(app, PREFERENCES[preference.min(2)])?;
            // The stored style is offered from now on.
            app.ui.tuning.motion_hidden.remove(PREFERENCE_PICKER);
            dialogs::close(app);
            notices::confirmation(app, "Autotune", "Autotune complete");
            Ok(())
        }
        _ => Ok(()),
    }
}

/// The wizard was cancelled: the samples are cleared and the capture switched off.
pub fn wizard_cancel(app: &mut MyApp) -> Result<(), String> {
    app.backend.clear_intellitune_samples().map_err(|error| error.detail)?;
    app.backend.set_intellitune_capture(false).map_err(|error| error.detail)
}

/// The wizard's timer thread.
fn wizard_timer(app: Weak<Mutex<MyApp>>) {
    let step = |app: &Weak<Mutex<MyApp>>, run: &dyn Fn(&mut MyApp)| -> bool {
        let Some(shared) = app.upgrade() else { return false };
        let mut app = crate::host::lock(&shared);
        if !wizard_open(&app) {
            return false;
        }
        run(&mut app);
        true
    };
    let Some(backend) = app.upgrade().map(|shared| crate::host::lock(&shared).backend.clone()) else { return };
    clip(&*backend, "intellitune_starting", true);
    for second in (1..=10).rev() {
        if !step(&app, &|app| set_step(app, WizardStep::Countdown(Some(second)))) {
            return;
        }
        clip(&*backend, &second.to_string(), false);
        std::thread::sleep(Duration::from_secs(1));
    }
    let begin = || -> Result<i64, String> {
        backend.clear_intellitune_samples().map_err(|error| error.detail)?;
        backend.set_intellitune_capture(true).map_err(|error| error.detail)?;
        let duration =
            backend.native_setting("MOTIONBOX", "intelli_tune_capture_secs").map_err(|error| error.detail)?;
        native_int(&duration)
    };
    let mut left = match begin() {
        Ok(duration) => duration,
        Err(error) => {
            tracing::error!(error, "autotune capture failed");
            return;
        }
    };
    while left > 0 {
        if !step(&app, &|app| set_step(app, WizardStep::Capture(left))) {
            return;
        }
        std::thread::sleep(Duration::from_secs(1));
        left -= 1;
    }
    if let Err(error) = backend.set_intellitune_capture(false) {
        tracing::error!(%error, "autotune capture failed");
        return;
    }
    clip(&*backend, "intellitune_captured", false);
    let Some(shared) = app.upgrade() else { return };
    let mut app = crate::host::lock(&shared);
    if !wizard_open(&app) {
        return;
    }
    match suggestions(&app) {
        Ok(rows) => set_step(&mut app, WizardStep::Review { rows, preference: 1 }),
        Err(error) => {
            close_wizard(&mut app);
            let message = app.backend.translate("An internal error occurred: ^detail^").replace("^detail^", &error);
            notices::warning(&mut app, "Autotune", &message);
        }
    }
}

/// Each Autotune effect's current maximum and the suggested one: the captured samples' mean averaged with
/// the factory profile's maximum, at least 1.
pub fn suggestions(app: &MyApp) -> Result<Vec<(String, String, i64)>, String> {
    let game = match app.backend.game_value().map_err(|error| error.detail)? {
        LegacyValue::Str(game) => game,
        other => return Err(format!("KeyError: {}", openrigcue_model::python_repr::repr(&other))),
    };
    let samples = app.backend.intellitune_samples().map_err(|error| error.detail)?;
    let factory = crate::catalog::factory_tune(&game).unwrap_or(Value::Null);
    let tune = app.backend.native_tune().map_err(|error| error.detail)?;
    let mut rows = Vec::new();
    for effect in &Registry::builtin().motion_effects {
        let key = effect.id.as_str();
        if !effect.intellitune || key == "wheel_slip" || key == "rumble_strips" {
            continue;
        }
        let channel = MOTION_MAPPING_VARS.iter().find(|(k, _)| *k == key).map_or(key, |(_, c)| *c);
        let measured = rounded_mean(&native_field_owned(&samples, &[channel])?)?;
        let default_max = at(&factory, &["max_telemetry_vars", key]);
        if default_max.is_null() {
            return Err(format!("KeyError: {}", openrigcue_model::python_repr::repr(&LegacyValue::Str(game))));
        }
        let suggested = ((measured as f64 + json_num(default_max).abs()) / 2.0).round_ties_even() as i64;
        let current = crate::backend::native_str(&native_field_owned(&tune, &["max_telemetry_vars", key])?)?;
        rows.push((key.to_owned(), current, suggested.max(1)));
    }
    Ok(rows)
}

/// The wizard's dialog.
pub fn wizard_dialog(app: &MyApp, step: &WizardStep) -> String {
    let (title, body, ok, ready) = match step {
        WizardStep::Intro => (
            t(app, "Autotune - Step 1/3"),
            format!(
                "<div class=\"dialog-text\"><p>{}</p><p>{}</p></div>",
                t(app, "Autotune records your driving telemetry and suggests motion settings."),
                t(app, "Press Next to begin")
            ),
            t(app, "Next"),
            true,
        ),
        WizardStep::Countdown(_) | WizardStep::Capture(_) => {
            let status = match step {
                WizardStep::Countdown(Some(n)) => {
                    format!("<p class=\"mono\">{}{}</p>", t(app, "Telemetry recording begins in "), format_time(*n))
                }
                WizardStep::Capture(n) => format!("<p class=\"mono\">{} {}</p>", t(app, "Time Left"), format_time(*n)),
                _ => String::new(),
            };
            (
                t(app, "Autotune - Step 2/3"),
                format!(
                    "<div class=\"dialog-text\"><p>{}</p><p>{}</p>{status}</div>",
                    t(app, "Get into the car and drive cleanly for 3 minutes."),
                    t(app, "OpenRigCue tells you when it is time to return to this wizard.")
                ),
                t(app, "Next"),
                false,
            )
        }
        WizardStep::Review { rows, preference } => {
            let options: Vec<(String, String)> =
                PREFERENCES.iter().enumerate().map(|(i, p)| (i.to_string(), tr(app, p))).collect();
            let table: String = rows
                .iter()
                .map(|(effect, current, suggested)| {
                    let label = Registry::builtin()
                        .motion_effects
                        .iter()
                        .find(|e| e.id == *effect)
                        .map_or(effect.clone(), |e| tr(app, &e.label));
                    format!(
                        "<tr><th scope=\"row\">{}</th><td class=\"mono\">{}</td><td class=\"mono\">{suggested}</td></tr>",
                        esc(&label),
                        esc(current)
                    )
                })
                .collect();
            (
                t(app, "Autotune - Step 3/3"),
                format!(
                    "{}<table class=\"data-table\"><thead><tr><th>{}</th><th>{}</th><th>{}</th></tr></thead><tbody>{table}</tbody></table>",
                    components::field(
                        &t(app, "Motion Preference"),
                        &components::select("wizard-preference", &tr(app, "Motion Preference"), &options, &preference.to_string(), "dialog-field", "preference"),
                        ""
                    ),
                    t(app, "Effect"),
                    t(app, "Current Max"),
                    t(app, "Suggested Max")
                ),
                t(app, "Apply"),
                true,
            )
        }
    };
    dialogs::frame(app, &title, &body, Some((&ok, ready)))
}

fn format_time(seconds: i64) -> String {
    format!("{:02}:{:02}", seconds.div_euclid(60), seconds.rem_euclid(60))
}

#[cfg(test)]
#[path = "motion_tests.rs"]
mod tests;
