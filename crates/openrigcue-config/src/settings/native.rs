//! Raw INI storage behind the typed settings view. Untouched raw values and keys survive edits.

use serde_json::Value;

use crate::import::reconcile_detached;
use crate::import::value::{Dict, Key, LegacyValue};

use super::Settings;

#[derive(Debug, Clone, Default)]
pub(super) struct NativeSettings {
    original: Dict,
    projection: Dict,
    retained: bool,
    pub(super) layout: Option<crate::import::configobj::Layout>,
}

impl NativeSettings {
    pub fn retain(mut original: Dict, settings: &Settings) -> Self {
        // Keep migration bookkeeping; omit the named account/cloud/analytics mechanisms.
        if let Some(app) = original.get_mut("APP").and_then(LegacyValue::as_dict_mut) {
            for key in [
                "uuid",
                "trial_subscription_started",
                "trial_started",
                "enable_cloud_backup",
                "marketing_notifications",
                "show_deactivate_license_button",
            ] {
                app.remove(key);
            }
        }
        Self { original, projection: projection(settings), retained: true, layout: None }
    }

    pub fn snapshot(&self, settings: &Settings) -> Dict {
        // A stored dictionary that is a shared object keeps what it holds: a typed edit of it is made in a copy.
        let mut raw = LegacyValue::Dict(self.original.clone());
        let (before, after) = (LegacyValue::Dict(self.projection.clone()), LegacyValue::Dict(projection(settings)));
        reconcile_detached(&mut raw, &before, &after);
        let LegacyValue::Dict(mut config) = raw else { unreachable!("settings are sections") };
        if !self.retained {
            let app = config.dict_entry("APP").as_dict_mut().expect("app section");
            app.insert("config_file_version", crate::import::migrate::CURRENT_VERSION.to_string().into());
            app.insert("first_time_installation", "no".into());
        }
        config
    }
}

const SECTIONS: [(&str, &str); 26] = [
    ("app", "APP"),
    ("shakers", "SHAKEBOX"),
    ("shortcuts", "SHORTCUTS"),
    ("wind", "INTELLIBOX"),
    ("motion", "MOTIONBOX"),
    ("dof_reality", "DOF REALITY"),
    ("prosimu", "PROSIMU"),
    ("yaw_vr", "Yaw VR"),
    ("pt_actuator", "PT-ACTUATOR"),
    ("thanos", "THANOS"),
    ("vnm", "VNM SIMULATION"),
    ("simtech", "SIMTECHREVOLUTION"),
    ("network_ports", "NETWORK_PORT"),
    ("port_forwarding", "PORT_FORWARDING"),
    ("features", "CUSTOM_FEATURES"),
    ("leds", "LED"),
    ("monitors", "MONITORS"),
    ("compensation", "MOTION_COMPENSATION"),
    ("belt", "BELT_TENSIONER"),
    ("hud", "HUD"),
    ("iflag", "IFLAG"),
    ("sim3d", "SIM3D"),
    ("hapticr", "HAPTICR"),
    ("magicbox", "MAGICBOX"),
    ("symprojects", "SYMPROJECTS"),
    ("version", ""),
];

fn projection(settings: &Settings) -> Dict {
    let json = serde_json::to_value(settings).expect("settings serialize");
    project_json(&json)
}

fn project_json(json: &Value) -> Dict {
    project_json_with_integer_fields(json, true)
}

// These values are created and consumed as integers by the UI. The
// typed settings model exposes f64s, which must not write "1.0" where an
// integer is read back. Stored strings retain their exact spelling.
const UI_INTEGER_FIELDS: &[(&str, &str)] = &[
    ("features", "smoothing_slider_step"),
    ("compensation", "height"),
    ("compensation", "surge_horizontal_stroke"),
    ("compensation", "left_right_distance_mm"),
    ("compensation", "front_back_distance_mm"),
    ("compensation", "actuator_vertical_stroke"),
    ("compensation", "surge_factor"),
    ("compensation", "surge_horizontal_factor"),
    ("compensation", "sway_factor"),
    ("compensation", "yaw_factor"),
    ("compensation", "roll_factor"),
    ("compensation", "pitch_factor"),
    ("belt", "calibration_factor"),
    ("belt", "calibration_retraction"),
];

