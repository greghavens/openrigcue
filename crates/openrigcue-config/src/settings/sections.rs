//! The `config.ini` sections, one typed struct each.
//!
//! Field names are openrigcue's own (they name `settings.toml` keys). Where it differs, the
//! `config.ini` key is kept as a `serde(alias)`, so a converted `config.ini` section deserializes
//! directly; `settings::tests::every_migrated_config_key_is_carried_over` checks that every
//! migrated key reaches a field.
//!
//! Defaults are the values a fresh installation ends with on the Windows platform (the
//! default): the template (`template.ini`) after every
//! migration step with the first-installation branches. The Linux first-run values
//! (`--original-platform linux`) come from [`crate::import::first_run_for`]. The step that sets
//! each value is noted as `vNNN` (`tpl` = template). The test
//! `settings::tests::defaults_equal_migrated_template` proves these literals against the chain.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Speed display unit (`APP.measurement_unit`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MeasurementUnit {
    #[serde(rename = "kmh", alias = "K")]
    KilometresPerHour,
    #[serde(rename = "mph", alias = "M")]
    MilesPerHour,
}

/// `[APP]`: application behaviour.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    /// Window close button quits instead of minimising (tpl yes).
    #[serde(alias = "x_button_exit")]
    pub close_button_quits: bool,
    /// Minimise after start (tpl no).
    pub start_minimized: bool,
    /// tpl yes; nothing reads it.
    #[serde(alias = "use_dark_theme")]
    pub dark_theme: bool,
    /// Pause the telemetry loop while no game runs (tpl yes).
    pub pause_telemetry_when_not_playing: bool,
    /// Root log level after load: 10 debug … 50 critical (tpl 30; v402 converts names).
    pub log_level: u32,
    /// App page "Shift Sound" switch (tpl no).
    #[serde(alias = "shift_sound")]
    pub gear_shift_sound: bool,
    /// v86.
    #[serde(alias = "measurement_unit")]
    pub speed_unit: MeasurementUnit,
    /// v87.
    pub window_width: u32,
    /// v87.
    pub window_height: u32,
    /// v177; −1 = centred.
    #[serde(alias = "window_x_pos")]
    pub window_x: i32,
    /// v177.
    #[serde(alias = "window_y_pos")]
    pub window_y: i32,
    /// v125: device pixel ratio reported by the page.
    #[serde(alias = "pixel_ratio")]
    pub device_pixel_ratio: f64,
    /// v89: show every telemetry channel in the telemetry table.
    #[serde(alias = "additional_telemetry")]
    pub show_all_telemetry_channels: bool,
    /// v90: suppress the "tuning disabled in Stand By" alert.
    #[serde(alias = "disable_tuning_alert_standby")]
    pub hide_standby_tuning_alert: bool,
    /// v91: tuning page shown at start.
    #[serde(alias = "startup_page")]
    pub start_page: String,
    /// v106 (moved from CUSTOM_FEATURES): web UI port.
    #[serde(alias = "webui_port")]
    pub web_ui_port: u16,
    /// v310: the host the app's own client URLs use (window, downloads). The server binds
    /// `0.0.0.0`.
    #[serde(alias = "webui_hostname")]
    pub web_ui_host: String,
    /// App page "Start at Login" (launcher flag `StartWithWindows`, `'1'` from v288 /
    /// v359): start OpenRigCue when the user logs in. Shown on the Windows platform only.
    pub start_with_login: bool,
    /// App page "Show Splash Screen" (launcher flag `ShowSplashScreen`, `'1'` from v288 /
    /// v359). Shown on the Windows platform only.
    pub show_splash_screen: bool,
    /// v153: do not hook the window `closed` event.
    #[serde(alias = "force_manual_quit")]
    pub manual_quit_only: bool,
    /// v248.
    pub voice_notifications: bool,
    /// v295: PlayStation address for GT7 / GT Sport.
    #[serde(alias = "playstation_ip")]
    pub playstation_address: String,
    /// v337: SDL2 joystick backend.
    #[serde(alias = "enable_sdl2")]
    pub sdl2_joysticks: bool,
    /// v358/v359: UI language.
    pub language: String,
    /// v367: the HapticR page, its menu entries and its hardware widget are shown only when
    /// `enable_hapticr` reads as true (v367 writes `'no'`).
    pub enable_hapticr: bool,
    /// v396: serial ports the device scanners may probe (empty = all). An INI list keeps its
    /// items; a plain string is one entry (the scanner does its own matching on it).
    #[serde(alias = "allowed_com_ports")]
    pub allowed_serial_ports: Vec<String>,
    /// The UI theme chosen under Settings → App → General: `redline` (the default), `apex` or `vector`. A
    /// missing, retired or unknown value shows Redline.
    pub theme: String,
}

