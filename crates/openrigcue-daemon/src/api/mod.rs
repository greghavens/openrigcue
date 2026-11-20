//! The service functions called directly by the Rust UI, their HTTP adapters for tests, and the network
//! server, which serves the UI page, websocket and static files only.

pub(crate) mod direct;
mod joystick;
mod routes;
pub(crate) mod start_policy;
#[cfg(test)]
mod tests;
mod ui_mount;
pub mod ui_state;
#[cfg(test)]
mod ui_state_tests;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use openrigcue_effects::accessories::dispatcher::Accessories;
use serde::Serialize;

use crate::motion::MotionHandle;
use crate::profile::MainLoopHandle;
use crate::shakers::ShakersHandle;
use crate::state::ServiceState;
use crate::telemetry::TelemetryHandle;

pub use routes::router;
use routes::server_router;

/// What the handlers reach.
#[derive(Clone)]
pub struct ApiContext {
    pub state: ServiceState,
    pub telemetry: TelemetryHandle,
    pub motion: MotionHandle,
    pub shakers: ShakersHandle,
    pub accessories: Arc<Accessories>,
    /// VR motion compensation (rig-pose publisher state, VR settings, re-centre).
    pub compensation: crate::compensation::VrCompensation,
    pub main_loop: MainLoopHandle,
    /// The SRS installation given with `--srs-install` (the optional sound import); no route reads it.
    pub srs_install: Option<PathBuf>,
    /// The host's IP address.
    pub host_ip: String,
    /// Spoken notifications and the IntelliTune voice prompts.
    pub voice: crate::voice::Voice,
    /// Translations, log level and restart (see [`ui_state`]).
    pub ui: Arc<ui_state::UiServices>,
    /// The Joystick page (profiles, assignment dialog, bars).
    pub joystick: crate::joystick::JoystickService,
    /// Hotkeys and controller-button shortcuts (editor, input status, `ui_actions`).
    pub hotkeys: crate::hotkeys::Hotkeys,
}

/// An RFC 7807 problem document.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    detail: String,
}

#[derive(Serialize)]
struct Problem<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    title: &'a str,
    status: u16,
    detail: &'a str,
    /// The detail as a sentence with its values ([`crate::i18n::sentence`]), when it has any: the UI shows its
    /// translation, `detail` is the English text.
    #[serde(skip_serializing_if = "Option::is_none")]
    sentence: Option<&'a str>,
}

impl ApiError {
    pub fn new(status: StatusCode, detail: impl Into<String>) -> ApiError {
        ApiError { status, detail: detail.into() }
    }

    pub fn not_found(detail: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::NOT_FOUND, detail)
    }

    pub fn conflict(detail: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::CONFLICT, detail)
    }

    pub fn invalid(detail: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, detail)
    }

    pub fn internal(detail: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, detail)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let detail = crate::i18n::plain(&self.detail);
        let problem = Problem {
            kind: "about:blank",
            title: self.status.canonical_reason().unwrap_or("Error"),
            status: self.status.as_u16(),
            detail: &detail,
            sentence: (detail != self.detail).then_some(self.detail.as_str()),
        };
        let mut response = (self.status, Json(&problem)).into_response();
        response
            .headers_mut()
            .insert(axum::http::header::CONTENT_TYPE, axum::http::HeaderValue::from_static("application/problem+json"));
        response
    }
}

/// RFC 7386 JSON merge patch: objects merge key by key, `null` removes, anything else replaces.
pub fn merge_patch(target: &mut serde_json::Value, patch: &serde_json::Value) {
    let serde_json::Value::Object(changes) = patch else {
        *target = patch.clone();
        return;
    };
    if !target.is_object() {
        *target = serde_json::Value::Object(serde_json::Map::new());
    }
    let fields = target.as_object_mut().expect("made an object above");
    for (key, value) in changes {
        if value.is_null() {
            fields.remove(key);
        } else {
            merge_patch(fields.entry(key.clone()).or_insert(serde_json::Value::Null), value);
        }
    }
}

/// Applies a merge patch to a typed value through its JSON form.
pub fn patched<T: Serialize + serde::de::DeserializeOwned>(
    value: &T,
    patch: &serde_json::Value,
) -> Result<T, ApiError> {
    let mut json = serde_json::to_value(value).map_err(|e| ApiError::internal(e.to_string()))?;
    merge_patch(&mut json, patch);
    serde_json::from_value(json).map_err(|e| ApiError::invalid(e.to_string()))
}

/// The API server thread.
pub struct ApiServer {
    shutdown: openrigcue_ui::host::ServerStop,
    ui: openrigcue_ui::host::UiHost,
    thread: Option<JoinHandle<()>>,
    // Stopping the server stops HTTP acceptance; an existing application's callbacks,
    // main loop and (on hook failure) sockets can still use the runtime.
    _runtime: Arc<tokio::runtime::Runtime>,
    pub address: SocketAddr,
}

impl ApiServer {
    /// Binds `address` and serves the UI on its own runtime.
    pub fn start(context: ApiContext, address: SocketAddr) -> std::io::Result<ApiServer> {
        let legacy_ssl = start_policy::legacy_ssl_requested(|path| path.exists());
        Self::start_resolved(context, address, legacy_ssl)
    }

    pub(crate) fn start_imported(
        context: ApiContext,
        port: openrigcue_model::native_value::LegacyValue,
    ) -> std::io::Result<ApiServer> {
        let (address, legacy_ssl) = start_policy::imported_native_policy(&port, |path| path.exists())?;
        Self::start_resolved(context, address, legacy_ssl)
    }

    fn start_resolved(context: ApiContext, address: SocketAddr, legacy_ssl: bool) -> std::io::Result<ApiServer> {
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().thread_name("api").build()?,
        );
        let listener = runtime.block_on(tokio::net::TcpListener::bind(address))?;
        // The socket is bound first; a requested legacy certificate pair then fails the start.
        start_policy::after_bind(legacy_ssl)?;
        let address = listener.local_addr()?;
        let shutdown = openrigcue_ui::host::ServerStop::new();
        let stopped = shutdown.clone();
        let serving_runtime = runtime.clone();
        let (ready, ui) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new().name("startserver".into()).spawn(move || {
            serving_runtime.block_on(async move {
                let (router, host) = server_router(context, stopped.clone());
                let _ = ready.send(host);
                let serve = openrigcue_ui::host::serve_with_stop(listener, router, stopped);
                if let Err(error) = serve.await {
                    tracing::error!(%error, "API server stopped");
                }
            });
        })?;
        let ui = ui.recv().map_err(std::io::Error::other)?;
        tracing::info!(%address, "API listening");
        Ok(ApiServer { shutdown, ui, thread: Some(thread), _runtime: runtime, address })
    }

    pub fn stop(&mut self) -> Result<(), String> {
        self.shutdown.stop();
        self.ui.on_close()?;
        // Shutdown does not join active request handlers.
        // They may include the request that initiated this shutdown.
        self.thread.take();
        Ok(())
    }
}

#[cfg(test)]
mod merge_tests {
    use super::*;
    use serde_json::json;

    /// RFC 7386 examples.
    #[test]
    fn merge_patch_rules() {
        let mut target = json!({"a": "b", "c": {"d": "e", "f": "g"}});
        merge_patch(&mut target, &json!({"a": "z", "c": {"f": null}}));
        assert_eq!(target, json!({"a": "z", "c": {"d": "e"}}));
        let mut array = json!({"a": [1, 2]});
        merge_patch(&mut array, &json!({"a": [3]}));
        assert_eq!(array, json!({"a": [3]}));
    }
}
