//! The write-back of a newly loaded profile: every tuning control takes its stored value the way its widget
//! holds it (a slider rounds to its step and keeps within its range, a spin box keeps within its range, a range
//! orders and limits its two ends, a drop-down falls back to its last entry), and the control's own write
//! stores what the widget holds. The rows go in this order: the routines, SIM3D, HapticR, iFlag, the belt
//! switch, the early Motion rows (driving style, game set-up check, spike filters, smoothing, vertical
//! actuators), the Wind speed rows, Belt, the Wind fan rows, the SIM3D and HapticR advanced rows, the LED shift
//! point, the shift-light brightness, Shake, the shaker volumes, then the remaining Motion rows. A required value
//! that is missing or unusable stops the rows after it.
//!
//! A switch writes only when it is forced to a value (its widget already holds the stored value). Values that
//! are already what the widget would store are not written again.
//!
//! The LED segments go through the same treatment once at start ([`start`]).

use openrigcue_config::catalog::registry::Registry;
use openrigcue_config::catalog::{Catalog, GameKind};
use openrigcue_model::native_value::{Dict, LegacyValue};
use openrigcue_model::python_compare::native_equal;

use super::{belt, iflag, motion, routines, wind};
use crate::app::MyApp;
use crate::backend::{
    native_bool, native_field_owned, native_float, native_int, native_int_as_float, native_int_float,
};
use crate::slider::{RangeScale, SliderScale};

// ---- What every row shares ------------------------------------------------------------------------------

/// A stored tune value; a missing key is an error.
pub(crate) fn stored(app: &MyApp, path: &[&str]) -> Result<LegacyValue, String> {
    let tune = app.backend.native_tune().map_err(|error| error.detail)?;
    native_field_owned(&tune, path)
}

/// A value of a downloaded or stored tune; a missing key is an error.
pub(crate) fn field(tune: &LegacyValue, path: &[&str]) -> Result<LegacyValue, String> {
    native_field_owned(tune, path)
}

/// A number as the controls store it: a whole number as an integer.
pub(crate) fn as_number(value: f64) -> LegacyValue {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 {
        LegacyValue::Int(value as i64)
    } else {
        LegacyValue::Float(value)
    }
}

/// What a slider holds for a value: the value as a number, rounded to the slider's step within its range.
pub(crate) fn slider(scale: SliderScale, value: &LegacyValue) -> Result<f64, String> {
    Ok(scale.snap(native_float(value)?))
}

/// What a range slider holds for two values (numbers or booleans only): both rounded to its step, ordered and
/// kept within its limits.
pub(crate) fn range(scale: RangeScale, low: &LegacyValue, high: &LegacyValue) -> Result<(f64, f64), String> {
    let operand = |value: &LegacyValue| match value {
        LegacyValue::Int(value) => Ok(*value as f64),
        LegacyValue::Float(value) => Ok(*value),
        LegacyValue::Bool(value) => Ok(f64::from(u8::from(*value))),
        other => Err(format!("TypeError: unsupported operand type(s) for -: '{}' and 'float'", other.type_name())),
    };
    Ok(scale.snap(operand(low)?, operand(high)?))
}

/// What a spin box with the whole-number limits `min`…`max` stores for a value put into it
/// ([`crate::ui::read::NumberBox`]); `None` for a value that is no number (the box keeps what it showed).
pub(crate) fn spin(value: &LegacyValue, (min, max): (i64, i64)) -> Option<i64> {
    crate::ui::read::NumberBox::new(min as f64, max as f64, 1.0).take_whole(value, &LegacyValue::None).ok()
}

/// Whether two stored values are the same (numbers by value, a boolean is no number).
fn same(a: &LegacyValue, b: &LegacyValue) -> bool {
    match (a, b) {
        (LegacyValue::Int(_) | LegacyValue::Float(_), LegacyValue::Int(_) | LegacyValue::Float(_)) => {
            a.number() == b.number()
        }
        _ => a == b,
    }
}

/// Stores a tune value unless it is already stored.
pub(crate) fn put(app: &mut MyApp, path: &[&str], value: LegacyValue) -> Result<(), String> {
    if stored(app, path).is_ok_and(|current| same(&current, &value)) {
        return Ok(());
    }
    app.set_tune_native_checked(path, &value)
}

