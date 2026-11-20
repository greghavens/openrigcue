//! The API routes and their handlers.

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use openrigcue_config::catalog::Catalog;
use openrigcue_config::import::migrate::Platform as ImportPlatform;
use openrigcue_config::profiles::{
    export_all_rows, export_profile_from_lookup, read_profile_file, ExportMetadata, SaveTarget, Vehicle,
};
use openrigcue_config::settings::Settings;
use openrigcue_config::tunes::FactoryTunes;
use openrigcue_effects::shakers::{Effect, Speaker};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{patched, ApiContext, ApiError};
use crate::accessories::{accessory_status, hardware_test, HardwareTest};
use crate::games::{self, GamesPage};
use crate::profile::session_vehicle;
use crate::runtime::{Controls, Shared};
use crate::state::ServiceEvent;

type ApiResult<T> = Result<T, ApiError>;

/// Runs `work` on the UI main loop state, off the async workers.
async fn with_main_loop<R: Send + 'static>(
    ctx: &ApiContext,
    work: impl FnOnce(&mut crate::profile::MainLoop) -> R + Send + 'static,
) -> ApiResult<R> {
    let main_loop = ctx.main_loop.clone();
    blocking(move || work(&mut main_loop.lock().unwrap_or_else(std::sync::PoisonError::into_inner))).await
}

/// Runs `work` off the async workers: the dispatcher and the main loop are held by their threads for a whole
/// tick, which may wait on a listener's socket timeout.
pub(super) async fn blocking<R: Send + 'static>(work: impl FnOnce() -> R + Send + 'static) -> ApiResult<R> {
    tokio::task::spawn_blocking(work).await.map_err(|e| ApiError::internal(e.to_string()))
}

/// What the network listener serves: the UI page, its websocket and the static files (the only network
/// service besides the game listeners). The UI calls backend functions directly.
pub fn server_router(
    context: ApiContext,
    stop: openrigcue_ui::host::ServerStop,
) -> (Router, openrigcue_ui::host::UiHost) {
    super::ui_mount::router(context, stop)
}

/// HTTP adapters used by service-contract tests; these routes are not mounted by the product's network server.
pub fn router(context: ApiContext) -> Router {
    Router::new()
        .route("/api/v1/status", get(status))
        .route("/api/v1/start-info", post(take_start_info))
        .route("/api/v1/telemetry", get(telemetry))
        .route("/api/v1/telemetry/table", get(telemetry_table))
        .route("/api/v1/telemetry/export.csv", get(telemetry_csv))
        .route("/api/v1/telemetry/snapshot", post(telemetry_snapshot))
        .route("/api/v1/mobile-companion", post(mobile_companion))
        .route("/api/v1/telemetry/reset", post(reset_extremes))
        .route("/api/v1/telemetry/history", get(history).delete(clear_history))
        .route("/api/v1/intellitune", get(intellitune).delete(take_intellitune))
        .route("/api/v1/controls", get(controls).patch(patch_controls))
        .route("/api/v1/settings", get(settings).put(put_settings).patch(patch_settings))
        .route("/api/v1/settings/save", post(save_settings))
        .route("/api/v1/settings/hide-standby-alert", post(hide_standby_alert))
        .route("/api/v1/profile", get(profile).patch(patch_profile))
        .route("/api/v1/profile/save", post(save_profile))
        .route("/api/v1/profile/cancel", post(cancel_profile))
        .route("/api/v1/profile/reset", post(reset_profile))
        .route("/api/v1/profile/vehicle", delete(delete_vehicle_profile))
        .route("/api/v1/tunes/{game}", get(game_tune).put(put_game_tune).delete(reset_game_tune))
        .route(
            "/api/v1/tunes/{game}/vehicles/{vehicle}",
            get(vehicle_tune).put(put_vehicle_tune).delete(delete_vehicle_tune),
        )
        .route("/api/v1/profiles", get(profile_manager))
        .route("/api/v1/profiles/export", get(export_one))
        .route("/api/v1/profiles/export-all", get(export_everything))
        .route("/api/v1/profiles/import", post(import_profile))
        .route("/api/v1/profiles/import-check", post(check_import))
        .route("/api/v1/catalog/games", get(catalog_games))
        .route("/api/v1/games", get(games_table))
        .route("/api/v1/games/{key}/patch", post(patch_game))
        .route("/api/v1/games/{key}/port", put(set_game_port))
        .route("/api/v1/games/{key}/instructions", get(game_instructions))
        .route("/api/v1/games/detection", put(set_detection))
        .route("/api/v1/games/bridge-path", get(bridge_path))
        .route("/api/v1/notifications", get(notifications))
        .route("/api/v1/notifications/{id}", delete(dismiss_notification))
        .route("/api/v1/notifications/{id}/hide", post(hide_notification))
        .route("/api/v1/notifications/{id}/confirm", post(confirm_notification))
        .route("/api/v1/hardware", get(hardware))
        .route("/api/v1/hardware/test/{target}", post(run_hardware_test))
        .route("/api/v1/motion/test", put(set_motion_sliders))
        .route("/api/v1/motion/test/toggle", post(toggle_motion_test))
        .route("/api/v1/motion/disconnect", post(disconnect_motion))
        .route("/api/v1/shakers/soundcard", put(set_soundcard))
        .route("/api/v1/shakers/refresh-soundcard", delete(clear_soundcard_refresh))
        .route("/api/v1/srs-import", post(import_srs_files))
        .route("/api/v1/audio/fix/{action}", post(fix_soundcard))
        .route("/api/v1/shakers/test/{effect}", post(test_shaker_effect))
        .route("/api/v1/shakers/speakers/{speaker}/test", post(test_speaker))
        .route("/api/v1/shakers/speakers/{speaker}/matrix", put(set_speaker_matrix))
        .route("/api/v1/belt/homing", post(request_belt_homing))
        .merge(crate::compensation::routes::router())
        .merge(crate::voice::api_routes())
        .merge(super::ui_state::router())
        .merge(super::joystick::routes())
        .merge(crate::hotkeys::routes::router())
        .with_state(context)
}

/// What the sidebar and the profile box show.
async fn status(State(ctx): State<ApiContext>) -> ApiResult<Json<Value>> {
    let session = ctx.state.session.snapshot();
    let current = session.vehicle_value();
    let shown_vehicle = ctx.main_loop.displayed_native_vehicle(&current);
    // Copied out first: the profile lock is released before the configuration lock below is taken, the
    // order the profile save takes them in (configuration, then profile).
    let profile = {
        let profile = ctx.state.profile.read();
        json!({ "game": profile.game, "vehicle_level": profile.level == openrigcue_config::profiles::ProfileLevel::Vehicle, "generation": profile.generation, "load_generation": profile.load_generation })
    };
    Ok(Json(json!({
        "session": session,
        "displayed_vehicle": match shown_vehicle { Vehicle::Named(name) => Some(name), Vehicle::Unknown => None },
        "profile": profile,
        "controls": super::ui_state::controls_view(&ctx),
        "motion": *ctx.state.motion.read(),
        "devices": super::ui_state::device_details(&ctx),
        "accessories": accessory_status(&ctx.accessories),
        "shakers": ctx.shakers.status(),
        "platform": ctx.state.platform,
        "host_ip": ctx.host_ip,
        "instance": ctx.ui.instance(),
        "documents": ctx.state.config.read().dir().map(|d| d.root().to_path_buf()),
        "arguments": {"profile_path": ctx.state.profile_path},
    })))
}

/// What the configuration start found, taken once by the first UI main-loop pass: the reset
/// message box (`RESET_NOTICE_TITLE` / `RESET_NOTICE_TEXT`), the first-installation flag, after
/// which the UI opens Setup -> Hardware, and
/// the `--profile-path` file, which the main loop imports before its first iteration.
async fn take_start_info(State(ctx): State<ApiContext>) -> Json<Value> {
    let info = ctx.state.config.write().take_start_info();
    let reset = info.reset.is_some().then(|| {
        json!({
            "title": openrigcue_config::settings::RESET_NOTICE_TITLE,
            "text": openrigcue_config::settings::RESET_NOTICE_TEXT,
            "reason": info.reset,
        })
    });
    Json(json!({
        "first_installation": info.first_installation,
        "reset": reset,
        "profile_path": ctx.state.profile_path,
    }))
}

async fn telemetry(State(ctx): State<ApiContext>) -> Json<openrigcue_model::Telemetry> {
    Json(ctx.state.telemetry.snapshot())
}

async fn disconnect_motion(State(ctx): State<ApiContext>) -> StatusCode {
    ctx.motion.mark_disconnected(&ctx.state);
    StatusCode::NO_CONTENT
}

#[derive(Deserialize)]
struct SoundcardSelection {
    name: String,
}

