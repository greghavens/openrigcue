//! What the UI reaches in the service:
//! direct service operations and the translator.

use openrigcue_model::native_value::LegacyValue;
use serde_json::Value;

/// The operation kinds used by the UI's service interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

/// Dictionary iteration that keeps per-row failures at their consumer boundary.
pub type MetadataRows = Box<dyn Iterator<Item = Result<(String, LegacyValue), Failure>> + Send>;
pub type MetadataKeys = Box<dyn Iterator<Item = Result<String, Failure>> + Send>;

/// A device-notification branch reached while updating hardware widgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardwareNotification {
    YawVr { needs_center: bool },
    IflagFirmware,
    Belt { needs_calibration: bool },
}

/// Notification fields included in the hardware-widget snapshot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HardwareNotificationState {
    pub showing_belt: bool,
    pub showing_iflag: bool,
    pub dismissed_yaw_vr: bool,
    pub dismissed_belt: bool,
    pub dismissed_iflag: bool,
}

/// The selected hardware parameters through an adapter's current accessors.
pub fn hardware_parameter_from_backend<B: Backend + ?Sized>(backend: &B, name: &str) -> Result<LegacyValue, Failure> {
    let control = match name {
        "power_wind_status" => Some("wind"),
        "power_wind_center_status" => Some("wind_center"),
        "led_status" => Some("led"),
        "mc_switch" => Some("compensation"),
        "belt_switch" => Some("belt"),
        "hud_switch" => Some("hud"),
        "iflag_switch" => Some("iflag"),
        "sim3d_switch" => Some("sim3d"),
        "haptic_switch" => Some("haptic"),
        "motion_switch" => Some("motion"),
        "shake_box_status" => Some("shakers"),
        _ => None,
    };
    let result = if let Some(control) = control {
        backend.control_value(&["switches", control])
    } else {
        match name {
            "game_running" => backend.game_value(),
            "listener_running" => backend.session_value("listener_running"),
            "belt_homing" => backend.control_value(&["tests", "belt_homing"]),
            "Yaw VR" | "motion_status" | "motion_com_port" => backend.motion_parameter(name),
            "iflag_version" => backend.accessory_status("iflag_firmware"),
            "iflag_dismiss_firmware_notification" => Ok(backend.hardware_notification_state().dismissed_iflag.into()),
            "belt_dismiss_center_notification" => Ok(backend.hardware_notification_state().dismissed_belt.into()),
            "yawvr_dismiss_center_notification" => Ok(backend.hardware_notification_state().dismissed_yaw_vr.into()),
            name if name.ends_with("_status")
                && matches!(
                    name,
                    "intellibox_status"
                        | "mc_status"
                        | "iflag_status"
                        | "sim3d_status"
                        | "hapticr_status"
                        | "hud_status"
                        | "belt_status"
                ) =>
            {
                let device = match name {
                    "mc_status" => "compensation",
                    "hapticr_status" => "hapticr",
                    _ => name.strip_suffix("_status").unwrap(),
                };
                backend.accessory_status(device)
            }
            "intellibox_com_port" => backend.accessory_status("intellibox_port"),
            "mc_com_port" => backend.accessory_status("compensation_port"),
            "belt_com_port" => backend.accessory_status("belt_port"),
            _ => {
                let path: &[&str] = match name {
                    "intellibox_model" => &["accessories", "intellibox_model"],
                    "intellibox_version" => &["accessories", "intellibox_version"],
                    "motion_warning" => &["motion", "warning"],
                    "motion_error" => &["motion", "error"],
                    "axis_assignment_status" => &["motion", "assignment", "status"],
                    "belt_model" => &["accessories", "belt_identity", "model"],
                    "belt_version" => &["accessories", "belt_identity", "version"],
                    "belt_sn" => &["accessories", "belt_identity", "serial"],
                    _ => return Ok(LegacyValue::None),
                };
                let status = LegacyValue::from(backend.call(Method::Get, "status", None)?);
                Ok(native_field(&status, path).cloned().unwrap_or_else(|_| match name {
                    "axis_assignment_status" | "motion_error" => false.into(),
                    "motion_warning" | "belt_model" | "belt_sn" => "".into(),
                    "intellibox_model" | "intellibox_version" | "belt_version" => (-1).into(),
                    _ => LegacyValue::None,
                }))
            }
        }
    };
    match result {
        Err(error) if error.detail.starts_with("KeyError:") => Ok(LegacyValue::None),
        result => result,
    }
}

/// A failed call: the status and the problem's detail.
#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    pub status: u16,
    pub detail: String,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.detail, self.status)
    }
}

