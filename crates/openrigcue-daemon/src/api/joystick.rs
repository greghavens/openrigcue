//! The shared state read and written by the Joystick page. Widget, capture and profile-dialog logic lives
//! in openrigcue-ui; these operations only access the input state and autocommit profile table.

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{delete, get};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{ApiContext, ApiError};
use crate::joystick::JoystickError;

impl From<JoystickError> for ApiError {
    fn from(error: JoystickError) -> ApiError {
        match error {
            JoystickError::NameExists => ApiError::conflict(error.to_string()),
            JoystickError::NoProfile(name) => {
                ApiError::not_found(format!("KeyError: {}", openrigcue_model::python_repr::repr_str(&name)))
            }
            JoystickError::NoRow => ApiError::not_found(error.to_string()),
            JoystickError::Store(message) => ApiError::internal(message),
        }
    }
}

pub fn routes() -> Router<ApiContext> {
    Router::new()
        .route("/api/v1/joystick/db", get(database).put(store).delete(remove))
        .route("/api/v1/joystick/profile", get(profile).put(select_profile))
        .route("/api/v1/joystick/devices", get(devices))
        .route("/api/v1/joystick/devices/last-state", delete(clear_last_state))
}

#[derive(Deserialize)]
struct Name {
    name: String,
}

#[derive(Deserialize)]
struct Entry {
    key: String,
    value: Value,
}

#[derive(Deserialize)]
struct Key {
    key: String,
}

async fn database(State(ctx): State<ApiContext>) -> Result<Json<Value>, ApiError> {
    Ok(Json(ctx.joystick.profiles_json()?))
}

async fn store(State(ctx): State<ApiContext>, Json(entry): Json<Entry>) -> Result<StatusCode, ApiError> {
    ctx.joystick.store_profile_value(&entry.key, entry.value)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn remove(State(ctx): State<ApiContext>, Json(key): Json<Key>) -> Result<StatusCode, ApiError> {
    ctx.joystick.delete_profile_key(&key.key)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn profile(State(ctx): State<ApiContext>) -> Json<Value> {
    Json(json!({"name": ctx.joystick.selected_profile()}))
}

async fn select_profile(State(ctx): State<ApiContext>, Json(body): Json<Name>) -> StatusCode {
    ctx.joystick.set_selected_profile(body.name);
    StatusCode::NO_CONTENT
}

async fn devices(State(ctx): State<ApiContext>) -> Json<Value> {
    Json(ctx.joystick.devices_json())
}

async fn clear_last_state(State(ctx): State<ApiContext>) -> StatusCode {
    ctx.joystick.clear_last_state();
    StatusCode::NO_CONTENT
}

/// Direct UI operations over the same service functions as the test route adapters.
pub(crate) async fn direct_call(
    ctx: crate::api::ApiContext,
    call: &crate::api::direct::Call<'_>,
) -> Result<Option<axum::response::Response>, crate::api::ApiError> {
    use openrigcue_ui::backend::Method;
    let response = match (call.method, call.segments.as_slice()) {
        (Method::Get, ["joystick", "db"]) => axum::response::IntoResponse::into_response(database(State(ctx)).await),
        (Method::Put, ["joystick", "db"]) => {
            axum::response::IntoResponse::into_response(store(State(ctx), call.json()?).await)
        }
        (Method::Delete, ["joystick", "db"]) => {
            axum::response::IntoResponse::into_response(remove(State(ctx), call.json()?).await)
        }
        (Method::Get, ["joystick", "profile"]) => {
            axum::response::IntoResponse::into_response(profile(State(ctx)).await)
        }
        (Method::Put, ["joystick", "profile"]) => {
            axum::response::IntoResponse::into_response(select_profile(State(ctx), call.json()?).await)
        }
        (Method::Get, ["joystick", "devices"]) => {
            axum::response::IntoResponse::into_response(devices(State(ctx)).await)
        }
        (Method::Delete, ["joystick", "devices", "last-state"]) => {
            axum::response::IntoResponse::into_response(clear_last_state(State(ctx)).await)
        }
        _ => return Ok(None),
    };
    Ok(Some(response))
}
