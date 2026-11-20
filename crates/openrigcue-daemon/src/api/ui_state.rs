//! Service state the web UI shows or changes beyond the tuning documents: device details for the hardware
//! status box and the Hardware page dialogs, the Yaw VR, Thanos and speaker-layout actions, the profile edit
//! context, the language, the log level and the factory reset.

use std::sync::{Arc, Mutex, PoisonError};

use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use openrigcue_config::settings::paths::ConfigDir;
use openrigcue_config::settings::{Configuration, Settings};
use openrigcue_config::tunes::{ChannelEffectLevels, SpeakerLayout};
use openrigcue_devices::motion::thanos::protocol::{Field, FieldValue};
use openrigcue_devices::motion::thanos::{PendingChange, Role};
use openrigcue_devices::motion::yaw_vr::protocol::YawLimits;
use openrigcue_devices::motion::LinkStatus;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{ApiContext, ApiError};
use crate::accessories::homing_of;
use crate::i18n::{LocaleFiles, Translator};
use crate::runtime::{Controls, ProfileEdit};
use crate::state::{LayoutEdit, ServiceEvent};

type ApiResult<T> = Result<T, ApiError>;

/// Applies `APP.log_level` (a numeric level: 10 Debug … 50 Critical) to the service's log output.
pub type LogLevelHook = Arc<dyn Fn(u32) + Send + Sync>;
/// Stops the service and starts it again on the same options.
pub type RestartHook = Arc<dyn Fn() + Send + Sync>;
/// The desktop window's two blocks of settings assignments made before Save.
pub type GeometryHook = Arc<dyn Fn() -> Option<openrigcue_model::native_value::Dict> + Send + Sync>;

/// The language state and the hooks into the process that runs the service.
pub struct UiServices {
    translator: Mutex<Translator>,
    log_level: Option<LogLevelHook>,
    restart: Option<RestartHook>,
    geometry: Option<GeometryHook>,
    /// Differs for every start of the service, so a page can tell that a restart happened (`instance` in the
    /// status) even when it never saw the service down.
    instance: String,
}

