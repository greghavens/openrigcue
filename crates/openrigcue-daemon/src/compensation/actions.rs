//! What the Motion Compensation page's buttons and the motion-compensation hotkeys do.

use std::str::FromStr;

use openrigcue_config::settings::{CompensationAxis, MotionCompensationSettings};
use openrigcue_devices::motion::yaw_vr::YawShared;
use openrigcue_devices::motion::LiveSettings;
use openrigcue_effects::accessories::compensation::MotionSensor;
use openrigcue_effects::accessories::dispatcher::Accessories;

use super::VrCompensation;
use crate::state::{ServiceEvent, ServiceState};

/// A compensation axis as the page and the hotkeys name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McAxis {
    Pitch,
    Roll,
    Yaw,
    Heave,
    Sway,
    Surge,
}

impl McAxis {
    fn name(self) -> &'static str {
        match self {
            Self::Pitch => "pitch",
            Self::Roll => "roll",
            Self::Yaw => "yaw",
            Self::Heave => "heave",
            Self::Sway => "sway",
            Self::Surge => "surge",
        }
    }

    fn parse(name: &str) -> Option<McAxis> {
        Some(match name {
            "pitch" => McAxis::Pitch,
            "roll" => McAxis::Roll,
            "yaw" => McAxis::Yaw,
            "heave" => McAxis::Heave,
            "sway" => McAxis::Sway,
            "surge" => McAxis::Surge,
            _ => return None,
        })
    }

    fn settings(self, c: &mut MotionCompensationSettings) -> &mut CompensationAxis {
        match self {
            McAxis::Pitch => &mut c.pitch,
            McAxis::Roll => &mut c.roll,
            McAxis::Yaw => &mut c.yaw,
            McAxis::Heave => &mut c.heave,
            McAxis::Sway => &mut c.sway,
            McAxis::Surge => &mut c.surge,
        }
    }

    /// The axis' raw sensor reading: the sensor pass writes it for pitch and roll; the raw yaw is written
    /// by the Yaw VR driver when it reads a response and is otherwise 0, its initial value.
    fn raw(self, sensor: &MotionSensor, yaw: &YawShared) -> f64 {
        match self {
            McAxis::Pitch => sensor.pitch.raw,
            McAxis::Roll => sensor.roll.raw,
            McAxis::Yaw => yaw.raw_yaw,
            McAxis::Heave => sensor.heave.raw,
            McAxis::Sway => sensor.sway.raw,
            McAxis::Surge => sensor.surge.raw,
        }
    }
}

/// The calibrate buttons exist for these axes.
const CALIBRATED: [McAxis; 3] = [McAxis::Yaw, McAxis::Roll, McAxis::Pitch];

/// One action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompensationAction {
    /// `motion_compensation_switch`: `mc_switch` toggles.
    Switch,
    /// `motion_compensation_<axis>_calibrate` (pitch, roll, yaw) and the page's calibrate buttons.
    Calibrate(McAxis),
    /// `motion_compensation_all_calibrate`: yaw, roll, pitch.
    CalibrateAll,
    /// `motion_compensation_<axis>_switch`: the axis' `enabled` toggles.
    AxisSwitch(McAxis),
    /// `motion_compensation_recenter` (OpenRigCue): the VR consumers re-calibrate their seat frame.
    Recenter,
}

impl FromStr for CompensationAction {
    type Err = ();

    /// The shortcut action names (`motion_compensation_*`, including
    /// `motion_compensation_recenter`); the short forms `switch`, `<axis>_calibrate`, `all_calibrate`,
    /// `<axis>_switch` and `recenter` are accepted too.
    fn from_str(name: &str) -> Result<Self, ()> {
        let name = name.strip_prefix("motion_compensation_").unwrap_or(name);
        match name {
            "switch" => return Ok(CompensationAction::Switch),
            "all_calibrate" => return Ok(CompensationAction::CalibrateAll),
            "recenter" => return Ok(CompensationAction::Recenter),
            _ => {}
        }
        if let Some(axis) = name.strip_suffix("_calibrate").and_then(McAxis::parse) {
            return CALIBRATED.contains(&axis).then_some(CompensationAction::Calibrate(axis)).ok_or(());
        }
        name.strip_suffix("_switch").and_then(McAxis::parse).map(CompensationAction::AxisSwitch).ok_or(())
    }
}