/// Stores a shaker table's value (which rebuilds the shaker channels) unless it is already stored.
pub(crate) fn put_shaker(app: &mut MyApp, table: &str, key: &str, value: LegacyValue) -> Result<(), String> {
    if stored(app, &[table, key]).is_ok_and(|current| same(&current, &value)) {
        return Ok(());
    }
    app.set_shaker_value_checked(table, key, &value)
}

/// Stores a setting unless it is already stored.
fn put_setting(app: &mut MyApp, section: &str, key: &str, value: LegacyValue) -> Result<(), String> {
    if app.backend.native_setting(section, key).is_ok_and(|current| same(&current, &value)) {
        return Ok(());
    }
    app.set_setting_native_checked(section, key, &value)
}

/// A switch forced to `on`: its write runs when the switch shows the other state (the stored value's truth).
pub(crate) fn force_switch(app: &mut MyApp, path: &[&str], on: bool, written: LegacyValue) -> Result<(), String> {
    let shown = stored(app, path).ok().and_then(|value| native_bool(&value).ok()).unwrap_or(false);
    if shown == on {
        return Ok(());
    }
    app.set_tune_native_checked(path, &written)
}

/// The scale of a page's slider row.
fn scale_of(page: &crate::spec::Page, row: &str) -> Result<SliderScale, String> {
    match page.control(row).map(|control| &control.widget) {
        Some(crate::spec::Widget::Slider { scale }) => Ok(*scale),
        _ => Err(format!("no slider {row:?}")),
    }
}

/// A percentage slider (0 to 100, whole steps).
const PERCENT: SliderScale = SliderScale::new(0.0, 100.0, 1.0);
/// An effect slider (0 to 20, whole steps).
const EFFECT: SliderScale = SliderScale::new(0.0, 20.0, 1.0);
/// A percentage range (0 to 100, whole steps).
const PERCENT_RANGE: RangeScale = RangeScale::new(0.0, 100.0, 1.0);

// ---- The profile load ------------------------------------------------------------------------------------

/// The rows of a newly loaded profile, in order (see the module). The first failing required value stops the
/// rows after it, and is returned.
pub fn profile_load(app: &mut MyApp) -> Result<(), String> {
    routines::on_profile_load(app);
    sim3d_rows(app)?;
    hapticr_rows(app)?;
    if let Err(error) = iflag::on_profile_load(app) {
        tracing::debug!(%error, "iFlag rows");
    }
    belt::on_profile_load(app)?;
    motion_early(app)?;
    wind_speed(app)?;
    belt_rows(app)?;
    wind_fans(app)?;
    sim3d_advanced(app)?;
    hapticr_advanced(app)?;
    let shift = stored(app, &["gear_shift_perc"])?;
    put(app, &["gear_shift_perc"], as_number(slider(PERCENT, &shift)?))?;
    let dim = stored(app, &["gear_shift_dim_perc"])?;
    put(app, &["gear_shift_dim_perc"], as_number(slider(PERCENT, &dim)?))?;
    shake_rows(app)?;
    for key in ["rr", "rl", "fr", "fl", "ce", "sub"] {
        let volume = stored(app, &["shaker_volume", key])?;
        put_shaker(app, "shaker_volume", key, as_number(slider(EFFECT, &volume)?))?;
    }
    motion_rows(app)
}

/// SIM3D: each effect's strength (a whole number) and its switch, which must be a boolean word.
fn sim3d_rows(app: &mut MyApp) -> Result<(), String> {
    for effect in &Registry::builtin().rumble_effects {
        let key = effect.id.as_str();
        let value = slider(EFFECT, &stored(app, &["rumble_effects", key])?)?;
        put(app, &["rumble_effects", key], LegacyValue::Int(value.trunc() as i64))?;
        native_bool(&stored(app, &["rumble_switch", key])?)?;
    }
    Ok(())
}

/// Whether HapticR is enabled (`APP.enable_hapticr`, which must be a boolean word).
fn hapticr_enabled(app: &MyApp) -> Result<bool, String> {
    native_bool(&app.backend.native_setting("APP", "enable_hapticr").map_err(|error| error.detail)?)
}

