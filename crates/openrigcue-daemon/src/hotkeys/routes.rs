//! `/api/v1/hotkeys`: the shortcut editor.
//!
//! - `GET /api/v1/hotkeys`: the controllers and every page's shortcut list with labels and bindings.
//! - `PUT /api/v1/hotkeys/bindings/{action}`: the editor's Confirm (in memory, saved with the settings).
//! - `POST` / `DELETE /api/v1/hotkeys/capture`: the editor opens / closes (`list_shortcuts`).
//! - `GET /api/v1/hotkeys/capture?joystick=<name>`: what the keyboard and controller captures read.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use super::names::{groups, label};
use crate::api::{ApiContext, ApiError};
use crate::state::ServiceEvent;

pub fn router() -> Router<ApiContext> {
    Router::new()
        .route("/api/v1/hotkeys", get(view))
        .route("/api/v1/hotkeys/bindings/{action}", put(set_binding))
        .route("/api/v1/hotkeys/capture", get(captured).post(open_editor).delete(close_editor))
}

#[derive(Debug, Serialize)]
struct ActionView {
    action: String,
    label: String,
    keys: String,
    /// `<joystick>^<button>` as stored.
    buttons: String,
    /// The two halves the editor shows.
    joystick: String,
    button: String,
}

#[derive(Debug, Serialize)]
struct GroupView {
    page: &'static str,
    actions: Vec<ActionView>,
}

#[derive(Debug, Serialize)]
struct HotkeysView {
    /// Connected controllers by name; empty while controller input is disabled.
    joysticks: Vec<String>,
    groups: Vec<GroupView>,
    capturing: bool,
    /// No keyboard can be read for lack of permission: keyboard shortcuts need the keyboard access rule.
    keyboard_denied: bool,
}

async fn view(State(ctx): State<ApiContext>) -> Json<HotkeysView> {
    let shortcuts = ctx.state.config.read().settings().shortcuts.clone();
    let groups = groups()
        .into_iter()
        .map(|group| GroupView {
            page: group.page,
            actions: group
                .actions
                .into_iter()
                .map(|action| {
                    let binding = shortcuts.binding(&action).cloned().unwrap_or_default();
                    let mut halves = binding.buttons.split('^');
                    let joystick = halves.next().unwrap_or_default().to_owned();
                    let button = halves.next().unwrap_or_default().to_owned();
                    ActionView {
                        label: label(&action).unwrap_or_else(|| action.clone()),
                        action,
                        keys: binding.keys,
                        buttons: binding.buttons,
                        joystick,
                        button,
                    }
                })
                .collect(),
        })
        .collect();
    Json(HotkeysView {
        joysticks: ctx.hotkeys.controllers(),
        groups,
        capturing: ctx.hotkeys.capturing(),
        keyboard_denied: ctx.hotkeys.keyboard_denied(),
    })
}

/// The editor's Confirm: the keyboard field, and the controller (`'None'` for none) with its button.
#[derive(Debug, Deserialize)]
struct BindingChange {
    #[serde(default)]
    keys: String,
    #[serde(default)]
    joystick: String,
    #[serde(default)]
    button: String,
}

/// Confirms a binding: `<action>_keys` takes the keyboard field; `<action>_btns` is empty when no controller or no
/// button is chosen, else `<joystick>^<button>`. The editor closes.
async fn set_binding(
    State(ctx): State<ApiContext>,
    Path(action): Path<String>,
    Json(change): Json<BindingChange>,
) -> Result<StatusCode, ApiError> {
    if label(&action).is_none() {
        return Err(ApiError::not_found(format!("no shortcut action {action}")));
    }
    ctx.hotkeys.set_capture(false);
    let buttons = if change.button.is_empty() || change.joystick == "None" || change.joystick.is_empty() {
        String::new()
    } else {
        format!("{}^{}", change.joystick, change.button)
    };
    ctx.state.config.write().update(|config| {
        let binding = config.settings.shortcuts.bindings.entry(action).or_default();
        binding.keys = change.keys;
        binding.buttons = buttons;
    });
    ctx.state.announce(ServiceEvent::Settings);
    Ok(StatusCode::NO_CONTENT)
}

async fn open_editor(State(ctx): State<ApiContext>) -> StatusCode {
    ctx.hotkeys.set_capture(true);
    StatusCode::NO_CONTENT
}

async fn close_editor(State(ctx): State<ApiContext>) -> StatusCode {
    ctx.hotkeys.set_capture(false);
    StatusCode::NO_CONTENT
}

#[derive(Debug, Deserialize)]
struct CaptureQuery {
    #[serde(default)]
    joystick: String,
}

#[derive(Debug, Serialize)]
struct CaptureView {
    keys: Option<String>,
    /// Hotkeys read so far: each read sets the keyboard field again.
    reads: u64,
    button: Option<String>,
}

async fn captured(State(ctx): State<ApiContext>, Query(query): Query<CaptureQuery>) -> Json<CaptureView> {
    let (keys, reads) = ctx.hotkeys.captured_keys().map_or((None, 0), |(keys, reads)| (Some(keys), reads));
    Json(CaptureView { keys, reads, button: ctx.hotkeys.captured_button(&query.joystick) })
}

/// Direct UI operations over the same service functions as the test route adapters.
pub(crate) async fn direct_call(
    ctx: crate::api::ApiContext,
    call: &crate::api::direct::Call<'_>,
) -> Result<Option<axum::response::Response>, crate::api::ApiError> {
    use openrigcue_ui::backend::Method;
    let response = match (call.method, call.segments.as_slice()) {
        (Method::Get, ["hotkeys"]) => axum::response::IntoResponse::into_response(view(State(ctx)).await),
        (Method::Put, ["hotkeys", "bindings", parameter0]) => axum::response::IntoResponse::into_response(
            set_binding(State(ctx), Path(call.parameter(parameter0)?), call.json()?).await,
        ),
        (Method::Get, ["hotkeys", "capture"]) => {
            axum::response::IntoResponse::into_response(captured(State(ctx), call.query()?).await)
        }
        (Method::Post, ["hotkeys", "capture"]) => {
            axum::response::IntoResponse::into_response(open_editor(State(ctx)).await)
        }
        (Method::Delete, ["hotkeys", "capture"]) => {
            axum::response::IntoResponse::into_response(close_editor(State(ctx)).await)
        }
        _ => return Ok(None),
    };
    Ok(Some(response))
}