/// Settings key access for adapters backed by projected settings documents.
pub fn projected_native_setting<B: Backend + ?Sized>(
    backend: &B,
    section: &str,
    key: &str,
) -> Result<LegacyValue, Failure> {
    if section == "SHORTCUTS" {
        if let Some((action, field)) = shortcut_field(key) {
            let settings = LegacyValue::from(backend.call(Method::Get, "settings", None)?);
            return native_field(&settings, &["shortcuts", "bindings", action, field]).cloned().map_err(|detail| {
                Failure {
                    status: 500,
                    detail: if detail.starts_with("KeyError:") {
                        format!("KeyError: {}", openrigcue_model::python_repr::repr_str(key))
                    } else {
                        detail
                    },
                }
            });
        }
    }
    if section == "LED" {
        if let Some((index, field)) = led_range_field(key) {
            let settings = LegacyValue::from(backend.call(Method::Get, "settings", None)?);
            return (|| {
                let segments = native_field(&settings, &["leds", "segments"])?;
                let LegacyValue::List(segments) = segments else {
                    return Err("TypeError: LED segments is not an array".into());
                };
                let segment = segments.get(index).ok_or("IndexError: LED segment is missing")?;
                native_field(segment, &[field]).cloned().map(|value| {
                    if field == "kind" {
                        led_segment_type(value, true)
                    } else {
                        value
                    }
                })
            })()
            .map_err(|detail| Failure { status: 500, detail });
        }
    }
    let path = if let Some((section, key)) = serial_setting_path(section, key) {
        [section, key]
    } else {
        match (section, key) {
            ("NETWORK_PORT", key) => ["network_ports", key],
            ("PORT_FORWARDING", key) => ["port_forwarding", key],
            ("APP", "playstation_ip") => ["app", "playstation_address"],
            ("APP", "webui_hostname") => ["app", "web_ui_host"],
            ("APP", "webui_port") => ["app", "web_ui_port"],
            ("APP", "shift_sound") => ["app", "gear_shift_sound"],
            ("APP", "x_button_exit") => ["app", "close_button_quits"],
            ("APP", "startup_page") => ["app", "start_page"],
            ("APP", key @ ("voice_notifications" | "start_minimized" | "log_level")) => ["app", key],
            ("MOTION_COMPENSATION", key @ ("pitch" | "roll" | "yaw" | "sway" | "surge" | "heave")) => {
                ["compensation", key]
            }
            ("APP", "measurement_unit") => ["app", "speed_unit"],
            ("MOTION_COMPENSATION", "type") => ["compensation", "sensor"],
            ("MOTION_COMPENSATION", "installation") => ["compensation", "installation"],
            ("MOTION_COMPENSATION", "surge_horizontal_reverse") => ["compensation", "surge_horizontal_reverse"],
            ("MOTION_COMPENSATION", "mm_distance_left_right") => ["compensation", "left_right_distance_mm"],
            ("MOTION_COMPENSATION", "mm_distance_front_back") => ["compensation", "front_back_distance_mm"],
            ("MOTION_COMPENSATION", "vertical_actuators_stroke") => ["compensation", "actuator_vertical_stroke"],
            (
                "MOTION_COMPENSATION",
                key @ ("height"
                | "surge_horizontal_stroke"
                | "pitch_factor"
                | "roll_factor"
                | "yaw_factor"
                | "sway_factor"
                | "surge_factor"
                | "surge_horizontal_factor"
                | "swap_roll_pitch"),
            ) => ["compensation", key],
            ("INTELLIBOX", "nbr_fans") => ["wind", "fan_count"],
            ("INTELLIBOX", "wind_type") => ["wind", "fan_kind"],
            ("LED", "nbr_leds") => ["leds", "nbr_leds"],
            ("LED", "nbr_segments") => ["leds", "nbr_segments"],
            ("LED", "model") => ["leds", "model"],
            ("LED", "brightness") => ["leds", "brightness"],
            ("HUD", "model") => ["hud", "model"],
            ("IFLAG", "brightness") => ["iflag", "brightness"],
            ("IFLAG", "rotation") => ["iflag", "rotation_degrees"],
            ("IFLAG", "mirror") => ["iflag", "mirror"],
            ("APP", "enable_hapticr") => ["app", "enable_hapticr"],
            ("APP", "disable_tuning_alert_standby") => ["app", "hide_standby_tuning_alert"],
            ("CUSTOM_FEATURES", "show_debug_page") => ["features", "show_debug_page"],
            ("CUSTOM_FEATURES", "hide_effects_not_in_game") => ["features", "hide_effects_not_in_game"],
            ("CUSTOM_FEATURES", "smoothing_slider_step") => ["features", "smoothing_slider_step"],
            ("CUSTOM_FEATURES", "allow_edit_emulator") => ["features", "standby_tuning_editable"],
            ("CUSTOM_FEATURES", "enable_dyadic_surge") => ["features", "dyadic_surge"],
            ("CUSTOM_FEATURES", "enable_motion_measurement_mode") => ["features", "motion_measurement_mode"],
            ("MOTIONBOX", "model") => ["motion", "model"],
            ("MOTIONBOX", "axis_assignment") => ["motion", "motor_axes"],
            ("MOTIONBOX", "refresh_rate") => ["motion", "refresh_rate"],
            ("MOTIONBOX", "reaction_speed") => ["motion", "reaction_speed"],
            ("MOTIONBOX", "auto_smoothing_low_speed") => ["motion", "auto_smoothing_low_speed"],
            ("MOTIONBOX", "intelli_tune_capture_secs") => ["motion", "intellitune_capture_seconds"],
            ("SIM3D", "model") => ["sim3d", "model"],
            ("SIM3D", key @ ("port1" | "port2" | "port3" | "port4")) => ["sim3d", key],
            ("SHAKEBOX", "soundcard") => ["shakers", "sound_card"],
            ("SHAKEBOX", "profile") => ["shakers", "speaker_layout"],
            ("SHAKEBOX", "enable_test_effect") => ["shakers", "test_effect_enabled"],
            ("BELT_TENSIONER", "belt_calibration_factor") => ["belt", "calibration_factor"],
            ("BELT_TENSIONER", "belt_calibration_retraction") => ["belt", "calibration_retraction"],
            ("PROSIMU", "invert_tl") => ["prosimu", "invert_traction_loss"],
            ("PROSIMU", "prs200_com_port1") => ["prosimu", "prs200_serial_port_1"],
            ("PROSIMU", "prs200_com_port2") => ["prosimu", "prs200_serial_port_2"],
            ("VNM SIMULATION", "vnm_com_port") => ["vnm", "serial_port"],
            ("PT-ACTUATOR", "pt_com_port") => ["pt_actuator", "canbus_serial_port"],
            ("PT-ACTUATOR", "vertical_actuators_stroke") => ["pt_actuator", "actuator_vertical_stroke"],
            ("PT-ACTUATOR", "udp_config") => ["pt_actuator", "udp_targets"],
            ("THANOS", "vertical_actuators_stroke") => ["thanos", "actuator_vertical_stroke"],
            ("THANOS", "lateral_distance_actuators") => ["thanos", "actuator_lateral_distance"],
            ("PT-ACTUATOR", "lateral_distance_actuators") => ["pt_actuator", "actuator_lateral_distance"],
            ("VNM SIMULATION", "vertical_actuators_stroke") => ["vnm", "actuator_vertical_stroke"],
            ("Yaw VR", "6dof_mode") => ["yaw_vr", "six_dof_mode"],
            ("DOF REALITY", "update_kp") => ["dof_reality", "update_kp"],
            (
                "VNM SIMULATION",
                key @ ("reverse_controller_port_5"
                | "reverse_controller_port_6"
                | "reverse_controller_port_7"
                | "reverse_controller_port_8"
                | "reverse_controller_port_9"),
            ) => ["vnm", key],
            (
                section @ ("THANOS" | "PT-ACTUATOR"),
                key @ ("reverse_controller_port_5"
                | "reverse_controller_port_6"
                | "reverse_controller_port_7"
                | "reverse_controller_port_8"),
            ) => [if section == "THANOS" { "thanos" } else { "pt_actuator" }, key],
            ("Yaw VR", key @ ("vibration_power" | "auto_park" | "center_before_start_alert")) => ["yaw_vr", key],
            _ => return Err(Failure { status: 500, detail: "native setting is unavailable in this adapter".into() }),
        }
    };
    let settings = LegacyValue::from(backend.call(Method::Get, "settings", None)?);
    let value = native_field(&settings, &path).map_err(|detail| Failure { status: 500, detail })?.clone();
    Ok(match (section, key, value) {
        ("APP", "measurement_unit", LegacyValue::Str(value)) if value == "kmh" => LegacyValue::Str("K".into()),
        ("APP", "measurement_unit", LegacyValue::Str(value)) if value == "mph" => LegacyValue::Str("M".into()),
        ("MOTION_COMPENSATION", "type", LegacyValue::Str(value)) if value == "virtual" => LegacyValue::Str("V".into()),
        ("MOTION_COMPENSATION", "type", LegacyValue::Str(value)) if value == "witmotion" => {
            LegacyValue::Str("W".into())
        }
        (_, _, value) => value,
    })
}

/// The service, as the UI thread sees it. Calls are synchronous: the UI runs on its own threads (the
/// main loop thread and the server thread).
pub trait Backend: Send + Sync {
    /// Current active game_config, retaining stored value kinds in the production backend.
    fn native_tune(&self) -> Result<LegacyValue, Failure> {
        let profile = self.call(Method::Get, "profile", None)?;
        Ok(LegacyValue::from(profile.get("tune").cloned().unwrap_or(Value::Null)))
    }

    fn intellitune_samples(&self) -> Result<LegacyValue, Failure> {
        self.call(Method::Get, "intellitune", None).map(LegacyValue::from)
    }

    fn clear_intellitune_samples(&self) -> Result<(), Failure> {
        self.call(Method::Delete, "intellitune", None).map(|_| ())
    }

    fn set_intellitune_capture(&self, capture: bool) -> Result<(), Failure> {
        self.call(Method::Patch, "controls", Some(patch_for(&["telemetry", "intellitune_capture"], capture.into())))
            .map(|_| ())
    }

    fn telemetry_history(&self) -> Result<LegacyValue, Failure> {
        self.call(Method::Get, "telemetry/history", None).map(LegacyValue::from)
    }

    /// The latched display flags, distinct from later edits to additional_telemetry.
    fn telemetry_all_channels(&self) -> bool {
        self.call(Method::Get, "status", None)
            .ok()
            .and_then(|status| status["session"]["show_all_channels"].as_bool())
            .unwrap_or(false)
    }

    fn assign_telemetry_plot(&self, channel: &str) -> Result<(), Failure> {
        self.call(Method::Patch, "controls", Some(patch_for(&["telemetry", "plot_channel"], channel.into())))
            .map(|_| ())
    }

    fn clear_telemetry_history(&self) -> Result<(), Failure> {
        self.call(Method::Delete, "telemetry/history", None).map(|_| ())
    }

    fn set_telemetry_reset(&self, reset: bool) -> Result<(), Failure> {
        self.call(Method::Patch, "controls", Some(patch_for(&["telemetry", "reset_min_max"], reset.into()))).map(|_| ())
    }

    fn reset_telemetry(&self) -> Result<(), Failure> {
        self.call(Method::Post, "telemetry/reset", Some(serde_json::json!({})))?;
        self.set_telemetry_reset(true)
    }

    /// An optional runtime parameter, retaining stored values at the snapshot's individual read positions.
    fn hardware_parameter(&self, name: &str) -> Result<LegacyValue, Failure> {
        hardware_parameter_from_backend(self, name)
    }

    /// Current accessory status, held port (`<name>_port`) or numeric iflag_firmware parameter.
    fn accessory_status(&self, name: &str) -> Result<LegacyValue, Failure> {
        let status = LegacyValue::from(self.call(Method::Get, "status", None)?);
        native_field(&status, &["accessories", name]).cloned().map_err(|detail| Failure { status: 500, detail })
    }

    /// Current runtime parameter, retaining stored value kinds in the production accessor.
    fn control_value(&self, path: &[&str]) -> Result<LegacyValue, Failure> {
        let controls = LegacyValue::from(self.call(Method::Get, "controls", None)?);
        let value = native_field(&controls, path).cloned().map_err(|detail| Failure { status: 500, detail })?;
        if matches!(path, ["switches", "wind" | "wind_center" | "led" | "tach"]) {
            if let LegacyValue::Str(mode) = &value {
                match mode.as_str() {
                    "off" => return Ok(LegacyValue::Int(0)),
                    "auto" => return Ok(LegacyValue::Int(1)),
                    "always_on" => return Ok(LegacyValue::Int(2)),
                    _ => {}
                }
            }
        }
        Ok(value)
    }