/// HapticR, while enabled: each effect's strength; an effect without a usable strength empties the whole
/// `haptic_effects` table and takes 0. Each effect's switch; one that is missing or no boolean word empties the
/// whole `haptic_switch` table and is switched on.
fn hapticr_rows(app: &mut MyApp) -> Result<(), String> {
    if !hapticr_enabled(app)? {
        return Ok(());
    }
    for effect in &Registry::builtin().haptic_effects {
        let key = effect.id.as_str();
        match stored(app, &["haptic_effects", key]).and_then(|value| slider(EFFECT, &value)) {
            Ok(value) => put(app, &["haptic_effects", key], LegacyValue::Int(value.trunc() as i64))?,
            Err(_) => {
                app.set_tune_native_checked(&["haptic_effects"], &LegacyValue::Dict(Dict::new()))?;
                put(app, &["haptic_effects", key], LegacyValue::Int(0))?;
            }
        }
        if stored(app, &["haptic_switch", key]).and_then(|value| native_bool(&value)).is_err() {
            app.set_tune_native_checked(&["haptic_switch"], &LegacyValue::Dict(Dict::new()))?;
            app.set_tune_native_checked(&["haptic_switch", key], &LegacyValue::Bool(true))?;
        }
    }
    Ok(())
}

/// The Motion rows before the Wind rows: the game set-up check, the spike filters, the smoothing and the
/// vertical actuators. Whether the driving style is offered follows the stored style (none, or an empty text,
/// offers none).
fn motion_early(app: &mut MyApp) -> Result<(), String> {
    let style = stored(app, &["intellitune_motion_preference"]).unwrap_or(LegacyValue::Str(String::new()));
    if style == LegacyValue::Str(String::new()) {
        app.ui.tuning.motion_hidden.insert(motion::PREFERENCE_PICKER.to_owned());
    } else {
        app.ui.tuning.motion_hidden.remove(motion::PREFERENCE_PICKER);
    }
    app.backend.check_game_config().map_err(|error| error.detail)?;
    for key in ["thanos_spike_filter_horizontal", "thanos_spike_filter_vertical"] {
        if let Some(value) = spin(&stored(app, &[key])?, motion::SPIKE_FILTER_RANGE) {
            put(app, &[key], LegacyValue::Int(value))?;
        }
    }
    let smoothing = SliderScale::new(0.0, 100.0, motion::smoothing_step(app));
    for axis in ["surge", "sway", "traction_loss", "traction_loss_front", "roll", "heave", "pitch", "yaw"] {
        let value = slider(smoothing, &stored(app, &["motion_effect_smoothing_factor", axis])?)?;
        put(app, &["motion_effect_smoothing_factor", axis], LegacyValue::Int(value.trunc() as i64))?;
    }
    for axis in ["surge", "sway", "traction_loss", "traction_loss_front"] {
        let key = format!("motion_main_actuators_{axis}");
        let value = slider(PERCENT, &stored(app, &[&key])?)?;
        put(app, &[&key], LegacyValue::Int(value.trunc() as i64))?;
    }
    Ok(())
}

/// Whether the app shows speeds in km/h (`APP.measurement_unit` is "K").
fn kmh(app: &MyApp) -> Result<bool, String> {
    Ok(app.backend.native_setting("APP", "measurement_unit").map_err(|error| error.detail)?
        == LegacyValue::Str("K".into()))
}

/// The number of a value for the speed conversion (numbers and booleans only).
fn speed_number(value: &LegacyValue) -> Result<f64, String> {
    match value {
        LegacyValue::Int(value) => Ok(*value as f64),
        LegacyValue::Float(value) => Ok(*value),
        LegacyValue::Bool(value) => Ok(f64::from(*value)),
        other => Err(format!("TypeError: cannot multiply {} by float", other.type_name())),
    }
}

/// Wind: the default max speed, shown in the unit (in mph the km/h times 0.621371, truncated) and stored back
/// in km/h (in mph the shown value times 1.60934, truncated), then the speed factor.
fn wind_speed(app: &mut MyApp) -> Result<(), String> {
    let kmh = kmh(app)?;
    let value = stored(app, &["default_max_speed_kmh"])?;
    let shown =
        if kmh { value } else { LegacyValue::Int(native_int(&LegacyValue::Float(speed_number(&value)? * 0.621371))?) };
    let shown = slider(SliderScale::new(0.0, 2000.0, 1.0), &shown)?;
    let written =
        if kmh { native_int(&LegacyValue::Float(shown))? } else { native_int(&LegacyValue::Float(shown * 1.60934))? };
    put(app, &["default_max_speed_kmh"], LegacyValue::Int(written))?;
    let factor = stored(app, &["speed_factor"])
        .and_then(|value| slider(SliderScale::new(1.0, 10.0, 1.0), &value))
        .and_then(|value| put(app, &["speed_factor"], LegacyValue::Int(value.trunc() as i64)));
    if let Err(error) = factor {
        tracing::debug!(%error, "speed factor");
    }
    Ok(())
}

