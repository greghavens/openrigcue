//! The hotkey runtime against a service state, driven by fake controller and keyboard event streams (no
//! device, socket or file outside a temporary directory is used).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use openrigcue_config::settings::{Binding, Configuration};
use openrigcue_effects::accessories::belt::Homing;
use openrigcue_effects::accessories::dispatcher::Accessories;
use openrigcue_effects::shakers::SampleBank;
use openrigcue_sources::api::{JoystickEvents, JoystickInputs};
use openrigcue_sources::derive::Dispatcher;
use serde_json::{json, Value};
use tower::ServiceExt;

use super::keyboard::{KeyEvent, KeyboardHotkeys, KEY_DOWN, KEY_UP};
use super::*;
use crate::games::GameSetup;
use crate::motion::testing::OfflineHost;
use crate::motion::MotionHandle;
use crate::profile::{MainLoop, MainLoopHandle};
use crate::runtime::OutputMode;
use crate::shakers::{ShakersHandle, SilentBeep};
use crate::state::{OriginalPlatform, ServiceState};
use crate::store::ConfigStore;
use crate::telemetry::TelemetryHandle;

const WHEEL: &str = "Fanatec Wheel";
const CTRL: u16 = 29;
const SHIFT: u16 = 42;

struct Rig {
    ctx: ApiContext,
    inputs: JoystickInputs,
    events: JoystickEvents,
    buttons: ButtonHotkeys,
    keys: KeyboardHotkeys,
    tmp: tempfile::TempDir,
}

fn rig(platform: OriginalPlatform) -> Rig {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("export")).unwrap();
    let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), platform);
    let motion = MotionHandle::for_settings(state.config.read().settings(), Box::new(OfflineHost::default()));
    let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), false);
    let setup = GameSetup::discover(tmp.path(), &Default::default());
    let inputs = JoystickInputs::default();
    let ctx = ApiContext {
        state,
        telemetry: TelemetryHandle::new(Dispatcher::new(Vec::new())),
        motion,
        shakers,
        accessories: Arc::new(Accessories::new(std::time::Instant::now())),
        compensation: crate::compensation::VrCompensation::new(None, tmp.path().join("rig-pose")),
        main_loop: MainLoopHandle::new(MainLoop::new(setup, tmp.path().join("proc"))),
        srs_install: None,
        host_ip: "127.0.0.1".into(),
        voice: crate::voice::Voice::off(),
        ui: Arc::new(crate::api::ui_state::UiServices::off()),
        joystick: crate::joystick::JoystickService::offline(),
        hotkeys: Hotkeys::new(inputs.clone(), tmp.path()),
    };
    let events = inputs.events();
    inputs.connect(WHEEL);
    Rig { ctx, inputs, events, buttons: ButtonHotkeys::default(), keys: KeyboardHotkeys::default(), tmp }
}

impl Rig {
    fn bind(&self, action: &str, keys: &str, buttons: &str) {
        self.ctx.state.config.write().update(|c| {
            c.settings.shortcuts.bindings.insert(action.into(), Binding { keys: keys.into(), buttons: buttons.into() });
        });
    }

    /// One controller pass over what the fake controller sent since the previous one.
    fn pass(&mut self) {
        let events = self.events.take();
        axis_pass(&self.ctx, &mut self.buttons, &events);
    }

    fn click(&mut self, button: u32) {
        self.inputs.button_event(WHEEL, button, true);
        self.pass();
        self.inputs.button_event(WHEEL, button, false);
        self.pass();
    }

    fn key(&mut self, code: u16, value: i32) {
        key_pass(&self.ctx, &mut self.keys, KeyEvent { code, value });
    }

    /// Presses the keys in order, then releases them in reverse order.
    fn combo(&mut self, codes: &[u16]) {
        for code in codes {
            self.key(*code, KEY_DOWN);
        }
        for code in codes.iter().rev() {
            self.key(*code, KEY_UP);
        }
    }

    fn switches(&self) -> crate::runtime::Switches {
        self.ctx.state.controls.read().switches
    }
}