/// Calibrates one axis: its `MOTION_COMPENSATION` offset becomes the negated raw sensor reading.
fn calibrate(c: &mut MotionCompensationSettings, sensor: &MotionSensor, yaw: &YawShared, axis: McAxis) {
    axis.settings(c).offset = -axis.raw(sensor, yaw);
}

/// Where the calibrate actions read the raw sensor values: the IMU accessory (pitch, roll) and the Yaw VR
/// driver's shared state (yaw).
#[derive(Clone, Copy)]
pub struct RawSources<'a> {
    pub accessories: &'a Accessories,
    pub yaw_vr: &'a LiveSettings<YawShared>,
}

/// The UI calibration reads the shared sensor value before requiring each settings parent.
pub(super) fn run_native_calibration(
    state: &ServiceState,
    raw: RawSources<'_>,
    vr: &VrCompensation,
    axis: Option<McAxis>,
) -> Result<(), String> {
    use openrigcue_model::native_value::LegacyValue;
    fn item<'a>(parent: &'a LegacyValue, key: &str) -> Result<&'a LegacyValue, String> {
        parent.item(key).map_err(|detail| {
            format!("{}: {detail}", if parent.as_dict().is_some() { "KeyError" } else { "TypeError" })
        })
    }
    let axes: &[McAxis] = if let Some(axis) = axis.as_ref() { std::slice::from_ref(axis) } else { &CALIBRATED };
    for axis in axes {
        // RHS first, independently of device I/O.
        let raw_key = format!("raw_{}", axis.name());
        let reading = raw
            .accessories
            .motion_sensor_value(&raw_key)
            .ok_or_else(|| format!("KeyError: {}", openrigcue_model::python_repr::repr_str(&raw_key)))?;
        #[allow(
            clippy::neg_multiply,
            reason = "The reference multiplies by -1; preserve that floating-point operation."
        )]
        let offset = match reading {
            LegacyValue::Int(value) => {
                LegacyValue::Int(value.checked_neg().ok_or("OverflowError: integer exceeds supported range")?)
            }
            LegacyValue::Bool(value) => LegacyValue::Int(if value { -1 } else { 0 }),
            LegacyValue::Float(value) => LegacyValue::Float(value * -1.0),
            LegacyValue::Str(_) => LegacyValue::Str(String::new()),
            LegacyValue::Bytes(_) => LegacyValue::Bytes(Vec::new()),
            LegacyValue::List(_) => LegacyValue::List(Vec::new()),
            LegacyValue::Tuple(_) => LegacyValue::Tuple(Vec::new()),
            value => {
                return Err(format!("TypeError: unsupported operand type(s) for *: '{}' and 'int'", value.type_name()))
            }
        };
        {
            let mut store = state.config.write();
            let root = LegacyValue::Dict(store.settings().native_snapshot());
            let mut section = item(&root, "MOTION_COMPENSATION")?.clone();
            let mut target = item(&section, axis.name())?.clone();
            target.assign_item("offset", offset).map_err(|detail| format!("TypeError: {detail}"))?;
            section.assign_item(axis.name(), target).map_err(|detail| format!("TypeError: {detail}"))?;
            store.update(|configuration| {
                configuration.settings.edit_native(|raw| raw.insert("MOTION_COMPENSATION", section))
            });
        }
        state.announce(ServiceEvent::Settings);
    }
    vr.recenter();
    Ok(())
}

/// Runs `action` from the page (or the API). Calibrations and re-centres also bump the VR re-centre
/// generation, so the headset is re-levelled with the new offsets.
pub fn run_action(state: &ServiceState, raw: RawSources<'_>, vr: &VrCompensation, action: CompensationAction) {
    match action {
        CompensationAction::Switch => {
            // The compensation switch toggles: 1 -> 0, 0 -> 1.
            let mut controls = state.controls.write();
            controls.switches.compensation = !controls.switches.compensation;
        }
        CompensationAction::Calibrate(_) | CompensationAction::CalibrateAll => {
            let sensor = raw.accessories.imu.lock().unwrap_or_else(std::sync::PoisonError::into_inner).motion_sensor();
            let yaw = raw.yaw_vr.get();
            let axes: &[McAxis] = match action {
                CompensationAction::Calibrate(axis) => &[axis][..],
                _ => &CALIBRATED,
            };
            let axes = axes.to_vec();
            state.config.write().update(|config| {
                for axis in axes {
                    calibrate(&mut config.settings.compensation, &sensor, &yaw, axis);
                }
            });
            state.announce(ServiceEvent::Settings);
            vr.recenter();
        }
        CompensationAction::AxisSwitch(axis) => {
            state.config.write().update(|config| {
                let a = axis.settings(&mut config.settings.compensation);
                a.enabled = !a.enabled;
            });
            state.announce(ServiceEvent::Settings);
        }
        CompensationAction::Recenter => {
            vr.recenter();
        }
    }
}