/// The Belt rows: each switch must be a boolean word, each slider stores a whole number.
fn belt_rows(app: &mut MyApp) -> Result<(), String> {
    let page = belt::page();
    for (row, key, is_switch) in BELT_ROWS {
        let value = stored(app, &[key])?;
        if is_switch {
            native_bool(&value)?;
        } else {
            let value = slider(scale_of(&page, row)?, &value)?;
            put(app, &[key], LegacyValue::Int(value.trunc() as i64))?;
        }
    }
    Ok(())
}

/// The Belt rows in the order they load: row, stored key, whether it is a switch.
pub(crate) const BELT_ROWS: [(&str, &str, bool); 12] = [
    ("belt_sway_curving_enabled", "belt_sway_curving_enabled", true),
    ("belt_sway_reverse", "belt_sway_curving_reverse", true),
    ("belt_surge_reverse", "belt_surge_reverse", true),
    ("belt_curving", "belt_sway_curving_factor", false),
    ("belt_sway_smoothing", "belt_sway_smoothing", false),
    ("belt_surge_enabled", "belt_surge_enabled", true),
    ("belt_surge", "belt_surge", false),
    ("belt_surge_smoothing", "belt_surge_smoothing", false),
    ("belt_heave_enabled", "belt_heave_enabled", true),
    ("belt_heave", "belt_heave_factor", false),
    ("belt_heave_smoothing", "belt_heave_smoothing", false),
    ("belt_heave_reverse", "belt_heave_reverse", true),
];

/// Wind: the ejection seat (off without a usable value), wind curving (with fewer than two fans off with an
/// effect of 1), and both fan power ranges (the centre one the full range without usable values).
fn wind_fans(app: &mut MyApp) -> Result<(), String> {
    if stored(app, &["enable_ejection_seat"]).and_then(|value| native_bool(&value)).is_err() {
        app.set_tune_native_checked(&["enable_ejection_seat"], &LegacyValue::Int(0))?;
    }
    let curving_scale = SliderScale::new(1.0, 10.0, 1.0);
    if wind::two_fans(app) {
        native_bool(&stored(app, &["wind_curving"])?)?;
        let effect = slider(curving_scale, &stored(app, &["curving_effect"])?)?;
        put(app, &["curving_effect"], LegacyValue::Int(effect.trunc() as i64))?;
    } else {
        force_switch(app, &["wind_curving"], false, LegacyValue::Int(0))?;
        put(app, &["curving_effect"], LegacyValue::Int(1))?;
    }
    let (low, high) = range(
        PERCENT_RANGE,
        &stored(app, &["power_wind_min_speed_perc"])?,
        &stored(app, &["power_wind_max_speed_perc"])?,
    )?;
    put(app, &["power_wind_max_speed_perc"], as_number(high))?;
    put(app, &["power_wind_min_speed_perc"], as_number(low))?;
    let center = (|| {
        let low = stored(app, &["power_center_wind_min_speed_perc"])?;
        let high = stored(app, &["power_center_wind_max_speed_perc"])?;
        range(PERCENT_RANGE, &low, &high)
    })();
    let (low, high) = match center {
        Ok(values) => values,
        Err(_) => range(PERCENT_RANGE, &LegacyValue::Int(0), &LegacyValue::Int(100))?,
    };
    put(app, &["power_center_wind_max_speed_perc"], as_number(high))?;
    put(app, &["power_center_wind_min_speed_perc"], as_number(low))
}

/// The advanced percentage rows of SIM3D and HapticR: the row's stored key and its fallback.
const ADVANCED: [(&str, f64); 4] = [
    ("rpm_start_rumble", 0.0),
    ("wheel_slip_min_activation", 30.0),
    ("wheel_lock_min_activation", 50.0),
    ("traction_loss_min_activation", 40.0),
];

/// The RPM fade range of a table: its start read as a number and truncated, its stop truncated.
fn fade_range(app: &MyApp, table: &str) -> Result<(f64, f64), String> {
    let start = native_int_float(&stored(app, &[table, "rpm_start_fading"])?)?;
    let stop = native_int_as_float(&stored(app, &[table, "rpm_stop_fading"])?)?;
    range(PERCENT_RANGE, &LegacyValue::Float(start), &LegacyValue::Float(stop))
}

