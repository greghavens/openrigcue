//! The motion inputs (configuration, tune and runtime values), built from the typed
//! settings: the cueing settings and tune, and every motion driver's settings.

use openrigcue_config::settings::Settings;
use openrigcue_config::tunes::{AxisSource, Tune};
use openrigcue_cueing::{
    Axis, ClipFactors, Effects, LowSpeedSmoothing, MainActuatorShares, Manufacturer, ManufacturerSettings,
    MotionSettings, MotionTune, PerAxis, RotationInput, SliderMap, TelemetryMaxima, TractionLossAutoSmoothing,
};
use openrigcue_devices::motion::prosimu::ProsimuSettings;
use openrigcue_devices::motion::pt_actuator::ptamotion::PtaMotionSettings;
use openrigcue_devices::motion::pt_actuator::ptcan::PtCanSettings;
use openrigcue_devices::motion::scn6::DyadicSettings;
use openrigcue_devices::motion::simtech::SimTechSettings;
use openrigcue_devices::motion::smc3::{NativeSmc3Startup, NativeSmc3Switches, Smc3Settings};
use openrigcue_devices::motion::thanos::{NativeSpikeFilters, ThanosSettings};
use openrigcue_devices::motion::vnm::VnmSettings;
use openrigcue_devices::motion::yaw_vr::YawVrSettings;
use openrigcue_model::{
    native_value::{Dict, LegacyValue},
    Telemetry,
};

/// The integer of a configuration number: truncation toward zero.
pub fn py_int(value: f64) -> i64 {
    value.trunc() as i64
}

/// The `[MOTIONBOX] *_clip_factor` values, read once at start-up.
pub fn clip_factors(settings: &Settings) -> ClipFactors {
    let m = &settings.motion;
    ClipFactors(PerAxis::from_fn(|axis| match axis {
        Axis::Pitch => m.pitch_clip_factor,
        Axis::Roll => m.roll_clip_factor,
        Axis::Yaw => m.yaw_clip_factor,
        Axis::Sway => m.sway_clip_factor,
        Axis::Surge => m.surge_clip_factor,
        Axis::Heave => m.heave_clip_factor,
        Axis::TractionLoss => m.traction_loss_clip_factor,
        Axis::TractionLossFront => m.traction_loss_front_clip_factor,
    }))
}

/// The `[MOTIONBOX]`, `[CUSTOM_FEATURES]` and manufacturer-section values the stream path reads per frame.
pub fn motion_settings(settings: &Settings, manufacturer: Manufacturer) -> MotionSettings {
    let m = &settings.motion;
    let f = &settings.features;
    MotionSettings {
        refresh_rate: i64::from(m.refresh_rate),
        soft_crash_factor: py_int(m.soft_crash_factor),
        soft_crash_enabled: m.soft_crash_enabled,
        boost_multiplier: m.boost_multiplier,
        sway_curving_factor: m.sway_curving_factor,
        surge_curving_factor: m.surge_curving_factor,
        low_speed_smoothing: LowSpeedSmoothing {
            enabled: m.auto_smoothing_low_speed,
            below_speed: m.auto_smoothing_speed,
            window: py_int(m.auto_smoothing_slider),
        },
        gear_smoothing_min_delta: py_int(m.gear_smoothing_min_delta),
        overall_smoothing_map: SliderMap {
            min: py_int(m.overall_smoothing_map_min_slider),
            max: py_int(m.overall_smoothing_map_max_slider),
        },
        legacy_gear_smoothing: f.legacy_gear_smoothing,
        traction_loss_auto_smoothing: TractionLossAutoSmoothing {
            mode: i64::from(f.dams_front_traction_loss_mode),
            max_window: py_int(f.dams_traction_loss_max_auto_smoothing),
            max_speed_kmh: f.dams_traction_loss_auto_smoothing_max_speed_kmh,
        },
        manufacturer: manufacturer_settings(settings, manufacturer),
    }
}

/// The manufacturer's section: smoothing, soft-crash and boost thresholds and the soft start/stop values.
fn manufacturer_settings(settings: &Settings, manufacturer: Manufacturer) -> ManufacturerSettings {
    let section = |smoothing: f64, crash: f64, boost: f64, factor: f64, timer: f64| ManufacturerSettings {
        smoothing_threshold: Some(smoothing),
        soft_crash_threshold: Some(crash),
        boost_threshold: Some(boost),
        soft_start_stop_factor: Some(py_int(factor)),
        soft_start_stop_timer: Some(py_int(timer)),
    };
    match manufacturer {
        Manufacturer::None => ManufacturerSettings::for_manufacturer(Manufacturer::None),
        Manufacturer::DofReality => {
            let s = &settings.dof_reality;
            section(
                s.smoothing_threshold,
                s.soft_crash_threshold,
                s.boost_threshold,
                s.soft_start_stop_factor,
                s.soft_start_stop_timer,
            )
        }
        Manufacturer::YawVr => {
            let s = &settings.yaw_vr;
            section(
                s.smoothing_threshold,
                s.soft_crash_threshold,
                s.boost_threshold,
                s.soft_start_stop_factor,
                s.soft_start_stop_timer,
            )
        }
        Manufacturer::Prosimu => {
            let s = &settings.prosimu;
            section(
                s.smoothing_threshold,
                s.soft_crash_threshold,
                s.boost_threshold,
                s.soft_start_stop_factor,
                s.soft_start_stop_timer,
            )
        }
        Manufacturer::PtActuator => {
            let s = &settings.pt_actuator;
            section(
                s.smoothing_threshold,
                s.soft_crash_threshold,
                s.boost_threshold,
                s.soft_start_stop_factor,
                s.soft_start_stop_timer,
            )
        }
        Manufacturer::Thanos => {
            let s = &settings.thanos;
            section(
                s.smoothing_threshold,
                s.soft_crash_threshold,
                s.boost_threshold,
                s.soft_start_stop_factor,
                s.soft_start_stop_timer,
            )
        }
        Manufacturer::VnmSimulation => {
            let s = &settings.vnm;
            section(
                s.smoothing_threshold,
                s.soft_crash_threshold,
                s.boost_threshold,
                s.soft_start_stop_factor,
                s.soft_start_stop_timer,
            )
        }
        Manufacturer::SimTechRevolution => {
            let s = &settings.simtech;
            section(
                s.smoothing_threshold,
                s.soft_crash_threshold,
                s.boost_threshold,
                s.soft_start_stop_factor,
                s.soft_start_stop_timer,
            )
        }
    }
}

/// The motion keys of the active tune.
pub fn motion_tune(tune: &Tune) -> MotionTune {
    let e = &tune.motion_effects;
    let s = &tune.motion_switch;
    let r = &tune.max_telemetry_vars;
    let smoothing = &tune.motion_effect_smoothing_factor;
    MotionTune {
        effects: Effects {
            pitch: e.pitch,
            roll: e.roll,
            yaw: e.yaw,
            sway: e.sway,
            surge_acceleration: e.surge_acceleration,
            surge_deceleration: e.surge_deceleration,
            heave: e.heave,
            traction_loss: e.traction_loss,
            traction_loss_front: e.traction_loss_front,
            wheel_slip: e.wheel_slip,
            rumble_strips: e.rumble_strips,
            afterburner: e.afterburner,
            cannon_firing: e.cannon_firing,
            landing_gear: e.landing_gear,
            missile_firing: e.missile_firing,
            canopy: e.canopy,
            engine: e.engine,
            abs_lock: e.abs_lock,
        },
        switches: Effects {
            pitch: s.pitch,
            roll: s.roll,
            yaw: s.yaw,
            sway: s.sway,
            surge_acceleration: s.surge_acceleration,
            surge_deceleration: s.surge_deceleration,
            heave: s.heave,
            traction_loss: s.traction_loss,
            traction_loss_front: s.traction_loss_front,
            wheel_slip: s.wheel_slip,
            rumble_strips: s.rumble_strips,
            afterburner: s.afterburner,
            cannon_firing: s.cannon_firing,
            landing_gear: s.landing_gear,
            missile_firing: s.missile_firing,
            canopy: s.canopy,
            engine: s.engine,
            abs_lock: s.abs_lock,
        },
        pitch_input: rotation_input(e.pitch_type),
        roll_input: rotation_input(e.roll_type),
        maxima: TelemetryMaxima {
            pitch: r.pitch,
            roll: r.roll,
            yaw: r.yaw,
            sway: r.sway,
            heave: r.heave,
            traction_loss: r.traction_loss,
            traction_loss_front: r.traction_loss_front,
            surge_acceleration: r.surge_acceleration,
            surge_deceleration: r.surge_deceleration,
            pitch_rate: r.pitch_rate,
            roll_rate: r.roll_rate,
        },
        smoothing_factor: tune.motion_smoothing_factor,
        boost_factor: tune.motion_boost_factor,
        effect_smoothing: PerAxis::from_fn(|axis| match axis {
            Axis::Pitch => smoothing.pitch,
            Axis::Roll => smoothing.roll,
            Axis::Yaw => smoothing.yaw,
            Axis::Sway => smoothing.sway,
            Axis::Surge => smoothing.surge,
            Axis::Heave => smoothing.heave,
            Axis::TractionLoss => smoothing.traction_loss,
            Axis::TractionLossFront => smoothing.traction_loss_front,
        }),
        max_power: tune.motion_max_power,
        max_horizontal: tune.motion_max_horizontal,
        min_traction_loss_activation: tune.min_traction_loss_activation,
        min_traction_loss_front_activation: tune.min_traction_loss_front_activation,
        enhanced_gforces: tune.motion_enhanced_gforces != 0,
        belt_tensioner: tune.motion_belt_tensioner,
        main_actuator_shares: MainActuatorShares {
            sway: tune.motion_main_actuators_sway,
            surge: tune.motion_main_actuators_surge,
            traction_loss: tune.motion_main_actuators_traction_loss,
            traction_loss_front: tune.motion_main_actuators_traction_loss_front,
        },
        gear_smoothing_factor: py_int(tune.gear_smoothing_factor),
    }
}

/// `motion_effects.pitch_type` / `roll_type`: missing or `angle` reads the angle, anything else the rate.
fn rotation_input(source: Option<AxisSource>) -> RotationInput {
    match source {
        None | Some(AxisSource::Angle) => RotationInput::Angle,
        Some(AxisSource::Rate) => RotationInput::Rate,
    }
}

/// Runtime values some drivers read on every call (the runtime state and the telemetry).
#[derive(Debug, Clone)]
pub struct DriverRuntime<'a> {
    /// The running game (`'none'` when idle).
    pub game_running: &'a str,
    /// The running listener (`'none'` when idle).
    pub listener_running: &'a str,
    /// Whether the full-rate motion test runs.
    pub motion_test: bool,
    /// Whether the motion switch is on.
    pub motion_switch: bool,
    /// Whether a motion error is set.
    pub motion_error: bool,
    pub telemetry: &'a Telemetry,
    /// The tune fields the SimTech driver reads, as stored, or the tune's malformed root value.
    pub simtech_tune: LegacyValue,
}

/// `[DOF REALITY]` and `[MOTIONBOX]` values of the SMC3 driver, under the Windows rules:
/// no pre-connection reset, ports opened exclusively.
pub fn smc3_settings(settings: &Settings) -> Smc3Settings {
    smc3_settings_with_native(settings, &settings.native_snapshot())
}

