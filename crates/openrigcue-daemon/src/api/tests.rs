//! Requests through the router against a service state without threads or devices.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use openrigcue_config::settings::Configuration;
use openrigcue_effects::accessories::dispatcher::Accessories;
use openrigcue_effects::shakers::SampleBank;
use openrigcue_sources::derive::{Dispatcher, Listener, Rate};
use serde_json::{json, Value};
use tower::ServiceExt;

use super::{router, ApiContext};
use crate::games::GameSetup;
use crate::motion::testing::OfflineHost;
use crate::motion::MotionHandle;
use crate::profile::{MainLoop, MainLoopHandle};
use crate::shakers::{ShakersHandle, SilentBeep};
use crate::state::{OriginalPlatform, ServiceState};
use crate::store::ConfigStore;
use crate::telemetry::TelemetryHandle;

pub(super) struct Harness {
    pub(super) context: ApiContext,
    pub(super) _tmp: tempfile::TempDir,
}

pub(super) fn harness() -> Harness {
    let tmp = tempfile::tempdir().unwrap();
    let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
    let motion = MotionHandle::for_settings(state.config.read().settings(), Box::new(OfflineHost::default()));
    let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), false);
    let setup = GameSetup::discover(tmp.path(), &Default::default());
    let joystick = crate::joystick::JoystickService::new(
        state.clone(),
        Default::default(),
        crate::runtime::Shared::new("Default".into()),
    );
    let context = ApiContext {
        state,
        telemetry: TelemetryHandle::new(Dispatcher::new(Vec::new())),
        motion,
        shakers,
        accessories: Arc::new(Accessories::new(std::time::Instant::now())),
        compensation: crate::compensation::VrCompensation::new(None, tmp.path().join("rig-pose")),
        main_loop: MainLoopHandle::new(MainLoop::new(setup, tmp.path().join("proc"))),
        srs_install: None,
        host_ip: "192.168.1.10".into(),
        voice: crate::voice::Voice::off(),
        ui: Arc::new(crate::api::ui_state::UiServices::off()),
        joystick,
        hotkeys: crate::hotkeys::Hotkeys::offline(),
    };
    context
        .motion
        .driver_settings()
        .yaw_shared
        .update(|shared| shared.share_motion_sensor(context.accessories.motion_sensor_values()));
    Harness { context, _tmp: tmp }
}

#[tokio::test]
async fn standby_alert_confirmation_keeps_yes_and_requires_the_app_section() {
    use openrigcue_model::native_value::LegacyValue;
    let h = harness();
    let (code, _) = call(&h, Method::POST, "/api/v1/settings/hide-standby-alert", Some(json!({}))).await;
    assert_eq!(code, StatusCode::NO_CONTENT);
    assert_eq!(
        h.context
            .state
            .config
            .read()
            .settings()
            .native_snapshot()
            .get("APP")
            .unwrap()
            .item("disable_tuning_alert_standby")
            .unwrap(),
        &LegacyValue::Str("yes".into())
    );
    for missing in [false, true] {
        let h = harness();
        h.context.state.config.write().update(|config| {
            config.settings.edit_native(|raw| {
                if missing {
                    raw.remove("APP");
                } else {
                    raw.insert("APP", LegacyValue::None);
                }
            })
        });
        let before = h.context.state.config.read().settings().native_snapshot();
        let (code, body) = call(&h, Method::POST, "/api/v1/settings/hide-standby-alert", Some(json!({}))).await;
        assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR, "{}", String::from_utf8_lossy(&body));
        assert_eq!(h.context.state.config.read().settings().native_snapshot(), before);
    }
}

#[tokio::test]
async fn standby_alert_missing_stored_app_keeps_the_memory_assignment_and_file() {
    use openrigcue_model::native_value::LegacyValue;
    let h = harness();
    let directory = openrigcue_config::settings::paths::ConfigDir::at(h._tmp.path().join("configuration"));
    let store =
        ConfigStore::open(directory.clone(), openrigcue_config::import::migrate::Platform::Windows, None).unwrap();
    store.persist_first_run().unwrap();
    let before = b"[VENDOR]\nopaque = keep\n";
    std::fs::write(directory.settings_file(), before).unwrap();
    *h.context.state.config.write() = store;
    let (code, body) = call(&h, Method::POST, "/api/v1/settings/hide-standby-alert", Some(json!({}))).await;
    assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR, "{}", String::from_utf8_lossy(&body));
    assert_eq!(std::fs::read(directory.settings_file()).unwrap(), before);
    assert_eq!(
        h.context
            .state
            .config
            .read()
            .settings()
            .native_snapshot()
            .get("APP")
            .unwrap()
            .item("disable_tuning_alert_standby")
            .unwrap(),
        &LegacyValue::Str("yes".into())
    );
}

#[tokio::test]
async fn standby_alert_missing_stored_file_is_not_recreated() {
    use openrigcue_model::native_value::LegacyValue;
    let h = harness();
    let directory = openrigcue_config::settings::paths::ConfigDir::at(h._tmp.path().join("configuration"));
    let store =
        ConfigStore::open(directory.clone(), openrigcue_config::import::migrate::Platform::Windows, None).unwrap();
    store.persist_first_run().unwrap();
    std::fs::remove_file(directory.settings_file()).unwrap();
    *h.context.state.config.write() = store;
    let (code, _) = call(&h, Method::POST, "/api/v1/settings/hide-standby-alert", Some(json!({}))).await;
    assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(!directory.settings_file().exists());
    assert_eq!(
        h.context
            .state
            .config
            .read()
            .settings()
            .native_snapshot()
            .get("APP")
            .unwrap()
            .item("disable_tuning_alert_standby")
            .unwrap(),
        &LegacyValue::Str("yes".into())
    );
}

#[tokio::test]
async fn standby_alert_file_write_changes_only_the_stored_setting() {
    let h = harness();
    let directory = openrigcue_config::settings::paths::ConfigDir::at(h._tmp.path().join("configuration"));
    let store =
        ConfigStore::open(directory.clone(), openrigcue_config::import::migrate::Platform::Windows, None).unwrap();
    store.persist_first_run().unwrap();
    let text = std::fs::read_to_string(directory.settings_file()).unwrap();
    std::fs::write(directory.settings_file(), format!("{text}\n[VENDOR]\nopaque = keep\n")).unwrap();
    let stored_language =
        openrigcue_config::settings::Settings::load_ini_only(&directory.settings_file()).unwrap().app.language;
    let paths = [directory.profiles_file(), directory.metadata_file(), directory.registry_file()];
    let before = paths.each_ref().map(|path| std::fs::read(path).ok());
    *h.context.state.config.write() = store;
    h.context.state.config.write().update(|configuration| configuration.settings.app.language = "fr".into());
    let (code, _) = call(&h, Method::POST, "/api/v1/settings/hide-standby-alert", Some(json!({}))).await;
    assert_eq!(code, StatusCode::NO_CONTENT);
    let stored = openrigcue_config::settings::Settings::load_ini_only(&directory.settings_file()).unwrap();
    assert_eq!(stored.app.language, stored_language);
    assert!(stored.app.hide_standby_tuning_alert);
    assert_eq!(stored.native_snapshot().get("VENDOR").unwrap().item("opaque").unwrap().as_str(), Some("keep"));
    assert_eq!(h.context.state.config.read().settings().app.language, "fr");
    assert_eq!(paths.each_ref().map(|path| std::fs::read(path).ok()), before);
}

async fn call(h: &Harness, method: Method, uri: &str, body: Option<Value>) -> (StatusCode, Vec<u8>) {
    let request = Request::builder().method(method).uri(uri);
    let request = match body {
        Some(json) => request.header("content-type", "application/json").body(Body::from(json.to_string())).unwrap(),
        None => request.body(Body::empty()).unwrap(),
    };
    let response = router(h.context.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes().to_vec();
    (status, bytes)
}

async fn json_of(h: &Harness, method: Method, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let (status, bytes) = call(h, method, uri, body).await;
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// The sidebar view: stand-by session, initial switches, not connected.
#[tokio::test]
async fn status_reports_standby() {
    let h = harness();
    let (status, body) = json_of(&h, Method::GET, "/api/v1/status", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["session"]["game"], Value::Null);
    assert_eq!(body["controls"]["switches"]["motion"], true);
    assert_eq!(body["motion"]["link"]["status"], "NotConnected");
    assert_eq!(body["profile"]["game"], "none");
}

/// While the sidebar view waits for the configuration lock (a write holds it), no profile lock is held:
/// a profile write goes through. The profile save takes the configuration write lock first and then
/// the profile write lock, so a view holding the profile lock there deadlocks with it.
#[test]
fn status_holds_no_profile_lock_while_waiting_for_the_configuration() {
    use std::sync::mpsc;
    use std::time::Duration;
    let h = harness();
    let state = h.context.state.clone();
    let config_write = state.config.write();
    let (started_tx, started_rx) = mpsc::channel();
    let (status_tx, status_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        started_tx.send(()).unwrap();
        let _ = status_tx.send(runtime.block_on(json_of(&h, Method::GET, "/api/v1/status", None)));
    });
    started_rx.recv_timeout(Duration::from_secs(5)).expect("status request started");
    // Time for the request to reach the configuration lock, which the write above holds.
    std::thread::sleep(Duration::from_millis(300));
    let (written_tx, written_rx) = mpsc::channel();
    {
        let state = state.clone();
        std::thread::spawn(move || {
            drop(state.profile.write());
            let _ = written_tx.send(());
        });
    }
    assert!(
        written_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
        "the profile write waited for the status view, which holds the profile lock while waiting for the configuration"
    );
    drop(config_write);
    let (status, body) = status_rx.recv_timeout(Duration::from_secs(5)).expect("status answered once the write ends");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["profile"]["game"], "none");
}