/// Stores a table's RPM fade range as whole numbers.
fn put_fade(app: &mut MyApp, table: &str, (low, high): (f64, f64)) -> Result<(), String> {
    put(app, &[table, "rpm_start_fading"], LegacyValue::Int(low.trunc() as i64))?;
    put(app, &[table, "rpm_stop_fading"], LegacyValue::Int(high.trunc() as i64))
}

/// SIM3D's advanced rows: the RPM fade range and the four percentages; all required.
fn sim3d_advanced(app: &mut MyApp) -> Result<(), String> {
    let fade = fade_range(app, "rumble_effects")?;
    put_fade(app, "rumble_effects", fade)?;
    for (key, _) in ADVANCED {
        let value = native_int_float(&stored(app, &["rumble_effects", key])?)?;
        put(app, &["rumble_effects", key], as_number(slider(PERCENT, &LegacyValue::Float(value))?))?;
    }
    Ok(())
}

/// HapticR's advanced rows, while enabled: the RPM fade range (94 – 100 without usable values) and the four
/// percentages (each with its fallback).
fn hapticr_advanced(app: &mut MyApp) -> Result<(), String> {
    if !hapticr_enabled(app)? {
        return Ok(());
    }
    let fade = match fade_range(app, "haptic_effects") {
        Ok(fade) => fade,
        Err(_) => range(PERCENT_RANGE, &LegacyValue::Float(94.0), &LegacyValue::Float(100.0))?,
    };
    put_fade(app, "haptic_effects", fade)?;
    for (key, fallback) in ADVANCED {
        let value = stored(app, &["haptic_effects", key])
            .and_then(|value| native_int_float(&value))
            .and_then(|value| slider(PERCENT, &LegacyValue::Float(value)));
        let value = match value {
            Ok(value) => value,
            Err(_) => slider(PERCENT, &LegacyValue::Float(fallback))?,
        };
        put(app, &["haptic_effects", key], as_number(value))?;
    }
    Ok(())
}

/// Shake: the engine type ("O" when the profile has none; anything but "O" is stored as "E"), then each effect's
/// strength and its switch, which must be a boolean word.
fn shake_rows(app: &mut MyApp) -> Result<(), String> {
    if stored(app, &["shaker_engine_type"]).is_err() {
        app.set_tune_native_checked(&["shaker_engine_type"], &LegacyValue::Str("O".into()))?;
    }
    let kind = if stored(app, &["shaker_engine_type"])? == LegacyValue::Str("O".into()) { "O" } else { "E" };
    put(app, &["shaker_engine_type"], LegacyValue::Str(kind.into()))?;
    for effect in &Registry::builtin().shaker_effects {
        let key = effect.id.as_str();
        let value = slider(EFFECT, &stored(app, &["shaker_effects", key])?)?;
        put_shaker(app, "shaker_effects", key, as_number(value))?;
        native_bool(&stored(app, &["shaker_switch", key])?)?;
    }
    Ok(())
}

/// The running game is a racing game (also in stand-by and for a game the catalogue lacks).
fn racing(app: &MyApp) -> bool {
    app.backend
        .game_value()
        .ok()
        .and_then(|game| match game {
            LegacyValue::Str(game) => Catalog::builtin().game(&game).map(|game| game.kind == GameKind::Racing),
            _ => None,
        })
        .unwrap_or(true)
}

/// A motion axis's type row: the stored type becomes "angle" or (anything else) "rate", and its Max Telemetry
/// box takes the maximum of that type. Without a type, or without that maximum, the row is hidden. Whether the
/// axis follows the rate (an axis without a type follows the angle).
pub(crate) fn axis_type(app: &mut MyApp, axis: &str, source: Option<&LegacyValue>) -> Result<bool, String> {
    let row = format!("{axis}_type");
    let mut rate = false;
    let result = (|| -> Result<(), String> {
        let kind = match source {
            Some(tune) => field(tune, &["motion_effects", &row])?,
            None => stored(app, &["motion_effects", &row])?,
        };
        rate = kind != LegacyValue::Str("angle".into());
        put(app, &["motion_effects", &row], LegacyValue::Str(if rate { "rate" } else { "angle" }.into()))?;
        let key = if rate { format!("{axis}_rate") } else { axis.to_owned() };
        let max = stored(app, &["max_telemetry_vars", &key])?;
        if let Some(max) = spin(&max, motion::MAX_TELEMETRY_RANGE) {
            put(app, &["max_telemetry_vars", &key], LegacyValue::Int(max))?;
        }
        Ok(())
    })();
    if result.is_ok() {
        app.ui.tuning.motion_hidden.remove(&row);
    } else {
        app.ui.tuning.motion_hidden.insert(row);
    }
    Ok(rate)
}