pub(super) fn smc3_settings_with_native(settings: &Settings, native: &Dict) -> Smc3Settings {
    use openrigcue_model::native_value::LegacyValue;
    let d = &settings.dof_reality;
    let resolution =
        |key: &str| match native.get("DOF REALITY").and_then(LegacyValue::as_dict).and_then(|section| section.get(key))
        {
            Some(LegacyValue::Str(text)) => text.clone(),
            Some(LegacyValue::Bytes(bytes)) if bytes.is_ascii() => String::from_utf8(bytes.clone()).expect("ASCII"),
            value => value.and_then(LegacyValue::py_int).map(|value| value.to_string()).unwrap_or_default(),
        };
    Smc3Settings {
        runtime: Default::default(),
        native_startup: Some(native_smc3_startup(native, settings.motion.reaction_speed)),
        native_reaction_speed: Some(native_setting_value(native, "MOTIONBOX", "reaction_speed").cloned()),
        native_switches: Some(NativeSmc3Switches {
            enable_debug: native_setting_boolean(native, "MOTIONBOX", "enable_debug"),
            update_kp: native_setting_boolean(native, "DOF REALITY", "update_kp"),
        }),
        enable_debug: settings.motion.debug,
        reaction_speed: settings.motion.reaction_speed,
        update_kp: d.update_kp,
        left_controller_com_port: d.left_serial_port.clone(),
        right_controller_com_port: d.right_serial_port.clone(),
        left_controller_resolution: resolution("left_controller_resolution"),
        right_controller_resolution: resolution("right_controller_resolution"),
        left_debug_port: d.left_debug_port,
        right_debug_port: d.right_debug_port,
        pre_connection_reset: false,
        exclusive_open: true,
    }
}

/// The SMC3 driver's start-up values. The second argument (the typed reaction speed)
/// is unused; every value is converted as an integer of any width.
pub(super) fn native_smc3_startup(config: &Dict, _typed_reaction: i64) -> Result<NativeSmc3Startup, String> {
    let frame = |line, error| {
        format!(
        "Traceback (most recent call last):\n  File \"backend/controllers/motion_helpers.py\", line 16, in init backend.controllers.motion_helpers\n  File \"backend/controllers/smc3.py\", line {line}, in init backend.controllers.smc3\n{error}\n"
    )
    };
    // The `DOF REALITY` port as an integer of any width; a port outside 0..=65535 is a valid integer whose
    // later send fails where it is used.
    let port = |key| -> Result<Option<u16>, String> {
        let value = native_setting_value(config, "DOF REALITY", key)?;
        let port = openrigcue_devices::motion::native::to_bigint(value).map_err(|e| e.to_string())?;
        Ok(u16::try_from(port).ok())
    };
    let left = port("left_debug_port").map_err(|error| frame(26, error))?;
    let right = port("right_debug_port").map_err(|error| frame(27, error))?;
    // The previous reaction speed starts as `MOTIONBOX.reaction_speed` + 20, at any width.
    let reaction = native_setting_value(config, "MOTIONBOX", "reaction_speed")
        .and_then(|raw| openrigcue_devices::motion::native::to_bigint(raw).map_err(|e| e.to_string()))
        .map_err(|error| frame(785, error))?;
    Ok(NativeSmc3Startup { debug_ports: [left, right], previous_kp: reaction + 20 })
}

/// `[MOTION_COMPENSATION] vertical_actuators_stroke` as an integer.
fn compensation_stroke(settings: &Settings) -> i64 {
    py_int(settings.compensation.actuator_vertical_stroke)
}

/// The values the PROSIMU PRS200 drivers read.
pub fn prosimu_settings(settings: &Settings) -> ProsimuSettings {
    prosimu_settings_with_native(settings, &settings.native_snapshot())
}

pub(super) fn prosimu_settings_with_native(settings: &Settings, native: &Dict) -> ProsimuSettings {
    ProsimuSettings {
        runtime: Default::default(),
        model: native_setting_value(native, "MOTIONBOX", "model").cloned(),
        port1: native_setting_value(native, "PROSIMU", "prs200_com_port1").cloned(),
        port2: native_setting_value(native, "PROSIMU", "prs200_com_port2").cloned(),
        firmware: native_setting_value(native, "PROSIMU", "prs200_firmware").cloned(),
        enable_dyadic_tl: native_setting_boolean(native, "PROSIMU", "enable_dyatic_tl"),
        invert_tl: native_setting_boolean(native, "PROSIMU", "invert_tl"),
        reaction_speed: native_setting_value(native, "MOTIONBOX", "reaction_speed").cloned(),
        vertical_stroke_mm: native_setting_value(native, "MOTION_COMPENSATION", "vertical_actuators_stroke").cloned(),
        axis_assignment: settings.motion.motor_axes.clone(),
    }
}

/// The values the SCN6 Dyadic drivers read. Required first-frame lookups/conversions retain their errors
/// until the driver reaches that read; migrations do not create these two keys.
pub fn dyadic_settings(settings: &Settings) -> DyadicSettings {
    dyadic_settings_with_native(settings, &settings.native_snapshot())
}

pub(super) fn dyadic_settings_with_native(settings: &Settings, native: &Dict) -> DyadicSettings {
    let p = &settings.prosimu;
    DyadicSettings {
        runtime: Default::default(),
        invert_tl: p.invert_traction_loss,
        reaction_speed: settings.motion.reaction_speed,
        native_reaction_speed: Some(native_setting_value(native, "MOTIONBOX", "reaction_speed").cloned()),
        post_homing_speed: i64::from(p.post_homing_speed),
        post_homing_speed_timeout: py_int(p.post_homing_speed_timeout),
        simtech_scn6_speed: native_setting_value(native, "SIMTECHREVOLUTION", "scn6_speed").cloned(),
        simtech_scn6_acceleration: native_setting_value(native, "SIMTECHREVOLUTION", "scn6_acceleration").cloned(),
        axis_assignment: settings.motion.motor_axes.clone(),
    }
}

fn native_field<'a>(document: &'a LegacyValue, key: &str) -> Result<&'a LegacyValue, String> {
    document.item(key).map_err(|detail| {
        let class = if document.as_dict().is_some() { "KeyError" } else { "TypeError" };
        format!("{class}: {detail}")
    })
}

fn native_setting_section<'a>(config: &'a Dict, section: &str) -> Result<&'a LegacyValue, String> {
    config.get(section).ok_or_else(|| format!("KeyError: {}", openrigcue_model::python_repr::repr_str(section)))
}

fn native_setting_value<'a>(config: &'a Dict, section: &str, key: &str) -> Result<&'a LegacyValue, String> {
    native_field(native_setting_section(config, section)?, key)
}

fn native_setting_boolean(config: &Dict, section: &str, key: &str) -> Result<bool, String> {
    let value = native_setting_value(config, section, key)?;
    openrigcue_cueing::str2bool(&openrigcue_cueing::PyValue::from(value))
        .map_err(|error| format!("{}: {}", error.exception().name(), error.message()))
}

/// The values the Thanos driver reads from its manufacturer's section (THANOS, or PT-ACTUATOR for the PT models
/// it drives), `MOTION_COMPENSATION`, the tune and the runtime state.
pub fn thanos_settings(
    settings: &Settings,
    manufacturer: &str,
    tune: &Tune,
    runtime: &DriverRuntime<'_>,
) -> ThanosSettings {
    let (com_port, spike_filter, reverse) = if manufacturer == "PT-ACTUATOR" {
        let p = &settings.pt_actuator;
        (
            p.thanos_serial_port.clone(),
            p.spike_filter,
            [
                p.reverse_controller_port_5,
                p.reverse_controller_port_6,
                p.reverse_controller_port_7,
                p.reverse_controller_port_8,
            ],
        )
    } else {
        let t = &settings.thanos;
        (
            t.serial_port.clone(),
            t.spike_filter,
            [
                t.reverse_controller_port_5,
                t.reverse_controller_port_6,
                t.reverse_controller_port_7,
                t.reverse_controller_port_8,
            ],
        )
    };
    let native = settings.native_snapshot();
    ThanosSettings {
        runtime: Default::default(),
        section: if manufacturer == "PT-ACTUATOR" { "PT-ACTUATOR" } else { "THANOS" }.into(),
        com_port,
        send_spike_filter: spike_filter,
        native_send_spike_filter: Some(native_thanos_filter(&native, manufacturer)),
        reverse_ports: reverse,
        native_reverse_ports: Some(std::array::from_fn(|index| {
            let section = if manufacturer == "PT-ACTUATOR" { "PT-ACTUATOR" } else { "THANOS" };
            native_setting_boolean(&native, section, &format!("reverse_controller_port_{}", index + 5))
        })),
        compensation_stroke_mm: compensation_stroke(settings),
        native_compensation_stroke_mm: Some(
            native_setting_value(&native, "MOTION_COMPENSATION", "vertical_actuators_stroke").cloned(),
        ),
        spike_filter_horizontal: tune.thanos_spike_filter_horizontal,
        spike_filter_vertical: tune.thanos_spike_filter_vertical,
        native_spike_filters: None,
        test_mode: runtime.motion_test,
        no_game_running: runtime.game_running == "none",
    }
}

pub(super) fn thanos_settings_with_native_profile(
    settings: &Settings,
    manufacturer: &str,
    tune: &Tune,
    runtime: &DriverRuntime<'_>,
    profile: &LegacyValue,
) -> ThanosSettings {
    let mut converted = thanos_settings(settings, manufacturer, tune, runtime);
    let values = (|| {
        Ok([
            native_field(profile, "thanos_spike_filter_horizontal")?.clone(),
            native_field(profile, "thanos_spike_filter_vertical")?.clone(),
        ])
    })();
    converted.native_spike_filters = Some(NativeSpikeFilters { values });
    converted
}

/// One section of the live settings tree, resolved one level, as the one-section dictionary the lookups
/// above take (a missing section is an error here).
fn live_section(tree: &LegacyValue, section: &str) -> Result<Dict, String> {
    let fields = tree.lookup_owned(&LegacyValue::from(section))?.resolved();
    let mut config = Dict::new();
    config.insert(section, fields);
    Ok(config)
}

/// One setting read from the live tree now.
pub(super) fn live_setting_value(tree: &LegacyValue, section: &str, key: &str) -> Result<LegacyValue, String> {
    native_setting_value(&live_section(tree, section)?, section, key).cloned()
}

/// One boolean setting read from the live tree now.
pub(super) fn live_setting_boolean(tree: &LegacyValue, section: &str, key: &str) -> Result<bool, String> {
    native_setting_boolean(&live_section(tree, section)?, section, key)
}

/// The Thanos/Actuon spike-filter switch of `section` read from the live tree now.
pub(super) fn live_thanos_filter(tree: &LegacyValue, section: &str) -> Result<bool, String> {
    native_thanos_filter(&live_section(tree, section)?, section)
}