/// The settings tree accepts in-memory scalar assignments. A malformed refresh rate is retained until the
/// motion loop converts it to an integer, rather than rejected at the service boundary.
#[tokio::test]
async fn settings_patch_and_problem_documents() {
    let h = harness();
    let (status, body) = json_of(&h, Method::PATCH, "/api/v1/settings", Some(json!({"motion": {"model": "P3"}}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["motion"]["model"], "P3");
    assert_eq!(h.context.state.config.read().settings().motion.model, "P3");
    let (status, value) =
        json_of(&h, Method::PATCH, "/api/v1/settings", Some(json!({"motion": {"refresh_rate": "fast"}}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["motion"]["refresh_rate"], "fast");
    assert_eq!(
        crate::motion::stream_pass(&h.context.state, &h.context.motion),
        Some(std::time::Duration::from_millis(500))
    );
    // The stream loop waits until the shared telemetry holds its `spotter_right` key; a completed
    // telemetry pass leaves the filled channels there and raises the readiness flag.
    *h.context.state.telemetry.write() = openrigcue_model::Telemetry::off();
    *h.context.state.telemetry_ready.write() = true;
    assert!(crate::motion::stream_pass(&h.context.state, &h.context.motion).is_none());
    let (status, bytes) = call(&h, Method::PATCH, "/api/v1/settings", Some(json!({"motion":[]}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let problem: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(problem["status"], 422);
}

/// Motion test sliders and the motion test toggle: sliders set, the toggle zeroes them.
#[tokio::test]
async fn motion_test_controls() {
    let h = harness();
    let (status, body) = json_of(&h, Method::PUT, "/api/v1/motion/test", Some(json!({"pitch": 40.0}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["tests"]["motion_sliders"]["pitch"], 40.0);
    let (_, body) = json_of(&h, Method::POST, "/api/v1/motion/test/toggle", None).await;
    assert_eq!(body["tests"]["motion_test"], true);
    assert_eq!(body["tests"]["motion_sliders"]["pitch"], 0.0);
    assert!(h.context.state.controls.read().tests.motion_test);
}

/// Telemetry page row click: a new chart channel starts a new history.
#[tokio::test]
async fn plot_channel_change() {
    let h = harness();
    let (status, body) =
        json_of(&h, Method::PATCH, "/api/v1/controls", Some(json!({"telemetry": {"plot_channel": "rpm"}}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["telemetry"]["plot_channel"], "rpm");
    let (_, history) = json_of(&h, Method::GET, "/api/v1/telemetry/history", None).await;
    assert_eq!(history, json!([]));
}

struct TelemetryFrame;
impl Listener for TelemetryFrame {
    fn id(&self) -> &str {
        "api"
    }
    fn is_running(&mut self) -> bool {
        true
    }
    fn receive(&mut self) {}
    fn channel(&self, name: &str) -> Option<f64> {
        Some(match name {
            "speed" => 30.0,
            "rpm" => 5000.0,
            "max_rpm" => 8000.0,
            "gear" => 3.0,
            _ => 0.0,
        })
    }
    fn rate(&self, _: Rate) -> Option<f64> {
        Some(0.0)
    }
    fn vehicle(&self) -> Option<String> {
        Some("Car".into())
    }
    fn location(&self) -> Option<String> {
        Some("Track".into())
    }
}

#[tokio::test]
async fn history_read_and_plot_change_finish_while_dispatcher_is_busy() {
    let mut h = harness();
    h.context.telemetry = TelemetryHandle::new(Dispatcher::new(vec![Box::new(TelemetryFrame)]));
    h.context.state.controls.write().telemetry.plot_channel = Some("rpm".into());
    let parts = crate::telemetry::TelemetryParts {
        dispatcher: h.context.telemetry.clone(),
        snapshot: Default::default(),
        context: Default::default(),
        outputs: Default::default(),
        joystick_profile: crate::runtime::Shared::new("Default".into()),
    };
    crate::telemetry::main_loop_tick(&h.context.state, &parts, &crate::telemetry::CatalogDirectory::default());
    let (independent, cleared_while_busy, ((history_status, history), (status, controls))) =
        std::thread::scope(|scope| {
            let locked = h.context.telemetry.lock();
            assert_eq!(locked.history().len(), 1);
            let (done, completed) = std::sync::mpsc::channel();
            let h = &h;
            let reader = scope.spawn(move || {
                let result =
                    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
                        let history = json_of(h, Method::GET, "/api/v1/telemetry/history", None).await;
                        let changed = json_of(
                            h,
                            Method::PATCH,
                            "/api/v1/controls",
                            Some(json!({"telemetry": {"plot_channel": "speed"}})),
                        )
                        .await;
                        (history, changed)
                    });
                done.send(()).unwrap();
                result
            });
            let independent = completed.recv_timeout(std::time::Duration::from_secs(1));
            let cleared = locked.history().is_empty();
            drop(locked);
            (independent, cleared, reader.join().unwrap())
        });
    assert!(independent.is_ok(), "history read or plot change waited for dispatcher mutex");
    assert_eq!(history_status, StatusCode::OK);
    assert_eq!(history, json!([5000.0]));
    assert_eq!(status, StatusCode::OK);
    assert_eq!(controls["telemetry"]["plot_channel"], "speed");
    assert!(cleared_while_busy, "the dispatcher and UI must use the same history");
    crate::telemetry::main_loop_tick(&h.context.state, &parts, &crate::telemetry::CatalogDirectory::default());
    let (_, history) = json_of(&h, Method::GET, "/api/v1/telemetry/history", None).await;
    assert_eq!(history, json!([30]), "the next append retains the cleared history and native integer speed");
}

#[test]
fn minmax_table_and_resets_finish_while_dispatcher_is_busy() {
    let mut h = harness();
    h.context.telemetry = TelemetryHandle::new(Dispatcher::new(vec![Box::new(TelemetryFrame)]));
    h.context.state.config.write().update(|config| config.settings.app.show_all_telemetry_channels = true);
    let parts = crate::telemetry::TelemetryParts {
        dispatcher: h.context.telemetry.clone(),
        snapshot: Default::default(),
        context: Default::default(),
        outputs: Default::default(),
        joystick_profile: crate::runtime::Shared::new("Default".into()),
    };
    let tick =
        || crate::telemetry::main_loop_tick(&h.context.state, &parts, &crate::telemetry::CatalogDirectory::default());
    for use_hotkey in [false, true] {
        tick();
        let (independent, cleared_while_busy, (status, before, after)) = std::thread::scope(|scope| {
            let locked = h.context.telemetry.lock();
            assert_eq!(locked.extremes().get("speed").unwrap().max, 30.0);
            let (done, completed) = std::sync::mpsc::channel();
            let h = &h;
            let reader = scope.spawn(move || {
                let result =
                    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
                        let (_, before) = json_of(h, Method::GET, "/api/v1/telemetry/table", None).await;
                        let status = if use_hotkey {
                            crate::hotkeys::actions::run(&h.context, h._tmp.path(), "telemetry_reset");
                            StatusCode::NO_CONTENT
                        } else {
                            call(h, Method::POST, "/api/v1/telemetry/reset", None).await.0
                        };
                        let (_, after) = json_of(h, Method::GET, "/api/v1/telemetry/table", None).await;
                        (status, before, after)
                    });
                done.send(()).unwrap();
                result
            });
            let independent = completed.recv_timeout(std::time::Duration::from_secs(1));
            let cleared = locked.extremes().get("speed").is_none();
            drop(locked);
            (independent, cleared, reader.join().unwrap())
        });
        assert!(independent.is_ok(), "min/max view or reset waited for dispatcher mutex");
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(cleared_while_busy, "reset must clear the dispatcher's authoritative table");
        assert_eq!(before.as_array().unwrap().len(), openrigcue_sources::derive::CHANNEL_SPECS.len());
        let speed = |rows: &Value| rows.as_array().unwrap().iter().find(|row| row["label"] == "Speed").unwrap().clone();
        assert_eq!(speed(&before)["min"], "108.000000");
        assert_eq!(speed(&after)["min"], "0");
        assert_eq!(speed(&after)["value"], "108.000000");
    }
}

#[test]
fn intellitune_read_and_reset_finish_while_dispatcher_is_busy() {
    use openrigcue_sources::derive::{GameDirectory, IntellituneReference};
    struct CaptureGames;
    impl GameDirectory for CaptureGames {
        fn is_flying(&self, _: &str) -> bool {
            false
        }
        fn intellitune_reference(&self, _: &str) -> Option<IntellituneReference> {
            crate::telemetry::CatalogDirectory::default().intellitune_reference("dirtrally2.0")
        }
    }
    let mut h = harness();
    h.context.telemetry = TelemetryHandle::new(Dispatcher::new(vec![Box::new(TelemetryFrame)]));
    h.context.state.controls.write().telemetry.intellitune_capture = true;
    let parts = crate::telemetry::TelemetryParts {
        dispatcher: h.context.telemetry.clone(),
        snapshot: Default::default(),
        context: Default::default(),
        outputs: Default::default(),
        joystick_profile: crate::runtime::Shared::new("Default".into()),
    };
    let tick = || crate::telemetry::main_loop_tick(&h.context.state, &parts, &CaptureGames);
    tick();
    let (independent, cleared_while_busy, (status, before, after)) = std::thread::scope(|scope| {
        let locked = h.context.telemetry.lock();
        assert_eq!(locked.intellitune().native_values().len(), 7);
        let (done, completed) = std::sync::mpsc::channel();
        let h = &h;
        let reader = scope.spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
                let (_, before) = json_of(h, Method::GET, "/api/v1/intellitune", None).await;
                let status = call(h, Method::DELETE, "/api/v1/intellitune", None).await.0;
                let (_, after) = json_of(h, Method::GET, "/api/v1/intellitune", None).await;
                (status, before, after)
            });
            done.send(()).unwrap();
            result
        });
        let independent = completed.recv_timeout(std::time::Duration::from_secs(1));
        let cleared = locked.intellitune().native_values().is_empty();
        drop(locked);
        (independent, cleared, reader.join().unwrap())
    });
    assert!(independent.is_ok(), "IntelliTune read or reset waited for dispatcher mutex");
    assert!(cleared_while_busy, "reset must clear the actual captured samples");
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(before.as_object().unwrap().len(), 7);
    assert_eq!(after, json!({}));
    tick();
    assert!(h.context.telemetry.lock().intellitune().native_values().iter().all(|(_, value)| {
        matches!(value, openrigcue_model::native_value::LegacyValue::List(values) if values.len() == 1)
    }));
}

#[test]
fn profile_edit_and_clear_do_not_wait_or_replay_over_newer_listener_identity() {
    use openrigcue_model::native_value::LegacyValue;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;
    struct Blocking {
        id: &'static str,
        before_identity: bool,
        entered: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
        paused: AtomicBool,
    }
    impl Blocking {
        fn pause(&self) {
            if !self.paused.swap(true, Ordering::Relaxed) {
                self.entered.send(()).unwrap();
                self.release.lock().unwrap().recv_timeout(Duration::from_secs(5)).unwrap();
            }
        }
    }
    impl Listener for Blocking {
        fn id(&self) -> &str {
            self.id
        }
        fn is_running(&mut self) -> bool {
            if self.before_identity {
                self.pause();
            }
            true
        }
        fn receive(&mut self) {}
        fn channel(&self, name: &str) -> Option<f64> {
            if !self.before_identity && name == "rpm" {
                self.pause();
            }
            TelemetryFrame.channel(name)
        }
        fn rate(&self, rate: Rate) -> Option<f64> {
            TelemetryFrame.rate(rate)
        }
        fn vehicle(&self) -> Option<String> {
            TelemetryFrame.vehicle()
        }
        fn location(&self) -> Option<String> {
            TelemetryFrame.location()
        }
    }
    for (id, before_identity, clear_edit) in
        [("api", true, false), ("api", false, false), ("api", true, true), ("api", false, true), ("dcs", true, true)]
    {
        let mut h = harness();
        let (entered, waiting) = mpsc::channel();
        let (release, resume) = mpsc::channel();
        h.context.telemetry = TelemetryHandle::new(Dispatcher::new(vec![Box::new(Blocking {
            id,
            before_identity,
            entered,
            release: Mutex::new(resume),
            paused: AtomicBool::new(false),
        })]));
        let parts = crate::telemetry::TelemetryParts {
            dispatcher: h.context.telemetry.clone(),
            snapshot: Default::default(),
            context: Default::default(),
            outputs: Default::default(),
            joystick_profile: crate::runtime::Shared::new("Default".into()),
        };
        if id == "dcs" {
            let game = openrigcue_config::catalog::Catalog::builtin().game("dcs").unwrap();
            parts.snapshot.write().windows = Some(vec![game.window_name.clone().unwrap()]);
        }
        let (independent, during) = std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                crate::telemetry::main_loop_tick(
                    &h.context.state,
                    &parts,
                    &crate::telemetry::CatalogDirectory::default(),
                )
            });
            waiting.recv_timeout(Duration::from_secs(5)).unwrap();
            let (done, completed) = mpsc::channel();
            let h = &h;
            let editor = scope.spawn(move || {
                tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
                    super::ui_state::begin_edit_native(&h.context, "ac".into(), LegacyValue::Int(5)).await.unwrap();
                    if clear_edit {
                        assert_eq!(
                            call(h, Method::DELETE, "/api/v1/profile/edit", None).await.0,
                            StatusCode::NO_CONTENT
                        );
                    }
                });
                done.send(()).unwrap();
            });
            let independent = completed.recv_timeout(Duration::from_secs(1));
            let during = h.context.state.session.read().vehicle_value();
            release.send(()).unwrap();
            worker.join().unwrap();
            editor.join().unwrap();
            (independent, during)
        });
        let after = h.context.state.session.read().vehicle_value();
        let expected = if before_identity { LegacyValue::Str("Car".into()) } else { LegacyValue::Int(5) };
        assert!(
            independent.is_ok() && after == expected,
            "before_identity={before_identity}, clear={clear_edit}, completion={independent:?}, after={after:?}"
        );
        assert_eq!(during, LegacyValue::Int(if before_identity && clear_edit { 0 } else { 5 }));
        assert_eq!(
            h.context.telemetry.lock().vehicle_value(),
            &expected,
            "working compatibility identity catches up without republishing"
        );
        if id == "dcs" {
            assert_eq!(h.context.state.session.read().game, None);
            assert_eq!(parts.context.read().game_running, None);
            let dispatcher = h.context.telemetry.lock();
            assert_eq!(dispatcher.game_id(), None);
            assert!(
                dispatcher.extremes().get("speed").is_none(),
                "the current stand-by game clears extremes at finish"
            );
        }
    }
}

