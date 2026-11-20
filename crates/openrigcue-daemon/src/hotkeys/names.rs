//! The hotkey actions as the shortcut editor lists them: labels
//! and the action list of every page's shortcut button.

use openrigcue_config::catalog::registry::Registry;
use serde::Serialize;

/// The fixed labels, in editor order (trailing spaces included).
const LABELS: &[(&str, &str)] = &[
    ("power_wind_switch", "Cycle Wind L/R Mode"),
    ("power_wind_up", "Increase Wind L/R Speed"),
    ("power_wind_down", "Decrease Wind L/R Speed"),
    ("power_center_wind_switch", "Cycle Wind Center Mode"),
    ("power_center_wind_up", "Increase Wind Center Speed"),
    ("power_center_wind_down", "Decrease Wind Center Speed"),
    ("shake_box_switch", "Toggle Shakebox"),
    ("joystick_switch", "Toggle Joystick"),
    ("led_switch", "Cycle LED Mode"),
    ("motion_compensation_switch", "Toggle Motion Compensation"),
    ("motion_compensation_yaw_calibrate", "Calibrate Motion Compensation Yaw"),
    ("motion_compensation_roll_calibrate", "Calibrate Motion Compensation Roll"),
    ("motion_compensation_pitch_calibrate", "Calibrate Motion Compensation Pitch"),
    ("motion_compensation_all_calibrate", "Calibrate All Motion Compensation Effects"),
    ("motion_compensation_yaw_switch", "Toggle Motion Compensation Yaw"),
    ("motion_compensation_roll_switch", "Toggle Motion Compensation Roll"),
    ("motion_compensation_pitch_switch", "Toggle Motion Compensation Pitch"),
    ("motion_compensation_heave_switch", "Toggle Motion Compensation Heave"),
    ("motion_compensation_surge_switch", "Toggle Motion Compensation Surge"),
    ("motion_compensation_sway_switch", "Toggle Motion Compensation Sway"),
    ("belt_switch", "Toggle Belt"),
    ("motion_box_switch", "Toggle Motion"),
    ("motion_box_homing", "Center the Motion Platform"),
    ("belt_homing", "Calibrate Belt"),
    ("telemetry_reset", "Reset Telemetry"),
    ("telemetry_capture", "Toggle Telemetry Capture"),
    ("telemetry_snapshot", "Save CSV Snapshot"),
    ("telemetry_export", "Export Telemetry"),
    ("shake_box_engine_up", "Increase Engine "),
    ("shake_box_engine_down", "Decrease Engine "),
    ("shake_box_gear_up", "Increase Gear "),
    ("shake_box_gear_down", "Decrease Gear "),
    ("shake_box_suspension_up", "Increase Suspension "),
    ("shake_box_suspension_down", "Decrease Suspension "),
    ("shake_box_road_bump_up", "Increase Vert G "),
    ("shake_box_road_bump_down", "Decrease Vert G "),
    ("shake_box_impact_up", "Increase Impact "),
    ("shake_box_impact_down", "Decrease Impact "),
    ("shake_box_acceleration_up", "Increase Long G Acceleration "),
    ("shake_box_acceleration_down", "Decrease Long G Acceleration "),
    ("shake_box_deceleration_up", "Increase Long G Deceleration "),
    ("shake_box_deceleration_down", "Decrease Long G Deceleration "),
    ("shake_box_lateral_acceleration_up", "Increase Lateral G "),
    ("shake_box_lateral_acceleration_down", "Decrease Lateral G "),
    ("shake_box_traction_loss_up", "Increase Traction Loss "),
    ("shake_box_traction_loss_down", "Decrease Traction Loss "),
    ("shake_box_wheel_slip_up", "Increase Wheel Slip "),
    ("shake_box_wheel_slip_down", "Decrease Wheel Slip "),
    ("shake_box_afterburner_up", "Increase Afterburner "),
    ("shake_box_afterburner_down", "Decrease Afterburner "),
    ("shake_box_cannon_firing_up", "Increase Cannon Firing "),
    ("shake_box_cannon_firing_down", "Decrease Cannon Firing "),
    ("shake_box_canopy_up", "Increase Canopy "),
    ("shake_box_canopy_down", "Decrease Canopy "),
    ("shake_box_abs_lock_up", "Increase ABS Lock "),
    ("shake_box_abs_lock_down", "Decrease ABS Lock "),
    ("shake_box_landing_gear_up", "Increase Landing Gear "),
    ("shake_box_landing_gear_down", "Decrease Landing Gear "),
    ("shake_box_missile_firing_up", "Increase Missile Firing "),
    ("shake_box_missile_firing_down", "Decrease Missile Firing "),
    ("belt_surge_up", "Increase Surge   "),
    ("belt_surge_down", "Decrease Surge   "),
    ("belt_surge_smoothing_up", "Increase Surge Smoothing   "),
    ("belt_surge_smoothing_down", "Decrease Surge Smoothing   "),
    ("belt_heave_up", "Increase Heave   "),
    ("belt_heave_down", "Decrease Heave   "),
    ("belt_heave_smoothing_up", "Increase Heave Smoothing   "),
    ("belt_heave_smoothing_down", "Decrease Heave Smoothing   "),
    ("belt_sway_curving_up", "Increase Sway   "),
    ("belt_sway_curving_down", "Decrease Sway   "),
    ("belt_sway_smoothing_up", "Increase Sway Smoothing   "),
    ("belt_sway_smoothing_down", "Decrease Sway Smoothing   "),
    ("motion_box_pitch_up", "Increase Pitch"),
    ("motion_box_pitch_down", "Decrease Pitch"),
    ("motion_box_roll_up", "Increase Roll"),
    ("motion_box_roll_down", "Decrease Roll"),
    ("motion_box_yaw_up", "Increase Yaw Rear"),
    ("motion_box_yaw_down", "Decrease Yaw Rear"),
    ("motion_box_sway_up", "Increase Lat G Sway"),
    ("motion_box_sway_down", "Decrease Lat G Sway"),
    ("motion_box_surge_acceleration_up", "Increase Long G Surge Accel"),
    ("motion_box_surge_acceleration_down", "Decrease Long G Surge Accel"),
    ("motion_box_surge_deceleration_up", "Increase Long G Surge Decel"),
    ("motion_box_surge_deceleration_down", "Decrease Long G Surge Decel"),
    ("motion_box_heave_up", "Increase Vert G Heave"),
    ("motion_box_heave_down", "Decrease Vert G Heave"),
    ("motion_box_traction_loss_up", "Increase Traction Loss"),
    ("motion_box_traction_loss_down", "Decrease Traction Loss"),
    ("motion_box_traction_loss_front_up", "Increase Traction Loss Front"),
    ("motion_box_traction_loss_front_down", "Decrease Traction Loss Front"),
    ("motion_box_wheel_slip_up", "Increase Wheel Slip / Lock"),
    ("motion_box_wheel_slip_down", "Decrease Wheel Slip / Lock"),
    ("motion_box_rumble_strips_up", "Increase Rumble Strips"),
    ("motion_box_rumble_strips_down", "Decrease Rumble Strips"),
    ("motion_box_engine_up", "Increase Engine"),
    ("motion_box_engine_down", "Decrease Engine"),
    ("motion_box_afterburner_up", "Increase Afterburner"),
    ("motion_box_afterburner_down", "Decrease Afterburner"),
    ("motion_box_cannon_firing_up", "Increase Cannon Firing"),
    ("motion_box_cannon_firing_down", "Decrease Cannon Firing"),
    ("motion_box_canopy_up", "Increase Canopy"),
    ("motion_box_canopy_down", "Decrease Canopy"),
    ("motion_box_abs_lock_up", "Increase ABS Lock"),
    ("motion_box_abs_lock_down", "Decrease ABS Lock"),
    ("motion_box_landing_gear_up", "Increase Landing Gear"),
    ("motion_box_landing_gear_down", "Decrease Landing Gear"),
    ("motion_box_missile_firing_up", "Increase Missile Firing"),
    ("motion_box_missile_firing_down", "Decrease Missile Firing"),
    ("motion_box_motion_max_power_up", "Increase Max Angle"),
    ("motion_box_motion_max_power_down", "Decrease Max Angle"),
    ("motion_box_motion_max_horizontal_up", "Increase Max Horizontal"),
    ("motion_box_motion_max_horizontal_down", "Decrease Max Horizontal"),
    ("motion_box_motion_smoothing_factor_up", "Increase Overall Smoothing"),
    ("motion_box_motion_smoothing_factor_down", "Decrease Overall Smoothing"),
    ("motion_box_reaction_speed_up", "Increase Reaction Speed"),
    ("motion_box_reaction_speed_down", "Decrease Reaction Speed"),
    ("motion_box_motion_boost_factor_up", "Increase Boost"),
    ("motion_box_motion_boost_factor_down", "Decrease Boost"),
    ("motion_box_smoothing_pitch_up", "Increase Smoothing of Pitch"),
    ("motion_box_smoothing_pitch_down", "Decrease Smoothing of Pitch"),
    ("motion_box_smoothing_roll_up", "Increase Smoothing of Roll"),
    ("motion_box_smoothing_roll_down", "Decrease Smoothing of Roll"),
    ("motion_box_smoothing_yaw_up", "Increase Smoothing of Yaw"),
    ("motion_box_smoothing_yaw_down", "Decrease Smoothing of Yaw"),
    ("motion_box_smoothing_sway_up", "Increase Smoothing of Sway"),
    ("motion_box_smoothing_sway_down", "Decrease Smoothing of Sway"),
    ("motion_box_smoothing_heave_up", "Increase Smoothing of Heave"),
    ("motion_box_smoothing_heave_down", "Decrease Smoothing of Heave"),
    ("motion_box_smoothing_traction_loss_up", "Increase Smoothing of Traction Loss"),
    ("motion_box_smoothing_traction_loss_down", "Decrease Smoothing of Traction Loss"),
    ("motion_box_smoothing_traction_loss_front_up", "Increase Smoothing of Traction Loss Front"),
    ("motion_box_smoothing_traction_loss_front_down", "Decrease Smoothing of Traction Loss Front"),
    ("motion_box_smoothing_surge_up", "Increase Smoothing of Surge"),
    ("motion_box_smoothing_surge_down", "Decrease Smoothing of Surge"),
    ("cannon_firing", "Cannon Firing"),
    ("ejection_seat", "Ejection Seat"),
    ("missile_firing", "Missile Firing"),
    ("rumble_switch", "Toggle Rumble"),
    ("haptic_switch", "Toggle Haptic"),
    ("shake_box_touchdown_up", "Increase Shakebox Touchdown "),
    ("shake_box_touchdown_down", "Decrease Shakebox Touchdown "),
    ("shake_box_turbulence_up", "Increase Shakebox Turbulence "),
    ("shake_box_turbulence_down", "Decrease Shakebox Turbulence "),
    // The VR re-centre action of the compensation unit.
    ("motion_compensation_recenter", "Recenter Motion Compensation"),
];