impl Default for AppSettings {
    fn default() -> Self {
        AppSettings {
            close_button_quits: true,
            start_minimized: false,
            dark_theme: true,
            pause_telemetry_when_not_playing: true,
            log_level: 30,
            gear_shift_sound: false,
            speed_unit: MeasurementUnit::KilometresPerHour,
            window_width: 1280,
            window_height: 800,
            window_x: -1,
            window_y: -1,
            device_pixel_ratio: 1.0,
            show_all_telemetry_channels: false,
            hide_standby_tuning_alert: false,
            start_page: "Wind".into(),
            web_ui_port: 33733,
            web_ui_host: "localhost".into(),
            start_with_login: true,
            show_splash_screen: true,
            manual_quit_only: false,
            voice_notifications: false,
            playstation_address: String::new(),
            sdl2_joysticks: true,
            language: "en".into(),
            enable_hapticr: false,
            allowed_serial_ports: Vec::new(),
            theme: "redline".into(),
        }
    }
}

/// `[SHAKEBOX]`: bass shakers (sound-card driven).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShakerSettings {
    /// Output device name (tpl `None`; v230 sets the CM106 card on a Linux first installation).
    #[serde(alias = "soundcard")]
    pub sound_card: String,
    /// tpl 1.
    #[serde(alias = "nbr_shakers")]
    pub shaker_count: u32,
    /// Shaker loop rate in Hz (v106).
    pub refresh_rate: u32,
    pub side_enhancement: i64,
    pub reduction_factor: i64,
    pub min_side_enhancement: i64,
    pub min_suspension_velocity: i64,
    pub max_suspension_velocity: i64,
    pub min_bump_velocity: i64,
    pub max_bump_velocity: i64,
    /// Speaker layout name, a key of the shaker layouts (tpl; v230 sets `SRS UShake6` on a Linux first installation).
    #[serde(alias = "profile")]
    pub speaker_layout: String,
    #[serde(alias = "auto_refresh_soundcard")]
    pub auto_refresh_sound_card: bool,
    #[serde(alias = "legacy_sound_lib")]
    pub legacy_sound_library: bool,
    /// v170.
    #[serde(alias = "enable_test_effect")]
    pub test_effect_enabled: bool,
    /// v327.
    pub check_surround: bool,
    /// v327.
    #[serde(alias = "default_soundcard")]
    pub use_default_sound_card: bool,
    /// "Use SRS Sound Samples": play the shaker samples and the FMOD engine banks of the SRS
    /// installation or installer OpenRigCue was given, instead of the built-in sounds. An imported
    /// SRS `config.ini` has no such key; off by default.
    pub use_srs_sounds: bool,
}

impl Default for ShakerSettings {
    fn default() -> Self {
        ShakerSettings {
            sound_card: "None".into(),
            shaker_count: 1,
            refresh_rate: 27,
            side_enhancement: 1,
            reduction_factor: 1,
            min_side_enhancement: 2,
            min_suspension_velocity: 90,
            max_suspension_velocity: 100000,
            min_bump_velocity: 70,
            max_bump_velocity: 100000,
            speaker_layout: "1 Speaker - Center".into(),
            auto_refresh_sound_card: true,
            legacy_sound_library: false,
            test_effect_enabled: false,
            check_surround: true,
            use_default_sound_card: true,
            use_srs_sounds: false,
        }
    }
}

/// Fan kind (`INTELLIBOX.wind_type` and the tune key `wind_type`).
/// Stored as the `.srs` file codes because tunes are exchanged as `.srs` files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WindType {
    #[serde(rename = "F")]
    Fan,
    #[serde(rename = "B")]
    Blower,
    #[serde(rename = "T")]
    Turbo,
}

/// `[INTELLIBOX]`: wind (fans) controller.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindSettings {
    /// v106.
    pub refresh_rate: u32,
    pub auto_search: bool,
    #[serde(alias = "com_port")]
    pub serial_port: String,
    #[serde(alias = "timeout_secs")]
    pub timeout_seconds: u32,
    #[serde(alias = "wind_power_increment")]
    pub power_step: u32,
    /// v88: copied from the emulator tune.
    #[serde(alias = "wind_type")]
    pub fan_kind: WindType,
    /// v88: emulator `wind_curving + 1`.
    #[serde(alias = "nbr_fans")]
    pub fan_count: u32,
    /// v300.
    pub fan_frequency: u32,
}

impl Default for WindSettings {
    fn default() -> Self {
        WindSettings {
            refresh_rate: 7,
            auto_search: true,
            serial_port: String::new(),
            timeout_seconds: 5,
            power_step: 10,
            fan_kind: WindType::Fan,
            fan_count: 2,
            fan_frequency: 3,
        }
    }
}