/// A value that differs for every [`UiServices`] made: the start time and a per-process count.
fn new_instance() -> String {
    static STARTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("{nanos:x}-{}", STARTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

impl UiServices {
    /// The translator starts with the locale and fallback set from `APP.language` (no language is selected
    /// yet). The configured log level is applied once the page is shown ([`app_loaded`]).
    pub fn new(settings: &Settings, log_level: Option<LogLevelHook>, restart: Option<RestartHook>) -> UiServices {
        UiServices {
            translator: Mutex::new(Translator::new(LocaleFiles::Builtin, &settings.app.language)),
            log_level,
            restart,
            geometry: None,
            instance: new_instance(),
        }
    }

    /// English (the built-in files), no hooks (tests and embedded use).
    pub fn off() -> UiServices {
        UiServices {
            translator: Mutex::new(Translator::new(LocaleFiles::Builtin, "en")),
            log_level: None,
            restart: None,
            geometry: None,
            instance: new_instance(),
        }
    }

    /// This start of the service (see the field).
    pub fn instance(&self) -> &str {
        &self.instance
    }

    pub fn with_geometry_hook(mut self, geometry: Option<GeometryHook>) -> Self {
        self.geometry = geometry;
        self
    }

    /// Lays the locale files of `folder` (`locales/` in the configuration directory) over the built-in ones,
    /// key by key; the folder need not exist and is never written.
    pub fn with_locale_folder(mut self, folder: std::path::PathBuf) -> Self {
        self.translator.get_mut().unwrap_or_else(PoisonError::into_inner).set_files(LocaleFiles::User(folder));
        self
    }

    pub fn capture_window_geometry(&self, state: &crate::state::ServiceState) {
        if let Some(capture) = &self.geometry {
            // Query the desktop window's main thread before locking config: its loaded and
            // closed callbacks also read config and must be free to complete.
            if let Some(assignments) = capture() {
                use openrigcue_model::native_value::LegacyValue;
                state.config.write().update(|config| {
                    config.settings.edit_native(|raw| {
                        if let Some(position) = assignments.get("APP").and_then(LegacyValue::as_dict) {
                            if let Some(app) = raw.get_mut("APP").and_then(LegacyValue::as_dict_mut) {
                                for key in ["window_x_pos", "window_y_pos"] {
                                    if let Some(value) = position.get(key) {
                                        app.insert(key, value.clone());
                                    }
                                }
                            }
                        }
                        if let Some(monitors) = assignments.get("MONITORS") {
                            raw.insert("MONITORS", monitors.clone());
                        }
                    })
                });
            }
        }
    }

    fn translator(&self) -> std::sync::MutexGuard<'_, Translator> {
        self.translator.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Translates `text` into the selected language, for texts the service publishes (the HUD's
    /// flashing status).
    pub fn translate(&self, text: &str) -> String {
        self.translator().translate(text)
    }

    /// Translates a failure or status text of the service into the selected language: a sentence that carries
    /// values ([`crate::i18n::sentence`]) is looked up and filled in, any other text is looked up as it is.
    pub fn translate_sentence(&self, message: &str) -> String {
        self.translator().translate_sentence(message)
    }

    /// Translates a help page into the selected language for the Instructions window.
    /// `None` where the page cannot be translated.
    pub fn translate_html(&self, html: &str) -> Option<String> {
        self.translator().translate_html(html)
    }
}

/// The routes of this module.
pub fn router() -> Router<ApiContext> {
    Router::new()
        .route("/api/v1/i18n", get(selected_language).put(select_language))
        .route("/api/v1/i18n/reapply", post(reapply_language))
        .route("/api/v1/motion/center", post(center_yaw_vr))
        .route("/api/v1/motion/yaw-vr/limits", put(set_yaw_limits).patch(patch_yaw_limits))
        .route("/api/v1/motion/axis-assignment", post(set_axis_assignment))
        .route("/api/v1/motion/thanos", put(set_thanos_config))
        .route("/api/v1/motion/thanos/pending", put(set_thanos_pending))
        .route("/api/v1/tunes/apply-all", post(apply_to_all_tunes))
        .route("/api/v1/shakers/layouts", get(layouts))
        .route("/api/v1/shakers/layouts/{name}", delete(remove_layout))
        .route("/api/v1/shakers/layouts/{name}/duplicate", post(duplicate_layout))
        .route("/api/v1/shakers/speakers/{speaker}/levels", get(speaker_levels))
        .route("/api/v1/profile/edit", post(start_profile_edit).delete(clear_profile_edit))
        .route("/api/v1/factory-reset", post(factory_reset))
        .route("/api/v1/app-loaded", post(app_loaded))
}

/// The page was shown: the log output takes `APP.log_level`.
async fn app_loaded(State(ctx): State<ApiContext>) -> StatusCode {
    apply_log_level(&ctx);
    // The sound device monitor may now show its notifications.
    ctx.shakers.app_loaded();
    StatusCode::NO_CONTENT
}

/// The App page callback applies the level even when it equals the previous value.
pub(super) fn apply_log_level(ctx: &ApiContext) {
    if let Some(hook) = &ctx.ui.log_level {
        let level = ctx.state.config.read().settings().app.log_level;
        hook(level);
    }
}

// ---- run-time controls --------------------------------------------------------------------------------------

/// The run-time controls with `tests.belt_homing` read from the belt (the belt homing state, which the
/// belt controller and the UI both write).
pub fn controls_view(ctx: &ApiContext) -> Controls {
    let mut controls = ctx.state.controls.snapshot();
    let homing = ctx.accessories.belt_homing();
    controls.tests.belt_homing = crate::accessories::homing_code(homing);
    controls
}

/// After a controls patch: a `belt_homing` the UI wrote (0 when the belt switch goes off or the belt test
/// stops) reaches the belt.
pub fn controls_changed(ctx: &ApiContext, before: &Controls, after: &Controls, patch: &Value) {
    let supplied = patch.get("tests").is_some_and(|tests| tests.is_null() || tests.get("belt_homing").is_some());
    if supplied || after.tests.belt_homing != before.tests.belt_homing {
        ctx.accessories.set_belt_homing(homing_of(after.tests.belt_homing));
    }
}

/// After a settings change: a new log level is applied at once; a new speaker layout is loaded, dropping an
/// unsaved matrix edit, and surround is checked again; a new sound card is opened.
pub fn settings_changed(ctx: &ApiContext, before: &Settings, after: &Settings) {
    if after.app.log_level != before.app.log_level {
        if let Some(hook) = &ctx.ui.log_level {
            hook(after.app.log_level);
        }
    }
    if after.shakers.speaker_layout != before.shakers.speaker_layout {
        match crate::shaker_layout::load(&ctx.state) {
            Ok(()) => ctx.shakers.trigger_surround_recheck(&ctx.state),
            Err(error) => tracing::error!(%error, "get_shaker_effects"),
        }
    }
    if after.shakers.sound_card != before.shakers.sound_card {
        // A sound card change: `SHAKEBOX.soundcard` is stored, then the card is opened on the
        // handler's thread; when that fails the card stays closed.
        if let Err(error) = ctx.shakers.set_soundcard(&ctx.state) {
            tracing::error!("{error}");
        }
    }
}

// ---- device details -----------------------------------------------------------------------------------------

/// The Thanos controller's runtime state (`thanos_controller`, `thanos_firmware`, `thanos_config`,
/// `thanos_config2`).
#[derive(Serialize)]
struct ThanosView {
    controller: String,
    firmware: String,
    config: serde_json::Map<String, Value>,
    config2: serde_json::Map<String, Value>,
}

fn config_map(
    config: &openrigcue_devices::motion::thanos::protocol::ControllerConfig,
) -> serde_json::Map<String, Value> {
    config
        .entries()
        .iter()
        .map(|(field, value)| {
            let value = match value {
                FieldValue::Int(v) => json!(v),
                FieldValue::Text(t) => json!(t),
            };
            (field.name(), value)
        })
        .collect()
}

#[derive(Serialize)]
struct YawLimitsView {
    power: i64,
    pitch_fwd_limit: i64,
    pitch_bwd_limit: i64,
    roll_limit: i64,
    yaw_limit: i64,
    yaw_limit_enabled: i64,
    vibration_enabled: i64,
}

impl From<YawLimits> for YawLimitsView {
    fn from(l: YawLimits) -> Self {
        YawLimitsView {
            power: l.power,
            pitch_fwd_limit: l.pitch_forward,
            pitch_bwd_limit: l.pitch_backward,
            roll_limit: l.roll,
            yaw_limit: l.yaw,
            yaw_limit_enabled: l.yaw_limit_enabled,
            vibration_enabled: l.vibration_enabled,
        }
    }
}

/// The driver state the status box, the Motion page and the Hardware page dialogs read every tick.
pub fn device_details(ctx: &ApiContext) -> Value {
    let drivers = ctx.motion.driver_settings();
    let thanos = drivers.thanos_shared.get().controller;
    let yaw = drivers.yaw_shared.get();
    json!({
        "thanos": ThanosView {
            controller: thanos.controller.clone(),
            firmware: thanos.firmware.clone(),
            config: config_map(&thanos.main_config),
            config2: config_map(&thanos.secondary_config),
        },
        "yaw_vr": {
            "centered": yaw.centered,
            "device_model": yaw.device_model,
            "limits": yaw.limits.map(YawLimitsView::from),
        },
    })
}

// ---- language -----------------------------------------------------------------------------------------------

/// The selected language, for the language menu's flag.
async fn selected_language(State(ctx): State<ApiContext>) -> Json<Value> {
    Json(json!({ "language": ctx.ui.translator().selected() }))
}

#[derive(Deserialize)]
struct LanguageChoice {
    code: String,
}

/// A choice in the language menu: `APP.language` takes the lower
/// case code and the configuration file is written; the page then re-translates.
async fn select_language(State(ctx): State<ApiContext>, Json(choice): Json<LanguageChoice>) -> ApiResult<Json<Value>> {
    set_language(&ctx, Some(choice.code)).await.map(|language| Json(json!({ "language": language })))
}

/// A left- or top-menu selection ends by setting the selected language again:
/// the selected language is applied again and the whole configuration,
/// unsaved settings edits included, is written.
async fn reapply_language(State(ctx): State<ApiContext>) -> ApiResult<StatusCode> {
    set_language(&ctx, None).await.map(|_| StatusCode::NO_CONTENT)
}

/// Sets the language to `code`; `None` sets the selected language again. Answers the selected language.
async fn set_language(ctx: &ApiContext, code: Option<String>) -> ApiResult<String> {
    let ui = Arc::clone(&ctx.ui);
    let (stored, selected) = tokio::task::spawn_blocking(move || {
        let mut translator = ui.translator();
        let code = code.unwrap_or_else(|| translator.selected().to_owned());
        let stored = translator.set_language(&code);
        (stored, translator.selected().to_owned())
    })
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?;
    {
        let mut store = ctx.state.config.write();
        store.update(|c| c.settings.app.language = stored);
        store.write_settings_native();
    }
    ctx.state.announce(ServiceEvent::Settings);
    Ok(selected)
}

// ---- Yaw VR ---------------------------------------------------------------------------------------------------

/// The Center buttons: motion homing is requested while the platform is connected.
async fn center_yaw_vr(State(ctx): State<ApiContext>) -> StatusCode {
    if ctx.state.motion.read().link.status == LinkStatus::Connected {
        ctx.motion.driver_settings().yaw_shared.update(|s| s.homing_requested = true);
    }
    StatusCode::NO_CONTENT
}

/// The Yaw VR dialog's values that its confirmation writes into the Yaw VR runtime limits (the next frame
/// sends them); the other switches of the dialog are settings.
#[derive(Deserialize)]
struct LimitsChange {
    power: i64,
    pitch_fwd_limit: i64,
    pitch_bwd_limit: i64,
    roll_limit: i64,
    yaw_limit: i64,
    yaw_limit_enabled: i64,
    vibration_enabled: i64,
}

async fn set_yaw_limits(State(ctx): State<ApiContext>, Json(change): Json<LimitsChange>) -> StatusCode {
    let limits = YawLimits {
        power: change.power,
        pitch_forward: change.pitch_fwd_limit,
        pitch_backward: change.pitch_bwd_limit,
        roll: change.roll_limit,
        yaw: change.yaw_limit,
        yaw_limit_enabled: change.yaw_limit_enabled,
        vibration_enabled: change.vibration_enabled,
    };
    ctx.motion.driver_settings().yaw_shared.update(|s| s.limits = Some(limits));
    StatusCode::NO_CONTENT
}

/// One assignment to the Yaw VR runtime limits; the constructor requires the existing limits.
pub(super) fn assign_yaw_limit(ctx: &ApiContext, key: &str, value: i64) -> Result<(), String> {
    let mut result = Ok(());
    ctx.motion.driver_settings().yaw_shared.update(|shared| {
        let Some(limits) = &mut shared.limits else {
            result = Err(format!("KeyError: {}", openrigcue_model::python_repr::repr_str(key)));
            return;
        };
        let target = match key {
            "power" => &mut limits.power,
            "pitch_fwd_limit" => &mut limits.pitch_forward,
            "pitch_bwd_limit" => &mut limits.pitch_backward,
            "roll_limit" => &mut limits.roll,
            "yaw_limit" => &mut limits.yaw,
            "yaw_limit_enabled" => &mut limits.yaw_limit_enabled,
            "vibration_enabled" => &mut limits.vibration_enabled,
            _ => {
                result = Err(format!("KeyError: {}", openrigcue_model::python_repr::repr_str(key)));
                return;
            }
        };
        *target = value;
    });
    result
}

async fn patch_yaw_limits(
    State(ctx): State<ApiContext>,
    Json(changes): Json<serde_json::Map<String, Value>>,
) -> Result<StatusCode, ApiError> {
    for (key, value) in changes {
        let value = value.as_i64().ok_or_else(|| ApiError::invalid("Yaw limit must be an integer"))?;
        assign_yaw_limit(&ctx, &key, value).map_err(ApiError::internal)?;
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---- Axis Assignment -----------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct AxisAssignmentChange {
    /// Assignment-option index → the text `<axis>,<sn>` as the dialog built it.
    assignment: std::collections::BTreeMap<String, String>,
}

/// The Axis Assignment dialog's confirmation: `MOTIONBOX.axis_assignment` (and the runtime axis assignment) take the
/// dialog's result, and the platform is marked not connected so the next pass connects with it.
async fn set_axis_assignment(
    State(ctx): State<ApiContext>,
    Json(change): Json<AxisAssignmentChange>,
) -> ApiResult<StatusCode> {
    use openrigcue_model::native_value::{Dict, LegacyValue};
    let assignment: Dict =
        change.assignment.into_iter().map(|(key, value)| (key.into(), LegacyValue::Str(value))).collect();
    {
        let mut store = ctx.state.config.write();
        let raw = store.settings().native_snapshot();
        let mut section = raw.get("MOTIONBOX").ok_or_else(|| ApiError::internal("KeyError: 'MOTIONBOX'"))?.clone();
        section
            .assign_item("axis_assignment", LegacyValue::Dict(assignment))
            .map_err(|error| ApiError::internal(format!("TypeError: {error}")))?;
        store.update(|configuration| {
            configuration.settings.edit_native(|raw| raw.insert("MOTIONBOX", section));
        });
    }
    ctx.motion.mark_disconnected(&ctx.state);
    ctx.state.announce(ServiceEvent::Settings);
    Ok(StatusCode::NO_CONTENT)
}

// ---- Thanos ---------------------------------------------------------------------------------------------------

/// The Thanos dialog's pending writes, in the order the dialog made them (one list per controller).
#[derive(Deserialize)]
struct ThanosChange {
    #[serde(default)]
    main: Vec<(String, i64)>,
    #[serde(default)]
    secondary: Vec<(String, i64)>,
}

/// A `thanos_config` key as the controller driver dispatches it; keys the dialog builds with a port index
/// outside 1…8 (which its two-controller port arithmetic can produce) have no command.
fn field_named(name: &str) -> Option<Field> {
    const FIXED: [Field; 28] = [
        Field::DataSrs,
        Field::FirmwareVersion,
        Field::BetaVersion,
        Field::Model,
        Field::Filler,
        Field::MotorNumber,
        Field::FilterFactor,
        Field::SpikeRange,
        Field::SpikeFactor,
        Field::SpikeFilterEnabled,
        Field::StrokeCm,
        Field::SpikeFactorTl,
        Field::SpikeFilterEnabledTl,
        Field::DisableParkType,
        Field::PlatformCheck,
        Field::Firmware,
        Field::MasterSlaveRole,
        Field::ParkPosition,
        Field::ParkMoveSpeed,
        Field::ParkMoveTimeout,
        Field::SixDofStartPosition,
        Field::StartMoveStep,
        Field::StartMoveTimeout,
        Field::KlmMode,
        Field::EstopMode,
        Field::SpikeMoveSpeed,
        Field::SpikeFineMoveSpeed,
        Field::ForceOfflineMode,
    ];
    if let Some(field) = FIXED.into_iter().chain([Field::PulseFrequency, Field::ButtonSpeed]).find(|f| f.name() == name)
    {
        return Some(field);
    }
    let families: [(&str, FieldFamily); 4] = [
        ("stroke", Field::Stroke),
        ("lead_screw", Field::LeadScrew),
        ("inline", Field::Inline),
        ("ratio", Field::Ratio),
    ];
    families.into_iter().find_map(|(prefix, make)| {
        let index: u8 = name.strip_prefix(prefix)?.parse().ok()?;
        (1..=8).contains(&index).then(|| make(index))
    })
}

/// A per-motor `thanos_config` key family (`stroke<n>`, …).
type FieldFamily = fn(u8) -> Field;

fn pending(entries: &[(String, i64)]) -> Vec<PendingChange> {
    let mut changes: Vec<PendingChange> = Vec::new();
    for (name, value) in entries {
        match field_named(name) {
            Some(field) => match changes.iter_mut().find(|c| c.field == field) {
                Some(change) => change.value = FieldValue::Int(*value),
                None => changes.push(PendingChange { field, value: FieldValue::Int(*value) }),
            },
            None => tracing::warn!(name, "Thanos setting without a controller command, not sent"),
        }
    }
    changes
}

#[derive(Deserialize)]
struct ThanosPendingChange {
    secondary: bool,
    replace: bool,
    entries: Vec<(String, i64)>,
}

/// The UI publishes each pending assignment before evaluating later model/configuration reads.
pub(super) fn publish_thanos_pending(ctx: &ApiContext, secondary: bool, replace: bool, entries: &[(String, i64)]) {
    let changes = pending(entries);
    ctx.motion.driver_settings().thanos_shared.update(|shared| {
        let controller = &mut shared.controller;
        let role = if secondary { Role::Secondary } else { Role::Main };
        if replace {
            if secondary {
                controller.secondary_pending = changes;
            } else {
                controller.main_pending = changes;
            }
        } else {
            for change in changes {
                controller.queue(role, change.field, change.value);
            }
        }
    });
}

async fn set_thanos_pending(
    State(ctx): State<ApiContext>,
    Json(change): Json<ThanosPendingChange>,
) -> ApiResult<StatusCode> {
    publish_thanos_pending(&ctx, change.secondary, change.replace, &change.entries);
    Ok(StatusCode::NO_CONTENT)
}

/// The Thanos dialog's confirmation: the dialog's changes become the pending configuration of each controller, with
/// `motor_number` (two controllers: 4 and `nbr_motors - 4`; one controller: `nbr_motors - 4`); the reverse
/// switches are settings the dialog writes itself.
async fn set_thanos_config(State(ctx): State<ApiContext>, Json(change): Json<ThanosChange>) -> ApiResult<StatusCode> {
    let model = ctx.state.config.read().settings().motion.model.clone();
    let motors = openrigcue_devices::motion::motion_model(&model, false)
        .map(|m| m.motors as i64)
        .ok_or_else(|| ApiError::conflict(format!("unknown motion model {model}")))?;
    let two_controllers = ctx.state.motion.read().link.ports.len() > 1;
    ctx.motion.driver_settings().thanos_shared.update(|shared| {
        let state = &mut shared.controller;
        state.main_pending = pending(&change.main);
        state.secondary_pending = pending(&change.secondary);
        if two_controllers {
            state.queue(Role::Main, Field::MotorNumber, FieldValue::Int(4));
            state.queue(Role::Secondary, Field::MotorNumber, FieldValue::Int(motors - 4));
        } else {
            state.queue(Role::Main, Field::MotorNumber, FieldValue::Int(motors - 4));
        }
    });
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum AllTunesField {
    /// "Apply Max Angle to all Games / Vehicles".
    MaxAngle,
    /// "Apply Spike Filter to all Games / Vehicles".
    SpikeFilter,
}

#[derive(Deserialize)]
struct AllTunesChange {
    field: AllTunesField,
    value: i64,
}

/// The UI's version: the getter runs at every assignment, after each row is decoded.
pub(super) fn apply_all_tunes_native(
    ctx: &ApiContext,
    spike: bool,
    read: &mut dyn FnMut() -> Result<i64, String>,
) -> Result<(), String> {
    use openrigcue_model::native_value::LegacyValue;
    let apply = |target: &mut LegacyValue, read: &mut dyn FnMut() -> Result<i64, String>| -> Result<(), String> {
        let keys: &[&str] = if spike {
            &["thanos_spike_filter_horizontal", "thanos_spike_filter_vertical"]
        } else {
            &["motion_max_power"]
        };
        for key in keys {
            let value = read()?;
            target.assign_item(key, LegacyValue::Int(value)).map_err(|detail| format!("TypeError: {detail}"))?;
        }
        Ok(())
    };
    let mut store = ctx.state.config.write();
    let rows = store.profile_rows().map_err(|error| error.native_detail())?;
    for row in rows {
        let (game, mut value) = row.map_err(|error| error.native_detail())?;
        if spike && game == "emulator" {
            continue;
        }
        apply(&mut value, read)?;
        let fields = value.as_dict_mut().expect("successful string-key assignment requires a dictionary");
        let vehicles = fields.get_mut("vehicles").ok_or("KeyError: 'vehicles'")?;
        let kind = vehicles.type_name();
        let vehicles = vehicles
            .as_dict_mut()
            .ok_or_else(|| format!("AttributeError: '{kind}' object has no attribute 'items'"))?;
        for (_, vehicle) in vehicles.iter_mut() {
            apply(vehicle, read)?;
        }
        store.stage_game_row(&game, Some(value)).map_err(|error| error.native_detail())?;
    }
    store.commit_profiles().map_err(|error| error.native_detail())
}

/// Writes the recommended value into every stored game and vehicle tune (the profile table, with a commit); the
/// spike filter skips the emulator. The tune in use is not reloaded.
async fn apply_to_all_tunes(
    State(ctx): State<ApiContext>,
    Json(change): Json<AllTunesChange>,
) -> ApiResult<StatusCode> {
    let mut store = ctx.state.config.write();
    let rows = store.profile_rows().map_err(|error| ApiError::internal(error.to_string()))?;
    for row in rows {
        let (game, mut value) = row.map_err(|error| ApiError::internal(error.to_string()))?;
        if matches!(change.field, AllTunesField::SpikeFilter) && game == "emulator" {
            continue;
        }
        let apply = |fields: &mut openrigcue_config::import::value::Dict| match change.field {
            AllTunesField::MaxAngle => fields.insert("motion_max_power", change.value.into()),
            AllTunesField::SpikeFilter => {
                fields.insert("thanos_spike_filter_horizontal", change.value.into());
                fields.insert("thanos_spike_filter_vertical", change.value.into());
            }
        };
        let fields = value.as_dict_mut().ok_or_else(|| ApiError::invalid("game profile is not a dictionary"))?;
        apply(fields);
        let vehicles = fields
            .get_mut("vehicles")
            .and_then(|vehicles| vehicles.as_dict_mut())
            .ok_or_else(|| ApiError::invalid("vehicles is missing or is not a dictionary"))?;
        for (_, vehicle) in vehicles.iter_mut() {
            apply(vehicle.as_dict_mut().ok_or_else(|| ApiError::invalid("vehicle profile is not a dictionary"))?);
        }
        store.stage_game_row(&game, Some(value)).map_err(|error| ApiError::internal(error.to_string()))?;
    }
    store.commit_profiles().map_err(|error| ApiError::internal(error.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- speaker layouts ------------------------------------------------------------------------------------------

/// The layout that is selected after a layout was removed.
pub const DEFAULT_LAYOUT: &str = "SRS ShakeKit or SRS Shakebox Elite";

#[derive(Serialize)]
struct LayoutRow {
    name: String,
    /// A built-in layout: no Remove, no custom matrix.
    builtin: bool,
    /// Whether each speaker channel of the layout is enabled.
    speakers: serde_json::Map<String, Value>,
}

fn speaker_use(layout: &SpeakerLayout) -> serde_json::Map<String, Value> {
    [
        ("FL", layout.front_left.enable),
        ("FR", layout.front_right.enable),
        ("CE", layout.center.enable),
        ("SUB", layout.subwoofer.enable),
        ("RL", layout.rear_left.enable),
        ("RR", layout.rear_right.enable),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), Value::Bool(v)))
    .collect()
}

/// The Profile dropdown: the stored layout names, sorted.
async fn layouts(State(ctx): State<ApiContext>) -> ApiResult<Json<Value>> {
    let store = ctx.state.config.read();
    let builtin = &openrigcue_config::catalog::registry::Registry::builtin().builtin_shaker_layouts;
    let table = store.metadata_table("shakers").map_err(|error| ApiError::internal(error.to_string()))?;
    let mut rows: Vec<LayoutRow> = table
        .iter()
        .filter_map(|(name, value)| {
            let layout: SpeakerLayout = openrigcue_config::import::de::from_legacy(value).ok()?;
            let name = name.to_text();
            Some(LayoutRow { builtin: builtin.contains(&name), name, speakers: speaker_use(&layout) })
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(json!({ "layouts": rows, "selected": store.settings().shakers.speaker_layout })))
}

#[derive(Deserialize)]
struct DuplicateRequest {
    name: String,
}

/// The Duplicate dialog's confirmation: an existing name is refused ("This profile name already exists, please
/// try again."); otherwise the stored layout is copied under the new name and selected (an unsaved matrix edit
/// is dropped by the reload of the layout list).
async fn duplicate_layout(
    State(ctx): State<ApiContext>,
    UrlPath(source): UrlPath<String>,
    Json(request): Json<DuplicateRequest>,
) -> ApiResult<StatusCode> {
    {
        let store = ctx.state.config.read();
        if store.metadata_contains("shakers", &request.name).map_err(|error| ApiError::internal(error.to_string()))? {
            return Err(ApiError::conflict("A profile with this name exists already; choose another name."));
        }
    }
    crate::shaker_layout::copy(&ctx.state, &source, &request.name).map_err(ApiError::internal)?;
    crate::shaker_layout::assign_profile(&ctx.state, &request.name).map_err(ApiError::internal)?;
    crate::shaker_layout::load(&ctx.state).map_err(ApiError::internal)?;
    ctx.state.announce(ServiceEvent::Settings);
    Ok(StatusCode::NO_CONTENT)
}

/// The Remove dialog's confirmation: the layout is deleted and `SRS ShakeKit or SRS Shakebox Elite` selected.
async fn remove_layout(State(ctx): State<ApiContext>, UrlPath(name): UrlPath<String>) -> ApiResult<StatusCode> {
    crate::shaker_layout::remove(&ctx.state, &name).map_err(|error| {
        if error.starts_with("KeyError:") {
            ApiError::not_found(error)
        } else {
            ApiError::internal(error)
        }
    })?;
    crate::shaker_layout::assign_profile(&ctx.state, DEFAULT_LAYOUT).map_err(ApiError::internal)?;
    crate::shaker_layout::load(&ctx.state).map_err(ApiError::internal)?;
    ctx.state.announce(ServiceEvent::Settings);
    Ok(StatusCode::NO_CONTENT)
}

/// The typed view of the loaded speaker layout.
pub fn active_layout(ctx: &ApiContext) -> Option<(String, SpeakerLayout)> {
    let store = ctx.state.config.read();
    let name = store.settings().shakers.speaker_layout.clone();
    openrigcue_config::import::de::from_legacy(&ctx.state.shaker_layout.read().value).ok().map(|layout| (name, layout))
}

/// The speaker channel of a layout by its API name.
pub fn channel_mut<'a>(layout: &'a mut SpeakerLayout, speaker: &str) -> Option<&'a mut ChannelEffectLevels> {
    Some(match speaker {
        "FL" | "fl" => &mut layout.front_left.sfx,
        "FR" | "fr" => &mut layout.front_right.sfx,
        "CE" | "ce" => &mut layout.center.sfx,
        "SUB" | "sub" => &mut layout.subwoofer.sfx,
        "RL" | "rl" => &mut layout.rear_left.sfx,
        "RR" | "rr" => &mut layout.rear_right.sfx,
        _ => return None,
    })
}

/// The custom matrix dialog opens with the speaker's levels.
async fn speaker_levels(
    State(ctx): State<ApiContext>,
    UrlPath(speaker): UrlPath<String>,
) -> ApiResult<Json<ChannelEffectLevels>> {
    let (_, mut layout) = active_layout(&ctx).ok_or_else(|| ApiError::not_found("no speaker layout selected"))?;
    let levels =
        channel_mut(&mut layout, &speaker).ok_or_else(|| ApiError::not_found(format!("no speaker {speaker}")))?;
    Ok(Json(levels.clone()))
}

/// The custom matrix dialog's confirmation: the speaker's levels replace the edited copy's, `accel`/`brake`
/// become the larger of their corners, and the channel groups are rebuilt. Saved with the profile.
pub fn edit_speaker_levels(
    ctx: &ApiContext,
    speaker: &str,
    levels: &std::collections::BTreeMap<String, f64>,
) -> ApiResult<()> {
    let (name, mut layout) = active_layout(ctx).ok_or_else(|| ApiError::not_found("no speaker layout selected"))?;
    let channel =
        channel_mut(&mut layout, speaker).ok_or_else(|| ApiError::not_found(format!("no speaker {speaker}")))?;
    let mut json = serde_json::to_value(&*channel).map_err(|e| ApiError::internal(e.to_string()))?;
    for (key, value) in levels {
        let slot = json.get_mut(key).ok_or_else(|| ApiError::not_found(format!("no channel group {key}")))?;
        *slot = json!(value);
    }
    let mut edited: ChannelEffectLevels = serde_json::from_value(json).map_err(|e| ApiError::invalid(e.to_string()))?;
    edited.accel = python_max(edited.accel_lr, edited.accel_rr);
    edited.brake = python_max(edited.brake_lf, edited.brake_rf);
    *channel = edited;
    {
        use openrigcue_config::import::{reconcile, value::LegacyValue};
        let mut loaded = ctx.state.shaker_layout.write();
        let before: SpeakerLayout = openrigcue_config::import::de::from_legacy(&loaded.value)
            .map_err(|error| ApiError::internal(error.to_string()))?;
        reconcile(
            &mut loaded.value,
            &LegacyValue::from(serde_json::to_value(before).map_err(|error| ApiError::internal(error.to_string()))?),
            &LegacyValue::from(serde_json::to_value(&layout).map_err(|error| ApiError::internal(error.to_string()))?),
        );
    }
    *ctx.state.layout_edit.write() = Some(LayoutEdit { name, layout });
    ctx.shakers
        .load_channel_groups(&ctx.state)
        .map_err(|error| ApiError::internal(format!("{}: {}", error.class, error.message)))
}

/// The larger of two values: `b` only when it compares greater (so `a` when either is NaN).
fn python_max(a: f64, b: f64) -> f64 {
    if b > a {
        b
    } else {
        a
    }
}

/// Profile Save always assigns the selected
/// raw loaded layout under the current profile name, including when no matrix
/// edit is pending. A failed write keeps the edit for the next Save.
///
/// The configuration lock is taken before the edit's, the order of [`active_layout`].
pub fn store_shaker_effects(ctx: &ApiContext) -> ApiResult<()> {
    let mut store = ctx.state.config.write();
    let mut pending = ctx.state.layout_edit.write();
    let name = crate::shaker_layout::selected_name(&store).map_err(ApiError::internal)?;
    let layout = ctx.state.shaker_layout.read().value.clone();
    store.set_shaker_entry(&name, layout).map_err(|error| ApiError::internal(error.to_string()))?;
    *pending = None;
    Ok(())
}

// ---- profile edit context -------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct EditRequest {
    game: String,
    /// `None` for the game row.
    vehicle: Option<String>,
}

/// Starts an edit context: edit mode on, `game` as the forced game, `vehicle` as the vehicle name.
pub async fn begin_edit(ctx: &ApiContext, game: String, vehicle: Option<String>) -> ApiResult<()> {
    let value = vehicle
        .map_or(openrigcue_model::native_value::LegacyValue::Int(0), openrigcue_model::native_value::LegacyValue::Str);
    begin_edit_native(ctx, game, value).await
}

/// Import confirmation assigns the vehicle named in the file to the shared edit context.
pub async fn begin_edit_native(
    ctx: &ApiContext,
    game: String,
    value: openrigcue_model::native_value::LegacyValue,
) -> ApiResult<()> {
    let vehicle = match openrigcue_config::profiles::Vehicle::from_native(&value) {
        openrigcue_config::profiles::Vehicle::Unknown => None,
        openrigcue_config::profiles::Vehicle::Named(name) => Some(name),
    };
    let without_listener = ctx.state.session.read().listener.is_none();
    {
        let mut controls = ctx.state.controls.write();
        controls.forced_game = Some(game.clone());
        controls.profile_edit =
            Some(ProfileEdit { game, vehicle: vehicle.clone(), started_without_listener: without_listener });
    }
    ctx.telemetry.publish_vehicle_value(&ctx.state, value, vehicle);
    Ok(())
}

/// The Profile Manager's Edit button.
async fn start_profile_edit(State(ctx): State<ApiContext>, Json(request): Json<EditRequest>) -> ApiResult<StatusCode> {
    begin_edit(&ctx, request.game, request.vehicle).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Ends the profile edit context: the edit and the forced game end; an edit started and ended with no listener
/// also returns to stand-by (no running game, no vehicle, no previous vehicle).
async fn clear_profile_edit(State(ctx): State<ApiContext>) -> ApiResult<StatusCode> {
    clear_edit_native(&ctx).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn clear_edit_native(ctx: &ApiContext) -> ApiResult<bool> {
    let edit = ctx.state.controls.read().profile_edit.clone();
    let listener_idle = ctx.state.session.read().listener.is_none();
    {
        let mut controls = ctx.state.controls.write();
        controls.forced_game = None;
        controls.profile_edit = None;
    }
    let started_without = edit.as_ref().map_or(listener_idle, |e| e.started_without_listener);
    let reset = edit.is_some() && started_without && ctx.state.session.read().listener.is_none();
    if reset {
        ctx.telemetry.publish_game_value(&ctx.state, None);
        ctx.telemetry.publish_vehicle_value(&ctx.state, openrigcue_model::native_value::LegacyValue::Int(0), None);
        let main_loop = ctx.main_loop.clone();
        super::routes::blocking(move || main_loop.lock().unwrap_or_else(PoisonError::into_inner).forget_vehicle())
            .await?;
    }
    Ok(reset)
}

/// The vehicle an imported file names, for the edit context of an import in stand-by.
pub fn imported_vehicle(target: &openrigcue_config::profiles::SaveTarget) -> Option<String> {
    match target {
        openrigcue_config::profiles::SaveTarget::Vehicle(name) => Some(name.clone()),
        openrigcue_config::profiles::SaveTarget::Game => None,
        openrigcue_config::profiles::SaveTarget::NativeVehicle(key) => {
            key.to_value().python_string().and_then(|value| value.scalar_string())
        }
    }
}

// ---- factory reset ------------------------------------------------------------------------------------------

/// The factory reset: the stored configuration files and the accessory port cache are deleted
/// ([`delete_configuration`]), then the service restarts on the first-run configuration.
/// `vr-compensation.toml` is kept.
async fn factory_reset(State(ctx): State<ApiContext>) -> ApiResult<StatusCode> {
    {
        let mut store = ctx.state.config.write();
        store.close_opened_files();
        if let Some(dir) = store.dir().cloned() {
            delete_configuration(&dir);
        }
        store.update(|c| *c = Configuration::first_run());
        store.reset_disk_initialization();
    }
    match &ctx.ui.restart {
        Some(restart) => {
            let restart = Arc::clone(restart);
            tokio::task::spawn_blocking(move || restart());
        }
        None => {
            ctx.state.request_profile_reload();
            ctx.state.announce(ServiceEvent::Settings);
        }
    }
    Ok(StatusCode::ACCEPTED)
}

/// Removes the files a factory reset deletes: every one is tried, and one that cannot be
/// removed is reported and skipped; the reset goes on.
pub fn delete_configuration(dir: &ConfigDir) {
    let port_cache = dir.root().join(crate::accessories::PORT_CACHE_FILE);
    for file in [
        dir.settings_file(),
        dir.old_settings_file(),
        dir.root().join("config.db"),
        dir.profiles_file(),
        dir.metadata_file(),
        dir.shaker_layouts_file(),
        dir.joysticks_file(),
        dir.state_file(),
        port_cache,
    ] {
        if let Err(e) = std::fs::remove_file(&file) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!("Cannot remove file {}: {e}", file.display());
            }
        }
    }
    let tunes = dir.tunes_dir();
    if let Err(e) = std::fs::remove_dir_all(&tunes) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!("Cannot remove file {}: {e}", tunes.display());
        }
    }
}

/// Direct UI operations over the same service functions as the test route adapters.
pub(crate) async fn direct_call(
    ctx: crate::api::ApiContext,
    call: &crate::api::direct::Call<'_>,
) -> Result<Option<axum::response::Response>, crate::api::ApiError> {
    use openrigcue_ui::backend::Method;
    let response = match (call.method, call.segments.as_slice()) {
        (Method::Get, ["i18n"]) => axum::response::IntoResponse::into_response(selected_language(State(ctx)).await),
        (Method::Put, ["i18n"]) => {
            axum::response::IntoResponse::into_response(select_language(State(ctx), call.json()?).await)
        }
        (Method::Post, ["i18n", "reapply"]) => {
            axum::response::IntoResponse::into_response(reapply_language(State(ctx)).await)
        }
        (Method::Post, ["motion", "center"]) => {
            axum::response::IntoResponse::into_response(center_yaw_vr(State(ctx)).await)
        }
        (Method::Put, ["motion", "yaw-vr", "limits"]) => {
            axum::response::IntoResponse::into_response(set_yaw_limits(State(ctx), call.json()?).await)
        }
        (Method::Patch, ["motion", "yaw-vr", "limits"]) => {
            axum::response::IntoResponse::into_response(patch_yaw_limits(State(ctx), call.json()?).await)
        }
        (Method::Post, ["motion", "axis-assignment"]) => {
            axum::response::IntoResponse::into_response(set_axis_assignment(State(ctx), call.json()?).await)
        }
        (Method::Put, ["motion", "thanos"]) => {
            axum::response::IntoResponse::into_response(set_thanos_config(State(ctx), call.json()?).await)
        }
        (Method::Put, ["motion", "thanos", "pending"]) => {
            axum::response::IntoResponse::into_response(set_thanos_pending(State(ctx), call.json()?).await)
        }
        (Method::Post, ["tunes", "apply-all"]) => {
            axum::response::IntoResponse::into_response(apply_to_all_tunes(State(ctx), call.json()?).await)
        }
        (Method::Get, ["shakers", "layouts"]) => axum::response::IntoResponse::into_response(layouts(State(ctx)).await),
        (Method::Delete, ["shakers", "layouts", parameter0]) => axum::response::IntoResponse::into_response(
            remove_layout(State(ctx), UrlPath(call.parameter(parameter0)?)).await,
        ),
        (Method::Post, ["shakers", "layouts", parameter0, "duplicate"]) => axum::response::IntoResponse::into_response(
            duplicate_layout(State(ctx), UrlPath(call.parameter(parameter0)?), call.json()?).await,
        ),
        (Method::Get, ["shakers", "speakers", parameter0, "levels"]) => axum::response::IntoResponse::into_response(
            speaker_levels(State(ctx), UrlPath(call.parameter(parameter0)?)).await,
        ),
        (Method::Post, ["profile", "edit"]) => {
            axum::response::IntoResponse::into_response(start_profile_edit(State(ctx), call.json()?).await)
        }
        (Method::Delete, ["profile", "edit"]) => {
            axum::response::IntoResponse::into_response(clear_profile_edit(State(ctx)).await)
        }
        (Method::Post, ["factory-reset"]) => {
            axum::response::IntoResponse::into_response(factory_reset(State(ctx)).await)
        }
        (Method::Post, ["app-loaded"]) => axum::response::IntoResponse::into_response(app_loaded(State(ctx)).await),
        _ => return Ok(None),
    };
    Ok(Some(response))
}