fn native_thanos_filter(config: &Dict, manufacturer: &str) -> Result<bool, String> {
    let section = if manufacturer == "PT-ACTUATOR" { "PT-ACTUATOR" } else { "THANOS" };
    let fields = native_setting_section(config, section)?;
    let fallback = LegacyValue::from("yes");
    let selected = match fields {
        LegacyValue::Dict(fields) => {
            fields.get("filter").or_else(|| fields.get("send_spike_filter")).unwrap_or(&fallback)
        }
        LegacyValue::Bytes(_) => return Err("TypeError: a bytes-like object is required, not 'str'".into()),
        LegacyValue::Str(value) => {
            if value.contains("filter") {
                return Err("TypeError: string indices must be integers, not 'str'".into());
            }
            return Err("AttributeError: 'str' object has no attribute 'get'".into());
        }
        LegacyValue::List(values) | LegacyValue::Tuple(values) | LegacyValue::Set(values) => {
            if values.iter().any(|value| matches!(value, LegacyValue::Str(value) if value == "filter")) {
                return Err(format!("TypeError: {}", fields.item("filter").unwrap_err()));
            }
            return Err(format!("AttributeError: '{}' object has no attribute 'get'", fields.type_name()));
        }
        value => return Err(format!("TypeError: argument of type '{}' is not iterable", value.type_name())),
    };
    openrigcue_cueing::str2bool(&openrigcue_cueing::PyValue::from(selected))
        .map_err(|error| format!("{}: {}", error.exception().name(), error.message()))
}

/// The values the Yaw VR driver reads.
pub fn yaw_vr_settings(settings: &Settings, tune: &Tune, runtime: &DriverRuntime<'_>) -> YawVrSettings {
    yaw_vr_settings_with_native(settings, &settings.native_snapshot(), tune, runtime)
}

pub(super) fn yaw_vr_settings_with_native(
    settings: &Settings,
    native: &Dict,
    tune: &Tune,
    runtime: &DriverRuntime<'_>,
) -> YawVrSettings {
    let y = &settings.yaw_vr;
    let t = runtime.telemetry;
    let yaw = native
        .get("MOTION_COMPENSATION")
        .and_then(LegacyValue::as_dict)
        .and_then(|section| section.get("yaw"))
        .and_then(LegacyValue::as_dict);
    let native_compensation = openrigcue_devices::motion::yaw_vr::NativeYawCompensation {
        reverse: yaw.and_then(|yaw| yaw.get("reverse")).map(LegacyValue::is_truthy),
        offset: yaw
            .and_then(|yaw| yaw.get("offset"))
            .and_then(|value| openrigcue_cueing::PyValue::from(value).to_float().ok()),
    };
    YawVrSettings {
        runtime: Default::default(),
        native: Some(openrigcue_devices::motion::yaw_vr::NativeYawConfig {
            udp_port: native_setting_value(native, "Yaw VR", "udp_port").cloned(),
            device_id: native_setting_value(native, "Yaw VR", "device_id").cloned(),
            auto_park: native_setting_boolean(native, "Yaw VR", "auto_park"),
            center_before_start_alert: native_setting_boolean(native, "Yaw VR", "center_before_start_alert"),
            vibration_power: native_setting_value(native, "Yaw VR", "vibration_power").cloned(),
            min_degree_null_yaw_movement: native_setting_value(native, "MOTIONBOX", "min_degree_null_yaw_movement")
                .cloned(),
        }),
        udp_port: i64::from(y.udp_port),
        device_id: y.device_id.clone(),
        auto_park: y.auto_park,
        center_before_start_alert: y.center_before_start_alert,
        disable_two_way_communication: y.disable_two_way_communication,
        sixdof_mode: y.six_dof_mode,
        vibration_power: Some(i64::from(y.vibration_power)),
        min_degree_null_yaw_movement: py_int(settings.motion.min_null_yaw_movement_degrees),
        compensation_yaw_offset: settings.compensation.yaw.offset,
        compensation_yaw_reverse: settings.compensation.yaw.reverse,
        native_compensation: Some(native_compensation),
        yaw_motion_switch: i64::from(tune.motion_switch.yaw),
        game_running: runtime.game_running.to_owned(),
        listener_running: runtime.listener_running.to_owned(),
        test_mode: runtime.motion_test,
        yaw_angle: t.yaw_angle,
        speed: t.speed,
        max_speed: t.max_speed,
        rpm: t.rpm,
        max_rpm: t.max_rpm,
    }
}

/// The values the PT-Actuator CANBus driver reads.
pub fn pt_can_settings(settings: &Settings, runtime: &DriverRuntime<'_>) -> PtCanSettings {
    let p = &settings.pt_actuator;
    let native = settings.native_snapshot();
    // Each read in the driver converts the raw value to an integer itself.
    let raw = |section: &str, key: &str| native_setting_value(&native, section, key).cloned();
    PtCanSettings {
        runtime: Default::default(),
        pt_com_port: p.canbus_serial_port.clone(),
        baud_rate: raw("PT-ACTUATOR", "pt_com_baud_rate"),
        parking_speed: raw("PT-ACTUATOR", "parking_speed"),
        reaction_speed_map_min: raw("PT-ACTUATOR", "reaction_speed_map_min"),
        reaction_speed_map_max: raw("PT-ACTUATOR", "reaction_speed_map_max"),
        park_to_motion_speed_delay_s: raw("PT-ACTUATOR", "park_to_motion_speed_delay_s"),
        reverse_ports: [p.reverse_controller_port_5, p.reverse_controller_port_6, p.reverse_controller_port_7],
        reaction_speed: raw("MOTIONBOX", "reaction_speed"),
        compensation_stroke_mm: raw("MOTION_COMPENSATION", "vertical_actuators_stroke"),
        vehicle_speed: runtime.telemetry.speed,
        motion_test_mode: i64::from(runtime.motion_test),
        no_game_running: runtime.game_running == "none",
    }
}

/// The values the PT-Actuator UDP driver reads.
pub fn pta_motion_settings(settings: &Settings) -> PtaMotionSettings {
    PtaMotionSettings {
        runtime: Default::default(),
        udp_config: settings.pt_actuator.udp_targets.clone(),
        refresh_rate: i64::from(settings.motion.refresh_rate),
        native_refresh_rate: Some(
            native_setting_value(&settings.native_snapshot(), "MOTIONBOX", "refresh_rate").cloned(),
        ),
    }
}

/// The values the VNM driver reads.
pub fn vnm_settings(settings: &Settings) -> VnmSettings {
    let v = &settings.vnm;
    let native = settings.native_snapshot();
    VnmSettings {
        runtime: Default::default(),
        port: v.serial_port.clone(),
        reverse_ports: [
            v.reverse_controller_port_5,
            v.reverse_controller_port_6,
            v.reverse_controller_port_7,
            v.reverse_controller_port_8,
            v.reverse_controller_port_9,
        ],
        native_reverse_ports: Some(std::array::from_fn(|index| {
            native_setting_boolean(&native, "VNM SIMULATION", &format!("reverse_controller_port_{}", index + 5))
        })),
        vertical_stroke_mm: compensation_stroke(settings),
        native_vertical_stroke_mm: Some(
            native_setting_value(&native, "MOTION_COMPENSATION", "vertical_actuators_stroke").cloned(),
        ),
    }
}

/// The values the SimTech Revolution driver reads.
pub fn simtech_settings(settings: &Settings, runtime: &DriverRuntime<'_>) -> SimTechSettings {
    simtech_settings_with_native(settings, &settings.native_snapshot(), runtime)
}

pub(super) fn simtech_settings_with_native(
    settings: &Settings,
    native: &Dict,
    runtime: &DriverRuntime<'_>,
) -> SimTechSettings {
    // Each raw value is converted to an integer by the driver read that uses it.
    let setting = |section: &str, key: &str| native_setting_value(native, section, key).cloned();
    SimTechSettings {
        runtime: Default::default(),
        motion_switch_on: runtime.motion_switch,
        motion_error: runtime.motion_error,
        post_homing_speed: setting("SIMTECHREVOLUTION", "post_homing_speed"),
        post_homing_acceleration: setting("SIMTECHREVOLUTION", "post_homing_acceleration"),
        post_homing_speed_timeout: setting("SIMTECHREVOLUTION", "post_homing_speed_timeout"),
        scn6_speed: native_field(&runtime.simtech_tune, "scn6_speed").cloned(),
        scn6_acceleration: native_field(&runtime.simtech_tune, "scn6_acceleration").cloned(),
        axis_assignment: settings.motion.motor_axes.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        refresh_rate: setting("MOTIONBOX", "refresh_rate"),
        // The Windows rules apply.
        windows: true,
    }
}

#[cfg(test)]
mod tests {
    /// The integer of a raw setting as the driver converts it (the tests' values fit an i128).
    fn int_setting(setting: openrigcue_devices::motion::native::NativeSetting) -> Result<i128, String> {
        setting
            .and_then(|raw| openrigcue_devices::motion::native::to_bigint(&raw).map_err(|e| e.to_string()))
            .map(|value| value.to_string().parse().expect("test values fit an i128"))
    }

    #[test]
    fn native_yaw_feedback_missing_and_bad_offset_are_local_zero_with_live_refresh() {
        use super::*;
        use crate::motion::drivers::DriverSettings;
        use openrigcue_devices::motion::yaw_vr::net;
        use openrigcue_model::motion_sensor::MotionSensorValues;
        let mut settings = Settings::default();
        settings.yaw_vr.six_dof_mode = true;
        let telemetry = Telemetry::default();
        let runtime = DriverRuntime {
            game_running: "none",
            listener_running: "none",
            motion_test: false,
            motion_switch: false,
            motion_error: false,
            telemetry: &telemetry,
            simtech_tune: LegacyValue::Dict(Dict::new()),
        };
        let tune = openrigcue_config::tunes::FactoryTunes::builtin().get("emulator").unwrap();
        let drivers = DriverSettings::default();
        let sensor = MotionSensorValues::default();
        drivers.yaw_shared.update(|state| state.share_motion_sensor(sensor.clone()));
        for bad in [LegacyValue::from("bad"), LegacyValue::None, LegacyValue::List(vec![])] {
            settings.edit_native(|root| {
                root.get_mut("MOTION_COMPENSATION")
                    .unwrap()
                    .as_dict_mut()
                    .unwrap()
                    .get_mut("yaw")
                    .unwrap()
                    .as_dict_mut()
                    .unwrap()
                    .insert("offset", bad.clone());
            });
            drivers.refresh(&settings, "Yaw VR", tune, &runtime, &LegacyValue::from(tune.to_json()));
            let record =
                net::handle_feedback(b"SY[20]SP[1]SR[2]U[90]", &drivers.yaw_vr.get(), &drivers.yaw_shared).unwrap();
            assert!(record.is_some());
            assert_eq!(sensor.get("calculated_yaw"), Some(LegacyValue::Int(0)));
            assert_eq!(sensor.get("raw_yaw"), Some(LegacyValue::Int(0)));
        }
        for key in ["reverse", "offset"] {
            let mut missing = Settings::default();
            missing.edit_native(|root| {
                root.get_mut("MOTION_COMPENSATION")
                    .unwrap()
                    .as_dict_mut()
                    .unwrap()
                    .get_mut("yaw")
                    .unwrap()
                    .as_dict_mut()
                    .unwrap()
                    .remove(key);
            });
            drivers.refresh(&missing, "Yaw VR", tune, &runtime, &LegacyValue::from(tune.to_json()));
            net::handle_feedback(b"SY[20]SP[1]SR[2]U[90]", &drivers.yaw_vr.get(), &drivers.yaw_shared).unwrap();
            assert!(drivers.yaw_shared.get().compensation_zeroed);
        }
        settings.edit_native(|root| {
            let fields = root
                .get_mut("MOTION_COMPENSATION")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .get_mut("yaw")
                .unwrap()
                .as_dict_mut()
                .unwrap();
            fields.insert("offset", LegacyValue::from("10.5"));
            fields.insert("reverse", LegacyValue::Bool(false));
        });
        drivers.refresh(&settings, "Yaw VR", tune, &runtime, &LegacyValue::from(tune.to_json()));
        net::handle_feedback(b"SY[20]SP[1]SR[2]U[90]", &drivers.yaw_vr.get(), &drivers.yaw_shared).unwrap();
        assert_eq!(sensor.get("calculated_yaw"), Some(LegacyValue::Float(30.5)));
        assert_eq!(sensor.get("raw_yaw"), Some(LegacyValue::Float(20.0)));
        assert!(!drivers.yaw_shared.get().compensation_zeroed);
    }

