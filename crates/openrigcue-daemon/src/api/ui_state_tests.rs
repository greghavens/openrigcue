//! The routes of [`super::ui_state`] through the router, without threads or devices.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use openrigcue_config::settings::paths::ConfigDir;
use openrigcue_config::settings::Configuration;
use openrigcue_devices::motion::LinkStatus;
use openrigcue_effects::accessories::belt::Homing;
use openrigcue_effects::shakers::SampleBank;
use serde_json::{json, Value};
use tower::ServiceExt;

use super::ui_state::{LogLevelHook, UiServices, DEFAULT_LAYOUT};
use super::{router, ApiContext};
use crate::motion::testing::OfflineHost;
use crate::motion::MotionHandle;
use crate::shakers::{ShakersHandle, SilentBeep};
use crate::state::{OriginalPlatform, ServiceState};
use crate::store::ConfigStore;

struct Harness {
    context: ApiContext,
    tmp: tempfile::TempDir,
    _base: super::tests::Harness,
}

/// A service on a configuration folder in a temporary directory (written once, as after a first start); the
/// other parts come from the API test harness.
fn harness_with(ui: impl FnOnce(&ServiceState) -> UiServices) -> Harness {
    harness_on(true, ui)
}

/// As [`harness_with`]; `saved: false` is a brand-new configuration folder nothing was written to yet.
fn harness_on(saved: bool, ui: impl FnOnce(&ServiceState) -> UiServices) -> Harness {
    let tmp = tempfile::tempdir().unwrap();
    let store = ConfigStore::load(ConfigDir::at(tmp.path().join("config"))).unwrap();
    if saved {
        store.save().unwrap();
    }
    let state = ServiceState::new(store, OriginalPlatform::Windows);
    let base = super::tests::harness();
    let mut context = base.context.clone();
    context.motion = MotionHandle::for_settings(state.config.read().settings(), Box::new(OfflineHost::default()));
    context.shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), false);
    context.ui = Arc::new(ui(&state));
    context.joystick = crate::joystick::JoystickService::new(
        state.clone(),
        Default::default(),
        crate::runtime::Shared::new("Default".into()),
    );
    context.state = state;
    Harness { context, tmp, _base: base }
}

fn harness() -> Harness {
    harness_with(|_| UiServices::off())
}

#[test]
fn desktop_capture_queries_before_config_lock_and_keeps_native_assignments_pending() {
    use openrigcue_model::native_value::{Dict, LegacyValue};
    let h = harness_with(|state| {
        let state = state.clone();
        UiServices::off().with_geometry_hook(Some(Arc::new(move || {
            // The loaded/closed callbacks can read this while Save waits.
            let _settings = state.config.read().settings().native_snapshot();
            let mut app = Dict::new();
            app.insert("window_x_pos", LegacyValue::Int(100));
            app.insert("window_y_pos", LegacyValue::Int(-200));
            let mut monitors = Dict::new();
            monitors.insert("0", LegacyValue::Int(3000));
            Some([("APP".into(), app.into()), ("MONITORS".into(), monitors.into())].into_iter().collect())
        })))
    });
    let path = h.context.state.config.read().dir().unwrap().settings_file();
    let before = std::fs::read(&path).unwrap();
    h.context.ui.capture_window_geometry(&h.context.state);
    let config = h.context.state.config.read();
    let raw = config.settings().native_snapshot();
    assert_eq!(raw.get("APP").unwrap().as_dict().unwrap().get("window_y_pos"), Some(&LegacyValue::Int(-200)));
    assert_eq!(raw.get("MONITORS").unwrap().as_dict().unwrap().get("0"), Some(&LegacyValue::Int(3000)));
    assert_eq!(std::fs::read(path).unwrap(), before, "opening Save must not write the INI yet");
}

#[tokio::test]
async fn raw_joystick_autocommit_retains_null_types_and_order_through_restart() {
    use openrigcue_config::import::{sqlitedict, value::LegacyValue};

    let h = harness();
    let profile = json!({
        "speed":{"joystick":null, "mode":null, "trigger":"", "type":"", "invert_axis":false},
        "right":{"joystick":"Wheel", "mode":"Single Axis", "trigger":0.5, "extra":[1,null]}
    });
    for (name, value) in [("Wheel", profile.clone()), ("None profile", Value::Null), ("Other", json!({}))] {
        assert_eq!(
            call(&h, Method::PUT, "/api/v1/joystick/db", Some(json!({"key":name, "value":value}))).await.0,
            StatusCode::NO_CONTENT
        );
    }
    assert_eq!(
        call(&h, Method::PUT, "/api/v1/joystick/db", Some(json!({"key":"Wheel", "value":profile}))).await.0,
        StatusCode::NO_CONTENT
    );
    let (_, view) = call(&h, Method::GET, "/api/v1/joystick/db", None).await;
    assert_eq!(view["Wheel"], profile);
    assert_eq!(
        view.as_object().unwrap().keys().next_back().unwrap(),
        "Wheel",
        "equal assignment still performs REPLACE"
    );
    assert_eq!(view["Wheel"].as_object().unwrap().keys().collect::<Vec<_>>(), ["speed", "right"]);
    let dir = ConfigDir::at(h.tmp.path().join("config"));
    let reloaded = Configuration::load(&dir).unwrap();
    let raw = reloaded.metadata_table("joystick");
    assert_eq!(raw, sqlitedict::read_table(&dir.metadata_file(), "joystick").unwrap());
    assert_eq!(raw.get("Wheel"), Some(&LegacyValue::from(profile)));
    assert_eq!(raw.get("None profile"), Some(&LegacyValue::None));
    assert_eq!(h.context.state.config.read().config().metadata_table("joystick"), raw);
    assert_eq!(
        call(&h, Method::DELETE, "/api/v1/joystick/db", Some(json!({"key":"None profile"}))).await.0,
        StatusCode::NO_CONTENT
    );
    assert!(!Configuration::load(&dir).unwrap().metadata_table("joystick").contains("None profile"));
}

