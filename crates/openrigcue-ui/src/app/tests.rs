//! `MyApp` on a fake service: the page tree, events and the main loop.

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use super::*;
use crate::backend::{Failure, HardwareNotification, HardwareNotificationState, Method};
use crate::remi::gui::{dispatch, parse_parameters};

type ProfileReloadProbe = Box<dyn Fn(&str) + Send + Sync>;

/// A service answering from documents kept in memory; records the calls.
#[derive(Default)]
pub struct FakeBackend {
    pub synchronized: std::sync::atomic::AtomicUsize,
    /// The game setup checks asked for.
    pub game_setup_checks: std::sync::atomic::AtomicUsize,
    pub fail_shaker_save: std::sync::atomic::AtomicBool,
    reload_lock_probe: Mutex<Option<std::sync::Weak<crate::host::UpdateLock>>>,
    finish_probe: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    reload_probe: Mutex<Option<ProfileReloadProbe>>,
    hardware_notification_state: Mutex<HardwareNotificationState>,
    pub calls: Mutex<Vec<(Method, String, Option<Value>)>>,
    pub translations: Mutex<Vec<String>>,
    pub docs: Mutex<Value>,
}

impl FakeBackend {
    pub fn new() -> Arc<FakeBackend> {
        let mut docs = json!({
            "status": {
                "session": {"game": null, "listener": null, "vehicle": null},
                "controls": {"switches": {"wind": "auto", "wind_center": "auto", "led": "auto", "motion": true, "shakers": true, "belt": true, "sim3d": true, "hud": true, "iflag": true, "haptic": true, "compensation": true}, "tests": {"belt_homing": 0}, "profile_edit": null},
                "profile": {"game": null, "vehicle_level": false, "generation": 1},
                "motion": {"link": {"status": "Not Connected", "ports": {}}, "warning": ""},
                "devices": {"thanos": {"controller":"", "firmware":"", "config":{}, "config2":{}}},
                "accessories": {"intellibox": "Not Connected"},
                "platform": "linux",
                "host_ip": "127.0.0.1",
                "displayed_vehicle": null
            },
            "settings": {
                "app": {"speed_unit": "kmh", "start_page": "Wind", "log_level": 20, "hide_standby_tuning_alert": false},
                "wind": {"auto_search": true, "serial_port": "", "fan_count": "2"},
                "motion": {"model": "None", "auto_search": true},
                "shakers": {"sound_card": "None"},
                "belt": {"auto_search": true, "serial_port": ""},
                "sim3d": {"auto_search": true, "serial_port": ""},
                "iflag": {"auto_search": true, "serial_port": ""},
                "hud": {"auto_search": true},
                "features": {}
            },
            "tune": {"default_max_speed_kmh": 200, "speed_factor": 3, "wind_curving": 1, "curving_effect": 4,
                     "power_wind_min_speed_perc": 10, "power_wind_max_speed_perc": 90,
                     "power_center_wind_min_speed_perc": 0, "power_center_wind_max_speed_perc": 100,
                     "enable_ejection_seat": 0}
        });
        let mut settings = serde_json::to_value(openrigcue_config::settings::Settings::default()).unwrap();
        merge(&mut settings, &docs["settings"]);
        docs["settings"] = settings;
        let mut tune = openrigcue_config::tunes::FactoryTunes::builtin().get("emulator").unwrap().to_json();
        merge(&mut tune, &docs["tune"]);
        docs["tune"] = tune;
        docs["joystick_db"] = json!({"Default": {}});
        docs["joystick_profile"] = json!("Default");
        docs["notifications"] = json!([]);
        docs["catalog_games"] = json!(openrigcue_config::catalog::Catalog::builtin()
            .games()
            .iter()
            .map(|game| json!({"id": game.id, "name": game.name}))
            .collect::<Vec<_>>());
        docs["hardware"] = json!({"sound_cards": ["None", "USB Sound Device"], "serial_ports": []});
        docs["status"]["shakers"] = json!({"refresh_soundcard": false});
        docs["status"]["devices"]["yaw_vr"] = json!({"centered": false});
        docs["status"]["accessories"]["iflag_firmware"] = json!(0);
        for key in ["compensation_port", "belt_port", "intellibox_port", "iflag_port", "sim3d_port", "hapticr_port"] {
            docs["status"]["accessories"][key] = json!("");
        }
        docs["status"]["controls"]["switches"]["joystick_status"] = json!(0);
        docs["status"]["controls"]["tests"] = json!({
            "motion_test": false, "motion_sliders": {}, "belt_test": false,
            "belt_test_value": 0, "belt_homing": 0, "iflag_test": false,
            "haptic_test": 0, "rumble_test": 0
        });
        docs["status"]["controls"]["telemetry"] = json!({
            "capture": false, "plot_channel": null, "intellitune_capture": false, "export": false,
            "reset_min_max": false
        });
        Arc::new(FakeBackend {
            synchronized: std::sync::atomic::AtomicUsize::new(0),
            game_setup_checks: std::sync::atomic::AtomicUsize::new(0),
            fail_shaker_save: std::sync::atomic::AtomicBool::new(false),
            reload_lock_probe: Mutex::new(None),
            finish_probe: Mutex::new(None),
            reload_probe: Mutex::new(None),
            hardware_notification_state: Mutex::new(HardwareNotificationState::default()),
            calls: Mutex::new(Vec::new()),
            translations: Mutex::new(Vec::new()),
            docs: Mutex::new(docs),
        })
    }

    pub fn patches(&self, path: &str) -> Vec<Value> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, p, _)| *m == Method::Patch && p == path)
            .filter_map(|(_, _, b)| b.clone())
            .collect()
    }
}

impl Backend for FakeBackend {
    fn hardware_parameter(&self, name: &str) -> Result<openrigcue_model::native_value::LegacyValue, Failure> {
        {
            let mut docs = self.docs.lock().unwrap();
            if let Some(sequence) = docs
                .get_mut("hardware_parameter_sequence")
                .and_then(|values| values.get_mut(name))
                .and_then(Value::as_array_mut)
            {
                if !sequence.is_empty() {
                    return Ok(sequence.remove(0).into());
                }
            }
        }
        crate::backend::hardware_parameter_from_backend(self, name)
    }

    fn native_setting(&self, section: &str, key: &str) -> Result<openrigcue_model::native_value::LegacyValue, Failure> {
        {
            let mut docs = self.docs.lock().unwrap();
            if let Some(settings) = docs.get("native_settings") {
                let settings = openrigcue_model::native_value::LegacyValue::from(settings.clone());
                let section = settings
                    .item(section)
                    .map_err(|detail| Failure { status: 500, detail: format!("KeyError: {detail}") })?;
                return section.item(key).cloned().map_err(|detail| Failure {
                    status: 500,
                    detail: format!("{}: {detail}", if section.as_dict().is_some() { "KeyError" } else { "TypeError" }),
                });
            }
            if let Some(sequence) = docs
                .get_mut("native_setting_sequence")
                .and_then(|sections| sections.get_mut(section))
                .and_then(|values| values.get_mut(key))
                .and_then(Value::as_array_mut)
            {
                if !sequence.is_empty() {
                    return Ok(sequence.remove(0).into());
                }
            }
        }
        crate::backend::projected_native_setting(self, section, key)
    }

    fn ui_actions(&self) -> Vec<String> {
        self.docs.lock().unwrap()["ui_actions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
    }
    fn clear_ui_actions(&self) {
        self.docs.lock().unwrap()["ui_actions"] = json!([]);
    }
    fn store_shaker_effects(&self) -> Result<(), Failure> {
        self.calls.lock().unwrap().push((Method::Post, "shakers/store-effects".into(), None));
        if self.fail_shaker_save.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(Failure { status: 500, detail: "RuntimeError: fixture shaker save failed".into() });
        }
        Ok(())
    }
    fn assign_app_setting(&self, key: &str, value: Value) -> Result<Value, Failure> {
        let mut docs = self.docs.lock().unwrap();
        let app = docs["settings"]
            .as_object_mut()
            .ok_or_else(|| Failure { status: 500, detail: "TypeError: settings are not subscriptable".into() })?
            .get_mut("app")
            .ok_or_else(|| Failure { status: 500, detail: "KeyError: 'APP'".into() })?;
        let mut native = openrigcue_config::import::value::LegacyValue::from(app.clone());
        native
            .assign_item(key, openrigcue_config::import::value::LegacyValue::from(value.clone()))
            .map_err(|error| Failure { status: 500, detail: format!("TypeError: {error}") })?;
        let field = crate::backend::app_field(key);
        let value = if key == "measurement_unit" {
            match value.as_str() {
                Some("K") => Value::from("kmh"),
                Some("M") => Value::from("mph"),
                _ => value,
            }
        } else {
            value
        };
        app.as_object_mut().unwrap().insert(field.into(), value.clone());
        self.calls.lock().unwrap().push((
            Method::Patch,
            "settings".into(),
            Some(crate::backend::patch_for(&["app", field], value)),
        ));
        Ok(docs["settings"].clone())
    }

    fn close_opened_files(&self) {
        self.calls.lock().unwrap().push((Method::Post, "desktop/close-files".into(), None));
    }

    fn quit_after_pagehide(&self) -> Result<(), Failure> {
        self.calls.lock().unwrap().push((Method::Post, "desktop/kill".into(), None));
        Ok(())
    }
    fn capture_window_geometry(&self) {
        self.calls.lock().unwrap().push((Method::Post, "desktop/capture-geometry".into(), None));
    }
    fn synchronize_profile(&self) -> Result<(), Failure> {
        if let Some(probe) = self.reload_probe.lock().unwrap().as_ref() {
            probe("synchronize");
        }
        if let Some(error) = self.docs.lock().unwrap()["profile_sync_failure"].as_str() {
            return Err(Failure { status: 500, detail: error.into() });
        }
        self.synchronized.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    fn profile_reload_needed(&self) -> Result<Option<bool>, Failure> {
        if let Some(probe) = self.reload_probe.lock().unwrap().as_ref() {
            probe("decision");
        }
        if let Some(error) = self.docs.lock().unwrap()["profile_decision_failure"].as_str() {
            return Err(Failure { status: 500, detail: error.into() });
        }
        Ok(self.docs.lock().unwrap()["native_profile_reload"].as_bool())
    }
    fn load_current_profile(&self) -> Result<(), Failure> {
        if let Some(probe) = self.reload_probe.lock().unwrap().as_ref() {
            probe("load");
        }
        if let Some(error) = self.docs.lock().unwrap()["profile_load_failure"].as_str() {
            return Err(Failure { status: 500, detail: error.into() });
        }
        if let Some(gate) = self.reload_lock_probe.lock().unwrap().as_ref().and_then(std::sync::Weak::upgrade) {
            if !gate.owned_by_current_thread() {
                return Err(Failure { status: 500, detail: "RuntimeError: reload without update lock".into() });
            }
        }
        self.synchronize_profile()
    }
    fn hardware_notification(&self, notification: HardwareNotification) -> Result<(), Failure> {
        self.calls.lock().unwrap().push((Method::Post, format!("hardware-notification/{notification:?}"), None));
        if let Some(error) = self.docs.lock().unwrap()["hardware_notification_failure"].as_str() {
            return Err(Failure { status: 500, detail: error.into() });
        }
        Ok(())
    }
    fn hardware_notification_state(&self) -> HardwareNotificationState {
        *self.hardware_notification_state.lock().unwrap()
    }
    fn finish_main_loop(&self) -> Result<(), Failure> {
        if let Some(probe) = self.finish_probe.lock().unwrap().as_ref() {
            probe();
        }
        if let Some(gate) = self.reload_lock_probe.lock().unwrap().as_ref().and_then(std::sync::Weak::upgrade) {
            if gate.owned_by_current_thread() {
                return Err(Failure {
                    status: 500,
                    detail: "RuntimeError: notification tail retained update lock".into(),
                });
            }
        }
        if let Some(error) = self.docs.lock().unwrap()["finish_main_loop_failure"].as_str() {
            return Err(Failure { status: 500, detail: error.into() });
        }
        Ok(())
    }
    fn check_game_config(&self) -> Result<(), Failure> {
        self.game_setup_checks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Some(error) = self.docs.lock().unwrap()["game_setup_check_failure"].as_str() {
            return Err(Failure { status: 500, detail: error.into() });
        }
        Ok(())
    }
    fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value, Failure> {
        self.calls.lock().unwrap().push((method, path.to_owned(), body.clone()));
        let mut docs = self.docs.lock().unwrap();
        let profile_failure = match (method, path) {
            (Method::Post, "profile/save") => "profile_save_failure",
            (Method::Delete, "profile/edit") => "profile_clear_failure",
            _ => "",
        };
        if let Some(error) = docs.get(profile_failure).and_then(Value::as_str) {
            return Err(Failure { status: 500, detail: error.into() });
        }
        if method == Method::Patch && path == "controls" {
            if let Some(error) = docs["control_failure"].as_str() {
                return Err(Failure { status: 500, detail: error.into() });
            }
        }
        if method == Method::Patch && path == "settings" {
            if let Some(error) = docs["settings_failure"].as_str() {
                return Err(Failure { status: 500, detail: error.into() });
            }
        }
        if method == Method::Post && path == "mobile-companion" {
            if let Some(error) = docs["mobile_companion_failure"].as_str() {
                return Err(Failure { status: 500, detail: error.into() });
            }
        }
        if method == Method::Get && path == "compensation" {
            if let Some(sequence) = docs.get_mut("compensation_sequence").and_then(Value::as_array_mut) {
                if sequence.len() > 1 {
                    return Ok(sequence.remove(0));
                }
                if let Some(value) = sequence.first() {
                    return Ok(value.clone());
                }
            }
            return Ok(Value::Null);
        }
        if method == Method::Post && path.starts_with("compensation/actions/") {
            if docs["mc_calibration_failure"]
                .as_str()
                .is_some_and(|effect| path.ends_with(&format!("{effect}_calibrate")))
            {
                return Err(Failure { status: 500, detail: "TypeError: fixture calibration failure".into() });
            }
            if let Some(Value::Array(additions)) = docs.get_mut("enqueue_ui_actions_after_calibration").map(Value::take)
            {
                docs["ui_actions"].as_array_mut().unwrap().extend(additions);
            }
            return Ok(Value::Null);
        }
        if path == "hardware" {
            if let Some(model) = docs.as_object_mut().unwrap().shift_remove("motion_model_after_enumeration") {
                docs["settings"]["motion"]["model"] = model;
            }
            return match docs["hardware_failure"].as_str() {
                Some(error) => Err(Failure { status: 500, detail: error.into() }),
                None => Ok(docs["hardware"].clone()),
            };
        }
        if path == "shakers/refresh-soundcard" && method == Method::Delete {
            docs["status"]["shakers"]["refresh_soundcard"] = json!(false);
            return Ok(Value::Null);
        }
        if path == "settings/hide-standby-alert" && method == Method::Post {
            let failure = docs["standby_write_failure"].as_str().map(str::to_owned);
            let section = docs["settings"]
                .get_mut("app")
                .ok_or_else(|| Failure { status: 500, detail: "KeyError: 'APP'".into() })?;
            let fields = section.as_object_mut().ok_or_else(|| Failure {
                status: 500,
                detail: "TypeError: APP does not support item assignment".into(),
            })?;
            fields.insert("hide_standby_tuning_alert".into(), json!("yes"));
            return failure.map_or(Ok(Value::Null), |detail| Err(Failure { status: 500, detail }));
        }
        if let Some(answer) = super::shaker_test_backend::call(&mut docs, method, path, body.as_ref()) {
            return answer;
        }
        match path {
            "start-info" => return Ok(docs["start_info"].clone()),
            "catalog/games" => return Ok(docs["catalog_games"].clone()),
            "joystick/db" => {
                match method {
                    Method::Put => {
                        let value = body.as_ref().unwrap();
                        let key = value["key"].as_str().unwrap();
                        docs["joystick_db"][key] = value["value"].clone();
                    }
                    Method::Delete => {
                        let key = body.as_ref().unwrap()["key"].as_str().unwrap();
                        docs["joystick_db"].as_object_mut().unwrap().remove(key);
                    }
                    _ => {}
                }
                return Ok(docs["joystick_db"].clone());
            }
            "joystick/profile" => {
                if method == Method::Put {
                    docs["joystick_profile"] = body.as_ref().unwrap()["name"].clone();
                }
                return Ok(json!({"name": docs["joystick_profile"]}));
            }
            "joystick/devices" => return Ok(json!({"joysticks": {}, "state": {}, "last_state": {}})),
            "telemetry/history" => {
                if method == Method::Delete {
                    docs["telemetry_history"] = json!([]);
                    return Ok(Value::Null);
                }
                if let Some(sequence) = docs.get_mut("telemetry_history_sequence").and_then(Value::as_array_mut) {
                    if sequence.is_empty() {
                        return Err(Failure { status: 500, detail: "fixture history sequence exhausted".into() });
                    }
                    return Ok(sequence.remove(0));
                }
                return Ok(docs["telemetry_history"].clone());
            }
            "telemetry/table" => return Ok(docs["telemetry_table"].clone()),
            "compensation/setup" => return Ok(docs["compensation/setup"].clone()),
            _ => {}
        }
        let key = match path {
            "status" => "status",
            "settings" => "settings",
            "profile" => "tune",
            "controls" => "controls",
            "notifications" => "notifications",
            _ => return Ok(Value::Null),
        };
        if method == Method::Patch {
            let target = if key == "controls" { &mut docs["status"]["controls"] } else { &mut docs[key] };
            if key == "tune" {
                merge_assignment(target, body.as_ref().unwrap_or(&Value::Null));
                if let Some(Value::Array(additions)) =
                    docs.get_mut("enqueue_ui_actions_after_profile_write").map(Value::take)
                {
                    let queue = docs
                        .as_object_mut()
                        .unwrap()
                        .entry("ui_actions")
                        .or_insert_with(|| json!([]))
                        .as_array_mut()
                        .unwrap();
                    queue.extend(additions);
                }
                if let Some(Value::String(game)) = docs.get_mut("game_after_profile_write").map(Value::take) {
                    docs["status"]["session"]["game"] = Value::String(game);
                }
            } else {
                merge(target, body.as_ref().unwrap_or(&Value::Null));
                if key == "settings" && body.as_ref().is_some_and(|body| body["motion"].get("model").is_some()) {
                    if let Some(model) = docs.as_object_mut().unwrap().shift_remove("motion_model_after_write") {
                        docs["settings"]["motion"]["model"] = model;
                    }
                }
            }
        }
        Ok(match key {
            "tune" => json!({"tune": docs["tune"].clone()}),
            "controls" => docs["status"]["controls"].clone(),
            "status" => {
                if let Some(sequence) = docs.get_mut("status_sequence").and_then(Value::as_array_mut) {
                    if !sequence.is_empty() {
                        return Ok(sequence.remove(0));
                    }
                }
                let mut status = docs["status"].clone();
                if let Some(accessories) = status.get_mut("accessories").and_then(Value::as_object_mut) {
                    for key in ["intellibox", "belt", "iflag", "sim3d", "hapticr", "hud", "compensation"] {
                        accessories.entry(key).or_insert_with(|| json!("Not Connected"));
                    }
                }
                status
            }
            other => docs[other].clone(),
        })
    }

    fn upload(&self, _path: &str, _bytes: Vec<u8>) -> Result<Value, Failure> {
        Ok(Value::Null)
    }

    fn fetch(&self, path: &str) -> Result<(Vec<u8>, String), Failure> {
        self.calls.lock().unwrap().push((Method::Get, path.to_owned(), None));
        Ok((b"zip".to_vec(), "file.srs".to_owned()))
    }

    fn translate(&self, text: &str) -> String {
        self.translations.lock().unwrap().push(text.to_owned());
        // An optional fixture language: `docs["translations"][text]`.
        let docs = self.docs.lock().unwrap();
        docs.get("translations").and_then(|table| table.get(text)).and_then(Value::as_str).unwrap_or(text).to_owned()
    }

    fn excluded_strings(&self) -> Vec<String> {
        Vec::new()
    }
}

fn merge(target: &mut Value, patch: &Value) {
    if let (Some(t), Some(p)) = (target.as_object_mut(), patch.as_object()) {
        for (k, v) in p {
            merge(t.entry(k.clone()).or_insert(Value::Null), v);
        }
    } else {
        *target = patch.clone();
    }
}

fn merge_assignment(target: &mut Value, patch: &Value) {
    if let (Some(target), Some(patch)) = (target.as_object_mut(), patch.as_object().filter(|patch| !patch.is_empty())) {
        for (key, value) in patch {
            merge_assignment(target.entry(key.clone()).or_insert(Value::Null), value);
        }
    } else {
        *target = patch.clone();
    }
}

/// The message of the alert the interface shows (the newest "Alert" warning still on screen).
pub fn alert_text(app: &MyApp) -> Option<String> {
    let title = app.backend.translate("Alert");
    app.ui
        .notices
        .history
        .iter()
        .rev()
        .find(|n| n.title == title && app.ui.notices.toasts.contains(&n.id))
        .map(|n| n.message.clone())
}

/// Dismisses the alerts on screen.
pub fn dismiss_alerts(app: &mut MyApp) {
    let title = app.backend.translate("Alert");
    let shown: Vec<u64> = app.ui.notices.history.iter().filter(|n| n.title == title).map(|n| n.id).collect();
    for id in shown {
        app.ui.notices.hide(id);
    }
}

/// The interface's notices, oldest first: title and message.
pub fn notice_texts(app: &MyApp) -> Vec<(String, String)> {
    app.ui.notices.history.iter().map(|n| (n.title.clone(), n.message.clone())).collect()
}

/// The app after the page load (`main`, `onload` → `deferred_main`) and one main-loop pass.
pub fn loaded(backend: Arc<FakeBackend>) -> MyApp {
    let mut app = MyApp::new(backend);
    let body = app.gui.create("body", "BODY");
    let root = app.main(body);
    app.gui.set_root(root);
    dispatch(&mut app, body, "onload", &Default::default());
    app.main_loop_pass();
    app
}

#[test]
fn native_ini_constructs_complete_ui_without_running_jobs() {
    let settings = if let Some(path) = std::env::var_os("OPENRIGCUE_UI_READONLY_INI") {
        let path = std::path::PathBuf::from(path);
        let mut settings = openrigcue_config::settings::Settings::load_ini_only(&path).unwrap();
        settings.recover_legacy_typed_integers(&path, &path.with_file_name("settings.toml"));
        settings
    } else {
        openrigcue_config::settings::Settings::default()
    };
    let backend = FakeBackend::new();
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["settings"] = settings.ui_view().unwrap();
        docs["native_settings"] =
            serde_json::to_value(openrigcue_model::native_value::LegacyValue::Dict(settings.native_snapshot()))
                .unwrap();
    }
    let mut app = MyApp::new(backend);
    let body = app.gui.create("body", "BODY");
    let root = app.main(body);
    app.gui.set_root(root);
    // The first page `onload`: `deferred_main` builds the frame and starts its threads.
    crate::remi::gui::dispatch(&mut app, body, "onload", &Default::default());
    app.threads.clear();
    assert_eq!(app.callback_error, None, "constructor stopped");
    assert!(app.gui.child(root, "loading_label").is_none(), "startup still shows loading");
    assert!(app.ui.mounted, "the interface was built");
    app.main_loop_pass();
    app.threads.clear();
    assert_eq!(app.ml.fatal_error, None, "first UI pass stopped");
    // Every screen draws from these settings.
    for screen in crate::ui::Screen::ALL {
        crate::ui::navigate(&mut app, screen, 0);
        assert!(!crate::ui::screens::main(&app).is_empty(), "{screen:?}");
    }
    app.threads.clear();
}