/// Console page Manual mode: the row key's catalog game is forced, testmode clears it.
#[tokio::test]
async fn manual_detection() {
    use openrigcue_config::import::value::LegacyValue;
    let h = harness();
    h.context.telemetry.publish_vehicle_value(&h.context.state, LegacyValue::Bool(false), None);
    let (status, _) =
        call(&h, Method::PUT, "/api/v1/games/detection", Some(json!({"entry": "dirtrally2.0_steam"}))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(h.context.state.controls.read().forced_game.as_deref(), Some("dirtrally2.0"));
    assert_eq!(h.context.state.session.read().vehicle_value(), LegacyValue::Int(0));
    assert_eq!(h.context.telemetry.lock().vehicle_value(), &LegacyValue::Int(0));
    h.context.telemetry.publish_vehicle_value(
        &h.context.state,
        "Keep on Stand By".into(),
        Some("Keep on Stand By".into()),
    );
    call(&h, Method::PUT, "/api/v1/games/detection", Some(json!({"entry": "testmode"}))).await;
    assert_eq!(h.context.state.controls.read().forced_game, None);
    assert_eq!(h.context.state.session.read().vehicle_value(), LegacyValue::Str("Keep on Stand By".into()));
    call(&h, Method::PUT, "/api/v1/games/detection", Some(json!({"entry": null}))).await;
    assert_eq!(h.context.state.session.read().vehicle_value(), LegacyValue::Str("Keep on Stand By".into()));
}

/// Save at vehicle level needs a known vehicle; at game level it stores the active tune.
#[tokio::test]
async fn profile_save_levels() {
    let h = harness();
    let (status, _) = call(&h, Method::POST, "/api/v1/profile/save", Some(json!({"level": "vehicle"}))).await;
    assert_eq!(status, StatusCode::CONFLICT);
    call(&h, Method::PATCH, "/api/v1/profile", Some(json!({"gear_shift_perc": 91.0}))).await;
    let (status, _) = call(&h, Method::POST, "/api/v1/profile/save", Some(json!({"level": "game"}))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let stored = h.context.state.config.read().config().profiles.get("emulator").unwrap().gear_shift_perc;
    assert_eq!(stored, 91.0);
}

/// Export then import a profile while no game runs: the import opens the game for editing.
#[tokio::test]
async fn export_and_import_a_profile() {
    let h = harness();
    let (status, bytes) = call(&h, Method::GET, "/api/v1/profiles/export?game=dirtrally2.0", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(bytes.starts_with(b"PK"));
    let request =
        Request::builder().method(Method::POST).uri("/api/v1/profiles/import").body(Body::from(bytes)).unwrap();
    let response = router(h.context.clone()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(h.context.state.controls.read().forced_game.as_deref(), Some("dirtrally2.0"));
    let (status, _) = call(&h, Method::GET, "/api/v1/profiles/export?game=nonsense", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// The import dialog compares the listener's value before converting anything to display text.
#[tokio::test]
async fn import_offer_compares_native_vehicle_values() {
    use openrigcue_config::import::value::LegacyValue;
    use std::io::{Cursor, Write};

    let h = harness();
    h.context.state.session.write().game = Some("ac".into());
    for (current, file_vehicle, expected) in [
        (LegacyValue::None, json!(null), StatusCode::NO_CONTENT),
        (LegacyValue::None, json!("None"), StatusCode::CONFLICT),
        (LegacyValue::Int(5), json!(5.0), StatusCode::NO_CONTENT),
        (LegacyValue::Int(5), json!("5"), StatusCode::CONFLICT),
        (LegacyValue::Bool(true), json!(1), StatusCode::NO_CONTENT),
        (LegacyValue::List(vec![LegacyValue::Int(7)]), json!([7.0]), StatusCode::NO_CONTENT),
    ] {
        let projection = Some(openrigcue_model::python_repr::str_value(&current));
        h.context.state.session.write().set_vehicle_value(current.clone(), projection);
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        zip.start_file("fixture.srs", zip::write::SimpleFileOptions::default()).unwrap();
        zip.write_all(json!({"game":"ac", "vehicle":file_vehicle, "srs_version":"fixture"}).to_string().as_bytes())
            .unwrap();
        let bytes = zip.finish().unwrap().into_inner();
        let response = router(h.context.clone())
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/profiles/import-check")
                    .body(Body::from(bytes))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "{current:?} versus {file_vehicle}");
    }
}

#[tokio::test]
async fn integer_import_retains_the_storage_key_and_native_edit_context() {
    use openrigcue_config::import::value::{Key, LegacyValue};
    use std::io::{Cursor, Write};
    let h = harness();
    let mut value = openrigcue_config::tunes::FactoryTunes::builtin().get("ac").unwrap().to_json();
    let fields = value.as_object_mut().unwrap();
    fields.insert("game".into(), json!("ac"));
    fields.insert("vehicle".into(), json!(5));
    fields.insert("srs_version".into(), json!("fixture"));
    fields.insert("gear_shift_perc".into(), json!(43));
    let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
    archive.start_file("fixture.srs", zip::write::SimpleFileOptions::default()).unwrap();
    archive.write_all(value.to_string().as_bytes()).unwrap();
    let bytes = archive.finish().unwrap().into_inner();
    let response = router(h.context.clone())
        .oneshot(
            Request::builder().method(Method::POST).uri("/api/v1/profiles/import").body(Body::from(bytes)).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let row = h.context.state.config.read().profile_row("ac").unwrap().unwrap();
    let vehicles = row.as_dict().unwrap().get("vehicles").unwrap().as_dict().unwrap();
    assert!(vehicles.get_key(&Key::Int(5)).is_some());
    assert!(!vehicles.contains("5"));
    assert_eq!(h.context.state.session.read().vehicle_value(), LegacyValue::Int(5));
    assert_eq!(h.context.telemetry.lock().vehicle_value(), &LegacyValue::Int(5));
    h.context.state.session.write().game = Some("ac".into());
    h.context.main_loop.lock().unwrap().synchronize_profile(&h.context).unwrap();
    assert_eq!(h.context.state.profile.read().level, openrigcue_config::profiles::ProfileLevel::Vehicle);
    assert_eq!(h.context.state.profile.read().tune.gear_shift_perc, 43.0);
}

#[tokio::test]
async fn snapshot_keeps_native_vehicle_concatenation_errors() {
    use openrigcue_config::import::value::LegacyValue;
    let mut h = harness();
    h.context.hotkeys = crate::hotkeys::Hotkeys::new(Default::default(), h._tmp.path());
    std::fs::create_dir(h.context.hotkeys.export_dir()).unwrap();
    h.context.state.session.write().game = Some("ac".into());
    for value in [LegacyValue::None, LegacyValue::Int(5), LegacyValue::List(Vec::new())] {
        let projection = Some(openrigcue_model::python_repr::str_value(&value));
        h.context.state.session.write().set_vehicle_value(value, projection);
        let (status, body) = json_of(&h, Method::POST, "/api/v1/telemetry/snapshot", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!(["ERROR", crate::hotkeys::snapshot::SNAPSHOT_ERROR]));
        crate::hotkeys::actions::run(&h.context, h.context.hotkeys.export_dir(), "telemetry_snapshot");
    }
    assert_eq!(std::fs::read_dir(h.context.hotkeys.export_dir()).unwrap().count(), 0);
    h.context.state.session.write().set_vehicle_value(LegacyValue::Str("None".into()), Some("None".into()));
    let (_, body) = json_of(&h, Method::POST, "/api/v1/telemetry/snapshot", None).await;
    assert_eq!(body[0], "OK");
    assert!(body[1].as_str().unwrap().contains("snapshot_ac_None_"));
}

#[tokio::test]
async fn snapshot_reads_the_completed_export_table_while_dispatcher_is_busy() {
    use openrigcue_sources::derive::{Listener, Rate};
    struct Frame;
    impl Listener for Frame {
        fn id(&self) -> &str {
            "api"
        }
        fn is_running(&mut self) -> bool {
            true
        }
        fn receive(&mut self) {}
        fn channel(&self, name: &str) -> Option<f64> {
            Some(match name {
                "speed" => 30.0,
                "rpm" => 5000.0,
                "max_rpm" => 8000.0,
                "gear" => 3.0,
                _ => 0.0,
            })
        }
        fn rate(&self, _: Rate) -> Option<f64> {
            Some(0.0)
        }
        fn vehicle(&self) -> Option<String> {
            Some("Car".into())
        }
        fn location(&self) -> Option<String> {
            None
        }
    }
    let mut h = harness();
    h.context.hotkeys = crate::hotkeys::Hotkeys::new(Default::default(), h._tmp.path());
    std::fs::create_dir(h.context.hotkeys.export_dir()).unwrap();
    h.context.telemetry = TelemetryHandle::new(Dispatcher::new(vec![Box::new(Frame)]));
    let parts = crate::telemetry::TelemetryParts {
        dispatcher: h.context.telemetry.clone(),
        snapshot: Default::default(),
        context: Default::default(),
        outputs: Default::default(),
        joystick_profile: crate::runtime::Shared::new("Default".into()),
    };
    crate::telemetry::main_loop_tick(&h.context.state, &parts, &crate::telemetry::CatalogDirectory::default());
    let expected = crate::hotkeys::snapshot::contents(h.context.telemetry.lock().export_rows());
    let locked = h.context.telemetry.lock();
    let (done, completed) = std::sync::mpsc::channel();
    let (independent, result) = std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            let result = crate::hotkeys::snapshot::take(&h.context);
            done.send(()).unwrap();
            result
        });
        let independent = completed.recv_timeout(std::time::Duration::from_secs(1));
        drop(locked);
        (independent, reader.join().unwrap())
    });
    assert!(independent.is_ok(), "snapshot waited for the dispatcher mutex");
    let path = result.unwrap().unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), expected);
}