/// Title case: a cased character is upper-cased after an uncased one and lower-cased otherwise.
pub fn py_title(text: &str) -> String {
    let mut previous_cased = false;
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        let cased = c.is_uppercase() || c.is_lowercase();
        if cased && !previous_cased {
            out.extend(c.to_uppercase());
        } else if cased {
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
        previous_cased = cased;
    }
    out
}

/// The label the editor shows for `action`: the fixed table, then the rumble and haptic rules
/// (underscores become spaces, the word `rumble` or `haptic` is removed, then title case). `None` for an action
/// without a label.
pub fn label(action: &str) -> Option<String> {
    if let Some((_, text)) = LABELS.iter().find(|(name, _)| *name == action) {
        return Some((*text).to_owned());
    }
    let registry = Registry::builtin();
    for (prefix, effects) in [("rumble", &registry.rumble_effects), ("haptic", &registry.haptic_effects)] {
        for up_down in ["up", "down"] {
            for effect in effects.iter() {
                if action == format!("{prefix}_{}_{up_down}", effect.id) {
                    return Some(py_title(&action.replace('_', " ").replace(prefix, "")));
                }
            }
        }
    }
    None
}

/// One page's shortcut list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShortcutGroup {
    /// The left-menu page the shortcut button sits on.
    pub page: &'static str,
    pub actions: Vec<String>,
}