#[tokio::test]
async fn active_raw_profile_edits_and_save_preserve_missing_null_and_unknown_fields() {
    use openrigcue_config::import::value::{Dict, LegacyValue};
    use openrigcue_config::profiles::{SaveTarget, Vehicle};

    let h = harness();
    {
        let mut store = h.context.state.config.write();
        let mut changed = store.config().clone();
        let mut native = changed.profiles.resolve_native("emulator", &Vehicle::Unknown).unwrap().0;
        let fields = native.as_dict_mut().unwrap();
        fields.remove("motion_reaction_speed");
        fields.insert("custom", serde_json::json!({"first":1,"second":null}).into());
        fields.insert("haptic_effects", serde_json::json!({"wheel_gear":4,"rpm_start_rumble":12}).into());
        fields.insert("routines", Dict::new().into());
        changed.profiles.save_native("emulator", &SaveTarget::Game, native).unwrap();
        store.commit_game("emulator", changed).unwrap();
        *h.context.state.profile.write() = crate::profile::ActiveProfile::standby(&store);
    }
    let (_, before) = call(&h, Method::GET, "/api/v1/profile", None).await;
    assert!(before["tune"].get("motion_reaction_speed").is_none());
    assert_eq!(before["tune"]["custom"], json!({"first":1,"second":null}));
    let edits = json!({
        "gear_shift_perc":71,
        "haptic_effects":{},
        "intellitune_motion_preference":null,
        "routines":{"Fanalab":{"enabled":true,"profile":null}}
    });
    let (status, edited) = call(&h, Method::PATCH, "/api/v1/profile", Some(edits)).await;
    assert_eq!(status, StatusCode::OK, "{edited}");
    assert_eq!(edited["gear_shift_perc"], json!(71));
    assert_eq!(edited["haptic_effects"], json!({}));
    assert_eq!(edited["intellitune_motion_preference"], Value::Null);
    assert_eq!(edited["routines"]["Fanalab"]["profile"], Value::Null);
    assert!(edited.get("motion_reaction_speed").is_none());
    assert_eq!(call(&h, Method::PATCH, "/api/v1/profile", Some(json!({}))).await.1, edited);
    assert_eq!(
        call(&h, Method::POST, "/api/v1/profile/save", Some(json!({"level":"game"}))).await.0,
        StatusCode::NO_CONTENT
    );
    let restored = Configuration::load(&ConfigDir::at(h.tmp.path().join("config"))).unwrap();
    let (native, _) = restored.profiles.resolve_native("emulator", &Vehicle::Unknown).unwrap();
    let fields = native.as_dict().unwrap();
    assert_eq!(fields.get("gear_shift_perc"), Some(&LegacyValue::Int(71)));
    assert!(fields.get("motion_reaction_speed").is_none());
    assert_eq!(fields.get("haptic_effects"), Some(&LegacyValue::Dict(Dict::new())));
    assert_eq!(openrigcue_config::import::de::from_legacy::<Value>(&native).unwrap(), edited);
}