#[test]
fn uncaught_profile_reload_failure_ends_the_main_loop() {
    use std::sync::atomic::Ordering;

    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    assert_eq!(app.ml.fatal_error, None);
    let generation = app.ml.loaded_generation;
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["native_profile_reload"] = json!(true);
        docs["profile_load_failure"] = json!("TypeError: fixture reload failure");
    }
    app.main_loop_pass();
    assert_eq!(app.ml.fatal_error.as_deref(), Some("TypeError: fixture reload failure"));
    assert_eq!(app.ml.loaded_generation, generation, "the failed reload loaded nothing");
    // The loop has ended: later passes neither synchronize nor load.
    let synchronized = backend.synchronized.load(Ordering::SeqCst);
    backend.docs.lock().unwrap().as_object_mut().unwrap().shift_remove("profile_load_failure");
    app.main_loop_pass();
    assert_eq!(backend.synchronized.load(Ordering::SeqCst), synchronized);
    assert_eq!(app.ml.fatal_error.as_deref(), Some("TypeError: fixture reload failure"));
    app.threads.clear();
}

#[test]
fn shared_main_loop_reload_holds_update_lock_then_releases_it_before_notifications() {
    use std::sync::atomic::Ordering;

    let backend = FakeBackend::new();
    let app = loaded(backend.clone());
    *backend.reload_lock_probe.lock().unwrap() = Some(Arc::downgrade(&app.update_lock));
    backend.docs.lock().unwrap()["native_profile_reload"] = json!(true);
    let previous = backend.synchronized.load(Ordering::SeqCst);
    let shared = Mutex::new(app);
    assert!(super::mainloop::iteration_shared(&shared));
    assert_eq!(backend.synchronized.load(Ordering::SeqCst), previous + 1);
}

#[test]
fn shared_main_loop_without_reload_does_not_wait_for_update_lock() {
    use std::sync::mpsc;
    use std::time::Duration;

    let backend = FakeBackend::new();
    let app = loaded(backend.clone());
    backend.docs.lock().unwrap()["native_profile_reload"] = json!(false);
    let gate = app.update_lock.clone();
    let (ready, waiting) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let owner = std::thread::spawn(move || {
        let _guard = gate.lock();
        ready.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(2)).is_ok()
    });
    waiting.recv().unwrap();
    let shared = Mutex::new(app);
    let result = super::mainloop::iteration_shared(&shared);
    let released_by_caller = release.send(()).is_ok();
    let owner_completed = owner.join().unwrap();
    assert!(result);
    assert!(released_by_caller && owner_completed, "a non-reload pass must finish while update_lock stays held");
}

#[test]
fn shared_main_loop_notification_service_does_not_hold_application_state() {
    let backend = FakeBackend::new();
    let app = loaded(backend.clone());
    backend.docs.lock().unwrap()["native_profile_reload"] = json!(false);
    let shared = Arc::new(Mutex::new(app));
    let probe = Arc::downgrade(&shared);
    let (sent, received) = std::sync::mpsc::channel();
    *backend.finish_probe.lock().unwrap() = Some(Box::new(move || {
        let app = probe.upgrade().unwrap();
        let available = app.try_lock().is_ok();
        sent.send(available).unwrap();
    }));
    assert!(super::mainloop::iteration_shared(&shared));
    assert!(received.recv().unwrap(), "the service-only notification lookup must not hold MyApp");
}

#[test]
fn shared_main_loop_profile_services_release_app_but_keep_source_reload_lock() {
    for native in [Some(true), Some(false), None] {
        let backend = FakeBackend::new();
        let app = loaded(backend.clone());
        backend.docs.lock().unwrap()["native_profile_reload"] = native.map(Value::Bool).unwrap_or(Value::Null);
        let gate = app.update_lock.clone();
        let shared = Arc::new(Mutex::new(app));
        let weak = Arc::downgrade(&shared);
        let (sent, observed) = std::sync::mpsc::channel();
        *backend.reload_probe.lock().unwrap() = Some(Box::new(move |step| {
            let app = weak.upgrade().unwrap();
            sent.send((step.to_owned(), app.try_lock().is_ok(), gate.owned_by_current_thread())).unwrap();
        }));
        assert!(super::mainloop::iteration_shared(&shared));
        let events: Vec<_> = observed.try_iter().collect();
        assert!(!events.is_empty());
        for (step, available, source_lock) in events {
            assert!(available, "{native:?} {step}: profile service must not retain MyApp");
            assert_eq!(source_lock, step != "decision" && native == Some(true), "{native:?} {step}");
        }
    }
}

#[test]
fn independent_native_timer_runs_during_profile_load() {
    use crate::host::UiApp;
    use std::sync::mpsc;
    use std::time::Duration;

    let backend = FakeBackend::new();
    let app = loaded(backend.clone());
    backend.docs.lock().unwrap()["native_profile_reload"] = json!(true);
    let shared = Arc::new(Mutex::new(app));
    crate::host::lock(&shared).attach_shared(Arc::downgrade(&shared));
    let (entered, observed) = mpsc::channel();
    let (release, waiting) = mpsc::channel();
    let waiting = Mutex::new(waiting);
    *backend.reload_probe.lock().unwrap() = Some(Box::new(move |step| {
        if step == "load" {
            entered.send(()).unwrap();
            waiting.lock().unwrap().recv_timeout(Duration::from_secs(3)).unwrap();
        }
    }));
    let runner = shared.clone();
    let thread = std::thread::spawn(move || super::mainloop::iteration_shared(&runner));
    observed.recv_timeout(Duration::from_secs(2)).unwrap();
    let (completed, timer_done) = mpsc::channel();
    let available = if let Ok(mut app) = shared.try_lock() {
        app.timer(Duration::ZERO, move |app| {
            app.ui.search = "timer during profile load".into();
            completed.send(()).unwrap();
        });
        true
    } else {
        false
    };
    let timer_ran = available && timer_done.recv_timeout(Duration::from_secs(2)).is_ok();
    let timer_text = timer_ran.then(|| crate::host::lock(&shared).ui.search.clone());
    release.send(()).unwrap();
    assert!(thread.join().unwrap());
    assert!(timer_ran, "an independent native Timer must run before profile I/O returns");
    assert_eq!(timer_text.as_deref(), Some("timer during profile load"));
}

#[test]
fn shared_profile_service_failures_stop_before_reload_widgets_and_tail() {
    for (native, failure) in
        [(Some(true), "profile_decision_failure"), (Some(true), "profile_load_failure"), (None, "profile_sync_failure")]
    {
        let backend = FakeBackend::new();
        let app = loaded(backend.clone());
        let generation = app.ml.loaded_generation;
        let previous = app.ml.previous_belt_homing.clone();
        {
            let mut docs = backend.docs.lock().unwrap();
            docs["native_profile_reload"] = native.map(Value::Bool).unwrap_or(Value::Null);
            docs[failure] = json!("RuntimeError: failed profile service");
            docs["status"]["profile"]["load_generation"] = json!(999);
            docs["status"]["controls"]["tests"]["belt_homing"] = json!(3);
        }
        let (finished, tail) = std::sync::mpsc::channel();
        *backend.finish_probe.lock().unwrap() = Some(Box::new(move || finished.send(()).unwrap()));
        let shared = Mutex::new(app);
        assert!(!super::mainloop::iteration_shared(&shared));
        let app = crate::host::lock(&shared);
        assert!(app.ml.fatal_error.as_deref().unwrap().contains("failed profile service"));
        assert_eq!(app.ml.loaded_generation, generation, "{failure}");
        assert_eq!(app.ml.previous_belt_homing, previous, "{failure}");
        assert!(tail.try_recv().is_err(), "{failure}: tail must not run");
        assert!(!app.update_lock.owned_by_current_thread(), "{failure}: source lock must unwind");
    }
}

#[test]
fn shared_main_loop_service_failure_stops_before_notification_widgets_and_calibration() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    drop(app.take_threads());
    let previous = app.ml.previous_belt_homing.clone();
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["native_profile_reload"] = json!(false);
        docs["finish_main_loop_failure"] = json!("RuntimeError: stopped at notification service");
        docs["status"]["controls"]["tests"]["belt_homing"] = json!(3);
        docs["notifications"] = json!([
            {"id":987,"kind":"game_not_configured","title":"Later","message":"Later","colour":"red"}
        ]);
    }
    let shared = Mutex::new(app);
    assert!(!super::mainloop::iteration_shared(&shared));
    let mut app = crate::host::lock(&shared);
    assert!(app.ml.fatal_error.as_deref().unwrap().contains("stopped at notification service"));
    assert_eq!(app.ml.previous_belt_homing, previous);
    assert!(!app.ui.notices.service.contains_key(&987), "the later notification is not shown");
    assert!(app.take_threads().iter().all(|thread| thread.name != "check_calibration"));
}

#[test]
fn attached_application_starts_a_worker_before_the_current_callback_returns() {
    use std::sync::mpsc;
    use std::time::Duration;

    let shared = Arc::new(Mutex::new(MyApp::new(FakeBackend::new())));
    let (started, observed) = mpsc::channel();
    let mut app = crate::host::lock(&shared);
    app.attach_shared(Arc::downgrade(&shared));
    app.start_thread("independent-start", Box::new(move |_| started.send(()).unwrap()));
    // The real host retains the app mutex until its current callback finishes.
    // A thread can enter its own target (including a timed wait) before then.
    let result = observed.recv_timeout(Duration::from_secs(1));
    drop(app.take_threads());
    assert!(result.is_ok(), "Thread.start must not wait for another GUI host pass");
}

