//! Pure callbacks and synchronization metadata: no host, app or socket worker.

use std::time::Instant;

use super::*;
use crate::host::Client;
use crate::remi::gui::HasGui;
use crate::remi::{Gui, Untranslated, WidgetId};

struct App {
    gui: Gui<Self>,
    events: Arc<Mutex<Vec<&'static str>>>,
    failure: Option<&'static str>,
}

impl App {
    fn new(events: Arc<Mutex<Vec<&'static str>>>) -> (Self, WidgetId) {
        let mut app = Self { gui: Gui::new(Arc::new(Untranslated)), events, failure: None };
        let body = app.gui.create("body", "BODY");
        (app, body)
    }
}

impl HasGui for App {
    fn gui(&mut self) -> &mut Gui<Self> {
        &mut self.gui
    }
}

impl UiApp for App {
    fn main(&mut self, body: WidgetId) -> WidgetId {
        body
    }
    fn title(&self) -> String {
        String::new()
    }
    fn head(&self) -> String {
        String::new()
    }
    fn poll(&mut self, _: Instant) {}
    fn on_close(&mut self) -> Result<(), String> {
        lock(&self.events).push("on_close");
        self.failure.map_or(Ok(()), |error| Err(error.into()))
    }
}

#[tokio::test]
async fn close_attribute_waits_for_http_accept_stop_before_the_hook() {
    for failure in [None, Some("KeyError: 'APP'")] {
        let events = Arc::new(Mutex::new(Vec::new()));
        let (mut app, body) = App::new(events.clone());
        app.failure = failure;
        let server = ServerStop::new();
        let stop = server.clone();
        // This thread runs one fake callback, not the UI/HTTP/service loop.
        let callback = std::thread::spawn(move || {
            let mut lifecycle = Lifecycle::new(stop);
            let clients = Clients::default();
            let widget = app.gui.identifier(body).to_owned();
            let result = lifecycle.attribute(&mut app, &clients, body, &widget, "close", &[]).unwrap();
            (result, lifecycle.updates_stopped, app)
        });
        server.requested().await;
        assert!(lock(&events).is_empty(), "hook must wait for HTTP acknowledgement");
        lock(&events).push("http_accept_stopped");
        server.accept_stopped();
        let (result, stopped, app) = callback.join().unwrap();
        assert_eq!(result, failure.map_or(Ok(()), |error| Err(error.into())));
        assert_eq!(stopped, failure.is_none());
        assert_eq!(*lock(&events), ["http_accept_stopped", "on_close"]);
        assert_eq!(app.gui.node(body).tag, "body", "the application is retained");
    }
}

#[test]
fn direct_on_close_keeps_http_and_application_without_websockets() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let (mut app, body) = App::new(events.clone());
    let server = ServerStop::new();
    let mut lifecycle = Lifecycle::new(server.clone());
    assert_eq!(
        lifecycle.callback(&mut app, &Clients::default(), body, body, "on_close", &Parameters::new()),
        Some(Ok(()))
    );
    assert!(lifecycle.updates_stopped);
    assert!(!*server.0.requested.borrow());
    assert_eq!(*lock(&events), ["on_close"]);
    app.gui.set_attr(body, "data-still-alive", Some("yes"));
    assert_eq!(app.gui.attr(body, "data-still-alive"), Some("yes"));
}

#[test]
fn close_routes_and_keyword_binding_apply_only_to_the_application() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let (mut app, body) = App::new(events.clone());
    let other = app.gui.create("button", "Button");
    let mut lifecycle = Lifecycle::new(ServerStop::detached());
    let clients = Clients::default();
    let empty = Parameters::new();
    assert!(lifecycle.callback(&mut app, &clients, body, other, "close", &empty).is_none());
    assert!(lifecycle.attribute(&mut app, &clients, body, "unknown", "close", &[]).is_none());
    assert!(lifecycle.callback(&mut app, &clients, body, body, "onpagehide", &empty).is_none());
    for method in ["close", "on_close"] {
        for (key, message) in [
            ("self", "got multiple values for argument 'self'"),
            ("unexpected", "got an unexpected keyword argument 'unexpected'"),
        ] {
            let params = [(key.into(), "ignored".into())].into_iter().collect();
            assert_eq!(
                lifecycle.callback(&mut app, &clients, body, body, method, &params),
                Some(Err(format!("TypeError: App.{method}() {message}")))
            );
        }
    }
    assert!(lock(&events).is_empty());
    assert!(!lifecycle.updates_stopped);
    let widget = app.gui.identifier(body).to_owned();
    assert_eq!(lifecycle.attribute(&mut app, &clients, body, &widget, "on_close", &[]), Some(Ok(())));
    assert_eq!(lifecycle.callback(&mut app, &clients, body, body, "close", &empty), Some(Ok(())));
    assert_eq!(*lock(&events), ["on_close", "on_close"]);
}

