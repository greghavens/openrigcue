//! `/api/v1/compensation`: what the Motion Compensation page shows and does.
//!
//! - `GET /api/v1/compensation`: sensor readings, the IMU, the published rig
//!   pose, the active consumer (OpenXR layer or SteamVR driver) and the SteamVR driver's status file.
//! - `GET`/`PATCH /api/v1/compensation/vr`: the VR settings (centre of rotation, SRS export), merge patch.
//! - `POST /api/v1/compensation/actions/{action}`: calibrate, switches, re-centre ([`CompensationAction`]).

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use openrigcue_effects::accessories::dispatcher::Accessories;
use openrigcue_effects::accessories::port_scan::Device;
use openrigcue_model::native_value::{Dict, LegacyValue};
use openrigcue_rigpose::{monotonic_now_ns, ConsumerClaim, RigPoseReader, RigSample, RigValues};
use serde_json::{json, Value};

use super::{run_action, CompensationAction, RawSources, VrCompensation, VrSettings};
use crate::api::{patched, ApiContext, ApiError};
use crate::state::ServiceState;

/// The status file the SteamVR driver keeps next to the rig pose (`steamvr-driver.status`, key=value lines).
const DRIVER_STATUS_FILE: &str = "steamvr-driver.status";

/// The routes, to merge into the API router.
pub fn router() -> Router<ApiContext> {
    Router::new()
        .route("/api/v1/compensation", get(status))
        .route("/api/v1/compensation/setup", get(setup_status))
        .route("/api/v1/compensation/setup/{component}", post(install_component))
        .route("/api/v1/compensation/setup/{component}/uninstall", post(uninstall_component))
        .route("/api/v1/compensation/vr", get(vr_settings).patch(patch_vr_settings))
        .route("/api/v1/compensation/actions/{action}", post(action))
}

async fn setup_status(State(ctx): State<ApiContext>) -> Json<super::setup::SetupStatus> {
    shown_setup(&ctx.ui, ctx.compensation.setup_view())
}

/// The setup status as the page shows it: the failures in the selected language.
fn shown_setup(
    ui: &crate::api::ui_state::UiServices,
    mut status: super::setup::SetupStatus,
) -> Json<super::setup::SetupStatus> {
    status.translate_errors(|message| ui.translate_sentence(message));
    Json(status)
}

async fn install_component(
    State(ctx): State<ApiContext>,
    UrlPath(component): UrlPath<String>,
) -> Result<Json<super::setup::SetupStatus>, ApiError> {
    let component =
        super::setup::Component::parse(&component).ok_or_else(|| ApiError::invalid("Unknown VR component"))?;
    let ui = ctx.ui.clone();
    tokio::task::spawn_blocking(move || ctx.compensation.configure_integration(component, false))
        .await
        .map_err(|error| ApiError::internal(error.to_string()))?
        .map(|status| shown_setup(&ui, status))
        .map_err(ApiError::internal)
}

async fn uninstall_component(
    State(ctx): State<ApiContext>,
    UrlPath(component): UrlPath<String>,
) -> Result<Json<super::setup::SetupStatus>, ApiError> {
    let component =
        super::setup::Component::parse(&component).ok_or_else(|| ApiError::invalid("Unknown VR component"))?;
    let ui = ctx.ui.clone();
    tokio::task::spawn_blocking(move || ctx.compensation.configure_integration(component, true))
        .await
        .map_err(|error| ApiError::internal(error.to_string()))?
        .map(|status| shown_setup(&ui, status))
        .map_err(ApiError::internal)
}

async fn status(State(ctx): State<ApiContext>) -> Json<Value> {
    let mut view = status_view(&ctx.state, &ctx.accessories, &ctx.compensation);
    // The writer's failure in the selected language (a sentence with values is looked up and filled in).
    if let Some(error) = view["rig_pose"]["error"].as_str().map(|error| ctx.ui.translate_sentence(error)) {
        view["rig_pose"]["error"] = Value::String(error);
    }
    Json(view)
}

async fn vr_settings(State(ctx): State<ApiContext>) -> Json<VrSettings> {
    Json(ctx.compensation.settings())
}