    #[test]
    fn native_yaw_feedback_reverse_preserves_builtin_truthiness() {
        use super::*;
        let telemetry = Telemetry::default();
        let runtime = DriverRuntime {
            game_running: "none",
            listener_running: "none",
            motion_test: false,
            motion_switch: false,
            motion_error: false,
            telemetry: &telemetry,
            simtech_tune: LegacyValue::Dict(Dict::new()),
        };
        let tune = openrigcue_config::tunes::FactoryTunes::builtin().get("emulator").unwrap();
        for (value, expected) in [
            (LegacyValue::from("no"), true),
            (LegacyValue::from("False"), true),
            (LegacyValue::from(""), false),
            (LegacyValue::Bool(false), false),
            (LegacyValue::None, false),
            (LegacyValue::Int(0), false),
            (LegacyValue::List(vec![1.into()]), true),
        ] {
            let mut settings = Settings::default();
            settings.edit_native(|root| {
                root.get_mut("MOTION_COMPENSATION")
                    .unwrap()
                    .as_dict_mut()
                    .unwrap()
                    .get_mut("yaw")
                    .unwrap()
                    .as_dict_mut()
                    .unwrap()
                    .insert("reverse", value.clone());
            });
            assert_eq!(
                yaw_vr_settings(&settings, tune, &runtime).native_compensation.unwrap().reverse,
                Some(expected),
                "{value:?}"
            );
        }
    }

    #[test]
    fn native_yaw_feedback_saved_false_is_raw_truthy_until_the_switch_writes_a_bool() {
        use super::*;
        use openrigcue_devices::motion::yaw_vr::{net, YawShared};
        use openrigcue_devices::motion::LiveSettings;
        let raw =
            openrigcue_config::import::configobj::parse("[MOTION_COMPENSATION]\n[[yaw]]\noffset = 10\nreverse = no\n")
                .unwrap();
        let settings = Settings::from_legacy(&raw).0;
        let telemetry = Telemetry::default();
        let runtime = DriverRuntime {
            game_running: "none",
            listener_running: "none",
            motion_test: false,
            motion_switch: false,
            motion_error: false,
            telemetry: &telemetry,
            simtech_tune: LegacyValue::Dict(Dict::new()),
        };
        let tune = openrigcue_config::tunes::FactoryTunes::builtin().get("emulator").unwrap().clone();
        let shared = LiveSettings::new(YawShared::default());
        let converted = yaw_vr_settings(&settings, &tune, &runtime);
        net::handle_feedback(b"SY[20]SP[1]SR[2]U[90]", &converted, &shared).unwrap();
        assert_eq!(shared.get().calculated_yaw, -30.0, "saved 'no' is nonempty in original raw truth test");
        let mut settings = settings;
        settings.edit_native(|root| {
            root.get_mut("MOTION_COMPENSATION")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .get_mut("yaw")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .insert("reverse", LegacyValue::Bool(false));
        });
        net::handle_feedback(b"SY[20]SP[1]SR[2]U[90]", &yaw_vr_settings(&settings, &tune, &runtime), &shared).unwrap();
        assert_eq!(shared.get().calculated_yaw, 30.0);
    }

    #[test]
    fn native_prs200_startup_model_is_read_when_the_queued_worker_runs() {
        use super::*;
        use crate::motion::testing::OfflineHost;
        use openrigcue_devices::discovery::PortInfo;
        use openrigcue_devices::motion::driver_support::HostActivity;
        use openrigcue_devices::motion::prosimu::{
            board::{speed_frame, BoardPorts, SpeedLayout},
            Prs200Driver, StartupJob,
        };
        use openrigcue_devices::motion::{motion_model, LinkState, LiveSettings, MotionContext, MotionDriver};
        use openrigcue_devices::serial::{Link, LinkError};
        use std::sync::{Arc, Mutex};
        struct RecordingLink {
            written: Arc<Mutex<Vec<Vec<u8>>>>,
        }
        impl Link for RecordingLink {
            fn write_all(&mut self, bytes: &[u8]) -> Result<(), LinkError> {
                self.written.lock().unwrap().push(bytes.to_vec());
                Ok(())
            }
            fn read_available(&mut self) -> Result<Vec<u8>, LinkError> {
                Ok(Vec::new())
            }
            fn clear_input(&mut self) -> Result<(), LinkError> {
                Ok(())
            }
            fn name(&self) -> &str {
                "/dev/openrigcue-fixture"
            }
        }
        struct RecordingPorts {
            written: Arc<Mutex<Vec<Vec<u8>>>>,
        }
        impl BoardPorts for RecordingPorts {
            fn open(&mut self, _: &str) -> Result<Box<dyn Link>, LinkError> {
                Ok(Box::new(RecordingLink { written: self.written.clone() }))
            }
        }
        let mut settings = Settings::default();
        settings.motion.model = "T1000-2M SV".into();
        settings.prosimu.prs200_serial_port_1 = "/dev/openrigcue-fixture".into();
        let live = LiveSettings::new(prosimu_settings(&settings));
        let written = Arc::new(Mutex::new(Vec::new()));
        let jobs = Arc::new(Mutex::new(Vec::<StartupJob>::new()));
        let queued = jobs.clone();
        let mut driver = Prs200Driver::with_board_ports(
            live.clone(),
            LiveSettings::new(HostActivity::default()),
            Box::new(RecordingPorts { written: written.clone() }),
        )
        .with_startup_spawner(Box::new(move |job| queued.lock().unwrap().push(job)));
        let mut host = OfflineHost::default();
        host.listed_ports.push(PortInfo { path: "/dev/openrigcue-fixture".into(), stable_path: None, usb: None });
        let mut link = LinkState::default();
        let mut context =
            MotionContext { model: motion_model("T1000-2M SV", false).unwrap(), link: &mut link, host: &mut host };
        driver.connect(&mut context).unwrap();
        settings.motion.model = "T1000-2M PRS200".into();
        live.set(prosimu_settings(&settings));
        let job = jobs.lock().unwrap().remove(0);
        job(context.host.clock());
        assert_eq!(*written.lock().unwrap(), vec![speed_frame(3, SpeedLayout::Mode4, 4, 200).unwrap(); 5]);
    }

    #[test]
    fn native_prs200_reaction_error_is_deferred_until_the_delayed_profile() {
        use super::*;
        use crate::motion::testing::OfflineHost;
        use openrigcue_devices::motion::driver_support::HostActivity;
        use openrigcue_devices::motion::prosimu::Prs200Driver;
        use openrigcue_devices::motion::{motion_model, LinkState, LiveSettings, MotionContext, MotionDriver};
        let mut raw = Settings::default().native_snapshot();
        raw.get_mut("MOTIONBOX").unwrap().as_dict_mut().unwrap().insert("reaction_speed", "bad".into());
        raw.get_mut("MOTION_COMPENSATION")
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .insert("vertical_actuators_stroke", LegacyValue::Int(150));
        let settings = Settings::from_legacy(&raw).0;
        let activity = HostActivity {
            game_running: "fixture".into(),
            listener_running: "fixture".into(),
            ..HostActivity::default()
        };
        let mut driver = Prs200Driver::new(LiveSettings::new(prosimu_settings(&settings)), LiveSettings::new(activity))
            .with_startup_spawner(Box::new(|_| panic!("an offline host must not start a worker")));
        let mut host = OfflineHost::default();
        let mut link = LinkState::default();
        let mut context =
            MotionContext { model: motion_model("T1000-2M PRS200", false).unwrap(), link: &mut link, host: &mut host };
        driver.connect(&mut context).unwrap();
        assert_eq!(
            driver.send_stream(&[32768; 6], &mut context).unwrap_err().python_str(),
            "'NoneType' object has no attribute 'send_speed_and_acceleration'"
        );
        driver.send_stream(&[32768; 6], &mut context).unwrap();
        context.host.clock().sleep(std::time::Duration::from_secs(4));
        assert_eq!(
            driver.send_stream(&[32768; 6], &mut context).unwrap_err().python_str(),
            "invalid literal for int() with base 10: 'bad'"
        );
    }

    #[test]
    fn vnm_native_stroke_preserves_runtime_settings_and_required_failures() {
        let mut initial = Settings::default();
        initial.vnm.serial_port = "/dev/vnm-stroke-fixture".into();
        initial.vnm.reverse_controller_port_5 = true;
        initial.compensation.height = 777.0;
        for (value, expected) in [
            (serde_json::json!(275), Ok(275)),
            (serde_json::json!("-４_０"), Ok(-40)),
            (serde_json::json!(150.5), Err("ValueError: invalid literal for int() with base 10: '150.5'".into())),
            (serde_json::json!("bad"), Err("ValueError: invalid literal for int() with base 10: 'bad'".into())),
        ] {
            let settings = initial
                .apply_ui_patch(&serde_json::json!({"compensation": {"actuator_vertical_stroke": value}}))
                .unwrap();
            let converted = vnm_settings(&settings);
            assert_eq!(converted.native_vertical_stroke_mm.map(int_setting), Some(expected));
            assert_eq!(converted.port, "/dev/vnm-stroke-fixture");
            assert_eq!(converted.native_reverse_ports.unwrap()[0], Ok(true));
            assert_eq!(settings.compensation.height, 777.0);
        }
        for (value, expected) in [
            (Some(LegacyValue::Float(150.5)), Ok(150)),
            (Some(LegacyValue::Bytes(b"275".to_vec())), Ok(275)),
            (
                Some(LegacyValue::None),
                Err("TypeError: int() argument must be a string, a bytes-like object or a real number, not 'NoneType'"
                    .into()),
            ),
            (None, Err("KeyError: 'vertical_actuators_stroke'".into())),
        ] {
            let mut settings = initial.clone();
            settings.edit_native(|root| {
                let section = root.get_mut("MOTION_COMPENSATION").unwrap().as_dict_mut().unwrap();
                match value {
                    Some(value) => section.insert("vertical_actuators_stroke", value),
                    None => {
                        section.remove("vertical_actuators_stroke");
                    }
                }
            });
            assert_eq!(vnm_settings(&settings).native_vertical_stroke_mm.map(int_setting), Some(expected));
        }
        initial.edit_native(|root| {
            root.remove("MOTION_COMPENSATION");
        });
        assert_eq!(
            vnm_settings(&initial).native_vertical_stroke_mm.map(int_setting),
            Some(Err("KeyError: 'MOTION_COMPENSATION'".into()))
        );
    }