async fn set_soundcard(State(ctx): State<ApiContext>, Json(body): Json<SoundcardSelection>) -> ApiResult<StatusCode> {
    blocking(move || {
        ctx.state.config.write().update(|config| config.settings.shakers.sound_card = body.name);
        ctx.shakers.set_soundcard(&ctx.state)
    })
    .await?
    .map_err(|error| ApiError::conflict(error.message()))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn clear_soundcard_refresh(State(ctx): State<ApiContext>) -> StatusCode {
    ctx.shakers.clear_soundcard_refresh();
    StatusCode::NO_CONTENT
}

#[derive(Deserialize)]
struct SrsImportSource {
    path: String,
}

/// The Hardware page's "Import" button: copies the sounds and firmware of the SRS installer (`.exe`) or
/// installation folder at `path` into the configuration folder ([`crate::import_srs_install`]) and lets "Use SRS
/// Sound Samples" read the shaker samples from that copy at once. The voice prompts and the firmware images are
/// looked up when the service starts, so those follow at the next start. A leading `~/` is the home folder.
async fn import_srs_files(State(ctx): State<ApiContext>, Json(body): Json<SrsImportSource>) -> ApiResult<StatusCode> {
    let text = body.path.trim();
    if text.is_empty() {
        return Err(ApiError::invalid("no SRS installer or installation folder given"));
    }
    let source = match (text.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => std::path::PathBuf::from(home).join(rest),
        _ => std::path::PathBuf::from(text),
    };
    let config_root = ctx
        .state
        .config
        .read()
        .dir()
        .map(|dir| dir.root().to_path_buf())
        .ok_or_else(|| ApiError::conflict("the configuration is not stored in a folder"))?;
    blocking(move || {
        // A source without SRS files is reported as a sentence the UI translates; any other failure is the path
        // and the system's error text.
        let imported = crate::import_srs_install(&source, &config_root).map_err(|error| {
            let (text, none) = (error.to_string(), crate::no_srs_files(&source));
            ApiError::invalid(if text == crate::i18n::plain(&none) {
                none
            } else {
                format!("{}: {text}", source.display())
            })
        })?;
        ctx.shakers.clone().with_srs_sounds(&ctx.state, Some(imported));
        Ok(StatusCode::NO_CONTENT)
    })
    .await?
}

/// The notification's "Fix for me" worker. The UI owns the overlay and its 30-second timeout;
/// the sound-device monitor withdraws the notice when it observes the changed configuration.
async fn fix_soundcard(Path(action): Path<String>, Json(body): Json<SoundcardSelection>) -> ApiResult<StatusCode> {
    let fix = match action.as_str() {
        "surround" => crate::sound_devices::set_soundcard_surround,
        "default" => crate::sound_devices::set_soundcard_default,
        _ => return Err(ApiError::not_found(format!("no sound-card operation {action}"))),
    };
    std::thread::Builder::new()
        .name(format!("fix_{action}_soundcard"))
        .spawn(move || {
            if let Err(error) = fix(&body.name, &mut crate::sound_devices::DeviceSources::system()) {
                tracing::error!(%error, "sound-card fix failed");
            }
        })
        .map_err(|error| ApiError::internal(error.to_string()))?;
    Ok(StatusCode::ACCEPTED)
}

/// The page reads current shared telemetry independently of the cached export table.
async fn telemetry_table(
    State(ctx): State<ApiContext>,
) -> ApiResult<Json<Vec<openrigcue_sources::derive::LiveTableRow>>> {
    let rows = blocking(move || {
        let telemetry = ctx.state.telemetry.snapshot();
        let native = ctx.state.config.read().settings().native_snapshot();
        openrigcue_sources::derive::live_table_rows(
            &telemetry,
            &ctx.telemetry.extremes_snapshot(),
            openrigcue_sources::derive::SpeedUnit::from_native_config(&native),
            ctx.telemetry.shows_all_channels(),
        )
    })
    .await?;
    Ok(Json(rows))
}

/// Telemetry page "Take CSV Snapshot": the file is
/// written to `<documents>/export`; the answer is `["OK", <path>]`,
/// `["ERROR", <text>]`, or `null` when no game runs.
async fn telemetry_snapshot(State(ctx): State<ApiContext>) -> ApiResult<Json<Option<[String; 2]>>> {
    let result = blocking(move || crate::hotkeys::snapshot::take(&ctx)).await?;
    Ok(Json(result.map(|outcome| match outcome {
        Ok(path) => ["OK".to_owned(), path.display().to_string()],
        Err(text) => ["ERROR".to_owned(), text],
    })))
}

/// The Stand By notice's "Do not display this message again": `APP.disable_tuning_alert_standby` becomes
/// `yes` in memory and in the stored file, nothing else written.
async fn hide_standby_alert(State(ctx): State<ApiContext>) -> ApiResult<StatusCode> {
    let config = ctx.state.config.clone();
    blocking(move || {
        config.write().write_native_setting(
            "APP",
            "disable_tuning_alert_standby",
            &openrigcue_model::native_value::LegacyValue::Str("yes".into()),
        )
    })
    .await?
    .map_err(ApiError::internal)?;
    Ok(StatusCode::NO_CONTENT)
}

/// The top bar's mobile button: the address a phone or tablet opens,
/// `http://<host_ip>:<webui_port>`; `<documents>/mobile` is created when missing.
async fn mobile_companion(State(ctx): State<ApiContext>) -> ApiResult<Json<String>> {
    let (port, documents) = {
        let config = ctx.state.config.read();
        (config.settings().app.web_ui_port, config.dir().map(|dir| dir.root().to_path_buf()))
    };
    if let Some(documents) = documents {
        let folder = documents.join("mobile");
        if !folder.is_dir() {
            std::fs::create_dir(&folder).map_err(|e| ApiError::internal(e.to_string()))?;
        }
    }
    Ok(Json(format!("http://{}:{port}", ctx.host_ip)))
}

/// The export table as CSV.
async fn telemetry_csv(State(ctx): State<ApiContext>) -> ApiResult<impl IntoResponse> {
    let telemetry = ctx.telemetry.clone();
    let text = blocking(move || crate::hotkeys::snapshot::contents(&telemetry.export_rows_snapshot())).await?;
    Ok(([(header::CONTENT_TYPE, "text/csv")], text))
}

/// The Telemetry page "Reset" button.
async fn reset_extremes(State(ctx): State<ApiContext>) -> ApiResult<StatusCode> {
    ctx.telemetry.reset_extremes();
    ctx.state.controls.write().telemetry.reset_min_max = true;
    Ok(StatusCode::NO_CONTENT)
}

async fn history(State(ctx): State<ApiContext>) -> ApiResult<Json<Vec<openrigcue_model::native_value::LegacyValue>>> {
    Ok(Json(ctx.telemetry.history_snapshot().into_iter().collect()))
}

async fn clear_history(State(ctx): State<ApiContext>) -> StatusCode {
    ctx.telemetry.clear_history();
    StatusCode::NO_CONTENT
}

/// The Intellitune samples captured so far, per channel.
async fn intellitune(State(ctx): State<ApiContext>) -> ApiResult<Json<Value>> {
    let telemetry = ctx.telemetry.clone();
    let samples = blocking(move || {
        serde_json::to_value(openrigcue_model::native_value::LegacyValue::Dict(
            telemetry.intellitune_snapshot().native_values().clone(),
        ))
    })
    .await?
    .map_err(|error| ApiError::internal(error.to_string()))?;
    Ok(Json(samples))
}

/// Hands the captured Intellitune samples to the wizard and starts a new capture.
async fn take_intellitune(State(ctx): State<ApiContext>) -> ApiResult<StatusCode> {
    ctx.telemetry.take_intellitune();
    Ok(StatusCode::NO_CONTENT)
}

async fn controls(State(ctx): State<ApiContext>) -> Json<Controls> {
    Json(super::ui_state::controls_view(&ctx))
}

/// The sidebar switches, test controls, Telemetry page switches and the manual game choice.
async fn patch_controls(State(ctx): State<ApiContext>, Json(patch): Json<Value>) -> ApiResult<Json<Controls>> {
    let (before, after) = edit_controls(&ctx, |before| {
        let mut after: Controls = patched(before, &patch)?;
        after.retain_test_values_after_patch(before, &patch);
        super::ui_state::controls_changed(&ctx, before, &after, &patch);
        Ok(after)
    })?;
    if after.telemetry.plot_channel != before.telemetry.plot_channel {
        ctx.telemetry.clear_history();
    }
    Ok(Json(after))
}

/// A UI field assignment must preserve updates from the hotkey and controller threads to other fields.
/// Keep the current-document read, validation and replacement within the same write lock.
fn edit_controls(
    ctx: &ApiContext,
    edit: impl FnOnce(&Controls) -> ApiResult<Controls>,
) -> ApiResult<(Controls, Controls)> {
    let mut controls = ctx.state.controls.write();
    let mut before = controls.clone();
    before.tests.belt_homing = crate::accessories::homing_code(ctx.accessories.belt_homing());
    let after = edit(&before)?;
    *controls = after.clone();
    Ok((before, after))
}

async fn settings(State(ctx): State<ApiContext>) -> ApiResult<Json<Value>> {
    Ok(Json(ctx.state.config.read().settings().ui_view().map_err(|error| ApiError::internal(error.to_string()))?))
}

async fn put_settings(State(ctx): State<ApiContext>, Json(settings): Json<Settings>) -> ApiResult<Json<Value>> {
    let before = ctx.state.config.read().settings().clone();
    ctx.state.config.write().replace_settings(settings.clone());
    super::ui_state::settings_changed(&ctx, &before, &settings);
    ctx.state.announce(ServiceEvent::Settings);
    Ok(Json(ctx.state.config.read().settings().ui_view().map_err(|error| ApiError::internal(error.to_string()))?))
}

async fn patch_settings(State(ctx): State<ApiContext>, Json(patch): Json<Value>) -> ApiResult<Json<Value>> {
    let (current, settings) = edit_settings(&ctx, |current| current.apply_ui_patch(&patch).map_err(ApiError::invalid))?;
    super::ui_state::settings_changed(&ctx, &current, &settings);
    ctx.state.announce(ServiceEvent::Settings);
    Ok(Json(settings.ui_view().map_err(|error| ApiError::internal(error.to_string()))?))
}

fn edit_settings(
    ctx: &ApiContext,
    edit: impl FnOnce(&Settings) -> ApiResult<Settings>,
) -> ApiResult<(Settings, Settings)> {
    let mut store = ctx.state.config.write();
    let current = store.settings().clone();
    let settings = edit(&current)?;
    store.update(|configuration| configuration.settings = settings.clone());
    Ok((current, settings))
}

/// The Save button for the global settings.
async fn save_settings(State(ctx): State<ApiContext>) -> ApiResult<StatusCode> {
    ctx.state.config.read().write_settings_native();
    Ok(StatusCode::NO_CONTENT)
}

async fn profile(State(ctx): State<ApiContext>) -> ApiResult<Json<Value>> {
    let profile = ctx.state.profile.read();
    let mut view = serde_json::to_value(&*profile).map_err(|e| ApiError::internal(e.to_string()))?;
    view["tune"] = profile.tune_json().map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(view))
}

/// A tuning-page change of the active tune. Effect level/switch and channel volume changes rebuild the shaker
/// channel groups; every other change is read by the next frame.
async fn patch_profile(State(ctx): State<ApiContext>, Json(patch): Json<Value>) -> ApiResult<Json<Value>> {
    let (before, tune, view) = edit_profile(&ctx, |native| apply_profile_patch(native, patch))?;
    let rebuild = tune.shaker_effects != before.shaker_effects
        || tune.shaker_switch != before.shaker_switch
        || tune.shaker_volume != before.shaker_volume;
    if rebuild {
        // The handler rebuilds the channel groups itself, before it returns.
        let (state, shakers) = (ctx.state.clone(), ctx.shakers.clone());
        blocking(move || shakers.load_channel_groups(&state))
            .await?
            .map_err(|error| ApiError::internal(format!("{}: {}", error.class, error.message)))?;
    }
    Ok(Json(view))
}

fn apply_profile_patch(native: &mut openrigcue_config::import::value::LegacyValue, patch: Value) -> ApiResult<()> {
    let mut patch = patch;
    let routines = patch.as_object_mut().and_then(|p| p.remove("routines"));
    let entries = patch.as_object().ok_or_else(|| ApiError::invalid("profile edit is not a dictionary"))?;
    if !entries.is_empty() {
        assign_profile_patch(native, &patch.into())?;
    }
    if let Some(value) = routines {
        if !matches!(native.resolved(), openrigcue_config::import::value::LegacyValue::Dict(_)) {
            return Err(ApiError::invalid("active profile is not a dictionary"));
        }
        if let Some(entries) = value.as_object().filter(|entries| !entries.is_empty()) {
            if native.item_owned("routines").is_err() {
                native
                    .assign_item("routines", openrigcue_config::import::value::Dict::new().into())
                    .map_err(ApiError::invalid)?;
            }
            let mut current = native.item_owned("routines").map_err(ApiError::invalid)?;
            if !matches!(current.resolved(), openrigcue_config::import::value::LegacyValue::Dict(_)) {
                return Err(ApiError::invalid("routines is not a dictionary"));
            }
            for (key, entry) in entries {
                current.assign_item(key, entry.clone().into()).map_err(ApiError::invalid)?;
            }
            native.assign_item("routines", current).map_err(ApiError::invalid)?;
        } else {
            native.assign_item("routines", value.into()).map_err(ApiError::invalid)?;
        }
    }
    Ok(())
}

fn edit_profile(
    ctx: &ApiContext,
    edit: impl FnOnce(&mut openrigcue_config::import::value::LegacyValue) -> ApiResult<()>,
) -> ApiResult<(openrigcue_config::tunes::Tune, openrigcue_config::tunes::Tune, Value)> {
    let mut profile = ctx.state.profile.write();
    let before = profile.tune.clone();
    let mut native = profile.native_tune();
    edit(&mut native)?;
    profile.replace_native_tune(native);
    profile.generation += 1;
    let view = profile.tune_json().map_err(|e| ApiError::internal(e.to_string()))?;
    Ok((before, profile.tune.clone(), view))
}