#[tokio::test]
async fn settings_api_returns_original_values_and_saves_only_page_assignments() {
    use openrigcue_config::settings::Settings;
    let h = harness();
    h.context.state.config.write().update(|configuration| {
        let mut raw = configuration.settings.native_snapshot();
        let motion = raw.get_mut("MOTIONBOX").unwrap().as_dict_mut().unwrap();
        motion.remove("reaction_speed");
        motion.insert("refresh_rate", "fast".into());
        configuration.settings = Settings::from_legacy(&raw).0;
    });
    let (status, before) = call(&h, Method::GET, "/api/v1/settings", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(before["motion"].get("reaction_speed").is_none());
    assert_eq!(before["motion"]["refresh_rate"], "fast");
    let (_, after) = call(&h, Method::PATCH, "/api/v1/settings", Some(json!({"app":{"start_page":"Motion"}}))).await;
    assert!(after["motion"].get("reaction_speed").is_none());
    assert_eq!(after["motion"]["refresh_rate"], "fast");
    assert_eq!(call(&h, Method::POST, "/api/v1/settings/save", None).await.0, StatusCode::NO_CONTENT);
    let restored = Configuration::load(&ConfigDir::at(h.tmp.path().join("config"))).unwrap();
    let view = restored.settings.ui_view().unwrap();
    assert_eq!(view["app"]["start_page"], "Motion");
    assert_eq!(view["motion"]["refresh_rate"], "fast");
    assert!(view["motion"].get("reaction_speed").is_none());
}

#[tokio::test]
async fn native_profile_import_stays_pending_until_the_actual_save() {
    use openrigcue_config::profiles::{export_profile, ExportMetadata, Vehicle};
    let h = harness();
    let dir = ConfigDir::at(h.tmp.path().join("config"));
    let original_profile_db = std::fs::read(dir.profiles_file()).unwrap();
    let original_ini = std::fs::read(dir.settings_file()).unwrap();
    let mut source = h.context.state.config.read().config().profiles.clone();
    let mut raw = source.resolve_native("emulator", &Vehicle::Unknown).unwrap().0;
    let fields = raw.as_dict_mut().unwrap();
    fields.remove("motion_reaction_speed");
    fields.insert("gear_shift_perc", 71.into());
    fields.insert("custom_tag", serde_json::json!([1, null]).into());
    source.set_native_game("emulator", raw);
    let meta = ExportMetadata { motion_model: "P3", shaker_layout: "Custom", builtin_layouts: &[] };
    let (_, bytes) = export_profile(
        &source,
        openrigcue_config::catalog::Catalog::builtin(),
        "emulator",
        &Vehicle::Unknown,
        None,
        &meta,
    )
    .unwrap();
    let request =
        Request::builder().method(Method::POST).uri("/api/v1/profiles/import").body(Body::from(bytes)).unwrap();
    let response = router(h.context.clone()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        std::fs::read(dir.profiles_file()).unwrap(),
        original_profile_db,
        "import has not committed the profile"
    );
    assert_eq!(std::fs::read(dir.settings_file()).unwrap(), original_ini, "import did not write unrelated settings");
    h.context.main_loop.lock().unwrap().synchronize_profile(&h.context).unwrap();
    let (_, active) = call(&h, Method::GET, "/api/v1/profile", None).await;
    assert_eq!(active["tune"]["custom_tag"], json!([1, null]));
    assert!(active["tune"].get("motion_reaction_speed").is_none());
    assert_eq!(
        call(&h, Method::POST, "/api/v1/profile/save", Some(json!({"level":"game"}))).await.0,
        StatusCode::NO_CONTENT
    );
    let restored = Configuration::load(&dir).unwrap();
    let raw = restored.profiles.resolve_native("emulator", &Vehicle::Unknown).unwrap().0;
    assert_eq!(raw.as_dict().unwrap().get("custom_tag"), Some(&serde_json::json!([1, null]).into()));
    assert!(raw.as_dict().unwrap().get("motion_reaction_speed").is_none());
}

#[tokio::test]
async fn manager_reset_restores_one_raw_factory_row_including_untypable_vehicle_entries() {
    let h = harness();
    let raw = json!({"custom":true,"vehicles":{"Empty":{},"None":null}});
    assert_eq!(call(&h, Method::PUT, "/api/v1/tunes/ac", Some(raw)).await.0, StatusCode::NO_CONTENT);
    assert_eq!(
        call(&h, Method::PUT, "/api/v1/tunes/ac/vehicles/None", Some(Value::Null)).await.0,
        StatusCode::NO_CONTENT
    );
    let generation = h.context.state.profile.read().load_generation;
    assert_eq!(call(&h, Method::DELETE, "/api/v1/tunes/ac", None).await.0, StatusCode::NO_CONTENT);
    let raw = h.context.state.config.read().config().profiles.native_table();
    assert_eq!(raw.get("ac"), openrigcue_config::import::shipped::ShippedData::get().master_tunes.get("ac"));
    assert_eq!(
        h.context.state.profile.read().load_generation,
        generation,
        "manager reset does not load a different active profile"
    );
    let loaded = Configuration::load(&ConfigDir::at(h.tmp.path().join("config"))).unwrap();
    assert_eq!(loaded.profiles.native_table().get("ac"), raw.get("ac"));
}

#[tokio::test]
async fn missing_factory_profile_leaves_reset_deletion_pending_until_commit() {
    let h = harness();
    let key = "not_a_factory_game";
    {
        let mut store = h.context.state.config.write();
        store.stage_game_row(key, Some(json!({"vehicles":{}}).into())).unwrap();
        store.commit_profiles().unwrap();
    }
    assert_eq!(call(&h, Method::DELETE, "/api/v1/tunes/not_a_factory_game", None).await.0, StatusCode::CONFLICT);
    assert!(!h.context.state.config.read().config().profiles.native_table().contains(key));
    let dir = ConfigDir::at(h.tmp.path().join("config"));
    assert!(Configuration::load(&dir).unwrap().profiles.native_table().contains(key));
    h.context.state.config.read().commit_profiles().unwrap();
    assert!(!Configuration::load(&dir).unwrap().profiles.native_table().contains(key));
}

#[tokio::test]
async fn routine_null_profile_survives_the_real_save_and_restart_path() {
    let h = harness();
    let routines = json!({"controller":{"enabled":true,"profile":null}});
    assert_eq!(call(&h, Method::PATCH, "/api/v1/profile", Some(json!({"routines":routines}))).await.0, StatusCode::OK);
    assert_eq!(
        call(&h, Method::POST, "/api/v1/profile/save", Some(json!({"level":"game"}))).await.0,
        StatusCode::NO_CONTENT
    );
    let restored = Configuration::load(&ConfigDir::at(h.tmp.path().join("config"))).unwrap();
    assert_eq!(restored.profiles.get("emulator").unwrap().routines.as_ref(), Some(&routines));
    let raw = restored.profiles.native_table();
    let routine = raw
        .get("emulator")
        .unwrap()
        .as_dict()
        .unwrap()
        .get("routines")
        .unwrap()
        .as_dict()
        .unwrap()
        .get("controller")
        .unwrap()
        .as_dict()
        .unwrap();
    assert_eq!(routine.get("profile"), Some(&openrigcue_config::import::value::LegacyValue::None));
}

async fn call(h: &Harness, method: Method, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let request = Request::builder().method(method).uri(uri);
    let request = match body {
        Some(json) => request.header("content-type", "application/json").body(Body::from(json.to_string())).unwrap(),
        None => request.body(Body::empty()).unwrap(),
    };
    let response = router(h.context.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes().to_vec();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// The hardware status box's inputs: the belt identity and homing, the Intellibox model/version (-1 before one
/// answered), the iFlag firmware, the HUD (`Not Connected` until the OBD2HUD thread finds one), the Thanos
/// controller, the Yaw VR.
#[tokio::test]
async fn status_publishes_device_details() {
    let h = harness();
    let (status, body) = call(&h, Method::GET, "/api/v1/status", None).await;
    assert_eq!(status, StatusCode::OK);
    let accessories = &body["accessories"];
    assert_eq!(
        (accessories["intellibox_model"].clone(), accessories["intellibox_version"].clone()),
        (json!(-1), json!(-1))
    );
    assert_eq!(accessories["belt_homing"], 0);
    assert_eq!(accessories["belt_identity"], Value::Null);
    assert_eq!(accessories["hud"], "Not Connected");
    assert_eq!(accessories["iflag_firmware"], json!(0));
    assert_eq!(body["devices"]["thanos"]["config"], json!({}));
    assert_eq!(body["devices"]["yaw_vr"]["limits"], Value::Null);
    assert_eq!(body["motion"]["assignment"], Value::Null);
}

/// The belt homing state is shared by the belt controller and the UI: the controls show the belt's state
/// and a 0 the UI writes reaches the belt.
#[tokio::test]
async fn live_belt_homing_explicit_unchanged_write_wins_over_a_later_publication() {
    let h = harness();
    let before = h.context.state.controls.snapshot();
    let after = before.clone();
    h.context.accessories.set_belt_homing(Homing::InProgress);
    super::ui_state::controls_changed(&h.context, &before, &after, &json!({"tests":{"belt_homing":0}}));
    assert_eq!(h.context.accessories.belt_homing(), Homing::NotCalibrated);
    h.context.accessories.set_belt_homing(Homing::InProgress);
    super::ui_state::controls_changed(&h.context, &before, &after, &json!({"tests":{"belt_test":false}}));
    assert_eq!(h.context.accessories.belt_homing(), Homing::InProgress);
}

#[tokio::test]
async fn belt_homing_is_the_belts() {
    let h = harness();
    h.context.accessories.belt.lock().unwrap().set_homing(Homing::InProgress);
    let (_, controls) = call(&h, Method::GET, "/api/v1/controls", None).await;
    assert_eq!(controls["tests"]["belt_homing"], 3);
    let (_, status) = call(&h, Method::GET, "/api/v1/status", None).await;
    assert_eq!(status["controls"]["tests"]["belt_homing"], 3);
    let (code, _) = call(&h, Method::PATCH, "/api/v1/controls", Some(json!({ "tests": { "belt_homing": 0 } }))).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(h.context.accessories.belt_homing(), Homing::NotCalibrated);
    let (_, status) = call(&h, Method::GET, "/api/v1/status", None).await;
    assert_eq!(status["accessories"]["belt_homing"], 0);
    // The motion test toggle and the motion test sliders answer with the same controls: the belt's homing too.
    h.context.accessories.belt.lock().unwrap().set_homing(Homing::InProgress);
    let (_, controls) = call(&h, Method::POST, "/api/v1/motion/test/toggle", None).await;
    assert_eq!(
        (controls["tests"]["motion_test"].clone(), controls["tests"]["belt_homing"].clone()),
        (json!(true), json!(3))
    );
    let (code, controls) = call(&h, Method::PUT, "/api/v1/motion/test", Some(json!({}))).await;
    assert_eq!((code, controls["tests"]["belt_homing"].clone()), (StatusCode::OK, json!(3)));
}

/// The Yaw VR centre button: motion homing is requested only while the platform is connected.
#[tokio::test]
async fn center_needs_a_connected_platform() {
    let h = harness();
    let yaw = || h.context.motion.driver_settings().yaw_shared.get().homing_requested;
    assert_eq!(call(&h, Method::POST, "/api/v1/motion/center", None).await.0, StatusCode::NO_CONTENT);
    assert!(!yaw());
    h.context.state.motion.write().link.status = LinkStatus::Connected;
    call(&h, Method::POST, "/api/v1/motion/center", None).await;
    assert!(yaw());
}

/// The limits confirmation: the dialog's limits become the Yaw VR runtime limits (sent by the next frame).
#[tokio::test]
async fn native_yaw_limits_confirmation_partial_api_retains_unsupplied_fields() {
    let h = harness();
    let before = openrigcue_devices::motion::yaw_vr::protocol::YawLimits::EMULATOR;
    h.context.motion.driver_settings().yaw_shared.update(|shared| shared.limits = Some(before));
    assert_eq!(
        call(&h, Method::PATCH, "/api/v1/motion/yaw-vr/limits", Some(json!({"power":17}))).await.0,
        StatusCode::NO_CONTENT
    );
    let mut expected = before;
    expected.power = 17;
    assert_eq!(h.context.motion.driver_settings().yaw_shared.get().limits, Some(expected));
    assert_eq!(
        call(&h, Method::PATCH, "/api/v1/motion/yaw-vr/limits", Some(json!({"unknown":0}))).await.0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(h.context.motion.driver_settings().yaw_shared.get().limits, Some(expected));
}

#[tokio::test]
async fn yaw_limits() {
    let h = harness();
    let limits = json!({ "power": 80, "pitch_fwd_limit": 10, "pitch_bwd_limit": 20, "roll_limit": 15, "yaw_limit": 90, "yaw_limit_enabled": 0, "vibration_enabled": 1 });
    assert_eq!(
        call(&h, Method::PUT, "/api/v1/motion/yaw-vr/limits", Some(limits.clone())).await.0,
        StatusCode::NO_CONTENT
    );
    let set = h.context.motion.driver_settings().yaw_shared.get().limits.unwrap();
    assert_eq!(
        (
            set.power,
            set.pitch_forward,
            set.pitch_backward,
            set.roll,
            set.yaw,
            set.yaw_limit_enabled,
            set.vibration_enabled
        ),
        (80, 10, 20, 15, 90, 0, 1)
    );
    let (_, status) = call(&h, Method::GET, "/api/v1/status", None).await;
    assert_eq!(status["devices"]["yaw_vr"]["limits"], limits);
}

/// The Thanos confirmation: the dialog's writes in order, `motor_number` appended (one controller:
/// `nbr_motors - 4`; two: 4 and `nbr_motors - 4`); keys without a command are not sent.
#[tokio::test]
async fn thanos_pending_configuration() {
    let h = harness();
    let model = h.context.state.config.read().settings().motion.model.clone();
    let motors = openrigcue_devices::motion::motion_model(&model, false).unwrap().motors as i64;
    let body = json!({ "main": [["park_position", 10], ["stroke1", 15], ["park_position", 12]], "secondary": [["ratio-2", 1], ["klm_mode", 2]] });
    assert_eq!(call(&h, Method::PUT, "/api/v1/motion/thanos", Some(body.clone())).await.0, StatusCode::NO_CONTENT);
    let controller = h.context.motion.driver_settings().thanos_shared.get().controller;
    let names = |list: &[openrigcue_devices::motion::thanos::PendingChange]| {
        list.iter().map(|c| (c.field.name(), c.value.to_string())).collect::<Vec<_>>()
    };
    assert_eq!(
        names(&controller.main_pending),
        [
            ("park_position".into(), "12".into()),
            ("stroke1".into(), "15".into()),
            ("motor_number".into(), (motors - 4).to_string())
        ]
    );
    assert_eq!(names(&controller.secondary_pending), [("klm_mode".to_owned(), "2".to_owned())]);
    h.context.state.motion.write().link.ports.insert(0, "/dev/a".into());
    h.context.state.motion.write().link.ports.insert(1, "/dev/b".into());
    call(&h, Method::PUT, "/api/v1/motion/thanos", Some(body)).await;
    let controller = h.context.motion.driver_settings().thanos_shared.get().controller;
    assert_eq!(names(&controller.main_pending).last().unwrap().1, "4");
    assert_eq!(names(&controller.secondary_pending).last().unwrap().1, (motors - 4).to_string());
}

#[tokio::test]
async fn thanos_staged_pending_configuration() {
    let h = harness();
    h.context.state.config.write().update(|config| config.settings.motion.model = "missing model".into());
    let path = "/api/v1/motion/thanos/pending";
    assert_eq!(
        call(
            &h,
            Method::PUT,
            path,
            Some(json!({"secondary":false,"replace":true,"entries":[["park_position",10],["stroke1",15]]}))
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(&h, Method::PUT, path, Some(json!({"secondary":true,"replace":true,"entries":[["klm_mode",2]]}))).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(
            &h,
            Method::PUT,
            path,
            Some(json!({"secondary":false,"replace":false,"entries":[["park_position",12],["motor_number",4]]}))
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    let controller = h.context.motion.driver_settings().thanos_shared.get().controller;
    let names = |list: &[openrigcue_devices::motion::thanos::PendingChange]| {
        list.iter().map(|change| (change.field.name(), change.value.to_string())).collect::<Vec<_>>()
    };
    assert_eq!(
        names(&controller.main_pending),
        [("park_position".into(), "12".into()), ("stroke1".into(), "15".into()), ("motor_number".into(), "4".into())]
    );
    assert_eq!(names(&controller.secondary_pending), [("klm_mode".into(), "2".into())]);
    assert_eq!(
        call(&h, Method::PUT, path, Some(json!({"secondary":false,"replace":true,"entries":[]}))).await.0,
        StatusCode::NO_CONTENT
    );
    let controller = h.context.motion.driver_settings().thanos_shared.get().controller;
    assert!(controller.main_pending.is_empty());
    assert_eq!(names(&controller.secondary_pending), [("klm_mode".into(), "2".into())]);
}

/// The max angle confirmation (every game and vehicle) and the spike filter confirmation (not the emulator).
#[tokio::test]
async fn apply_to_all_tunes() {
    let h = harness();
    assert_eq!(
        call(&h, Method::POST, "/api/v1/tunes/apply-all", Some(json!({ "field": "max_angle", "value": 60 }))).await.0,
        StatusCode::NO_CONTENT
    );
    call(&h, Method::POST, "/api/v1/tunes/apply-all", Some(json!({ "field": "spike_filter", "value": 2100 }))).await;
    let store = h.context.state.config.read();
    let profiles = &store.config().profiles;
    assert!(profiles
        .games()
        .all(|(_, t)| t.motion_max_power == 60.0 && t.vehicles.values().all(|v| v.motion_max_power == 60.0)));
    assert_ne!(profiles.get("emulator").unwrap().thanos_spike_filter_vertical, 2100.0);
    let game = profiles.games().find(|(g, _)| *g != "emulator").unwrap().1;
    assert_eq!((game.thanos_spike_filter_horizontal, game.thanos_spike_filter_vertical), (2100.0, 2100.0));
}

#[tokio::test]
async fn failed_bulk_profile_update_keeps_earlier_rows_pending_until_a_later_commit() {
    use openrigcue_config::import::value::LegacyValue;
    let h = harness();
    let (first, second, before) = {
        let mut store = h.context.state.config.write();
        let table = store.config().profiles.native_table();
        let keys = table.str_keys();
        let (first, second) = (keys[0].clone(), keys[1].clone());
        let mut bad = table.get(&second).unwrap().clone();
        bad.as_dict_mut().unwrap().insert("vehicles", LegacyValue::None);
        store.stage_game_row(&second, Some(bad)).unwrap();
        store.commit_profiles().unwrap();
        let before = store.config().profiles.native_table();
        (first, second, before)
    };
    let (status, _) =
        call(&h, Method::POST, "/api/v1/tunes/apply-all", Some(json!({"field":"max_angle","value":63}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let in_memory = h.context.state.config.read().config().profiles.native_table();
    assert_eq!(in_memory.get(&first).unwrap().as_dict().unwrap().get("motion_max_power"), Some(&LegacyValue::Int(63)));
    assert_eq!(in_memory.get(&second), before.get(&second));
    let dir = ConfigDir::at(h.tmp.path().join("config"));
    let on_disk = Configuration::load(&dir).unwrap().profiles.native_table();
    assert_eq!(on_disk.get(&first), before.get(&first), "the interrupted loop has not committed");
    h.context.state.config.read().commit_profiles().unwrap();
    let committed = Configuration::load(&dir).unwrap().profiles.native_table();
    assert_eq!(committed.get(&first), in_memory.get(&first));
}

/// Loading the shaker profiles, duplicating one and removing one; both changes are written at once
/// (the speaker layout table autocommits).
#[tokio::test]
async fn speaker_layouts() {
    let h = harness();
    let (_, list) = call(&h, Method::GET, "/api/v1/shakers/layouts", None).await;
    let names: Vec<String> =
        list["layouts"].as_array().unwrap().iter().map(|l| l["name"].as_str().unwrap().to_owned()).collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted);
    assert!(list["layouts"].as_array().unwrap().iter().all(|l| l["builtin"] == true));
    let source = "6 Speakers - 5.1";
    let (code, problem) = call(
        &h,
        Method::POST,
        &format!("/api/v1/shakers/layouts/{}/duplicate", "6%20Speakers%20-%205.1"),
        Some(json!({ "name": source })),
    )
    .await;
    assert_eq!(
        (code, problem["detail"].as_str().unwrap()),
        (StatusCode::CONFLICT, "A profile with this name exists already; choose another name.")
    );
    let (code, _) = call(
        &h,
        Method::POST,
        "/api/v1/shakers/layouts/6%20Speakers%20-%205.1/duplicate",
        Some(json!({ "name": "Mine" })),
    )
    .await;
    assert_eq!(code, StatusCode::NO_CONTENT);
    {
        let store = h.context.state.config.read();
        assert_eq!(store.settings().shakers.speaker_layout, "Mine");
        assert_eq!(store.config().shaker_layouts.get("Mine"), store.config().shaker_layouts.get(source));
    }
    let written =
        openrigcue_config::import::sqlitedict::read_table(&h.tmp.path().join("config/metadata.db"), "shakers").unwrap();
    assert!(written.contains("Mine"));
    assert_eq!(call(&h, Method::DELETE, "/api/v1/shakers/layouts/Mine", None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(h.context.state.config.read().settings().shakers.speaker_layout, DEFAULT_LAYOUT);
    assert!(h.context.state.config.read().config().shaker_layouts.get("Mine").is_none());
    assert_eq!(call(&h, Method::DELETE, "/api/v1/shakers/layouts/Mine", None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(
        call(&h, Method::DELETE, &format!("/api/v1/shakers/layouts/{}", DEFAULT_LAYOUT.replace(' ', "%20")), None)
            .await
            .0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert!(
        !h.context.state.config.read().metadata_contains("shakers", DEFAULT_LAYOUT).unwrap(),
        "native deletion precedes the failed reload of the removed default"
    );
}

/// The config button opens with the speaker's effect matrix; the confirmation stores the edited copy with
/// accel/brake as the larger corner; the profile Save stores it; another layout drops it.
#[tokio::test]
async fn profile_save_always_writes_the_shaker_row_after_the_game_commit() {
    use openrigcue_config::import::{sqlitedict, value::LegacyValue};
    let h = harness();
    let directory = h.context.state.config.read().dir().unwrap().clone();
    let selected = h.context.state.config.read().settings().shakers.speaker_layout.clone();
    assert!(h.context.state.layout_edit.read().is_none());
    let connection = rusqlite::Connection::open(directory.metadata_file()).unwrap();
    connection.execute_batch(
        "CREATE TRIGGER reject_layout BEFORE INSERT ON shakers BEGIN SELECT RAISE(ABORT,'fixture unchanged layout failure'); END;",
    ).unwrap();
    let original = h.context.state.config.read().profile_row("emulator").unwrap().unwrap();
    {
        let mut active = h.context.state.profile.write();
        let mut raw = active.native_tune();
        raw.assign_item("gear_shift_perc", LegacyValue::Int(61)).unwrap();
        active.replace_native_tune(raw);
    }
    let (status, _) = call(&h, Method::POST, "/api/v1/profile/save", Some(json!({"level":"game"}))).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "no matrix edit does not skip set_shaker_effects");
    let written = sqlitedict::read_item(&directory.profiles_file(), "unnamed", "emulator").unwrap().unwrap();
    assert_ne!(written, original, "the game commit precedes the failing layout assignment");
    assert_eq!(written.item("gear_shift_perc"), Ok(&LegacyValue::Int(61)));
    connection.execute_batch("DROP TRIGGER reject_layout").unwrap();
    let save = json!({"level":"game"});
    assert_eq!(call(&h, Method::POST, "/api/v1/profile/save", Some(save.clone())).await.0, StatusCode::NO_CONTENT);
    let other = if selected == "6 Speakers - 5.1" { "1 Speaker - Center" } else { "6 Speakers - 5.1" };
    let other_value = sqlitedict::read_item(&directory.metadata_file(), "shakers", other).unwrap().unwrap();
    sqlitedict::set_item(&directory.metadata_file(), "shakers", other, &other_value).unwrap();
    assert_eq!(call(&h, Method::POST, "/api/v1/profile/save", Some(save)).await.0, StatusCode::NO_CONTENT);
    let keys: Vec<String> = connection
        .prepare("SELECT key FROM shakers ORDER BY rowid")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(keys.last(), Some(&selected), "even an equal assignment moves the selected row to the end");
}

#[tokio::test]
async fn shaker_save_preserves_raw_fields_and_ignores_unrelated_opaque_rows() {
    use openrigcue_config::import::{sqlitedict, value::LegacyValue};
    let h = harness();
    let directory = h.context.state.config.read().dir().unwrap().clone();
    let selected = h.context.state.config.read().settings().shakers.speaker_layout.clone();
    let mut raw = sqlitedict::read_item(&directory.metadata_file(), "shakers", &selected).unwrap().unwrap();
    let channel = raw.as_dict_mut().unwrap().get_mut("FL").unwrap().as_dict_mut().unwrap();
    channel.insert("unknown", LegacyValue::None);
    channel.get_mut("sfx").unwrap().as_dict_mut().unwrap().insert("future", LegacyValue::from(json!([1, null])));
    sqlitedict::set_item(&directory.metadata_file(), "shakers", &selected, &raw).unwrap();
    // Represents loading the raw layout before a matrix edit.
    *h.context.state.config.write() = ConfigStore::load(directory.clone()).unwrap();
    crate::shaker_layout::load(&h.context.state).unwrap();
    let connection = rusqlite::Connection::open(directory.metadata_file()).unwrap();
    connection.execute("INSERT INTO shakers (key,value) VALUES ('opaque',?)", [vec![255_u8]]).unwrap();
    assert_eq!(
        call(&h, Method::PUT, "/api/v1/shakers/speakers/FL/matrix", Some(json!({"engine":4.25}))).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(&h, Method::POST, "/api/v1/profile/save", Some(json!({"level":"game"}))).await.0,
        StatusCode::NO_CONTENT
    );
    raw.as_dict_mut()
        .unwrap()
        .get_mut("FL")
        .unwrap()
        .as_dict_mut()
        .unwrap()
        .get_mut("sfx")
        .unwrap()
        .as_dict_mut()
        .unwrap()
        .insert("engine", LegacyValue::Float(4.25));
    assert_eq!(sqlitedict::read_item(&directory.metadata_file(), "shakers", &selected).unwrap(), Some(raw));
    let opaque: Vec<u8> =
        connection.query_row("SELECT value FROM shakers WHERE key='opaque'", [], |row| row.get(0)).unwrap();
    assert_eq!(opaque, [255]);
    assert!(h.context.state.layout_edit.read().is_none());
}

#[tokio::test]
async fn custom_matrix_edit() {
    let h = harness();
    call(&h, Method::POST, "/api/v1/shakers/layouts/6%20Speakers%20-%205.1/duplicate", Some(json!({ "name": "Mine" })))
        .await;
    let (code, levels) = call(&h, Method::GET, "/api/v1/shakers/speakers/SUB/levels", None).await;
    assert_eq!(code, StatusCode::OK);
    assert!(levels["engine"].is_number());
    let edit = json!({ "accel_lr": 7.0, "accel_rr": 3.0, "brake_lf": 2.0, "brake_rf": 9.0 });
    assert_eq!(
        call(&h, Method::PUT, "/api/v1/shakers/speakers/SUB/matrix", Some(edit)).await.0,
        StatusCode::NO_CONTENT
    );
    let (_, levels) = call(&h, Method::GET, "/api/v1/shakers/speakers/SUB/levels", None).await;
    assert_eq!((levels["accel"].as_f64(), levels["brake"].as_f64()), (Some(7.0), Some(9.0)));
    assert_ne!(h.context.state.config.read().config().shaker_layouts.get("Mine").unwrap().subwoofer.sfx.accel, 7.0);
    assert_eq!(
        call(&h, Method::POST, "/api/v1/profile/save", Some(json!({ "level": "game" }))).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(h.context.state.config.read().config().shaker_layouts.get("Mine").unwrap().subwoofer.sfx.accel, 7.0);
    call(&h, Method::PUT, "/api/v1/shakers/speakers/SUB/matrix", Some(json!({ "engine": 1.0 }))).await;
    assert!(h.context.state.layout_edit.read().is_some());
    call(&h, Method::PATCH, "/api/v1/settings", Some(json!({ "shakers": { "speaker_layout": "6 Speakers - 5.1" } })))
        .await;
    assert!(h.context.state.layout_edit.read().is_none());
}

/// The profile Save ends by storing the layout into the autocommitted speaker layout table: on a saved folder a matrix
/// edit saved with the tuning page's Save is kept at the next start without the settings Save. A failed layout
/// write answers an error, keeps the stored layout and the pending edit, and the next Save writes it.
#[tokio::test]
async fn matrix_edit_saved_with_the_profile_survives_a_restart() {
    let h = harness();
    let config_dir = ConfigDir::at(h.tmp.path().join("config"));
    let sub_accel = |c: &Configuration| c.shaker_layouts.get("Mine").map(|l| l.subwoofer.sfx.accel).unwrap_or_default();
    call(&h, Method::POST, "/api/v1/shakers/layouts/6%20Speakers%20-%205.1/duplicate", Some(json!({ "name": "Mine" })))
        .await;
    let stored = sub_accel(h.context.state.config.read().config());
    assert_ne!(stored, 7.0);
    call(&h, Method::PUT, "/api/v1/shakers/speakers/SUB/matrix", Some(json!({ "accel_lr": 7.0, "accel_rr": 3.0 })))
        .await;

    let layouts_file = config_dir.metadata_file();
    let connection = rusqlite::Connection::open(&layouts_file).unwrap();
    connection.execute_batch(
        "CREATE TRIGGER reject_matrix BEFORE INSERT ON shakers BEGIN SELECT RAISE(ABORT,'fixture matrix write failure'); END;",
    ).unwrap();
    let save = json!({ "level": "game" });
    assert_eq!(
        call(&h, Method::POST, "/api/v1/profile/save", Some(save.clone())).await.0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(sub_accel(h.context.state.config.read().config()), stored);
    assert!(h.context.state.layout_edit.read().is_some());

    connection.execute_batch("DROP TRIGGER reject_matrix").unwrap();
    assert_eq!(call(&h, Method::POST, "/api/v1/profile/save", Some(save)).await.0, StatusCode::NO_CONTENT);
    assert!(h.context.state.layout_edit.read().is_none());
    assert_eq!(sub_accel(h.context.state.config.read().config()), 7.0);
    assert_eq!(sub_accel(&Configuration::load(&config_dir).unwrap()), 7.0);
}

/// A deferred failed SQL assignment is never returned as the current stored tune.
#[tokio::test]
async fn failed_tune_write_keeps_the_stored_tune() {
    let h = harness();
    let config_dir = ConfigDir::at(h.tmp.path().join("config"));
    let (game, mut tune) = {
        let store = h.context.state.config.read();
        let (game, tune) = store.config().profiles.games().next().unwrap();
        (game.to_owned(), tune.clone())
    };
    let stored = tune.gear_shift_perc;
    tune.vehicles.clear();
    tune.gear_shift_perc = stored + 1.5;
    let connection = rusqlite::Connection::open(config_dir.profiles_file()).unwrap();
    connection.execute_batch(&format!(
        "CREATE TRIGGER reject_game BEFORE INSERT ON unnamed WHEN NEW.key = '{}' BEGIN SELECT RAISE(ABORT, 'test write failure'); END;",
        game.replace('\'', "''"))).unwrap();
    let uri = format!("/api/v1/tunes/{game}");
    let body = serde_json::to_value(&tune).unwrap();
    assert_eq!(call(&h, Method::PUT, &uri, Some(body.clone())).await.0, StatusCode::INTERNAL_SERVER_ERROR);
    let (status, current) = call(&h, Method::GET, &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(current["gear_shift_perc"], stored);

    h.context.state.config.read().profile_fixture_sql("DROP TRIGGER reject_game");
    assert_eq!(call(&h, Method::PUT, &uri, Some(body)).await.0, StatusCode::NO_CONTENT);
    let reloaded = Configuration::load(&config_dir).unwrap();
    assert_eq!(reloaded.profiles.get(&game).unwrap().gear_shift_perc, stored + 1.5);
}

/// Starting a Profile Manager edit and clearing the edit context.
#[tokio::test]
async fn profile_edit_context() {
    let h = harness();
    let edit = json!({ "game": "dirtrally2.0", "vehicle": "Fiesta" });
    assert_eq!(call(&h, Method::POST, "/api/v1/profile/edit", Some(edit)).await.0, StatusCode::NO_CONTENT);
    let (_, controls) = call(&h, Method::GET, "/api/v1/controls", None).await;
    assert_eq!(controls["forced_game"], "dirtrally2.0");
    assert_eq!(
        controls["profile_edit"],
        json!({ "game": "dirtrally2.0", "vehicle": "Fiesta", "started_without_listener": true })
    );
    assert_eq!(h.context.telemetry.lock().vehicle(), Some("Fiesta"));
    assert_eq!(call(&h, Method::DELETE, "/api/v1/profile/edit", None).await.0, StatusCode::NO_CONTENT);
    let (_, controls) = call(&h, Method::GET, "/api/v1/controls", None).await;
    assert_eq!((controls["forced_game"].clone(), controls["profile_edit"].clone()), (Value::Null, Value::Null));
    assert_eq!(h.context.telemetry.lock().vehicle(), None);
    // Clearing also ends a forced game without an edit.
    h.context.state.controls.write().forced_game = Some("acc".into());
    call(&h, Method::DELETE, "/api/v1/profile/edit", None).await;
    assert_eq!(h.context.state.controls.read().forced_game, None);
}

/// A language selection: `APP.language` takes the lower-case code and the settings file is written.
#[tokio::test]
async fn language_choice_is_stored() {
    let h = harness();
    let (code, catalog) = call(&h, Method::PUT, "/api/v1/i18n", Some(json!({ "code": "DE" }))).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(catalog["language"], "DE");
    assert_eq!(h.context.state.config.read().settings().app.language, "de");
    let saved = std::fs::read_to_string(h.tmp.path().join("config/config.ini")).unwrap();
    assert!(saved.contains("language = de"));
    let (_, selected) = call(&h, Method::GET, "/api/v1/i18n", None).await;
    assert_eq!(selected, json!({ "language": "DE" }));
}

/// The first single-file write stores `config.ini`, `profile.db` and `metadata.db` together, so the language
/// write and the speaker layout autocommit never lose the rest: on a folder nothing was
/// written to yet, a language switch, a duplicate and a removal each leave a configuration that reloads whole.
#[tokio::test]
async fn first_writes_keep_the_whole_configuration() {
    let reload = |h: &Harness| Configuration::load(&ConfigDir::at(h.tmp.path().join("config"))).unwrap();
    let h = harness_on(false, |_| UiServices::off());
    h.context.state.config.write().update(|c| c.settings.app.start_page = "Motion".into());
    assert_eq!(call(&h, Method::PUT, "/api/v1/i18n", Some(json!({ "code": "DE" }))).await.0, StatusCode::OK);
    let loaded = reload(&h);
    let first = Configuration::first_run();
    assert_eq!(loaded.settings.app.language, "de");
    assert_eq!(loaded.settings.app.start_page, "Motion");
    assert_eq!(
        (loaded.shaker_layouts, loaded.profiles, loaded.joysticks),
        (first.shaker_layouts.clone(), first.profiles.clone(), first.joysticks.clone())
    );

    let h = harness_on(false, |_| UiServices::off());
    h.context.state.config.write().update(|c| c.settings.app.start_page = "Motion".into());
    let (code, _) = call(
        &h,
        Method::POST,
        "/api/v1/shakers/layouts/6%20Speakers%20-%205.1/duplicate",
        Some(json!({ "name": "Mine" })),
    )
    .await;
    assert_eq!(code, StatusCode::NO_CONTENT);
    let loaded = reload(&h);
    assert_eq!(loaded.shaker_layouts.get("Mine"), first.shaker_layouts.get("6 Speakers - 5.1"));
    assert!(first.shaker_layouts.0.keys().all(|name| loaded.shaker_layouts.get(name).is_some()));
    assert_eq!(loaded.profiles, first.profiles);
    // An autocommit of the speaker layout table does not write `config.ini`: the stored settings are the first run's.
    assert_eq!(loaded.settings, first.settings);

    let h = harness_on(false, |_| UiServices::off());
    h.context
        .state
        .config
        .write()
        .update(|c| c.shaker_layouts.set("Mine", first.shaker_layouts.get("6 Speakers - 5.1").unwrap().clone()));
    assert_eq!(call(&h, Method::DELETE, "/api/v1/shakers/layouts/Mine", None).await.0, StatusCode::NO_CONTENT);
    let loaded = reload(&h);
    assert_eq!((loaded.shaker_layouts, loaded.profiles), (first.shaker_layouts.clone(), first.profiles.clone()));

    // The max angle confirmation's profile commit keeps the change and the rest of the configuration.
    let h = harness_on(false, |_| UiServices::off());
    assert_eq!(
        call(&h, Method::POST, "/api/v1/tunes/apply-all", Some(json!({ "field": "max_angle", "value": 60 }))).await.0,
        StatusCode::NO_CONTENT
    );
    let loaded = reload(&h);
    assert!(loaded.profiles.games().all(|(_, t)| t.motion_max_power == 60.0));
    assert_eq!(loaded.profiles.games().count(), first.profiles.games().count());
    assert_eq!((loaded.shaker_layouts, loaded.settings), (first.shaker_layouts.clone(), first.settings.clone()));
}

/// A menu selection ends by setting the selected language again, whose
/// settings write stores the whole configuration: a settings edit left unsaved is on disk after a
/// page change, on a saved folder and on one nothing was written to yet; the language stays the selected one.
#[tokio::test]
async fn page_change_writes_the_settings() {
    for saved in [true, false] {
        let h = harness_on(saved, |_| UiServices::off());
        call(&h, Method::PATCH, "/api/v1/settings", Some(json!({ "app": { "start_page": "Motion" } }))).await;
        assert_eq!(call(&h, Method::POST, "/api/v1/i18n/reapply", None).await.0, StatusCode::NO_CONTENT);
        let loaded = Configuration::load(&ConfigDir::at(h.tmp.path().join("config"))).unwrap();
        assert_eq!((loaded.settings.app.start_page.as_str(), loaded.settings.app.language.as_str()), ("Motion", "en"));
        assert_eq!(loaded.profiles, Configuration::first_run().profiles);
    }
    let h = harness();
    call(&h, Method::PUT, "/api/v1/i18n", Some(json!({ "code": "FR" }))).await;
    call(&h, Method::POST, "/api/v1/i18n/reapply", None).await;
    assert_eq!(h.context.state.config.read().settings().app.language, "fr");
}

/// A log level change applies `APP.log_level` at once; showing the page applies the
/// configured one.
#[tokio::test]
async fn log_level_is_applied_live() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&seen);
    let h = harness_with(move |state| {
        let hook: LogLevelHook = Arc::new(move |level| record.lock().unwrap().push(level));
        UiServices::new(state.config.read().settings(), Some(hook), None)
    });
    assert!(seen.lock().unwrap().is_empty());
    call(&h, Method::POST, "/api/v1/app-loaded", None).await;
    call(&h, Method::PATCH, "/api/v1/settings", Some(json!({ "app": { "log_level": 10 } }))).await;
    call(&h, Method::PATCH, "/api/v1/settings", Some(json!({ "app": { "start_minimized": true } }))).await;
    assert_eq!(*seen.lock().unwrap(), [30, 10]);
}

/// The factory reset: `config.ini`, `profile.db` and `metadata.db` go, the port cache with them
/// (it is `metadata.db`'s `cache` table), and the service restarts on the first-run configuration.
#[tokio::test]
async fn factory_reset() {
    let restarted = Arc::new(Mutex::new(0));
    let count = Arc::clone(&restarted);
    let h = harness_with(move |state| {
        UiServices::new(state.config.read().settings(), None, Some(Arc::new(move || *count.lock().unwrap() += 1)))
    });
    call(&h, Method::PATCH, "/api/v1/settings", Some(json!({ "app": { "start_page": "Motion" } }))).await;
    call(&h, Method::POST, "/api/v1/settings/save", None).await;
    let config = h.tmp.path().join("config");
    std::fs::write(config.join("ports.toml"), "").unwrap();
    assert!(config.join("config.ini").exists() && config.join("profile.db").exists());
    assert_eq!(call(&h, Method::POST, "/api/v1/factory-reset", None).await.0, StatusCode::ACCEPTED);
    for gone in [
        "config.ini",
        "settings.toml",
        "profile.db",
        "metadata.db",
        "tunes",
        "shaker_layouts.toml",
        "joysticks.toml",
        "state.toml",
        "ports.toml",
    ] {
        assert!(!config.join(gone).exists(), "{gone}");
    }
    assert_eq!(*h.context.state.config.read().config(), Configuration::first_run());
    for _ in 0..100 {
        if *restarted.lock().unwrap() == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(*restarted.lock().unwrap(), 1);
    // The fixture restart hook records the call instead of replacing the
    // process. Its reset store must still initialize a complete fresh folder.
    call(&h, Method::POST, "/api/v1/settings/save", None).await;
    for restored in ["config.ini", "profile.db", "metadata.db"] {
        assert!(config.join(restored).is_file(), "{restored}");
    }
    assert_eq!(
        Configuration::load(&openrigcue_config::settings::paths::ConfigDir::at(&config)).unwrap(),
        Configuration::first_run()
    );
}

/// The factory reset tries every file and only reports one it cannot remove;
/// it always reaches the restart: the others go and the service still restarts.
#[tokio::test]
async fn factory_reset_skips_a_file_it_cannot_remove() {
    let restarted = Arc::new(Mutex::new(0));
    let count = Arc::clone(&restarted);
    let h = harness_with(move |state| {
        UiServices::new(state.config.read().settings(), None, Some(Arc::new(move || *count.lock().unwrap() += 1)))
    });
    let config = h.tmp.path().join("config");
    // A directory where the settings file belongs cannot be removed as a file.
    std::fs::remove_file(config.join("config.ini")).unwrap();
    std::fs::create_dir_all(config.join("config.ini/held")).unwrap();
    std::fs::write(config.join("ports.toml"), "").unwrap();
    call(&h, Method::PATCH, "/api/v1/settings", Some(json!({ "app": { "start_page": "Motion" } }))).await;
    assert_eq!(call(&h, Method::POST, "/api/v1/factory-reset", None).await.0, StatusCode::ACCEPTED);
    assert!(config.join("config.ini").is_dir());
    for gone in
        ["profile.db", "metadata.db", "tunes", "shaker_layouts.toml", "joysticks.toml", "state.toml", "ports.toml"]
    {
        assert!(!config.join(gone).exists(), "{gone}");
    }
    assert_eq!(*h.context.state.config.read().config(), Configuration::first_run());
    for _ in 0..100 {
        if *restarted.lock().unwrap() == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(*restarted.lock().unwrap(), 1);
}

/// The page reloads after the restart once a new start answers: every start publishes its own `instance`.
#[tokio::test]
async fn status_names_the_service_start() {
    let (first, second) = (harness(), harness());
    let (_, a) = call(&first, Method::GET, "/api/v1/status", None).await;
    let (_, again) = call(&first, Method::GET, "/api/v1/status", None).await;
    let (_, b) = call(&second, Method::GET, "/api/v1/status", None).await;
    assert!(a["instance"].is_string());
    assert_eq!(a["instance"], again["instance"]);
    assert_ne!(a["instance"], b["instance"]);
}

/// The axis assignment confirmation: the assignment is stored and the platform connects again.
#[tokio::test]
async fn native_axis_confirmation_requires_its_original_parent() {
    use openrigcue_model::native_value::LegacyValue;
    let h = harness();
    h.context.state.config.write().update(|configuration| {
        configuration.settings.edit_native(|raw| raw.insert("MOTIONBOX", LegacyValue::None));
    });
    h.context.state.motion.write().link.status = LinkStatus::Connected;
    let body = json!({"assignment":{"0":"0,00000011"}});
    let (status, error) = call(&h, Method::POST, "/api/v1/motion/axis-assignment", Some(body)).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(error["detail"], json!("TypeError: 'NoneType' object does not support item assignment"));
    assert_eq!(h.context.state.config.read().settings().native_snapshot().get("MOTIONBOX"), Some(&LegacyValue::None));
    assert_eq!(h.context.state.motion.read().link.status, LinkStatus::Connected);
}

#[tokio::test]
async fn native_axis_confirmation_does_not_create_a_missing_parent() {
    let h = harness();
    h.context.state.config.write().update(|configuration| {
        configuration.settings.edit_native(|raw| {
            raw.remove("MOTIONBOX");
        });
    });
    h.context.state.motion.write().link.status = LinkStatus::Connected;
    let body = json!({"assignment":{"0":"0,00000011"}});
    let (status, error) = call(&h, Method::POST, "/api/v1/motion/axis-assignment", Some(body)).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(error["detail"], json!("KeyError: 'MOTIONBOX'"));
    assert!(h.context.state.config.read().settings().native_snapshot().get("MOTIONBOX").is_none());
    assert_eq!(h.context.state.motion.read().link.status, LinkStatus::Connected);
}

#[tokio::test]
async fn axis_assignment() {
    let h = harness();
    h.context.state.motion.write().link.status = LinkStatus::Connected;
    let body = json!({ "assignment": { "0": "0,00000011" } });
    assert_eq!(call(&h, Method::POST, "/api/v1/motion/axis-assignment", Some(body)).await.0, StatusCode::NO_CONTENT);
    assert_eq!(
        h.context.state.config.read().settings().motion.motor_axes.get("0").map(String::as_str),
        Some("0,00000011")
    );
    assert_eq!(h.context.state.motion.read().link.status, LinkStatus::NotConnected);
}

/// The service's translator lays `locales/<code>.json` of the given folder over the built-in file: the user's
/// key wins, the other keys stay built in, a file that does not parse is ignored, and a folder that does not
/// exist changes nothing and is not created.
#[test]
fn user_locale_folder_overlays_the_builtin_texts() {
    let tmp = tempfile::tempdir().unwrap();
    let folder = tmp.path().join("locales");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("en.json"), r#"{"Wind": "Breeze"}"#).unwrap();
    let ui = UiServices::off().with_locale_folder(folder.clone());
    assert_eq!(ui.translate("Wind"), "Breeze");
    assert_eq!(ui.translate("Motionbox"), "Motion");
    std::fs::write(folder.join("en.json"), "{nope").unwrap();
    let ui = UiServices::off().with_locale_folder(folder);
    assert_eq!(ui.translate("Wind"), "Wind");
    assert_eq!(ui.translate("Motionbox"), "Motion");
    let absent = tmp.path().join("absent").join("locales");
    let ui = UiServices::off().with_locale_folder(absent.clone());
    assert_eq!(ui.translate("Motionbox"), "Motion");
    assert!(!absent.exists());
}

/// A failure the service wrote as a sentence with values: the problem document holds the English text as its
/// detail and the sentence beside it, which the UI's own calls show in the selected language; a failure
/// without values has no sentence.
#[tokio::test]
async fn failure_with_values_carries_its_sentence() {
    let body = |error: super::ApiError| async {
        let response = axum::response::IntoResponse::into_response(error);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice::<Value>(&bytes).unwrap()
    };
    let sentence = crate::i18n::sentence("Cannot read ^path^: ^error^", &[("path", "/x"), ("error", "denied")]);
    let problem = body(super::ApiError::internal(sentence.clone())).await;
    assert_eq!(problem["detail"], json!("Cannot read /x: denied"));
    assert_eq!(problem["sentence"], json!(sentence));
    let ui = UiServices::off();
    assert_eq!(ui.translate_sentence(problem["sentence"].as_str().unwrap()), "Cannot read /x: denied");
    let plain = body(super::ApiError::internal("SteamVR was not found")).await;
    assert_eq!(plain["detail"], json!("SteamVR was not found"));
    assert!(plain.get("sentence").is_none());
}