    #[test]
    fn vnm_native_reverse_producer_keeps_required_order_and_nonfactory_values() {
        let mut settings = Settings::default();
        settings.vnm.serial_port = "/dev/vnm-fixture".into();
        settings.compensation.actuator_vertical_stroke = 275.0;
        settings.edit_native(|root| {
            let fields = root.get_mut("VNM SIMULATION").unwrap().as_dict_mut().unwrap();
            fields.insert("reverse_controller_port_5", LegacyValue::from("yes"));
            fields.insert("reverse_controller_port_6", LegacyValue::from("invalid"));
            fields.insert("reverse_controller_port_7", LegacyValue::None);
            fields.remove("reverse_controller_port_8");
            fields.insert("reverse_controller_port_9", LegacyValue::Bool(false));
        });
        let values = vnm_settings(&settings);
        let native = values.native_reverse_ports.unwrap();
        assert_eq!(native[0], Ok(true));
        assert_eq!(native[1], Err("Exception: Boolean value expected. Got invalid".into()));
        assert_eq!(native[2], Err("TypeError: can only concatenate str (not \"NoneType\") to str".into()));
        assert_eq!(native[3], Err("KeyError: 'reverse_controller_port_8'".into()));
        assert_eq!(native[4], Ok(false));
        assert_eq!(values.port, "/dev/vnm-fixture");
        assert_eq!(values.vertical_stroke_mm, 275);
        let patch = settings.apply_ui_patch(&serde_json::json!({"vnm": {"reverse_controller_port_6": "no"}})).unwrap();
        assert_eq!(vnm_settings(&patch).native_reverse_ports.unwrap()[1], Ok(false));
    }

    #[test]
    fn smc3_native_startup_values_use_integer_conversion_without_port_range_validation() {
        let mut settings = Settings::default();
        settings.edit_native(|root| {
            let dof = root.get_mut("DOF REALITY").unwrap().as_dict_mut().unwrap();
            dof.insert("left_debug_port", LegacyValue::from("-1"));
            dof.insert("right_debug_port", LegacyValue::from("65536"));
            root.get_mut("MOTIONBOX")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .insert("reaction_speed", LegacyValue::Float(55.5));
        });
        let startup = smc3_settings(&settings).native_startup.unwrap().unwrap();
        assert_eq!(startup.debug_ports, [None, None], "port range checking belongs to caught sendto");
        assert_eq!(startup.previous_kp, 75.into());
        settings.edit_native(|root| {
            let dof = root.get_mut("DOF REALITY").unwrap().as_dict_mut().unwrap();
            dof.insert("left_debug_port", LegacyValue::Bytes(b"30017".to_vec()));
            dof.insert("right_debug_port", LegacyValue::Bool(true));
        });
        assert_eq!(smc3_settings(&settings).native_startup.unwrap().unwrap().debug_ports, [Some(30_017), Some(1)]);
        settings.edit_native(|root| {
            let dof = root.get_mut("DOF REALITY").unwrap().as_dict_mut().unwrap();
            dof.insert("left_debug_port", LegacyValue::from("３_０_０_１_７"));
            dof.insert("right_debug_port", LegacyValue::Float(30_018.9));
        });
        assert_eq!(smc3_settings(&settings).native_startup.unwrap().unwrap().debug_ports, [Some(30_017), Some(30_018)]);
    }

    #[test]
    fn smc3_native_startup_required_reads_fail_in_module_order() {
        let mut settings = Settings::default();
        settings.edit_native(|root| {
            let dof = root.get_mut("DOF REALITY").unwrap().as_dict_mut().unwrap();
            dof.insert("left_debug_port", LegacyValue::None);
            dof.insert("right_debug_port", LegacyValue::from("bad-right"));
            root.get_mut("MOTIONBOX")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .insert("reaction_speed", LegacyValue::from("bad-reaction"));
        });
        let error = smc3_settings(&settings).native_startup.unwrap().unwrap_err();
        assert!(error.contains("line 26"));
        assert!(error.ends_with(
            "TypeError: int() argument must be a string, a bytes-like object or a real number, not 'NoneType'\n"
        ));
        settings.edit_native(|root| {
            root.get_mut("DOF REALITY")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .insert("left_debug_port", LegacyValue::from("20017"));
        });
        let error = smc3_settings(&settings).native_startup.unwrap().unwrap_err();
        assert!(error.contains("line 27"));
        assert!(error.ends_with("ValueError: invalid literal for int() with base 10: 'bad-right'\n"));
        settings.edit_native(|root| {
            root.get_mut("DOF REALITY")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .insert("right_debug_port", LegacyValue::from("20018"));
        });
        let error = smc3_settings(&settings).native_startup.unwrap().unwrap_err();
        assert!(error.contains("line 785"));
        assert!(error.ends_with("ValueError: invalid literal for int() with base 10: 'bad-reaction'\n"));
        settings.edit_native(|root| {
            root.remove("DOF REALITY");
        });
        assert!(smc3_settings(&settings).native_startup.unwrap().unwrap_err().ends_with("KeyError: 'DOF REALITY'\n"));
    }

    #[test]
    fn native_prs200_startup_model_keeps_required_raw_values() {
        use super::*;
        let mut section = Dict::new();
        let mut raw = Dict::new();
        raw.insert("MOTIONBOX", LegacyValue::Dict(section.clone()));
        assert_eq!(prosimu_settings(&Settings::from_legacy(&raw).0).model, Err("KeyError: 'model'".into()));
        section.insert("model", LegacyValue::Bool(true));
        raw.insert("MOTIONBOX", LegacyValue::Dict(section));
        assert_eq!(prosimu_settings(&Settings::from_legacy(&raw).0).model, Ok(LegacyValue::Bool(true)));
    }

    #[test]
    fn native_prs200_reaction_keeps_native_integer_conversion_and_missing_keys() {
        use super::*;
        let mut section = Dict::new();
        let mut raw = Dict::new();
        raw.insert("MOTIONBOX", LegacyValue::Dict(section.clone()));
        assert_eq!(
            prosimu_settings(&Settings::from_legacy(&raw).0).reaction_speed,
            Err("KeyError: 'reaction_speed'".into())
        );
        for (value, expected) in [
            (LegacyValue::Str("８_５".into()), Ok(85)),
            (LegacyValue::Float(85.5), Ok(85)),
            (LegacyValue::Bool(true), Ok(1)),
            (LegacyValue::Str("bad".into()), Err("ValueError: invalid literal for int() with base 10: 'bad'".into())),
        ] {
            section.insert("reaction_speed", value.clone());
            raw.insert("MOTIONBOX", LegacyValue::Dict(section.clone()));
            // The raw value reaches the driver; it is converted where the reaction speed is checked.
            let carried = prosimu_settings(&Settings::from_legacy(&raw).0).reaction_speed;
            assert_eq!(carried, Ok(value));
            assert_eq!(int_setting(carried), expected);
        }
    }

    #[test]
    fn native_prs200_ports_raise_at_the_first_enumerated_port() {
        use super::*;
        use crate::motion::testing::OfflineHost;
        use openrigcue_devices::discovery::PortInfo;
        use openrigcue_devices::motion::driver_support::HostActivity;
        use openrigcue_devices::motion::prosimu::{board::BoardPorts, Prs200Driver};
        use openrigcue_devices::motion::{motion_model, LinkState, LiveSettings, MotionContext, MotionDriver};
        use openrigcue_devices::serial::{Link, LinkError};
        struct NoBoardOpens;
        impl BoardPorts for NoBoardOpens {
            fn open(&mut self, _: &str) -> Result<Box<dyn Link>, LinkError> {
                panic!("missing required port must stop before a board open")
            }
        }
        let mut raw = Settings::default().native_snapshot();
        let section = raw.get_mut("PROSIMU").unwrap().as_dict_mut().unwrap();
        section.remove("prs200_com_port1");
        section.remove("prs200_com_port2");
        let settings = Settings::from_legacy(&raw).0;
        let mut driver = Prs200Driver::with_board_ports(
            LiveSettings::new(prosimu_settings(&settings)),
            LiveSettings::new(HostActivity::default()),
            Box::new(NoBoardOpens),
        )
        .with_startup_spawner(Box::new(|_| panic!("missing required port must stop before startup")));
        let mut host = OfflineHost::default();
        host.listed_ports.push(PortInfo { path: "/dev/openrigcue-fixture".into(), stable_path: None, usb: None });
        let mut link = LinkState::default();
        let mut context =
            MotionContext { model: motion_model("T1000-2M PRS200", false).unwrap(), link: &mut link, host: &mut host };
        assert_eq!(driver.connect(&mut context).unwrap_err().python_str(), "'prs200_com_port1'");
    }

    #[test]
    fn native_prs200_ports_keep_required_native_values() {
        use super::*;
        let missing = Settings::from_legacy(&Dict::new()).0;
        assert_eq!(prosimu_settings(&missing).port1, Err("KeyError: 'PROSIMU'".into()));
        let mut section = Dict::new();
        let mut raw = Dict::new();
        raw.insert("PROSIMU", LegacyValue::Dict(section.clone()));
        assert_eq!(prosimu_settings(&Settings::from_legacy(&raw).0).port2, Err("KeyError: 'prs200_com_port2'".into()));
        section.insert("prs200_com_port1", LegacyValue::Bool(true));
        section.insert("prs200_com_port2", LegacyValue::Bytes(b"fixture".to_vec()));
        raw.insert("PROSIMU", LegacyValue::Dict(section));
        let inputs = prosimu_settings(&Settings::from_legacy(&raw).0);
        assert_eq!(inputs.port1, Ok(LegacyValue::Bool(true)));
        assert_eq!(inputs.port2, Ok(LegacyValue::Bytes(b"fixture".to_vec())));
    }

    #[test]
    fn native_prs200_stroke_error_reaches_the_streaming_read() {
        use super::*;
        use crate::motion::testing::OfflineHost;
        use openrigcue_devices::motion::driver_support::HostActivity;
        use openrigcue_devices::motion::prosimu::Prs200Driver;
        use openrigcue_devices::motion::{motion_model, LinkState, LiveSettings, MotionContext, MotionDriver};
        let mut raw = Settings::default().native_snapshot();
        raw.get_mut("MOTION_COMPENSATION")
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .insert("vertical_actuators_stroke", "bad".into());
        let settings = Settings::from_legacy(&raw).0;
        let mut driver = Prs200Driver::new(
            LiveSettings::new(prosimu_settings(&settings)),
            LiveSettings::new(HostActivity::default()),
        )
        .with_startup_spawner(Box::new(|_| panic!("an offline host must not start a worker")));
        let mut host = OfflineHost::default();
        let mut link = LinkState::default();
        let mut context =
            MotionContext { model: motion_model("T1000-3M SV", false).unwrap(), link: &mut link, host: &mut host };
        driver.connect(&mut context).unwrap();
        assert_eq!(
            driver.send_stream(&[32768; 6], &mut context).unwrap_err().python_str(),
            "invalid literal for int() with base 10: 'bad'"
        );
    }

    #[test]
    fn native_prs200_enable_reaches_connect_before_any_port_scan() {
        use super::*;
        use crate::motion::testing::OfflineHost;
        use openrigcue_devices::motion::driver_support::HostActivity;
        use openrigcue_devices::motion::prosimu::Prs200Driver;
        use openrigcue_devices::motion::{motion_model, LinkState, LiveSettings, MotionContext, MotionDriver};
        let mut section = Dict::new();
        section.insert("enable_dyatic_tl", "maybe".into());
        let mut raw = Dict::new();
        raw.insert("PROSIMU", LegacyValue::Dict(section));
        let settings = Settings::from_legacy(&raw).0;
        let mut driver = Prs200Driver::new(
            LiveSettings::new(prosimu_settings(&settings)),
            LiveSettings::new(HostActivity::default()),
        )
        .with_startup_spawner(Box::new(|_| panic!("invalid enable must stop before startup")));
        let mut host = OfflineHost::default();
        let mut link = LinkState::default();
        let mut context =
            MotionContext { model: motion_model("T1000-2M PRS200", false).unwrap(), link: &mut link, host: &mut host };
        assert_eq!(driver.connect(&mut context).unwrap_err().python_str(), "Boolean value expected. Got maybe");
    }