    fn assign_motion_test_value(&self, key: &str, value: &LegacyValue, _projection: f64) -> Result<(), Failure> {
        let value = native_json(value).map_err(|detail| Failure { status: 500, detail })?;
        self.call(Method::Patch, "controls", Some(patch_for(&["tests", "motion_sliders", key], value))).map(|_| ())
    }

    fn assign_belt_test_value(&self, value: &LegacyValue, _projection: f64) -> Result<(), Failure> {
        let value = native_json(value).map_err(|detail| Failure { status: 500, detail })?;
        self.call(Method::Patch, "controls", Some(patch_for(&["tests", "belt_test_value"], value))).map(|_| ())
    }

    fn assign_yaw_limit(&self, key: &str, value: i64) -> Result<(), Failure> {
        self.call(Method::Patch, "motion/yaw-vr/limits", Some(patch_for(&[key], value.into()))).map(|_| ())
    }

    /// Direct assignment of motion_homing after the caller's current connection check.
    fn request_yaw_center(&self) -> Result<(), Failure> {
        self.call(Method::Post, "motion/center", Some(serde_json::json!({}))).map(|_| ())
    }

    fn cached_profiles(&self) -> Result<Vec<openrigcue_config::synced_profiles::Entry>, Failure> {
        let root =
            openrigcue_config::synced_profiles::default_root().map_err(|detail| Failure { status: 500, detail })?;
        openrigcue_config::synced_profiles::list(&root).map_err(|detail| Failure { status: 500, detail })
    }

    fn cached_profile(&self, id: &str) -> Result<(openrigcue_config::synced_profiles::Entry, LegacyValue), Failure> {
        let root =
            openrigcue_config::synced_profiles::default_root().map_err(|detail| Failure { status: 500, detail })?;
        openrigcue_config::synced_profiles::load(&root, id).map_err(|detail| Failure { status: 500, detail })
    }

    fn active_profile_game(&self) -> Result<String, Failure> {
        let profile = self.call(Method::Get, "profile", None)?;
        match profile.get("game").and_then(Value::as_str) {
            Some(game) => Ok(game.into()),
            None => match self.game_value()? {
                LegacyValue::Str(game) => Ok(game),
                _ => Err(Failure { status: 500, detail: "Active profile game is unavailable".into() }),
            },
        }
    }

    fn active_profile_vehicle(&self) -> Result<LegacyValue, Failure> {
        let profile = self.call(Method::Get, "profile", None)?;
        Ok(match profile.get("vehicle") {
            Some(Value::String(name)) => LegacyValue::Str(name.clone()),
            _ => self.session_value("vehicle_name")?,
        })
    }

    fn soundcard_refresh_requested(&self) -> Result<bool, Failure> {
        let status = LegacyValue::from(self.call(Method::Get, "status", None)?);
        native_field(&status, &["shakers", "refresh_soundcard"])
            .map(LegacyValue::is_truthy)
            .map_err(|detail| Failure { status: 500, detail })
    }

    /// The currently running game; JSON null represents the stand-by string.
    fn game_value(&self) -> Result<LegacyValue, Failure> {
        let status = LegacyValue::from(self.call(Method::Get, "status", None)?);
        let game =
            native_field(&status, &["session", "game"]).map_err(|detail| Failure { status: 500, detail })?.clone();
        Ok(if game == LegacyValue::None { LegacyValue::Str("none".into()) } else { game })
    }

    /// Required lookup of the running game's name in the current catalog.
    fn game_name_value(&self, game: &LegacyValue) -> Result<LegacyValue, Failure> {
        let key = game_catalog_key(game)?;
        let catalog = self.call(Method::Get, "catalog/games", None)?;
        let row =
            catalog.as_array().and_then(|rows| rows.iter().find(|row| row["id"].as_str() == Some(key))).ok_or_else(
                || Failure { status: 500, detail: format!("KeyError: {}", openrigcue_model::python_repr::repr(game)) },
            )?;
        let row = LegacyValue::from(row.clone());
        native_field(&row, &["name"]).cloned().map_err(|detail| Failure { status: 500, detail })
    }

    /// Selected current session parameters; production keeps the stored vehicle value.
    fn session_value(&self, parameter: &str) -> Result<LegacyValue, Failure> {
        let field = match parameter {
            "listener_running" => "listener",
            "vehicle_name" => "vehicle",
            _ => {
                return Err(Failure {
                    status: 500,
                    detail: format!("KeyError: {}", openrigcue_model::python_repr::repr_str(parameter)),
                });
            }
        };
        let status = LegacyValue::from(self.call(Method::Get, "status", None)?);
        let value =
            native_field(&status, &["session", field]).map_err(|detail| Failure { status: 500, detail })?.clone();
        Ok(match (parameter, value) {
            ("listener_running", LegacyValue::None) => "none".into(),
            ("vehicle_name", LegacyValue::None) => LegacyValue::Int(0),
            (_, value) => value,
        })
    }

    /// MyApp's previous actual vehicle; the JSON view falls back to displayed text.
    fn previous_vehicle_value(&self) -> Result<LegacyValue, Failure> {
        let status = LegacyValue::from(self.call(Method::Get, "status", None)?);
        if let Ok(value) = native_field(&status, &["previous_actual_vehicle_name"]) {
            return Ok(value.clone());
        }
        Ok(match native_field(&status, &["displayed_vehicle"]) {
            Ok(value) if value != &LegacyValue::None => value.clone(),
            _ => LegacyValue::Int(0),
        })
    }

    /// One pending assignment, independently of later model reads or device I/O.
    fn update_thanos_pending(&self, secondary: bool, replace: bool, entries: &[(String, i64)]) -> Result<(), Failure> {
        self.call(
            Method::Put,
            "motion/thanos/pending",
            Some(serde_json::json!({"secondary":secondary,"replace":replace,"entries":entries})),
        )
        .map(|_| ())
    }

    /// The UI's reads at each assignment; JSON adapters retain their single-value endpoint.
    fn apply_all_tunes(&self, spike: bool, value: &mut dyn FnMut() -> Result<i64, String>) -> Result<(), Failure> {
        let value = value().map_err(|detail| Failure { status: 500, detail })?;
        self.call(
            Method::Post,
            "tunes/apply-all",
            Some(serde_json::json!({"field":if spike {"spike_filter"} else {"max_angle"},"value":value})),
        )
        .map(|_| ())
    }

    /// Current driver parameters without the broad status-route lock in production.
    fn motion_parameter(&self, name: &str) -> Result<LegacyValue, Failure> {
        let status = LegacyValue::from(self.call(Method::Get, "status", None)?);
        if name == "Yaw VR" {
            let parameters =
                native_field(&status, &["devices", "yaw_vr"]).map_err(|detail| Failure { status: 500, detail })?;
            let LegacyValue::Dict(fields) = parameters else { return Ok(parameters.clone()) };
            let mut fields = fields.clone();
            if let Some(LegacyValue::Dict(limits)) = fields.remove("limits") {
                for (key, value) in limits.iter() {
                    fields.insert(key.clone(), value.clone());
                }
            }
            return Ok(LegacyValue::Dict(fields));
        }
        if name == "actuators_position" {
            use openrigcue_model::native_value::Dict;
            let mut positions = match native_field(&status, &["motion", "actuators"]) {
                Ok(LegacyValue::Dict(fields)) => fields.clone(),
                Ok(LegacyValue::None) | Err(_) => {
                    let mut fields = Dict::new();
                    for key in ["front_left", "front_right", "rear_left", "rear_right"] {
                        fields.insert(key, LegacyValue::Int(0));
                    }
                    fields
                }
                Ok(value) => return Ok(value.clone()),
            };
            if let Some(motors) = positions.remove("motors") {
                let LegacyValue::List(motors) = motors else {
                    return Err(Failure { status: 500, detail: "actuator motor view is not an array".into() });
                };
                for (index, value) in motors.into_iter().take(6).enumerate() {
                    positions.insert(format!("m{index}"), value);
                }
            }
            return Ok(LegacyValue::Dict(positions));
        }
        if name == "actuators" {
            use openrigcue_model::native_value::{Dict, Key};
            let rows = match native_field(&status, &["motion", "assignment", "actuators"]) {
                Ok(LegacyValue::List(rows)) => rows,
                Ok(LegacyValue::None) | Err(_) => return Ok(LegacyValue::Dict(Dict::new())),
                Ok(_) => return Err(Failure { status: 500, detail: "actuator view is not an array".into() }),
            };
            let mut actuators = Dict::new();
            for row in rows {
                let key = native_field(row, &["key"]).map_err(|detail| Failure { status: 500, detail })?;
                let key = match key {
                    LegacyValue::Str(value) => Key::Str(value.clone()),
                    LegacyValue::Int(value) => Key::Int(*value),
                    LegacyValue::Bool(value) => Key::Bool(*value),
                    LegacyValue::None => Key::None,
                    LegacyValue::Float(value) if !value.is_nan() => Key::float(*value).unwrap(),
                    _ => return Err(Failure { status: 500, detail: "unsupported actuator-view key".into() }),
                };
                actuators.insert(key, row.clone());
            }
            return Ok(LegacyValue::Dict(actuators));
        }
        if name == "prs200_snc6_tl_com_port" {
            // A missing assignment view means no driver has published one yet. The parameter
            // already exists then, with the empty string; an explicitly present value is retained.
            return Ok(native_field(&status, &["motion", "assignment", "tl_port"])
                .cloned()
                .unwrap_or_else(|_| LegacyValue::Str(String::new())));
        }
        let path: &[&str] = match name {
            "motion_status" => &["motion", "link", "status"],
            "motion_com_port" => &["motion", "link", "ports"],
            "thanos_config" => &["devices", "thanos", "config"],
            "thanos_config2" => &["devices", "thanos", "config2"],
            "thanos_controller" => &["devices", "thanos", "controller"],
            "thanos_firmware" => &["devices", "thanos", "firmware"],
            "yaw_limit_enabled" => &["devices", "yaw_vr", "limits", "yaw_limit_enabled"],
            _ => return Err(Failure { status: 500, detail: format!("KeyError: {name:?}") }),
        };
        let value = match native_field(&status, path) {
            Ok(value) => value.clone(),
            Err(_) if matches!(name, "thanos_config" | "thanos_config2") => LegacyValue::Dict(Default::default()),
            Err(detail) => return Err(Failure { status: 500, detail }),
        };
        Ok(match (name, value) {
            ("motion_status", LegacyValue::Str(status)) if status == "NotConnected" => {
                LegacyValue::Str("Not connected".into())
            }
            ("motion_status", LegacyValue::Str(status)) if status == "NotConnectedAfterWriteError" => {
                LegacyValue::Str("Not Connected".into())
            }
            (_, value) => value,
        })
    }