/// `[MOTIONBOX]`: motion platform selection and global motion processing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MotionSettings {
    /// Platform model, a key of the motion model table (tpl `None`; v230 sets `H3` on a Linux first installation).
    pub model: String,
    pub soft_crash_factor: f64,
    pub soft_crash_enabled: bool,
    pub boost_multiplier: f64,
    /// Copied into every tune as `motion_reaction_speed` at v366.
    pub reaction_speed: i64,
    /// v104: motor index → axis name.
    #[serde(alias = "axis_assignment")]
    pub motor_axes: BTreeMap<String, String>,
    /// v230 (manufacturer default rate).
    pub refresh_rate: u32,
    /// v134.
    pub sway_curving_factor: f64,
    /// v134.
    pub surge_curving_factor: f64,
    /// v165.
    pub pitch_clip_factor: f64,
    pub roll_clip_factor: f64,
    pub yaw_clip_factor: f64,
    pub surge_clip_factor: f64,
    pub sway_clip_factor: f64,
    pub heave_clip_factor: f64,
    pub traction_loss_clip_factor: f64,
    /// v328.
    pub traction_loss_front_clip_factor: f64,
    /// v170.
    #[serde(alias = "intelli_tune_capture_secs")]
    pub intellitune_capture_seconds: u32,
    /// v221.
    pub auto_search: bool,
    /// v263.
    pub auto_smoothing_low_speed: bool,
    pub auto_smoothing_speed: f64,
    pub auto_smoothing_slider: f64,
    /// v269.
    #[serde(alias = "enable_debug")]
    pub debug: bool,
    /// v331.
    #[serde(alias = "min_degree_null_yaw_movement")]
    pub min_null_yaw_movement_degrees: f64,
    /// v349.
    pub gear_smoothing_min_delta: f64,
    /// v370/v371.
    pub overall_smoothing_map_min_slider: f64,
    pub overall_smoothing_map_max_slider: f64,
}

impl Default for MotionSettings {
    fn default() -> Self {
        MotionSettings {
            model: "None".into(),
            soft_crash_factor: 90.0,
            soft_crash_enabled: true,
            boost_multiplier: 2.0,
            reaction_speed: 100,
            motor_axes: [("0".to_owned(), "None".to_owned()), ("1".to_owned(), "None".to_owned())].into(),
            refresh_rate: 30,
            sway_curving_factor: 1.0,
            surge_curving_factor: 1.0,
            pitch_clip_factor: 1000.0,
            roll_clip_factor: 1000.0,
            yaw_clip_factor: 1000.0,
            surge_clip_factor: 1000.0,
            sway_clip_factor: 1000.0,
            heave_clip_factor: 1000.0,
            traction_loss_clip_factor: 1000.0,
            traction_loss_front_clip_factor: 1000.0,
            intellitune_capture_seconds: 180,
            auto_search: true,
            auto_smoothing_low_speed: false,
            auto_smoothing_speed: 1.66,
            auto_smoothing_slider: 60.0,
            debug: false,
            min_null_yaw_movement_degrees: 30.0,
            gear_smoothing_min_delta: 1.0,
            overall_smoothing_map_min_slider: 90.0,
            overall_smoothing_map_max_slider: 99.0,
        }
    }
}

/// `[DOF REALITY]`: SMC3 controllers (created at v105).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DofRealitySettings {
    #[serde(alias = "left_controller_com_port")]
    pub left_serial_port: String,
    #[serde(alias = "right_controller_com_port")]
    pub right_serial_port: String,
    pub left_debug_port: u16,
    pub right_debug_port: u16,
    /// v111 rescale, v374 = 15.
    pub smoothing_threshold: f64,
    pub soft_crash_threshold: f64,
    pub boost_threshold: f64,
    pub update_kp: bool,
    /// v139.
    pub real_heave: bool,
    /// v232.
    pub soft_start_stop_factor: f64,
    pub soft_start_stop_timer: f64,
    /// v376.
    #[serde(alias = "left_controller_resolution")]
    pub left_resolution: u32,
    #[serde(alias = "right_controller_resolution")]
    pub right_resolution: u32,
}

impl Default for DofRealitySettings {
    fn default() -> Self {
        DofRealitySettings {
            left_serial_port: String::new(),
            right_serial_port: String::new(),
            left_debug_port: 20017,
            right_debug_port: 20018,
            smoothing_threshold: 15.0,
            soft_crash_threshold: 50.0,
            boost_threshold: 15.0,
            update_kp: true,
            real_heave: false,
            soft_start_stop_factor: 30.0,
            soft_start_stop_timer: 4.0,
            left_resolution: 10,
            right_resolution: 10,
        }
    }
}

/// `[PROSIMU]` (created at v105).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProsimuSettings {
    pub smoothing_threshold: f64,
    pub soft_crash_threshold: f64,
    pub boost_threshold: f64,
    /// v113.
    #[serde(alias = "enable_dyatic_tl")]
    pub dyadic_traction_loss: bool,
    /// v114.
    pub prs200_firmware: u32,
    /// v122.
    #[serde(alias = "prs200_com_port1")]
    pub prs200_serial_port_1: String,
    #[serde(alias = "prs200_com_port2")]
    pub prs200_serial_port_2: String,
    pub real_heave: bool,
    pub soft_start_stop_factor: f64,
    pub soft_start_stop_timer: f64,
    /// v256.
    pub scn6_protocol: String,
    /// v268.
    #[serde(alias = "invert_tl")]
    pub invert_traction_loss: bool,
    /// v320.
    pub post_homing_speed: u32,
    pub post_homing_speed_timeout: f64,
    /// v350.
    #[serde(alias = "use_scn6_legacy")]
    pub scn6_legacy: bool,
}