/// The Max Telemetry key of an axis following the rate or not.
fn max_key(effect: &str, rate: bool) -> String {
    if rate {
        format!("{effect}_rate")
    } else {
        effect.to_owned()
    }
}

/// The Motion rows after Shake: the axis types, every effect (strength, switch, Max Telemetry), Enhanced
/// G-Forces, the racing rows, the platform rows, the actuator speed rows or Reaction Speed, the belt tensioner,
/// then the fixed values of the STR-SCN6150 platforms.
fn motion_rows(app: &mut MyApp) -> Result<(), String> {
    let roll_rate = axis_type(app, "roll", None)?;
    let pitch_rate = axis_type(app, "pitch", None)?;
    for effect in &Registry::builtin().motion_effects {
        let key = effect.id.as_str();
        let value = slider(EFFECT, &stored(app, &["motion_effects", key])?)?;
        put(app, &["motion_effects", key], as_number(value))?;
        native_bool(&stored(app, &["motion_switch", key])?)?;
        let rate = (key == "pitch" && pitch_rate) || (key == "roll" && roll_rate);
        let max_key = max_key(key, rate);
        if let Some(max) =
            stored(app, &["max_telemetry_vars", &max_key]).ok().and_then(|max| spin(&max, motion::MAX_TELEMETRY_RANGE))
        {
            put(app, &["max_telemetry_vars", &max_key], LegacyValue::Int(max))?;
        }
    }
    // Enhanced G-Forces: a whole number its switch can show (0 or 1).
    native_bool(&LegacyValue::Int(native_int(&stored(app, &["motion_enhanced_gforces"])?)?))?;
    if racing(app) {
        let value = slider(PERCENT, &stored(app, &["gear_smoothing_factor"])?)?;
        put(app, &["gear_smoothing_factor"], LegacyValue::Int(value.trunc() as i64))?;
        for key in ["min_traction_loss_activation", "min_traction_loss_front_activation"] {
            if let Some(value) = spin(&stored(app, &[key])?, motion::ACTIVATION_RANGE) {
                put(app, &[key], LegacyValue::Int(value))?;
            }
        }
    }
    for key in ["motion_max_power", "motion_max_horizontal", "motion_smoothing_factor"] {
        let value = slider(PERCENT, &stored(app, &[key])?)?;
        put(app, &[key], as_number(value))?;
    }
    let boost = slider(SliderScale::new(1.0, 100.0, 1.0), &stored(app, &["motion_boost_factor"])?)?;
    put(app, &["motion_boost_factor"], as_number(boost))?;
    let model = crate::catalog::current_model(&*app.backend)?;
    if model.manufacturer != "SIMTECHREVOLUTION" {
        let reaction = (|| -> Result<(), String> {
            let speed = native_int(&stored(app, &["motion_reaction_speed"])?)?;
            let speed = slider(SliderScale::new(70.0, 100.0, 1.0), &LegacyValue::Int(speed))?.trunc() as i64;
            put(app, &["motion_reaction_speed"], LegacyValue::Int(speed))?;
            let setting = app.backend.native_setting("MOTIONBOX", "reaction_speed").map_err(|error| error.detail)?;
            if !native_equal(&setting, &LegacyValue::Int(speed)) {
                app.set_motion_setting_native_checked("reaction_speed", &LegacyValue::Int(speed))?;
            }
            Ok(())
        })();
        if let Err(error) = reaction {
            tracing::debug!(%error, "reaction speed");
        }
    } else {
        for (key, max) in [("scn6_speed", 10000.0), ("scn6_acceleration", 250.0)] {
            let speed = stored(app, &[key])
                .and_then(|value| native_int(&value))
                .and_then(|value| slider(SliderScale::new(1.0, max, 1.0), &LegacyValue::Int(value)))
                .and_then(|value| put(app, &[key], LegacyValue::Int(value.trunc() as i64)));
            if let Err(error) = speed {
                tracing::debug!(%error, key, "actuator speed");
            }
        }
    }
    if model.belt_tensioner_motor {
        let value = slider(PERCENT, &stored(app, &["motion_belt_tensioner"])?)?;
        put(app, &["motion_belt_tensioner"], LegacyValue::Int(value.trunc() as i64))?;
    }
    str_values(app)
}