    /// One current motion_sensor parameter; the production implementation reads the shared slot.
    fn motion_sensor_value(&self, key: &str) -> Result<LegacyValue, Failure> {
        let (effect, field) = if let Some(effect) = key.strip_prefix("raw_") {
            (effect, "raw")
        } else if let Some(effect) = key.strip_prefix("calculated_") {
            (effect, "calculated")
        } else {
            (key, "value")
        };
        let sensor = LegacyValue::from(self.call(Method::Get, "compensation", None)?);
        native_field_owned(&sensor, &["motion_sensor", effect, field]).map_err(|detail| Failure { status: 500, detail })
    }

    /// An optional settings section (empty when absent) for a snapshot; adapters can project the requested keys.
    fn native_config_section(&self, section: &str, keys: &[&str]) -> Result<LegacyValue, Failure> {
        let mut values = openrigcue_model::native_value::Dict::new();
        for key in keys {
            match self.native_setting(section, key) {
                Ok(value) => {
                    values.insert(*key, value);
                }
                Err(error) if error.detail.starts_with("KeyError:") => {}
                Err(error) => return Err(error),
            }
        }
        Ok(LegacyValue::Dict(values))
    }

    /// Required settings lookup by stored key names; fixtures map the supported UI-view fields.
    fn native_setting(&self, section: &str, key: &str) -> Result<LegacyValue, Failure> {
        projected_native_setting(self, section, key)
    }

    /// Direct MOTIONBOX assignment, retaining the required section and stored value in production.
    fn assign_motion_setting(&self, key: &str, value: &LegacyValue) -> Result<(), Failure> {
        self.assign_setting_value("MOTIONBOX", key, value)
    }

    /// Loads the LED colour table again when the Hardware page asks for it; in-memory fixtures have no live
    /// colour cache.
    fn reload_led_colors(&self) -> Result<(), Failure> {
        Ok(())
    }

    /// Required settings section assignment. The fixture adapter supports the current callbacks.
    fn assign_setting_value(&self, section_name: &str, key: &str, value: &LegacyValue) -> Result<(), Failure> {
        if section_name == "SHORTCUTS" {
            if let Some((action, field)) = shortcut_field(key) {
                let settings = LegacyValue::from(self.call(Method::Get, "settings", None)?);
                let bindings = native_field(&settings, &["shortcuts", "bindings"])
                    .map_err(|detail| Failure { status: 500, detail })?;
                if bindings.as_dict().is_none() {
                    return Err(Failure {
                        status: 500,
                        detail: "TypeError: shortcut bindings is not a dictionary".into(),
                    });
                }
                let value = native_json(value).map_err(|detail| Failure { status: 500, detail })?;
                return self
                    .call(Method::Patch, "settings", Some(patch_for(&["shortcuts", "bindings", action, field], value)))
                    .map(|_| ());
            }
        }
        if section_name == "LED" {
            if let Some((index, field)) = led_range_field(key) {
                let settings = LegacyValue::from(self.call(Method::Get, "settings", None)?);
                let segments = (|| {
                    let mut segments = native_field(&settings, &["leds", "segments"])?.clone();
                    let LegacyValue::List(entries) = &mut segments else {
                        return Err("TypeError: LED segments is not an array".into());
                    };
                    let segment = entries.get_mut(index).ok_or("IndexError: LED segment is missing")?;
                    let value = if field == "kind" { led_segment_type(value.clone(), false) } else { value.clone() };
                    segment.assign_item(field, value).map_err(|detail| format!("TypeError: {detail}"))?;
                    native_json(&segments)
                })()
                .map_err(|detail| Failure { status: 500, detail })?;
                return self
                    .call(Method::Patch, "settings", Some(patch_for(&["leds", "segments"], segments)))
                    .map(|_| ());
            }
        }
        let (section_name, key) = if let Some(path) = serial_setting_path(section_name, key) {
            path
        } else {
            match (section_name, key) {
                ("INTELLIBOX", "nbr_fans") => ("wind", "fan_count"),
                ("INTELLIBOX", "wind_type") => ("wind", "fan_kind"),
                ("LED", "nbr_leds") => ("leds", "nbr_leds"),
                ("LED", "nbr_segments") => ("leds", "nbr_segments"),
                ("LED", "model") => ("leds", "model"),
                ("HUD", "model") => ("hud", "model"),
                ("SHAKEBOX", "profile") => ("shakers", "speaker_layout"),
                ("SHAKEBOX", "soundcard") => ("shakers", "sound_card"),
                ("SIM3D", key @ ("model" | "port1" | "port2" | "port3" | "port4")) => ("sim3d", key),
                ("IFLAG", "rotation") => ("iflag", "rotation_degrees"),
                ("IFLAG", "mirror") => ("iflag", "mirror"),
                ("PORT_FORWARDING", key) => ("port_forwarding", key),
                ("APP", "playstation_ip") => ("app", "playstation_address"),
                ("MOTION_COMPENSATION", "surge_horizontal_reverse") => ("compensation", "surge_horizontal_reverse"),
                ("MOTION_COMPENSATION", "type") => ("compensation", "sensor"),
                ("MOTION_COMPENSATION", "installation") => ("compensation", "installation"),
                ("MOTION_COMPENSATION", "mm_distance_left_right") => ("compensation", "left_right_distance_mm"),
                ("MOTION_COMPENSATION", "mm_distance_front_back") => ("compensation", "front_back_distance_mm"),
                ("MOTION_COMPENSATION", "vertical_actuators_stroke") => ("compensation", "actuator_vertical_stroke"),
                (
                    "MOTION_COMPENSATION",
                    key @ ("height"
                    | "surge_horizontal_stroke"
                    | "pitch_factor"
                    | "roll_factor"
                    | "yaw_factor"
                    | "sway_factor"
                    | "surge_factor"
                    | "surge_horizontal_factor"
                    | "swap_roll_pitch"),
                ) => ("compensation", key),
                ("MOTIONBOX", key) => ("motion", key),
                ("PROSIMU", "invert_tl") => ("prosimu", "invert_traction_loss"),
                ("PROSIMU", "prs200_com_port1") => ("prosimu", "prs200_serial_port_1"),
                ("PROSIMU", "prs200_com_port2") => ("prosimu", "prs200_serial_port_2"),
                ("VNM SIMULATION", "vnm_com_port") => ("vnm", "serial_port"),
                ("PT-ACTUATOR", "pt_com_port") => ("pt_actuator", "canbus_serial_port"),
                ("PT-ACTUATOR", "lateral_distance_actuators") => ("pt_actuator", "actuator_lateral_distance"),
                ("PT-ACTUATOR", "vertical_actuators_stroke") => ("pt_actuator", "actuator_vertical_stroke"),
                ("PT-ACTUATOR", "udp_config") => ("pt_actuator", "udp_targets"),
                ("LED", "brightness") => ("leds", "brightness"),
                ("IFLAG", "brightness") => ("iflag", "brightness"),
                ("BELT_TENSIONER", "belt_calibration_factor") => ("belt", "calibration_factor"),
                ("BELT_TENSIONER", "belt_calibration_retraction") => ("belt", "calibration_retraction"),
                ("Yaw VR", key @ ("auto_park" | "center_before_start_alert" | "vibration_power")) => ("yaw_vr", key),
                ("Yaw VR", "6dof_mode") => ("yaw_vr", "six_dof_mode"),
                ("DOF REALITY", "update_kp") => ("dof_reality", "update_kp"),
                (
                    "VNM SIMULATION",
                    key @ ("reverse_controller_port_5"
                    | "reverse_controller_port_6"
                    | "reverse_controller_port_7"
                    | "reverse_controller_port_8"
                    | "reverse_controller_port_9"),
                ) => ("vnm", key),
                (
                    section @ ("THANOS" | "PT-ACTUATOR"),
                    key @ ("reverse_controller_port_5"
                    | "reverse_controller_port_6"
                    | "reverse_controller_port_7"
                    | "reverse_controller_port_8"),
                ) => (if section == "THANOS" { "thanos" } else { "pt_actuator" }, key),
                _ => {
                    return Err(Failure {
                        status: 500,
                        detail: "native setting assignment is unavailable in this adapter".into(),
                    });
                }
            }
        };
        let settings = LegacyValue::from(self.call(Method::Get, "settings", None)?);
        let mut section =
            native_field(&settings, &[section_name]).map_err(|detail| Failure { status: 500, detail })?.clone();
        section
            .assign_item(key, value.clone())
            .map_err(|detail| Failure { status: 500, detail: format!("TypeError: {detail}") })?;
        let value = native_json(value).map_err(|detail| Failure { status: 500, detail })?;
        let value = match (section_name, key, value) {
            ("compensation", "sensor", Value::String(value)) if value == "V" => Value::String("virtual".into()),
            ("compensation", "sensor", Value::String(value)) if value == "W" => Value::String("witmotion".into()),
            (_, _, value) => value,
        };
        self.call(Method::Patch, "settings", Some(patch_for(&[section_name, key], value))).map(|_| ())
    }