/// A controller button bound to `motion_box_switch` toggles motion once per press, from a fake event stream
/// through the joystick inputs; the gamepad table follows the button.
#[test]
fn controller_button_toggles_motion() {
    let mut r = rig(OriginalPlatform::Windows);
    r.bind("motion_box_switch", "", &format!("{WHEEL}^BTN_3"));
    r.pass();
    assert!(r.switches().motion);
    r.inputs.button_event(WHEEL, 3, true);
    r.pass();
    assert!(!r.switches().motion);
    assert_eq!(r.ctx.hotkeys.captured_button(WHEEL).as_deref(), Some("BTN_3"));
    r.inputs.button_event(WHEEL, 3, false);
    r.pass();
    assert!(!r.switches().motion, "a release does nothing");
    assert_eq!(r.ctx.hotkeys.captured_button(WHEEL), None);
    r.click(3);
    assert!(r.switches().motion);
}

/// The default keyboard bindings under the Windows rules: ctrl+shift+f1 cycles the wind, f5 the shakers,
/// f3 motion, f4 the LEDs, f11 the belt.
#[test]
fn default_keyboard_bindings() {
    let mut r = rig(OriginalPlatform::Windows);
    r.combo(&[CTRL, SHIFT, 59]);
    assert_eq!(r.switches().wind, OutputMode::AlwaysOn);
    r.combo(&[CTRL, SHIFT, 59]);
    assert_eq!(r.switches().wind, OutputMode::Off);
    r.combo(&[CTRL, SHIFT, 63]);
    assert!(!r.switches().shakers);
    r.combo(&[CTRL, SHIFT, 61]);
    assert!(!r.switches().motion);
    r.combo(&[CTRL, SHIFT, 62]);
    assert_eq!(r.switches().led, OutputMode::AlwaysOn);
    r.combo(&[CTRL, SHIFT, 87]);
    assert!(r.switches().belt);
}

/// The canned effects are held while their key or button is: the telemetry loop and the wind read them.
#[test]
fn canned_effects_are_held() {
    let mut r = rig(OriginalPlatform::Windows);
    r.bind("cannon_firing", "", &format!("{WHEEL}^BTN_0"));
    r.bind("ejection_seat", "ctrl+e", "");
    r.pass();
    r.inputs.button_event(WHEEL, 0, true);
    r.pass();
    let controls = r.ctx.state.controls.snapshot();
    assert_eq!(controls.canned.cannon_firing, Some(1));
    assert_eq!(
        crate::telemetry::runtime_inputs(&controls).cannon_firing,
        Some(openrigcue_model::native_value::LegacyValue::Int(1))
    );
    r.inputs.button_event(WHEEL, 0, false);
    r.pass();
    assert_eq!(r.ctx.state.controls.read().canned.cannon_firing, Some(0));
    r.key(CTRL, KEY_DOWN);
    assert_eq!(r.ctx.state.controls.read().canned.ejection_seat, Some(0));
    r.key(18, KEY_DOWN);
    assert_eq!(r.ctx.state.controls.read().canned.ejection_seat, Some(1));
    r.key(18, KEY_UP);
    assert_eq!(r.ctx.state.controls.read().canned.ejection_seat, Some(0));
}

/// Belt: the switch hotkey turns the belt off with its test mode and homing; calibration is asked only while
/// the belt is on.
#[test]
fn belt_switch_and_homing() {
    let mut r = rig(OriginalPlatform::Windows);
    let homing = |r: &Rig| r.ctx.accessories.belt_homing();
    r.combo(&[CTRL, SHIFT, 88]);
    assert_ne!(homing(&r), Homing::Requested, "belt off: no calibration");
    r.combo(&[CTRL, SHIFT, 87]);
    r.combo(&[CTRL, SHIFT, 88]);
    assert_eq!(homing(&r), Homing::Requested);
    r.ctx.state.controls.write().tests.belt_test = true;
    r.combo(&[CTRL, SHIFT, 87]);
    assert!(!r.switches().belt);
    assert!(!r.ctx.state.controls.read().tests.belt_test);
    assert_eq!(crate::accessories::homing_code(homing(&r)), 0);
}