#[test]
fn attached_application_does_not_queue_a_started_worker_for_a_second_launch() {
    use std::sync::mpsc;
    use std::time::Duration;

    let shared = Arc::new(Mutex::new(MyApp::new(FakeBackend::new())));
    let (started, observed) = mpsc::channel();
    {
        let mut app = crate::host::lock(&shared);
        app.attach_shared(Arc::downgrade(&shared));
        app.start_thread(
            "single-start",
            Box::new(move |app| {
                assert!(app.upgrade().is_some());
                started.send(()).unwrap();
            }),
        );
    }
    let queued = crate::host::lock(&shared).take_threads();
    assert!(queued.is_empty(), "the host must not relaunch an independently started worker");
    observed.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(observed.recv_timeout(Duration::from_secs(1)).is_err());
}

#[test]
fn notification_finish_failure_stops_before_calibration_and_previous_parameters() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    assert_eq!(app.ml.fatal_error, None);
    // Discard queued constructor work; this fixture never runs an application worker.
    drop(app.take_threads());
    let previous = app.ml.previous_belt_homing.clone();
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["finish_main_loop_failure"] = json!("RuntimeError: fixture notification finish failed");
        docs["status"]["controls"]["tests"]["belt_homing"] = json!(3);
        docs["notifications"] = json!([
            {"id": 987, "kind": "game_not_configured", "title": "Later notice", "message": "Later", "colour": "red"}
        ]);
    }
    app.main_loop_pass();
    assert!(app.ml.fatal_error.as_deref().unwrap().contains("fixture notification finish failed"));
    assert_eq!(app.ml.previous_belt_homing, previous);
    assert!(app.take_threads().iter().all(|thread| thread.name != "check_calibration"));
    assert!(!app.ui.notices.service.contains_key(&987), "the later notification is not shown");
}

#[test]
fn late_parameters_start_calibration_from_the_current_homing_value() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    drop(app.take_threads());
    backend.docs.lock().unwrap()["status"]["controls"]["tests"]["belt_homing"] = json!(3);
    super::mainloop::finish_parameters(&mut app).unwrap();
    let threads = app.take_threads();
    assert_eq!(threads.len(), 1);
    super::mainloop::finish_parameters(&mut app).unwrap();
    assert!(app.take_threads().is_empty());
}

#[test]
fn late_parameters_preserve_status_initial_values_and_last_connected_progress() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    assert!(app.ml.previous_intellibox_status.is_none());
    assert!(app.ml.previous_belt_status.is_none());
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["status"]["accessories"]["intellibox"] = json!("Reconnecting");
        docs["status"]["accessories"]["belt"] = json!("Motor Offline");
    }
    super::mainloop::finish_parameters(&mut app).unwrap();
    assert_eq!(app.ml.previous_intellibox_status, Some(LegacyValue::Str("Reconnecting".into())));
    assert_eq!(app.ml.previous_belt_status, Some(LegacyValue::Str("Motor Offline".into())));
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["status"]["accessories"]["intellibox"] = json!("Not Connected");
        docs["status"]["accessories"]["belt"] = json!("Not Connected");
    }
    super::mainloop::finish_parameters(&mut app).unwrap();
    assert_eq!(app.ml.previous_intellibox_status, Some(LegacyValue::Str("Reconnecting".into())));
    assert_eq!(app.ml.previous_belt_status, Some(LegacyValue::Str("Motor Offline".into())));
}

#[test]
fn late_parameters_compare_homing_natively_without_integer_coercion() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    drop(app.take_threads());
    for value in [json!("3"), json!(3.9), Value::Null] {
        backend.docs.lock().unwrap()["status"]["controls"]["tests"]["belt_homing"] = value;
        super::mainloop::finish_parameters(&mut app).unwrap();
        assert!(app.take_threads().is_empty());
    }
    backend.docs.lock().unwrap()["status"]["controls"]["tests"]["belt_homing"] = json!(3.0);
    super::mainloop::finish_parameters(&mut app).unwrap();
    let threads = app.take_threads();
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0].name, "check_calibration");
    backend.docs.lock().unwrap()["status"]["controls"]["tests"]["belt_homing"] = json!(3);
    super::mainloop::finish_parameters(&mut app).unwrap();
    assert!(app.take_threads().is_empty());
}

#[test]
fn late_parameters_keep_the_thread_and_assignment_prefix_before_a_missing_model() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    drop(app.take_threads());
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["status"]["controls"]["tests"]["belt_homing"] = json!(3);
        // The accessory statuses are read last: without them the pass fails there.
        docs["status"].as_object_mut().unwrap().shift_remove("accessories");
    }
    assert!(super::mainloop::finish_parameters(&mut app).unwrap_err().starts_with("KeyError:"));
    // What came before the failure stays: the calibration check started and the homing state is remembered.
    assert_eq!(app.take_threads().iter().map(|thread| thread.name).collect::<Vec<_>>(), ["check_calibration"]);
    assert_eq!(app.ml.previous_belt_homing, Some(LegacyValue::Int(3)));
    assert!(app.ml.previous_intellibox_status.is_none());
    assert!(app.ml.previous_belt_status.is_none());
}

#[test]
fn a_sound_device_refresh_request_opens_the_device_again_and_is_cleared() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    backend.docs.lock().unwrap()["status"]["shakers"]["refresh_soundcard"] = json!(true);
    backend.calls.lock().unwrap().clear();
    app.main_loop_pass();
    assert_eq!(app.ml.fatal_error, None);
    let calls = backend.calls.lock().unwrap().clone();
    assert!(calls.iter().any(|(m, p, _)| *m == Method::Put && p == "shakers/soundcard"));
    assert!(calls.iter().any(|(m, p, _)| *m == Method::Delete && p == "shakers/refresh-soundcard"));
    assert_eq!(backend.docs.lock().unwrap()["status"]["shakers"]["refresh_soundcard"], json!(false));
}

#[test]
fn location_memory_keeps_its_assignment_before_a_missing_game_error() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["status"]["session"]["location"] = json!("Track");
        docs["status"]["session"].as_object_mut().unwrap().remove("game");
    }
    assert!(app.remember_location().unwrap_err().starts_with("KeyError:"));
    assert_eq!(app.ml.previous_location, Some(LegacyValue::Str("Track".into())));
}

/// A notification being shown hands its message to the service to speak; whether it is spoken is the service's
/// reading of the Voice Notifications setting.
#[test]
fn shown_notification_requests_its_message_to_be_spoken() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    backend.calls.lock().unwrap().clear();
    app.alert_notification("Success", "Settings and profile have been saved!", "green", 3.0);
    let spoken: Vec<_> = backend.calls.lock().unwrap().iter().filter(|(_, path, _)| path == "voice").cloned().collect();
    assert_eq!(
        spoken,
        [(Method::Post, "voice".to_owned(), Some(json!({"message": "Settings and profile have been saved!"})))]
    );
}

/// An internal diagnostic is shown inside the translated sentence; any other alert text is shown as given.
#[test]
fn alert_wraps_an_internal_diagnostic_in_a_sentence() {
    let mut app = loaded(FakeBackend::new());
    app.alert("KeyError: 'vehicle'");
    assert_eq!(alert_text(&app).unwrap(), "An internal error occurred: KeyError: 'vehicle'");
    app.alert("This tuning category is disabled for the current setup.");
    assert_eq!(alert_text(&app).unwrap(), "This tuning category is disabled for the current setup.");
    for (text, diagnostic) in [
        ("TypeError: bad operand", true),
        ("Error: plain", false),
        ("Could not import the SRS files: no such file", false),
        ("Note: Error: x", false),
        ("KeyError", false),
    ] {
        assert_eq!(super::is_internal_diagnostic(text), diagnostic, "{text}");
    }
}

/// The stored Pitch effect (the motion hot keys write the tune).
fn stored_pitch(app: &MyApp) -> i64 {
    crate::backend::num(app.tune(&["motion_effects", "pitch"])) as i64
}

#[test]
fn ui_slider_actions_use_the_current_game_gate() {
    let mut failures = Vec::new();
    for (cached, current, expected) in [(Value::Null, json!("api"), 11), (json!("api"), Value::Null, 10)] {
        let backend = FakeBackend::new();
        let mut app = loaded(backend.clone());
        app.set_tune_native_checked(&["motion_effects", "pitch"], &LegacyValue::Int(10)).unwrap();
        app.docs.status["session"]["game"] = cached;
        backend.docs.lock().unwrap()["status"]["session"]["game"] = current;
        backend.docs.lock().unwrap()["ui_actions"] = json!(["motion_box_pitch_up"]);
        super::mainloop::apply_ui_actions(&mut app).unwrap();
        let actual = stored_pitch(&app);
        if actual != expected {
            failures.push(format!("actual {actual}, expected {expected}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn ui_slider_actions_skip_disabled_haptic_rows_from_the_native_flag() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["settings"]["app"]["enable_hapticr"] = json!("no");
    let mut app = loaded(backend.clone());
    // Startup's switch callback normalizes this flag; exercise a later stored string again.
    backend.docs.lock().unwrap()["settings"]["app"]["enable_hapticr"] = json!("no");
    app.docs.settings["app"]["enable_hapticr"] = json!("no");
    app.docs.status["session"]["game"] = json!("api");
    backend.docs.lock().unwrap()["status"]["session"]["game"] = json!("api");
    let effect = &openrigcue_config::catalog::registry::Registry::builtin().haptic_effects[0].id;
    let before = backend.docs.lock().unwrap()["tune"]["haptic_effects"][effect].clone();
    backend.docs.lock().unwrap()["ui_actions"] = json!([format!("haptic_{effect}_up")]);
    super::mainloop::apply_ui_actions(&mut app).unwrap();
    assert_eq!(backend.docs.lock().unwrap()["tune"]["haptic_effects"][effect], before, "HapticR is switched off");
    assert!(backend.patches("profile").iter().all(|patch| patch.get("haptic_effects").is_none()));
    app.threads.clear();
}

#[test]
fn ui_slider_action_callback_failure_stops_later_actions_and_queue_clear() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    app.docs.status["session"]["game"] = json!("api");
    let before = backend.docs.lock().unwrap()["tune"]["belt_heave_factor"].clone();
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["status"]["session"]["game"] = json!("api");
        docs["tune"]["shaker_effects"] = Value::Null;
        docs["ui_actions"] = json!(["shake_box_engine_up", "belt_heave_up"]);
    }
    app.main_loop_pass();
    assert!(
        app.ml.fatal_error.as_deref().is_some_and(|error| error.starts_with("TypeError:")),
        "{:?}",
        app.ml.fatal_error
    );
    assert_eq!(backend.docs.lock().unwrap()["tune"]["belt_heave_factor"], before, "the later Belt key is not applied");
    assert_eq!(backend.ui_actions(), ["shake_box_engine_up", "belt_heave_up"]);
}

#[test]
fn ui_slider_actions_see_actions_queued_by_an_earlier_setter() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    app.set_tune_native_checked(&["motion_effects", "pitch"], &LegacyValue::Int(10)).unwrap();
    app.docs.status["session"]["game"] = json!("api");
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["status"]["session"]["game"] = json!("api");
        docs["ui_actions"] = json!(["shake_box_engine_up"]);
        docs["enqueue_ui_actions_after_profile_write"] = json!(["motion_box_pitch_up"]);
    }
    super::mainloop::apply_ui_actions(&mut app).unwrap();
    assert_eq!(stored_pitch(&app), 11);
}

#[test]
fn ui_slider_actions_keep_one_game_guard_for_the_group() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    app.set_tune_native_checked(&["motion_effects", "pitch"], &LegacyValue::Int(10)).unwrap();
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["status"]["session"]["game"] = json!("api");
        docs["ui_actions"] = json!(["shake_box_engine_up", "motion_box_pitch_up"]);
        docs["game_after_profile_write"] = json!("none");
    }
    super::mainloop::apply_ui_actions(&mut app).unwrap();
    assert_eq!(backend.docs.lock().unwrap()["status"]["session"]["game"], json!("none"));
    assert_eq!(stored_pitch(&app), 11);
}

#[test]
fn ui_slider_actions_see_a_down_action_queued_after_up() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    app.set_tune_native_checked(&["motion_effects", "pitch"], &LegacyValue::Int(10)).unwrap();
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["status"]["session"]["game"] = json!("api");
        docs["ui_actions"] = json!(["motion_box_pitch_up"]);
        docs["enqueue_ui_actions_after_profile_write"] = json!(["motion_box_pitch_down"]);
    }
    super::mainloop::apply_ui_actions(&mut app).unwrap();
    assert_eq!(stored_pitch(&app), 10);
}

#[test]
fn mc_ui_actions_callback_failure_stops_later_calibrations_with_a_warning() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["ui_actions"] = json!(["motion_compensation_all_calibrate"]);
        docs["mc_calibration_failure"] = json!("roll");
    }
    backend.calls.lock().unwrap().clear();
    app.main_loop_pass();
    // The failure is a warning on screen; the main loop goes on and the queue is cleared.
    assert_eq!(app.ml.fatal_error, None);
    assert!(alert_text(&app).is_some_and(|text| text.contains("fixture calibration failure")));
    let actions: Vec<_> = backend
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(method, path, _)| *method == Method::Post && path.starts_with("compensation/actions/"))
        .map(|(_, path, _)| path.clone())
        .collect();
    assert_eq!(actions, ["compensation/actions/pitch_calibrate", "compensation/actions/roll_calibrate"]);
    assert_eq!(backend.docs.lock().unwrap()["ui_actions"], json!([]));
}

#[test]
fn mc_ui_actions_axis_parent_failure_stops_later_actions_and_keeps_the_queue() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    let roll_before = backend.docs.lock().unwrap()["settings"]["compensation"]["roll"].clone();
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["settings"]["compensation"]["pitch"] = Value::Null;
        docs["ui_actions"] = json!(["motion_compensation_pitch_switch", "motion_compensation_roll_switch"]);
    }
    app.main_loop_pass();
    assert_eq!(app.ml.fatal_error.as_deref(), Some("TypeError: 'NoneType' object is not subscriptable"));
    let docs = backend.docs.lock().unwrap();
    assert_eq!(docs["settings"]["compensation"]["pitch"], Value::Null);
    assert_eq!(docs["settings"]["compensation"]["roll"], roll_before);
    assert_eq!(docs["ui_actions"], json!(["motion_compensation_pitch_switch", "motion_compensation_roll_switch"]));
}

#[test]
fn mc_ui_actions_see_an_action_queued_by_an_earlier_calibration() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["ui_actions"] = json!(["motion_compensation_pitch_calibrate"]);
        docs["enqueue_ui_actions_after_calibration"] = json!(["motion_compensation_roll_calibrate"]);
    }
    backend.calls.lock().unwrap().clear();
    app.main_loop_pass();
    assert_eq!(app.ml.fatal_error, None);
    let actions: Vec<_> = backend
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(method, path, _)| *method == Method::Post && path.starts_with("compensation/actions/"))
        .map(|(_, path, _)| path.clone())
        .collect();
    assert_eq!(actions, ["compensation/actions/pitch_calibrate", "compensation/actions/roll_calibrate"]);
    assert_eq!(backend.docs.lock().unwrap()["ui_actions"], json!([]));
}

#[test]
fn game_feature_uses_the_current_game_without_refreshing_documents() {
    let features = crate::catalog::features();
    let (column, supported, unsupported) = features
        .columns
        .iter()
        .find_map(|column| {
            let supported = features.games.iter().find(|(_, row)| row.contains(column))?.0.clone();
            let unsupported = features.games.iter().find(|(_, row)| !row.contains(column))?.0.clone();
            Some((column.clone(), supported, unsupported))
        })
        .unwrap();
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["status"]["session"]["game"] = json!(supported);
    let mut app = MyApp::new(backend.clone());
    app.docs.status = backend.docs.lock().unwrap()["status"].clone();
    assert_eq!(app.game_feature(&column), Some(true));
    backend.docs.lock().unwrap()["status"]["session"]["game"] = json!(unsupported);
    assert_eq!(app.game_feature(&column), Some(false));
    for game in [Value::Null, json!([]), json!("__unknown_game__")] {
        backend.docs.lock().unwrap()["status"]["session"]["game"] = game;
        assert_eq!(app.game_feature(&column), None);
    }
    backend.docs.lock().unwrap()["status"]["session"]["game"] = json!(supported);
    assert_eq!(app.game_feature("__unknown_feature__"), None);
}

