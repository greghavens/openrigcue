//! The UI main loop's consumption of the queued UI actions: slider steps of the tuning pages while a
//! game runs.
//!
//! Each `<prefix><key>_up` / `_down` in the list moves its slider one unit: the shown value is truncated
//! to an integer, changed by one, and set through the slider's change handler with the pointer
//! fraction of the new value, so the value is rounded to the slider's step
//! and clamped to its range before the page handler stores it. An action present several times counts once;
//! up is applied before down. The page rows, ranges and stored fields are the UI page descriptions
//! (`openrigcue_ui::spec`).

use openrigcue_config::catalog::registry::Registry;
use openrigcue_ui::spec::{self, Control, Field, Page, Read, Store, Widget, Write};
use serde_json::Value;

use super::names::{BELT_KEYS, MOTION_PLATFORM_KEYS, SMOOTHING_KEYS};

/// The documents a slider can store into: the active tune and the settings, as JSON.
pub struct Stores<'a> {
    pub tune: &'a mut Value,
    pub settings: &'a mut Value,
}

impl Stores<'_> {
    fn get(&self, field: &Field) -> Option<f64> {
        let mut value = match field.store {
            Store::Tune => &*self.tune,
            Store::Settings => &*self.settings,
            Store::Controls => return None,
        };
        for key in &field.path {
            value = value.get(key)?;
        }
        value.as_f64()
    }

    fn set(&mut self, field: &Field, number: f64) {
        let mut value = match field.store {
            Store::Tune => &mut *self.tune,
            Store::Settings => &mut *self.settings,
            Store::Controls => return,
        };
        for key in &field.path {
            let Some(next) = value.get_mut(key) else { return };
            value = next;
        }
        *value = json_number(number);
    }
}

/// A whole number as a JSON integer (integer fields refuse `20.0`), anything else as a float.
fn json_number(number: f64) -> Value {
    if number.fract() == 0.0 && number.abs() < 9.0e15 {
        Value::from(number as i64)
    } else {
        serde_json::Number::from_f64(number).map_or(Value::Null, Value::Number)
    }
}

/// What the consumption did.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SliderOutcome {
    /// Rows moved, as `page/key`.
    pub moved: Vec<String>,
    /// The action whose row does not exist on its page
    /// (`belt_sway_curving_*`: the Belt row is `belt_curving`; `rumble_<x>_rumble_strips_*`:
    /// removing the `rumble_` prefix also strips the inner `rumble_`). The rows before it have moved, the ones
    /// after it are not looked at, and the error ends the UI main loop.
    pub missing: Option<String>,
}

/// The pages whose sliders have hotkeys, built as the UI builds them.
struct Pages {
    shake: Page,
    motion: Page,
    belt: Page,
    sim3d: Page,
    haptic: Page,
}

impl Pages {
    fn build(smoothing_step: f64) -> Pages {
        let registry = Registry::builtin();
        Pages {
            shake: spec::shake_page(registry),
            motion: spec::motion_page(registry, smoothing_step.trunc()),
            belt: spec::belt_page(),
            sim3d: spec::sim3d_page(registry),
            haptic: spec::haptic_page(registry),
        }
    }
}