impl Default for ProsimuSettings {
    fn default() -> Self {
        ProsimuSettings {
            smoothing_threshold: 0.01,
            soft_crash_threshold: 50.0,
            boost_threshold: 15.0,
            dyadic_traction_loss: true,
            prs200_firmware: 3,
            prs200_serial_port_1: String::new(),
            prs200_serial_port_2: String::new(),
            real_heave: false,
            soft_start_stop_factor: 90.0,
            soft_start_stop_timer: 4.0,
            scn6_protocol: "D".into(),
            invert_traction_loss: false,
            post_homing_speed: 500,
            post_homing_speed_timeout: 5.0,
            scn6_legacy: false,
        }
    }
}

/// `[Yaw VR]` (created at v112).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct YawVrSettings {
    pub smoothing_threshold: f64,
    pub soft_crash_threshold: f64,
    pub boost_threshold: f64,
    /// v124.
    pub auto_park: bool,
    /// v127.
    pub center_before_start_alert: bool,
    /// v130.
    pub udp_port: u16,
    /// v132.
    pub device_id: String,
    /// v137.
    pub disable_two_way_communication: bool,
    /// v138.
    pub vibration_power: u32,
    pub real_heave: bool,
    /// v179 (`6dof_mode`).
    #[serde(alias = "6dof_mode")]
    pub six_dof_mode: bool,
    pub soft_start_stop_factor: f64,
    pub soft_start_stop_timer: f64,
}

impl Default for YawVrSettings {
    fn default() -> Self {
        YawVrSettings {
            smoothing_threshold: 0.01,
            soft_crash_threshold: 50.0,
            boost_threshold: 15.0,
            auto_park: true,
            center_before_start_alert: true,
            udp_port: 33333,
            device_id: String::new(),
            disable_two_way_communication: false,
            vibration_power: 100,
            real_heave: false,
            six_dof_mode: true,
            soft_start_stop_factor: 90.0,
            soft_start_stop_timer: 4.0,
        }
    }
}

/// `[PT-ACTUATOR]` (created at v139).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PtActuatorSettings {
    /// v372.
    pub smoothing_threshold: f64,
    pub soft_crash_threshold: f64,
    pub boost_threshold: f64,
    pub reverse_controller_port_5: bool,
    pub reverse_controller_port_6: bool,
    pub reverse_controller_port_7: bool,
    /// v270.
    pub reverse_controller_port_8: bool,
    pub real_heave: bool,
    /// v147.
    #[serde(alias = "lateral_distance_actuators")]
    pub actuator_lateral_distance: f64,
    #[serde(alias = "vertical_actuators_stroke")]
    pub actuator_vertical_stroke: f64,
    pub soft_start_stop_factor: f64,
    pub soft_start_stop_timer: f64,
    /// v319: `host:port` targets of the UDP motion system.
    #[serde(alias = "udp_config")]
    pub udp_targets: Vec<String>,
    /// v339.
    #[serde(alias = "pt_com_port")]
    pub canbus_serial_port: String,
    /// v353.
    #[serde(alias = "pt_com_baud_rate")]
    pub canbus_baud_rate: u32,
    /// v355.
    pub parking_speed: u32,
    /// v355/v365.
    pub reaction_speed_map_min: u32,
    pub reaction_speed_map_max: u32,
    #[serde(alias = "park_to_motion_speed_delay_s")]
    pub park_to_motion_delay_seconds: f64,
    /// v412 (was `thanos_com_port`).
    #[serde(alias = "com_port")]
    pub thanos_serial_port: String,
    /// v412 (was `send_spike_filter`).
    #[serde(alias = "filter")]
    pub spike_filter: bool,
}

impl Default for PtActuatorSettings {
    fn default() -> Self {
        PtActuatorSettings {
            smoothing_threshold: 100000.0,
            soft_crash_threshold: 50.0,
            boost_threshold: 5.0,
            reverse_controller_port_5: false,
            reverse_controller_port_6: false,
            reverse_controller_port_7: false,
            reverse_controller_port_8: false,
            real_heave: true,
            actuator_lateral_distance: 0.0,
            actuator_vertical_stroke: 0.0,
            soft_start_stop_factor: 90.0,
            soft_start_stop_timer: 4.0,
            udp_targets: vec!["192.168.1.1:7408".into()],
            canbus_serial_port: String::new(),
            canbus_baud_rate: 115200,
            parking_speed: 1000,
            reaction_speed_map_min: 1000,
            reaction_speed_map_max: 7000,
            park_to_motion_delay_seconds: 5.0,
            thanos_serial_port: String::new(),
            spike_filter: true,
        }
    }
}