#[test]
fn registered_page_events_keep_string_parameters_and_report_strict_flag_errors() {
    let backend = FakeBackend::new();
    let mut app = MyApp::new(backend.clone());
    let body = app.gui.create("body", "BODY");
    app.main(body);
    app.docs.settings = backend.docs.lock().unwrap()["settings"].clone();
    dispatch(
        &mut app,
        body,
        "onresize",
        &[("width".into(), "1234".into()), ("height".into(), "777".into())].into_iter().collect(),
    );
    assert!(app.callback_error.is_none());
    assert_eq!(backend.docs.lock().unwrap()["settings"]["app"]["window_width"], json!("1234"));
    dispatch(&mut app, body, "onpixelratio", &[("pixel_ratio".into(), "1.25".into())].into_iter().collect());
    assert_eq!(backend.docs.lock().unwrap()["settings"]["app"]["device_pixel_ratio"], json!("1.25"));
    backend.docs.lock().unwrap()["settings"]["app"]["close_button_quits"] = json!("bad");
    dispatch(&mut app, body, "onpagehide", &Default::default());
    assert_eq!(app.callback_error.as_deref(), Some("Exception: Boolean value expected. Got bad"));
    assert!(!backend.calls.lock().unwrap().iter().any(|(_, path, _)| path == "desktop/kill"));
}

#[test]
fn close_events_read_current_native_policy_instead_of_the_last_ui_snapshot() {
    for event in ["onpagehide", "on_close"] {
        for (current, expected_quit) in [(json!(true), true), (json!(false), false)] {
            let backend = FakeBackend::new();
            let mut app = MyApp::new(backend.clone());
            app.docs.settings = json!({"app":{"close_button_quits":!expected_quit}});
            backend.docs.lock().unwrap()["settings"]["app"]["close_button_quits"] = current;
            let result = if event == "onpagehide" { app.onpagehide() } else { app.on_close() };
            assert_eq!(result, Ok(()));
            let quit = backend
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, path, _)| path.starts_with("desktop/"))
                .map(|(_, path, _)| path.clone())
                .collect::<Vec<_>>();
            assert_eq!(
                quit,
                if expected_quit { vec!["desktop/close-files", "desktop/kill"] } else { vec![] },
                "{event}"
            );
        }
    }
}

#[test]
fn close_events_preserve_current_native_lookup_and_boolean_failures() {
    for (settings, expected) in [
        (json!({}), "KeyError: 'APP'"),
        (json!({"APP":{}}), "KeyError: 'x_button_exit'"),
        (json!({"APP":{"x_button_exit":"invalid"}}), "Exception: Boolean value expected. Got invalid"),
    ] {
        let backend = FakeBackend::new();
        let mut app = MyApp::new(backend.clone());
        app.docs.settings = json!({"app":{"close_button_quits":true}});
        backend.docs.lock().unwrap()["native_settings"] = settings;
        assert_eq!(app.onpagehide().unwrap_err(), expected);
        assert!(!backend.calls.lock().unwrap().iter().any(|(_, path, _)| path.starts_with("desktop/")));
    }
}

/// The hardware status of a device as the rig panel shows it: its colour and explanation.
fn hardware_state(app: &MyApp, key: &str) -> Option<(String, String)> {
    app.ml.hardware_states.get(key).cloned()
}

/// The hardware notifications sent to the service, in order.
fn hardware_notifications(backend: &FakeBackend) -> Vec<String> {
    backend
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, path, _)| path.starts_with("hardware-notification/"))
        .map(|(_, path, _)| path.clone())
        .collect()
}

#[test]
fn hardware_widget_disabled_haptic_feature_skips_live_mode_and_status_updates() {
    let fake = FakeBackend::new();
    fake.docs.lock().unwrap()["settings"]["app"]["enable_hapticr"] = json!(true);
    let mut app = loaded(fake.clone());
    app.ml.hardware_states.insert("HapticR".into(), ("prior".into(), "prior tooltip".into()));
    {
        let mut docs = fake.docs.lock().unwrap();
        docs["settings"]["app"]["enable_hapticr"] = json!("no");
        docs["status"]["controls"]["switches"].as_object_mut().unwrap().remove("haptic");
    }
    // A switched-off HapticR is neither read nor recorded; its missing switch is never looked up.
    super::mainloop::hardware_status(&mut app).unwrap();
    assert_eq!(hardware_state(&app, "HapticR"), Some(("prior".into(), "prior tooltip".into())));
    assert!(!crate::ui::themes::rig(&app).contains("data-device=\"HapticR\""), "the rig panel leaves it out");
}

#[test]
fn hardware_notification_precedes_later_haptic_status_failure() {
    let fake = FakeBackend::new();
    fake.docs.lock().unwrap()["settings"]["app"]["enable_hapticr"] = json!(true);
    let mut app = loaded(fake.clone());
    fake.calls.lock().unwrap().clear();
    fake.docs.lock().unwrap()["status"]["accessories"]["iflag_firmware"] = json!(1.3);
    fake.docs.lock().unwrap()["status"]["accessories"]["iflag"] = json!("Connected");
    app.ml.hardware_states.insert("HapticR".into(), ("prior".into(), "prior tooltip".into()));
    // The flag is read at the HapticR row, after the iFlag row, and fails there.
    fake.docs.lock().unwrap()["native_setting_sequence"] = json!({"APP": {"enable_hapticr": ["invalid"]}});
    let error = super::mainloop::hardware_status(&mut app).unwrap_err();
    assert!(error.starts_with("Exception: Boolean value expected"), "{error}");
    assert!(hardware_notifications(&fake).iter().any(|path| path == "hardware-notification/IflagFirmware"));
    assert_eq!(hardware_state(&app, "HapticR"), Some(("prior".into(), "prior tooltip".into())));
}

#[test]
fn hardware_notification_branches_follow_device_order_and_skip_disabled_devices() {
    let fake = FakeBackend::new();
    let yaw = crate::catalog::models().iter().find(|model| model.manufacturer == "Yaw VR").unwrap();
    let mut app = loaded(fake.clone());
    {
        let mut docs = fake.docs.lock().unwrap();
        docs["settings"]["motion"]["model"] = json!(yaw.name);
        docs["status"]["motion"]["link"]["status"] = json!("Connected");
        docs["status"]["motion"]["assignment"]["status"] = json!(true);
        docs["status"]["devices"]["yaw_vr"]["centered"] = json!(false);
        docs["status"]["accessories"]["iflag"] = json!("Connected");
        docs["status"]["accessories"]["iflag_firmware"] = json!(1.3);
        docs["status"]["accessories"]["belt"] = json!("Connected");
        docs["status"]["controls"]["switches"]["belt"] = json!(true);
    }
    fake.calls.lock().unwrap().clear();
    super::mainloop::hardware_status(&mut app).unwrap();
    assert_eq!(
        hardware_notifications(&fake),
        [
            "hardware-notification/YawVr { needs_center: true }",
            "hardware-notification/IflagFirmware",
            "hardware-notification/Belt { needs_calibration: true }",
        ]
    );
    for key in ["Motionbox", "iFlag", "Belt"] {
        assert_eq!(hardware_state(&app, key).unwrap().0, "yellow", "{key} needs attention");
    }
    fake.calls.lock().unwrap().clear();
    for section in ["motion", "iflag", "belt"] {
        fake.docs.lock().unwrap()["settings"][section]["auto_search"] = json!(false);
    }
    super::mainloop::hardware_status(&mut app).unwrap();
    assert!(hardware_notifications(&fake).is_empty());
    for key in ["Motionbox", "iFlag", "Belt"] {
        assert_eq!(hardware_state(&app, key).unwrap().0, "orange", "{key} is switched off");
    }
}

#[test]
fn hardware_notification_failure_keeps_its_image_and_stops_later_widgets() {
    let fake = FakeBackend::new();
    let mut app = loaded(fake.clone());
    fake.docs.lock().unwrap()["status"]["accessories"]["iflag_firmware"] = json!(1.3);
    fake.docs.lock().unwrap()["status"]["accessories"]["iflag"] = json!("Connected");
    fake.docs.lock().unwrap()["status"]["accessories"]["hud"] = json!("Connected");
    fake.docs.lock().unwrap()["hardware_notification_failure"] = json!("RuntimeError: fixture notice failure");
    app.ml.hardware_states.insert("Hud".into(), ("prior".into(), "prior hud".into()));
    assert_eq!(super::mainloop::hardware_status(&mut app).unwrap_err(), "RuntimeError: fixture notice failure");
    assert_eq!(hardware_notifications(&fake), ["hardware-notification/IflagFirmware"]);
    // The HUD row comes after the iFlag row: the failure stops before it.
    assert_eq!(hardware_state(&app, "Hud"), Some(("prior".into(), "prior hud".into())));
}

#[test]
fn hardware_widget_bad_haptic_flag_stops_after_earlier_modes() {
    let fake = FakeBackend::new();
    fake.docs.lock().unwrap()["settings"]["app"]["enable_hapticr"] = json!(true);
    let mut app = loaded(fake.clone());
    app.ml.hardware_states.insert("Belt".into(), ("prior".into(), "prior belt".into()));
    fake.docs.lock().unwrap()["settings"]["app"]["enable_hapticr"] = json!("invalid");
    let error = super::mainloop::hardware_status(&mut app).unwrap_err();
    assert!(error.starts_with("Exception: Boolean value expected"), "{error}");
    assert_eq!(hardware_state(&app, "Belt"), Some(("prior".into(), "prior belt".into())));
}

#[test]
fn hardware_widget_missing_native_model_stops_before_mode_updates() {
    let fake = FakeBackend::new();
    let mut app = loaded(fake.clone());
    app.ml.hardware_states.insert("Intellibox".into(), ("prior".into(), "prior".into()));
    fake.docs.lock().unwrap()["settings"]["motion"].as_object_mut().unwrap().remove("model");
    assert_eq!(super::mainloop::hardware_status(&mut app).unwrap_err(), "KeyError: 'model'");
    assert_eq!(hardware_state(&app, "Intellibox"), Some(("prior".into(), "prior".into())));
}

#[test]
fn hardware_live_model_changed_assignment_flag_is_read_after_modes() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    let assigned = crate::catalog::models().iter().find(|model| model.axis_assignment).unwrap();
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["status"]["motion"]["link"]["status"] = json!("Connected");
        docs["status"]["motion"]["assignment"]["status"] = json!(false);
        docs["status"]["devices"]["yaw_vr"]["centered"] = json!(true);
        docs["native_setting_sequence"] = json!({"MOTIONBOX": {"model": ["None", assigned.name]}});
    }
    super::mainloop::hardware_status(&mut app).unwrap();
    let (color, detail) = hardware_state(&app, "Motionbox").unwrap();
    assert_eq!(color, "yellow", "the model read at the assignment check has an assignment");
    assert!(detail.contains("actuator assignment is wrong or unfinished"), "{detail}");
}

#[test]
fn hardware_native_status_identity_uses_current_parameter_after_status_capture() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["status"]["accessories"]["intellibox"] = json!("Connected");
        docs["status"]["accessories"]["intellibox_model"] = json!(1);
        docs["hardware_parameter_sequence"] = json!({"intellibox_model": [2]});
    }
    super::mainloop::hardware_status(&mut app).unwrap();
    let (color, detail) = hardware_state(&app, "Intellibox").unwrap();
    assert_eq!(color, "green");
    assert!(detail.starts_with("Model: 2"), "{detail}");
}

#[test]
fn hardware_native_status_mc_port_concat_error_precedes_connected_image() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    app.ml.hardware_states.insert("MCMode".into(), ("prior".into(), "prior mc".into()));
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["settings"]["compensation"]["sensor"] = json!("witmotion");
        docs["status"]["accessories"]["compensation"] = json!("Connected");
        docs["hardware_parameter_sequence"] = json!({"mc_com_port": [false]});
    }
    assert_eq!(
        super::mainloop::hardware_status(&mut app).unwrap_err(),
        "TypeError: can only concatenate str (not \"bool\") to str"
    );
    assert_eq!(hardware_state(&app, "MCMode"), Some(("prior".into(), "prior mc".into())));
}

#[test]
fn hardware_native_status_hud_requires_string_startswith_before_changing_image() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    app.ml.hardware_states.insert("Hud".into(), ("prior".into(), "prior hud".into()));
    backend.docs.lock().unwrap()["hardware_parameter_sequence"] = json!({"hud_status": [false, false]});
    assert_eq!(
        super::mainloop::hardware_status(&mut app).unwrap_err(),
        "AttributeError: 'bool' object has no attribute 'startswith'"
    );
    assert_eq!(hardware_state(&app, "Hud"), Some(("prior".into(), "prior hud".into())));
}

#[test]
fn hardware_native_status_assignment_uses_false_identity_instead_of_numeric_equality() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["status"]["motion"]["link"]["status"] = json!("Connected");
        docs["status"]["motion"]["assignment"]["status"] = json!(false);
        docs["status"]["devices"]["yaw_vr"]["centered"] = json!(true);
        docs["native_setting_sequence"] = json!({"MOTIONBOX": {"model": ["None", "Missing model"]}});
        docs["hardware_parameter_sequence"] = json!({"axis_assignment_status": [0]});
    }
    super::mainloop::hardware_status(&mut app).unwrap();
    // The number zero is not False: the model is not read again for the assignment.
    assert_eq!(backend.docs.lock().unwrap()["native_setting_sequence"]["MOTIONBOX"]["model"], json!(["Missing model"]));
    assert_eq!(hardware_state(&app, "Motionbox").unwrap().0, "green");
}

#[test]
fn hardware_live_model_late_lookup_error_keeps_earlier_image_and_old_titles() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    app.ml.hardware_states.insert("Motionbox".into(), ("prior".into(), "prior motion".into()));
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["status"]["motion"]["link"]["status"] = json!("Connected");
        docs["status"]["motion"]["assignment"]["status"] = json!(false);
        docs["native_setting_sequence"] = json!({"MOTIONBOX": {"model": ["None", "Missing model"]}});
    }
    assert_eq!(super::mainloop::hardware_status(&mut app).unwrap_err(), "KeyError: 'Missing model'");
    assert_eq!(hardware_state(&app, "Motionbox"), Some(("prior".into(), "prior motion".into())));
}

#[test]
fn hardware_live_model_guarded_fields_are_skipped_when_assignment_and_center_are_complete() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["status"]["motion"]["link"]["status"] = json!("Connected");
        docs["status"]["motion"]["assignment"]["status"] = json!(true);
        docs["status"]["devices"]["yaw_vr"]["centered"] = json!(true);
        docs["native_setting_sequence"] = json!({"MOTIONBOX": {"model": ["None", "Missing model"]}});
    }
    super::mainloop::hardware_status(&mut app).unwrap();
    assert_eq!(backend.docs.lock().unwrap()["native_setting_sequence"]["MOTIONBOX"]["model"], json!(["Missing model"]));
    assert_eq!(hardware_state(&app, "Motionbox").unwrap().0, "green");
}

#[test]
fn hardware_native_auto_search_false_text_disables_the_device() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    backend.docs.lock().unwrap()["settings"]["wind"]["auto_search"] = json!("False");
    super::mainloop::hardware_status(&mut app).unwrap();
    assert_eq!(
        hardware_state(&app, "Intellibox"),
        Some(("orange".into(), "Intellibox is switched off on the Setup -> Hardware page".into()))
    );
}

#[test]
fn hardware_native_ports_bad_value_keeps_pinned_image_before_concat_error() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    app.ml.hardware_states.insert("Intellibox".into(), ("prior".into(), "prior tooltip".into()));
    backend.docs.lock().unwrap()["settings"]["wind"]["serial_port"] = json!(17);
    assert_eq!(
        super::mainloop::hardware_status(&mut app).unwrap_err(),
        "TypeError: can only concatenate str (not \"int\") to str"
    );
    assert_eq!(hardware_state(&app, "Intellibox"), Some(("prior".into(), "prior tooltip".into())));
}

#[test]
fn hardware_native_ports_reread_after_guard_and_before_tooltip_concat() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    backend.docs.lock().unwrap()["native_setting_sequence"] = json!({"INTELLIBOX": {"com_port": ["/dev/first", 17]}});
    assert_eq!(
        super::mainloop::hardware_status(&mut app).unwrap_err(),
        "TypeError: can only concatenate str (not \"int\") to str"
    );
    // The pinned port is read for the guard, then again for the explanation.
    assert_eq!(backend.docs.lock().unwrap()["native_setting_sequence"]["INTELLIBOX"]["com_port"], json!([]));
}

#[test]
fn hardware_native_ports_pinned_port_is_named_in_the_explanation() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    backend.docs.lock().unwrap()["settings"]["wind"]["serial_port"] = json!("/dev/ttyUSB3");
    super::mainloop::hardware_status(&mut app).unwrap();
    let (color, detail) = hardware_state(&app, "Intellibox").unwrap();
    assert_eq!(color, "red");
    assert!(
        detail.starts_with("No connection. Check that the USB cable is plugged in . Auto search is switched off")
            && detail.contains("is /dev/ttyUSB3."),
        "{detail}"
    );
}