/// A field assignment from an in-process page handler. Null is the empty value; an empty dictionary replaces
/// the field. Nonempty dictionaries describe a path through existing parent dictionaries.
fn assign_profile_patch(
    target: &mut openrigcue_config::import::value::LegacyValue,
    patch: &openrigcue_config::import::value::LegacyValue,
) -> ApiResult<()> {
    use openrigcue_config::import::value::LegacyValue;
    let Some(changes) = patch.as_dict().filter(|changes| !changes.is_empty()) else {
        *target = patch.clone();
        return Ok(());
    };
    if !matches!(target.resolved(), LegacyValue::Dict(_)) {
        return Err(ApiError::invalid("profile assignment parent is not a dictionary"));
    }
    for (key, value) in changes.iter() {
        if let Some(nested) = value.as_dict().filter(|nested| !nested.is_empty()) {
            let mut field = target
                .item_owned(&key.to_text())
                .ok()
                .ok_or_else(|| ApiError::invalid(format!("missing profile key {key:?}")))?;
            assign_profile_patch(&mut field, &LegacyValue::Dict(nested.clone()))?;
            target.assign_item(&key.to_text(), field).map_err(ApiError::invalid)?;
        } else {
            target.assign_item(&key.to_text(), value.clone()).map_err(ApiError::invalid)?;
        }
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum SaveLevel {
    Game,
    Vehicle,
}

#[derive(Deserialize)]
struct SaveRequest {
    level: SaveLevel,
}

/// The Save button of a tuning page: the active tune at game or vehicle level.
async fn save_profile(State(ctx): State<ApiContext>, Json(request): Json<SaveRequest>) -> ApiResult<StatusCode> {
    let profile = ctx.state.profile.snapshot();
    let session = ctx.state.session.snapshot();
    let current = session.vehicle_value();
    let vehicle = with_main_loop(&ctx, move |m| m.displayed_native_vehicle(&current)).await?;
    let target = match (request.level, vehicle) {
        (SaveLevel::Game, _) => SaveTarget::Game,
        (SaveLevel::Vehicle, Vehicle::Named(name)) => SaveTarget::Vehicle(name),
        (SaveLevel::Vehicle, Vehicle::Unknown) => return Err(ApiError::conflict("no vehicle is known to save to")),
    };
    let game = profile.game.clone();
    let status = save_profile_target(&ctx, &game, target, profile)?;
    super::ui_state::store_shaker_effects(&ctx)?;
    Ok(status)
}

/// The profile save with arguments already selected by the UI. Its caller
/// updates the UI level before the subsequent speaker layout save.
pub(super) fn save_profile_target(
    ctx: &ApiContext,
    game: &str,
    target: SaveTarget,
    profile: crate::profile::ActiveProfile,
) -> ApiResult<StatusCode> {
    record_profile_hardware_preferences(ctx)?;
    let (key, _) = openrigcue_config::profiles::profile_key(game, &Vehicle::Unknown);
    let key = key.to_owned();
    {
        let mut store = ctx.state.config.write();
        let mut changed = store.config_with_profile_row(&key).map_err(|error| ApiError::internal(error.to_string()))?;
        let mut native = profile.native_tune();
        if matches!(target, SaveTarget::Game) {
            let table = changed.profiles.native_table();
            let vehicles = table
                .get(&key)
                .and_then(|game| game.item_owned("vehicles").ok())
                .ok_or_else(|| ApiError::conflict("KeyError: vehicles"))?;
            native.assign_item("vehicles", vehicles).map_err(ApiError::conflict)?;
            // The vehicles entry is assigned before committing, so even a failed commit leaves this
            // in-memory mutation in place.
            ctx.state.profile.write().replace_native_tune(native.clone());
        }
        changed.profiles.save_native(&key, &target, native).map_err(|e| ApiError::conflict(e.to_string()))?;
        store.commit_game(&key, changed).map_err(|e| ApiError::internal(e.to_string()))?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Required argument reads precede each setter, in order. Reading only
/// the selected settings/registry rows avoids decoding unrelated metadata.
fn record_profile_hardware_preferences(ctx: &ApiContext) -> ApiResult<()> {
    for (section_name, field_name, hint_name) in
        [("MOTIONBOX", "model", "MotionModel"), ("SHAKEBOX", "soundcard", "Soundcard")]
    {
        let store = ctx.state.config.read();
        let settings = store.settings().native_snapshot();
        let section = settings.get(section_name).ok_or_else(|| {
            ApiError::conflict(format!("KeyError: {}", openrigcue_model::python_repr::repr_str(section_name)))
        })?;
        let value = section.item(field_name).map_err(|error| {
            ApiError::conflict(format!(
                "{}: {error}",
                if section.as_dict().is_some() { "KeyError" } else { "TypeError" }
            ))
        })?;
        let value = value.as_str().ok_or_else(|| {
            ApiError::conflict(format!(
                "TypeError: Argument 'value' has incorrect type (expected str, got {})",
                value.type_name()
            ))
        })?;
        let _ = store.record_saved_hardware_hint(hint_name, value);
    }
    Ok(())
}

/// Cancel: reload the stored tune.
async fn cancel_profile(State(ctx): State<ApiContext>) -> ApiResult<StatusCode> {
    ctx.state.request_profile_reload();
    Ok(StatusCode::NO_CONTENT)
}

/// "Reset" of the game profile, then a reload.
async fn reset_profile(State(ctx): State<ApiContext>) -> ApiResult<StatusCode> {
    let game = ctx.state.profile.read().game.clone();
    let (key, _) = openrigcue_config::profiles::profile_key(&game, &Vehicle::Unknown);
    let key = key.to_owned();
    {
        let mut store = ctx.state.config.write();
        let mut changed = store.config_with_profile_row(&key).map_err(|error| ApiError::internal(error.to_string()))?;
        // The row is removed before the factory lookup. A missing factory leaves the deletion pending in the
        // open profile transaction; only reaching the commit persists it.
        let reset = changed.profiles.reset_game(&key, FactoryTunes::builtin());
        store
            .stage_game_row(&key, changed.profiles.native_table().get(&key).cloned())
            .map_err(|error| ApiError::internal(error.to_string()))?;
        reset.map_err(|e| ApiError::conflict(e.to_string()))?;
        store.commit_profiles().map_err(|error| ApiError::internal(error.to_string()))?;
    }
    ctx.state.request_profile_reload();
    Ok(StatusCode::NO_CONTENT)
}

/// Delete the running vehicle's tune, then a reload.
async fn delete_vehicle_profile(State(ctx): State<ApiContext>) -> ApiResult<StatusCode> {
    let profile = ctx.state.profile.snapshot();
    let removed = {
        let mut store = ctx.state.config.write();
        let (key, vehicle) = openrigcue_config::profiles::profile_key(&profile.game, &profile.vehicle);
        if vehicle.is_none() {
            false
        } else {
            let key = key.to_owned();
            let mut changed =
                store.config_with_profile_row(&key).map_err(|error| ApiError::internal(error.to_string()))?;
            let removed = changed.profiles.delete_vehicle(&profile.game, &profile.vehicle);
            if removed {
                store.commit_game(&key, changed).map_err(|e| ApiError::internal(e.to_string()))?;
            }
            removed
        }
    };
    if removed {
        // The vehicle name is cleared after a stored vehicle tune is removed.
        let telemetry = ctx.telemetry.clone();
        blocking(move || telemetry.lock().set_vehicle(None)).await?;
        ctx.state.session.write().vehicle = None;
    }
    ctx.state.request_profile_reload();
    Ok(StatusCode::NO_CONTENT)
}

async fn game_tune(State(ctx): State<ApiContext>, Path(game): Path<String>) -> ApiResult<Json<Value>> {
    let store = ctx.state.config.read();
    let mut tune = store
        .profile_row(&game)
        .map_err(|error| ApiError::internal(error.to_string()))?
        .ok_or_else(|| ApiError::not_found(format!("no tune for {game}")))?;
    tune.assign_item("vehicles", openrigcue_config::import::value::Dict::new().into()).map_err(ApiError::invalid)?;
    Ok(Json(openrigcue_config::import::de::from_legacy(&tune).map_err(|error| ApiError::internal(error.to_string()))?))
}

async fn put_game_tune(
    State(ctx): State<ApiContext>,
    Path(game): Path<String>,
    Json(tune): Json<Value>,
) -> ApiResult<StatusCode> {
    let mut store = ctx.state.config.write();
    let mut changed = store.config_with_profile_row(&game).map_err(|error| ApiError::internal(error.to_string()))?;
    changed
        .profiles
        .save_native(&game, &SaveTarget::Game, tune.into())
        .map_err(|e| ApiError::not_found(e.to_string()))?;
    store.commit_game(&game, changed).map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

/// The Profile Manager reset is one factory-row replacement, including its vehicle dictionary.
/// It does not load that tune into the active tune or run a sequence of vehicle deletions.
async fn reset_game_tune(State(ctx): State<ApiContext>, Path(game): Path<String>) -> ApiResult<StatusCode> {
    let (key, _) = openrigcue_config::profiles::profile_key(&game, &Vehicle::Unknown);
    let mut store = ctx.state.config.write();
    let mut changed = store.config_with_profile_row(key).map_err(|error| ApiError::internal(error.to_string()))?;
    let result = changed.profiles.reset_game(key, FactoryTunes::builtin());
    store
        .stage_game_row(key, changed.profiles.native_table().get(key).cloned())
        .map_err(|error| ApiError::internal(error.to_string()))?;
    result.map_err(|error| ApiError::conflict(error.to_string()))?;
    store.commit_profiles().map_err(|error| ApiError::internal(error.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn vehicle_tune(
    State(ctx): State<ApiContext>,
    Path((game, vehicle)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let store = ctx.state.config.read();
    let row = store.profile_row(&game).map_err(|error| ApiError::internal(error.to_string()))?;
    let tune = row
        .as_ref()
        .and_then(|game| game.item_owned("vehicles").ok())
        .and_then(|vehicles| vehicles.item_owned(&vehicle).ok())
        .ok_or_else(|| ApiError::not_found(format!("no tune for {game}/{vehicle}")))?;
    Ok(Json(openrigcue_config::import::de::from_legacy(&tune).map_err(|error| ApiError::internal(error.to_string()))?))
}

async fn put_vehicle_tune(
    State(ctx): State<ApiContext>,
    Path((game, vehicle)): Path<(String, String)>,
    Json(tune): Json<Value>,
) -> ApiResult<StatusCode> {
    let mut store = ctx.state.config.write();
    let mut changed = store.config_with_profile_row(&game).map_err(|error| ApiError::internal(error.to_string()))?;
    changed
        .profiles
        .save_native(&game, &SaveTarget::Vehicle(vehicle), tune.into())
        .map_err(|e| ApiError::not_found(e.to_string()))?;
    store.commit_game(&game, changed).map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_vehicle_tune(
    State(ctx): State<ApiContext>,
    Path((game, vehicle)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let mut store = ctx.state.config.write();
    let mut changed = store.config_with_profile_row(&game).map_err(|error| ApiError::internal(error.to_string()))?;
    if changed.profiles.delete_vehicle(&game, &Vehicle::Named(vehicle)) {
        store.commit_game(&game, changed).map_err(|e| ApiError::internal(e.to_string()))?;
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
struct ManagerRow {
    game: String,
    name: String,
    vehicles: Vec<openrigcue_model::native_value::PythonString>,
}

/// The Profile Manager list (the rows shown under the Windows rules).
async fn profile_manager(State(ctx): State<ApiContext>) -> ApiResult<Json<Vec<ManagerRow>>> {
    let store = ctx.state.config.read();
    let entries = store
        .profile_manager_entries(Catalog::builtin(), ImportPlatform::Windows)
        .map_err(|e| ApiError::conflict(e.to_string()))?;
    Ok(Json(
        entries
            .into_iter()
            .map(|e| ManagerRow { game: e.game.id.clone(), name: e.game.name.clone(), vehicles: e.vehicles })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct ExportQuery {
    game: String,
    vehicle: Option<String>,
    location: Option<String>,
}

fn export_metadata_parts(settings: &Settings) -> (String, String, Vec<String>) {
    (
        settings.motion.model.clone(),
        settings.shakers.speaker_layout.clone(),
        openrigcue_config::catalog::registry::Registry::builtin().builtin_shaker_layouts.clone(),
    )
}

fn attachment(name: &str, bytes: Vec<u8>) -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "application/zip".to_owned()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}\"")),
        ],
        bytes,
    )
}

/// The Profile Manager "Export" action.
async fn export_one(State(ctx): State<ApiContext>, Query(query): Query<ExportQuery>) -> ApiResult<impl IntoResponse> {
    let store = ctx.state.config.read();
    let (model, layout, builtin) = export_metadata_parts(store.settings());
    let meta = ExportMetadata { motion_model: &model, shaker_layout: &layout, builtin_layouts: &builtin };
    let vehicle = session_vehicle(query.vehicle.as_deref());
    let location =
        query.location.as_ref().map(|value| openrigcue_config::import::value::LegacyValue::Str(value.clone()));
    let (name, bytes) = export_profile_from_lookup(
        |key| {
            store.profile_row(key).map_err(|error| openrigcue_config::profiles::ProfileError::Native(error.to_string()))
        },
        Catalog::builtin(),
        &query.game,
        &vehicle,
        location.as_ref(),
        &meta,
    )
    .map_err(|e| ApiError::not_found(e.to_string()))?;
    Ok(attachment(&name, bytes))
}

/// "Export All" (`srs_export_all.zip`).
async fn export_everything(State(ctx): State<ApiContext>) -> ApiResult<impl IntoResponse> {
    let store = ctx.state.config.read();
    let (model, layout, builtin) = export_metadata_parts(store.settings());
    let meta = ExportMetadata { motion_model: &model, shaker_layout: &layout, builtin_layouts: &builtin };
    let rows = store.profile_rows().map_err(|error| ApiError::internal(error.to_string()))?;
    let bytes = export_all_rows(
        rows.map(|row| row.map_err(|error| openrigcue_config::profiles::ProfileError::Native(error.to_string()))),
        Catalog::builtin(),
        &meta,
    )
    .map_err(|error| ApiError::internal(error.to_string()))?;
    Ok(attachment("srs_export_all.zip", bytes))
}

/// The import's checks of the file: a profile, for the game and vehicle in use (or no game
/// running); the answer is the message shown when one fails.
async fn check_import(State(ctx): State<ApiContext>, body: Bytes) -> ApiResult<StatusCode> {
    let file = read_profile_file(&body).map_err(|e| ApiError::invalid(e.to_string()))?;
    let session = ctx.state.session.snapshot();
    let forced = ctx.state.controls.read().forced_game.clone();
    file.check_native(session.game_key(), &session.vehicle_value(), forced.as_deref())
        .map_err(|e| ApiError::conflict(e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

/// Profile import (the file handed over, then the confirmation): accepted while no game runs or for the game and
/// vehicle in use; with no game running the profile is opened for editing (its game becomes the forced game).
async fn import_profile(State(ctx): State<ApiContext>, body: Bytes) -> ApiResult<StatusCode> {
    let file = read_profile_file(&body).map_err(|e| ApiError::invalid(e.to_string()))?;
    let session = ctx.state.session.snapshot();
    let forced = ctx.state.controls.read().forced_game.clone();
    file.check_native(session.game_key(), &session.vehicle_value(), forced.as_deref())
        .map_err(|e| ApiError::conflict(e.to_string()))?;
    confirm_profile_import(&ctx, &file).await
}

/// The modal already checked its retained content when offering it. Confirmation
/// has no second identity check and reads standby state only after staging the row.
pub(super) async fn confirm_profile_import(
    ctx: &ApiContext,
    file: &openrigcue_config::profiles::ProfileFile,
) -> ApiResult<StatusCode> {
    {
        let mut store = ctx.state.config.write();
        let (game, _, value) = file
            .import_from_lookup(|key| {
                store.profile_row(key).map_err(|_| openrigcue_config::profiles::ProfileFileError::Unreadable)
            })
            .map_err(|error| ApiError::invalid(error.to_string()))?;
        store.stage_game_row(&game, Some(value)).map_err(|error| ApiError::internal(error.to_string()))?;
    }
    if ctx.state.session.read().game.is_none() {
        if let openrigcue_config::profiles::FileGame::Id(game) = &file.game {
            super::ui_state::begin_edit_native(ctx, game.clone(), file.vehicle_value().clone()).await?;
        }
    }
    ctx.state.request_profile_reload();
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
struct CatalogGame {
    id: String,
    name: String,
    listener: String,
    flying: bool,
    console: bool,
}

async fn catalog_games() -> Json<Vec<CatalogGame>> {
    Json(
        Catalog::builtin()
            .games()
            .iter()
            .map(|g| CatalogGame {
                id: g.id.clone(),
                name: g.name.clone(),
                listener: g.listener.clone(),
                flying: g.kind == openrigcue_config::catalog::GameKind::Flying,
                console: g.supports_console(),
            })
            .collect(),
    )
}

#[derive(Deserialize)]
struct GamesQuery {
    page: Option<GamesPage>,
}

/// The Games (`PC`) or Console page table.
async fn games_table(State(ctx): State<ApiContext>, Query(query): Query<GamesQuery>) -> ApiResult<Json<Value>> {
    let page = query.page.unwrap_or(GamesPage::Pc);
    let (ports, forwarding, native_settings) = {
        let store = ctx.state.config.read();
        (
            store.settings().network_ports.clone(),
            store.settings().port_forwarding.clone(),
            store.settings().native_snapshot(),
        )
    };
    let patch_settings = native_settings.clone();
    let main_loop = ctx.main_loop.clone();
    let rows = tokio::task::spawn_blocking(move || {
        let guard = main_loop.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.setup().load_table_native(page, &ports, &forwarding, &patch_settings)
    })
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(games_table_value(rows, &native_settings)))
}

fn games_table_value(rows: games::GamesTable, native_settings: &openrigcue_config::import::value::Dict) -> Value {
    let native_error = rows
        .native_exception(native_settings)
        .map(|(class_name, message)| json!({"class_name":class_name, "message":message}));
    json!({
        "rows": rows.rows,
        "error": rows.error.map(|(game, error)| format!("{game}: {error}")),
        "native_error": native_error,
    })
}

/// A Games-page status button of a `config`/`dll` entry.
async fn patch_game(State(ctx): State<ApiContext>, Path(key): Path<String>) -> ApiResult<StatusCode> {
    let running = ctx.state.session.read().game.is_some();
    let (ports, native) = {
        let store = ctx.state.config.read();
        (store.settings().network_ports.clone(), store.settings().native_snapshot())
    };
    let main_loop = ctx.main_loop.clone();
    tokio::task::spawn_blocking(move || {
        let guard = main_loop.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.setup().patch_native(&key, running, &ports, &native)
    })
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?
    .map_err(|e| match e {
        // Both refusals are conflicts whose text the Games page shows: a running game, or a running Steam that
        // would overwrite the launch options.
        games::GamesError::GameRunning | games::GamesError::SteamRunning => ApiError::conflict(e.to_string()),
        games::GamesError::UnknownEntry(_) => ApiError::not_found(e.to_string()),
        other => ApiError::invalid(other.to_string()),
    })?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct PortChange {
    port: u16,
    #[serde(default)]
    forwarding: Vec<String>,
}

/// The "Game UDP Listening Port" modal's confirmation: sets the port and the forwarding list in
/// memory, writes only the `PORT_FORWARDING` section to the stored file, re-patches `config` entries. Listeners
/// pick up the change after a restart. A re-patch that fails answers an error: the handler ends there,
/// so neither the table reload nor the restart alert follows.
async fn set_game_port(
    State(ctx): State<ApiContext>,
    Path(key): Path<String>,
    Json(change): Json<PortChange>,
) -> ApiResult<Json<Value>> {
    let forwarding = openrigcue_model::native_value::LegacyValue::List(
        change.forwarding.into_iter().map(openrigcue_model::native_value::LegacyValue::Str).collect(),
    );
    let restart_required = confirm_game_port(&ctx, &key, i64::from(change.port), forwarding, true).await?;
    Ok(Json(json!({ "restart_required": restart_required })))
}

/// The UI callback keeps the stored settings values through comparison and each assignment. The HTTP fixture
/// adapter additionally simulates edits to an existing shared list; the live UI has already made those edits.
pub(super) async fn confirm_game_port(
    ctx: &ApiContext,
    key: &str,
    port: i64,
    forwarding: openrigcue_model::native_value::LegacyValue,
    simulate_shared_list: bool,
) -> ApiResult<bool> {
    use openrigcue_model::{native_value::LegacyValue, python_compare::native_equal};
    fn required<'a>(value: &'a LegacyValue, key: &str) -> ApiResult<&'a LegacyValue> {
        value.item(key).map_err(|detail| {
            ApiError::invalid(format!("{}: {detail}", if value.as_dict().is_some() { "KeyError" } else { "TypeError" }))
        })
    }
    fn contains(section: &LegacyValue, key: &str) -> ApiResult<bool> {
        match section {
            LegacyValue::Dict(values) => Ok(values.contains(key)),
            LegacyValue::Str(value) => Ok(value.contains(key)),
            LegacyValue::List(values) | LegacyValue::Tuple(values) | LegacyValue::Set(values) => {
                Ok(values.iter().any(|value| native_equal(value, &LegacyValue::Str(key.into()))))
            }
            LegacyValue::Bytes(_) => Err(ApiError::invalid("TypeError: a bytes-like object is required, not 'str'")),
            value => {
                Err(ApiError::invalid(format!("TypeError: argument of type '{}' is not iterable", value.type_name())))
            }
        }
    }
    let port_key = games::port_key(key).map_err(|e| match e {
        games::GamesError::UnknownEntry(_) => ApiError::not_found(e.to_string()),
        other => ApiError::invalid(other.to_string()),
    })?;
    // The port change confirmation: nothing is saved, re-patched or announced unless the port or the forwarding
    // list differs from the configured one. The modal edits the configured list object in place, so with an
    // existing key the new list is live in memory even when nothing is written.
    let (ports, native) = {
        let mut store = ctx.state.config.write();
        let root = LegacyValue::Dict(store.settings().native_snapshot());
        let section = required(&root, "PORT_FORWARDING")?;
        let exists = contains(section, port_key)?;
        let previous = if exists { required(section, port_key)?.clone() } else { LegacyValue::List(Vec::new()) };
        let current_port = openrigcue_ui::backend::native_int(required(required(&root, "NETWORK_PORT")?, port_key)?)
            .map_err(ApiError::invalid)?;
        let saves = current_port != port || (!simulate_shared_list || !exists) && !native_equal(&forwarding, &previous);
        if !saves && !(simulate_shared_list && exists) {
            return Ok(false);
        }
        let mut raw = store.settings().native_snapshot();
        raw.get_mut("PORT_FORWARDING")
            .expect("required section")
            .assign_item(port_key, forwarding)
            .map_err(|detail| ApiError::invalid(format!("TypeError: {detail}")))?;
        store.update(|configuration| configuration.settings.edit_native(|settings| *settings = raw.clone()));
        if !saves {
            return Ok(false);
        }
        raw.get_mut("NETWORK_PORT")
            .expect("required section")
            .assign_item(port_key, LegacyValue::Int(port))
            .map_err(|detail| ApiError::invalid(format!("TypeError: {detail}")))?;
        store.update(|configuration| configuration.settings.edit_native(|settings| *settings = raw));
        // The stored file is read, its `PORT_FORWARDING` replaced, and written back; a failure is only
        // logged.
        if let Err(error) = store.write_port_forwarding() {
            tracing::error!(%error, "error while saving config file in the config_helpers");
        }
        (store.settings().network_ports.clone(), store.settings().native_snapshot())
    };
    let main_loop = ctx.main_loop.clone();
    let key = key.to_owned();
    let repatch = tokio::task::spawn_blocking(move || {
        let guard = main_loop.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.setup().repatch_after_port_change_native(&key, &ports, &native).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?;
    repatch.map_err(|e| ApiError::internal(format!("re-configuring the game failed: {e}")))?;
    Ok(true)
}

#[derive(Deserialize)]
struct InstructionsQuery {
    #[serde(default)]
    console: bool,
}

/// The in-app game instructions, from the built-in help pages.
async fn game_instructions(
    State(ctx): State<ApiContext>,
    Path(key): Path<String>,
    Query(query): Query<InstructionsQuery>,
) -> ApiResult<impl IntoResponse> {
    let page = games::instructions_native(&key, query.console, &ctx.host_ip, |port_key| {
        ctx.state.config.read().settings().native_snapshot().get("NETWORK_PORT")?.as_dict()?.get(port_key).cloned()
    })
    .map_err(|e| ApiError::not_found(e.to_string()))?;
    // The Instructions window translates the page line by line in the selected language.
    let page = ctx.ui.translate_html(&page).ok_or_else(|| ApiError::not_found(format!("instructions page {key:?}")))?;
    Ok(([(header::CONTENT_TYPE, "text/html; charset=utf-8")], page))
}

/// The Games page's launch steps of a bridge game for a launcher other than Steam: where the bridge and its
/// launch wrapper are and the launch option that starts a game through it, or the folders searched when the
/// bridge files are not found.
async fn bridge_path(State(ctx): State<ApiContext>) -> ApiResult<Json<Value>> {
    let main_loop = ctx.main_loop.clone();
    let launch = tokio::task::spawn_blocking(move || {
        let guard = main_loop.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.setup().bridge_launch()
    })
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?
    .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(bridge_path_value(launch, &games::proton_bridge::packaged_dirs())))
}

fn bridge_path_value(launch: Option<games::proton_bridge::BridgeLaunch>, searched: &[std::path::PathBuf]) -> Value {
    match launch {
        Some(launch) => json!({
            "bridge": launch.bridge.display().to_string(),
            "bridge_present": launch.bridge_present,
            "launch_helper": launch.run.display().to_string(),
            "launch_option": launch.option(),
        }),
        None => json!({
            "searched": searched.iter().map(|dir| dir.display().to_string()).collect::<Vec<_>>(),
        }),
    }
}

#[derive(Deserialize)]
struct Detection {
    /// Games-page key picked in Manual mode, `None` for Auto (or the stand-by row).
    entry: Option<String>,
}

/// Console page Auto/Manual detection (a click on a table row): a picked row forces its catalog game
/// (the key up to its first `_`), `testmode` or Auto clears the choice; picking resets the vehicle.
async fn set_detection(State(ctx): State<ApiContext>, Json(detection): Json<Detection>) -> ApiResult<StatusCode> {
    let forced =
        detection.entry.filter(|key| key != "testmode").map(|key| key.split('_').next().unwrap_or(&key).to_owned());
    ctx.state.controls.write().forced_game = forced.clone();
    if forced.is_some() {
        ctx.telemetry.publish_vehicle_value(&ctx.state, openrigcue_config::import::value::LegacyValue::Int(0), None);
        with_main_loop(&ctx, |m| m.forget_vehicle()).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn notifications(State(ctx): State<ApiContext>) -> Json<Vec<crate::notifications::Notification>> {
    Json(ctx.state.notifications.read().items.clone())
}

/// A notification's Dismiss button: setup notifications are remembered for the running game
/// (in the game notification table).
async fn dismiss_notification(State(ctx): State<ApiContext>, Path(id): Path<u64>) -> ApiResult<StatusCode> {
    let device_permissions = ctx.state.notifications.read().items.iter().any(|item| {
        item.id == id && matches!(item.kind, crate::notifications::NotificationKind::DevicePermissions { .. })
    });
    let running = ctx.state.session.read().game_key().to_owned();
    let game = ctx.state.notifications.write().dismiss(id, &running);
    if let Some(game) = game {
        if let Err(error) = ctx.state.config.write().record_notification_marker("game_config_notifications", &game) {
            tracing::error!(%error, "Error while dismissing telemetry config notification");
        }
    }
    if device_permissions {
        ctx.state
            .config
            .write()
            .record_notification_marker("notifications", "linux_device_access")
            .map_err(|error| ApiError::internal(error.to_string()))?;
    }
    ctx.state.announce(ServiceEvent::Notifications);
    Ok(StatusCode::NO_CONTENT)
}

/// A notification's first button (Game Page, Instructions, Center, Calibrate): its handler runs
/// ([`crate::profile::confirm_notification`]); the answer names the page the UI shows next, if any.
async fn confirm_notification(State(ctx): State<ApiContext>, Path(id): Path<u64>) -> ApiResult<Json<Value>> {
    let confirmed = {
        let ctx = ctx.clone();
        blocking(move || crate::profile::confirm_notification(&ctx, id)).await?
    };
    let Some(confirmed) = confirmed else {
        return Err(ApiError::not_found(format!("no notification {id}")));
    };
    ctx.state.announce(ServiceEvent::Notifications);
    Ok(Json(match confirmed {
        crate::notifications::Confirmed::GamePage => json!({ "show": "game_page" }),
        crate::notifications::Confirmed::Instructions { game } => json!({ "show": "instructions", "game": game }),
        crate::notifications::Confirmed::CenterYawVr | crate::notifications::Confirmed::CalibrateBelt => json!({}),
    }))
}

/// A notification button other than Dismiss ("Game Page", "Instructions"): the notification is hidden, nothing recorded.
async fn hide_notification(State(ctx): State<ApiContext>, Path(id): Path<u64>) -> ApiResult<StatusCode> {
    if !ctx.state.notifications.write().hide(id) {
        return Err(ApiError::not_found(format!("no notification {id}")));
    }
    ctx.state.announce(ServiceEvent::Notifications);
    Ok(StatusCode::NO_CONTENT)
}

/// The Hardware page: serial ports, sound cards, motion and accessory state, driver details.
async fn hardware(State(ctx): State<ApiContext>) -> ApiResult<Json<Value>> {
    // The serial ports (port, description, address), as the motion model change lists them.
    let ports: Vec<Value> = tokio::task::spawn_blocking(openrigcue_devices::discovery::try_comports)
        .await
        .map_err(|error| ApiError::internal(error.to_string()))?
        .map_err(|error| ApiError::internal(error.to_string()))?
        .into_iter()
        .map(|p| json!({ "device": p.device, "description": p.description, "hwid": p.hwid }))
        .collect();
    let yaw_vr = ctx.motion.driver_settings().yaw_shared.get();
    let selected = ctx.state.config.read().settings().shakers.sound_card.clone();
    let sound_devices = crate::shakers::sound_devices(&selected);
    let sound_cards: Vec<_> = sound_devices.iter().map(|device| device.id.clone()).collect();
    Ok(Json(json!({
        "serial_ports": ports,
        "sound_cards": sound_cards,
        "sound_devices": sound_devices,
        "motion": *ctx.state.motion.read(),
        "accessories": accessory_status(&ctx.accessories),
        "yaw_vr": { "devices": yaw_vr.devices.iter().map(|(ip, _)| ip.clone()).collect::<Vec<_>>(), "centered": yaw_vr.centered, "model": yaw_vr.device_model },
        "devices": super::ui_state::device_details(&ctx),
    })))
}

/// The Hardware page "Test" buttons of wind, LEDs and tachometer.
async fn run_hardware_test(State(ctx): State<ApiContext>, Path(target): Path<String>) -> ApiResult<StatusCode> {
    let target: HardwareTest =
        serde_json::from_value(Value::String(target)).map_err(|e| ApiError::not_found(e.to_string()))?;
    let controls: Shared<Controls> = ctx.state.controls.clone();
    tokio::task::spawn_blocking(move || hardware_test(&controls, target));
    Ok(StatusCode::ACCEPTED)
}

/// The motion test sliders, −100…100 each.
async fn set_motion_sliders(State(ctx): State<ApiContext>, Json(patch): Json<Value>) -> ApiResult<Json<Controls>> {
    let before = ctx.state.controls.snapshot();
    let after: Controls = patched(&before, &json!({ "tests": { "motion_sliders": patch } }))?;
    *ctx.state.controls.write() = after;
    Ok(Json(super::ui_state::controls_view(&ctx)))
}

/// "Start Test" / "Stop Test" of the motion test: toggles the motion test mode and zeroes every slider.
async fn toggle_motion_test(State(ctx): State<ApiContext>) -> Json<Controls> {
    {
        let mut controls = ctx.state.controls.write();
        controls.tests.motion_test = !controls.tests.motion_test;
        controls.reset_motion_test_sliders();
    }
    Json(super::ui_state::controls_view(&ctx))
}

fn effect_named(name: &str) -> ApiResult<Effect> {
    Effect::ALL
        .into_iter()
        .find(|e| e.key() == name)
        .ok_or_else(|| ApiError::not_found(format!("no shaker effect {name}")))
}

fn speaker_named(name: &str) -> ApiResult<Speaker> {
    match name {
        "FL" | "fl" => Ok(Speaker::FrontLeft),
        "FR" | "fr" => Ok(Speaker::FrontRight),
        "CE" | "ce" => Ok(Speaker::Center),
        "SUB" | "sub" => Ok(Speaker::Sub),
        "RL" | "rl" => Ok(Speaker::RearLeft),
        "RR" | "rr" => Ok(Speaker::RearRight),
        other => Err(ApiError::not_found(format!("no speaker {other}"))),
    }
}

/// The Shake page "Test" button of an effect: plays the effect test when a sound card
/// is selected, otherwise nothing. The handler blocks for the test (about 0.8 s).
async fn test_shaker_effect(State(ctx): State<ApiContext>, Path(effect): Path<String>) -> ApiResult<StatusCode> {
    let effect = effect_named(&effect)?;
    if !crate::shakers::effect_test_allowed(&ctx.state) {
        return Ok(StatusCode::NO_CONTENT);
    }
    let (state, shakers) = (ctx.state.clone(), ctx.shakers.clone());
    blocking(move || crate::shakers::test_effect(&state, &shakers, effect))
        .await?
        .map_err(|error| ApiError::internal(error.detail()))?;
    Ok(StatusCode::NO_CONTENT)
}

/// The Hardware page speaker test: plays the speaker test when a sound card
/// is selected, otherwise nothing.
async fn test_speaker(State(ctx): State<ApiContext>, Path(speaker): Path<String>) -> ApiResult<StatusCode> {
    let speaker = speaker_named(&speaker)?;
    if crate::shakers::speaker_test_allowed(&ctx.state) {
        crate::shakers::test_sound(&ctx.state, &ctx.shakers, speaker)
            .map_err(|error| ApiError::internal(format!("{}: {}", error.class, error.message)))?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// The custom speaker matrix dialog's confirmation: one speaker's level per channel group.
async fn set_speaker_matrix(
    State(ctx): State<ApiContext>,
    Path(speaker): Path<String>,
    Json(levels): Json<std::collections::BTreeMap<String, f64>>,
) -> ApiResult<StatusCode> {
    speaker_named(&speaker)?;
    super::ui_state::edit_speaker_levels(&ctx, &speaker, &levels)?;
    Ok(StatusCode::NO_CONTENT)
}

/// The Belt page "Calibrate" button (requests belt homing).
async fn request_belt_homing(State(ctx): State<ApiContext>) -> StatusCode {
    ctx.accessories.set_belt_homing(openrigcue_effects::accessories::belt::Homing::Requested);
    StatusCode::ACCEPTED
}

/// Direct UI operations over the same service functions as the test route adapters.
pub(crate) async fn direct_call(
    ctx: crate::api::ApiContext,
    call: &crate::api::direct::Call<'_>,
) -> Result<Option<axum::response::Response>, crate::api::ApiError> {
    use openrigcue_ui::backend::Method;
    let response = match (call.method, call.segments.as_slice()) {
        (Method::Get, ["status"]) => axum::response::IntoResponse::into_response(status(State(ctx)).await),
        (Method::Post, ["start-info"]) => {
            axum::response::IntoResponse::into_response(take_start_info(State(ctx)).await)
        }
        (Method::Get, ["telemetry"]) => axum::response::IntoResponse::into_response(telemetry(State(ctx)).await),
        (Method::Get, ["telemetry", "table"]) => {
            axum::response::IntoResponse::into_response(telemetry_table(State(ctx)).await)
        }
        (Method::Get, ["telemetry", "export.csv"]) => {
            axum::response::IntoResponse::into_response(telemetry_csv(State(ctx)).await)
        }
        (Method::Post, ["telemetry", "snapshot"]) => {
            axum::response::IntoResponse::into_response(telemetry_snapshot(State(ctx)).await)
        }
        (Method::Post, ["mobile-companion"]) => {
            axum::response::IntoResponse::into_response(mobile_companion(State(ctx)).await)
        }
        (Method::Post, ["telemetry", "reset"]) => {
            axum::response::IntoResponse::into_response(reset_extremes(State(ctx)).await)
        }
        (Method::Get, ["telemetry", "history"]) => {
            axum::response::IntoResponse::into_response(history(State(ctx)).await)
        }
        (Method::Delete, ["telemetry", "history"]) => {
            axum::response::IntoResponse::into_response(clear_history(State(ctx)).await)
        }
        (Method::Get, ["intellitune"]) => axum::response::IntoResponse::into_response(intellitune(State(ctx)).await),
        (Method::Delete, ["intellitune"]) => {
            axum::response::IntoResponse::into_response(take_intellitune(State(ctx)).await)
        }
        (Method::Get, ["controls"]) => axum::response::IntoResponse::into_response(controls(State(ctx)).await),
        (Method::Patch, ["controls"]) => {
            axum::response::IntoResponse::into_response(patch_controls(State(ctx), call.json()?).await)
        }
        (Method::Get, ["settings"]) => axum::response::IntoResponse::into_response(settings(State(ctx)).await),
        (Method::Put, ["settings"]) => {
            axum::response::IntoResponse::into_response(put_settings(State(ctx), call.json()?).await)
        }
        (Method::Patch, ["settings"]) => {
            axum::response::IntoResponse::into_response(patch_settings(State(ctx), call.json()?).await)
        }
        (Method::Post, ["settings", "save"]) => {
            axum::response::IntoResponse::into_response(save_settings(State(ctx)).await)
        }
        (Method::Post, ["settings", "hide-standby-alert"]) => {
            axum::response::IntoResponse::into_response(hide_standby_alert(State(ctx)).await)
        }
        (Method::Get, ["profile"]) => axum::response::IntoResponse::into_response(profile(State(ctx)).await),
        (Method::Patch, ["profile"]) => {
            axum::response::IntoResponse::into_response(patch_profile(State(ctx), call.json()?).await)
        }
        (Method::Post, ["profile", "save"]) => {
            axum::response::IntoResponse::into_response(save_profile(State(ctx), call.json()?).await)
        }
        (Method::Post, ["profile", "cancel"]) => {
            axum::response::IntoResponse::into_response(cancel_profile(State(ctx)).await)
        }
        (Method::Post, ["profile", "reset"]) => {
            axum::response::IntoResponse::into_response(reset_profile(State(ctx)).await)
        }
        (Method::Delete, ["profile", "vehicle"]) => {
            axum::response::IntoResponse::into_response(delete_vehicle_profile(State(ctx)).await)
        }
        (Method::Get, ["tunes", parameter0]) => {
            axum::response::IntoResponse::into_response(game_tune(State(ctx), Path(call.parameter(parameter0)?)).await)
        }
        (Method::Put, ["tunes", parameter0]) => axum::response::IntoResponse::into_response(
            put_game_tune(State(ctx), Path(call.parameter(parameter0)?), call.json()?).await,
        ),
        (Method::Delete, ["tunes", parameter0]) => axum::response::IntoResponse::into_response(
            reset_game_tune(State(ctx), Path(call.parameter(parameter0)?)).await,
        ),
        (Method::Get, ["tunes", parameter0, "vehicles", parameter1]) => axum::response::IntoResponse::into_response(
            vehicle_tune(State(ctx), Path((call.parameter(parameter0)?, call.parameter(parameter1)?))).await,
        ),
        (Method::Put, ["tunes", parameter0, "vehicles", parameter1]) => axum::response::IntoResponse::into_response(
            put_vehicle_tune(
                State(ctx),
                Path((call.parameter(parameter0)?, call.parameter(parameter1)?)),
                call.json()?,
            )
            .await,
        ),
        (Method::Delete, ["tunes", parameter0, "vehicles", parameter1]) => axum::response::IntoResponse::into_response(
            delete_vehicle_tune(State(ctx), Path((call.parameter(parameter0)?, call.parameter(parameter1)?))).await,
        ),
        (Method::Get, ["profiles"]) => axum::response::IntoResponse::into_response(profile_manager(State(ctx)).await),
        (Method::Get, ["profiles", "export"]) => {
            axum::response::IntoResponse::into_response(export_one(State(ctx), call.query()?).await)
        }
        (Method::Get, ["profiles", "export-all"]) => {
            axum::response::IntoResponse::into_response(export_everything(State(ctx)).await)
        }
        (Method::Post, ["profiles", "import"]) => {
            axum::response::IntoResponse::into_response(import_profile(State(ctx), call.body.clone()).await)
        }
        (Method::Post, ["profiles", "import-check"]) => {
            axum::response::IntoResponse::into_response(check_import(State(ctx), call.body.clone()).await)
        }
        (Method::Get, ["catalog", "games"]) => axum::response::IntoResponse::into_response(catalog_games().await),
        (Method::Get, ["games"]) => {
            axum::response::IntoResponse::into_response(games_table(State(ctx), call.query()?).await)
        }
        (Method::Post, ["games", parameter0, "patch"]) => {
            axum::response::IntoResponse::into_response(patch_game(State(ctx), Path(call.parameter(parameter0)?)).await)
        }
        (Method::Put, ["games", parameter0, "port"]) => axum::response::IntoResponse::into_response(
            set_game_port(State(ctx), Path(call.parameter(parameter0)?), call.json()?).await,
        ),
        (Method::Get, ["games", parameter0, "instructions"]) => axum::response::IntoResponse::into_response(
            game_instructions(State(ctx), Path(call.parameter(parameter0)?), call.query()?).await,
        ),
        (Method::Put, ["games", "detection"]) => {
            axum::response::IntoResponse::into_response(set_detection(State(ctx), call.json()?).await)
        }
        (Method::Get, ["games", "bridge-path"]) => {
            axum::response::IntoResponse::into_response(bridge_path(State(ctx)).await)
        }
        (Method::Get, ["notifications"]) => {
            axum::response::IntoResponse::into_response(notifications(State(ctx)).await)
        }
        (Method::Delete, ["notifications", parameter0]) => axum::response::IntoResponse::into_response(
            dismiss_notification(State(ctx), Path(call.parameter(parameter0)?)).await,
        ),
        (Method::Post, ["notifications", parameter0, "hide"]) => axum::response::IntoResponse::into_response(
            hide_notification(State(ctx), Path(call.parameter(parameter0)?)).await,
        ),
        (Method::Post, ["notifications", parameter0, "confirm"]) => axum::response::IntoResponse::into_response(
            confirm_notification(State(ctx), Path(call.parameter(parameter0)?)).await,
        ),
        (Method::Get, ["hardware"]) => axum::response::IntoResponse::into_response(hardware(State(ctx)).await),
        (Method::Post, ["hardware", "test", parameter0]) => axum::response::IntoResponse::into_response(
            run_hardware_test(State(ctx), Path(call.parameter(parameter0)?)).await,
        ),
        (Method::Put, ["motion", "test"]) => {
            axum::response::IntoResponse::into_response(set_motion_sliders(State(ctx), call.json()?).await)
        }
        (Method::Post, ["motion", "test", "toggle"]) => {
            axum::response::IntoResponse::into_response(toggle_motion_test(State(ctx)).await)
        }
        (Method::Post, ["motion", "disconnect"]) => {
            axum::response::IntoResponse::into_response(disconnect_motion(State(ctx)).await)
        }
        (Method::Put, ["shakers", "soundcard"]) => {
            axum::response::IntoResponse::into_response(set_soundcard(State(ctx), call.json()?).await)
        }
        (Method::Delete, ["shakers", "refresh-soundcard"]) => {
            axum::response::IntoResponse::into_response(clear_soundcard_refresh(State(ctx)).await)
        }
        (Method::Post, ["srs-import"]) => {
            axum::response::IntoResponse::into_response(import_srs_files(State(ctx), call.json()?).await)
        }
        (Method::Post, ["audio", "fix", parameter0]) => axum::response::IntoResponse::into_response(
            fix_soundcard(Path(call.parameter(parameter0)?), call.json()?).await,
        ),
        (Method::Post, ["shakers", "test", parameter0]) => axum::response::IntoResponse::into_response(
            test_shaker_effect(State(ctx), Path(call.parameter(parameter0)?)).await,
        ),
        (Method::Post, ["shakers", "speakers", parameter0, "test"]) => axum::response::IntoResponse::into_response(
            test_speaker(State(ctx), Path(call.parameter(parameter0)?)).await,
        ),
        (Method::Put, ["shakers", "speakers", parameter0, "matrix"]) => axum::response::IntoResponse::into_response(
            set_speaker_matrix(State(ctx), Path(call.parameter(parameter0)?), call.json()?).await,
        ),
        (Method::Post, ["belt", "homing"]) => {
            axum::response::IntoResponse::into_response(request_belt_homing(State(ctx)).await)
        }
        _ => return Ok(None),
    };
    Ok(Some(response))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stop a UI patch after it has read its input; make a normal worker update another field at that point.
    /// The worker must wait until the assignment commits, then its update must remain in the shared document.
    fn concurrent_patch(
        ctx: &ApiContext,
        edit: impl FnOnce(ApiContext, std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>) + Send + 'static,
        change: impl FnOnce(ApiContext) + Send + 'static,
    ) {
        use std::sync::mpsc;
        use std::time::Duration;
        let (entered, input_read) = mpsc::channel();
        let (resume, release) = mpsc::channel();
        let editor_ctx = ctx.clone();
        let editor = std::thread::spawn(move || edit(editor_ctx, entered, release));
        input_read.recv_timeout(Duration::from_secs(2)).expect("patch reached the input read");
        let (started, starting) = mpsc::channel();
        let (finished, done) = mpsc::channel();
        let writer_ctx = ctx.clone();
        let writer = std::thread::spawn(move || {
            started.send(()).unwrap();
            change(writer_ctx);
            finished.send(()).unwrap();
        });
        starting.recv_timeout(Duration::from_secs(2)).unwrap();
        let committed_early = done.recv_timeout(Duration::from_millis(100)).is_ok();
        resume.send(()).unwrap();
        editor.join().unwrap();
        writer.join().unwrap();
        assert!(!committed_early, "another writer committed between the patch input read and replacement");
    }

    #[test]
    fn controls_patch_preserves_concurrent_hotkey_update() {
        let h = super::super::tests::harness();
        concurrent_patch(
            &h.context,
            |ctx, entered, release| {
                edit_controls(&ctx, |before| {
                    entered.send(()).unwrap();
                    release.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
                    patched(before, &json!({"telemetry":{"capture":true}}))
                })
                .unwrap();
            },
            |ctx| crate::hotkeys::actions::run(&ctx, std::path::Path::new("unused"), "motion_box_switch"),
        );
        let controls = h.context.state.controls.read();
        assert!(controls.telemetry.capture);
        assert!(!controls.switches.motion, "the unrelated hotkey toggle is retained");
    }

    #[test]
    fn settings_patch_preserves_concurrent_native_assignment() {
        use openrigcue_model::native_value::LegacyValue;
        let h = super::super::tests::harness();
        concurrent_patch(
            &h.context,
            |ctx, entered, release| {
                edit_settings(&ctx, |before| {
                    entered.send(()).unwrap();
                    release.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
                    before.apply_ui_patch(&json!({"shakers":{"use_srs_sounds":true}})).map_err(ApiError::invalid)
                })
                .unwrap();
            },
            |ctx| {
                ctx.state.config.write().update(|config| {
                    config.settings.edit_native(|raw| {
                        raw.get_mut("APP").unwrap().assign_item("startup_page", LegacyValue::from("Setup")).unwrap();
                    });
                });
            },
        );
        let store = h.context.state.config.read();
        assert!(store.settings().shakers.use_srs_sounds);
        assert_eq!(store.settings().app.start_page, "Setup");
    }

    #[test]
    fn profile_patch_preserves_concurrent_hotkey_update() {
        let h = super::super::tests::harness();
        h.context.state.profile.write().tune.power_wind_max_speed_perc = 50.0;
        let step = h.context.state.config.read().settings().wind.power_step;
        concurrent_patch(
            &h.context,
            |ctx, entered, release| {
                edit_profile(&ctx, |native| {
                    entered.send(()).unwrap();
                    release.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
                    apply_profile_patch(native, json!({"speed_factor":3.0}))
                })
                .unwrap();
            },
            |ctx| crate::hotkeys::actions::run(&ctx, std::path::Path::new("unused"), "power_wind_up"),
        );
        let profile = h.context.state.profile.read();
        assert_eq!(profile.tune.speed_factor, 3.0);
        assert_eq!(profile.tune.power_wind_max_speed_perc, 50.0 + f64::from(step));
    }

    #[tokio::test]
    async fn rejected_document_patches_release_locks_without_committing() {
        let h = super::super::tests::harness();
        let controls = h.context.state.controls.snapshot();
        let settings = h.context.state.config.read().settings().native_snapshot();
        let revision = h.context.state.config.read().revision();
        let profile = h.context.state.profile.read().native_tune();
        let generation = h.context.state.profile.read().generation;
        assert!(patch_controls(State(h.context.clone()), Json(json!({"switches":{"motion":"invalid"}})))
            .await
            .is_err());
        assert!(patch_settings(State(h.context.clone()), Json(json!({"app":null}))).await.is_err());
        assert!(patch_profile(State(h.context.clone()), Json(json!({"missing_parent":{"value":1}}))).await.is_err());
        assert_eq!(h.context.state.controls.snapshot(), controls);
        assert_eq!(h.context.state.config.read().settings().native_snapshot(), settings);
        assert_eq!(h.context.state.config.read().revision(), revision);
        assert_eq!(h.context.state.profile.read().native_tune(), profile);
        assert_eq!(h.context.state.profile.read().generation, generation);
    }

    #[tokio::test]
    async fn native_speaker_test_route_preserves_guard_error_detail_and_recovery() {
        use openrigcue_effects::shakers::rig::Voice;
        use openrigcue_model::native_value::LegacyValue;
        /// An owned plain tree of a stored value: every object handle is read and copied, so an edit of the
        /// copy reaches the profile only through `replace_native_tune`, and the first copy stays unedited.
        fn detached(value: &LegacyValue) -> LegacyValue {
            match value.resolved() {
                LegacyValue::Dict(fields) => {
                    LegacyValue::Dict(fields.iter().map(|(key, value)| (key.clone(), detached(value))).collect())
                }
                LegacyValue::List(items) => LegacyValue::List(items.iter().map(detached).collect()),
                LegacyValue::Tuple(items) => LegacyValue::Tuple(items.iter().map(detached).collect()),
                other => other,
            }
        }
        let h = super::super::tests::harness();
        let original = detached(&h.context.state.profile.read().native_tune());
        let mut raw = original.clone();
        raw.as_dict_mut().unwrap().get_mut("shaker_volume").unwrap().assign_item("fl", LegacyValue::from("7")).unwrap();
        h.context.state.profile.write().replace_native_tune(raw);
        assert_eq!(test_speaker(State(h.context.clone()), Path("CE".into())).await.unwrap(), StatusCode::NO_CONTENT);
        assert!(!h.context.shakers.lock().system.mixer().lock().unwrap().is_playing(Voice::SpeakerTest));
        h.context
            .state
            .config
            .write()
            .update(|configuration| configuration.settings.shakers.sound_card = "fake-card".into());
        let error = test_speaker(State(h.context.clone()), Path("CE".into())).await.unwrap_err();
        assert_eq!(error.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(error.detail, "TypeError: unsupported operand type(s) for /: 'str' and 'float'");
        assert!(!h.context.shakers.lock().system.mixer().lock().unwrap().is_playing(Voice::SpeakerTest));
        h.context.state.profile.write().replace_native_tune(original);
        assert_eq!(test_speaker(State(h.context.clone()), Path("CE".into())).await.unwrap(), StatusCode::NO_CONTENT);
        assert!(h.context.shakers.lock().system.mixer().lock().unwrap().is_playing(Voice::SpeakerTest));
    }

    /// The launch steps the Games page shows: the three texts `bridge-path` prints, or the folders searched.
    #[test]
    fn bridge_path_answers_the_paths_and_launch_option_or_the_searched_folders() {
        let launch = games::proton_bridge::BridgeLaunch {
            bridge: "/home/u/bridge dir/openrigcue-bridge.exe".into(),
            bridge_present: false,
            run: "/home/u/bridge dir/openrigcue-bridge-run".into(),
        };
        assert_eq!(
            bridge_path_value(Some(launch), &[]),
            json!({
                "bridge": "/home/u/bridge dir/openrigcue-bridge.exe",
                "bridge_present": false,
                "launch_helper": "/home/u/bridge dir/openrigcue-bridge-run",
                "launch_option": "\"/home/u/bridge dir/openrigcue-bridge-run\" %command%",
            })
        );
        assert_eq!(
            bridge_path_value(None, &["/opt/app/../lib/openrigcue".into(), "/opt/app".into()]),
            json!({"searched": ["/opt/app/../lib/openrigcue", "/opt/app"]})
        );
    }

    fn failing_f12015_table() -> games::GamesTable {
        games::GamesTable {
            rows: vec![games::GameRow {
                key: "ac",
                name: "Assetto Corsa",
                status: games::RowStatus::ReadyToPlay,
                entry_type: games::EntryType::ReadyToPlay,
                port_key: None,
                port: None,
                forwarding: vec![],
                bridge: true,
            }],
            error: Some((
                "f12015",
                openrigcue_config::patch::PatchError::MissingPort(openrigcue_config::patch::PortKey::F1),
            )),
        }
    }

    fn kartkraft_fixture_read_error(input: &[u8]) -> openrigcue_config::patch::PatchError {
        use openrigcue_config::patch::{
            library::BUILTIN, EntryKind, ModulePorts, PatchEnvironment, PortKey, WinePrefix, GAME_ENTRIES,
        };

        let directory = tempfile::tempdir().unwrap();
        let prefix_root = directory.path().join("pfx");
        std::fs::create_dir_all(prefix_root.join("drive_c/users/steamuser/Documents")).unwrap();
        let prefix = WinePrefix::open(prefix_root);
        let file = prefix.local_app_data().join("project_k/Saved/Config/WindowsNoEditor/Game.ini");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, input).unwrap();
        let EntryKind::Config(patcher) = GAME_ENTRIES.iter().find(|entry| entry.key == "kartkraft").unwrap().kind
        else {
            panic!("KartKraft is a configuration entry");
        };
        let ports = |_: PortKey| Some(10000);
        let loaded = ModulePorts::default();
        let env = PatchEnvironment {
            prefix: Some(&prefix),
            steam: None,
            library: &BUILTIN,
            ports: &ports,
            loaded_ports: &loaded,
        };
        patcher.patched(&env).unwrap_err()
    }

    #[test]
    fn games_table_kartkraft_decode_error_keeps_native_details_and_legacy_row_prefix() {
        let mut input = vec![b'a'; 8209];
        input.extend_from_slice(b"\x81\x8dTAIL\r\n");
        let error = kartkraft_fixture_read_error(&input);
        let legacy = format!("kartkraft: {error}");
        let mut table = failing_f12015_table();
        let prefix = serde_json::to_value(&table.rows).unwrap();
        table.error = Some(("kartkraft", error));
        let response = games_table_value(table, &Default::default());
        assert_eq!(response["rows"], prefix);
        assert_eq!(response["error"], legacy);
        assert!(legacy.ends_with("Game.ini is not valid Ansi text"));
        assert_eq!(
            response["native_error"],
            json!({
                "class_name":"UnicodeDecodeError",
                "message":"'charmap' codec can't decode byte 0x81 in position 8209: character maps to <undefined>",
            })
        );
    }

    fn assert_crew_table_decode_error(game: &'static str, folder: &str, bad_byte: u8, last_completed: &str) {
        use openrigcue_config::patch::AnsiReadDecodeError;

        let directory = tempfile::tempdir().unwrap();
        let folder = directory.path().join(".wine/drive_c/users/steamuser/Documents").join(folder);
        std::fs::create_dir_all(&folder).unwrap();
        let mut input = b"<ExtraConfig\r\n".to_vec();
        input.resize(8209, b'a');
        input.push(bad_byte);
        input.extend_from_slice(b"\x81TAIL\r\n");
        std::fs::write(folder.join("ExtraConfig.xml"), &input).unwrap();

        let ports = openrigcue_config::settings::NetworkPorts::default();
        let setup = games::GameSetup::discover(directory.path(), &ports);
        let table = setup.load_table(games::GamesPage::Pc, &ports, &Default::default());
        let (failed_game, error) = table.error.as_ref().unwrap();
        assert_eq!(*failed_game, game);
        let openrigcue_config::patch::PatchError::Io(source) = error else {
            panic!("the actual Crew target must fail during its whole text read");
        };
        let source = source.get_ref().unwrap().downcast_ref::<AnsiReadDecodeError>().unwrap();
        assert_eq!(source.input(), input);
        assert_eq!((source.start(), source.end()), (8209, 8210));
        assert_eq!(table.rows.last().unwrap().key, last_completed);
        assert!(!table.rows.iter().any(|row| row.key == game));
        let prefix = serde_json::to_value(&table.rows).unwrap();
        let legacy = format!("{game}: {error}");
        let response = games_table_value(table, &Default::default());
        assert_eq!(response["rows"], prefix);
        assert_eq!(response["error"], legacy);
        assert!(legacy.ends_with("ExtraConfig.xml is not valid Ansi text"));
        assert_eq!(
            response["native_error"],
            json!({
                "class_name":"UnicodeDecodeError",
                "message":format!("'charmap' codec can't decode byte 0x{bad_byte:02x} in position 8209: character maps to <undefined>"),
            })
        );
    }

    #[test]
    fn games_table_crew1_decode_error_keeps_the_actual_patcher_payload_and_row_prefix() {
        assert_crew_table_decode_error("thecrew", "The Crew", 0x81, "thebus");
    }

    #[test]
    fn games_table_crew2_decode_error_keeps_the_actual_patcher_payload_and_row_prefix() {
        assert_crew_table_decode_error("thecrew2", "The Crew 2", 0x8d, "thecrew");
    }

    #[test]
    fn games_table_dcsbeta_utf8_error_retains_actual_payload_and_completed_rows() {
        let directory = tempfile::tempdir().unwrap();
        let folder = directory.path().join(".wine/drive_c/users/steamuser/Saved Games/DCS.openbeta/Scripts/Hooks");
        std::fs::create_dir_all(&folder).unwrap();
        let mut input = b"-- edited\r\n".to_vec();
        input.resize(8209, b'a');
        input.extend_from_slice(b"\xe2\x82(");
        std::fs::write(folder.join("SRSExport.lua"), &input).unwrap();
        let settings = Settings::default();
        let setup = games::GameSetup::discover(directory.path(), &settings.network_ports);
        let table = setup.load_table_native(
            games::GamesPage::Pc,
            &settings.network_ports,
            &Default::default(),
            &settings.native_snapshot(),
        );
        let (failed, error) = table.error.as_ref().unwrap();
        assert_eq!(*failed, "dcs_beta");
        let openrigcue_config::patch::PatchError::Io(io) = error else { panic!("target read error") };
        let native = io.get_ref().unwrap().downcast_ref::<openrigcue_config::patch::Utf8ReadDecodeError>().unwrap();
        assert_eq!(native.input(), input);
        assert_eq!((native.start(), native.end(), native.reason()), (8209, 8211, "invalid continuation byte"));
        assert_eq!(table.rows.last().unwrap().key, "dcs");
        let prefix = serde_json::to_value(&table.rows).unwrap();
        let legacy = format!("dcs_beta: {error}");
        let response = games_table_value(table, &settings.native_snapshot());
        assert_eq!(response["rows"], prefix);
        assert_eq!(response["error"], legacy);
        assert!(legacy.ends_with("SRSExport.lua is not valid Utf8 text"));
        assert_eq!(
            response["native_error"],
            json!({
                "class_name":"UnicodeDecodeError",
                "message":"'utf-8' codec can't decode bytes in position 8209-8210: invalid continuation byte",
            })
        );
    }

    #[test]
    fn games_table_decode_error_forwarding_is_typed_and_limited_to_live_templates() {
        use openrigcue_config::patch::{AnsiReadDecodeError, PatchError, Utf8ReadDecodeError};
        let live = [
            "condor",
            "condor3",
            "dcs",
            "dcs_beta",
            "fs2020_steam",
            "fs2020_xbox",
            "fs2024_steam",
            "fs2024_xbox",
            "fsx",
            "kartkraft",
            "p3d",
            "p3d_v5",
            "p3d_v6",
            "thecrew",
            "thecrew2",
            "vracer",
        ];
        for game in live.into_iter().chain(["dcs_legacy", "beamng", "f12015", "unverified"]) {
            for utf8 in [true, false] {
                let source: Box<dyn std::error::Error + Send + Sync> = if utf8 {
                    Box::new(Utf8ReadDecodeError::from_bytes(b"\xe2\x82(", std::path::Path::new("fixture")).unwrap())
                } else {
                    Box::new(AnsiReadDecodeError::from_bytes(b"\x81", std::path::Path::new("fixture")).unwrap())
                };
                let mut table = failing_f12015_table();
                let rows = serde_json::to_value(&table.rows).unwrap();
                table.error =
                    Some((game, PatchError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, source))));
                let response = games_table_value(table, &Default::default());
                assert_eq!(response["rows"], rows);
                if live.contains(&game) {
                    assert_eq!(
                        response["native_error"],
                        json!({"class_name":"UnicodeDecodeError", "message": if utf8 {
                            "'utf-8' codec can't decode bytes in position 0-1: invalid continuation byte"
                        } else {
                            "'charmap' codec can't decode byte 0x81 in position 0: character maps to <undefined>"
                        }})
                    );
                } else {
                    assert!(response["native_error"].is_null());
                }
            }
            let mut table = failing_f12015_table();
            table.error = Some((
                game,
                PatchError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "'utf-8' codec can't decode byte 0xff in position 0: invalid start byte",
                )),
            ));
            assert!(games_table_value(table, &Default::default())["native_error"].is_null());
        }
    }

    #[test]
    fn games_table_decode_error_requires_a_proven_game_and_typed_source() {
        use openrigcue_config::patch::PatchError;

        let mut table = failing_f12015_table();
        table.error = Some(("dcs_legacy", kartkraft_fixture_read_error(b"\x81")));
        assert!(games_table_value(table, &Default::default())["native_error"].is_null());
        for game in ["kartkraft", "thecrew", "thecrew2"] {
            for source in [
                std::io::Error::new(std::io::ErrorKind::PermissionDenied, "fixture denied"),
                std::io::Error::new(std::io::ErrorKind::InvalidData, "ExtraConfig.xml is not valid Ansi text"),
                std::io::Error::new(std::io::ErrorKind::InvalidData, "UnicodeDecodeError: 'charmap' codec can't decode byte 0x81 in position 0: character maps to <undefined>"),
            ] {
                let mut table = failing_f12015_table();
                table.error = Some((game, PatchError::Io(source)));
                assert!(games_table_value(table, &Default::default())["native_error"].is_null());
            }
        }
    }

    #[test]
    fn games_table_native_error_retains_missing_key_class_and_compatibility_prefix() {
        let mut settings = Settings::default();
        settings.edit_native(|raw| {
            raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().remove("f1");
        });
        assert_eq!(settings.network_ports.get("f1"), None);
        let table = failing_f12015_table();
        let expected_rows = serde_json::to_value(&table.rows).unwrap();
        let response = games_table_value(table, &settings.native_snapshot());
        assert_eq!(response["rows"], expected_rows);
        assert_eq!(response["error"], "f12015: no [NETWORK_PORT] f1 entry");
        assert_eq!(response["native_error"], json!({"class_name":"KeyError", "message":"'f1'"}));
    }

    #[test]
    fn games_table_wrc_conversion_retains_typed_error_and_completed_rows() {
        use openrigcue_config::patch::{library::BUILTIN, EntryKind, ModulePorts, PatchEnvironment, GAME_ENTRIES};
        let EntryKind::Config(patcher) = GAME_ENTRIES.iter().find(|entry| entry.key == "wrc2023").unwrap().kind else {
            panic!("config")
        };
        let mut settings = Settings::default();
        settings.edit_native(|raw| {
            raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert("wrc2023", "bad".into());
        });
        let loaded = ModulePorts::at_startup(&settings);
        let env =
            PatchEnvironment { prefix: None, steam: None, library: &BUILTIN, ports: &settings, loaded_ports: &loaded };
        let error = patcher.patched(&env).unwrap_err();
        let mut table = failing_f12015_table();
        let prefix = serde_json::to_value(&table.rows).unwrap();
        table.error = Some(("wrc2023", error));
        let response = games_table_value(table, &settings.native_snapshot());
        assert_eq!(response["rows"], prefix);
        assert_eq!(
            response["native_error"],
            json!({"class_name":"ValueError", "message":"invalid literal for int() with base 10: 'bad'"})
        );
        let mut table = failing_f12015_table();
        table.error = Some((
            "wrc2023",
            openrigcue_config::patch::PatchError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid literal for int() with base 10: 'bad'",
            )),
        ));
        assert!(games_table_value(table, &settings.native_snapshot())["native_error"].is_null());
        for missing_section in [false, true] {
            let mut settings = Settings::default();
            settings.edit_native(|raw| {
                if missing_section {
                    raw.remove("NETWORK_PORT");
                } else {
                    raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().remove("wrc2023");
                }
            });
            let loaded = ModulePorts::at_startup(&settings);
            let env = PatchEnvironment {
                prefix: None,
                steam: None,
                library: &BUILTIN,
                ports: &settings,
                loaded_ports: &loaded,
            };
            let mut table = failing_f12015_table();
            table.error = Some(("wrc2023", patcher.patched(&env).unwrap_err()));
            let expected = if missing_section { "'NETWORK_PORT'" } else { "'wrc2023'" };
            assert_eq!(
                games_table_value(table, &settings.native_snapshot())["native_error"],
                json!({"class_name":"KeyError", "message":expected})
            );
        }
    }

    #[test]
    fn games_table_native_error_distinguishes_missing_section_from_missing_key() {
        let mut settings = Settings::default();
        settings.edit_native(|raw| {
            raw.remove("NETWORK_PORT");
        });
        assert!(!settings.native_snapshot().contains("NETWORK_PORT"));
        let response = games_table_value(failing_f12015_table(), &settings.native_snapshot());
        assert_eq!(response["error"], "f12015: no [NETWORK_PORT] f1 entry");
        assert_eq!(response["native_error"], json!({"class_name":"KeyError", "message":"'NETWORK_PORT'"}));
    }

    #[test]
    fn games_table_native_error_does_not_classify_present_invalid_values_or_unproven_paths() {
        use openrigcue_config::import::value::LegacyValue;
        use openrigcue_config::patch::{PatchError, PortKey};

        for value in [
            LegacyValue::None,
            LegacyValue::Str("bad".into()),
            LegacyValue::Str("70000".into()),
            LegacyValue::Int(-1),
            LegacyValue::Float(1.5),
            LegacyValue::Int(20778),
        ] {
            let mut settings = Settings::default();
            settings.edit_native(|raw| {
                raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert("f1", value);
            });
            let native = settings.native_snapshot();
            assert!(native.get("NETWORK_PORT").unwrap().as_dict().unwrap().contains("f1"));
            let response = games_table_value(failing_f12015_table(), &native);
            assert!(response["native_error"].is_null());
            assert_eq!(response["error"], "f12015: no [NETWORK_PORT] f1 entry");
        }
        let mut raw = Settings::default().native_snapshot();
        raw.insert("NETWORK_PORT", LegacyValue::None);
        assert!(games_table_value(failing_f12015_table(), &raw)["native_error"].is_null());
        for error in [
            Some(("f12015", PatchError::MissingPort(PortKey::Dirt))),
            Some(("f12020", PatchError::MissingPort(PortKey::F1))),
            Some(("f12015", PatchError::MissingElement)),
            None,
        ] {
            let mut table = failing_f12015_table();
            table.error = error;
            assert!(games_table_value(table, &Default::default())["native_error"].is_null());
        }
    }

    /// The shaker effect and speaker names the UI uses.
    #[test]
    fn names() {
        assert_eq!(effect_named("road_bump").unwrap(), Effect::RoadBump);
        assert!(effect_named("nope").is_err());
        assert_eq!(speaker_named("SUB").unwrap(), Speaker::Sub);
    }
}