fn up_down(prefix: &str, keys: impl IntoIterator<Item = String>) -> Vec<String> {
    keys.into_iter().flat_map(|key| [format!("{prefix}{key}_up"), format!("{prefix}{key}_down")]).collect()
}

/// The axes of the Motion Compensation page (every axis, without the `all` entry), in its order.
pub const COMPENSATION_AXES: [&str; 6] = ["pitch", "roll", "yaw", "heave", "sway", "surge"];

/// The Motion page's platform sliders that have hotkeys (non-`pta` brand).
pub const MOTION_PLATFORM_KEYS: [&str; 5] =
    ["motion_max_power", "motion_max_horizontal", "motion_smoothing_factor", "reaction_speed", "motion_boost_factor"];

/// The Motion page's smoothing rows with hotkeys (`motion_box_smoothing_<axis>`), in the main loop's order.
pub const SMOOTHING_KEYS: [&str; 8] =
    ["pitch", "roll", "yaw", "sway", "surge", "heave", "traction_loss", "traction_loss_front"];

/// The Belt page's hotkey sliders, in the main loop's order.
pub const BELT_KEYS: [&str; 6] = [
    "belt_heave",
    "belt_heave_smoothing",
    "belt_surge",
    "belt_surge_smoothing",
    "belt_sway_curving",
    "belt_sway_smoothing",
];