#[test]
fn hardware_native_ports_soundcard_concat_failure_precedes_shakebox_image() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    app.ml.hardware_states.insert("Shakebox".into(), ("prior".into(), "prior shake".into()));
    backend.docs.lock().unwrap()["settings"]["shakers"]["sound_card"] = json!(false);
    assert_eq!(
        super::mainloop::hardware_status(&mut app).unwrap_err(),
        "TypeError: can only concatenate str (not \"bool\") to str"
    );
    assert_eq!(hardware_state(&app, "Shakebox"), Some(("prior".into(), "prior shake".into())));
}

#[test]
fn hardware_native_auto_search_invalid_first_flag_stops_before_images() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    app.ml.hardware_states.insert("Intellibox".into(), ("prior".into(), "prior".into()));
    backend.docs.lock().unwrap()["settings"]["wind"]["auto_search"] = json!("invalid");
    let error = super::mainloop::hardware_status(&mut app).unwrap_err();
    assert!(error.starts_with("Exception: Boolean value expected"), "{error}");
    assert_eq!(hardware_state(&app, "Intellibox"), Some(("prior".into(), "prior".into())));
}

#[test]
fn hardware_native_auto_search_invalid_belt_flag_keeps_earlier_images_and_old_titles() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    app.ml.hardware_states.insert("Belt".into(), ("prior".into(), "prior belt".into()));
    backend.docs.lock().unwrap()["settings"]["belt"]["auto_search"] = json!("invalid");
    let error = super::mainloop::hardware_status(&mut app).unwrap_err();
    assert!(error.starts_with("Exception: Boolean value expected"), "{error}");
    assert_eq!(hardware_state(&app, "Belt"), Some(("prior".into(), "prior belt".into())));
}

/// The hardware status: each device's colour and explanation are recorded for the rig panel, which shows the
/// explanation on the device row; the main loop runs the status rules again only when what they read changed.
#[test]
fn hardware_tooltips_and_snapshot() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["status"]["accessories"]["belt"] = json!("Not Connected");
    let mut app = loaded(backend.clone());
    assert_eq!(hardware_state(&app, "Belt"), Some(("red".to_owned(), "No USB link to the belt".to_owned())));
    let rig = crate::ui::themes::rig(&app);
    assert!(rig.contains("title=\"No USB link to the belt\""), "{rig}");
    assert!(alert_text(&app).is_none());
    // Nothing the rules read changed: the next pass leaves the recorded status alone.
    app.ml.hardware_states.insert("Belt".into(), ("kept".into(), "kept".into()));
    app.main_loop_pass();
    assert_eq!(hardware_state(&app, "Belt"), Some(("kept".to_owned(), "kept".to_owned())));
    // A status change runs them again; a powerless belt has its own explanation.
    backend.docs.lock().unwrap()["status"]["accessories"]["belt"] = json!("Motor Offline");
    app.main_loop_pass();
    assert_eq!(hardware_state(&app, "Belt"), Some(("red".to_owned(), "Belt has no power connection".to_owned())));
    assert!(crate::ui::themes::rig(&app).contains("Belt has no power connection"));
}

#[test]
fn shared_alert_keeps_current_message_and_belt_status_does_not_navigate() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["settings"]["belt"]["auto_search"] = json!(false);
    let mut app = loaded(backend);
    app.goto_page_checked("Setup", "Motion Compensation").unwrap();
    assert_eq!(app.ui.screen, crate::ui::Screen::Compensation);
    assert_eq!(
        hardware_state(&app, "Belt"),
        Some(("orange".to_owned(), "Motion is switched off on the Setup -> Hardware page".to_owned()))
    );
    // Drawing the rig panel shows the explanation without opening another screen or an alert.
    assert!(crate::ui::themes::rig(&app).contains("Motion is switched off on the Setup -&gt; Hardware page"));
    assert_eq!(app.ui.screen, crate::ui::Screen::Compensation);
    assert!(alert_text(&app).is_none());
    app.alert("A different alert <with markup>");
    assert_eq!(alert_text(&app).unwrap(), "A different alert <with markup>");
    // The notice escapes its markup.
    assert!(crate::ui::notices::toasts(&app).contains("A different alert &lt;with markup&gt;"));
    dismiss_alerts(&mut app);
    app.goto_page_checked("Tuning", "Belt").unwrap();
    assert_eq!(app.ui.screen, crate::ui::Screen::Belt);
    assert!(crate::ui::screens::main(&app).contains("data-arg=\"Belt:belt_surge\""));
    assert!(alert_text(&app).is_none());
    app.threads.clear();
}

#[test]
fn hardware_snapshot_uses_native_numeric_equality() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["status"]["accessories"]["iflag_firmware"] = json!(1);
    let mut app = loaded(backend.clone());
    // Both numeric forms compare equal to the previous integer value.
    for value in [json!(1.0), json!(true)] {
        app.ml.hardware_states.insert("Hud".into(), ("kept".into(), "retained".into()));
        backend.docs.lock().unwrap()["status"]["accessories"]["iflag_firmware"] = value;
        app.main_loop_pass();
        assert_eq!(app.ml.fatal_error, None);
        assert_eq!(hardware_state(&app, "Hud"), Some(("kept".to_owned(), "retained".to_owned())));
    }
}

#[test]
fn hardware_snapshot_ignores_motion_readouts_that_the_source_does_not_select() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    app.ml.hardware_states.insert("Hud".into(), ("kept".into(), "retained".into()));
    backend.docs.lock().unwrap()["status"]["motion"]["heave_cue"] = json!(0.5);
    app.main_loop_pass();
    assert_eq!(app.ml.fatal_error, None);
    assert_eq!(hardware_state(&app, "Hud"), Some(("kept".to_owned(), "retained".to_owned())));
}

#[test]
fn hardware_iflag_version_comparison_rejects_text_at_its_source_branch() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    backend.docs.lock().unwrap()["status"]["accessories"]["iflag_firmware"] = json!("1.3");
    backend.docs.lock().unwrap()["status"]["accessories"]["iflag"] = json!("Connected");
    app.ml.hardware_states.insert("iFlag".into(), ("prior".into(), "prior".into()));
    assert_eq!(
        super::mainloop::hardware_status(&mut app).unwrap_err(),
        "TypeError: '<=' not supported between instances of 'str' and 'float'"
    );
    assert_eq!(hardware_state(&app, "iFlag"), Some(("prior".to_owned(), "prior".to_owned())));
    // The disabled branch does not read the version at all.
    backend.docs.lock().unwrap()["settings"]["iflag"]["auto_search"] = json!(false);
    super::mainloop::hardware_status(&mut app).unwrap();
    assert_eq!(hardware_state(&app, "iFlag").unwrap().0, "orange");
}

#[test]
fn hardware_iflag_version_reads_current_value_without_cached_coercion() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    app.docs.status["accessories"]["iflag"] = json!("Connected");
    app.docs.status["accessories"]["iflag_firmware"] = json!(1.4);
    backend.docs.lock().unwrap()["status"]["accessories"]["iflag_firmware"] = json!(true);
    backend.docs.lock().unwrap()["status"]["accessories"]["iflag"] = json!("Connected");
    super::mainloop::hardware_status(&mut app).unwrap();
    let (color, detail) = hardware_state(&app, "iFlag").unwrap();
    assert_eq!(color, "yellow", "the current value True counts as 1, an old firmware");
    assert!(detail.contains("cannot work with the firmware on this iFlag"), "{detail}");
}

#[test]
fn hardware_snapshot_includes_notification_dismissal_and_showing_state() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    for flags in [
        HardwareNotificationState { dismissed_iflag: true, ..Default::default() },
        HardwareNotificationState { showing_belt: true, ..Default::default() },
    ] {
        app.ml.hardware_states.insert("Hud".into(), ("stale".into(), "stale".into()));
        *backend.hardware_notification_state.lock().unwrap() = flags;
        app.main_loop_pass();
        assert_eq!(app.ml.fatal_error, None);
        assert_ne!(hardware_state(&app, "Hud"), Some(("stale".to_owned(), "stale".to_owned())));
    }
}

/// The page being shown tells the service the application has loaded (the service applies the log level), once
/// per showing and only then.
#[test]
fn onpageshow_marks_the_app_loaded() {
    let backend = FakeBackend::new();
    let mut app = MyApp::new(backend.clone());
    let body = app.gui.create("body", "BODY");
    let root = app.main(body);
    app.gui.set_root(root);
    dispatch(&mut app, body, "onload", &Default::default());
    let loaded = |backend: &FakeBackend| {
        backend.calls.lock().unwrap().iter().filter(|(m, p, _)| *m == Method::Post && p == "app-loaded").count()
    };
    assert_eq!(loaded(&backend), 0);
    dispatch(&mut app, body, "onpageshow", &parse_parameters("9|width=800|10|height=600"));
    assert_eq!(loaded(&backend), 1);
    app.threads.clear();
}

/// The downloaders answer the attribute call `/<id>/download` with their file.
#[test]
fn downloads_answer_attribute_calls() {
    let mut app = loaded(FakeBackend::new());
    app.downloads.insert("123".into(), ("game.srs".into(), b"zip".to_vec()));
    let answer = app.attribute_call("123", "download", &[]).unwrap();
    assert_eq!(answer.content, b"zip");
    assert!(answer.headers.iter().any(|(k, v)| k == "Content-Disposition" && v == "attachment; filename=\"game.srs\""));
    assert!(app.attribute_call("124", "download", &[]).is_none());
}

/// A start that reset the stored configuration files opens one window with the service's title and text; a
/// normal start opens none.
#[test]
fn startup_reset_notice_shows_the_service_text_until_dismissed() {
    let reset_notices = |app: &MyApp, title: &str| notice_texts(app).into_iter().filter(|(t, _)| t == title).count();
    let (title, message) = ("Ops, something is not right with the config files...", "The settings were reset.");
    let mut app = loaded(FakeBackend::new());
    assert_eq!(reset_notices(&app, title), 0);
    app.threads.clear();
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["start_info"] = json!({
        "first_installation": false,
        "reset": {"title": title, "text": message, "reason": "fixture reason"},
        "profile_path": null,
    });
    let mut app = loaded(backend);
    assert_eq!(notice_texts(&app).iter().filter(|(t, m)| t == title && m == message).count(), 1);
    let notice = app.ui.notices.history.iter().find(|n| n.title == title).unwrap();
    assert_eq!(notice.severity, crate::ui::notices::Severity::Warning, "a warning stays until dismissed");
    assert!(app.ui.notices.toasts.contains(&notice.id));
    // The start information is read once: later passes show no further notice.
    app.main_loop_pass();
    assert_eq!(reset_notices(&app, title), 1);
    assert!(app.ml.fatal_error.is_none(), "{:?}", app.ml.fatal_error);
    app.threads.clear();
}

#[test]
fn a_missing_compensation_setting_no_longer_stops_the_start() {
    let backend = FakeBackend::new();
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["settings"]["app"]["gear_shift_sound"] = json!(false);
        docs["settings"]["compensation"].as_object_mut().unwrap().remove("height");
    }
    let mut app = loaded(backend.clone());
    // The start puts each stored Settings number of Motion Compensation into its box again; the missing one is
    // left out and the start goes on.
    assert_eq!(app.ml.fatal_error, None);
    assert!(backend.calls.lock().unwrap().iter().any(|(_, path, _)| path == "start-info"));
    let compensation: Vec<Value> =
        backend.patches("settings").into_iter().filter_map(|patch| patch.get("compensation").cloned()).collect();
    assert!(!compensation.iter().any(|patch| patch.get("height").is_some()));
    assert!(compensation.iter().any(|patch| patch.get("surge_horizontal_stroke").is_some()));
    assert_eq!(backend.docs.lock().unwrap()["settings"]["app"]["gear_shift_sound"], json!(false));
    crate::ui::navigate(&mut app, crate::ui::Screen::Compensation, 0);
    assert!(!crate::ui::screens::main(&app).is_empty());
    app.threads.clear();
}

#[test]
fn the_start_keeps_the_stored_fan_count_and_one_fan_turns_wind_curving_off() {
    for (stored, curving) in [(json!("1"), json!(0)), (json!("2"), json!(1))] {
        let backend = FakeBackend::new();
        backend.docs.lock().unwrap()["settings"]["wind"]["fan_count"] = stored.clone();
        let app = loaded(backend.clone());
        assert!(app.ml.fatal_error.is_none(), "{stored}: {:?}", app.ml.fatal_error);
        assert_eq!(backend.docs.lock().unwrap()["settings"]["wind"]["fan_count"], stored);
        assert_eq!(backend.docs.lock().unwrap()["tune"]["wind_curving"], curving, "{stored}");
        drop(app);
    }
}

#[test]
fn the_start_leaves_a_bad_profile_value_alone() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["tune"]["speed_factor"] = Value::Null;
    let app = loaded(backend.clone());
    assert_eq!(backend.docs.lock().unwrap()["tune"]["speed_factor"], Value::Null);
    assert_eq!(backend.docs.lock().unwrap()["settings"]["wind"]["fan_count"], json!("2"));
    drop(app);
}

/// Exercise the actual registered frame without starting an app, server or queued worker.
#[test]
fn authored_branding_resources_resolve_in_the_actual_fake_frame() {
    use base64::Engine as _;

    fn embed_resources(mut content: String) -> String {
        for (path, bytes) in crate::assets::RES {
            let url = format!("/res:{path}");
            if content.contains(&url) {
                let bytes = if path.ends_with(".css") {
                    embed_resources(std::str::from_utf8(bytes).unwrap().to_owned()).into_bytes()
                } else {
                    bytes.to_vec()
                };
                let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
                content =
                    content.replace(&url, &format!("data:{};base64,{encoded}", crate::assets::content_type(path)));
            }
        }
        content
    }

    let backend = FakeBackend::new();
    let settings = openrigcue_config::settings::Settings::default();
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["settings"] = settings.ui_view().unwrap();
        docs["native_settings"] =
            serde_json::to_value(openrigcue_model::native_value::LegacyValue::Dict(settings.native_snapshot()))
                .unwrap();
    }
    let mut app = loaded(backend);
    assert!(app.ml.fatal_error.is_none(), "{:?}", app.ml.fatal_error);
    for (global, page, expected) in [
        ("Tuning", "Motion", "/res:images/openrigcue/mark.svg"),
        ("Setup", "Hardware", "/res:images/openrigcue/mark.svg"),
    ] {
        app.goto_page_checked(global, page).unwrap();
        // Discard every queued task, including startup/main-loop and fake page jobs.
        app.threads.clear();
        let html = app.gui.full_html();
        assert!(html.contains(expected), "{global}/{page} is missing the OpenRigCue mark");
        for reference in html.split("src=\"/res:").skip(1) {
            let path = reference.split('"').next().unwrap();
            assert!(crate::assets::file(path).is_some(), "{global}/{page} references missing {path}");
        }
        if let Some(directory) = std::env::var_os("OPENRIGCUE_STATIC_PREVIEW_DIR") {
            let styles = [crate::assets::UI_STYLESHEET]
                .into_iter()
                .map(|path| {
                    format!("<style>{}</style>", std::str::from_utf8(crate::assets::file(path).unwrap()).unwrap())
                })
                .collect::<String>();
            let document = format!("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; img-src data:; style-src 'unsafe-inline'; font-src data:;\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>OpenRigCue static fake-backend {page} preview</title>{styles}</head><body><aside style=\"position:fixed;bottom:0;left:0;z-index:999999;background:#15171b;color:#adb3bd;padding:4px 10px;font:12px sans-serif\">Static Rust GUI output with fake data; all jobs discarded. No live application.</aside>{html}</body></html>");
            let document = embed_resources(document);
            assert!(!document.contains("/res:"), "unresolved resource in exported {page}");
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(std::path::Path::new(&directory).join(format!("{}.html", page.to_lowercase())), document)
                .unwrap();
        }
    }
    assert!(app.threads.is_empty());
}