    /// One field through a required Motion Compensation axis.
    fn assign_mc_value(&self, effect: &str, field: &str, value: &LegacyValue) -> Result<(), Failure> {
        let mut axis = self.native_setting("MOTION_COMPENSATION", effect)?;
        axis.assign_item(field, value.clone())
            .map_err(|detail| Failure { status: 500, detail: format!("TypeError: {detail}") })?;
        let value = native_json(value).map_err(|detail| Failure { status: 500, detail })?;
        self.call(Method::Patch, "settings", Some(patch_for(&["compensation", effect, field], value))).map(|_| ())
    }

    /// The Games confirmation keeps forwarding container kinds in the in-process implementation.
    fn confirm_game_port(&self, entry: &str, port: i64, forwarding: &LegacyValue) -> Result<bool, Failure> {
        let forwarding = native_json(forwarding).map_err(|detail| Failure { status: 500, detail })?;
        let answer = self.call(
            Method::Put,
            &format!("games/{}/port", segment(entry)),
            Some(serde_json::json!({ "port": port, "forwarding": forwarding })),
        )?;
        Ok(answer.get("restart_required").and_then(Value::as_bool).unwrap_or(false))
    }

    /// Assign through existing parents; production retains kinds that JSON cannot represent.
    fn assign_tune_value(&self, path: &[&str], value: &LegacyValue) -> Result<(), Failure> {
        let (key, parents) =
            path.split_last().ok_or_else(|| Failure { status: 500, detail: "ValueError: empty tune path".into() })?;
        let tune = self.native_tune()?;
        let mut parent = native_field_owned(&tune, parents).map_err(|detail| Failure { status: 500, detail })?;
        parent
            .assign_item(key, value.clone())
            .map_err(|detail| Failure { status: 500, detail: format!("TypeError: {detail}") })?;
        let value = native_json(value).map_err(|detail| Failure { status: 500, detail })?;
        self.call(Method::Patch, "profile", Some(patch_for(path, value))).map(|_| ())
    }

    /// Main shaker callbacks assign the value as given, then reload channel groups even when unchanged.
    fn assign_shaker_value(&self, table: &str, key: &str, value: &LegacyValue) -> Result<(), Failure> {
        self.assign_tune_value(&[table, key], value)
    }

    /// The Profile Manager passes its vehicle argument unchanged, including game-level integer zero.
    fn begin_profile_edit(&self, game: &str, vehicle: &LegacyValue) -> Result<(), Failure> {
        let vehicle =
            serde_json::to_value(vehicle).map_err(|error| Failure { status: 400, detail: error.to_string() })?;
        self.call(Method::Post, "profile/edit", Some(serde_json::json!({"game":game,"vehicle":vehicle}))).map(|_| ())
    }

    /// The vehicle label actually displayed by the UI; None is a game-level save.
    /// Production also reads the current game directly at this call boundary.
    fn save_profile(&self, vehicle: Option<&str>) -> Result<(), Failure> {
        let mut request = serde_json::json!({"level": if vehicle.is_some() { "vehicle" } else { "game" }});
        if let Some(vehicle) = vehicle {
            request["vehicle"] = Value::from(vehicle);
        }
        self.call(Method::Post, "profile/save", Some(request)).map(|_| ())
    }

    /// The callbacks store effects after updating their vehicle-level UI state.
    /// The JSON save already includes this operation.
    fn store_shaker_effects(&self) -> Result<(), Failure> {
        Ok(())
    }

    /// Confirmation uses the offered content and does not repeat the offer's identity check.
    fn confirm_profile_import(&self, _file: &openrigcue_config::profiles::ProfileFile) -> Result<(), Failure> {
        Err(Failure { status: 500, detail: "native profile confirmation is unavailable".into() })
    }

    /// One live Profile Manager lookup, before any filtering or widget construction.
    /// Production reads the SQLite row; JSON fixture adapters may supply it directly.
    fn profile_row(&self, game: &str) -> Result<Option<LegacyValue>, Failure> {
        self.call(Method::Get, &format!("tunes/{}", segment(game)), None).map(|value| Some(value.into()))
    }

    /// Profile Manager buttons pass their vehicle argument unchanged and omit location.
    fn profile_export(&self, game: &str, vehicle: &LegacyValue) -> Result<(Vec<u8>, String), Failure> {
        self.profile_export_at(game, vehicle, None)
    }

    fn profile_export_at(
        &self,
        game: &str,
        vehicle: &LegacyValue,
        location: Option<&LegacyValue>,
    ) -> Result<(Vec<u8>, String), Failure> {
        let mut path = format!("profiles/export?game={}", segment(game));
        match vehicle {
            LegacyValue::Int(0) => (),
            LegacyValue::Str(vehicle) => path.push_str(&format!("&vehicle={}", segment(vehicle))),
            _ => return Err(Failure { status: 500, detail: "native profile export is unavailable".into() }),
        }
        if let Some(LegacyValue::Str(location)) = location {
            path.push_str(&format!("&location={}", segment(location)));
        }
        self.fetch(&path)
    }

    /// The current location; production reads its stored value directly.
    fn location_value(&self) -> LegacyValue {
        self.call(Method::Get, "status", None)
            .ok()
            .and_then(|status| status.pointer("/session/location").cloned())
            .filter(|value| !value.is_null())
            .map(LegacyValue::from)
            .unwrap_or(LegacyValue::Int(0))
    }

    /// Clear edit mode and report the branch that also resets the UI's remembered identities.
    fn clear_profile_edit(&self) -> Result<bool, Failure> {
        let status = self.call(Method::Get, "status", None)?;
        let edit = status.pointer("/controls/profile_edit").unwrap_or(&Value::Null);
        let idle = status.pointer("/session/listener").is_none_or(Value::is_null);
        let reset =
            !edit.is_null() && idle && edit.get("started_without_listener").and_then(Value::as_bool).unwrap_or(idle);
        self.call(Method::Delete, "profile/edit", None).map(|_| reset)
    }

    fn metadata_keys(&self, table: &str) -> Result<MetadataKeys, Failure> {
        let document = metadata_document(self, table)?;
        let rows = document
            .as_object()
            .ok_or_else(|| Failure { status: 500, detail: "TypeError: metadata table is not a dictionary".into() })?;
        Ok(Box::new(rows.keys().cloned().map(Ok).collect::<Vec<_>>().into_iter()))
    }