/// The STR-SCN6150 platforms' fixed values: Max Horizontal 100, Overall Smoothing 1, both activation
/// thresholds 0, the vertical actuators' sway and surge 100 and traction losses 0, and the switches of the
/// effects the platform has no use for off (rear traction loss excepted).
fn str_values(app: &mut MyApp) -> Result<(), String> {
    let disabled: &[&str] = match app.backend.native_setting("MOTIONBOX", "model").map_err(|error| error.detail)? {
        LegacyValue::Str(name) if name == "STR-SCN6150" => &motion::STR_HIDDEN,
        LegacyValue::Str(name) if name == "STR-SCN6150-TL" => &motion::STR_TL_HIDDEN,
        _ => return Ok(()),
    };
    put(app, &["motion_max_horizontal"], LegacyValue::Int(100))?;
    put(app, &["motion_smoothing_factor"], LegacyValue::Int(1))?;
    put(app, &["min_traction_loss_activation"], LegacyValue::Int(0))?;
    put(app, &["min_traction_loss_front_activation"], LegacyValue::Int(0))?;
    for (key, value) in [
        ("motion_main_actuators_sway", 100),
        ("motion_main_actuators_surge", 100),
        ("motion_main_actuators_traction_loss", 0),
        ("motion_main_actuators_traction_loss_front", 0),
    ] {
        put(app, &[key], LegacyValue::Int(value))?;
    }
    for effect in disabled.iter().filter(|effect| **effect != "traction_loss") {
        force_switch(app, &["motion_switch", effect], false, LegacyValue::Bool(false))?;
    }
    Ok(())
}

// ---- At start ----------------------------------------------------------------------------------------------

/// A whole number put into a slider of a screen's page description, as a change of the slider: rounded to its
/// step within its range, then stored by the row. `Err` when the page has no such slider, or the row's write
/// fails.
pub(crate) fn put_into_slider(
    app: &mut MyApp,
    screen: crate::ui::Screen,
    control: &str,
    value: i64,
) -> Result<(), String> {
    let scale = super::page(app, screen)
        .and_then(|page| match page.control(control).map(|c| &c.widget) {
            Some(crate::spec::Widget::Slider { scale }) => Some(*scale),
            _ => None,
        })
        .ok_or_else(|| format!("no slider {control:?}"))?;
    let held = scale.snap(value as f64);
    super::set(app, screen, control, "", &crate::ui::html::num(held))
}

/// The stored codes of the LED segment types, in the order of the page's choices ("O", Off, the last).
const KIND_CODES: [&str; 10] = ["R", "S", "F", "RF", "SF", "SR", "SL", "SRF", "SLF", "O"];

/// Once at start (after the LED count): each LED segment's range within the strip's LEDs, stored as whole
/// numbers, and its type, a type the page does not list becoming Off ("O").
pub fn start(app: &mut MyApp) -> Result<(), String> {
    let setting = |app: &MyApp, key: &str| app.backend.native_setting("LED", key).map_err(|error| error.detail);
    let leds = native_int(&setting(app, "nbr_leds")?)?;
    let scale = RangeScale::new(1.0, leds as f64, 1.0);
    for segment in 1..=4 {
        let key = format!("segment{segment}");
        let start = native_int(&setting(app, &format!("{key}_start"))?)?;
        let end = native_int(&setting(app, &format!("{key}_end"))?)?;
        let (low, high) = range(scale, &LegacyValue::Int(start), &LegacyValue::Int(end))?;
        put_setting(app, "LED", &format!("{key}_start"), LegacyValue::Int(low as i64))?;
        put_setting(app, "LED", &format!("{key}_end"), LegacyValue::Int(high as i64))?;
        let kind = setting(app, &format!("{key}_type"))?;
        let code = KIND_CODES
            .iter()
            .find(|code| native_equal(&kind, &LegacyValue::Str((**code).into())))
            .copied()
            .unwrap_or("O");
        put_setting(app, "LED", &format!("{key}_type"), LegacyValue::Str(code.into()))?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "reload_tests.rs"]
mod tests;