async fn patch_vr_settings(
    State(ctx): State<ApiContext>,
    Json(patch): Json<Value>,
) -> Result<Json<VrSettings>, ApiError> {
    let settings: VrSettings = patched(&ctx.compensation.settings(), &patch)?;
    let vr = ctx.compensation.clone();
    let saved = settings.clone();
    tokio::task::spawn_blocking(move || vr.set_settings(saved))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
        .map_err(|e| match e {
        super::VrSettingsError::Invalid(detail) => ApiError::invalid(detail),
        other => ApiError::internal(other.to_string()),
    })?;
    Ok(Json(settings))
}

async fn action(State(ctx): State<ApiContext>, UrlPath(name): UrlPath<String>) -> Result<StatusCode, ApiError> {
    let action: CompensationAction =
        name.parse().map_err(|()| ApiError::not_found(format!("no motion-compensation action {name}")))?;
    let raw = RawSources { accessories: &ctx.accessories, yaw_vr: &ctx.motion.driver_settings().yaw_shared };
    match action {
        CompensationAction::Calibrate(axis) => {
            super::actions::run_native_calibration(&ctx.state, raw, &ctx.compensation, Some(axis))
                .map_err(ApiError::invalid)?
        }
        CompensationAction::CalibrateAll => {
            super::actions::run_native_calibration(&ctx.state, raw, &ctx.compensation, None)
                .map_err(ApiError::invalid)?
        }
        _ => run_action(&ctx.state, raw, &ctx.compensation, action),
    }
    Ok(StatusCode::NO_CONTENT)
}

fn motion_sensor(m: &Dict) -> Value {
    let number = |key: &str| match m.get(key) {
        Some(LegacyValue::Int(value)) => *value as f64,
        Some(LegacyValue::Float(value)) => *value,
        _ => 0.0,
    };
    ["pitch", "roll", "yaw", "heave", "sway", "surge"]
        .into_iter()
        .map(|name| {
            (
                name.to_owned(),
                json!({
                    "value": number(name), "calculated": number(&format!("calculated_{name}")),
                    "raw": number(&format!("raw_{name}")),
                }),
            )
        })
        .collect()
}

fn values(v: &RigValues) -> Value {
    json!({ "sway": v.sway, "surge": v.surge, "heave": v.heave, "yaw": v.yaw, "roll": v.roll, "pitch": v.pitch })
}

fn sample(s: &RigSample, now_ns: u64) -> Value {
    json!({
        "values": values(&s.values),
        "rates": s.rates.as_ref().map(values),
        "enabled": s.enabled,
        "cor_offset": { "forward": s.cor_offset.forward, "down": s.cor_offset.down, "right": s.cor_offset.right },
        "recenter_generation": s.recenter_generation,
        "age_ms": s.age_ns(now_ns) as f64 / 1e6,
    })
}

fn consumer(claim: &ConsumerClaim, now_ns: u64) -> Value {
    json!({
        "kind": claim.kind.name(),
        "code": claim.kind.0,
        "pid": claim.pid,
        "live": claim.is_live(now_ns),
        "heartbeat_age_ms": now_ns.saturating_sub(claim.heartbeat_ns) as f64 / 1e6,
    })
}

/// The SteamVR driver's `key=value` status file, when it exists.
fn driver_status(rig_pose: &Path) -> Option<BTreeMap<String, String>> {
    let text = std::fs::read_to_string(rig_pose.with_file_name(DRIVER_STATUS_FILE)).ok()?;
    Some(
        text.lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
            .collect(),
    )
}

/// Everything the page shows.
pub fn status_view(state: &ServiceState, accessories: &Accessories, vr: &VrCompensation) -> Value {
    let now = Instant::now();
    let now_ns = monotonic_now_ns();
    let connection = accessories.connection_status(Device::MotionCompensation);
    let port = accessories.connection_port(Device::MotionCompensation);
    let sensor = accessories.motion_sensor_values().snapshot();
    let reading = accessories.imu_samples().and_then(|s| s.latest());
    let imu = json!({
        "connection": connection,
        "port": port,
        "angles": reading.map(|r| r.angles),
        "rates": reading.and_then(|r| r.rates).map(|(rates, _)| rates),
        "age_ms": reading.map(|r| now.saturating_duration_since(r.received).as_secs_f64() * 1e3),
    });
    let published = vr.publisher_view(now);
    let reader = RigPoseReader::open(vr.rig_pose_path());
    let claim = reader.as_ref().ok().and_then(|r| r.consumer().ok().flatten());
    let (sensor_type, switch_on) =
        (state.config.read().settings().compensation.sensor, state.controls.read().switches.compensation);
    json!({
        "sensor": sensor_type,
        "switch_on": switch_on,
        "motion_sensor": motion_sensor(&sensor),
        "imu": imu,
        "rig_pose": {
            "path": vr.rig_pose_path(),
            "running": published.running,
            "writing": published.writing,
            "error": published.error,
            "published": published.last.as_ref().map(|s| sample(s, now_ns)),
            "rate_hz": published.rate_hz,
        },
        "consumer": claim.as_ref().map(|c| consumer(c, now_ns)),
        "steamvr_driver": driver_status(vr.rig_pose_path()),
        "recenter_generation": vr.recenter_generation(),
        "settings": vr.settings(),
    })
}