/// `[THANOS]` (v235 copy of PT-ACTUATOR, v391/v412 controller keys).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThanosSettings {
    pub smoothing_threshold: f64,
    pub soft_crash_threshold: f64,
    pub boost_threshold: f64,
    pub reverse_controller_port_5: bool,
    pub reverse_controller_port_6: bool,
    pub reverse_controller_port_7: bool,
    pub reverse_controller_port_8: bool,
    pub real_heave: bool,
    #[serde(alias = "lateral_distance_actuators")]
    pub actuator_lateral_distance: f64,
    #[serde(alias = "vertical_actuators_stroke")]
    pub actuator_vertical_stroke: f64,
    pub soft_start_stop_factor: f64,
    pub soft_start_stop_timer: f64,
    #[serde(alias = "com_port")]
    pub serial_port: String,
    #[serde(alias = "filter")]
    pub spike_filter: bool,
}

impl Default for ThanosSettings {
    fn default() -> Self {
        ThanosSettings {
            smoothing_threshold: 0.01,
            soft_crash_threshold: 50.0,
            boost_threshold: 5.0,
            reverse_controller_port_5: false,
            reverse_controller_port_6: false,
            reverse_controller_port_7: false,
            reverse_controller_port_8: false,
            real_heave: true,
            actuator_lateral_distance: 0.0,
            actuator_vertical_stroke: 0.0,
            soft_start_stop_factor: 90.0,
            soft_start_stop_timer: 4.0,
            serial_port: String::new(),
            spike_filter: true,
        }
    }
}

/// `[VNM SIMULATION]` (v335/v336).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VnmSettings {
    #[serde(alias = "vnm_com_port")]
    pub serial_port: String,
    pub smoothing_threshold: f64,
    pub soft_crash_threshold: f64,
    pub boost_threshold: f64,
    pub reverse_controller_port_5: bool,
    pub reverse_controller_port_6: bool,
    pub reverse_controller_port_7: bool,
    pub reverse_controller_port_8: bool,
    pub reverse_controller_port_9: bool,
    pub real_heave: bool,
    #[serde(alias = "lateral_distance_actuators")]
    pub actuator_lateral_distance: f64,
    #[serde(alias = "vertical_actuators_stroke")]
    pub actuator_vertical_stroke: f64,
    pub soft_start_stop_factor: f64,
    pub soft_start_stop_timer: f64,
}

impl Default for VnmSettings {
    fn default() -> Self {
        VnmSettings {
            serial_port: String::new(),
            smoothing_threshold: 0.01,
            soft_crash_threshold: 50.0,
            boost_threshold: 5.0,
            reverse_controller_port_5: false,
            reverse_controller_port_6: false,
            reverse_controller_port_7: false,
            reverse_controller_port_8: false,
            reverse_controller_port_9: false,
            real_heave: true,
            actuator_lateral_distance: 0.0,
            actuator_vertical_stroke: 0.0,
            soft_start_stop_factor: 90.0,
            soft_start_stop_timer: 4.0,
        }
    }
}

/// `[SIMTECHREVOLUTION]` (v275 copy of PROSIMU without the PRS200 keys).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SimtechSettings {
    pub smoothing_threshold: f64,
    pub soft_crash_threshold: f64,
    pub boost_threshold: f64,
    #[serde(alias = "enable_dyatic_tl")]
    pub dyadic_traction_loss: bool,
    pub real_heave: bool,
    pub soft_start_stop_factor: f64,
    pub soft_start_stop_timer: f64,
    pub scn6_protocol: String,
    #[serde(alias = "invert_tl")]
    pub invert_traction_loss: bool,
    /// v315.
    #[serde(alias = "tl_act_speed")]
    pub traction_loss_actuator_speed: u32,
    #[serde(alias = "tl_act_accel")]
    pub traction_loss_actuator_acceleration: u32,
    /// v346.
    pub post_homing_speed: u32,
    pub post_homing_speed_timeout: f64,
    pub post_homing_acceleration: u32,
}

impl Default for SimtechSettings {
    fn default() -> Self {
        SimtechSettings {
            smoothing_threshold: 0.01,
            soft_crash_threshold: 50.0,
            boost_threshold: 15.0,
            dyadic_traction_loss: true,
            real_heave: false,
            soft_start_stop_factor: 90.0,
            soft_start_stop_timer: 4.0,
            scn6_protocol: "D".into(),
            invert_traction_loss: false,
            traction_loss_actuator_speed: 5000,
            traction_loss_actuator_acceleration: 80,
            post_homing_speed: 800,
            post_homing_speed_timeout: 15.0,
            post_homing_acceleration: 100,
        }
    }
}

/// `[NETWORK_PORT]`: UDP port of each listener, keyed by listener id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NetworkPorts(BTreeMap<String, u16>);

