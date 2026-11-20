//! What a hotkey does: the effect of one action name (for keyboard combos and
//! controller buttons alike), including the wind switch cycle.

use std::path::Path;

use openrigcue_config::settings::Shortcuts;

use crate::api::ApiContext;
use crate::runtime::OutputMode;
use crate::state::ServiceEvent;

/// The effect of one action name, in the order the names are tested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// `power_wind_switch` (`center` false) / `power_center_wind_switch`: the three-state cycle with the skips.
    WindSwitch { center: bool },
    /// `motion_compensation_switch`: the compensation switch toggles.
    CompensationSwitch,
    /// `shake_box_switch`: the shaker switch toggles.
    ShakersSwitch,
    /// `joystick_switch`: the joystick status toggles.
    JoystickSwitch,
    /// `led_switch`: the LED mode goes auto → always on → off → auto.
    LedSwitch,
    /// `motion_box_switch`: the motion switch toggles.
    MotionSwitch,
    /// `belt_switch`: on → off (which also ends the belt test), off → on.
    BeltSwitch,
    /// `rumble_switch`: the SIM3D switch toggles.
    RumbleSwitch,
    /// `haptic_switch`: the haptic switch toggles.
    HapticSwitch,
    /// `power_wind_up/down`, `power_center_wind_up/down`: the tune's maximum speed ± `wind_power_increment`.
    WindStep { center: bool, up: bool },
    /// `motion_box_homing`: motion homing is requested.
    MotionHoming,
    /// `belt_homing`: belt homing is requested while the belt switch is on.
    BeltHoming,
    /// `telemetry_capture`: telemetry capture toggles (without resetting the extremes).
    TelemetryCapture,
    /// `telemetry_reset`: the telemetry extremes are cleared and the UI is told to reset.
    TelemetryReset,
    /// `telemetry_export`: the telemetry export switch toggles.
    TelemetryExport,
    /// `telemetry_snapshot`: the telemetry table is written once ([`super::snapshot`]).
    TelemetrySnapshot,
    /// `cannon_firing`, `missile_firing`, `ejection_seat`: nothing here (the caller sets the canned effects).
    Canned,
    /// Anything else: queued for the UI main loop.
    Queue,
}

/// Classifies an action name.
pub fn effect_of(action: &str) -> Effect {
    match action {
        "power_wind_switch" => Effect::WindSwitch { center: false },
        "power_center_wind_switch" => Effect::WindSwitch { center: true },
        "motion_compensation_switch" => Effect::CompensationSwitch,
        "shake_box_switch" => Effect::ShakersSwitch,
        "joystick_switch" => Effect::JoystickSwitch,
        "led_switch" => Effect::LedSwitch,
        "motion_box_switch" => Effect::MotionSwitch,
        "belt_switch" => Effect::BeltSwitch,
        "rumble_switch" => Effect::RumbleSwitch,
        "haptic_switch" => Effect::HapticSwitch,
        "power_wind_up" => Effect::WindStep { center: false, up: true },
        "power_wind_down" => Effect::WindStep { center: false, up: false },
        "power_center_wind_up" => Effect::WindStep { center: true, up: true },
        "power_center_wind_down" => Effect::WindStep { center: true, up: false },
        "motion_box_homing" => Effect::MotionHoming,
        "belt_homing" => Effect::BeltHoming,
        "telemetry_capture" => Effect::TelemetryCapture,
        "telemetry_reset" => Effect::TelemetryReset,
        "telemetry_export" => Effect::TelemetryExport,
        "telemetry_snapshot" => Effect::TelemetrySnapshot,
        _ if super::CANNED_EFFECTS.contains(&action) => Effect::Canned,
        _ => Effect::Queue,
    }
}

/// The wind switch's off state: off, or auto with `skip_power_wind_off`, or always on with both off and auto
/// skipped.
fn wind_off_state(skips: &Shortcuts) -> i64 {
    match (skips.skip_power_wind_off, skips.skip_power_wind_auto) {
        (false, _) => 0,
        (true, false) => 1,
        (true, true) => 2,
    }
}

/// The `power_wind_switch` cycle on the wind (or centre wind) status: one step forward,
/// then 3 wraps to the off state, 2 is skipped (to the off state) with `skip_power_wind_always_on`, 1 is
/// skipped (to 2, with no further test) with `skip_power_wind_auto`, and 0 (only from -1) goes to the off state.
pub fn wind_cycle(status: i64, skips: &Shortcuts) -> i64 {
    match status + 1 {
        3 => wind_off_state(skips),
        2 if skips.skip_power_wind_always_on => wind_off_state(skips),
        1 if skips.skip_power_wind_auto => 2,
        0 => wind_off_state(skips),
        next => next,
    }
}