/// Direct UI operations over the same service functions as the test route adapters.
pub(crate) async fn direct_call(
    ctx: crate::api::ApiContext,
    call: &crate::api::direct::Call<'_>,
) -> Result<Option<axum::response::Response>, crate::api::ApiError> {
    use openrigcue_ui::backend::Method;
    let response = match (call.method, call.segments.as_slice()) {
        (Method::Get, ["compensation", "setup"]) => {
            axum::response::IntoResponse::into_response(setup_status(State(ctx)).await)
        }
        (Method::Post, ["compensation", "setup", component]) => axum::response::IntoResponse::into_response(
            install_component(State(ctx), UrlPath(call.parameter(component)?)).await,
        ),
        (Method::Post, ["compensation", "setup", component, "uninstall"]) => {
            axum::response::IntoResponse::into_response(
                uninstall_component(State(ctx), UrlPath(call.parameter(component)?)).await,
            )
        }
        (Method::Get, ["compensation"]) => axum::response::IntoResponse::into_response(status(State(ctx)).await),
        (Method::Get, ["compensation", "vr"]) => {
            axum::response::IntoResponse::into_response(vr_settings(State(ctx)).await)
        }
        (Method::Patch, ["compensation", "vr"]) => {
            axum::response::IntoResponse::into_response(patch_vr_settings(State(ctx), call.json()?).await)
        }
        (Method::Post, ["compensation", "actions", parameter0]) => {
            axum::response::IntoResponse::into_response(action(State(ctx), UrlPath(call.parameter(parameter0)?)).await)
        }
        _ => return Ok(None),
    };
    Ok(Some(response))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Method, Request};
    use http_body_util::BodyExt;
    use openrigcue_config::settings::Configuration;
    use openrigcue_effects::shakers::SampleBank;
    use openrigcue_rigpose::{ConsumerKind, RigPoseWriter};
    use openrigcue_sources::derive::Dispatcher;
    use tower::ServiceExt;

    use super::*;
    use crate::games::GameSetup;
    use crate::motion::testing::OfflineHost;
    use crate::motion::MotionHandle;
    use crate::profile::{MainLoop, MainLoopHandle};
    use crate::shakers::{ShakersHandle, SilentBeep};
    use crate::state::OriginalPlatform;
    use crate::store::ConfigStore;
    use crate::telemetry::TelemetryHandle;

    fn context(tmp: &tempfile::TempDir) -> ApiContext {
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let motion = MotionHandle::for_settings(state.config.read().settings(), Box::new(OfflineHost::default()));
        let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), false);
        let setup = GameSetup::discover(tmp.path(), &Default::default());
        let context = ApiContext {
            state,
            telemetry: TelemetryHandle::new(Dispatcher::new(Vec::new())),
            motion,
            shakers,
            accessories: Arc::new(Accessories::new(Instant::now())),
            compensation: VrCompensation::new(
                Some(tmp.path().join(super::super::SETTINGS_FILE)),
                tmp.path().join("rig-pose"),
            ),
            main_loop: MainLoopHandle::new(MainLoop::new(setup, tmp.path().join("proc"))),
            srs_install: None,
            host_ip: "127.0.0.1".into(),
            voice: crate::voice::Voice::off(),
            ui: Arc::new(crate::api::ui_state::UiServices::off()),
            joystick: crate::joystick::JoystickService::offline(),
            hotkeys: crate::hotkeys::Hotkeys::offline(),
        };
        context
            .motion
            .driver_settings()
            .yaw_shared
            .update(|shared| shared.share_motion_sensor(context.accessories.motion_sensor_values()));
        context
    }

    async fn call(ctx: &ApiContext, method: Method, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let request = Request::builder().method(method).uri(uri);
        let request = match body {
            Some(json) => {
                request.header("content-type", "application/json").body(Body::from(json.to_string())).unwrap()
            }
            None => request.body(Body::empty()).unwrap(),
        };
        let response = crate::api::router(ctx.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    /// The page's view: sensor readings, the publisher, the consumer claim of the layer or driver, the driver's
    /// status file, the VR settings; settings patches and actions.
    #[tokio::test]
    async fn status_settings_and_actions() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = context(&tmp);
        let (status, body) = call(&ctx, Method::GET, "/api/v1/compensation", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["imu"]["connection"], "Not Connected");
        assert_eq!(body["motion_sensor"]["pitch"]["raw"], 0.0);
        assert_eq!(body["rig_pose"]["running"], false);
        assert_eq!(body["consumer"], Value::Null);
        assert_eq!(body["settings"]["srs_export"], true);

        // A consumer (the SteamVR driver) holding the claim on the file, and its status file.
        let mut writer = RigPoseWriter::create(ctx.compensation.rig_pose_path()).unwrap();
        writer.publish(&RigSample { timestamp_ns: monotonic_now_ns(), enabled: true, ..RigSample::default() }).unwrap();
        let reader = RigPoseReader::open(ctx.compensation.rig_pose_path()).unwrap();
        reader.claim(ConsumerKind::STEAMVR_DRIVER, monotonic_now_ns()).unwrap();
        std::fs::write(tmp.path().join(DRIVER_STATUS_FILE), "claim=holding the claim\ncalibrated=true\n").unwrap();
        let (_, body) = call(&ctx, Method::GET, "/api/v1/compensation", None).await;
        assert_eq!(body["consumer"]["code"], 2);
        assert_eq!(body["consumer"]["live"], true);
        assert_eq!(body["steamvr_driver"]["calibrated"], "true");

        // The publisher's passes frame each sample; a moved CoR re-centres at the next one.
        let height = ctx.state.config.read().settings().compensation.height;
        assert_eq!(ctx.compensation.framing(height).recenter_generation, 0);
        let (status, body) = call(
            &ctx,
            Method::PATCH,
            "/api/v1/compensation/vr",
            Some(json!({ "cor_down_mm": 620.0, "srs_export": false })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["cor_down_mm"], 620.0);
        assert_eq!(ctx.compensation.framing(height).recenter_generation, 1, "a moved CoR re-centres");
        let (_, body) =
            call(&ctx, Method::PATCH, "/api/v1/compensation/vr", Some(json!({ "cor_down_mm": null }))).await;
        assert_eq!(body["cor_down_mm"], Value::Null, "null follows Seated Height again");
        assert_eq!(ctx.compensation.framing(height).recenter_generation, 2);
        let (status, _) =
            call(&ctx, Method::PATCH, "/api/v1/compensation/vr", Some(json!({ "cor_forward_mm": "far" }))).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        let (status, _) = call(&ctx, Method::POST, "/api/v1/compensation/actions/recenter", None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(ctx.compensation.recenter_generation(), 3);
        let (status, _) =
            call(&ctx, Method::POST, "/api/v1/compensation/actions/motion_compensation_switch", None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(!ctx.state.controls.read().switches.compensation);
        let (status, _) = call(&ctx, Method::POST, "/api/v1/compensation/actions/pitch_calibrate", None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _) = call(&ctx, Method::POST, "/api/v1/compensation/actions/levitate", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn linux_vr_setup_uses_direct_and_http_adapters_with_isolated_registration() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = context(&tmp);
        *ctx.compensation.0.setup.lock().unwrap() = Some(super::super::setup::tests::fixture(tmp.path()));
        let (status, before) = call(&ctx, Method::GET, "/api/v1/compensation/setup", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(before["steamvr"]["installed"], false);
        let response = crate::api::direct::call(
            ctx.clone(),
            openrigcue_ui::backend::Method::Post,
            "compensation/setup/steamvr",
            axum::body::Bytes::from_static(b"{}"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let installed: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(installed["steamvr"]["installed"], true);
        let (status, installed) = call(&ctx, Method::POST, "/api/v1/compensation/setup/openxr", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(installed["openxr"]["installed"], true);
        let (_, active) = call(&ctx, Method::GET, "/api/v1/compensation", None).await;
        assert_eq!(active["consumer"], Value::Null, "installing must not claim active compensation");
        let (status, _) = call(&ctx, Method::POST, "/api/v1/compensation/setup/not-a-component", None).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn external_deployment_roundtrip_leaves_internal_components_ready() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = context(&tmp);
        *ctx.compensation.0.setup.lock().unwrap() = Some(super::super::setup::tests::fixture(tmp.path()));
        let (_, initial) = call(&ctx, Method::GET, "/api/v1/compensation/setup", None).await;
        assert_eq!(initial["openxr"]["installed"], false, "read-only status must not install");
        ctx.compensation.initialize_native_integrations();
        let (_, installed) = call(&ctx, Method::GET, "/api/v1/compensation/setup", None).await;
        assert_eq!(installed["openxr"]["installed"], true);
        assert_eq!(installed["steamvr"]["registered"], false);
        assert_eq!(installed["openxr"]["registered"], false);
        for component in ["steamvr", "openxr"] {
            let (status, _) = call(&ctx, Method::POST, &format!("/api/v1/compensation/setup/{component}"), None).await;
            assert_eq!(status, StatusCode::OK);
        }
        assert_eq!(installed["steamvr"]["active"], false);
        assert_eq!(installed["steamvr"]["restart_needed"], false);
        let mut writer = RigPoseWriter::create(ctx.compensation.rig_pose_path()).unwrap();
        writer.publish(&RigSample { timestamp_ns: monotonic_now_ns(), enabled: true, ..Default::default() }).unwrap();
        let reader = RigPoseReader::open(ctx.compensation.rig_pose_path()).unwrap();
        reader.claim(ConsumerKind::STEAMVR_DRIVER, monotonic_now_ns()).unwrap();
        let (_, active) = call(&ctx, Method::GET, "/api/v1/compensation/setup", None).await;
        assert_eq!(active["steamvr"]["active"], true);
        assert_eq!(active["steamvr"]["restart_needed"], false);
        for component in ["steamvr", "openxr"] {
            let response = crate::api::direct::call(
                ctx.clone(),
                openrigcue_ui::backend::Method::Post,
                &format!("compensation/setup/{component}/uninstall"),
                axum::body::Bytes::from_static(b"{}"),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
        }
        ctx.compensation.initialize_native_integrations();
        let (_, disabled) = call(&ctx, Method::GET, "/api/v1/compensation/setup", None).await;
        assert_eq!(disabled["openxr"]["installed"], true);
        assert_eq!(disabled["openxr"]["registered"], false);
        assert_eq!(disabled["steamvr"]["registered"], false);
        assert_eq!(disabled["steamvr"]["installed"], true);
        let (status, enabled) = call(&ctx, Method::POST, "/api/v1/compensation/setup/openxr", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(enabled["openxr"]["registered"], true);
        assert_eq!(enabled["openxr"]["installed"], true);
    }

    #[test]
    fn native_vr_status_does_not_wait_for_imu_io() {
        use openrigcue_effects::accessories::status::{Connection, DeviceStatus};
        use std::sync::mpsc;
        use std::time::Duration;

        let tmp = tempfile::tempdir().unwrap();
        let ctx = context(&tmp);
        let mut imu = ctx.accessories.imu.lock().unwrap();
        imu.set_port("/dev/fake-imu".into());
        imu.set_connection(Connection::Offline(DeviceStatus::Progress("Reading".into())));
        let sensor = ctx.accessories.motion_sensor_values();
        sensor.set("pitch", LegacyValue::Float(12.5));
        sensor.set("raw_pitch", LegacyValue::Float(-3.0));
        sensor.set("calculated_pitch", LegacyValue::Float(4.5));
        sensor.set("yaw", LegacyValue::Int(7));
        let reader = ctx.clone();
        let (send, receive) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            send.send(status_view(&reader.state, &reader.accessories, &reader.compensation)).unwrap();
        });
        let result = receive.recv_timeout(Duration::from_secs(2));
        drop(imu);
        worker.join().unwrap();
        let status = result.expect("VR status must remain readable during device I/O");
        assert_eq!(status["imu"]["connection"], "Reading");
        assert_eq!(status["imu"]["port"], "/dev/fake-imu");
        assert_eq!(status["imu"]["angles"], Value::Null);
        assert_eq!(status["motion_sensor"]["pitch"], json!({"value": 12.5, "raw": -3.0, "calculated": 4.5}));
        assert_eq!(status["motion_sensor"]["yaw"]["value"], 7.0);
    }
}