/// Template ports (tpl) and the ports added by migrations (vNNN), first installation (dirt v213).
const DEFAULT_PORTS: [(&str, u16); 53] = [
    ("dirt", 30778),
    ("f1", 20778),
    ("f12018", 20779),
    ("f12019", 20780),
    ("xplane", 49000),
    ("dcs", 9800),
    ("nl2", 15151),
    ("warthunder", 8111),
    ("forza", 10001),
    ("api", 33001),
    ("motogp", 7100),
    ("lfs", 30000),
    ("kartkraft", 5000),
    ("p3d", 33445),
    ("condor", 55278),
    ("touringkarts", 4123),
    ("projectcarsudp", 5606),
    ("projectcars2udp", 5606),
    ("il2", 4321),
    ("grid", 20781),
    ("fsx", 33445),
    ("flyinside", 31234),
    ("silentwings", 6060),
    ("beamng", 4444),
    ("epic", 7701),
    ("joy", 33002),
    ("ironrebellion", 11631),
    ("rbr", 6776),
    ("thecrew", 1337),
    ("gtav", 20777),
    ("vracer", 8051),
    ("aeroflyfs2", 4123),
    ("f12020", 20782),
    ("bussim", 37337),
    ("driftphysicscrew", 20782),
    ("hyperstorm", 20741),
    ("redrover", 3001),
    ("acudp", 9997),
    ("f12021", 20783),
    ("aceshigh", 556),
    ("f12022", 20784),
    ("vector36", 4123),
    ("gt7", 33740),
    ("gtsport", 33340),
    ("wrc", 30779),
    ("f12023", 20785),
    ("wrc2023", 20999),
    ("f12024", 20786),
    ("flyinside_fsx", 31235),
    ("f12025", 20787),
    ("wreckfest2", 23123),
    ("pmracing", 7576),
    ("tdusc", 31123),
];

impl Default for NetworkPorts {
    fn default() -> Self {
        NetworkPorts(DEFAULT_PORTS.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect())
    }
}

impl NetworkPorts {
    /// The UDP port of `listener`.
    pub fn get(&self, listener: &str) -> Option<u16> {
        self.0.get(listener).copied()
    }

    pub fn set(&mut self, listener: &str, port: u16) {
        self.0.insert(listener.to_owned(), port);
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, u16)> {
        self.0.iter().map(|(k, v)| (k.as_str(), *v))
    }
}

/// `[CUSTOM_FEATURES]`: feature switches and listener-specific tweaks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CustomFeatures {
    #[serde(alias = "ed_invert_yaw")]
    pub elite_dangerous_invert_yaw: bool,
    #[serde(alias = "ed_invert_pitch")]
    pub elite_dangerous_invert_pitch: bool,
    #[serde(alias = "ed_invert_roll")]
    pub elite_dangerous_invert_roll: bool,
    pub xplane_invert_yaw: bool,
    pub xplane_invert_pitch: bool,
    pub xplane_invert_roll: bool,
    pub fsx_invert_yaw: bool,
    pub fsx_invert_pitch: bool,
    pub fsx_invert_roll: bool,
    /// v84.
    #[serde(alias = "p3d_invert_yaw")]
    pub prepar3d_invert_yaw: bool,
    #[serde(alias = "p3d_invert_pitch")]
    pub prepar3d_invert_pitch: bool,
    #[serde(alias = "p3d_invert_roll")]
    pub prepar3d_invert_roll: bool,
    /// v129.
    pub tuner_mode: bool,
    /// v172.
    #[serde(alias = "check_ed_updates")]
    pub check_elite_dangerous_updates: bool,
    /// v182.
    #[serde(alias = "enable_dash_telemetry")]
    pub dash_telemetry: bool,
    /// v216: hide sliders of effects a game lacks (game features table).
    pub hide_effects_not_in_game: bool,
    /// v224.
    pub smoothing_slider_step: f64,
    /// v226.
    pub show_debug_page: bool,
    /// v236.
    #[serde(alias = "surge_when_pitch_joy")]
    pub surge_when_pitch_joystick: bool,
    /// v243.
    #[serde(alias = "sway_when_roll_joy")]
    pub sway_when_roll_joystick: bool,
    /// v261: tuning pages stay editable in Stand By.
    #[serde(alias = "allow_edit_emulator")]
    pub standby_tuning_editable: bool,
    /// v264. Unused: OpenRigCue lists every model. Kept so
    /// imported SRS configs round-trip.
    #[serde(alias = "enable_dyadic_surge")]
    pub dyadic_surge: bool,
    /// v265.
    #[serde(alias = "enable_legacy_gear_smoothing")]
    pub legacy_gear_smoothing: bool,
    /// v306: console GT games (`yes` off Windows).
    #[serde(alias = "enable_gt")]
    pub gran_turismo: bool,
    /// v317.
    #[serde(alias = "enable_routines")]
    pub routines: bool,
    /// v329.
    #[serde(alias = "dams_front_tl_mode")]
    pub dams_front_traction_loss_mode: u32,
    #[serde(alias = "dams_front_tl_factor")]
    pub dams_front_traction_loss_factor: f64,
    /// v331.
    #[serde(alias = "dams_front_tl_a")]
    pub dams_front_traction_loss_a: f64,
    #[serde(alias = "dams_rear_tl_b")]
    pub dams_rear_traction_loss_b: f64,
    /// Read by the listener dispatcher with a fallback of 0 when missing; no step creates it.
    #[serde(alias = "dams_rear_tl_a")]
    pub dams_rear_traction_loss_a: Option<f64>,
    /// v332.
    #[serde(alias = "dams_front_steerratio")]
    pub dams_front_steering_ratio: f64,
    /// v333.
    #[serde(alias = "dams_tl_max_auto_smoothing")]
    pub dams_traction_loss_max_auto_smoothing: f64,
    #[serde(alias = "dams_tl_auto_smoothing_max_speed_kmh")]
    pub dams_traction_loss_auto_smoothing_max_speed_kmh: f64,
    /// v375.
    #[serde(alias = "enable_flyinside_overwrite")]
    pub flyinside_overwrite: bool,
    #[serde(alias = "rollback_xplane_latg")]
    pub rollback_xplane_lateral_g: bool,
    /// v378.
    pub disable_rfpro: bool,
    /// v379.
    #[serde(alias = "enable_smc3_ymotor")]
    pub smc3_y_motor: bool,
    /// v382.
    #[serde(alias = "enable_mc_telemetry")]
    pub mc_telemetry: bool,
    /// v393.
    #[serde(alias = "enable_motion_measurement_mode")]
    pub motion_measurement_mode: bool,
    /// tpl yes, v406 no, v409 yes.
    #[serde(alias = "enable_event_driven_device_scan")]
    pub event_driven_device_scan: bool,
}