/// Runs a hotkey action ([`run_action`]). Returns false when the action is not a motion-compensation one.
///
/// Called by the hotkey runtime (`crate::hotkeys`): `motion_compensation_switch` directly from the keyboard
/// event, the others when the queued UI actions are consumed.
pub fn run_shortcut(state: &ServiceState, raw: RawSources<'_>, vr: &VrCompensation, name: &str) -> bool {
    let Ok(action) = name.parse::<CompensationAction>() else { return false };
    run_action(state, raw, vr, action);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::OriginalPlatform;
    use crate::store::ConfigStore;
    use openrigcue_config::settings::Configuration;
    use openrigcue_effects::accessories::compensation::AxisReading;

    fn service(
        platform: OriginalPlatform,
    ) -> (ServiceState, Accessories, LiveSettings<YawShared>, VrCompensation, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), platform);
        let vr = VrCompensation::new(None, dir.path().join("rig-pose"));
        (state, Accessories::new(std::time::Instant::now()), LiveSettings::new(YawShared::default()), vr, dir)
    }

    /// The shortcut action names, including the re-centre.
    #[test]
    fn action_names() {
        let parse = |s: &str| s.parse::<CompensationAction>();
        assert_eq!(parse("motion_compensation_switch"), Ok(CompensationAction::Switch));
        assert_eq!(parse("motion_compensation_all_calibrate"), Ok(CompensationAction::CalibrateAll));
        for (name, axis) in [("yaw", McAxis::Yaw), ("roll", McAxis::Roll), ("pitch", McAxis::Pitch)] {
            assert_eq!(
                parse(&format!("motion_compensation_{name}_calibrate")),
                Ok(CompensationAction::Calibrate(axis))
            );
        }
        assert_eq!(parse("motion_compensation_heave_calibrate"), Err(()), "no calibrate button for heave");
        for name in ["yaw", "roll", "pitch", "heave", "surge", "sway"] {
            assert_eq!(
                parse(&format!("motion_compensation_{name}_switch")),
                Ok(CompensationAction::AxisSwitch(McAxis::parse(name).unwrap()))
            );
        }
        assert_eq!(parse("recenter"), Ok(CompensationAction::Recenter));
        assert_eq!(parse("belt_homing"), Err(()));
    }

    /// Calibration sets offset = raw * -1 per axis; 'all' does yaw, roll, pitch; without a Yaw VR
    /// report the raw yaw is the initial 0. Each calibration also asks the VR consumers to re-calibrate.
    #[test]
    fn calibrate_takes_the_negated_raw_reading() {
        let mut c = Configuration::first_run().settings.compensation;
        let sensor = MotionSensor {
            pitch: AxisReading { value: 3.0, calculated: 3.0, raw: 2.5 },
            roll: AxisReading { value: 1.0, calculated: 1.0, raw: -1.25 },
            ..MotionSensor::default()
        };
        c.yaw.offset = 4.0;
        let no_yaw_vr = YawShared::default();
        calibrate(&mut c, &sensor, &no_yaw_vr, McAxis::Pitch);
        assert_eq!(c.pitch.offset, -2.5);
        calibrate(&mut c, &sensor, &no_yaw_vr, McAxis::Roll);
        assert_eq!(c.roll.offset, 1.25);
        calibrate(&mut c, &sensor, &no_yaw_vr, McAxis::Yaw);
        assert_eq!(c.yaw.offset, 0.0);

        let (state, accessories, yaw, vr, _dir) = service(OriginalPlatform::Windows);
        state.config.write().update(|config| config.settings.compensation.pitch.offset = 9.0);
        run_action(
            &state,
            RawSources { accessories: &accessories, yaw_vr: &yaw },
            &vr,
            CompensationAction::CalibrateAll,
        );
        let settings = state.config.read().settings().compensation.clone();
        assert_eq!(
            (settings.pitch.offset, settings.roll.offset, settings.yaw.offset),
            (0.0, 0.0, 0.0),
            "no session: raw_* are 0"
        );
        assert_eq!(vr.recenter_generation(), 1);
    }

    /// Yaw calibration on a Yaw VR rig: the driver stores the reported yaw as the raw yaw, so the offset
    /// becomes the negated yaw, and the driver's calculated yaw, `(yaw + offset) * factor`, is 0.
    #[test]
    fn yaw_calibrate_takes_the_yaw_vr_reading() {
        use openrigcue_devices::motion::yaw_vr::{net, YawVrSettings};
        let (state, accessories, yaw, vr, _dir) = service(OriginalPlatform::Windows);
        net::handle_feedback(b"SY[20]SP[1]SR[2]U[90]", &YawVrSettings::default(), &yaw).unwrap();
        assert_eq!(yaw.get().raw_yaw, 20.0);
        run_action(
            &state,
            RawSources { accessories: &accessories, yaw_vr: &yaw },
            &vr,
            CompensationAction::Calibrate(McAxis::Yaw),
        );
        assert_eq!(state.config.read().settings().compensation.yaw.offset, -20.0);
        let report = yaw.get().report.unwrap();
        let settings = YawVrSettings { compensation_yaw_offset: -20.0, ..YawVrSettings::default() };
        assert_eq!(net::compensation_yaw(&report, &settings), (20.0, 0.0));
    }

    /// `motion_compensation_switch` toggles the compensation switch; the `<axis>_switch` and
    /// `_calibrate` hotkeys run too.
    #[test]
    fn switches_and_platform_gate() {
        let (state, accessories, yaw, vr, _dir) = service(OriginalPlatform::Windows);
        assert!(state.controls.read().switches.compensation);
        assert!(run_shortcut(
            &state,
            RawSources { accessories: &accessories, yaw_vr: &yaw },
            &vr,
            "motion_compensation_switch"
        ));
        assert!(!state.controls.read().switches.compensation);
        assert!(run_shortcut(
            &state,
            RawSources { accessories: &accessories, yaw_vr: &yaw },
            &vr,
            "motion_compensation_switch"
        ));
        assert!(state.controls.read().switches.compensation);
        assert!(run_shortcut(
            &state,
            RawSources { accessories: &accessories, yaw_vr: &yaw },
            &vr,
            "motion_compensation_recenter"
        ));
        assert_eq!(vr.recenter_generation(), 1);
        assert!(!run_shortcut(&state, RawSources { accessories: &accessories, yaw_vr: &yaw }, &vr, "joystick_switch"));

        let (state, accessories, yaw, vr, _dir) = service(OriginalPlatform::Windows);
        let yaw_enabled = state.config.read().settings().compensation.yaw.enabled;
        assert!(run_shortcut(
            &state,
            RawSources { accessories: &accessories, yaw_vr: &yaw },
            &vr,
            "motion_compensation_yaw_switch"
        ));
        assert_eq!(state.config.read().settings().compensation.yaw.enabled, !yaw_enabled);
        assert!(run_shortcut(
            &state,
            RawSources { accessories: &accessories, yaw_vr: &yaw },
            &vr,
            "motion_compensation_all_calibrate"
        ));
        assert_eq!(vr.recenter_generation(), 1);
    }

    /// `motion_compensation_recenter` has no default binding, but the settings
    /// document stores a binding for it like any other action (`[SHORTCUTS]` bindings are keyed by name).
    #[test]
    fn recenter_can_be_bound() {
        let settings = Configuration::first_run().settings;
        assert!(settings.shortcuts.binding("motion_compensation_recenter").is_none());
        let patch = serde_json::json!({ "shortcuts": { "bindings": { "motion_compensation_recenter": { "keys": "ctrl+f9" } } } });
        let bound = crate::api::patched(&settings, &patch).unwrap();
        assert_eq!(bound.shortcuts.binding("motion_compensation_recenter").unwrap().keys, "ctrl+f9");
        assert!("motion_compensation_recenter".parse::<CompensationAction>().is_ok());
    }
}