fn project_json_with_integer_fields(json: &Value, canonical_integers: bool) -> Dict {
    let mut root = Dict::new();
    for (typed, native) in SECTIONS {
        if native.is_empty() {
            continue;
        }
        let mut section = Dict::new();
        let Some(object) = json.get(typed).and_then(Value::as_object) else { continue };
        for (key, value) in object {
            if typed == "app" && matches!(key.as_str(), "start_with_login" | "show_splash_screen") {
                continue;
            }
            if typed == "shortcuts" {
                match key.as_str() {
                    "bindings" => {
                        for (action, binding) in value.as_object().expect("bindings") {
                            if let Some(value) = binding.get("keys") {
                                section.insert(format!("{action}_keys"), scalar(value));
                            }
                            if let Some(value) = binding.get("buttons") {
                                section.insert(format!("{action}_btns"), scalar(value));
                            }
                        }
                    }
                    "other_keys" => {
                        for (key, value) in value.as_object().expect("other shortcuts") {
                            section.insert(key.clone(), scalar(value));
                        }
                    }
                    key if key.starts_with("skip_power_wind_") => {
                        section.insert(
                            key,
                            match value.as_bool() {
                                Some(value) => if value { "1" } else { "0" }.into(),
                                None => scalar(value),
                            },
                        );
                    }
                    _ => section.insert(key.clone(), scalar(value)),
                }
                continue;
            }
            if typed == "leds" && key == "segments" {
                for (index, segment) in value.as_array().expect("led segments").iter().enumerate() {
                    for (field, value) in segment.as_object().expect("segment") {
                        let field = if field == "kind" { "type" } else { field };
                        let mut value = scalar(value);
                        if field == "type" {
                            value = led_type(value);
                        }
                        section.insert(format!("segment{}_{field}", index + 1), value);
                    }
                }
                continue;
            }
            let typed_key = key.as_str();
            let key = native_key(typed, key);
            let mut value = if canonical_integers && UI_INTEGER_FIELDS.contains(&(typed, typed_key)) {
                value
                    .as_f64()
                    .filter(|number| value.is_f64() && number.is_finite() && number.fract() == 0.0)
                    .map_or_else(
                        || scalar(value),
                        |number| {
                            if number == 0.0 {
                                "0".into()
                            } else {
                                format!("{number:.0}").into()
                            }
                        },
                    )
            } else {
                scalar(value)
            };
            if typed == "app" && key == "measurement_unit" {
                value = match value.as_str() {
                    Some("kmh") => "K".into(),
                    Some("mph") => "M".into(),
                    _ => value,
                };
            } else if typed == "compensation" && key == "type" {
                value = match value.as_str() {
                    Some("virtual") => "V".into(),
                    Some("witmotion") => "W".into(),
                    _ => value,
                };
            }
            section.insert(key, value);
        }
        root.insert(native, LegacyValue::Dict(section));
    }
    root
}

/// Recover only an explicit old typed float whose exact buggy projection is
/// still present. The caller establishes version/file provenance first.
pub(super) fn recover_typed_integer_fields(settings: &mut Settings, legacy: &toml::Value) -> bool {
    // Only these scalar inputs belong to this repair. Unrelated malformed
    // legacy tables must not reach the full typed projection's schema expects.
    let mut json = serde_json::Map::new();
    for &(section, key) in UI_INTEGER_FIELDS {
        let Some(value) = legacy.get(section).and_then(|section| section.get(key)).filter(|value| value.is_float())
        else {
            continue;
        };
        let Ok(value) = serde_json::to_value(value) else { continue };
        json.entry(section)
            .or_insert_with(|| Value::Object(Default::default()))
            .as_object_mut()
            .unwrap()
            .insert(key.into(), value);
    }
    let json = Value::Object(json);
    let old = project_json_with_integer_fields(&json, false);
    let corrected = project_json(&json);
    let mut raw = settings.native_snapshot();
    let mut changed = false;
    for &(typed, key) in UI_INTEGER_FIELDS {
        if !legacy.get(typed).and_then(|section| section.get(key)).is_some_and(toml::Value::is_float) {
            continue;
        }
        let native = SECTIONS.iter().find(|(name, _)| *name == typed).unwrap().1;
        let key = native_key(typed, key);
        let before = old.get(native).and_then(LegacyValue::as_dict).and_then(|section| section.get(key));
        let after = corrected.get(native).and_then(LegacyValue::as_dict).and_then(|section| section.get(key));
        if let (Some(before), Some(after), Some(fields)) =
            (before, after, raw.get_mut(native).and_then(LegacyValue::as_dict_mut))
        {
            if before != after && fields.get(key) == Some(before) {
                fields.insert(key, after.clone());
                changed = true;
            }
        }
    }
    if changed {
        let mut corrected = Settings::from_legacy(&raw).0;
        corrected.retain_ini_layout(settings);
        corrected.app.start_with_login = settings.app.start_with_login;
        corrected.app.show_splash_screen = settings.app.show_splash_screen;
        *settings = corrected;
    }
    changed
}