/// Moves the sliders `actions` asks for. `game_running` false moves nothing.
/// `enable_hapticr` is the `APP.enable_hapticr` setting, `smoothing_step` is `CUSTOM_FEATURES.smoothing_slider_step`.
pub fn apply(
    actions: &[String],
    game_running: bool,
    enable_hapticr: bool,
    smoothing_step: f64,
    stores: &mut Stores<'_>,
) -> SliderOutcome {
    let mut outcome = SliderOutcome::default();
    if !game_running || actions.is_empty() {
        return outcome;
    }
    let pages = Pages::build(smoothing_step);
    let registry = Registry::builtin();
    let mut rows: Vec<(&Page, String, String)> = Vec::new();
    for effect in &registry.shaker_effects {
        rows.push((&pages.shake, effect.id.clone(), format!("shake_box_{}", effect.id)));
    }
    for effect in &registry.motion_effects {
        rows.push((&pages.motion, effect.id.clone(), format!("motion_box_{}", effect.id)));
    }
    for key in MOTION_PLATFORM_KEYS {
        rows.push((&pages.motion, key.to_owned(), format!("motion_box_{key}")));
    }
    for key in SMOOTHING_KEYS {
        rows.push((&pages.motion, format!("motion_smoothing_{key}"), format!("motion_box_smoothing_{key}")));
    }
    for key in BELT_KEYS {
        rows.push((&pages.belt, key.to_owned(), key.to_owned()));
    }
    for effect in &registry.rumble_effects {
        let action = format!("rumble_{}", effect.id);
        rows.push((&pages.sim3d, action.replace("rumble_", ""), action));
    }
    // The HapticR rows exist only with `APP.enable_hapticr` on.
    if enable_hapticr {
        for effect in &registry.haptic_effects {
            let action = format!("haptic_{}", effect.id);
            rows.push((&pages.haptic, action.replace("haptic_", ""), action));
        }
    }
    for (page, key, action) in rows {
        for (suffix, delta) in [("_up", 1.0), ("_down", -1.0)] {
            let name = format!("{action}{suffix}");
            if !actions.contains(&name) {
                continue;
            }
            match page.control(&key).filter(|c| matches!(c.widget, Widget::Slider { .. })) {
                Some(control) => {
                    if step(control, delta, stores) {
                        outcome.moved.push(format!("{}/{key}", page.key));
                    }
                }
                None => {
                    outcome.missing = Some(name);
                    return outcome;
                }
            }
        }
    }
    outcome
}