/// Wind speed steps change the tune by `wind_power_increment`, clamped; motion homing asks the Yaw VR centre.
#[test]
fn wind_steps_and_motion_homing() {
    let mut r = rig(OriginalPlatform::Windows);
    r.ctx.state.config.write().update(|c| c.settings.wind.power_step = 30);
    r.ctx.state.profile.write().tune.power_wind_max_speed_perc = 80.0;
    let generation = r.ctx.state.profile.read().generation;
    let mut events = r.ctx.state.events.subscribe();
    r.combo(&[CTRL, SHIFT, 103]);
    assert_eq!(r.ctx.state.profile.read().tune.power_wind_max_speed_perc, 100.0);
    assert_eq!(events.try_recv().unwrap(), ServiceEvent::TuneChanged, "the UI re-reads the tune");
    r.combo(&[CTRL, SHIFT, 108]);
    assert_eq!(r.ctx.state.profile.read().tune.power_wind_max_speed_perc, 70.0);
    assert_eq!(r.ctx.state.profile.read().generation, generation + 2);
    r.combo(&[CTRL, SHIFT, 64]);
    assert!(r.ctx.motion.driver_settings().yaw_shared.get().homing_requested);
}

/// Telemetry hotkeys: capture and export toggle, reset clears the extremes and tells the UI, snapshot writes
/// the export table under the Windows rules only.
#[test]
fn telemetry_hotkeys() {
    let mut r = rig(OriginalPlatform::Windows);
    let mut events = r.ctx.state.events.subscribe();
    r.combo(&[CTRL, SHIFT, 65]);
    assert!(r.ctx.state.controls.read().telemetry.capture);
    r.combo(&[CTRL, SHIFT, 68]);
    assert!(r.ctx.state.controls.read().telemetry.export);
    r.combo(&[CTRL, SHIFT, 66]);
    assert_eq!(events.try_recv().unwrap(), ServiceEvent::MinMaxReset);
    r.combo(&[CTRL, SHIFT, 67]);
    let export = r.tmp.path().join("export");
    assert_eq!(std::fs::read_dir(&export).unwrap().count(), 0, "no game: no snapshot");
    r.ctx.state.session.write().game = Some("ac".into());
    r.combo(&[CTRL, SHIFT, 67]);
    let files: Vec<_> = std::fs::read_dir(&export).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(files.len(), 1);
    assert!(files[0].to_string_lossy().starts_with("snapshot_ac_"));
    let text = std::fs::read_to_string(export.join(&files[0])).unwrap();
    assert_eq!(text, "Variable,Value,Min,Max\r\n");
}

/// Slider hotkeys go through `ui_actions`: applied by the main-loop consumer only while a game runs, once per
/// pass whatever the number of presses, and the list is emptied either way.
#[test]
fn slider_hotkeys_through_ui_actions() {
    let mut r = rig(OriginalPlatform::Windows);
    r.bind("shake_box_engine_up", "", &format!("{WHEEL}^BTN_2"));
    r.ctx.state.profile.write().tune.shaker_effects.engine = 5.0;
    r.pass();
    r.click(2);
    consume_ui_actions(&r.ctx).unwrap();
    assert_eq!(r.ctx.state.profile.read().tune.shaker_effects.engine, 5.0, "stand-by");
    assert!(r.ctx.hotkeys.take_ui_actions().is_empty());
    r.ctx.state.session.write().game = Some("ac".into());
    r.click(2);
    r.click(2);
    let generation = r.ctx.state.profile.read().generation;
    let mut events = r.ctx.state.events.subscribe();
    consume_ui_actions(&r.ctx).unwrap();
    let profile = r.ctx.state.profile.snapshot();
    assert_eq!(profile.tune.shaker_effects.engine, 6.0);
    assert_eq!(profile.generation, generation + 1);
    assert_eq!(events.try_recv().unwrap(), ServiceEvent::TuneChanged, "the slider moves in the UI");
    assert!(events.try_recv().is_err());
    assert_eq!(serde_json::to_value(ServiceEvent::TuneChanged).unwrap(), json!({ "type": "tune_changed" }), "app.js");
}