/// Games page: the Console table and the port editor's refusal while a game runs.
#[tokio::test]
async fn games_page() {
    let h = harness();
    let (status, rows) = json_of(&h, Method::GET, "/api/v1/games?page=Console", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rows["rows"][0]["key"], "testmode");
    h.context.state.session.write().game = Some("dirt4".into());
    // The port change confirmation checks nothing: the port changes in memory while the game runs.
    call(&h, Method::PUT, "/api/v1/games/dirt4/port", Some(json!({"port": 20999}))).await;
    assert_eq!(h.context.state.config.read().settings().network_ports.get("dirt"), Some(20999));
    let (status, _) = call(&h, Method::POST, "/api/v1/games/dirt4/patch", None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    // The port dialog's hint follows the entry's own type, not the Console page's `instructions` status.
    let rows = rows["rows"].as_array().unwrap();
    assert!(rows.iter().any(|r| r["status"] == "instructions" && r["entry_type"] != "instructions"));
}

#[tokio::test]
async fn port_forwarding_confirmation_writes_only_the_native_forwarding_section() {
    use openrigcue_config::settings::{paths::ConfigDir, Settings};
    use openrigcue_model::native_value::LegacyValue;
    let h = harness();
    let dir = ConfigDir::at(h._tmp.path().join("port-config"));
    let mut store = ConfigStore::load(dir.clone()).unwrap();
    store.save().unwrap();
    let mut stored = Settings::load_ini_for_native_edit(&dir.settings_file()).unwrap();
    stored.edit_native(|raw| {
        raw.get_mut("APP").unwrap().as_dict_mut().unwrap().insert("language", "stored-language".into());
        raw.insert("VENDOR", LegacyValue::Str("stored-foreign-value".into()));
    });
    stored.write_ini_only(&dir.settings_file()).unwrap();
    let before_settings = Settings::load_ini_for_native_edit(&dir.settings_file()).unwrap().native_snapshot();
    let companions = [dir.profiles_file(), dir.metadata_file(), dir.registry_file()];
    let before = companions.each_ref().map(|path| std::fs::read(path).unwrap());
    store.update(|config| {
        config.settings.app.language = "unsaved-language".into();
        config.settings.edit_native(|raw| {
            raw.get_mut("PORT_FORWARDING").unwrap().as_dict_mut().unwrap().insert("other", "single-target".into());
        });
    });
    *h.context.state.config.write() = store;
    let (status, answer) = json_of(
        &h,
        Method::PUT,
        "/api/v1/games/gt7/port",
        Some(json!({"port": 20999, "forwarding": ["127.0.0.1:1234"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(answer["restart_required"], true);
    let after = Settings::load_ini_for_native_edit(&dir.settings_file()).unwrap().native_snapshot();
    let mut expected = before_settings;
    expected.insert(
        "PORT_FORWARDING",
        h.context.state.config.read().settings().native_snapshot().get("PORT_FORWARDING").unwrap().clone(),
    );
    assert_eq!(after, expected);
    assert_eq!(companions.each_ref().map(|path| std::fs::read(path).unwrap()), before);
    let store = h.context.state.config.read();
    assert_eq!(store.settings().network_ports.get("gt7"), Some(20999));
    assert_eq!(store.settings().app.language, "unsaved-language");
}

#[tokio::test]
async fn port_forwarding_confirmation_preserves_native_assignments_and_required_parents() {
    use openrigcue_model::native_value::LegacyValue;
    let h = harness();
    let (status, _) = json_of(
        &h,
        Method::PUT,
        "/api/v1/games/gt7/port",
        Some(json!({"port": 20999, "forwarding": ["127.0.0.1:1234"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        h.context.state.config.read().settings().native_snapshot().get("NETWORK_PORT").unwrap().item("gt7").unwrap(),
        &LegacyValue::Int(20999)
    );
    for section in ["PORT_FORWARDING", "NETWORK_PORT"] {
        let h = harness();
        h.context.state.config.write().update(|configuration| {
            configuration.settings.edit_native(|raw| raw.insert(section, LegacyValue::None));
        });
        let before = h.context.state.config.read().settings().native_snapshot();
        let (status, answer) = json_of(
            &h,
            Method::PUT,
            "/api/v1/games/gt7/port",
            Some(json!({"port": 20999, "forwarding": ["127.0.0.1:1234"]})),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{section}: {answer}");
        assert!(answer["detail"].as_str().unwrap().starts_with("TypeError:"));
        assert!(h.context.state.config.read().settings().native_snapshot() == before);
    }
}

#[tokio::test]
async fn port_forwarding_confirmation_continues_after_a_caught_file_error() {
    use openrigcue_config::settings::paths::ConfigDir;
    let h = harness();
    let dir = ConfigDir::at(h._tmp.path().join("port-config"));
    let store = ConfigStore::load(dir.clone()).unwrap();
    store.save().unwrap();
    let before = std::fs::read(dir.profiles_file()).unwrap();
    std::fs::remove_file(dir.settings_file()).unwrap();
    std::fs::create_dir(dir.settings_file()).unwrap();
    *h.context.state.config.write() = store;
    let (status, answer) = json_of(
        &h,
        Method::PUT,
        "/api/v1/games/gt7/port",
        Some(json!({"port": 20999, "forwarding": ["127.0.0.1:1234"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(answer["restart_required"], true);
    assert_eq!(h.context.state.config.read().settings().network_ports.get("gt7"), Some(20999));
    assert_eq!(h.context.state.config.read().settings().port_forwarding["gt7"], ["127.0.0.1:1234"]);
    assert!(dir.settings_file().is_dir());
    assert_eq!(std::fs::read(dir.profiles_file()).unwrap(), before);
}

/// "Game Page" / "Instructions" hide a notification in the service (nothing remembered); Dismiss remembers
/// the running game in the game notification table.
#[tokio::test]
async fn notification_buttons() {
    use openrigcue_config::patch::GameStatus;
    let h = harness();
    h.context.state.notifications.write().check_game_config("beamng", &GameStatus::Instructions, false);
    let (_, items) = json_of(&h, Method::GET, "/api/v1/notifications", None).await;
    let id = items[0]["id"].as_u64().unwrap();
    let (status, _) = call(&h, Method::POST, &format!("/api/v1/notifications/{id}/hide"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, items) = json_of(&h, Method::GET, "/api/v1/notifications", None).await;
    assert_eq!(items, json!([]));
    assert!(!h.context.state.config.read().config().dismissals.game_setup_dismissed("beamng"));
    let (status, _) = call(&h, Method::POST, &format!("/api/v1/notifications/{id}/hide"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    h.context.state.notifications.write().check_game_config("dirt4", &GameStatus::NotPatched, false);
    h.context.state.session.write().game = Some("dirt4".into());
    let (_, items) = json_of(&h, Method::GET, "/api/v1/notifications", None).await;
    let id = items[0]["id"].as_u64().unwrap();
    let (status, _) = call(&h, Method::DELETE, &format!("/api/v1/notifications/{id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(h.context.state.config.read().config().dismissals.game_setup_dismissed("dirt4"));
}

#[tokio::test]
async fn notification_dismissal_keeps_hidden_state_and_continues_after_storage_failure() {
    use openrigcue_config::patch::GameStatus;
    use openrigcue_config::settings::paths::ConfigDir;
    let h = harness();
    let dir = ConfigDir::at(h._tmp.path().join("notification-config"));
    let store = crate::store::ConfigStore::load(dir.clone()).unwrap();
    store.save().unwrap();
    *h.context.state.config.write() = store;
    h.context.state.session.write().game = Some("dirt4".into());
    h.context.state.notifications.write().check_game_config("dirt4", &GameStatus::NotPatched, false);
    let id = h.context.state.notifications.read().items[0].id;
    rusqlite::Connection::open(dir.metadata_file()).unwrap().execute_batch(
        "CREATE TRIGGER reject_dismissal BEFORE INSERT ON game_config_notifications BEGIN SELECT RAISE(ABORT,'fixture dismissal failure'); END;"
    ).unwrap();
    assert_eq!(call(&h, Method::DELETE, &format!("/api/v1/notifications/{id}"), None).await.0, StatusCode::NO_CONTENT);
    assert!(h.context.state.notifications.read().items.is_empty());
    assert_eq!(h.context.state.config.read().metadata_row("game_config_notifications", "dirt4").unwrap(), None);
    assert!(!h.context.state.config.read().config().dismissals.game_setup_dismissed("dirt4"));
}

#[tokio::test]
async fn mc_calibration_requires_native_parents_and_keeps_the_completed_prefix() {
    use openrigcue_model::native_value::LegacyValue;
    for (axis, missing_axis) in [("pitch", false), ("pitch", true)] {
        let h = harness();
        h.context.state.config.write().update(|configuration| {
            configuration.settings.edit_native(|raw| {
                let mc = raw.get_mut("MOTION_COMPENSATION").unwrap().as_dict_mut().unwrap();
                if missing_axis {
                    mc.remove(axis);
                } else {
                    mc.insert(axis, LegacyValue::None);
                }
            });
        });
        let before = h.context.state.config.read().settings().native_snapshot();
        let (status, answer) =
            json_of(&h, Method::POST, "/api/v1/compensation/actions/pitch_calibrate", Some(json!({}))).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
        let expected = if missing_axis {
            "KeyError: 'pitch'"
        } else {
            "TypeError: 'NoneType' object does not support item assignment"
        };
        assert_eq!(answer["detail"], expected);
        assert!(h.context.state.config.read().settings().native_snapshot() == before);
    }
    for missing in [false, true] {
        let h = harness();
        h.context.state.config.write().update(|configuration| {
            configuration.settings.edit_native(|raw| {
                if missing {
                    raw.remove("MOTION_COMPENSATION");
                } else {
                    raw.insert("MOTION_COMPENSATION", LegacyValue::None);
                }
            });
        });
        let before = h.context.state.config.read().settings().native_snapshot();
        let (status, answer) =
            json_of(&h, Method::POST, "/api/v1/compensation/actions/pitch_calibrate", Some(json!({}))).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            answer["detail"],
            if missing {
                "KeyError: 'MOTION_COMPENSATION'"
            } else {
                "TypeError: 'NoneType' object is not subscriptable"
            }
        );
        assert!(h.context.state.config.read().settings().native_snapshot() == before);
    }
    let h = harness();
    h.context.accessories.motion_sensor_values().set("raw_yaw", LegacyValue::Float(12.5));
    h.context.state.config.write().update(|configuration| {
        configuration.settings.edit_native(|raw| {
            raw.get_mut("MOTION_COMPENSATION").unwrap().as_dict_mut().unwrap().insert("roll", LegacyValue::None);
        });
    });
    let before_pitch = h
        .context
        .state
        .config
        .read()
        .settings()
        .native_snapshot()
        .get("MOTION_COMPENSATION")
        .unwrap()
        .item("pitch")
        .unwrap()
        .clone();
    let (status, answer) =
        json_of(&h, Method::POST, "/api/v1/compensation/actions/all_calibrate", Some(json!({}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
    let raw = h.context.state.config.read().settings().native_snapshot();
    let mc = raw.get("MOTION_COMPENSATION").unwrap();
    assert_eq!(mc.item("yaw").unwrap().item("offset").unwrap(), &LegacyValue::Float(-12.5));
    assert_eq!(mc.item("roll").unwrap(), &LegacyValue::None);
    assert_eq!(mc.item("pitch").unwrap(), &before_pitch);
}

#[tokio::test]
async fn mc_calibration_keeps_initial_integer_and_reported_float_zero() {
    use openrigcue_model::native_value::LegacyValue;
    let h = harness();
    for (reading, float) in [(LegacyValue::Int(0), false), (LegacyValue::Float(0.0), true)] {
        h.context.accessories.motion_sensor_values().set("raw_pitch", reading);
        let (status, _) = call(&h, Method::POST, "/api/v1/compensation/actions/pitch_calibrate", Some(json!({}))).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let raw = h.context.state.config.read().settings().native_snapshot();
        let offset = raw.get("MOTION_COMPENSATION").unwrap().item("pitch").unwrap().item("offset").unwrap();
        if float {
            assert!(matches!(offset, LegacyValue::Float(value) if value.to_bits() == (-0.0_f64).to_bits()));
        } else {
            assert_eq!(offset, &LegacyValue::Int(0));
        }
    }
}

#[tokio::test]
async fn mc_calibration_preserves_native_nan_offsets() {
    use openrigcue_model::native_value::LegacyValue;
    let h = harness();
    h.context.accessories.motion_sensor_values().set("raw_yaw", LegacyValue::Float(f64::NAN));
    let (status, _) = call(&h, Method::POST, "/api/v1/compensation/actions/yaw_calibrate", Some(json!({}))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(matches!(h.context.state.config.read().settings().native_snapshot().get("MOTION_COMPENSATION")
        .unwrap().item("yaw").unwrap().item("offset").unwrap(), LegacyValue::Float(value) if value.is_nan()));
}

/// The shaker test buttons do nothing while `SHAKEBOX.soundcard` is `'None'`.
#[tokio::test]
async fn shaker_tests_need_a_sound_card() {
    let h = harness();
    let (status, _) = call(&h, Method::POST, "/api/v1/shakers/test/engine", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(&h, Method::POST, "/api/v1/shakers/test/nonsense", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// The game catalog lists every game.
#[tokio::test]
async fn catalogs() {
    let h = harness();
    let (_, games) = json_of(&h, Method::GET, "/api/v1/catalog/games", None).await;
    assert_eq!(games.as_array().unwrap().len(), 125);
}

/// The IntelliTune prompts (played to the end with `?wait=true`, in the background otherwise); an unknown prompt
/// is 404. A notification text is queued for speaking only while the Voice Notifications setting, read at each
/// request, is on. The harness's voice is off: nothing is sent, played or taken from the queue.
#[tokio::test]
async fn voice_routes() {
    let h = harness();
    let speak = Some(json!({"message": "Calibration has finished"}));
    let (status, _) = call(&h, Method::POST, "/api/v1/voice", speak.clone()).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(h.context.voice.queued().is_empty(), "Voice Notifications is off");
    let (status, _) =
        call(&h, Method::PATCH, "/api/v1/settings", Some(json!({"app": {"voice_notifications": true}}))).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(&h, Method::POST, "/api/v1/voice", speak).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // The UI's in-process call reaches the same queue.
    let direct = crate::api::direct::call(
        h.context.clone(),
        openrigcue_ui::backend::Method::Post,
        "voice",
        axum::body::Bytes::from_static(br#"{"message":"Profile Saved"}"#),
    )
    .await;
    assert_eq!(direct.status(), StatusCode::NO_CONTENT);
    assert_eq!(h.context.voice.queued(), ["Calibration has finished", "Profile Saved"]);
    let (status, _) =
        call(&h, Method::PATCH, "/api/v1/settings", Some(json!({"app": {"voice_notifications": false}}))).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(&h, Method::POST, "/api/v1/voice", Some(json!({"message": "Profile Loaded"}))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(h.context.voice.queued().len(), 2);
    let (status, _) = call(&h, Method::POST, "/api/v1/voice", Some(json!({}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = call(&h, Method::POST, "/api/v1/voice/clips/intellitune_starting?wait=true", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(&h, Method::POST, "/api/v1/voice/clips/10", None).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (status, _) = call(&h, Method::POST, "/api/v1/voice/clips/11", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Joystick page: the bar pass's view, the add confirmation with an existing name (its alert text), then the
/// assignment dialog (opening it on a row, the mode change, the invert switch, the close, the confirmation
/// without a row), the proposed copy name and the remove confirmation on `Default`.
#[tokio::test]
async fn joystick_page_routes() {
    let h = harness();
    let (status, db) = json_of(&h, Method::GET, "/api/v1/joystick/db", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(db, json!({"Default": {}}));
    let (status, _) = call(&h, Method::PUT, "/api/v1/joystick/db", Some(json!({"key": "Wheel", "value": {}}))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(&h, Method::PUT, "/api/v1/joystick/profile", Some(json!({"name": "Wheel"}))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(h.context.joystick.selected_profile(), "Wheel");
    let (_, selected) = json_of(&h, Method::GET, "/api/v1/joystick/profile", None).await;
    assert_eq!(selected, json!({"name": "Wheel"}));
    let (status, _) = call(&h, Method::DELETE, "/api/v1/joystick/db", Some(json!({"key": "Default"}))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, db) = json_of(&h, Method::GET, "/api/v1/joystick/db", None).await;
    assert_eq!(db, json!({"Wheel": {}}));
    let (_, devices) = json_of(&h, Method::GET, "/api/v1/joystick/devices", None).await;
    assert!(devices["state"].is_object());
    assert!(devices["joysticks"].is_object());
    let (status, _) = call(&h, Method::DELETE, "/api/v1/joystick/devices/last-state", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

/// `status.shakers` (what the web UI reads for the FMOD credit): the built-in sounds and engine by default.
#[tokio::test]
async fn status_reports_built_in_shaker_sounds() {
    let h = harness();
    let (status, body) = json_of(&h, Method::GET, "/api/v1/status", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["shakers"]["is_fmod"], false);
    assert_eq!(body["shakers"]["sample_source"], "built_in");
    assert_eq!(body["shakers"]["imported_samples"], 0);
    assert_eq!(body["shakers"]["srs_sounds_available"], false);
}

/// The Hardware page switch "Use SRS Sound Samples" through `PATCH /api/v1/settings`: stored, and with an SRS
/// installation (temporary, one sample) the next shaker pass plays the imported sample; switched off, the built-in
/// sounds again.
#[tokio::test]
async fn use_srs_sounds_setting_reaches_the_shakers() {
    let mut h = harness();
    let install = crate::shakers::tests::srs_install_with_one_sample();
    let state = h.context.state.clone();
    h.context.shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), false)
        .with_srs_sounds(&state, Some(install.path().to_path_buf()));
    let pass = |h: &Harness| {
        let accessories = Accessories::new(std::time::Instant::now());
        crate::shakers::shaker_pass(&h.context.state, &h.context.shakers, &accessories);
        h.context.shakers.wait_for_sounds();
    };
    let (_, body) = json_of(&h, Method::GET, "/api/v1/status", None).await;
    assert_eq!(body["shakers"]["sample_source"], "built_in");
    assert_eq!(body["shakers"]["srs_sounds_available"], true);

    let (status, body) =
        json_of(&h, Method::PATCH, "/api/v1/settings", Some(json!({"shakers": {"use_srs_sounds": true}}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["shakers"]["use_srs_sounds"], "yes");
    assert!(h.context.state.config.read().settings().shakers.use_srs_sounds);
    pass(&h);
    let (_, body) = json_of(&h, Method::GET, "/api/v1/status", None).await;
    assert_eq!(body["shakers"]["sample_source"], "srs_import");
    assert_eq!(body["shakers"]["imported_samples"], 1);
    assert_eq!(body["shakers"]["is_fmod"], false);

    call(&h, Method::PATCH, "/api/v1/settings", Some(json!({"shakers": {"use_srs_sounds": false}}))).await;
    pass(&h);
    let (_, body) = json_of(&h, Method::GET, "/api/v1/status", None).await;
    assert_eq!(body["shakers"]["sample_source"], "built_in");
}

/// The Hardware page's "Import" button through `POST /api/v1/srs-import`: the files of an SRS installation
/// (temporary, one sample and one firmware image) are copied into the configuration folder and the shakers play
/// the imported sample without a new start; a path without SRS files changes nothing.
#[tokio::test]
async fn srs_import_copies_the_files_and_reaches_the_running_shakers() {
    let h = harness();
    let (status, body) = json_of(&h, Method::POST, "/api/v1/srs-import", Some(json!({"path": "/anywhere"}))).await;
    assert_eq!(status, StatusCode::CONFLICT, "a configuration held in memory has no folder: {body}");

    let directory = openrigcue_config::settings::paths::ConfigDir::at(h._tmp.path().join("configuration"));
    let store =
        ConfigStore::open(directory.clone(), openrigcue_config::import::migrate::Platform::Windows, None).unwrap();
    store.persist_first_run().unwrap();
    *h.context.state.config.write() = store;
    let imported = directory.root().join(crate::SRS_IMPORT_DIR);

    let (status, body) = json_of(&h, Method::POST, "/api/v1/srs-import", Some(json!({"path": "  "}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["detail"], "no SRS installer or installation folder given");
    let missing = h._tmp.path().join("no-such-installer.exe");
    let (status, body) = json_of(&h, Method::POST, "/api/v1/srs-import", Some(json!({"path": missing}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body["detail"].as_str().unwrap().starts_with(missing.to_str().unwrap()), "{body}");
    let empty = h._tmp.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let (status, body) = json_of(&h, Method::POST, "/api/v1/srs-import", Some(json!({"path": empty}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body["detail"].as_str().unwrap().contains("no SRS sound or firmware files found"), "{body}");
    assert_eq!(crate::imported_srs_install(directory.root()), None);
    let (_, body) = json_of(&h, Method::GET, "/api/v1/status", None).await;
    assert_eq!(body["shakers"]["srs_sounds_available"], false);

    let install = crate::shakers::tests::srs_install_with_one_sample();
    let firmware = install.path().join("firmware").join("intellibox");
    std::fs::create_dir_all(&firmware).unwrap();
    std::fs::write(firmware.join("intellibox.hex"), b"hex").unwrap();
    h.context.state.config.write().update(|config| config.settings.shakers.use_srs_sounds = true);
    // The way the UI reaches the service: an in-process call, with spaces around the typed path.
    let request = json!({"path": format!(" {} ", install.path().display())}).to_string();
    let response =
        super::direct::call(h.context.clone(), openrigcue_ui::backend::Method::Post, "srs-import", request.into())
            .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    drop(install);
    assert_eq!(crate::imported_srs_install(directory.root()), Some(imported.clone()));
    assert!(imported.join("sounds/transmission/1.ogg").is_file());
    assert_eq!(std::fs::read(imported.join("firmware/intellibox/intellibox.hex")).unwrap(), b"hex");
    h.context.shakers.wait_for_sounds();
    let (_, body) = json_of(&h, Method::GET, "/api/v1/status", None).await;
    assert_eq!(body["shakers"]["srs_sounds_available"], true);
    assert_eq!(body["shakers"]["sample_source"], "srs_import");
    assert_eq!(body["shakers"]["imported_samples"], 1);
}

#[tokio::test]
async fn live_table_reads_shared_values_and_canonical_units_without_export_cache() {
    use openrigcue_config::import::value::LegacyValue;
    use openrigcue_sources::derive::SpeedUnit;
    let h = harness();
    h.context.state.telemetry.write().set_native("pitch", LegacyValue::Bool(true));
    h.context.state.telemetry.write().set_native("speed", LegacyValue::Int(20));
    assert!(h.context.telemetry.lock().export_rows().is_empty());
    let (_, rows) = json_of(&h, Method::GET, "/api/v1/telemetry/table", None).await;
    let row =
        |rows: &Value, label: &str| rows.as_array().unwrap().iter().find(|r| r["label"] == label).unwrap().clone();
    assert_eq!(row(&rows, "Pitch")["value"], "True");
    assert_eq!(row(&rows, "Pitch")["direction_positive"], true);
    assert_eq!(row(&rows, "Speed")["value"], "72.000000");
    let mut native = h.context.state.config.read().settings().native_snapshot();
    native.get_mut("APP").unwrap().as_dict_mut().unwrap().insert("measurement_unit", LegacyValue::None);
    h.context
        .state
        .config
        .write()
        .update(|config| config.settings = openrigcue_config::settings::Settings::from_legacy(&native).0);
    let (_, rows) = json_of(&h, Method::GET, "/api/v1/telemetry/table", None).await;
    assert_eq!(row(&rows, "Speed")["value"], "44.738800");
    let settings = crate::telemetry::dispatch_settings(h.context.state.config.read().settings());
    assert_eq!(settings.native_speed_unit, Some(Ok(SpeedUnit::Mph)));
    native.get_mut("APP").unwrap().as_dict_mut().unwrap().remove("measurement_unit");
    h.context
        .state
        .config
        .write()
        .update(|config| config.settings = openrigcue_config::settings::Settings::from_legacy(&native).0);
    let (_, rows) = json_of(&h, Method::GET, "/api/v1/telemetry/table", None).await;
    assert_eq!(row(&rows, "Speed")["error"], "'measurement_unit'");
    h.context.state.telemetry.write().set_native("pitch", LegacyValue::Str("1".into()));
    let (_, rows) = json_of(&h, Method::GET, "/api/v1/telemetry/table", None).await;
    assert_eq!(row(&rows, "Pitch")["error"], "'>' not supported between instances of 'str' and 'int'");
    assert!(h.context.telemetry.lock().export_rows().is_empty());
}

#[tokio::test]
async fn intellitune_service_retains_native_presence_and_integer_samples() {
    use openrigcue_model::native_value::LegacyValue;
    use openrigcue_sources::derive::{Listener, Rate, RuntimeInputs, ScanHit, TickInput};
    struct NativeZeros;
    impl Listener for NativeZeros {
        fn id(&self) -> &str {
            "api"
        }
        fn is_running(&mut self) -> bool {
            true
        }
        fn receive(&mut self) {}
        fn channel(&self, _: &str) -> Option<f64> {
            Some(0.0)
        }
        fn native_channel(&self, _: &str) -> Option<LegacyValue> {
            Some(LegacyValue::Int(0))
        }
        fn rate(&self, _: Rate) -> Option<f64> {
            None
        }
        fn vehicle(&self) -> Option<String> {
            None
        }
        fn location(&self) -> Option<String> {
            None
        }
    }
    let mut h = harness();
    let (_, samples) = json_of(&h, Method::GET, "/api/v1/intellitune", None).await;
    assert_eq!(samples, json!({}));
    let settings = crate::telemetry::dispatch_settings(h.context.state.config.read().settings());
    let profile =
        crate::telemetry::profile_settings(openrigcue_config::tunes::FactoryTunes::builtin().get("emulator").unwrap());
    let mut dispatcher = Dispatcher::new(vec![Box::new(NativeZeros)]);
    dispatcher.tick(&TickInput {
        hits: &[ScanHit { game_id: "dirtrally2.0".into(), listener_id: "api".into() }],
        settings: &settings,
        profile: &profile,
        runtime: &RuntimeInputs { intellitune_capture: true, ..Default::default() },
        games: &crate::telemetry::CatalogDirectory::default(),
    });
    assert!(!dispatcher.has_ended());
    h.context.telemetry = TelemetryHandle::new(dispatcher);
    let (_, samples) = json_of(&h, Method::GET, "/api/v1/intellitune", None).await;
    assert_eq!(samples.as_object().unwrap().len(), 7);
    for values in samples.as_object().unwrap().values() {
        assert_eq!(values.as_array().unwrap()[0].as_i64(), Some(0));
    }
}

#[tokio::test]
async fn tune_reads_use_live_rows_without_decoding_other_profiles() {
    use openrigcue_config::import::{sqlitedict, value::LegacyValue};
    use openrigcue_config::settings::paths::ConfigDir;
    let h = harness();
    let dir = ConfigDir::at(h._tmp.path().join("native-profile-read"));
    Configuration::first_run().save(&dir).unwrap();
    *h.context.state.config.write() = ConfigStore::load(dir.clone()).unwrap();
    let mut row = h.context.state.config.read().profile_row("emulator").unwrap().unwrap();
    let mut car = row.clone();
    car.as_dict_mut().unwrap().insert("gear_shift_perc", LegacyValue::Int(43));
    row.as_dict_mut().unwrap().insert("gear_shift_perc", LegacyValue::Int(99));
    row.as_dict_mut().unwrap().get_mut("vehicles").unwrap().as_dict_mut().unwrap().insert("fixturecar", car);
    sqlitedict::set_item(&dir.profiles_file(), "unnamed", "emulator", &row).unwrap();
    let database = rusqlite::Connection::open(dir.profiles_file()).unwrap();
    database.execute("INSERT INTO unnamed (key,value) VALUES ('opaque',?)", [vec![0xff_u8]]).unwrap();
    drop(database);
    let (status, tune) = json_of(&h, Method::GET, "/api/v1/tunes/emulator", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(tune["gear_shift_perc"], 99);
    assert_eq!(tune["vehicles"], json!({}));
    let (status, tune) = json_of(&h, Method::GET, "/api/v1/tunes/emulator/vehicles/fixturecar", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(tune["gear_shift_perc"], 43);
    assert_eq!(call(&h, Method::GET, "/api/v1/tunes/opaque", None).await.0, StatusCode::INTERNAL_SERVER_ERROR);
    let mut pending = row;
    pending.as_dict_mut().unwrap().insert("gear_shift_perc", LegacyValue::Int(73));
    h.context.state.config.write().stage_game_row("emulator", Some(pending)).unwrap();
    assert_eq!(json_of(&h, Method::GET, "/api/v1/tunes/emulator", None).await.1["gear_shift_perc"], 73);
}

#[tokio::test]
async fn profile_edits_and_manager_use_current_rows_and_preserve_other_vehicles() {
    use openrigcue_config::import::{sqlitedict, value::LegacyValue};
    use openrigcue_config::settings::paths::ConfigDir;
    let h = harness();
    let dir = ConfigDir::at(h._tmp.path().join("native-profile-edit"));
    Configuration::first_run().save(&dir).unwrap();
    *h.context.state.config.write() = ConfigStore::load(dir.clone()).unwrap();
    let mut row = h.context.state.config.read().profile_row("emulator").unwrap().unwrap();
    let mut car = row.clone();
    car.as_dict_mut().unwrap().insert("gear_shift_perc", LegacyValue::Int(43));
    row.as_dict_mut().unwrap().insert("gear_shift_perc", LegacyValue::Int(99));
    row.as_dict_mut().unwrap().get_mut("vehicles").unwrap().as_dict_mut().unwrap().insert("fixturecar", car);
    sqlitedict::set_item(&dir.profiles_file(), "unnamed", "emulator", &row).unwrap();
    let database = rusqlite::Connection::open(dir.profiles_file()).unwrap();
    database.execute("INSERT INTO unnamed (key,value) VALUES ('opaque',?)", [vec![0xff_u8]]).unwrap();
    drop(database);
    {
        let store = h.context.state.config.read();
        let before: Vec<_> = store.config().profiles.native_table().keys().cloned().collect();
        let refreshed = store.config_with_profile_row("emulator").unwrap();
        let after: Vec<_> = refreshed.profiles.native_table().keys().cloned().collect();
        assert_eq!(before, after, "reading one row must not give it REPLACE ordering");
        assert_eq!(refreshed.profiles.native_table().get("emulator"), Some(&row));
    }
    let (status, manager) = json_of(&h, Method::GET, "/api/v1/profiles", None).await;
    assert_eq!(status, StatusCode::OK);
    let entry = manager.as_array().unwrap().iter().find(|v| v["game"] == "emulator").unwrap();
    assert_eq!(entry["vehicles"], json!(["fixturecar"]));
    let (_, mut tune) = json_of(&h, Method::GET, "/api/v1/tunes/emulator", None).await;
    tune["gear_shift_perc"] = json!(73);
    assert_eq!(call(&h, Method::PUT, "/api/v1/tunes/emulator", Some(tune)).await.0, StatusCode::NO_CONTENT);
    let saved = sqlitedict::read_item(&dir.profiles_file(), "unnamed", "emulator").unwrap().unwrap();
    assert_eq!(saved.as_dict().unwrap().get("gear_shift_perc"), Some(&LegacyValue::Int(73)));
    assert_eq!(call(&h, Method::GET, "/api/v1/tunes/emulator/vehicles/fixturecar", None).await.0, StatusCode::OK);
    assert_eq!(saved.as_dict().unwrap().get("vehicles"), row.as_dict().unwrap().get("vehicles"));
    assert_eq!(
        call(&h, Method::PUT, "/api/v1/tunes/emulator/vehicles/newcar", Some(json!({"marker": 17}))).await.0,
        StatusCode::NO_CONTENT
    );
    let (_, car) = json_of(&h, Method::GET, "/api/v1/tunes/emulator/vehicles/fixturecar", None).await;
    assert_eq!(car["gear_shift_perc"], 43);
}

#[tokio::test]
async fn bulk_edits_keep_the_pending_prefix_when_a_later_row_cannot_decode() {
    use openrigcue_config::import::{pickle, sqlitedict, value::LegacyValue};
    use openrigcue_config::settings::paths::ConfigDir;
    for bad_first in [true, false] {
        let h = harness();
        let dir = ConfigDir::at(h._tmp.path().join("native-profile-iteration"));
        Configuration::first_run().save(&dir).unwrap();
        *h.context.state.config.write() =
            ConfigStore::open(dir.clone(), openrigcue_config::import::migrate::Platform::Linux, None).unwrap();
        let row = h.context.state.config.read().profile_row("emulator").unwrap().unwrap();
        let original = row.as_dict().unwrap().get("motion_max_power").unwrap().clone();
        let bytes = pickle::encode(&row);
        let database = rusqlite::Connection::open(dir.profiles_file()).unwrap();
        database.execute("DELETE FROM unnamed", []).unwrap();
        let entries = if bad_first {
            vec![("bad", vec![0xff]), ("emulator", bytes.clone()), ("ac", bytes)]
        } else {
            vec![("emulator", bytes.clone()), ("bad", vec![0xff]), ("ac", bytes)]
        };
        for (key, bytes) in entries {
            database.execute("INSERT INTO unnamed (key,value) VALUES (?,?)", rusqlite::params![key, bytes]).unwrap();
        }
        drop(database);
        let (status, _) =
            call(&h, Method::POST, "/api/v1/tunes/apply-all", Some(json!({"field":"max_angle","value":37}))).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        let store = h.context.state.config.read();
        let current = store.profile_row("emulator").unwrap().unwrap();
        assert_eq!(
            current.as_dict().unwrap().get("motion_max_power"),
            Some(&if bad_first { original.clone() } else { LegacyValue::Int(37) })
        );
        let later = store.profile_row("ac").unwrap().unwrap();
        assert_eq!(later.as_dict().unwrap().get("motion_max_power"), Some(&original));
        let disk = sqlitedict::read_item(&dir.profiles_file(), "unnamed", "emulator").unwrap().unwrap();
        assert_eq!(disk.as_dict().unwrap().get("motion_max_power"), Some(&original));
        drop(store);
        h.context.state.config.write().close_profile_database();
        assert!(h.context.state.config.read().profile_row("emulator").is_err());
        let after = sqlitedict::read_item(&dir.profiles_file(), "unnamed", "emulator").unwrap().unwrap();
        assert_eq!(after.as_dict().unwrap().get("motion_max_power"), Some(&original));
    }
}

#[tokio::test]
async fn file_imports_and_exports_read_current_rows_and_preserve_live_vehicles() {
    use openrigcue_config::catalog::Catalog;
    use openrigcue_config::import::{sqlitedict, value::LegacyValue};
    use openrigcue_config::profiles::{export_profile, read_profile_file, ExportMetadata, SaveTarget, Vehicle};
    use openrigcue_config::settings::paths::ConfigDir;
    let h = harness();
    let dir = ConfigDir::at(h._tmp.path().join("native-profile-files"));
    Configuration::first_run().save(&dir).unwrap();
    *h.context.state.config.write() = ConfigStore::load(dir.clone()).unwrap();
    let mut row = h.context.state.config.read().profile_row("emulator").unwrap().unwrap();
    let mut car = row.clone();
    car.as_dict_mut().unwrap().insert("gear_shift_perc", LegacyValue::Int(43));
    row.as_dict_mut().unwrap().insert("gear_shift_perc", LegacyValue::Int(99));
    row.as_dict_mut().unwrap().get_mut("vehicles").unwrap().as_dict_mut().unwrap().insert("fixturecar", car);
    sqlitedict::set_item(&dir.profiles_file(), "unnamed", "emulator", &row).unwrap();
    let database = rusqlite::Connection::open(dir.profiles_file()).unwrap();
    database.execute("INSERT INTO unnamed (key,value) VALUES ('opaque',?)", [vec![0xff_u8]]).unwrap();
    drop(database);
    let (status, exported) =
        call(&h, Method::GET, "/api/v1/profiles/export?game=emulator&vehicle=fixturecar", None).await;
    assert_eq!(status, StatusCode::OK);
    let file = read_profile_file(&exported).unwrap();
    let mut imported = Configuration::first_run().profiles;
    file.import_into(&mut imported).unwrap();
    let (car, _) = imported.resolve_native("emulator", &Vehicle::Named("fixturecar".into())).unwrap();
    assert_eq!(car.as_dict().unwrap().get("gear_shift_perc"), Some(&LegacyValue::Int(43)));
    assert_eq!(call(&h, Method::GET, "/api/v1/profiles/export-all", None).await.0, StatusCode::INTERNAL_SERVER_ERROR);
    let mut source = Configuration::first_run().profiles;
    let mut value = source.resolve_native("emulator", &Vehicle::Unknown).unwrap().0;
    value.as_dict_mut().unwrap().insert("gear_shift_perc", LegacyValue::Int(73));
    source.save_native("emulator", &SaveTarget::Game, value).unwrap();
    let meta = ExportMetadata { motion_model: "Fixture", shaker_layout: "Custom", builtin_layouts: &[] };
    let (_, bytes) = export_profile(&source, Catalog::builtin(), "emulator", &Vehicle::Unknown, None, &meta).unwrap();
    let response = router(h.context.clone())
        .oneshot(
            Request::builder().method(Method::POST).uri("/api/v1/profiles/import").body(Body::from(bytes)).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let current = h.context.state.config.read().profile_row("emulator").unwrap().unwrap();
    assert_eq!(current.as_dict().unwrap().get("gear_shift_perc"), Some(&LegacyValue::Int(73)));
    assert_eq!(current.as_dict().unwrap().get("vehicles"), row.as_dict().unwrap().get("vehicles"));
    let disk = sqlitedict::read_item(&dir.profiles_file(), "unnamed", "emulator").unwrap().unwrap();
    assert_eq!(disk.as_dict().unwrap().get("gear_shift_perc"), Some(&LegacyValue::Int(99)));
}

/// The Games page's config action for a game that needs the Proton bridge: refused as a conflict, with the text
/// the page shows, while Steam runs; done once Steam is closed. A running game keeps its own refusal.
#[tokio::test]
async fn game_config_is_a_conflict_while_steam_runs() {
    use crate::games::bridge_tests::{Fixture, USER};
    let mut h = harness();
    let fixture = Fixture::new();
    fixture.process("4242", "steam");
    h.context.main_loop = MainLoopHandle::new(MainLoop::new(fixture.setup(), h._tmp.path().join("proc")));
    let (status, rows) = json_of(&h, Method::GET, "/api/v1/games?page=PC", None).await;
    assert_eq!(status, StatusCode::OK);
    let row = |rows: &Value, key: &str| {
        rows["rows"].as_array().unwrap().iter().find(|row| row["key"] == key).unwrap()["status"].clone()
    };
    assert_eq!(row(&rows, "ac"), "config");
    let before = fixture.config(USER);
    let (status, problem) = json_of(&h, Method::POST, "/api/v1/games/ac/patch", None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        problem["detail"],
        "Steam is running, so OpenRigCue cannot set the game up. Close Steam, then try again."
    );
    assert_eq!(fixture.config(USER), before, "Steam's file is untouched by the refusal");
    // A running game is refused first, with its own text.
    h.context.state.session.write().game = Some("ac".into());
    let (status, problem) = json_of(&h, Method::POST, "/api/v1/games/ac/patch", None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        problem["detail"],
        "A game is running, so OpenRigCue cannot set the game up. Close the game, then try again."
    );
    h.context.state.session.write().game = None;
    // Steam closed: configured, and the table says so.
    fixture.process("4242", "bash");
    let (status, _) = call(&h, Method::POST, "/api/v1/games/ac/patch", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(fixture.config(USER).contains("openrigcue-bridge-run %command% -x"));
    let (_, rows) = json_of(&h, Method::GET, "/api/v1/games?page=PC", None).await;
    assert_eq!(row(&rows, "ac"), "ready_to_play");
    // Any other failure of the action stays what it was (not a conflict).
    let (status, _) = call(&h, Method::POST, "/api/v1/games/beamng/patch", None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}