/// Moves one row's slider by `delta`; true when the stored value changed.
fn step(control: &Control, delta: f64, stores: &mut Stores<'_>) -> bool {
    let (Widget::Slider { scale }, Some(field)) = (&control.widget, &control.field) else { return false };
    let Some(stored) = stores.get(field) else { return false };
    let shown = match control.read {
        Read::IntOfFloat => stored.trunc(),
        Read::Number | Read::SpeedToUnit => stored,
    };
    let widget = scale.snap(shown);
    let target = widget.trunc() + delta;
    if target == widget {
        return false;
    }
    let value = scale.value_at((target - scale.min) / (scale.max - scale.min));
    let written = match control.write {
        Write::Int => value.trunc(),
        _ => value,
    };
    if written == stored {
        return false;
    }
    for field in std::iter::once(field).chain(control.also.iter()) {
        stores.set(field, written);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrigcue_config::settings::Settings;
    use openrigcue_config::tunes::FactoryTunes;

    fn documents() -> (Value, Value) {
        let tune = FactoryTunes::builtin().get("emulator").unwrap().clone();
        (serde_json::to_value(tune).unwrap(), serde_json::to_value(Settings::default()).unwrap())
    }

    fn run(actions: &[&str], tune: &mut Value, settings: &mut Value, step: f64) -> SliderOutcome {
        let actions: Vec<String> = actions.iter().map(|a| (*a).to_owned()).collect();
        apply(&actions, true, true, step, &mut Stores { tune, settings })
    }

    /// One unit per action; clamped at the range end; up then down leaves the value; nothing in stand-by.
    #[test]
    fn effect_sliders_step_by_one() {
        let (mut tune, mut settings) = documents();
        tune["shaker_effects"]["engine"] = 7.into();
        tune["motion_effects"]["pitch"] = 20.into();
        let outcome = run(&["shake_box_engine_up", "motion_box_pitch_up"], &mut tune, &mut settings, 1.0);
        assert_eq!(tune["shaker_effects"]["engine"], 8);
        assert_eq!(tune["motion_effects"]["pitch"], 20, "already at the maximum");
        assert_eq!(outcome.moved, ["Shake/engine"]);
        run(&["shake_box_engine_up", "shake_box_engine_down"], &mut tune, &mut settings, 1.0);
        assert_eq!(tune["shaker_effects"]["engine"], 8);
        let actions = vec!["shake_box_engine_up".to_owned()];
        apply(&actions, false, true, 1.0, &mut Stores { tune: &mut tune, settings: &mut settings });
        assert_eq!(tune["shaker_effects"]["engine"], 8, "game_running == 'none'");
    }

    /// The shown value is truncated; the new value is rounded to the slider's step, so with a
    /// smoothing step of 5 a one-unit move rounds back and nothing changes.
    #[test]
    fn rounding_to_the_slider_step() {
        let (mut tune, mut settings) = documents();
        tune["motion_effect_smoothing_factor"]["heave"] = 20.into();
        run(&["motion_box_smoothing_heave_up"], &mut tune, &mut settings, 5.0);
        assert_eq!(tune["motion_effect_smoothing_factor"]["heave"], 20);
        run(&["motion_box_smoothing_heave_up"], &mut tune, &mut settings, 1.0);
        assert_eq!(tune["motion_effect_smoothing_factor"]["heave"], 21);
        tune["motion_max_power"] = 41.7.into();
        run(&["motion_box_motion_max_power_down"], &mut tune, &mut settings, 1.0);
        assert_eq!(tune["motion_max_power"], 41, "shown 42 → int 42 → 41");
    }

    /// Reaction Speed also writes the motion settings; its range is 70…100.
    #[test]
    fn handler_writes_every_field() {
        let (mut tune, mut settings) = documents();
        tune["motion_reaction_speed"] = 70.into();
        run(&["motion_box_reaction_speed_down"], &mut tune, &mut settings, 1.0);
        assert_eq!(tune["motion_reaction_speed"], 70);
        run(&["motion_box_reaction_speed_up"], &mut tune, &mut settings, 1.0);
        assert_eq!(tune["motion_reaction_speed"], 71);
        assert_eq!(settings["motion"]["reaction_speed"], 71);
    }

    /// Belt, SIM3D and HapticR rows move; the first row that cannot be found stops the pass there.
    #[test]
    fn accessory_rows_and_the_missing_row() {
        let (mut tune, mut settings) = documents();
        tune["belt_heave_factor"] = 5.into();
        tune["rumble_effects"]["seat_gear"] = 3.into();
        tune["haptic_effects"]["wheel_rumble_strips"] = 3.into();
        let outcome = run(
            &["belt_heave_up", "rumble_seat_gear_down", "haptic_wheel_rumble_strips_up"],
            &mut tune,
            &mut settings,
            1.0,
        );
        assert_eq!(tune["belt_heave_factor"], 6);
        assert_eq!(tune["rumble_effects"]["seat_gear"], 2);
        assert_eq!(tune["haptic_effects"]["wheel_rumble_strips"], 4);
        assert_eq!(outcome.missing, None);

        let outcome = run(
            &["belt_heave_up", "belt_sway_curving_up", "rumble_seat_gear_down", "rumble_seat_rumble_strips_up"],
            &mut tune,
            &mut settings,
            1.0,
        );
        assert_eq!(tune["belt_heave_factor"], 7, "before the missing row");
        assert_eq!(tune["rumble_effects"]["seat_gear"], 2, "after the missing row");
        assert_eq!(outcome.missing.as_deref(), Some("belt_sway_curving_up"));
    }

    /// With `APP.enable_hapticr` off the HapticR rows are not looked at.
    #[test]
    fn hapticr_rows_follow_enable_hapticr() {
        let (mut tune, mut settings) = documents();
        tune["haptic_effects"]["wheel_rumble_strips"] = 3.into();
        let actions = vec!["haptic_wheel_rumble_strips_up".to_owned()];
        let outcome = apply(&actions, true, false, 1.0, &mut Stores { tune: &mut tune, settings: &mut settings });
        assert_eq!(tune["haptic_effects"]["wheel_rumble_strips"], 3);
        assert!(outcome.moved.is_empty());
    }

    /// The moved tune is still a valid tune (integer fields get JSON integers).
    #[test]
    fn result_deserializes() {
        let (mut tune, mut settings) = documents();
        run(&["motion_box_smoothing_pitch_up", "belt_surge_smoothing_up"], &mut tune, &mut settings, 1.0);
        serde_json::from_value::<openrigcue_config::tunes::Tune>(tune).unwrap();
        serde_json::from_value::<Settings>(settings).unwrap();
    }
}