impl Default for CustomFeatures {
    fn default() -> Self {
        CustomFeatures {
            elite_dangerous_invert_yaw: false,
            elite_dangerous_invert_pitch: false,
            elite_dangerous_invert_roll: false,
            xplane_invert_yaw: false,
            xplane_invert_pitch: false,
            xplane_invert_roll: false,
            fsx_invert_yaw: false,
            fsx_invert_pitch: false,
            fsx_invert_roll: false,
            prepar3d_invert_yaw: false,
            prepar3d_invert_pitch: false,
            prepar3d_invert_roll: false,
            tuner_mode: false,
            check_elite_dangerous_updates: true,
            dash_telemetry: false,
            hide_effects_not_in_game: false,
            smoothing_slider_step: 1.0,
            show_debug_page: false,
            surge_when_pitch_joystick: false,
            sway_when_roll_joystick: false,
            standby_tuning_editable: false,
            dyadic_surge: false,
            legacy_gear_smoothing: false,
            gran_turismo: false,
            routines: false,
            dams_front_traction_loss_mode: 0,
            dams_front_traction_loss_factor: 1.0,
            dams_front_traction_loss_a: 1.6929,
            dams_rear_traction_loss_b: 1.44,
            dams_rear_traction_loss_a: None,
            dams_front_steering_ratio: 14.5,
            dams_traction_loss_max_auto_smoothing: 80.0,
            dams_traction_loss_auto_smoothing_max_speed_kmh: 80.0,
            flyinside_overwrite: false,
            rollback_xplane_lateral_g: false,
            disable_rfpro: false,
            smc3_y_motor: false,
            mc_telemetry: false,
            motion_measurement_mode: false,
            event_driven_device_scan: true,
        }
    }
}

/// One axis of the motion compensation sensor (`MOTION_COMPENSATION.<axis>`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompensationAxis {
    pub offset: f64,
    pub enabled: bool,
    pub reverse: bool,
}

impl Default for CompensationAxis {
    fn default() -> Self {
        CompensationAxis { offset: 0.0, enabled: true, reverse: false }
    }
}

/// The compensation sensor kind (`MOTION_COMPENSATION.type`, v354).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompensationSensor {
    /// IntelliComp virtual sensor.
    #[serde(rename = "virtual", alias = "V")]
    Virtual,
    /// WITMotion IMU.
    #[serde(rename = "witmotion", alias = "W")]
    Witmotion,
}

/// `[MOTION_COMPENSATION]`: VR motion compensation (v180+).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MotionCompensationSettings {
    /// v390: disabled.
    pub yaw: CompensationAxis,
    pub roll: CompensationAxis,
    pub pitch: CompensationAxis,
    /// v383: disabled.
    pub heave: CompensationAxis,
    /// v354: reversed for the virtual sensor.
    pub surge: CompensationAxis,
    pub sway: CompensationAxis,
    /// v181.
    pub refresh_rate: u32,
    #[serde(alias = "com_port")]
    pub serial_port: String,
    /// v187.
    pub height: f64,
    /// v184.
    pub swap_roll_pitch: bool,
    pub installation: String,
    /// v185.
    pub auto_search: bool,
    /// v186.
    pub sway_factor: f64,
    pub surge_factor: f64,
    /// v252.
    pub surge_horizontal_factor: f64,
    pub surge_horizontal_stroke: f64,
    /// v254.
    pub surge_horizontal_reverse: bool,
    /// v354.
    #[serde(alias = "type")]
    pub sensor: CompensationSensor,
    #[serde(alias = "mm_distance_left_right")]
    pub left_right_distance_mm: f64,
    #[serde(alias = "mm_distance_front_back")]
    pub front_back_distance_mm: f64,
    #[serde(alias = "vertical_actuators_stroke")]
    pub actuator_vertical_stroke: f64,
    /// v395.
    pub roll_factor: f64,
    pub pitch_factor: f64,
    pub yaw_factor: f64,
}