    #[test]
    fn native_prs200_invert_reaches_the_frame_after_its_enable_guard() {
        use super::*;
        use crate::motion::testing::OfflineHost;
        use openrigcue_devices::motion::driver_support::HostActivity;
        use openrigcue_devices::motion::prosimu::Prs200Driver;
        use openrigcue_devices::motion::{motion_model, LinkState, LiveSettings, MotionContext, MotionDriver};
        let mut section = Dict::new();
        section.insert("enable_dyatic_tl", "yes".into());
        section.insert("invert_tl", "maybe".into());
        let mut raw = Dict::new();
        raw.insert("PROSIMU", LegacyValue::Dict(section));
        let settings = Settings::from_legacy(&raw).0;
        let mut driver = Prs200Driver::new(
            LiveSettings::new(prosimu_settings(&settings)),
            LiveSettings::new(HostActivity::default()),
        )
        .with_startup_spawner(Box::new(|_| panic!("an offline host must not start a worker")));
        let mut host = OfflineHost::default();
        let mut link = LinkState::default();
        let mut context =
            MotionContext { model: motion_model("T1000-3M PRS200", false).unwrap(), link: &mut link, host: &mut host };
        driver.connect(&mut context).unwrap();
        assert_eq!(
            driver.send_stream(&[32768; 6], &mut context).unwrap_err().python_str(),
            "'NoneType' object has no attribute 'send_speed_and_acceleration'"
        );
        assert_eq!(
            driver.send_stream(&[32768; 6], &mut context).unwrap_err().python_str(),
            "Boolean value expected. Got maybe"
        );
    }

    #[test]
    fn native_prs200_stroke_keeps_required_native_integer_conversion() {
        use super::*;
        let mut section = Dict::new();
        let mut raw = Dict::new();
        raw.insert("MOTION_COMPENSATION", LegacyValue::Dict(section.clone()));
        assert_eq!(
            int_setting(prosimu_settings(&Settings::from_legacy(&raw).0).vertical_stroke_mm),
            Err("KeyError: 'vertical_actuators_stroke'".into())
        );
        for (value, expected) in [
            (LegacyValue::Str("-４_０".into()), Ok(-40)),
            (LegacyValue::Float(1.5), Ok(1)),
            (LegacyValue::Bool(true), Ok(1)),
            (LegacyValue::Str("bad".into()), Err("ValueError: invalid literal for int() with base 10: 'bad'".into())),
        ] {
            section.insert("vertical_actuators_stroke", value);
            raw.insert("MOTION_COMPENSATION", LegacyValue::Dict(section.clone()));
            assert_eq!(int_setting(prosimu_settings(&Settings::from_legacy(&raw).0).vertical_stroke_mm), expected);
        }
    }

    #[test]
    fn native_prs200_firmware_preserves_values_outside_the_typed_projection() {
        use super::*;
        use openrigcue_model::native_value::{Dict, LegacyValue};
        let mut section = Dict::new();
        section.insert("prs200_firmware", LegacyValue::Str("-7".into()));
        let mut raw = Dict::new();
        raw.insert("PROSIMU", LegacyValue::Dict(section));
        let settings = Settings::from_legacy(&raw).0;
        assert_eq!(int_setting(prosimu_settings(&settings).firmware).expect("native firmware"), -7);
    }

    #[test]
    fn native_prs200_enable_keeps_native_boolean_errors() {
        use super::*;
        assert_eq!(
            prosimu_settings(&Settings::from_legacy(&Dict::new()).0).enable_dyadic_tl,
            Err("KeyError: 'PROSIMU'".into())
        );
        let mut section = Dict::new();
        let mut raw = Dict::new();
        raw.insert("PROSIMU", LegacyValue::Dict(section.clone()));
        assert_eq!(
            prosimu_settings(&Settings::from_legacy(&raw).0).enable_dyadic_tl,
            Err("KeyError: 'enable_dyatic_tl'".into())
        );
        for (value, expected) in [
            (LegacyValue::Str("maybe".into()), Err("Exception: Boolean value expected. Got maybe".into())),
            (LegacyValue::Float(1.0), Err("TypeError: can only concatenate str (not \"float\") to str".into())),
            (LegacyValue::Int(0), Ok(false)),
            (LegacyValue::Str("Y".into()), Ok(true)),
        ] {
            section.insert("enable_dyatic_tl", value);
            raw.insert("PROSIMU", LegacyValue::Dict(section.clone()));
            assert_eq!(prosimu_settings(&Settings::from_legacy(&raw).0).enable_dyadic_tl, expected);
        }
    }

    #[test]
    fn native_prs200_invert_uses_its_required_key_and_boolean_conversion() {
        use super::*;
        let mut section = Dict::new();
        let mut raw = Dict::new();
        raw.insert("PROSIMU", LegacyValue::Dict(section.clone()));
        assert_eq!(prosimu_settings(&Settings::from_legacy(&raw).0).invert_tl, Err("KeyError: 'invert_tl'".into()));
        section.insert("invert_tl", LegacyValue::Float(1.0));
        raw.insert("PROSIMU", LegacyValue::Dict(section));
        assert_eq!(
            prosimu_settings(&Settings::from_legacy(&raw).0).invert_tl,
            Err("TypeError: can only concatenate str (not \"float\") to str".into())
        );
    }

    #[test]
    fn native_prs200_firmware_keeps_required_lookup_and_conversion_failures() {
        use super::*;
        use openrigcue_model::native_value::{Dict, LegacyValue};
        let missing = Settings::from_legacy(&Dict::new()).0;
        assert_eq!(int_setting(prosimu_settings(&missing).firmware), Err("KeyError: 'PROSIMU'".into()));
        let mut section = Dict::new();
        let mut raw = Dict::new();
        raw.insert("PROSIMU", LegacyValue::Dict(section.clone()));
        assert_eq!(
            int_setting(prosimu_settings(&Settings::from_legacy(&raw).0).firmware),
            Err("KeyError: 'prs200_firmware'".into())
        );
        for (value, expected) in [
            (LegacyValue::Str("bad".into()), Err("ValueError: invalid literal for int() with base 10: 'bad'".into())),
            (LegacyValue::Str("４_０".into()), Ok(40)),
            (LegacyValue::Bool(true), Ok(1)),
        ] {
            section.insert("prs200_firmware", value);
            raw.insert("PROSIMU", LegacyValue::Dict(section.clone()));
            assert_eq!(int_setting(prosimu_settings(&Settings::from_legacy(&raw).0).firmware), expected);
        }
    }

    #[test]
    fn native_dyadic_inputs_preserve_configured_first_frame_values() {
        use super::*;
        use openrigcue_model::native_value::{Dict, LegacyValue};
        let mut simtech = Dict::new();
        simtech.insert("scn6_speed", LegacyValue::Str("4_000".into()));
        simtech.insert("scn6_acceleration", LegacyValue::Bool(true));
        let mut raw = Dict::new();
        raw.insert("SIMTECHREVOLUTION", LegacyValue::Dict(simtech));
        let settings = Settings::from_legacy(&raw).0;
        let inputs = dyadic_settings(&settings);
        assert_eq!(
            int_setting(inputs.simtech_scn6_speed.clone()).expect("configured speed must reach the first frame"),
            4000
        );
        assert_eq!(
            int_setting(inputs.simtech_scn6_acceleration.clone())
                .expect("configured acceleration must reach the first frame"),
            1
        );
    }

    #[test]
    fn native_simtech_inputs_preserve_a_configured_value_outside_the_typed_projection() {
        use super::*;
        use openrigcue_model::native_value::{Dict, LegacyValue};
        let mut section = Dict::new();
        section.insert("post_homing_speed", LegacyValue::Str("-7".into()));
        let mut raw = Dict::new();
        raw.insert("SIMTECHREVOLUTION", LegacyValue::Dict(section));
        let settings = Settings::from_legacy(&raw).0;
        let tune = FactoryTunes::builtin().get("emulator").unwrap().clone();
        let telemetry = Telemetry::off();
        let runtime = DriverRuntime {
            game_running: "none",
            listener_running: "none",
            motion_test: false,
            motion_switch: true,
            motion_error: false,
            telemetry: &telemetry,
            simtech_tune: LegacyValue::from(tune.to_json()),
        };
        assert_eq!(int_setting(simtech_settings(&settings, &runtime).post_homing_speed).expect("native speed"), -7);
    }

    #[test]
    fn native_simtech_inputs_keep_config_and_game_conversion_failures() {
        let mut simtech = Dict::new();
        simtech.insert("post_homing_speed", "bad".into());
        simtech.insert("post_homing_acceleration", LegacyValue::None);
        simtech.insert("post_homing_speed_timeout", "1_2".into());
        let mut motion = Dict::new();
        motion.insert("refresh_rate", LegacyValue::Bytes(b"3_0".to_vec()));
        let mut raw = Dict::new();
        raw.insert("SIMTECHREVOLUTION", simtech.into());
        raw.insert("MOTIONBOX", motion.into());
        let settings = Settings::from_legacy(&raw).0;
        let mut game = Dict::new();
        game.insert("scn6_speed", "1__2".into());
        game.insert("scn6_acceleration", true.into());
        let telemetry = Telemetry::off();
        let runtime = DriverRuntime {
            game_running: "none",
            listener_running: "none",
            motion_test: false,
            motion_switch: true,
            motion_error: false,
            telemetry: &telemetry,
            simtech_tune: game.into(),
        };
        // The raw values reach the driver, whose integer conversions give these results.
        let values = simtech_settings(&settings, &runtime);
        assert_eq!(
            int_setting(values.post_homing_speed.clone()).unwrap_err(),
            "ValueError: invalid literal for int() with base 10: 'bad'"
        );
        assert_eq!(
            int_setting(values.post_homing_acceleration.clone()).unwrap_err(),
            "TypeError: int() argument must be a string, a bytes-like object or a real number, not 'NoneType'"
        );
        assert_eq!(int_setting(values.post_homing_speed_timeout.clone()).unwrap(), 12);
        assert_eq!(int_setting(values.refresh_rate.clone()).unwrap(), 30);
        assert_eq!(
            int_setting(values.scn6_speed.clone()).unwrap_err(),
            "ValueError: invalid literal for int() with base 10: '1__2'"
        );
        assert_eq!(int_setting(values.scn6_acceleration.clone()).unwrap(), 1);
    }