struct ParameterTailBackend {
    inner: Arc<FakeBackend>,
    operations: Mutex<Vec<String>>,
    fail: Option<&'static str>,
    wait: Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
    homing_reads: std::sync::atomic::AtomicUsize,
    start_homing: i64,
}
impl ParameterTailBackend {
    fn record(&self, name: &str) -> Result<(), Failure> {
        self.operations.lock().unwrap().push(name.to_owned());
        if let Some((entered, release)) = self.wait.lock().unwrap().take() {
            entered.send(()).unwrap();
            release.recv().unwrap();
        }
        if self.fail == Some(name) {
            Err(Failure { status: 500, detail: format!("fixture failure: {name}") })
        } else {
            Ok(())
        }
    }
}
impl Backend for ParameterTailBackend {
    fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value, Failure> {
        self.inner.call(method, path, body)
    }
    fn upload(&self, path: &str, bytes: Vec<u8>) -> Result<Value, Failure> {
        self.inner.upload(path, bytes)
    }
    fn fetch(&self, path: &str) -> Result<(Vec<u8>, String), Failure> {
        self.inner.fetch(path)
    }
    fn translate(&self, text: &str) -> String {
        text.into()
    }
    fn excluded_strings(&self) -> Vec<String> {
        Vec::new()
    }
    fn control_value(&self, path: &[&str]) -> Result<LegacyValue, Failure> {
        if path == ["tests", "belt_homing"] {
            let nth = self.homing_reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.record(if nth == 0 { "homing start" } else { "homing remembered" })?;
            return Ok(LegacyValue::Int(if nth == 0 { self.start_homing } else { 1 }));
        }
        self.inner.control_value(path)
    }
    fn motion_parameter(&self, name: &str) -> Result<LegacyValue, Failure> {
        self.record(name)?;
        Ok(if name == "motion_status" { "Connected".into() } else { LegacyValue::Dict(Default::default()) })
    }
    fn native_setting(&self, section: &str, key: &str) -> Result<LegacyValue, Failure> {
        if section == "MOTIONBOX" && key == "model" {
            self.record("motion model")?;
            return Ok("P3".into());
        }
        self.inner.native_setting(section, key)
    }
    fn accessory_status(&self, name: &str) -> Result<LegacyValue, Failure> {
        self.record(name)?;
        Ok("Connected".into())
    }
}

#[test]
fn parameter_tail_service_read_allows_native_timer() {
    use std::time::Duration;
    let inner = FakeBackend::new();
    let mut app = loaded(inner.clone());
    drop(app.take_threads());
    let (entered, started) = std::sync::mpsc::channel();
    let (release, resume) = std::sync::mpsc::channel();
    let backend = Arc::new(ParameterTailBackend {
        inner,
        operations: Mutex::new(Vec::new()),
        fail: None,
        wait: Mutex::new(Some((entered, resume))),
        homing_reads: std::sync::atomic::AtomicUsize::new(0),
        start_homing: 0,
    });
    app.backend = backend;
    let label = app.gui.create("p", "orc-loading-message");
    app.gui.set_text(label, "waiting");
    let shared = Arc::new(Mutex::new(app));
    crate::host::lock(&shared).attach_shared(Arc::downgrade(&shared));
    let work = shared.clone();
    let (complete, done) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        complete.send(super::mainloop::finish_parameters_shared(&work)).unwrap();
    });
    started.recv_timeout(Duration::from_secs(2)).unwrap();
    let (timer_complete, timer_done) = std::sync::mpsc::channel();
    let available = if let Ok(mut app) = shared.try_lock() {
        app.timer(Duration::ZERO, move |app| {
            app.gui.set_text(label, "tail running");
            timer_complete.send(()).unwrap();
        });
        true
    } else {
        false
    };
    let timer_ran = available && timer_done.recv_timeout(Duration::from_secs(2)).is_ok();
    release.send(()).unwrap();
    done.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
    worker.join().unwrap();
    assert!(available, "pure previous-parameter service read retained MyApp");
    assert!(timer_ran, "independent native Timer was blocked by parameter tail");
    assert_eq!(crate::host::lock(&shared).gui.get_text(label), "tail running");
}

#[test]
fn parameter_tail_shared_preserves_each_read_and_assignment_prefix() {
    // The reads that fed only the old Motion page's own state are not part of the pass's behaviour.
    let main_loop_reads = |operations: Vec<String>| {
        operations
            .into_iter()
            .filter(|name| !matches!(name.as_str(), "motion_status" | "motion model" | "thanos_config"))
            .collect::<Vec<_>>()
    };
    for fail in [None, Some("homing remembered"), Some("intellibox"), Some("belt")] {
        let run = |shared_mode| {
            let inner = FakeBackend::new();
            let mut app = loaded(inner.clone());
            drop(app.take_threads());
            let backend = Arc::new(ParameterTailBackend {
                inner,
                operations: Mutex::new(Vec::new()),
                fail,
                wait: Mutex::new(None),
                homing_reads: std::sync::atomic::AtomicUsize::new(0),
                start_homing: 3,
            });
            app.backend = backend.clone();
            let state = Mutex::new(app);
            let result = if shared_mode {
                super::mainloop::finish_parameters_shared(&state)
            } else {
                super::mainloop::finish_parameters(&mut crate::host::lock(&state))
            };
            let mut app = crate::host::lock(&state);
            let values = (
                app.ml.previous_belt_homing.clone(),
                app.ml.previous_intellibox_status.clone(),
                app.ml.previous_belt_status.clone(),
            );
            let operations = main_loop_reads(backend.operations.lock().unwrap().clone());
            let threads = app.take_threads().iter().map(|thread| thread.name).collect::<Vec<_>>();
            (result, values, operations, threads)
        };
        let shared = run(true);
        if fail.is_none() {
            assert_eq!(shared.2, ["homing start", "homing remembered", "intellibox", "intellibox", "belt", "belt"]);
            assert_eq!(shared.3, ["check_calibration"]);
            assert_eq!(
                shared.1,
                (
                    Some(LegacyValue::Int(1)),
                    Some(LegacyValue::Str("Connected".into())),
                    Some(LegacyValue::Str("Connected".into()))
                )
            );
        }
        assert_eq!(shared, run(false), "failure at {fail:?}");
    }
}
struct LocationReadBackend {
    inner: Arc<FakeBackend>,
    operations: Mutex<Vec<String>>,
    wait: Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
    fail: Option<&'static str>,
    location_reads: std::sync::atomic::AtomicUsize,
    game: &'static str,
}
impl LocationReadBackend {
    fn operation(&self, name: &'static str) -> Result<(), Failure> {
        self.operations.lock().unwrap().push(name.into());
        if let Some((entered, release)) = self.wait.lock().unwrap().take() {
            entered.send(()).unwrap();
            release.recv().unwrap();
        }
        if self.fail == Some(name) {
            Err(Failure { status: 500, detail: format!("failure: {name}") })
        } else {
            Ok(())
        }
    }
}
impl Backend for LocationReadBackend {
    fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value, Failure> {
        self.inner.call(method, path, body)
    }
    fn upload(&self, path: &str, bytes: Vec<u8>) -> Result<Value, Failure> {
        self.inner.upload(path, bytes)
    }
    fn fetch(&self, path: &str) -> Result<(Vec<u8>, String), Failure> {
        self.inner.fetch(path)
    }
    fn translate(&self, text: &str) -> String {
        text.into()
    }
    fn excluded_strings(&self) -> Vec<String> {
        Vec::new()
    }
    fn remember_actual_vehicle(&self) -> Result<(), Failure> {
        self.operation("remember vehicle")
    }
    fn clear_remembered_vehicle(&self) -> Result<(), Failure> {
        self.operation("clear vehicle")
    }
    fn game_value(&self) -> Result<LegacyValue, Failure> {
        self.operation("game")?;
        Ok(self.game.into())
    }
    fn location_value(&self) -> LegacyValue {
        let count = self.location_reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.operation(if count == 0 { "location guard" } else { "location assignment" }).unwrap();
        if self.game == "zero" {
            LegacyValue::Int(0)
        } else if count == 0 {
            "Track before".into()
        } else {
            "Track after".into()
        }
    }
}

#[test]
fn remembered_location_service_read_allows_native_timer() {
    use std::time::Duration;
    let inner = FakeBackend::new();
    let mut app = loaded(inner.clone());
    let (entered, started) = std::sync::mpsc::channel();
    let (release, resume) = std::sync::mpsc::channel();
    app.backend = Arc::new(LocationReadBackend {
        inner,
        operations: Mutex::new(Vec::new()),
        wait: Mutex::new(Some((entered, resume))),
        fail: None,
        location_reads: std::sync::atomic::AtomicUsize::new(0),
        game: "none",
    });
    let label = app.gui.create("p", "orc-loading-message");
    app.gui.set_text(label, "waiting");
    let shared = Arc::new(Mutex::new(app));
    crate::host::lock(&shared).attach_shared(Arc::downgrade(&shared));
    let work = shared.clone();
    let (finished, done) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        finished.send(super::mainloop::remember_location_shared(&work)).unwrap();
    });
    started.recv_timeout(Duration::from_secs(2)).unwrap();
    let (timer_finished, timer_done) = std::sync::mpsc::channel();
    let available = if let Ok(mut app) = shared.try_lock() {
        app.timer(Duration::ZERO, move |app| {
            app.gui.set_text(label, "during history read");
            timer_finished.send(()).unwrap();
        });
        true
    } else {
        false
    };
    let timer_ran = available && timer_done.recv_timeout(Duration::from_secs(2)).is_ok();
    release.send(()).unwrap();
    done.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
    worker.join().unwrap();
    assert!(available, "history service read retained application state");
    assert!(timer_ran, "history read blocked independent Timer");
    assert_eq!(crate::host::lock(&shared).gui.get_text(label), "during history read");
}

#[test]
fn remembered_location_shared_preserves_repeated_reads_and_error_prefixes() {
    for (game, fail) in [
        ("ac", None),
        ("zero", None),
        ("none", None),
        ("ac", Some("remember vehicle")),
        ("ac", Some("game")),
        ("none", Some("clear vehicle")),
    ] {
        let run = |shared_mode| {
            let inner = FakeBackend::new();
            let mut app = loaded(inner.clone());
            app.ml.previous_location = Some("Old".into());
            let backend = Arc::new(LocationReadBackend {
                inner,
                operations: Mutex::new(Vec::new()),
                wait: Mutex::new(None),
                fail,
                location_reads: std::sync::atomic::AtomicUsize::new(0),
                game,
            });
            app.backend = backend.clone();
            let shared = Mutex::new(app);
            let result = if shared_mode {
                super::mainloop::remember_location_shared(&shared)
            } else {
                crate::host::lock(&shared).remember_location()
            };
            let previous = crate::host::lock(&shared).ml.previous_location.clone();
            let operations = backend.operations.lock().unwrap().clone();
            (result, previous, operations)
        };
        let shared = run(true);
        if fail.is_none() && game != "zero" {
            assert_eq!(shared.2[..4], ["remember vehicle", "location guard", "location assignment", "game"]);
        }
        assert_eq!(shared, run(false), "{game} / {fail:?}");
    }
}

struct ProfileSidebarReads {
    inner: Arc<FakeBackend>,
    operations: Mutex<Vec<String>>,
    fail: Option<&'static str>,
    wait: Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
}
impl ProfileSidebarReads {
    fn record(&self, name: &str) -> Result<(), Failure> {
        self.operations.lock().unwrap().push(name.into());
        if let Some((entered, release)) = self.wait.lock().unwrap().take() {
            entered.send(()).unwrap();
            release.recv().unwrap();
        }
        if self.fail == Some(name) {
            return Err(Failure { status: 500, detail: format!("sidebar failed: {name}") });
        }
        Ok(())
    }
}
impl Backend for ProfileSidebarReads {
    fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value, Failure> {
        self.inner.call(method, path, body)
    }
    fn upload(&self, path: &str, bytes: Vec<u8>) -> Result<Value, Failure> {
        self.inner.upload(path, bytes)
    }
    fn fetch(&self, path: &str) -> Result<(Vec<u8>, String), Failure> {
        self.inner.fetch(path)
    }
    fn translate(&self, text: &str) -> String {
        text.into()
    }
    fn excluded_strings(&self) -> Vec<String> {
        Vec::new()
    }
    fn game_value(&self) -> Result<LegacyValue, Failure> {
        self.record("game")?;
        self.inner.game_value()
    }
    fn game_name_value(&self, game: &LegacyValue) -> Result<LegacyValue, Failure> {
        self.record("game name")?;
        self.inner.game_name_value(game)
    }
    fn session_value(&self, key: &str) -> Result<LegacyValue, Failure> {
        self.record(key)?;
        self.inner.session_value(key)
    }
    fn control_value(&self, path: &[&str]) -> Result<LegacyValue, Failure> {
        self.record(&path.join("."))?;
        self.inner.control_value(path)
    }
    fn previous_vehicle_value(&self) -> Result<LegacyValue, Failure> {
        self.record("previous vehicle")?;
        self.inner.previous_vehicle_value()
    }
}
struct RuleReads {
    inner: Arc<FakeBackend>,
    operations: Mutex<Vec<String>>,
    wait_on: &'static str,
    wait: Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
}
impl RuleReads {
    fn record(&self, name: &str) {
        self.operations.lock().unwrap().push(name.into());
        if name == self.wait_on {
            if let Some((tx, rx)) = self.wait.lock().unwrap().take() {
                tx.send(()).unwrap();
                rx.recv().unwrap();
            }
        }
    }
}
impl Backend for RuleReads {
    fn call(&self, m: Method, p: &str, b: Option<Value>) -> Result<Value, Failure> {
        self.record(&format!("{m:?} {p}"));
        self.inner.call(m, p, b)
    }
    fn upload(&self, p: &str, b: Vec<u8>) -> Result<Value, Failure> {
        self.inner.upload(p, b)
    }
    fn fetch(&self, p: &str) -> Result<(Vec<u8>, String), Failure> {
        self.inner.fetch(p)
    }
    fn translate(&self, t: &str) -> String {
        t.into()
    }
    fn excluded_strings(&self) -> Vec<String> {
        Vec::new()
    }
    fn native_setting(&self, s: &str, k: &str) -> Result<LegacyValue, Failure> {
        self.record(&format!("{s}.{k}"));
        self.inner.native_setting(s, k)
    }
    fn motion_parameter(&self, k: &str) -> Result<LegacyValue, Failure> {
        self.record(&format!("motion.{k}"));
        self.inner.motion_parameter(k)
    }
    fn game_value(&self) -> Result<LegacyValue, Failure> {
        self.record("game");
        self.inner.game_value()
    }
    fn control_value(&self, p: &[&str]) -> Result<LegacyValue, Failure> {
        self.record(&p.join("."));
        self.inner.control_value(p)
    }
    fn accessory_status(&self, p: &str) -> Result<LegacyValue, Failure> {
        self.record(&format!("accessory.{p}"));
        self.inner.accessory_status(p)
    }
}
#[test]
fn shared_live_actions_keep_callback_enqueued_actions_and_stop_order() {
    for case in 0..5 {
        let run = |shared_mode| {
            let backend = FakeBackend::new();
            let mut app = loaded(backend.clone());
            app.set_tune_native_checked(&["motion_effects", "pitch"], &LegacyValue::Int(10)).unwrap();
            {
                let mut docs = backend.docs.lock().unwrap();
                docs["status"]["session"]["game"] = json!("api");
                match case {
                    0 => docs["ui_actions"] = json!(["motion_box_pitch_up"]),
                    1 => {
                        docs["ui_actions"] = json!(["motion_box_pitch_up"]);
                        docs["enqueue_ui_actions_after_profile_write"] = json!(["motion_box_pitch_down"]);
                    }
                    2 => {
                        docs["ui_actions"] = json!(["shake_box_engine_up"]);
                        docs["enqueue_ui_actions_after_profile_write"] = json!(["motion_box_pitch_up"]);
                    }
                    3 => {
                        docs["ui_actions"] = json!(["shake_box_engine_up", "motion_box_pitch_up"]);
                        docs["game_after_profile_write"] = json!("none");
                    }
                    4 => {
                        docs["ui_actions"] = json!(["shake_box_engine_up", "belt_heave_up"]);
                        docs["tune"]["shaker_effects"] = Value::Null;
                    }
                    _ => unreachable!(),
                }
            }
            backend.calls.lock().unwrap().clear();
            let shared = Mutex::new(app);
            let result = if shared_mode {
                super::mainloop::apply_ui_actions_shared(&shared)
            } else {
                super::mainloop::apply_ui_actions(&mut crate::host::lock(&shared))
            };
            let app = crate::host::lock(&shared);
            let values = (stored_pitch(&app), backend.docs.lock().unwrap()["tune"]["belt_heave_factor"].clone());
            let calls = backend.calls.lock().unwrap().clone();
            let queue = backend.ui_actions();
            (result, values, calls, queue, app.callback_error.clone())
        };
        assert_eq!(run(true), run(false), "actions case {case}");
    }
}