struct PendingCallback(Arc<Mutex<Vec<&'static str>>>);
impl Drop for PendingCallback {
    fn drop(&mut self) {
        lock(&self.0).push("socket_work_dropped");
    }
}

#[tokio::test]
async fn on_close_cancels_the_socket_waiting_for_its_own_callback() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let (mut app, body) = App::new(events.clone());
    let server = ServerStop::detached();
    let stop = server.clone();
    let (close, session) = SocketStop::new();
    let (updates, _received) = tokio::sync::mpsc::unbounded_channel();
    let clients = Mutex::new(vec![Client { updates, close, membership: std::sync::Weak::new() }]);
    let callback = std::thread::spawn(move || {
        let mut lifecycle = Lifecycle::new(stop);
        let result = lifecycle.callback(&mut app, &clients, body, body, "on_close", &Parameters::new());
        lock(&app.events).push("callback_finished");
        (result, lifecycle.updates_stopped)
    });
    let guard = PendingCallback(events.clone());
    session
        .run(async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        })
        .await;
    assert_eq!(callback.join().unwrap(), (Some(Ok(())), true));
    assert!(*server.0.requested.borrow(), "WebSocketsHandler.close also stops HTTP");
    assert_eq!(*lock(&events), ["on_close", "socket_work_dropped", "callback_finished"]);
}

#[tokio::test]
async fn ordinary_socket_disconnect_does_not_call_the_application_close_hook() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let (mut app, _) = App::new(events.clone());
    let server = ServerStop::new();
    let mut lifecycle = Lifecycle::new(server.clone());
    let (close, session) = SocketStop::new();
    let (updates, _received) = tokio::sync::mpsc::unbounded_channel();
    let clients = Mutex::new(vec![Client { updates, close, membership: std::sync::Weak::new() }]);
    session.run(async {}).await;
    assert!(lock(&events).is_empty());
    assert!(!lifecycle.updates_stopped);
    assert!(!*server.0.requested.borrow());
    lifecycle.on_close(&mut app, &clients).unwrap();
    assert!(!*server.0.requested.borrow(), "a disconnected socket is no longer in the native set");
}

#[test]
fn myapp_close_hook_keeps_native_flag_failures_before_superclass_cleanup() {
    use crate::app::tests::FakeBackend;
    use serde_json::json;
    for (settings, expected, calls) in [
        (json!({"app":{"close_button_quits":false}}), Ok(()), Vec::new()),
        (json!({"app":{"close_button_quits":"yes"}}), Ok(()), vec!["desktop/close-files", "desktop/kill"]),
        (json!({}), Err("KeyError: 'app'"), Vec::new()),
        (json!({"app":{}}), Err("KeyError: 'close_button_quits'"), Vec::new()),
        (
            json!({"app":{"close_button_quits":"invalid"}}),
            Err("Exception: Boolean value expected. Got invalid"),
            Vec::new(),
        ),
    ] {
        let backend = FakeBackend::new();
        let mut app = crate::app::MyApp::new(backend.clone());
        backend.docs.lock().unwrap()["settings"] = settings;
        lock(&backend.calls).clear();
        let server = ServerStop::new();
        let mut lifecycle = Lifecycle::new(server.clone());
        let result = lifecycle.on_close(&mut app, &Clients::default());
        assert_eq!(result, expected.map_err(str::to_owned));
        assert_eq!(lifecycle.updates_stopped, expected.is_ok());
        assert!(!*server.0.requested.borrow());
        assert_eq!(
            lock(&backend.calls)
                .iter()
                .filter(|call| call.1.starts_with("desktop/"))
                .map(|call| call.1.as_str())
                .collect::<Vec<_>>(),
            calls
        );
    }
}