    #[test]
    fn native_simtech_inputs_keep_missing_and_malformed_roots() {
        let settings = Settings::from_legacy(&Dict::new()).0;
        let telemetry = Telemetry::off();
        let runtime = DriverRuntime {
            game_running: "none",
            listener_running: "none",
            motion_test: false,
            motion_switch: true,
            motion_error: false,
            telemetry: &telemetry,
            simtech_tune: LegacyValue::None,
        };
        let values = simtech_settings(&settings, &runtime);
        assert_eq!(values.post_homing_speed.unwrap_err(), "KeyError: 'SIMTECHREVOLUTION'");
        assert_eq!(int_setting(values.refresh_rate.clone()).unwrap_err(), "KeyError: 'MOTIONBOX'");
        assert_eq!(
            int_setting(values.scn6_speed.clone()).unwrap_err(),
            "TypeError: 'NoneType' object is not subscriptable"
        );
        assert_eq!(
            int_setting(values.scn6_acceleration.clone()).unwrap_err(),
            "TypeError: 'NoneType' object is not subscriptable"
        );
    }

    #[test]
    fn native_dyadic_inputs_keep_required_lookup_and_conversion_errors_distinct() {
        use super::*;
        use openrigcue_model::native_value::{Dict, LegacyValue};
        let missing = dyadic_settings(&Settings::from_legacy(&Dict::new()).0);
        assert_eq!(int_setting(missing.simtech_scn6_speed.clone()).unwrap_err(), "KeyError: 'SIMTECHREVOLUTION'");
        for (section, speed_error, accel_error) in [
            (LegacyValue::Dict(Dict::new()), "KeyError: 'scn6_speed'", "KeyError: 'scn6_acceleration'"),
            (
                LegacyValue::None,
                "TypeError: 'NoneType' object is not subscriptable",
                "TypeError: 'NoneType' object is not subscriptable",
            ),
        ] {
            let mut raw = Dict::new();
            raw.insert("SIMTECHREVOLUTION", section);
            let inputs = dyadic_settings(&Settings::from_legacy(&raw).0);
            assert_eq!(int_setting(inputs.simtech_scn6_speed.clone()).unwrap_err(), speed_error);
            assert_eq!(int_setting(inputs.simtech_scn6_acceleration.clone()).unwrap_err(), accel_error);
        }
        let mut section = Dict::new();
        section.insert("scn6_speed", LegacyValue::Str("bad".into()));
        section.insert("scn6_acceleration", LegacyValue::None);
        let mut raw = Dict::new();
        raw.insert("SIMTECHREVOLUTION", LegacyValue::Dict(section));
        let inputs = dyadic_settings(&Settings::from_legacy(&raw).0);
        assert_eq!(
            int_setting(inputs.simtech_scn6_speed.clone()).unwrap_err(),
            "ValueError: invalid literal for int() with base 10: 'bad'"
        );
        assert_eq!(
            int_setting(inputs.simtech_scn6_acceleration.clone()).unwrap_err(),
            "TypeError: int() argument must be a string, a bytes-like object or a real number, not 'NoneType'"
        );
    }

    #[test]
    fn native_dyadic_inputs_reuse_native_integer_conversion_before_any_narrowing() {
        use super::*;
        use openrigcue_model::native_value::{Dict, LegacyValue};
        for (speed, expected) in
            [(LegacyValue::Float(12.9), 12), (LegacyValue::Str("9223372036854775808".into()), 9223372036854775808_i128)]
        {
            let mut section = Dict::new();
            section.insert("scn6_speed", speed);
            section.insert("scn6_acceleration", LegacyValue::Bytes(b"1_6".to_vec()));
            let mut raw = Dict::new();
            raw.insert("SIMTECHREVOLUTION", LegacyValue::Dict(section));
            let inputs = dyadic_settings(&Settings::from_legacy(&raw).0);
            assert_eq!(int_setting(inputs.simtech_scn6_speed.clone()).unwrap(), expected);
            assert_eq!(int_setting(inputs.simtech_scn6_acceleration.clone()).unwrap(), 16);
        }
    }

    #[test]
    fn configured_smc3_resolutions_come_from_native_values_before_unsigned_projection() {
        use super::*;
        use openrigcue_model::native_value::{Dict, LegacyValue};
        for (left, expected) in [
            (LegacyValue::Str("-3".into()), "-3"),
            (LegacyValue::Str("１２".into()), "１２"),
            (LegacyValue::Str("1__2".into()), "1__2"),
            (LegacyValue::Str("12.0".into()), "12.0"),
            (LegacyValue::Float(12.9), "12"),
            (LegacyValue::Bool(true), "1"),
            (LegacyValue::None, ""),
            (LegacyValue::List(Vec::new()), ""),
            (LegacyValue::Bytes(b"12".to_vec()), "12"),
        ] {
            let mut dof = Dict::new();
            dof.insert("left_controller_resolution", left);
            dof.insert("right_controller_resolution", LegacyValue::Str("bad".into()));
            let mut raw = Dict::new();
            raw.insert("DOF REALITY", LegacyValue::Dict(dof));
            let settings = Settings::from_legacy(&raw).0;
            let actual = smc3_settings(&settings);
            assert_eq!(actual.left_controller_resolution, expected);
            assert_eq!(actual.right_controller_resolution, "bad", "each box retains its own conversion/fallback");
        }
        let settings = Settings::from_legacy(&Dict::new()).0;
        assert_eq!(smc3_settings(&settings).left_controller_resolution, "");
        assert_eq!(smc3_settings(&Settings::default()).left_controller_resolution, "10");
    }

    #[test]
    fn smc3_native_switches_preserve_runtime_values_and_missing_errors() {
        let base = Settings::default();
        for (raw, expected) in [
            (Some(LegacyValue::from("yes")), Ok(true)),
            (Some(LegacyValue::from("no")), Ok(false)),
            (Some(LegacyValue::Bool(true)), Ok(true)),
            (Some(LegacyValue::Int(0)), Ok(false)),
            (Some(LegacyValue::from("invalid")), Err("Exception: Boolean value expected. Got invalid".into())),
            (Some(LegacyValue::None), Err("TypeError: can only concatenate str (not \"NoneType\") to str".into())),
            (None, Err("KeyError: 'enable_debug'".into())),
        ] {
            let mut settings = base.clone();
            settings.edit_native(|root| {
                let motion = root.get_mut("MOTIONBOX").unwrap().as_dict_mut().unwrap();
                motion.insert("reaction_speed", "82".into());
                if let Some(value) = raw.clone() {
                    motion.insert("enable_debug", value);
                } else {
                    motion.remove("enable_debug");
                }
            });
            let native = smc3_settings(&settings);
            assert_eq!(native.native_switches.unwrap().enable_debug, expected);
            assert_eq!(native.reaction_speed, 82);
        }
        let settings = base.apply_ui_patch(&serde_json::json!({"dof_reality": {"update_kp": "invalid"}})).unwrap();
        assert_eq!(
            smc3_settings(&settings).native_switches.unwrap().update_kp,
            Err("Exception: Boolean value expected. Got invalid".into())
        );
        let mut missing = base;
        missing.edit_native(|root| {
            root.get_mut("DOF REALITY").unwrap().as_dict_mut().unwrap().remove("update_kp");
        });
        assert_eq!(smc3_settings(&missing).native_switches.unwrap().update_kp, Err("KeyError: 'update_kp'".into()));
    }

    #[test]
    fn smc3_native_reaction_keeps_errors_and_valid_width_distinct() {
        for (raw, expected) in [
            (Some(LegacyValue::from("80.5")), Err("ValueError: invalid literal for int() with base 10: '80.5'".into())),
            (Some(LegacyValue::Float(80.5)), Ok(80)),
            (Some(LegacyValue::Bytes(b"80".to_vec())), Ok(80)),
            (Some(LegacyValue::Bool(true)), Ok(1)),
            (
                Some(LegacyValue::None),
                Err("TypeError: int() argument must be a string, a bytes-like object or a real number, not 'NoneType'"
                    .into()),
            ),
            (Some(LegacyValue::Float(f64::NAN)), Err("ValueError: cannot convert float NaN to integer".into())),
            (
                Some(LegacyValue::Float(f64::INFINITY)),
                Err("OverflowError: cannot convert float infinity to integer".into()),
            ),
            (None, Err("KeyError: 'reaction_speed'".into())),
        ] {
            let mut settings = Settings::default();
            settings.edit_native(|root| {
                let section = root.get_mut("MOTIONBOX").unwrap().as_dict_mut().unwrap();
                section.insert("enable_debug", "no".into());
                match raw.clone() {
                    Some(value) => {
                        section.insert("reaction_speed", value);
                    }
                    None => {
                        section.remove("reaction_speed");
                    }
                }
            });
            assert_eq!(int_setting(smc3_settings(&settings).native_reaction_speed.unwrap()), expected);
            assert_eq!(smc3_settings(&settings).native_switches.unwrap().enable_debug, Ok(false));
        }
        // A valid wide value reaches the driver as it is: its integer conversion there has no width limit.
        for value in [LegacyValue::from("9223372036854775808"), LegacyValue::Float(1e100)] {
            let mut settings = Settings::default();
            settings.edit_native(|root| {
                root.get_mut("MOTIONBOX").unwrap().as_dict_mut().unwrap().insert("reaction_speed", value.clone());
            });
            assert_eq!(smc3_settings(&settings).native_reaction_speed, Some(Ok(value)));
        }
    }

    #[test]
    fn smc3_runtime_settings_patch_reaches_native_reaction_before_any_frame() {
        use crate::motion::testing::OfflineHost;
        use openrigcue_devices::motion::{
            motion_model, smc3::Smc3Driver, LinkState, LiveSettings, MotionContext, MotionDriver,
        };
        let initial = Settings::default();
        let live = LiveSettings::new(smc3_settings(&initial));
        let mut driver = Smc3Driver::new(live.clone());
        let changed = initial.apply_ui_patch(&serde_json::json!({"motion": {"reaction_speed": "80.5"}})).unwrap();
        assert_eq!(
            changed.native_snapshot().get("MOTIONBOX").unwrap().item("reaction_speed"),
            Ok(&LegacyValue::from("80.5"))
        );
        live.set(smc3_settings(&changed));
        let mut host = OfflineHost::default();
        let mut link = LinkState::default();
        let mut context = MotionContext { model: motion_model("P3", false).unwrap(), link: &mut link, host: &mut host };
        let error = driver.send_stream(&[], &mut context).unwrap_err();
        assert_eq!(error.python_str(), "invalid literal for int() with base 10: '80.5'");
    }

    use super::*;
    use openrigcue_config::tunes::FactoryTunes;

    /// Start-up: the clip factors are the `MOTIONBOX.<axis>_clip_factor` floats,
    /// 1000 on a fresh install.
    #[test]
    fn clip_factors_from_motionbox() {
        let mut settings = Settings::default();
        assert_eq!(clip_factors(&settings), ClipFactors::default());
        settings.motion.traction_loss_front_clip_factor = 250.0;
        assert_eq!(clip_factors(&settings).0[Axis::TractionLossFront], 250.0);
    }

    /// The stream path reads `MOTIONBOX` integers and the manufacturer's section: a fresh install gives the
    /// cueing defaults of the DOF REALITY section.
    #[test]
    fn fresh_install_motion_settings_match_the_cueing_defaults() {
        let settings = Settings::default();
        let converted = motion_settings(&settings, Manufacturer::DofReality);
        assert_eq!(converted, MotionSettings::default());
    }

    /// The integer conversion truncates toward zero.
    #[test]
    fn python_int_truncates() {
        assert_eq!(py_int(4.9), 4);
        assert_eq!(py_int(-4.9), -4);
    }