#[test]
fn startup_import_upload_releases_app_after_pending_offer_assignment() {
    use std::time::Duration;
    struct WaitImport {
        inner: Arc<FakeBackend>,
        wait: Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
    }
    impl Backend for WaitImport {
        fn call(&self, m: Method, p: &str, b: Option<Value>) -> Result<Value, Failure> {
            self.inner.call(m, p, b)
        }
        fn upload(&self, p: &str, b: Vec<u8>) -> Result<Value, Failure> {
            if p == "profiles/import-check" {
                if let Some((tx, rx)) = self.wait.lock().unwrap().take() {
                    tx.send(()).unwrap();
                    rx.recv().unwrap();
                }
            }
            self.inner.upload(p, b)
        }
        fn fetch(&self, p: &str) -> Result<(Vec<u8>, String), Failure> {
            self.inner.fetch(p)
        }
        fn translate(&self, t: &str) -> String {
            t.into()
        }
        fn excluded_strings(&self) -> Vec<String> {
            Vec::new()
        }
    }
    let inner = FakeBackend::new();
    let mut app = loaded(inner.clone());
    let catalog = openrigcue_config::catalog::Catalog::builtin();
    let tune: LegacyValue = inner.docs.lock().unwrap()["tune"].clone().into();
    let (_, bytes) = openrigcue_config::profiles::export_native_profile_from_lookup(
        |_| Ok(Some(tune.clone())),
        catalog,
        "ac",
        &LegacyValue::Int(0),
        None,
        &openrigcue_config::profiles::ExportMetadata {
            motion_model: "None",
            shaker_layout: "None",
            builtin_layouts: &[],
        },
    )
    .unwrap();
    let path = std::env::temp_dir().join(format!("openrigcue-startup-import-{}.srs", std::process::id()));
    std::fs::write(&path, bytes).unwrap();
    app.docs.status["arguments"] = json!({"profile_path":path});
    let (tx, started) = std::sync::mpsc::channel();
    let (release, rx) = std::sync::mpsc::channel();
    app.backend = Arc::new(WaitImport { inner, wait: Mutex::new(Some((tx, rx))) });
    let shared = Arc::new(Mutex::new(app));
    let work = shared.clone();
    let worker = std::thread::spawn(move || crate::ui::profiles::offer_launcher_import_shared(&work, "new_instance"));
    started.recv_timeout(Duration::from_secs(2)).unwrap();
    let available = if let Ok(app) = shared.try_lock() {
        assert_eq!(app.ml_import_path.as_deref(), path.to_str());
        true
    } else {
        false
    };
    release.send(()).unwrap();
    worker.join().unwrap().unwrap();
    std::fs::remove_file(path).unwrap();
    assert!(available, "startup import upload retained app");
}

/// The first-pass motion model block of the main loop absorbs errors as one and then selects
/// 'None'. With a translated language the first model entry shows 'Aucune'; the model taken from it is its
/// value 'None', never the shown text, and the main loop continues with 'None' selected.
#[test]
fn a_missing_motion_model_is_stored_by_its_name_not_its_translation() {
    for shared_mode in [false, true] {
        let backend = FakeBackend::new();
        backend.docs.lock().unwrap()["translations"] = json!({"None": "Aucune"});
        let mut app = MyApp::new(backend.clone());
        let body = app.gui.create("body", "BODY");
        let root = app.main(body);
        app.gui.set_root(root);
        dispatch(&mut app, body, "onload", &Default::default());
        if shared_mode {
            let shared = Mutex::new(app);
            assert_eq!(crate::ui::screens::hardware::main_loop_start_shared(&shared), Ok(()));
            app = shared.into_inner().unwrap();
        } else {
            app.main_loop_pass();
            assert_eq!(app.ml.fatal_error, None);
        }
        assert_eq!(backend.docs.lock().unwrap()["settings"]["motion"]["model"], json!("None"));
        let calls = format!("{:?}", backend.calls.lock().unwrap());
        assert!(!calls.contains("Aucune"), "the shown text is never sent to the service: {calls}");
        app.threads.clear();
    }
}

/// The entries of one of the service's locale files (`locales/<code>.json` of the daemon crate, which builds
/// them into the program).
fn locale_entries(code: &str) -> serde_json::Map<String, Value> {
    let path = format!("{}/../openrigcue-daemon/assets/locales/{code}.json", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"))).unwrap()
}

/// Whether `text` is `template` with its `^name^` parts filled in: the template's fixed parts in order, the
/// first at the start and the last at the end.
fn is_filled(template: &str, text: &str) -> bool {
    let parts: Vec<&str> = template.split('^').step_by(2).collect();
    if parts.len() < 2 || !text.starts_with(parts[0]) || !text.ends_with(parts[parts.len() - 1]) {
        return false;
    }
    let mut rest = text;
    parts.iter().all(|part| match rest.find(part) {
        Some(at) => {
            rest = &rest[at + part.len()..];
            true
        }
        None => false,
    })
}

/// The looked-up texts that would be shown in English only: those without an entry in `en.json` (every other
/// locale file is checked against `en.json`'s keys). Not counted: names (products, devices, games, motion
/// models named after a product, and `names`, the fixture's own data), numbers and units, and a text made by
/// filling the `^name^` parts of a sentence that was looked up itself. The page tests use this for the windows
/// they open.
pub(crate) fn texts_without_entry(looked_up: &std::collections::BTreeSet<String>, names: &[&str]) -> Vec<String> {
    let mut known: std::collections::BTreeSet<String> = [
        "Fanalab",
        "Simucube 2 True Drive Paddock",
        "CrewChief",
        "DOF REALITY",
        "PROSIMU",
        "PT-ACTUATOR",
        "THANOS",
        "VNM SIMULATION",
        "Yaw VR",
        "SIMTECHREVOLUTION",
        "OpenRigCue",
        "MagicBox",
        "IntelliComp",
    ]
    .map(str::to_owned)
    .into();
    known.extend(names.iter().map(|name| (*name).to_owned()));
    let catalog = openrigcue_config::catalog::Catalog::builtin();
    known.extend(catalog.games().iter().map(|game| game.name.clone()));
    known.extend(openrigcue_config::patch::GAME_ENTRIES.iter().map(|entry| entry.name.to_owned()));
    // A motion model named after a product is a name; one that describes its actuators is a text.
    known.extend(crate::catalog::models().iter().map(|m| m.name.clone()).filter(|name| !name.contains(" Actuators ")));
    let templates: Vec<&String> = looked_up.iter().filter(|text| text.matches('^').count() >= 2).collect();
    let english = locale_entries("en");
    for code in ["de", "es", "fr", "it", "ja", "pt", "zh"] {
        let own = locale_entries(code);
        let missing: Vec<&String> = english.keys().filter(|key| !own.contains_key(*key)).collect();
        assert_eq!(missing, Vec::<&String>::new(), "{code}.json");
    }
    let mut missing = Vec::new();
    for text in looked_up {
        // A shown text is looked up once more as it is written into the page (`>` as `&gt;`), and an indented
        // text also without its indentation.
        let text = &text.replace("&gt;", ">").replace("&lt;", "<").replace("&amp;", "&");
        let plain = text.trim_start_matches(' ');
        if !text.chars().any(char::is_alphabetic)
            || super::excluded_pattern(text)
            || known.contains(text)
            || english.contains_key(text)
            || english.contains_key(plain)
            || templates.iter().any(|template| is_filled(template, text))
        {
            continue;
        }
        missing.push(text.clone());
    }
    missing
}

/// Every text the pages look up has an entry in all eight locale files, so none is shown in English only: the
/// labels, buttons, table headers, window titles, tooltips, dropdown entries and placeholders of every page as
/// it is built at start, the help window, and the notifications whose texts hold values. Not looked for: names
/// (products, devices, games, motion models, sound devices, the fixture's own data), numbers and units, and a
/// text made by filling the `^name^` parts of a sentence that was looked up itself.
#[test]
fn every_looked_up_text_has_an_entry_in_every_language() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["settings"]["app"]["enable_hapticr"] = json!(true);
    let mut app = loaded(backend.clone());
    render_every_screen(&mut app);
    backend.docs.lock().unwrap()["notifications"] = json!([
        {"id": 1, "game": "dirt4", "kind": "game_not_configured", "title": "This game is not set up",
         "message": "Leave the game, then set it up on the OpenRigCue Games page.", "colour": "red"},
        {"id": 2, "game": "forzahorizon5", "kind": "game_instructions", "title": "Forza Horizon 5 telemetry",
         "message": "Open the Forza Horizon 5 setup instructions to enable telemetry.", "colour": "red"},
        {"id": 3, "game": "forzahorizon5", "kind": "port_in_use", "process": "SimHub.exe", "port": 10001,
         "title": "Game Telemetry Not Detected", "message": "unused", "colour": "red"},
        {"id": 4, "game": "", "kind": "device_permissions", "devices": ["/dev/ttyUSB0"],
         "title": "Controller access required", "message": "unused", "colour": "red"},
        {"id": 5, "game": "", "kind": "surround_required", "profile": "6 Speakers - 5.1",
         "label": "6-channel (5.1 Surround)", "suggest_stereo": true, "title": "unused", "message": "unused",
         "colour": "red"},
        {"id": 6, "game": "", "kind": "surround_required", "profile": "4 Speakers - FL,FR,RL,RR",
         "label": "4-channel (Quadraphonic)", "suggest_stereo": false, "title": "unused", "message": "unused",
         "colour": "red"},
        {"id": 7, "game": "", "kind": "default_sound_card", "title": "Shakers - Default Sound Device",
         "message": "The sound device selected for the shakers is also the system default. Select a separate output for game audio so it does not play through the shakers.",
         "colour": "red"},
    ]);
    crate::ui::notices::synchronize(&mut app);
    let shown = |app: &MyApp, id: u64| {
        let note = app.ui.notices.get(app.ui.notices.service[&id]).unwrap();
        (note.title.clone(), note.message.clone())
    };
    assert_eq!(
        shown(&app, 2),
        ("Forza Horizon 5 telemetry".into(), "Open the Forza Horizon 5 setup instructions to enable telemetry.".into())
    );
    assert_eq!(shown(&app, 3).1, "No game telemetry reaches OpenRigCue because SimHub.exe occupies port 10001. Close SimHub.exe so that OpenRigCue can receive the telemetry");
    assert_eq!(shown(&app, 4).1, "Allow OpenRigCue to access /dev/ttyUSB0.");
    assert_eq!(shown(&app, 5).0, "Shakers - 6-channel (5.1 Surround) Required");
    assert_eq!(shown(&app, 5).1, "The \"6 Speakers - 5.1\" profile needs your sound device set to 6-channel (5.1 Surround) in the system sound settings. Click \"Fix automatically\" to have that done for you or switch to a 2-channel profile such as \"2 Speakers - Front Left / Right\".");
    assert!(shown(&app, 6).1.ends_with("Click \"Fix automatically\" to have that done for you."));

    use std::collections::BTreeSet;
    let mut looked_up: BTreeSet<String> = backend.translations.lock().unwrap().iter().cloned().collect();
    assert!(looked_up.len() > 400, "the screens were drawn: {} texts", looked_up.len());
    // The screens as other motion platforms draw them: their own rows, actuator names and choices.
    for model in [
        "6 Actuators (2 Front + 2 Rear + TL + Surge) CANBus",
        "8 Actuators [2 Front + 2 Rear + 2 TL + Surge + Belt]",
        "Yaw2 PRO",
    ] {
        let other = FakeBackend::new();
        other.docs.lock().unwrap()["settings"]["motion"]["model"] = json!(model);
        let mut other_app = loaded(other.clone());
        assert_eq!(other_app.ml.fatal_error, None, "{model}");
        render_every_screen(&mut other_app);
        other_app.threads.clear();
        looked_up.extend(other.translations.lock().unwrap().iter().cloned());
    }
    assert!(looked_up.contains("Action Mode"), "the models' own rows were built");
    let missing = texts_without_entry(&looked_up, &["USB Sound Device", "SimHub.exe"]);
    assert_eq!(missing, Vec::<String>::new(), "texts without an entry in en.json");
    // The texts with values were looked up as one sentence each.
    for template in [
        "^game^ telemetry",
        "Open the ^game^ setup instructions to enable telemetry.",
        "Allow OpenRigCue to access ^devices^.",
        "Shakers - ^label^ Required",
        "OpenRigCue ^version^ — motion cueing and sim effects",
    ] {
        assert!(looked_up.contains(template), "{template}");
    }
    assert!(is_filled("Port ^port^ of ^process^.", "Port 80 of x."));
    assert!(!is_filled("Port ^port^ of ^process^.", "Port 80"));
    assert!(!is_filled("Plain text", "Plain text"));
    app.threads.clear();
}

/// Draws every screen at every settings group, as the interface does when the user opens them.
fn render_every_screen(app: &mut MyApp) {
    for screen in crate::ui::Screen::ALL {
        crate::ui::navigate(app, screen, 0);
        for section in 0..12 {
            app.ui.section = section;
            crate::ui::screens::main(app);
        }
    }
    crate::ui::navigate(app, crate::ui::Screen::Home, 0);
}

/// In another language a text with values is the translated sentence with the values in the places the
/// translation gives them; the Save window's entries keep the values the confirmation reads; and an indented
/// label is the translated text behind its indentation.
#[test]
fn texts_with_values_are_translated_as_whole_sentences() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["translations"] = json!({
        "Allow OpenRigCue to access ^devices^.": "^devices^ für OpenRigCue freigeben.",
        "No game telemetry reaches OpenRigCue because ^process^ occupies port ^port^. Close ^process^ so that OpenRigCue can receive the telemetry":
            "Anschluss ^port^ ist von ^process^ belegt; ^process^ bitte schließen",
        "^game^ telemetry": "Telemetrie von ^game^",
        "Shakers - ^label^ Required": "Shaker - ^label^ erforderlich",
        "6-channel (5.1 Surround)": "6-Kanal",
        "Vehicle: ^vehicle^": "Fahrzeug: ^vehicle^",
        "Game: ^game^": "Spiel: ^game^",
        "Engine": "Motor",
    });
    let mut app = loaded(backend.clone());
    backend.docs.lock().unwrap()["notifications"] = json!([
        {"id": 2, "game": "forzahorizon5", "kind": "game_instructions", "title": "t", "message": "m", "colour": "red"},
        {"id": 3, "game": "forzahorizon5", "kind": "port_in_use", "process": "SimHub.exe", "port": 10001,
         "title": "Game Telemetry Not Detected", "message": "m", "colour": "red"},
        {"id": 4, "game": "", "kind": "device_permissions", "devices": ["/dev/ttyUSB0", "/dev/ttyACM1"],
         "title": "Controller access required", "message": "m", "colour": "red"},
        {"id": 5, "game": "", "kind": "surround_required", "profile": "6 Speakers - 5.1",
         "label": "6-channel (5.1 Surround)", "suggest_stereo": true, "title": "t", "message": "m", "colour": "red"},
    ]);
    crate::ui::notices::synchronize(&mut app);
    let shown = |app: &MyApp, id: u64| {
        let note = app.ui.notices.get(app.ui.notices.service[&id]).unwrap();
        (note.title.clone(), note.message.clone())
    };
    assert_eq!(shown(&app, 2).0, "Telemetrie von Forza Horizon 5");
    assert_eq!(shown(&app, 3).1, "Anschluss 10001 ist von SimHub.exe belegt; SimHub.exe bitte schließen");
    assert_eq!(shown(&app, 4).1, "/dev/ttyUSB0, /dev/ttyACM1 für OpenRigCue freigeben.");
    assert_eq!(shown(&app, 5).0, "Shaker - 6-Kanal erforderlich");

    // The Save dialog's choices are whole translated sentences; the choice it confirms is kept by position.
    app.ui.session.vehicle = "Displayed car".into();
    backend.docs.lock().unwrap()["status"]["session"]["game"] = json!("ac");
    crate::ui::dispatch(&mut app, "save", "", "").unwrap();
    let dialog = crate::ui::dialogs::render(&app);
    assert!(dialog.contains(">Fahrzeug: Displayed car</option>") && dialog.contains(">Spiel: ac</option>"), "{dialog}");
    backend.calls.lock().unwrap().clear();
    crate::ui::dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert!(backend.calls.lock().unwrap().iter().any(|(m, p, b)| *m == Method::Post
        && p == "profile/save"
        && b.as_ref() == Some(&json!({"level": "vehicle", "vehicle": "Displayed car"}))));

    assert_eq!(app.gui.translator.translate("  Engine"), "  Motor");
    assert_eq!(app.gui.translator.translate("Engine"), "Motor");
    assert_eq!(app.gui.translator.translate("  No entry"), "  No entry");
}

// ---- The start with values the old pages refused ----------------------------------------------------------

/// The page load and one main-loop pass, with the threads it queued dropped.
fn started(backend: Arc<FakeBackend>) -> MyApp {
    let mut app = loaded(backend);
    app.threads.clear();
    app
}