/// `led_switch`: 1 → 2, 2 → 0, 0 → 1.
pub fn led_cycle(mode: OutputMode) -> OutputMode {
    match mode {
        OutputMode::Auto => OutputMode::AlwaysOn,
        OutputMode::AlwaysOn => OutputMode::Off,
        OutputMode::Off => OutputMode::Auto,
    }
}

/// `power_wind_up`/`_down`: `max_speed` plus (or minus) the increment, clamped to 0…100.
pub fn wind_step(max_speed: f64, increment: u32, up: bool) -> f64 {
    let step = f64::from(increment);
    let next = if up { max_speed + step } else { max_speed - step };
    if up && next > 100.0 {
        100.0
    } else if !up && next < 0.0 {
        0.0
    } else {
        next
    }
}

fn mode_of(code: i64) -> OutputMode {
    match code {
        1 => OutputMode::Auto,
        2 => OutputMode::AlwaysOn,
        _ => OutputMode::Off,
    }
}

/// Runs `action` against the service. `export_dir` is where `telemetry_snapshot` writes.
///
/// Whether the emulator runs for an "always on" wind or LED switch is derived from the switch itself.
pub fn run(ctx: &ApiContext, export_dir: &Path, action: &str) {
    tracing::debug!(action, "hotkey");
    let effect = effect_of(action);
    match effect {
        Effect::WindSwitch { center } => {
            let skips = ctx.state.config.read().settings().shortcuts.clone();
            let mut controls = ctx.state.controls.write();
            let switch = if center { &mut controls.switches.wind_center } else { &mut controls.switches.wind };
            *switch = mode_of(wind_cycle(switch.code(), &skips));
        }
        Effect::CompensationSwitch => {
            let raw = crate::compensation::RawSources {
                accessories: &ctx.accessories,
                yaw_vr: &ctx.motion.driver_settings().yaw_shared,
            };
            crate::compensation::run_shortcut(&ctx.state, raw, &ctx.compensation, action);
        }
        Effect::ShakersSwitch => toggle(ctx, |s| &mut s.shakers),
        Effect::JoystickSwitch => {
            let mut controls = ctx.state.controls.write();
            controls.switches.joystick_status = match controls.switches.joystick_status {
                0 => 1,
                1 => 0,
                other => other,
            };
        }
        Effect::MotionSwitch => toggle(ctx, |s| &mut s.motion),
        Effect::RumbleSwitch => toggle(ctx, |s| &mut s.sim3d),
        Effect::HapticSwitch => toggle(ctx, |s| &mut s.haptic),
        Effect::LedSwitch => {
            let mut controls = ctx.state.controls.write();
            controls.switches.led = led_cycle(controls.switches.led);
        }
        Effect::BeltSwitch => {
            let turned_off = {
                let mut controls = ctx.state.controls.write();
                controls.switches.belt = !controls.switches.belt;
                if !controls.switches.belt {
                    controls.tests.belt_test = false;
                }
                !controls.switches.belt
            };
            if turned_off {
                // A pending belt homing request is cleared while the belt switch is off.
                ctx.accessories.set_belt_homing(crate::accessories::homing_of(0));
            }
        }
        Effect::WindStep { center, up } => {
            let increment = ctx.state.config.read().settings().wind.power_step;
            let mut profile = ctx.state.profile.write();
            let speed = if center {
                &mut profile.tune.power_center_wind_max_speed_perc
            } else {
                &mut profile.tune.power_wind_max_speed_perc
            };
            *speed = wind_step(*speed, increment, up);
            profile.generation += 1;
            drop(profile);
            ctx.state.announce(ServiceEvent::TuneChanged);
        }
        Effect::MotionHoming => {
            ctx.motion.driver_settings().yaw_shared.update(|s| s.homing_requested = true);
        }
        Effect::BeltHoming => {
            if ctx.state.controls.read().switches.belt {
                ctx.accessories.set_belt_homing(openrigcue_effects::accessories::belt::Homing::Requested);
            }
        }
        Effect::TelemetryCapture => {
            let mut controls = ctx.state.controls.write();
            controls.telemetry.reset_min_max = false;
            controls.telemetry.capture = !controls.telemetry.capture;
        }
        Effect::TelemetryReset => {
            ctx.telemetry.reset_extremes();
            ctx.state.controls.write().telemetry.reset_min_max = true;
            ctx.state.announce(ServiceEvent::MinMaxReset);
        }
        Effect::TelemetryExport => {
            let mut controls = ctx.state.controls.write();
            controls.telemetry.export = !controls.telemetry.export;
        }
        Effect::TelemetrySnapshot => {
            let session = ctx.state.session.snapshot();
            let rows = ctx.telemetry.export_rows_snapshot();
            let now = chrono::Local::now().naive_local();
            match super::snapshot::snapshot_native(session.game_key(), &session.vehicle_value(), &rows, export_dir, now)
            {
                Some(Ok(path)) => tracing::info!(path = %path.display(), "telemetry snapshot written"),
                Some(Err(error)) => tracing::warn!(%error, "telemetry snapshot failed"),
                None => {}
            }
        }
        Effect::Canned => {}
        Effect::Queue => ctx.hotkeys.queue(action),
    }
}