#[test]
fn native_shaker_rebuild_error_ends_ui_actions_after_assignment_without_clearing_queue() {
    use openrigcue_model::native_value::LegacyValue;
    let r = rig(OriginalPlatform::Windows);
    r.ctx.state.session.write().game = Some("ac".into());
    r.ctx.state.profile.write().tune.shaker_effects.engine = 5.0;
    r.ctx
        .state
        .shaker_layout
        .write()
        .value
        .as_dict_mut()
        .unwrap()
        .get_mut("FL")
        .unwrap()
        .as_dict_mut()
        .unwrap()
        .get_mut("sfx")
        .unwrap()
        .assign_item("engine", LegacyValue::from("9"))
        .unwrap();
    r.ctx.hotkeys.queue("shake_box_engine_up");
    r.ctx.hotkeys.queue("motion_compensation_pitch_switch");
    let before_switch = r.ctx.state.config.read().settings().compensation.pitch.enabled;
    let mut events = r.ctx.state.events.subscribe();
    let error = consume_ui_actions(&r.ctx).unwrap_err();
    assert_eq!(error.to_string(), "TypeError: unsupported operand type(s) for /: 'str' and 'int'");
    assert_eq!(r.ctx.state.profile.read().tune.shaker_effects.engine, 6.0);
    assert_eq!(r.ctx.state.config.read().settings().compensation.pitch.enabled, before_switch);
    assert_eq!(r.ctx.hotkeys.ui_actions(), ["shake_box_engine_up", "motion_compensation_pitch_switch"]);
    assert!(events.try_recv().is_err(), "the failure precedes later service announcements");
}

/// The motion-compensation actions of `ui_actions`: the axis switches toggle; the
/// main switch is direct.
#[test]
fn compensation_actions() {
    let mut r = rig(OriginalPlatform::Windows);
    r.bind("motion_compensation_pitch_switch", "", &format!("{WHEEL}^BTN_4"));
    r.bind("motion_compensation_switch", "", &format!("{WHEEL}^BTN_5"));
    let before = r.ctx.state.config.read().settings().compensation.pitch.enabled;
    r.pass();
    r.click(4);
    consume_ui_actions(&r.ctx).unwrap();
    let after = r.ctx.state.config.read().settings().compensation.pitch.enabled;
    assert_ne!(after, before);
    r.click(5);
    assert!(!r.switches().compensation);
}