/// The page-facing names for the actual INI values. Renaming/flattening is structural only;
/// missing keys and invalid scalar values do not acquire the typed view's defaults.
pub(super) fn ui_view(settings: &Settings) -> Result<Value, crate::import::de::ConvertError> {
    let raw = settings.native_snapshot();
    let schema = serde_json::to_value(settings).expect("settings serialize");
    let mut root = Dict::new();
    root.insert("version", LegacyValue::from(schema["version"].clone()));
    for (typed, native) in SECTIONS {
        if native.is_empty() {
            continue;
        }
        let Some(value) = raw.get(native) else { continue };
        let Some(fields) = value.as_dict() else {
            root.insert(typed, value.clone());
            continue;
        };
        let mut section = Dict::new();
        if typed == "shortcuts" {
            let mut bindings = Dict::new();
            let mut other = Dict::new();
            for (key, value) in fields.iter() {
                let key = key.to_text();
                let binding = key
                    .strip_suffix("_keys")
                    .map(|action| (action, "keys"))
                    .or_else(|| key.strip_suffix("_btns").map(|action| (action, "buttons")));
                if let Some((action, field)) = binding {
                    bindings.dict_entry(action).as_dict_mut().expect("binding dictionary").insert(field, value.clone());
                } else if matches!(
                    key.as_str(),
                    "ignore_joysticks" | "skip_power_wind_always_on" | "skip_power_wind_auto" | "skip_power_wind_off"
                ) {
                    section.insert(key, value.clone());
                } else {
                    other.insert(key, value.clone());
                }
            }
            section.insert("bindings", bindings.into());
            section.insert("other_keys", other.into());
        } else {
            let names = schema[typed].as_object().expect("typed section");
            for (key, value) in fields.iter() {
                let key = key.to_text();
                if typed == "leds" && key.starts_with("segment") && key.contains('_') {
                    continue;
                }
                let mapped =
                    names.keys().find(|name| native_key(typed, name) == key).map(String::as_str).unwrap_or(&key);
                section.insert(mapped, decode(typed, mapped, value));
            }
            if typed == "leds" {
                let mut segments = Vec::new();
                for index in 1..=4 {
                    let mut segment = Dict::new();
                    for (typed, native) in [
                        ("kind", "type"),
                        ("start", "start"),
                        ("end", "end"),
                        ("direction", "direction"),
                        ("color", "color"),
                    ] {
                        if let Some(value) = fields.get(&format!("segment{index}_{native}")) {
                            segment.insert(typed, if typed == "kind" { decode_led_type(value) } else { value.clone() });
                        }
                    }
                    segments.push(segment.into());
                }
                section.insert("segments", LegacyValue::List(segments));
            }
            if typed == "app" {
                section.insert("start_with_login", settings.app.start_with_login.into());
                section.insert("show_splash_screen", settings.app.show_splash_screen.into());
            }
        }
        root.insert(typed, section.into());
    }
    crate::import::de::from_legacy(&LegacyValue::Dict(root))
}