    fn shaker_effects(&self) -> Result<LegacyValue, Failure> {
        self.call(Method::Get, "shakers/effects", None).map(LegacyValue::from)
    }

    /// Applies the sound card after the separate setting assignment.
    fn apply_shaker_soundcard(&self) -> Result<(), Failure> {
        let name = native_json(&self.native_setting("SHAKEBOX", "soundcard")?)
            .map_err(|detail| Failure { status: 500, detail })?;
        self.call(Method::Put, "shakers/soundcard", Some(serde_json::json!({"name":name}))).map(|_| ())
    }

    /// Starts a shaker effect's test with the two listener values the Shake page captured beforehand.
    /// Production starts the test directly with them, without reading the session state again.
    fn test_shaker_effect(&self, key: &str, current: &LegacyValue, test: &LegacyValue) -> Result<(), Failure> {
        let current = native_json(current).map_err(|detail| Failure { status: 500, detail })?;
        let test = native_json(test).map_err(|detail| Failure { status: 500, detail })?;
        self.call(
            Method::Post,
            &format!("shakers/test/{}", segment(key)),
            Some(serde_json::json!({"current_listener":current,"test_listener":test})),
        )
        .map(|_| ())
    }

    /// Plays a speaker's test sound. The Hardware page has already checked that a sound card is set, so
    /// production plays the sound without checking again.
    fn test_shaker_speaker(&self, speaker: &str) -> Result<(), Failure> {
        self.call(Method::Post, &format!("shakers/speakers/{}/test", segment(speaker)), None).map(|_| ())
    }

    /// The advanced dialog publishes its copied matrix before deriving combined groups.
    /// Production overrides the JSON adapter to retain partial assignments and stored value kinds.
    fn confirm_shaker_levels(&self, speaker: Option<&str>, levels: &LegacyValue) -> Result<(), Failure> {
        let speaker = speaker.ok_or_else(|| Failure { status: 500, detail: "KeyError: None".into() })?;
        let levels = native_json(levels).map_err(|detail| Failure { status: 500, detail })?;
        self.call(Method::Put, &format!("shakers/speakers/{}/matrix", segment(speaker)), Some(levels)).map(|_| ())
    }

    fn load_shaker_effects(&self) -> Result<(), Failure> {
        self.call(Method::Post, "shakers/effects/reload", None).map(|_| ())
    }

    fn assign_shaker_profile(&self, name: &str) -> Result<Value, Failure> {
        self.call(Method::Post, "shakers/assign-profile", Some(serde_json::json!({"name":name})))
    }

    fn copy_shaker_profile(&self, source: &str, name: &str) -> Result<(), Failure> {
        self.call(Method::Post, "shakers/rows/copy", Some(serde_json::json!({"source":source,"name":name}))).map(|_| ())
    }

    fn delete_shaker_profile(&self, name: &str) -> Result<(), Failure> {
        self.call(Method::Post, "shakers/rows/delete", Some(serde_json::json!({"name":name}))).map(|_| ())
    }

    fn recheck_shakers_surround(&self) -> Result<(), Failure> {
        self.call(Method::Post, "shakers/surround-recheck", None).map(|_| ())
    }

    /// The two App-page flags live outside the settings file, in the registry.
    fn launcher_flag(&self, key: &str) -> Result<Option<LegacyValue>, Failure> {
        let field = launcher_field(key)?;
        let settings = self.call(Method::Get, "settings", None)?;
        Ok(settings.get("app").and_then(|app| app.get(field)).cloned().map(LegacyValue::from))
    }

    fn write_launcher_flag(&self, key: &str, enabled: bool) -> Result<(), Failure> {
        let field = launcher_field(key)?;
        let mut app = serde_json::Map::new();
        app.insert(field.into(), Value::Bool(enabled));
        self.call(Method::Patch, "settings", Some(serde_json::json!({"app":app}))).map(|_| ())
    }

    /// Direct APP settings assignment used by the callbacks. Production
    /// overrides this to retain stored value kinds rather than using a typed API patch.
    fn assign_app_setting(&self, key: &str, value: Value) -> Result<Value, Failure> {
        let settings = self.call(Method::Get, "settings", None)?;
        let mut app = field(&settings, &["app"]).map_err(|detail| Failure { status: 500, detail })?.clone();
        let mut native = openrigcue_config::import::value::LegacyValue::from(app.clone());
        native
            .assign_item(key, openrigcue_config::import::value::LegacyValue::from(value.clone()))
            .map_err(|error| Failure { status: 500, detail: format!("TypeError: {error}") })?;
        let field = app_field(key);
        let value = if key == "measurement_unit" {
            match value.as_str() {
                Some("K") => Value::from("kmh"),
                Some("M") => Value::from("mph"),
                _ => value,
            }
        } else {
            value
        };
        app.as_object_mut().expect("validated dictionary").insert(field.into(), value);
        self.call(Method::Patch, "settings", Some(serde_json::json!({"app":app})))
    }

    fn assign_window_setting(&self, key: &str, value: Value) -> Result<Value, Failure> {
        self.assign_app_setting(key, value)
    }

    /// Direct dictionary operations. Production overrides these with retained
    /// SQLite workers; simple fake backends can keep their JSON documents.
    fn metadata_row(&self, table: &str, key: &str) -> Result<Option<LegacyValue>, Failure> {
        let document = metadata_document(self, table)?;
        Ok(document.as_object().and_then(|rows| rows.get(key)).cloned().map(LegacyValue::from))
    }

    fn metadata_contains(&self, table: &str, key: &str) -> Result<bool, Failure> {
        let document = metadata_document(self, table)?;
        Ok(document.as_object().is_some_and(|rows| rows.contains_key(key)))
    }

    fn metadata_rows(&self, table: &str) -> Result<MetadataRows, Failure> {
        let document = metadata_document(self, table)?;
        let rows = document
            .as_object()
            .ok_or_else(|| Failure { status: 500, detail: "TypeError: metadata table is not a dictionary".into() })?;
        let rows: Vec<_> =
            rows.iter().map(|(key, value)| Ok((key.clone(), LegacyValue::from(value.clone())))).collect();
        Ok(Box::new(rows.into_iter()))
    }

    fn close_opened_files(&self) {}

    /// onpagehide sends SIGKILL after its close calls, whose errors are absorbed.
    fn quit_after_pagehide(&self) -> Result<(), Failure> {
        nix::sys::signal::kill(nix::unistd::Pid::this(), nix::sys::signal::Signal::SIGKILL)
            .map_err(|error| Failure { status: 500, detail: error.to_string() })
    }
    /// The desktop-only pre-Save position and monitor assignments. This happens
    /// when the Save dialog opens, even if the user subsequently cancels it.
    fn capture_window_geometry(&self) {}
    /// Direct service operations at the corresponding points of MyApp.main_loop. These do not create
    /// another loop or require a browser connection.
    fn synchronize_profile(&self) -> Result<(), Failure> {
        Ok(())
    }
    /// The reload decision before taking update_lock. None uses an adapter's existing load-generation view.
    fn profile_reload_needed(&self) -> Result<Option<bool>, Failure> {
        Ok(None)
    }
    /// Load after acquiring update_lock; the daemon reads current inputs again here.
    fn load_current_profile(&self) -> Result<(), Failure> {
        self.synchronize_profile()
    }
    /// The reload's setup check, after IntelliTune style and before the Thanos filters.
    fn check_game_config(&self) -> Result<(), Failure> {
        Ok(())
    }
    /// The reload's belt mode assignment, after the effect/IFlag rows.
    fn assign_belt_switch(&self, enabled: bool) -> Result<(), Failure> {
        self.call(Method::Patch, "controls", Some(serde_json::json!({"switches": {"belt": enabled}}))).map(|_| ())
    }
    /// `previous_actual_vehicle_name` at its readout-stage assignment.
    fn remember_actual_vehicle(&self) -> Result<(), Failure> {
        Ok(())
    }
    /// The no-game reset after both vehicle and location have been remembered.
    fn clear_remembered_vehicle(&self) -> Result<(), Failure> {
        Ok(())
    }
    /// Previous game/vehicle inputs, after sidebar refresh and before notifications.
    fn remember_profile_inputs(&self) -> Result<(), Failure> {
        Ok(())
    }
    fn hardware_notification(&self, _notification: HardwareNotification) -> Result<(), Failure> {
        Ok(())
    }
    fn hardware_notification_state(&self) -> HardwareNotificationState {
        HardwareNotificationState::default()
    }
    fn finish_main_loop(&self) -> Result<(), Failure> {
        Ok(())
    }
    fn ui_actions(&self) -> Vec<String> {
        Vec::new()
    }
    fn ui_action_pending(&self, action: &str) -> bool {
        self.ui_actions().iter().any(|queued| queued == action)
    }
    fn clear_ui_actions(&self) {}