/// The interface's own start: the page built by `ui::mount` from the documents, then the start rules of the
/// main loop that belong to the screens (Games and Hardware, the startup page) and one tick.
fn mounted(backend: Arc<FakeBackend>) -> Result<MyApp, String> {
    let mut app = MyApp::new(backend.clone());
    let body = app.gui.create("body", "BODY");
    let root = app.main(body);
    app.gui.set_root(root);
    app.refresh_docs();
    app.docs.games = backend.call(Method::Get, "catalog/games", None).unwrap();
    crate::ui::mount(&mut app, root);
    crate::ui::screens::hardware::main_loop_start(&mut app)?;
    crate::ui::screens::app::open_startup_page(&mut app);
    crate::ui::screens::tuning::on_profile_load(&mut app);
    crate::ui::tick(&mut app);
    assert!(app.ui.mounted && app.gui.child(root, "loading_label").is_none(), "the interface replaced the label");
    Ok(app)
}

/// Every screen draws at its first settings group.
fn every_screen_draws(app: &mut MyApp) {
    for screen in crate::ui::Screen::ALL {
        crate::ui::navigate(app, screen, 0);
        assert!(!crate::ui::screens::main(app).is_empty(), "{screen:?}");
    }
    crate::ui::navigate(app, crate::ui::Screen::Home, 0);
    app.threads.clear();
}

#[test]
fn an_invalid_smoothing_step_no_longer_stops_the_start() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["settings"]["features"]["smoothing_slider_step"] = json!("1.5");
    let mut app = mounted(backend).unwrap();
    // The Motion screen takes the step as 1.
    assert_eq!(crate::ui::screens::motion::smoothing_step(&app), 1.0);
    crate::ui::navigate(&mut app, crate::ui::Screen::Motion, 0);
    assert!(crate::ui::screens::main(&app).contains("data-arg=\"Motion:"));
    every_screen_draws(&mut app);
}

#[test]
fn a_missing_startup_page_keeps_the_start_screen_and_the_loop() {
    let backend = FakeBackend::new();
    let mut native = openrigcue_config::settings::Settings::default().native_snapshot();
    native.get_mut("APP").unwrap().as_dict_mut().unwrap().remove("startup_page");
    backend.docs.lock().unwrap()["native_settings"] = serde_json::to_value(LegacyValue::Dict(native)).unwrap();
    backend.docs.lock().unwrap()["settings"]["app"].as_object_mut().unwrap().remove("start_page");
    let mut app = mounted(backend).unwrap();
    assert_eq!(app.ui.screen, crate::ui::Screen::Home);
    every_screen_draws(&mut app);
}

#[test]
fn an_invalid_app_switch_no_longer_stops_the_start() {
    let backend = FakeBackend::new();
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["settings"]["app"]["gear_shift_sound"] = json!("invalid boolean");
        docs["settings"]["compensation"]["height"] = json!("invalid integer");
    }
    let mut app = mounted(backend.clone()).unwrap();
    // Nothing is written back at the start; the screens draw the stored values when they open.
    assert_eq!(backend.docs.lock().unwrap()["settings"]["app"]["gear_shift_sound"], json!("invalid boolean"));
    assert!(!backend.patches("settings").iter().any(|patch| patch.get("app").is_some()));
    every_screen_draws(&mut app);
}

#[test]
fn invalid_motion_settings_no_longer_stop_the_start() {
    // A refresh rate that is no whole number is left as it is; one written in other digits is stored as the number.
    for (value, stored) in [
        (json!("100.0"), json!("100.0")),
        (Value::Null, Value::Null),
        (json!([100]), json!([100])),
        (json!("١_٠٠"), json!(100)),
    ] {
        let backend = FakeBackend::new();
        backend.docs.lock().unwrap()["settings"]["motion"]["refresh_rate"] = value.clone();
        let mut app = mounted(backend.clone()).unwrap();
        assert_eq!(backend.docs.lock().unwrap()["settings"]["motion"]["refresh_rate"], stored, "{value}");
        every_screen_draws(&mut app);
    }
}

#[test]
fn bad_led_colours_no_longer_stop_the_start() {
    for color in [json!([1, 2]), json!([1, "bad", 3]), Value::Null] {
        let backend = FakeBackend::new();
        backend.docs.lock().unwrap()["settings"]["leds"]["segments"][2]["color"] = color.clone();
        let mut app = mounted(backend).unwrap();
        let shown = crate::ui::screens::led::color_of(&app, 3);
        assert!(
            shown.len() == 7 && shown.starts_with('#') && shown[1..].chars().all(|c| c.is_ascii_hexdigit()),
            "{color} shows as {shown}"
        );
        every_screen_draws(&mut app);
    }
}

/// At the start the LED segments are kept within the LED count; a segment whose start is not a number stops the
/// start there: the earlier segments are kept as they are, the count is not written and the main loop ends.
#[test]
fn a_led_segment_without_a_start_stops_the_start_after_the_earlier_segments() {
    let backend = FakeBackend::new();
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["settings"]["leds"]["nbr_leds"] = json!(300);
        docs["settings"]["leds"]["segments"][0]["start"] = json!(2);
        docs["settings"]["leds"]["segments"][0]["end"] = json!(7);
        docs["settings"]["leds"]["segments"][1]["start"] = Value::Null;
    }
    let error = mounted(backend.clone()).err().unwrap();
    assert!(error.contains("int("), "{error}");
    let docs = backend.docs.lock().unwrap().clone();
    assert_eq!(
        (&docs["settings"]["leds"]["segments"][0]["start"], &docs["settings"]["leds"]["segments"][0]["end"]),
        (&json!(2), &json!(7))
    );
    assert_eq!(docs["settings"]["leds"]["segments"][1]["start"], Value::Null);
    assert!(!backend.patches("settings").iter().any(|patch| !patch["leds"]["nbr_leds"].is_null()));
    // In the main loop the failure ends the loop before the first profile load.
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["settings"]["leds"]["segments"][1]["start"] = Value::Null;
    let app = started(backend);
    assert!(app.ml.fatal_error.as_deref().is_some_and(|error| error.contains("int(")), "{:?}", app.ml.fatal_error);
    assert_eq!(app.ml.loaded_generation, None);
}

// ---- The session and the hardware status in the shared main loop ----------------------------------------

#[test]
fn the_session_reads_leave_the_app_free_for_timers() {
    use std::time::Duration;
    let inner = FakeBackend::new();
    let mut app = loaded(inner.clone());
    app.threads.clear();
    let (entered, started) = std::sync::mpsc::channel();
    let (release, resume) = std::sync::mpsc::channel();
    app.backend = Arc::new(ProfileSidebarReads {
        inner,
        operations: Mutex::new(Vec::new()),
        fail: None,
        wait: Mutex::new(Some((entered, resume))),
    });
    let shared = Arc::new(Mutex::new(app));
    crate::host::lock(&shared).attach_shared(Arc::downgrade(&shared));
    let work = shared.clone();
    let worker = std::thread::spawn(move || crate::ui::tick_shared(&work));
    started.recv_timeout(Duration::from_secs(2)).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let available = if let Ok(mut app) = shared.try_lock() {
        app.timer(Duration::ZERO, move |_| tx.send(()).unwrap());
        true
    } else {
        false
    };
    let timer_ran = available && rx.recv_timeout(Duration::from_secs(2)).is_ok();
    release.send(()).unwrap();
    worker.join().unwrap();
    assert!(available, "the session read retained MyApp");
    assert!(timer_ran, "the session read blocked a timer");
}

#[test]
fn the_session_is_the_same_from_the_shared_main_loop() {
    for case in 0..8 {
        let run = |shared_mode| {
            let inner = FakeBackend::new();
            let mut app = loaded(inner.clone());
            app.threads.clear();
            {
                let mut docs = inner.docs.lock().unwrap();
                docs["status"]["session"]["game"] = json!(if case == 0 { "none" } else { "ac" });
                docs["status"]["session"]["listener"] = json!(if case < 3 { "none" } else { "ac" });
                docs["status"]["session"]["vehicle"] = json!(if case == 3 { "0" } else { "Current car" });
                docs["status"]["controls"]["profile_edit"] = json!(case == 2);
                docs["status"]["displayed_vehicle"] = json!("Previous car");
            }
            let backend = Arc::new(ProfileSidebarReads {
                inner,
                operations: Mutex::new(Vec::new()),
                wait: Mutex::new(None),
                fail: match case {
                    5 => Some("game name"),
                    6 => Some("vehicle_name"),
                    7 => Some("profile_edit"),
                    _ => None,
                },
            });
            app.backend = backend.clone();
            let state = Mutex::new(app);
            if shared_mode {
                crate::ui::tick_shared(&state);
            } else {
                crate::ui::tick(&mut crate::host::lock(&state));
            }
            let app = crate::host::lock(&state);
            let operations = backend.operations.lock().unwrap().clone();
            (app.ui.session.clone(), crate::ui::shell::profile_label(&app), operations)
        };
        assert_eq!(run(true), run(false), "session case {case}");
    }
}

#[test]
fn the_hardware_status_refresh_records_the_current_status_and_needs_the_model() {
    let fake = FakeBackend::new();
    let mut app = loaded(fake.clone());
    fake.docs.lock().unwrap()["status"]["accessories"]["hud"] = json!("Connected");
    super::mainloop::refresh_hardware_status(&mut app).unwrap();
    assert_eq!(hardware_state(&app, "Hud"), Some(("green".to_owned(), "Connected".to_owned())));
    fake.docs.lock().unwrap()["settings"]["motion"].as_object_mut().unwrap().remove("model");
    assert_eq!(super::mainloop::refresh_hardware_status(&mut app).unwrap_err(), "KeyError: 'model'");
}

#[test]
fn hardware_status_reads_leave_the_app_free_for_timers() {
    use std::time::Duration;
    for refresh in [true, false] {
        let inner = FakeBackend::new();
        let mut app = loaded(inner.clone());
        app.threads.clear();
        let (tx, started) = std::sync::mpsc::channel();
        let (release, rx) = std::sync::mpsc::channel();
        app.backend = Arc::new(RuleReads {
            inner,
            operations: Mutex::new(Vec::new()),
            wait_on: "MOTIONBOX.model",
            wait: Mutex::new(Some((tx, rx))),
        });
        let shared = Arc::new(Mutex::new(app));
        crate::host::lock(&shared).attach_shared(Arc::downgrade(&shared));
        let work = shared.clone();
        let worker = std::thread::spawn(move || {
            if refresh {
                super::mainloop::refresh_hardware_status_shared(&work)
            } else {
                super::mainloop::hardware_status_shared(&work)
            }
        });
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let available = if let Ok(mut app) = shared.try_lock() {
            app.timer(Duration::ZERO, move |_| tx.send(()).unwrap());
            true
        } else {
            false
        };
        let ran = available && rx.recv_timeout(Duration::from_secs(2)).is_ok();
        release.send(()).unwrap();
        worker.join().unwrap().unwrap();
        assert!(available, "the hardware status model read retained MyApp (refresh: {refresh})");
        assert!(ran, "the hardware status read blocked a timer (refresh: {refresh})");
    }
}

#[test]
fn the_shared_hardware_status_matches_the_direct_one() {
    for missing_model in [false, true] {
        let run = |shared_mode| {
            let backend = FakeBackend::new();
            let app = loaded(backend.clone());
            {
                let mut docs = backend.docs.lock().unwrap();
                docs["status"]["accessories"]["hud"] = json!("Connected");
                if missing_model {
                    docs["settings"]["motion"].as_object_mut().unwrap().remove("model");
                }
            }
            backend.calls.lock().unwrap().clear();
            let state = Mutex::new(app);
            let result = if shared_mode {
                super::mainloop::refresh_hardware_status_shared(&state)
            } else {
                super::mainloop::refresh_hardware_status(&mut crate::host::lock(&state))
            };
            let mut app = crate::host::lock(&state);
            app.threads.clear();
            let calls = backend.calls.lock().unwrap().clone();
            (result, calls, app.ml.hardware_states.clone())
        };
        assert_eq!(run(true), run(false), "missing model: {missing_model}");
    }
}

#[test]
fn the_shared_start_matches_the_direct_start() {
    for case in 0..4 {
        let run = |shared_mode| {
            let inner = FakeBackend::new();
            let mut app = loaded(inner.clone());
            app.threads.clear();
            {
                let mut docs = inner.docs.lock().unwrap();
                match case {
                    1 => docs["settings"]["motion"]["model"] = json!("missing model"),
                    2 => docs["status"]["shakers"]["refresh_soundcard"] = json!(true),
                    3 => docs["settings"]["sim3d"]["model"] = json!("missing"),
                    _ => {}
                }
            }
            let backend = Arc::new(RuleReads {
                inner: inner.clone(),
                operations: Mutex::new(Vec::new()),
                wait_on: "",
                wait: Mutex::new(None),
            });
            app.backend = backend.clone();
            let shared = Mutex::new(app);
            let result = if shared_mode {
                crate::ui::screens::hardware::main_loop_start_shared(&shared)
            } else {
                crate::ui::screens::hardware::main_loop_start(&mut crate::host::lock(&shared))
            };
            let mut app = crate::host::lock(&shared);
            app.threads.clear();
            let operations = backend.operations.lock().unwrap().clone();
            let settings = inner.docs.lock().unwrap()["settings"].clone();
            (result, operations, settings)
        };
        assert_eq!(run(true), run(false), "start case {case}");
    }
}

// ---- Timers --------------------------------------------------------------------------------------------------

#[test]
fn a_plain_timer_does_not_wait_for_the_update_lock() {
    use std::time::Duration;
    let shared = Arc::new(Mutex::new(MyApp::new(FakeBackend::new())));
    let gate = {
        let mut app = crate::host::lock(&shared);
        app.attach_shared(Arc::downgrade(&shared));
        app.update_lock.clone()
    };
    let (held, ready) = std::sync::mpsc::channel();
    let (release, waiting) = std::sync::mpsc::channel::<()>();
    let owner = std::thread::spawn(move || {
        let _update_guard = gate.lock();
        held.send(()).unwrap();
        waiting.recv_timeout(Duration::from_secs(5)).ok();
    });
    ready.recv().unwrap();
    let (fired, done) = std::sync::mpsc::channel();
    crate::host::lock(&shared).timer(Duration::ZERO, move |_| fired.send(()).unwrap());
    let ran = done.recv_timeout(Duration::from_secs(2)).is_ok();
    release.send(()).unwrap();
    owner.join().unwrap();
    assert!(ran, "a timer without the update lock runs while the interface is being updated");
}

#[test]
fn a_timer_keeps_its_application_until_it_has_run() {
    use std::time::{Duration, Instant};
    let shared = Arc::new(Mutex::new(MyApp::new(FakeBackend::new())));
    let (fired, done) = std::sync::mpsc::channel();
    {
        let mut app = crate::host::lock(&shared);
        app.attach_shared(Arc::downgrade(&shared));
        app.timer(Duration::from_millis(200), move |app| fired.send(app.ui.screen).unwrap());
    }
    let weak = Arc::downgrade(&shared);
    drop(shared);
    assert!(weak.upgrade().is_some(), "the waiting timer keeps the application");
    assert_eq!(done.recv_timeout(Duration::from_secs(2)).unwrap(), crate::ui::Screen::Home);
    let deadline = Instant::now() + Duration::from_secs(2);
    while weak.upgrade().is_some() && Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert!(weak.upgrade().is_none(), "the finished timer lets the application go");
}

// ---- Attribute calls -----------------------------------------------------------------------------------------

#[test]
fn attribute_call_arguments_bind_like_plain_methods() {
    assert_eq!(
        crate::ui::profiles::import_argument(&[("value".into(), "x".into())]).unwrap_err(),
        "TypeError: RemoteImport.api_set_file_path() got an unexpected keyword argument 'value'. Did you mean 'value1'?"
    );
    assert_eq!(
        crate::ui::profiles::import_argument(&[("self".into(), "x".into())]).unwrap_err(),
        "TypeError: RemoteImport.api_set_file_path() got multiple values for argument 'self'"
    );
    assert_eq!(crate::ui::profiles::import_argument(&[("value1".into(), "new_instance".into())]), Ok("new_instance"));
    assert_eq!(
        crate::ui::profiles::download_arguments("ExportAll", &[("sel".into(), "x".into())]).unwrap_err(),
        "TypeError: ExportAll.download() got an unexpected keyword argument 'sel'. Did you mean 'self'?"
    );
    // A download's file is served once its arguments bind.
    let mut app = started(FakeBackend::new());
    app.downloads.insert("123".into(), ("game.srs".into(), b"zip".to_vec()));
    assert!(app.attribute_call_checked("123", "download", &[("sel".into(), "x".into())]).is_err());
    assert_eq!(app.attribute_call_checked("123", "download", &[]).unwrap().unwrap().content, b"zip");
}