fn toggle(ctx: &ApiContext, switch: impl FnOnce(&mut crate::runtime::Switches) -> &mut bool) {
    let mut controls = ctx.state.controls.write();
    let value = switch(&mut controls.switches);
    *value = !*value;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skips(always_on: bool, auto: bool, off: bool) -> Shortcuts {
        Shortcuts {
            skip_power_wind_always_on: always_on,
            skip_power_wind_auto: auto,
            skip_power_wind_off: off,
            ..Shortcuts::default()
        }
    }

    /// The wind cycle for every combination of the three skips, from each state.
    #[test]
    fn wind_cycle_with_skips() {
        let cycle = |s: &Shortcuts, from: i64| wind_cycle(from, s);
        let plain = skips(false, false, false);
        assert_eq!([cycle(&plain, 0), cycle(&plain, 1), cycle(&plain, 2)], [1, 2, 0]);
        let no_always = skips(true, false, false);
        assert_eq!([cycle(&no_always, 0), cycle(&no_always, 1), cycle(&no_always, 2)], [1, 0, 0]);
        let no_auto = skips(false, true, false);
        assert_eq!([cycle(&no_auto, 0), cycle(&no_auto, 1), cycle(&no_auto, 2)], [2, 2, 0]);
        let no_off = skips(false, false, true);
        assert_eq!([cycle(&no_off, 0), cycle(&no_off, 1), cycle(&no_off, 2)], [1, 2, 1]);
        let auto_and_off = skips(false, true, true);
        assert_eq!([cycle(&auto_and_off, 0), cycle(&auto_and_off, 1), cycle(&auto_and_off, 2)], [2, 2, 2]);
        let all = skips(true, true, true);
        assert_eq!([cycle(&all, 0), cycle(&all, 1), cycle(&all, 2)], [2, 2, 2], "1 → 2 is not re-tested");
        assert_eq!(cycle(&no_off, -1), 1, "status 0 after the step goes to the off state");
    }

    #[test]
    fn led_and_wind_steps() {
        assert_eq!(led_cycle(OutputMode::Auto), OutputMode::AlwaysOn);
        assert_eq!(led_cycle(OutputMode::AlwaysOn), OutputMode::Off);
        assert_eq!(led_cycle(OutputMode::Off), OutputMode::Auto);
        assert_eq!(wind_step(95.0, 10, true), 100.0);
        assert_eq!(wind_step(5.0, 10, false), 0.0);
        assert_eq!(wind_step(42.5, 10, true), 52.5);
        assert_eq!(wind_step(120.0, 0, false), 120.0, "only the moved bound is clamped");
    }

    /// Every action name of a fresh configuration and of the editor's lists is classified, and only the
    /// names [`effect_of`] tests have an effect of their own; everything else is queued for the UI.
    #[test]
    fn classification() {
        assert_eq!(effect_of("power_wind_switch"), Effect::WindSwitch { center: false });
        assert_eq!(effect_of("power_center_wind_down"), Effect::WindStep { center: true, up: false });
        assert_eq!(effect_of("ejection_seat"), Effect::Canned);
        assert_eq!(effect_of("motion_compensation_switch"), Effect::CompensationSwitch);
        for queued in [
            "motion_compensation_all_calibrate",
            "motion_compensation_yaw_switch",
            "motion_compensation_recenter",
            "shake_box_engine_up",
            "belt_surge_down",
            "rumble_seat_gear_up",
            "haptic_wheel_engine_down",
            "motion_box_smoothing_heave_up",
            "export_csv",
        ] {
            assert_eq!(effect_of(queued), Effect::Queue, "{queued}");
        }
        let direct = openrigcue_config::settings::Shortcuts::default()
            .bindings
            .keys()
            .filter(|a| !matches!(effect_of(a), Effect::Queue | Effect::Canned))
            .count();
        assert_eq!(direct, 19, "the actions of a fresh install handled in OnKeyboardEvent itself");
    }
}