    /// One call of the service API (`path` below `/api/v1/`, without the leading slash); the JSON answer
    /// (`null` for an empty one).
    fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value, Failure>;

    /// A POST of raw bytes (a file handed to the service).
    fn upload(&self, path: &str, bytes: Vec<u8>) -> Result<Value, Failure>;

    /// A GET of a file the service makes: its contents and the file name it gives.
    fn fetch(&self, path: &str) -> Result<(Vec<u8>, String), Failure>;

    /// `text` translated into the selected language.
    fn translate(&self, text: &str) -> String;

    /// The strings excluded from translation (product and game names).
    fn excluded_strings(&self) -> Vec<String>;
}

/// Shorthands for [`Backend::call`].
pub trait BackendExt {
    fn get(&self, path: &str) -> Result<Value, Failure>;
    fn post(&self, path: &str, body: Value) -> Result<Value, Failure>;
    fn put(&self, path: &str, body: Value) -> Result<Value, Failure>;
    fn patch(&self, path: &str, body: Value) -> Result<Value, Failure>;
    fn delete(&self, path: &str) -> Result<Value, Failure>;
}

impl<B: Backend + ?Sized> BackendExt for B {
    fn get(&self, path: &str) -> Result<Value, Failure> {
        self.call(Method::Get, path, None)
    }
    fn post(&self, path: &str, body: Value) -> Result<Value, Failure> {
        self.call(Method::Post, path, Some(body))
    }
    fn put(&self, path: &str, body: Value) -> Result<Value, Failure> {
        self.call(Method::Put, path, Some(body))
    }
    fn patch(&self, path: &str, body: Value) -> Result<Value, Failure> {
        self.call(Method::Patch, path, Some(body))
    }
    fn delete(&self, path: &str) -> Result<Value, Failure> {
        self.call(Method::Delete, path, None)
    }
}

fn shortcut_field(key: &str) -> Option<(&str, &str)> {
    key.strip_suffix("_keys")
        .map(|action| (action, "keys"))
        .or_else(|| key.strip_suffix("_btns").map(|action| (action, "buttons")))
}

fn led_range_field(key: &str) -> Option<(usize, &str)> {
    let (index, field) = key.strip_prefix("segment")?.split_once('_')?;
    let index = index.parse::<usize>().ok()?.checked_sub(1)?;
    (index < 4 && matches!(field, "start" | "end" | "type" | "color"))
        .then_some((index, if field == "type" { "kind" } else { field }))
}

fn led_segment_type(value: LegacyValue, native: bool) -> LegacyValue {
    let Some(value_text) = value.as_str() else { return value };
    for (typed, code) in [
        ("rpm", "R"),
        ("speed", "S"),
        ("flag", "F"),
        ("rpm_flag", "RF"),
        ("speed_flag", "SF"),
        ("spotter_right", "SR"),
        ("spotter_left", "SL"),
        ("spotter_right_flag", "SRF"),
        ("spotter_left_flag", "SLF"),
        ("off", "O"),
    ] {
        if value_text == if native { typed } else { code } {
            return LegacyValue::from(if native { code } else { typed });
        }
    }
    value
}

fn serial_setting_path<'a>(section: &str, key: &'a str) -> Option<(&'static str, &'a str)> {
    let key = match key {
        "auto_search" => key,
        "com_port" => "serial_port",
        _ => return None,
    };
    let section = match section {
        "INTELLIBOX" => "wind",
        "MOTIONBOX" => "motion",
        "HUD" => "hud",
        "BELT_TENSIONER" => "belt",
        "SIM3D" => "sim3d",
        "HAPTICR" => "hapticr",
        "IFLAG" => "iflag",
        "MOTION_COMPENSATION" => "compensation",
        _ => return None,
    };
    Some((section, key))
}

fn metadata_document<B: Backend + ?Sized>(backend: &B, table: &str) -> Result<Value, Failure> {
    match table {
        "joystick" => backend.call(Method::Get, "joystick/db", None),
        "shakers" => backend.call(Method::Get, "shakers/db", None),
        _ => Err(Failure { status: 500, detail: format!("unsupported metadata table {table:?}") }),
    }
}

pub(crate) fn app_field(key: &str) -> &str {
    match key {
        "pixel_ratio" => "device_pixel_ratio",
        "startup_page" => "start_page",
        "x_button_exit" => "close_button_quits",
        "shift_sound" => "gear_shift_sound",
        "measurement_unit" => "speed_unit",
        key => key,
    }
}

fn launcher_field(key: &str) -> Result<&'static str, Failure> {
    match key {
        "StartWithWindows" => Ok("start_with_login"),
        "ShowSplashScreen" => Ok("show_splash_screen"),
        _ => Err(Failure { status: 400, detail: format!("unknown launcher flag {key:?}") }),
    }
}

/// Percent-encodes one path segment (game keys, effect names).
pub fn segment(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// The value at `path` in a JSON document (`Null` when missing).
pub fn at<'a>(doc: &'a Value, path: &[&str]) -> &'a Value {
    let mut value = doc;
    for key in path {
        value = match value {
            Value::Object(map) => map.get(*key).unwrap_or(&Value::Null),
            Value::Array(items) => key.parse::<usize>().ok().and_then(|i| items.get(i)).unwrap_or(&Value::Null),
            _ => &Value::Null,
        };
    }
    value
}

/// Subscripting at a stored-value use site. Missing keys and invalid
/// parents are errors; a present null is returned for the consumer to interpret.
pub fn field<'a>(doc: &'a Value, path: &[&str]) -> Result<&'a Value, String> {
    let mut value = doc;
    for key in path {
        value = match value {
            Value::Object(map) => {
                map.get(*key).ok_or_else(|| format!("KeyError: {}", openrigcue_model::python_repr::repr_str(key)))?
            }
            Value::Array(_) => return Err("TypeError: list indices must be integers or slices, not str".into()),
            Value::String(_) => return Err("TypeError: string indices must be integers, not 'str'".into()),
            _ => return Err(format!("TypeError: '{}' object is not subscriptable", value_kind(value))),
        };
    }
    Ok(value)
}

/// Required string-key lookups without a JSON projection of stored values.
pub(crate) fn native_field<'a>(doc: &'a LegacyValue, path: &[&str]) -> Result<&'a LegacyValue, String> {
    let mut value = doc;
    for key in path {
        value = value.item(key).map_err(|detail| {
            let class = if value.as_dict().is_some() { "KeyError" } else { "TypeError" };
            format!("{class}: {detail}")
        })?;
    }
    Ok(value)
}

/// The text of an imported or stored value, encoded for the UI's UTF-8
/// `String`/output: an integer of more than 4300 digits (nested too) is refused with an error;
/// valid scalar text is preserved exactly; an unpaired surrogate is an encoding error at this
/// boundary (never shown escaped or as `U+FFFD`). Use this where a value is explicitly converted
/// to text that reaches a `String`; hex formatting and the `str_value` projection
/// are unaffected.
pub(crate) fn native_str(value: &LegacyValue) -> Result<String, String> {
    let text = openrigcue_model::python_repr::str_native(value)?;
    openrigcue_config::import::pickle::utf8_strict(&text).map_err(|exception| exception.to_string())
}

/// `native_field` for a possibly-live value: a profile tune or a setting section may be a `LegacyValue`
/// `Object` alias, so each step reads through `item_owned` (which retains child identity) and classifies the
/// error from the resolved parent. Use this for `native_tune`/`native_setting` results; JSON-derived status,
/// settings and controls are plain dictionaries and use `native_field`.
pub(crate) fn native_field_owned(doc: &LegacyValue, path: &[&str]) -> Result<LegacyValue, String> {
    let mut value = doc.clone();
    for key in path {
        let is_mapping = value.resolved().as_dict().is_some();
        value = value
            .item_owned(key)
            .map_err(|detail| format!("{}: {detail}", if is_mapping { "KeyError" } else { "TypeError" }))?;
    }
    Ok(value)
}

/// The game catalog has string keys. Other hashable value kinds miss without string conversion.
pub fn game_catalog_key(game: &LegacyValue) -> Result<&str, Failure> {
    fn unhashable(value: &LegacyValue) -> Option<&'static str> {
        match value {
            LegacyValue::List(_) => Some("list"),
            LegacyValue::Dict(_) => Some("dict"),
            LegacyValue::Set(_) => Some("set"),
            LegacyValue::Tuple(values) => values.iter().find_map(unhashable),
            _ => None,
        }
    }
    if let LegacyValue::Str(key) = game {
        return Ok(key);
    }
    let detail = if let Some(kind) = unhashable(game) {
        format!("TypeError: unhashable type: '{kind}'")
    } else {
        format!("KeyError: {}", openrigcue_model::python_repr::repr(game))
    };
    Err(Failure { status: 500, detail })
}