async fn call(ctx: &ApiContext, method: Method, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let request = Request::builder().method(method).uri(uri);
    let request = match body {
        Some(json) => request.header("content-type", "application/json").body(Body::from(json.to_string())).unwrap(),
        None => request.body(Body::empty()).unwrap(),
    };
    let response = crate::api::router(ctx.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// The editor API: the page lists with labels and bindings, Confirm, and the capture.
#[tokio::test]
async fn editor_api() {
    let mut r = rig(OriginalPlatform::Windows);
    let (status, body) = call(&r.ctx, Method::GET, "/api/v1/hotkeys", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["groups"][0]["page"], "Wind");
    assert_eq!(body["groups"][0]["actions"][0]["label"], "Cycle Wind L/R Mode");
    assert_eq!(body["groups"][0]["actions"][0]["keys"], "ctrl+shift+f1");
    assert_eq!(body["groups"][0]["actions"][0]["joystick"], "", "template.ini: power_wind_switch_btns is empty");
    assert_eq!(body["joysticks"], json!([WHEEL]));

    let (status, _) = call(&r.ctx, Method::POST, "/api/v1/hotkeys/capture", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(r.ctx.hotkeys.capturing());
    r.pass();
    r.inputs.button_event(WHEEL, 9, true);
    r.pass();
    r.key(CTRL, KEY_DOWN);
    r.key(30, KEY_DOWN);
    r.key(30, KEY_UP);
    let uri = format!("/api/v1/hotkeys/capture?joystick={}", WHEEL.replace(' ', "%20"));
    let (_, captured) = call(&r.ctx, Method::GET, &uri, None).await;
    assert_eq!(captured, json!({ "keys": "ctrl+a", "reads": 1, "button": "BTN_9" }));

    let change = json!({ "keys": "ctrl+a", "joystick": WHEEL, "button": "BTN_9" });
    let (status, _) = call(&r.ctx, Method::PUT, "/api/v1/hotkeys/bindings/led_switch", Some(change)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!r.ctx.hotkeys.capturing(), "Confirm closes the editor");
    let binding = r.ctx.state.config.read().settings().shortcuts.binding("led_switch").cloned().unwrap();
    assert_eq!(binding, Binding { keys: "ctrl+a".into(), buttons: format!("{WHEEL}^BTN_9") });
    let none = json!({ "keys": "", "joystick": "None", "button": "BTN_9" });
    call(&r.ctx, Method::PUT, "/api/v1/hotkeys/bindings/led_switch", Some(none)).await;
    assert_eq!(r.ctx.state.config.read().settings().shortcuts.binding("led_switch").unwrap().buttons, "");
    let (status, _) = call(&r.ctx, Method::PUT, "/api/v1/hotkeys/bindings/nope", Some(json!({}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Each opening of the editor starts a fresh capture: closing and reopening it with no key event in between
/// does not bring back the previous session's hotkey.
#[tokio::test]
async fn editor_reopened_starts_a_fresh_capture() {
    let mut r = rig(OriginalPlatform::Windows);
    let uri = "/api/v1/hotkeys/capture";
    call(&r.ctx, Method::POST, uri, None).await;
    r.combo(&[CTRL, 30]);
    assert_eq!(r.ctx.hotkeys.captured_keys(), Some(("ctrl+a".to_owned(), 1)));
    call(&r.ctx, Method::DELETE, uri, None).await;
    call(&r.ctx, Method::POST, uri, None).await;
    r.key(48, KEY_DOWN);
    let (_, captured) = call(&r.ctx, Method::GET, uri, None).await;
    assert_eq!(captured, json!({ "keys": null, "reads": 0, "button": null }), "nothing from the last session");
    r.key(48, KEY_UP);
    assert_eq!(r.ctx.hotkeys.captured_keys(), Some(("b".to_owned(), 1)));
    call(&r.ctx, Method::POST, uri, None).await;
    assert_eq!(r.ctx.hotkeys.captured_keys(), None, "opened again without closing: a new session too");
}

/// The controller capture walks the gamepad in insertion order: with two buttons held, the one that first sent an event
/// fills the field, not the first in sorted order.
#[test]
fn captured_button_is_the_first_inserted() {
    let mut r = rig(OriginalPlatform::Windows);
    r.ctx.hotkeys.set_capture(true);
    r.pass();
    r.inputs.button_event(WHEEL, 2, true);
    r.pass();
    r.inputs.button_event(WHEEL, 10, true);
    r.pass();
    assert_eq!(r.ctx.hotkeys.captured_button(WHEEL).as_deref(), Some("BTN_2"));
    r.inputs.button_event(WHEEL, 2, false);
    r.pass();
    assert_eq!(r.ctx.hotkeys.captured_button(WHEEL).as_deref(), Some("BTN_10"));
}

/// `APP.enable_sdl2 = no`: controller input is disabled, so no controller-button hotkey fires, the
/// editor sees no button and its controller list is empty (the
/// dropdown stays at 'None', disabled); keyboard hotkeys are not affected.
#[tokio::test]
async fn controller_hotkeys_need_enable_sdl2() {
    let mut r = rig(OriginalPlatform::Windows);
    assert_eq!(button_hotkeys(&r.ctx).update(&[], &Default::default(), &[], false), Default::default());
    assert_eq!(call(&r.ctx, Method::GET, "/api/v1/hotkeys", None).await.1["joysticks"], json!([WHEEL]));
    r.ctx.state.config.write().update(|c| c.settings.app.sdl2_joysticks = false);
    r.buttons = button_hotkeys(&r.ctx);
    let (_, view) = call(&r.ctx, Method::GET, "/api/v1/hotkeys", None).await;
    assert_eq!(view["joysticks"], json!([]), "the stub Joystick lists no controller");
    r.bind("motion_box_switch", "", &format!("{WHEEL}^BTN_3"));
    r.ctx.hotkeys.set_capture(true);
    r.pass();
    let motion = r.switches().motion;
    r.inputs.button_event(WHEEL, 3, true);
    r.pass();
    assert_eq!(r.switches().motion, motion, "the button does nothing");
    assert_eq!(r.ctx.hotkeys.captured_button(WHEEL), None);
    r.ctx.hotkeys.set_capture(false);
    let before = r.switches().shakers;
    r.combo(&[CTRL, SHIFT, 63]);
    assert_ne!(r.switches().shakers, before, "ctrl+shift+f5 still toggles the shakers");
}