impl Default for MotionCompensationSettings {
    fn default() -> Self {
        let disabled = CompensationAxis { enabled: false, ..CompensationAxis::default() };
        MotionCompensationSettings {
            yaw: disabled.clone(),
            roll: CompensationAxis::default(),
            pitch: CompensationAxis::default(),
            heave: disabled,
            surge: CompensationAxis { reverse: true, ..CompensationAxis::default() },
            sway: CompensationAxis::default(),
            refresh_rate: 90,
            serial_port: String::new(),
            height: 1000.0,
            swap_roll_pitch: false,
            installation: "H".into(),
            auto_search: true,
            sway_factor: 100.0,
            surge_factor: 100.0,
            surge_horizontal_factor: 0.0,
            surge_horizontal_stroke: 100.0,
            surge_horizontal_reverse: false,
            sensor: CompensationSensor::Virtual,
            left_right_distance_mm: 558.0,
            front_back_distance_mm: 1270.0,
            actuator_vertical_stroke: 150.0,
            roll_factor: 100.0,
            pitch_factor: 100.0,
            yaw_factor: 100.0,
        }
    }
}

/// `[BELT_TENSIONER]` (v229+).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BeltSettings {
    pub refresh_rate: u32,
    /// v408: off on Linux (`--original-platform linux`).
    pub auto_search: bool,
    #[serde(alias = "com_port")]
    pub serial_port: String,
    /// v246/v247.
    #[serde(alias = "belt_calibration_factor")]
    pub calibration_factor: f64,
    /// v259.
    #[serde(alias = "belt_calibration_retraction")]
    pub calibration_retraction: f64,
}

impl Default for BeltSettings {
    fn default() -> Self {
        BeltSettings {
            refresh_rate: 90,
            auto_search: true,
            serial_port: String::new(),
            calibration_factor: 50.0,
            calibration_retraction: 50.0,
        }
    }
}

/// `[HUD]` (v266).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HudSettings {
    pub refresh_rate: u32,
    pub auto_search: bool,
    /// `H` (HUD) or an OBD2 adapter name.
    pub model: String,
}

impl Default for HudSettings {
    fn default() -> Self {
        HudSettings { refresh_rate: 5, auto_search: false, model: "H".into() }
    }
}

/// `[IFLAG]` (v273/v274/v294).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct IflagSettings {
    pub refresh_rate: u32,
    pub auto_search: bool,
    #[serde(alias = "com_port")]
    pub serial_port: String,
    /// Degrees: 0 Down, 90 Left, 180 Up, 270 Right.
    #[serde(alias = "rotation")]
    pub rotation_degrees: u32,
    pub brightness: u32,
    pub mirror: bool,
}

impl Default for IflagSettings {
    fn default() -> Self {
        IflagSettings {
            refresh_rate: 90,
            auto_search: true,
            serial_port: String::new(),
            rotation_degrees: 0,
            brightness: 10,
            mirror: false,
        }
    }
}

/// `[SIM3D]` (v277/v278).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Sim3dSettings {
    pub refresh_rate: u32,
    pub auto_search: bool,
    #[serde(alias = "com_port")]
    pub serial_port: String,
    /// `None`, `3 Ports` or `4 Ports`.
    pub model: String,
    /// Rumble group label per port.
    pub port1: String,
    pub port2: String,
    pub port3: String,
    pub port4: String,
}

impl Default for Sim3dSettings {
    fn default() -> Self {
        Sim3dSettings {
            refresh_rate: 90,
            auto_search: true,
            serial_port: String::new(),
            model: "None".into(),
            port1: "None".into(),
            port2: "None".into(),
            port3: "None".into(),
            port4: "None".into(),
        }
    }
}

/// `[HAPTICR]` (v311).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HapticrSettings {
    pub refresh_rate: u32,
    pub auto_search: bool,
    #[serde(alias = "com_port")]
    pub serial_port: String,
    pub model: String,
}

impl Default for HapticrSettings {
    fn default() -> Self {
        HapticrSettings { refresh_rate: 90, auto_search: true, serial_port: String::new(), model: "None".into() }
    }
}

/// `[MAGICBOX]` (v293).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MagicboxSettings {
    #[serde(alias = "magic_wheel_scan_code")]
    pub wheel_scan_code: String,
}

/// `[SYMPROJECTS]` (v298).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SymprojectsSettings {
    pub refresh_rate: u32,
    pub auto_search: bool,
    pub gi_max_brightness: u32,
}

impl Default for SymprojectsSettings {
    fn default() -> Self {
        SymprojectsSettings { refresh_rate: 90, auto_search: true, gi_max_brightness: 100 }
    }
}
