//! Direct in-process calls from the Rust UI to the service handlers. The product does not construct an HTTP
//! request or run a second router to reach globals; the HTTP route adapters remain available for API tests.

use axum::body::Bytes;
use axum::extract::{Json, Query};
use axum::http::Uri;
use axum::response::{IntoResponse, Response};
use openrigcue_ui::backend::Method;
use serde::de::DeserializeOwned;

use super::{ApiContext, ApiError};

pub(crate) struct Call<'a> {
    pub method: Method,
    pub segments: Vec<&'a str>,
    pub body: Bytes,
    uri: Uri,
}

impl<'a> Call<'a> {
    fn new(method: Method, path: &'a str, body: Bytes) -> Result<Self, ApiError> {
        let uri = format!("/api/v1/{path}")
            .parse()
            .map_err(|error| ApiError::invalid(format!("invalid call path: {error}")))?;
        let path = path.split('?').next().unwrap_or(path);
        Ok(Self { method, segments: path.split('/').collect(), body, uri })
    }

    pub fn json<T: DeserializeOwned>(&self) -> Result<Json<T>, ApiError> {
        serde_json::from_slice(&self.body).map(Json).map_err(|error| ApiError::invalid(error.to_string()))
    }

    pub fn query<T: DeserializeOwned>(&self) -> Result<Query<T>, ApiError> {
        Query::try_from_uri(&self.uri).map_err(|error| ApiError::invalid(error.to_string()))
    }

    pub fn parameter<T: DeserializeOwned>(&self, text: &str) -> Result<T, ApiError> {
        let decoded = decode_segment(text)?;
        serde_json::from_value(serde_json::Value::String(decoded.clone()))
            .or_else(|_| serde_json::from_str(&decoded))
            .map_err(|error| ApiError::invalid(error.to_string()))
    }
}

fn decode_segment(text: &str) -> Result<String, ApiError> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut position = 0;
    while position < bytes.len() {
        if bytes[position] == b'%' && position + 2 < bytes.len() {
            let digit = |value: u8| char::from(value).to_digit(16);
            if let (Some(high), Some(low)) = (digit(bytes[position + 1]), digit(bytes[position + 2])) {
                decoded.push((high * 16 + low) as u8);
                position += 3;
                continue;
            }
        }
        decoded.push(bytes[position]);
        position += 1;
    }
    String::from_utf8(decoded).map_err(|error| ApiError::invalid(error.to_string()))
}

pub(crate) async fn call(context: ApiContext, method: Method, path: &str, body: Bytes) -> Response {
    async fn dispatch(context: ApiContext, call: &Call<'_>) -> Result<Response, ApiError> {
        macro_rules! owner {
            ($module:path) => {
                if let Some(response) = $module(context.clone(), call).await? {
                    return Ok(response);
                }
            };
        }
        owner!(super::routes::direct_call);
        owner!(super::ui_state::direct_call);
        owner!(super::joystick::direct_call);
        owner!(crate::compensation::routes::direct_call);
        owner!(crate::hotkeys::routes::direct_call);
        owner!(crate::voice::direct_call);
        owner!(crate::device_access::direct_call);
        Err(ApiError::not_found("unknown service operation"))
    }
    match Call::new(method, path, body) {
        Ok(call) => dispatch(context, &call).await.into_response(),
        Err(error) => error.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::{Method as HttpMethod, Request};
    use tower::ServiceExt;

    #[tokio::test]
    async fn device_access_install_requires_a_current_notice_and_dismissal_persists() {
        let h = super::super::tests::harness();
        let response = call(h.context.clone(), Method::Post, "device-access/999/install", Bytes::new()).await;
        assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("no device access notification"));
        h.context.state.notifications.write().device_permissions(&["/dev/ttyUSB0".into()]);
        let id = h.context.state.notifications.read().items[0].id;
        let response = call(h.context.clone(), Method::Delete, &format!("notifications/{id}"), Bytes::new()).await;
        assert!(response.status().is_success());
        assert!(h.context.state.config.read().metadata_contains("notifications", "linux_device_access").unwrap());
    }

    #[tokio::test]
    async fn direct_reads_match_the_existing_service_contracts() {
        let h = super::super::tests::harness();
        for path in [
            "settings",
            "profile",
            "tunes/api",
            "tunes/api/vehicles/Torque%20Drift",
            "joystick/db",
            "joystick/profile",
            "joystick/devices",
            "hotkeys",
            "hotkeys/capture?joystick=Wheel%20%2F%20Pedals",
            "compensation/vr",
            "catalog/games",
        ] {
            let direct = call(h.context.clone(), Method::Get, path, Bytes::new()).await;
            let request =
                Request::builder().method(HttpMethod::GET).uri(format!("/api/v1/{path}")).body(Body::empty()).unwrap();
            let adapted = super::super::router(h.context.clone()).oneshot(request).await.unwrap();
            assert_eq!(direct.status(), adapted.status(), "{path}");
            let direct = to_bytes(direct.into_body(), usize::MAX).await.unwrap();
            let adapted = to_bytes(adapted.into_body(), usize::MAX).await.unwrap();
            assert_eq!(direct, adapted, "{path}");
        }
    }

    #[tokio::test]
    async fn direct_edits_and_file_downloads_reach_the_same_state() {
        let h = super::super::tests::harness();
        let response = call(
            h.context.clone(),
            Method::Put,
            "joystick/db",
            Bytes::from_static(br#"{"key":"Unassigned","value":{"front":{"joystick":null,"mode":null}}}"#),
        )
        .await;
        assert_eq!(response.status(), axum::http::StatusCode::NO_CONTENT);
        let value = h.context.joystick.profiles_json().unwrap();
        assert!(value["Unassigned"]["front"]["joystick"].is_null());
        assert_eq!(value["Unassigned"]["front"].as_object().unwrap().len(), 2);
        let response = call(
            h.context.clone(),
            Method::Patch,
            "profile",
            Bytes::from_static(br#"{"routines":{"Fanalab":{"enabled":true,"profile":null}}}"#),
        )
        .await;
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert!(h.context.state.profile.read().tune_json().unwrap()["routines"]["Fanalab"]["profile"].is_null());
        let response =
            call(h.context, Method::Get, "profiles/export?game=api&vehicle=Torque%20Drift", Bytes::new()).await;
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert!(response.headers()[axum::http::header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .contains("api_Torque Drift.srs"));
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(bytes.starts_with(b"PK"));
        assert!(openrigcue_config::profiles::read_profile_file(&bytes).is_ok());
    }
}