/// Every page's shortcut list in menu order: the header button's list, then the per-row buttons (Wind's speed
/// rows and Ejection Seat, the canned-effect rows of Shake and Motion). The motion-compensation list ends
/// with OpenRigCue's re-centre action.
pub fn groups() -> Vec<ShortcutGroup> {
    let registry = Registry::builtin();
    let ids = |effects: &[openrigcue_config::catalog::registry::GroupedEffectInfo], prefix: &str| -> Vec<String> {
        ["up", "down"]
            .iter()
            .flat_map(|direction| effects.iter().map(move |e| format!("{prefix}{}_{direction}", e.id)))
            .collect()
    };
    let mut shake = vec!["shake_box_switch".to_owned()];
    shake.extend(up_down("shake_box_", registry.shaker_effects.iter().map(|e| e.id.clone())));
    shake.extend(registry.shaker_effects.iter().filter(|e| e.shortcut).map(|e| e.id.clone()));
    let mut motion: Vec<String> =
        ["motion_box_switch", "motion_compensation_switch", "motion_box_homing"].map(str::to_owned).to_vec();
    motion.extend(up_down("motion_box_", registry.motion_effects.iter().map(|e| e.id.clone())));
    motion.extend(up_down("motion_box_", MOTION_PLATFORM_KEYS.map(str::to_owned)));
    motion.extend(up_down("motion_box_smoothing_", SMOOTHING_KEYS.map(str::to_owned)));
    motion.extend(registry.motion_effects.iter().filter(|e| e.shortcut).map(|e| e.id.clone()));
    let mut belt = vec!["belt_switch".to_owned(), "belt_homing".to_owned()];
    belt.extend(up_down(
        "",
        [
            "belt_surge",
            "belt_surge_smoothing",
            "belt_heave",
            "belt_heave_smoothing",
            "belt_sway_curving",
            "belt_sway_smoothing",
        ]
        .map(str::to_owned),
    ));
    let mut sim3d = vec!["rumble_switch".to_owned()];
    sim3d.extend(ids(&registry.rumble_effects, "rumble_"));
    let mut haptic = vec!["haptic_switch".to_owned()];
    haptic.extend(ids(&registry.haptic_effects, "haptic_"));
    let mut compensation: Vec<String> = [
        "motion_compensation_switch",
        "motion_compensation_yaw_calibrate",
        "motion_compensation_roll_calibrate",
        "motion_compensation_pitch_calibrate",
        "motion_compensation_all_calibrate",
    ]
    .map(str::to_owned)
    .to_vec();
    compensation.extend(COMPENSATION_AXES.iter().map(|axis| format!("motion_compensation_{axis}_switch")));
    compensation.push("motion_compensation_recenter".to_owned());
    let list = |names: &[&str]| names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>();
    vec![
        ShortcutGroup {
            page: "Wind",
            actions: list(&[
                "power_wind_switch",
                "power_center_wind_switch",
                "power_wind_up",
                "power_wind_down",
                "power_center_wind_up",
                "power_center_wind_down",
                "ejection_seat",
            ]),
        },
        ShortcutGroup { page: "Shake", actions: shake },
        ShortcutGroup { page: "Motion", actions: motion },
        ShortcutGroup { page: "Belt", actions: belt },
        ShortcutGroup { page: "SIM3D", actions: sim3d },
        ShortcutGroup { page: "LED", actions: list(&["led_switch"]) },
        ShortcutGroup { page: "HapticR", actions: haptic },
        ShortcutGroup {
            page: "Telemetry",
            actions: list(&["telemetry_capture", "telemetry_reset", "telemetry_snapshot", "telemetry_export"]),
        },
        ShortcutGroup { page: "Joystick", actions: list(&["joystick_switch"]) },
        ShortcutGroup { page: "Motion Compensation", actions: compensation },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Title case as applied to the generated rumble/haptic labels.
    #[test]
    fn python_title() {
        assert_eq!(py_title(" throttle wheel slip up"), " Throttle Wheel Slip Up");
        assert_eq!(py_title("f1x ABS"), "F1X Abs");
        assert_eq!(py_title("it's"), "It'S");
    }

    /// The generated labels: removing the word `rumble` also strips it inside
    /// "rumble strips", and the leading space of the removed prefix stays.
    #[test]
    fn generated_labels() {
        assert_eq!(label("rumble_throttle_wheel_slip_up").unwrap(), " Throttle Wheel Slip Up");
        assert_eq!(label("rumble_seat_rumble_strips_down").unwrap(), " Seat  Strips Down");
        assert_eq!(label("haptic_wheel_rumble_strips_up").unwrap(), " Wheel Rumble Strips Up");
        assert_eq!(label("belt_sway_curving_up").unwrap(), "Increase Sway   ");
        assert_eq!(label("power_wind_switch").unwrap(), "Cycle Wind L/R Mode");
        assert_eq!(label("motion_box_smoothing_surge_down").unwrap(), "Decrease Smoothing of Surge");
        assert!(label("export_csv").is_none());
    }

    /// Every action a page lists has a label (so opening the editor never fails on a missing label),
    /// and every action of a fresh configuration is on some page's list except `joystick_switch`-like
    /// validation-only names.
    #[test]
    fn page_lists_are_labelled_and_cover_the_defaults() {
        let groups = groups();
        let listed: std::collections::BTreeSet<&str> =
            groups.iter().flat_map(|g| g.actions.iter().map(String::as_str)).collect();
        for action in &listed {
            assert!(label(action).is_some(), "{action} has no label");
        }
        let defaults = openrigcue_config::settings::Shortcuts::default();
        for action in defaults.bindings.keys() {
            assert!(listed.contains(action.as_str()), "{action} is on no page");
        }
        let shake = &groups[1].actions;
        assert_eq!(&shake[..3], ["shake_box_switch", "shake_box_engine_up", "shake_box_engine_down"]);
        assert_eq!(&shake[shake.len() - 2..], ["cannon_firing", "missile_firing"]);
        let sim3d = &groups[4].actions;
        assert_eq!(sim3d[1], "rumble_throttle_wheel_slip_up");
        assert!(sim3d[1..].iter().take(22).all(|a| a.ends_with("_up")), "all ups, then all downs");
    }
}