    /// The tune's motion keys: the master configuration's `joy/vehicles/Default` tune becomes the cueing default tune.
    #[test]
    fn default_tune_matches_the_cueing_default() {
        let factory = FactoryTunes::builtin();
        let joy = factory.get("joy").expect("joy factory tune");
        let default = joy.vehicles.get("Default").expect("joy Default vehicle tune");
        assert_eq!(motion_tune(default), MotionTune::default());
    }

    /// motion_effects.pitch_type: 'angle' or missing reads the angle, 'rate' the rate.
    #[test]
    fn rotation_inputs() {
        assert_eq!(rotation_input(None), RotationInput::Angle);
        assert_eq!(rotation_input(Some(AxisSource::Angle)), RotationInput::Angle);
        assert_eq!(rotation_input(Some(AxisSource::Rate)), RotationInput::Rate);
    }

    /// Thanos reads its manufacturer's section: PT-ACTUATOR models use the PT-ACTUATOR com port and filter.
    #[test]
    fn thanos_section_follows_the_manufacturer() {
        let mut settings = Settings::default();
        settings.pt_actuator.thanos_serial_port = "/dev/pt".into();
        settings.thanos.serial_port = "/dev/thanos".into();
        let tune = FactoryTunes::builtin().get("emulator").unwrap().clone();
        let telemetry = Telemetry::off();
        let runtime = DriverRuntime {
            game_running: "none",
            listener_running: "none",
            motion_test: true,
            motion_switch: true,
            motion_error: false,
            telemetry: &telemetry,
            simtech_tune: LegacyValue::from(tune.to_json()),
        };
        assert_eq!(thanos_settings(&settings, "PT-ACTUATOR", &tune, &runtime).com_port, "/dev/pt");
        let thanos = thanos_settings(&settings, "THANOS", &tune, &runtime);
        assert_eq!(thanos.com_port, "/dev/thanos");
        assert!(thanos.test_mode && thanos.no_game_running);
    }

    #[test]
    fn thanos_native_reverse_values_follow_manufacturer_without_typed_fallback() {
        let mut settings = Settings::default();
        settings.edit_native(|root| {
            for (section, value) in [("THANOS", LegacyValue::from("yes")), ("PT-ACTUATOR", LegacyValue::from("no"))] {
                let fields = root.get_mut(section).unwrap().as_dict_mut().unwrap();
                for port in 5..=8 {
                    fields.insert(format!("reverse_controller_port_{port}"), value.clone());
                }
            }
            let fields = root.get_mut("THANOS").unwrap().as_dict_mut().unwrap();
            fields.insert("reverse_controller_port_6", "maybe".into());
            fields.insert("reverse_controller_port_7", LegacyValue::None);
            fields.remove("reverse_controller_port_8");
        });
        let mut tune = FactoryTunes::builtin().get("ac").unwrap().clone();
        tune.thanos_spike_filter_horizontal = 600.0;
        let telemetry = Telemetry::default();
        let runtime = DriverRuntime {
            game_running: "none",
            listener_running: "none",
            motion_test: false,
            motion_switch: true,
            motion_error: false,
            telemetry: &telemetry,
            simtech_tune: LegacyValue::from(tune.to_json()),
        };
        let thanos = thanos_settings(&settings, "THANOS", &tune, &runtime);
        let flags = thanos.native_reverse_ports.unwrap();
        assert_eq!(flags[0], Ok(true));
        assert_eq!(flags[1], Err("Exception: Boolean value expected. Got maybe".into()));
        assert_eq!(flags[2], Err("TypeError: can only concatenate str (not \"NoneType\") to str".into()));
        assert_eq!(flags[3], Err("KeyError: 'reverse_controller_port_8'".into()));
        assert_eq!(thanos.spike_filter_horizontal, 600.0);
        let pt = thanos_settings(&settings, "PT-ACTUATOR", &tune, &runtime);
        assert_eq!(pt.native_reverse_ports.unwrap(), std::array::from_fn(|_| Ok(false)));
    }

    #[test]
    fn thanos_native_filter_primary_legacy_and_default_preserve_source_order() {
        let mut settings = Settings::default();
        let tune = FactoryTunes::builtin().get("ac").unwrap().clone();
        let telemetry = Telemetry::default();
        let runtime = DriverRuntime {
            game_running: "none",
            listener_running: "none",
            motion_test: false,
            motion_switch: true,
            motion_error: false,
            telemetry: &telemetry,
            simtech_tune: LegacyValue::from(tune.to_json()),
        };
        for (primary, legacy, expected) in [
            (Some(LegacyValue::from("no")), Some(LegacyValue::from("yes")), Ok(false)),
            (
                Some(LegacyValue::from("invalid")),
                Some(LegacyValue::from("no")),
                Err("Exception: Boolean value expected. Got invalid".into()),
            ),
            (
                Some(LegacyValue::None),
                Some(LegacyValue::from("yes")),
                Err("TypeError: can only concatenate str (not \"NoneType\") to str".into()),
            ),
            (None, Some(LegacyValue::from("no")), Ok(false)),
            (None, None, Ok(true)),
        ] {
            settings.edit_native(|root| {
                let fields = root.get_mut("THANOS").unwrap().as_dict_mut().unwrap();
                fields.remove("filter");
                fields.remove("send_spike_filter");
                if let Some(value) = primary.clone() {
                    fields.insert("filter", value);
                }
                if let Some(value) = legacy.clone() {
                    fields.insert("send_spike_filter", value);
                }
            });
            assert_eq!(
                thanos_settings(&settings, "THANOS", &tune, &runtime).native_send_spike_filter.unwrap(),
                expected
            );
            assert_eq!(
                thanos_settings(&settings, "PT-ACTUATOR", &tune, &runtime).native_send_spike_filter.unwrap(),
                Ok(true)
            );
        }
        let missing = Dict::new();
        assert_eq!(native_thanos_filter(&missing, "THANOS"), Err("KeyError: 'THANOS'".into()));
        for (value, expected) in [
            (LegacyValue::None, "TypeError: argument of type 'NoneType' is not iterable"),
            (LegacyValue::from("filter"), "TypeError: string indices must be integers, not 'str'"),
            (LegacyValue::from("other"), "AttributeError: 'str' object has no attribute 'get'"),
        ] {
            let raw = Dict::from_iter([("THANOS".into(), value)]);
            assert_eq!(native_thanos_filter(&raw, "THANOS"), Err(expected.into()));
        }
    }

    #[test]
    fn thanos_native_stroke_uses_runtime_patch_before_typed_fallback() {
        let mut initial = Settings::default();
        initial.compensation.height = 777.0;
        let mut tune = FactoryTunes::builtin().get("ac").unwrap().clone();
        tune.thanos_spike_filter_horizontal = 600.0;
        let telemetry = Telemetry::default();
        let runtime = DriverRuntime {
            game_running: "none",
            listener_running: "none",
            motion_test: false,
            motion_switch: true,
            motion_error: false,
            telemetry: &telemetry,
            simtech_tune: LegacyValue::from(tune.to_json()),
        };
        for (value, expected) in [
            (serde_json::json!(275), Ok(275)),
            (serde_json::json!("-４_０"), Ok(-40)),
            (serde_json::json!(150.5), Err("ValueError: invalid literal for int() with base 10: '150.5'".into())),
            (serde_json::json!("bad"), Err("ValueError: invalid literal for int() with base 10: 'bad'".into())),
        ] {
            let settings = initial
                .apply_ui_patch(&serde_json::json!({"compensation": {"actuator_vertical_stroke": value}}))
                .unwrap();
            let native = settings.native_snapshot();
            assert_eq!(
                native_setting_value(&native, "MOTION_COMPENSATION", "height").unwrap().py_str().as_deref(),
                Some("777")
            );
            let converted = thanos_settings(&settings, "THANOS", &tune, &runtime);
            assert_eq!(converted.native_compensation_stroke_mm.map(int_setting), Some(expected));
            assert_eq!(converted.spike_filter_horizontal, 600.0);
        }
    }

    #[test]
    fn thanos_native_stroke_preserves_required_reads_and_integer_kinds() {
        let mut settings = Settings::default();
        let tune = FactoryTunes::builtin().get("ac").unwrap().clone();
        let telemetry = Telemetry::default();
        let runtime = DriverRuntime {
            game_running: "none",
            listener_running: "none",
            motion_test: false,
            motion_switch: true,
            motion_error: false,
            telemetry: &telemetry,
            simtech_tune: LegacyValue::from(tune.to_json()),
        };
        for (value, expected) in [
            (LegacyValue::Float(150.5), Ok(150)),
            (LegacyValue::Bytes(b"275".to_vec()), Ok(275)),
            (LegacyValue::Bool(true), Ok(1)),
            (LegacyValue::Int(0), Ok(0)),
            (
                LegacyValue::None,
                Err("TypeError: int() argument must be a string, a bytes-like object or a real number, not 'NoneType'"
                    .into()),
            ),
            (LegacyValue::Float(f64::NAN), Err("ValueError: cannot convert float NaN to integer".into())),
            (LegacyValue::Float(f64::INFINITY), Err("OverflowError: cannot convert float infinity to integer".into())),
        ] {
            settings.edit_native(|root| {
                root.get_mut("MOTION_COMPENSATION")
                    .unwrap()
                    .as_dict_mut()
                    .unwrap()
                    .insert("vertical_actuators_stroke", value);
            });
            assert_eq!(
                thanos_settings(&settings, "THANOS", &tune, &runtime).native_compensation_stroke_mm.map(int_setting),
                Some(expected)
            );
        }
        settings.edit_native(|root| {
            root.get_mut("MOTION_COMPENSATION").unwrap().as_dict_mut().unwrap().remove("vertical_actuators_stroke");
        });
        assert_eq!(
            thanos_settings(&settings, "THANOS", &tune, &runtime).native_compensation_stroke_mm.map(int_setting),
            Some(Err("KeyError: 'vertical_actuators_stroke'".into()))
        );
        settings.edit_native(|root| {
            root.remove("MOTION_COMPENSATION");
        });
        assert_eq!(
            thanos_settings(&settings, "THANOS", &tune, &runtime).native_compensation_stroke_mm,
            Some(Err("KeyError: 'MOTION_COMPENSATION'".into()))
        );
        // A valid wide integer reaches the driver's integer conversion unbounded.
        let mut wide = Settings::default();
        wide.edit_native(|root| {
            root.get_mut("MOTION_COMPENSATION")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .insert("vertical_actuators_stroke", LegacyValue::from("9223372036854775808"));
        });
        assert_eq!(
            thanos_settings(&wide, "THANOS", &tune, &runtime).native_compensation_stroke_mm.map(int_setting),
            Some(Ok(9_223_372_036_854_775_808))
        );
    }

    /// SimTech's connect gate reads the motion error flag, which starts false.
    #[test]
    fn simtech_reads_the_published_motion_error() {
        let settings = Settings::default();
        let tune = FactoryTunes::builtin().get("emulator").unwrap().clone();
        let telemetry = Telemetry::off();
        let runtime = |motion_error| DriverRuntime {
            game_running: "none",
            listener_running: "none",
            motion_test: false,
            motion_switch: true,
            motion_error,
            telemetry: &telemetry,
            simtech_tune: LegacyValue::from(tune.to_json()),
        };
        assert!(!super::super::MotionView::default().error);
        assert!(!simtech_settings(&settings, &runtime(false)).motion_error);
        assert!(simtech_settings(&settings, &runtime(true)).motion_error);
    }
}