pub(crate) fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// The operand of numeric arithmetic: numbers and bools, without a string conversion.
pub fn python_number(value: &Value) -> Result<f64, String> {
    match value {
        Value::Number(n) => n.as_f64().ok_or_else(|| "OverflowError: number is too large".to_owned()),
        Value::Bool(b) => Ok(f64::from(u8::from(*b))),
        _ => Err(format!("TypeError: {} is not a number", value_kind(value))),
    }
}

/// Conversion to a float, including the numeric string grammar. Errors propagate to the caller's handler.
pub fn python_float(value: &Value) -> Result<f64, String> {
    if let Value::String(s) = value {
        return openrigcue_config::import::value::parse_py_float(s).ok_or_else(|| {
            format!("ValueError: could not convert string to float: {}", openrigcue_model::python_repr::repr_str(s))
        });
    }
    match value {
        Value::Number(_) | Value::Bool(_) => python_number(value),
        _ => Err(format!("TypeError: float() argument must be a string or a real number, not '{}'", value_kind(value))),
    }
}

/// Conversion to an integer, with the integer-string grammar (a float
/// string is rejected). The shared value representation currently supports i128 here.
pub fn python_int(value: &Value) -> Result<i128, String> {
    let value = match value {
        Value::Number(value) => {
            if let Some(value) = value.as_i64() {
                return Ok(i128::from(value));
            }
            if let Some(value) = value.as_u64() {
                return Ok(i128::from(value));
            }
            openrigcue_cueing::PyValue::Float(
                value.as_f64().ok_or_else(|| "OverflowError: integer is too large".to_owned())?,
            )
        }
        _ => openrigcue_cueing::PyValue::from(&LegacyValue::from(value.clone())),
    };
    value.to_int().map_err(conversion_error)
}

fn conversion_error(error: openrigcue_cueing::CueError) -> String {
    format!("{}: {}", error.exception().name(), error.message())
}

/// The truth of a value, used by widget enable/disable setters rather than the boolean text parse.
pub fn python_truth(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

/// The boolean text parse: bool as is, otherwise the exact accepted string spellings.
pub fn python_bool(value: &Value) -> Result<bool, String> {
    let converted = match value {
        Value::Bool(b) => return Ok(*b),
        Value::String(s) => openrigcue_config::import::value::str2bool(s),
        Value::Number(n) if !n.is_f64() => openrigcue_config::import::value::str2bool(&n.to_string()),
        _ => None,
    };
    converted.ok_or_else(|| match value {
        Value::String(s) => format!("Exception: Boolean value expected. Got {s}"),
        _ => format!("TypeError: can only concatenate str (not \"{}\") to str", value_kind(value)),
    })
}

pub(crate) fn native_bool(value: &LegacyValue) -> Result<bool, String> {
    let converted = match value {
        LegacyValue::Bool(value) => return Ok(*value),
        LegacyValue::Str(value) => openrigcue_config::import::value::str2bool(value),
        LegacyValue::Int(value) => openrigcue_config::import::value::str2bool(&value.to_string()),
        _ => None,
    };
    converted.ok_or_else(|| match value {
        LegacyValue::Str(value) => format!("Exception: Boolean value expected. Got {value}"),
        value => format!("TypeError: can only concatenate str (not \"{}\") to str", value.type_name()),
    })
}

/// A merge patch setting `path` to `value` (`{"a": {"b": value}}`).
pub fn patch_for(path: &[&str], value: Value) -> Value {
    path.iter().rev().fold(value, |inner, key| {
        let mut map = serde_json::Map::new();
        map.insert((*key).to_owned(), inner);
        Value::Object(map)
    })
}

/// Sets `path` in `doc` (creating objects on the way).
pub fn set_at(doc: &mut Value, path: &[&str], value: Value) {
    let mut target = doc;
    for key in &path[..path.len().saturating_sub(1)] {
        if !target.is_object() {
            *target = Value::Object(serde_json::Map::new());
        }
        target = target.as_object_mut().expect("made an object").entry((*key).to_owned()).or_insert(Value::Null);
    }
    if let Some(last) = path.last() {
        if !target.is_object() {
            *target = Value::Object(serde_json::Map::new());
        }
        target.as_object_mut().expect("made an object").insert((*last).to_owned(), value);
    }
}

/// The float of a stored number, string or bool.
pub fn num(value: &Value) -> f64 {
    match value {
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        Value::Bool(b) => f64::from(u8::from(*b)),
        Value::String(s) => s.trim().parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// The boolean text parse of a stored value.
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|v| v != 0.0),
        Value::String(s) => matches!(s.to_lowercase().as_str(), "yes" | "true" | "t" | "1" | "on"),
        _ => false,
    }
}

/// The text of a stored value (strings as they are, numbers as JSON prints them).
pub fn text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

pub fn native_int(value: &LegacyValue) -> Result<i64, String> {
    let value = native_int_wide(value)?;
    i64::try_from(value).map_err(|_| "OverflowError: integer exceeds supported range".into())
}

pub(crate) fn native_int_wide(value: &LegacyValue) -> Result<i128, String> {
    openrigcue_cueing::PyValue::from(value).to_int().map_err(conversion_error)
}

pub(crate) fn native_float(value: &LegacyValue) -> Result<f64, String> {
    openrigcue_cueing::PyValue::from(value).to_float().map_err(conversion_error)
}

/// The value truncated to an integer, as a float, for numeric widget arithmetic. Finite floats are not narrowed through i64.
pub(crate) fn native_int_as_float(value: &LegacyValue) -> Result<f64, String> {
    let number = match value {
        LegacyValue::Float(value) if value.is_finite() => value.trunc(),
        value => openrigcue_cueing::PyValue::from(value).to_int().map_err(conversion_error)? as f64,
    };
    Ok(if number == 0.0 { 0.0 } else { number })
}

/// The value parsed as a float and truncated, projected to float only for the following widget arithmetic.
pub(crate) fn native_int_float(value: &LegacyValue) -> Result<f64, String> {
    native_int_as_float(&LegacyValue::Float(native_float(value)?))
}

pub(crate) fn native_json(value: &LegacyValue) -> Result<Value, String> {
    openrigcue_config::import::de::from_legacy(value).map_err(|error| format!("TypeError: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `native_str` at the UTF-8 output boundary: exact scalar text; the error for an integer of more than
    /// 4300 digits; the encoding error for a lone surrogate (never shown escaped or as `U+FFFD`).
    #[test]
    fn native_str_encodes_strictly_at_the_output_boundary() {
        use openrigcue_model::native_value::{BigInt, LegacyValue, PythonString};
        assert_eq!(native_str(&LegacyValue::from("vehicle å")), Ok("vehicle å".to_owned()));
        assert_eq!(native_str(&LegacyValue::Int(-42)), Ok("-42".to_owned()));
        let under: BigInt = "1".repeat(4300).parse().unwrap();
        assert_eq!(native_str(&LegacyValue::BigInt(under)).unwrap(), "1".repeat(4300));
        let over: BigInt = "1".repeat(4301).parse().unwrap();
        let error = native_str(&LegacyValue::BigInt(over)).unwrap_err();
        assert!(error.starts_with("ValueError: Exceeds the limit (4300 digits)"), "{error}");
        let surrogate = LegacyValue::Unicode(PythonString::from_codepoints(vec![0x61, 0xDCFF, 0x62]).unwrap());
        assert_eq!(
            native_str(&surrogate).unwrap_err(),
            "UnicodeEncodeError: 'utf-8' codec can't encode character '\\udcff' in position 1: surrogates not allowed"
        );
    }

    #[test]
    fn json_paths() {
        let mut doc = json!({"a": {"b": [1, 2]}});
        assert_eq!(at(&doc, &["a", "b", "1"]), &json!(2));
        assert_eq!(at(&doc, &["x"]), &Value::Null);
        set_at(&mut doc, &["a", "c", "d"], json!(true));
        assert_eq!(doc["a"]["c"]["d"], json!(true));
        assert_eq!(patch_for(&["a", "b"], json!(3)), json!({"a": {"b": 3}}));
        assert_eq!(segment("Assetto Corsa/x"), "Assetto%20Corsa%2Fx");
        assert!(truthy(&json!("yes")) && !truthy(&json!("no")) && truthy(&json!(1)));
    }
}