fn decode(typed: &str, key: &str, value: &LegacyValue) -> LegacyValue {
    let translated = match (typed, key, value.as_str()) {
        ("app", "speed_unit", Some("K")) => Some("kmh"),
        ("app", "speed_unit", Some("M")) => Some("mph"),
        ("compensation", "sensor_type", Some("V")) => Some("virtual"),
        ("compensation", "sensor_type", Some("W")) => Some("witmotion"),
        _ => None,
    };
    translated.map_or_else(|| value.clone(), LegacyValue::from)
}

fn decode_led_type(value: &LegacyValue) -> LegacyValue {
    let name = match value.as_str() {
        Some("R") => "rpm",
        Some("S") => "speed",
        Some("F") => "flag",
        Some("RF") => "rpm_flag",
        Some("SF") => "speed_flag",
        Some("SR") => "spotter_right",
        Some("SL") => "spotter_left",
        Some("SRF") => "spotter_right_flag",
        Some("SLF") => "spotter_left_flag",
        Some("O") => "off",
        _ => return value.clone(),
    };
    name.into()
}

pub(super) fn apply_ui_patch(settings: &Settings, patch: &Value) -> Result<Settings, String> {
    let changes = patch.as_object().ok_or("settings edit is not a dictionary")?;
    for (typed, changes) in changes {
        if typed == "version" {
            continue;
        }
        if !SECTIONS.iter().any(|(name, _)| name == typed) {
            return Err(format!("unknown settings section {typed:?}"));
        }
        let changes = changes.as_object().ok_or_else(|| format!("settings section {typed:?} is not a dictionary"))?;
        for (key, value) in changes {
            if typed == "shortcuts" && matches!(key.as_str(), "bindings" | "other_keys") {
                let object = value.as_object().ok_or("shortcut edit is not a dictionary")?;
                if key == "bindings" && object.values().any(|value| !value.is_object()) {
                    return Err("shortcut binding is not a dictionary".into());
                }
            }
            if typed == "leds"
                && key == "segments"
                && !value.as_array().is_some_and(|values| values.iter().all(Value::is_object))
            {
                return Err("LED segments are not a list of dictionaries".into());
            }
        }
    }
    let mut raw = settings.native_snapshot();
    for (section, values) in project_json(patch).iter() {
        let name = section.to_text();
        let values = values.as_dict().expect("projected section");
        if values.is_empty() {
            continue;
        }
        let fields = raw
            .get_mut(&name)
            .and_then(LegacyValue::as_dict_mut)
            .ok_or_else(|| format!("settings section {name:?} is missing or is not a dictionary"))?;
        assign_fields(fields, values)?;
    }
    let mut edited = Settings::from_legacy(&raw).0;
    edited.retain_ini_layout(settings);
    edited.app.start_with_login = settings.app.start_with_login;
    edited.app.show_splash_screen = settings.app.show_splash_screen;
    for (key, enabled) in [
        ("start_with_login", &mut edited.app.start_with_login),
        ("show_splash_screen", &mut edited.app.show_splash_screen),
    ] {
        if let Some(value) = patch.get("app").and_then(|app| app.get(key)) {
            *enabled = LegacyValue::from(value.clone())
                .py_str()
                .and_then(|text| crate::import::value::str2bool(&text))
                .ok_or_else(|| format!("invalid launcher flag {key:?}"))?;
        }
    }
    Ok(edited)
}

fn assign_fields(fields: &mut Dict, changes: &Dict) -> Result<(), String> {
    for (key, value) in changes.iter() {
        if let Some(nested) = value.as_dict().filter(|nested| !nested.is_empty()) {
            let existing = fields
                .get_mut(&key.to_text())
                .and_then(LegacyValue::as_dict_mut)
                .ok_or_else(|| format!("settings assignment parent {key:?} is missing or is not a dictionary"))?;
            assign_fields(existing, nested)?;
        } else {
            fields.insert(key.clone(), value.clone());
        }
    }
    Ok(())
}

fn scalar(value: &Value) -> LegacyValue {
    match value {
        Value::Bool(value) => if *value { "yes" } else { "no" }.into(),
        Value::Array(values) => LegacyValue::List(values.iter().map(scalar).collect()),
        Value::Object(values) => {
            LegacyValue::Dict(values.iter().map(|(key, value)| (Key::from(key.clone()), scalar(value))).collect())
        }
        value => LegacyValue::from(value.clone()).py_str().expect("config scalar").into(),
    }
}

fn led_type(value: LegacyValue) -> LegacyValue {
    match value.as_str() {
        Some("rpm") => "R",
        Some("speed") => "S",
        Some("flag") => "F",
        Some("rpm_flag") => "RF",
        Some("speed_flag") => "SF",
        Some("spotter_right") => "SR",
        Some("spotter_left") => "SL",
        Some("spotter_right_flag") => "SRF",
        Some("spotter_left_flag") => "SLF",
        Some("off") => "O",
        _ => return value,
    }
    .into()
}

// INI key names correspond to the serde aliases declared by the section types. Keeping them here
// leaves the in-process UI's field names stable while writing the `config.ini` key layout.
fn native_key<'a>(section: &str, key: &'a str) -> &'a str {
    match (section, key) {
        ("app", "close_button_quits") => "x_button_exit",
        ("app", "dark_theme") => "use_dark_theme",
        ("app", "gear_shift_sound") => "shift_sound",
        ("app", "speed_unit") => "measurement_unit",
        ("app", "window_x") => "window_x_pos",
        ("app", "window_y") => "window_y_pos",
        ("app", "device_pixel_ratio") => "pixel_ratio",
        ("app", "show_all_telemetry_channels") => "additional_telemetry",
        ("app", "hide_standby_tuning_alert") => "disable_tuning_alert_standby",
        ("app", "start_page") => "startup_page",
        ("app", "web_ui_port") => "webui_port",
        ("app", "web_ui_host") => "webui_hostname",
        ("app", "manual_quit_only") => "force_manual_quit",
        ("app", "playstation_address") => "playstation_ip",
        ("app", "sdl2_joysticks") => "enable_sdl2",
        ("app", "allowed_serial_ports") => "allowed_com_ports",
        ("shakers", "sound_card") => "soundcard",
        ("shakers", "shaker_count") => "nbr_shakers",
        ("shakers", "speaker_layout") => "profile",
        ("shakers", "auto_refresh_sound_card") => "auto_refresh_soundcard",
        ("shakers", "legacy_sound_library") => "legacy_sound_lib",
        ("shakers", "test_effect_enabled") => "enable_test_effect",
        ("shakers", "use_default_sound_card") => "default_soundcard",
        ("wind", "serial_port") => "com_port",
        ("wind", "timeout_seconds") => "timeout_secs",
        ("wind", "power_step") => "wind_power_increment",
        ("wind", "fan_kind") => "wind_type",
        ("wind", "fan_count") => "nbr_fans",
        ("motion", "motor_axes") => "axis_assignment",
        ("motion", "intellitune_capture_seconds") => "intelli_tune_capture_secs",
        ("motion", "debug") => "enable_debug",
        ("motion", "min_null_yaw_movement_degrees") => "min_degree_null_yaw_movement",
        ("dof_reality", "left_serial_port") => "left_controller_com_port",
        ("dof_reality", "right_serial_port") => "right_controller_com_port",
        ("dof_reality", "left_resolution") => "left_controller_resolution",
        ("dof_reality", "right_resolution") => "right_controller_resolution",
        ("prosimu", "dyadic_traction_loss") => "enable_dyatic_tl",
        ("prosimu", "prs200_serial_port_1") => "prs200_com_port1",
        ("prosimu", "prs200_serial_port_2") => "prs200_com_port2",
        ("prosimu", "invert_traction_loss") => "invert_tl",
        ("prosimu", "scn6_legacy") => "use_scn6_legacy",
        ("yaw_vr", "six_dof_mode") => "6dof_mode",
        ("pt_actuator", "actuator_lateral_distance") => "lateral_distance_actuators",
        ("pt_actuator", "actuator_vertical_stroke") => "vertical_actuators_stroke",
        ("pt_actuator", "udp_targets") => "udp_config",
        ("pt_actuator", "canbus_serial_port") => "pt_com_port",
        ("pt_actuator", "canbus_baud_rate") => "pt_com_baud_rate",
        ("pt_actuator", "park_to_motion_delay_seconds") => "park_to_motion_speed_delay_s",
        ("pt_actuator", "thanos_serial_port") => "com_port",
        ("pt_actuator", "spike_filter") => "filter",
        ("thanos", "actuator_lateral_distance") => "lateral_distance_actuators",
        ("thanos", "actuator_vertical_stroke") => "vertical_actuators_stroke",
        ("thanos", "serial_port") => "com_port",
        ("thanos", "spike_filter") => "filter",
        ("vnm", "serial_port") => "vnm_com_port",
        ("vnm", "actuator_lateral_distance") => "lateral_distance_actuators",
        ("vnm", "actuator_vertical_stroke") => "vertical_actuators_stroke",
        ("simtech", "dyadic_traction_loss") => "enable_dyatic_tl",
        ("simtech", "invert_traction_loss") => "invert_tl",
        ("simtech", "traction_loss_actuator_speed") => "tl_act_speed",
        ("simtech", "traction_loss_actuator_acceleration") => "tl_act_accel",
        ("features", "elite_dangerous_invert_yaw") => "ed_invert_yaw",
        ("features", "elite_dangerous_invert_pitch") => "ed_invert_pitch",
        ("features", "elite_dangerous_invert_roll") => "ed_invert_roll",
        ("features", "prepar3d_invert_yaw") => "p3d_invert_yaw",
        ("features", "prepar3d_invert_pitch") => "p3d_invert_pitch",
        ("features", "prepar3d_invert_roll") => "p3d_invert_roll",
        ("features", "check_elite_dangerous_updates") => "check_ed_updates",
        ("features", "dash_telemetry") => "enable_dash_telemetry",
        ("features", "surge_when_pitch_joystick") => "surge_when_pitch_joy",
        ("features", "sway_when_roll_joystick") => "sway_when_roll_joy",
        ("features", "standby_tuning_editable") => "allow_edit_emulator",
        ("features", "dyadic_surge") => "enable_dyadic_surge",
        ("features", "legacy_gear_smoothing") => "enable_legacy_gear_smoothing",
        ("features", "gran_turismo") => "enable_gt",
        ("features", "routines") => "enable_routines",
        ("features", "dams_front_traction_loss_mode") => "dams_front_tl_mode",
        ("features", "dams_front_traction_loss_factor") => "dams_front_tl_factor",
        ("features", "dams_front_traction_loss_a") => "dams_front_tl_a",
        ("features", "dams_rear_traction_loss_b") => "dams_rear_tl_b",
        ("features", "dams_rear_traction_loss_a") => "dams_rear_tl_a",
        ("features", "dams_front_steering_ratio") => "dams_front_steerratio",
        ("features", "dams_traction_loss_max_auto_smoothing") => "dams_tl_max_auto_smoothing",
        ("features", "dams_traction_loss_auto_smoothing_max_speed_kmh") => "dams_tl_auto_smoothing_max_speed_kmh",
        ("features", "flyinside_overwrite") => "enable_flyinside_overwrite",
        ("features", "rollback_xplane_lateral_g") => "rollback_xplane_latg",
        ("features", "smc3_y_motor") => "enable_smc3_ymotor",
        ("features", "mc_telemetry") => "enable_mc_telemetry",
        ("features", "motion_measurement_mode") => "enable_motion_measurement_mode",
        ("features", "event_driven_device_scan") => "enable_event_driven_device_scan",
        ("compensation", "serial_port") => "com_port",
        ("compensation", "sensor") => "type",
        ("compensation", "left_right_distance_mm") => "mm_distance_left_right",
        ("compensation", "front_back_distance_mm") => "mm_distance_front_back",
        ("compensation", "actuator_vertical_stroke") => "vertical_actuators_stroke",
        ("belt", "serial_port") => "com_port",
        ("belt", "calibration_factor") => "belt_calibration_factor",
        ("belt", "calibration_retraction") => "belt_calibration_retraction",
        ("iflag", "serial_port") => "com_port",
        ("iflag", "rotation_degrees") => "rotation",
        ("sim3d", "serial_port") => "com_port",
        ("hapticr", "serial_port") => "com_port",
        ("magicbox", "wheel_scan_code") => "magic_wheel_scan_code",
        _ => key,
    }
}
