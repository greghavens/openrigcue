//! The server and application runtime for one shared application: the server thread
//! handling page requests, static files, attribute calls and the websocket under `update_lock`, the GUI update
//! every `update_interval`, and the threads the application starts itself (the main loop).
//!
//! The application starts with the service; every connected browser receives the same updates.

use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::time::{Duration, Instant};

use axum::extract::Request;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use base64::Engine;
use sha1::{Digest, Sha1};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc as tokio_mpsc, oneshot};

mod http;
mod lifecycle;
mod paths;
mod session;
pub use http::{serve, serve_with_stop};
pub use lifecycle::ServerStop;
use lifecycle::{Lifecycle, SocketSession, SocketStop};
mod update_lock;
mod websocket;
pub use update_lock::UpdateLock;
use websocket::Socket;

use crate::remi::client::{self, Incoming};
use crate::remi::gui::{dispatch_checked, HasGui};
use crate::remi::parameters::{parse_parameters_checked, ParameterError, Parameters};
use crate::remi::WidgetId;

/// The interval between GUI update passes: 0.1 s.
pub const UPDATE_INTERVAL: Duration = Duration::from_millis(100);

/// The application state; the reentrant update lock is independent.
pub type Shared<A> = Arc<Mutex<A>>;

/// Takes the application-state mutex.
pub fn lock<A>(shared: &Mutex<A>) -> MutexGuard<'_, A> {
    shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A named thread the application starts; it reaches the application
/// through the weak handle and ends once the application is gone.
pub struct AppThread<A> {
    pub name: &'static str,
    pub run: Box<dyn FnOnce(Weak<Mutex<A>>) + Send>,
}

impl<A: Send + 'static> AppThread<A> {
    pub(crate) fn start(self, app: Weak<Mutex<A>>) {
        let run = self.run;
        if let Err(error) = std::thread::Builder::new().name(self.name.into()).spawn(move || run(app)) {
            tracing::error!(%error, name = self.name, "thread not started");
        }
    }
}

/// What a widget method called over HTTP answers (`[content, headers]`).
pub struct AttributeAnswer {
    pub content: Vec<u8>,
    pub headers: Vec<(String, String)>,
}

/// An application run by the host (`MyApp`).
pub trait UiApp: HasGui + Send + Sized + 'static {
    /// Attach independent application timers before constructing the page.
    fn attach_shared(&mut self, _shared: Weak<Mutex<Self>>) {}
    /// The shared reentrant update lock, obtained once when the host starts.
    fn update_lock(&self) -> Option<Arc<UpdateLock>> {
        None
    }
    /// One decoded websocket callback, with the parameter order as received.
    fn callback(&mut self, id: WidgetId, event: &str, parameters: &Parameters) -> Result<(), String> {
        dispatch_checked(self, id, event, &parameters.iter().map(|(key, value)| (key.clone(), value.clone())).collect())
    }
    /// Runs before the host applies its own update/socket
    /// cleanup on close. An error prevents that cleanup.
    fn on_close(&mut self) -> Result<(), String> {
        Ok(())
    }
    fn close_class_name(&self) -> &'static str {
        "App"
    }
    /// Runs once after the root exists, before any browser connects; `MyApp`
    /// does not use it (its frame, page constructors and threads belong to the first page's `onload`). A
    /// change made after a browser's handshake render and before its first message reaches no socket
    /// (the handshake renders, the first message registers), so an application using this hook must
    /// only do what a page render carries whole.
    fn start(&mut self) {}
    /// Builds and returns the root widget; `body` is the page body,
    /// whose events (`onload`, `onpageshow`, …) the application connects.
    fn main(&mut self, body: WidgetId) -> WidgetId;
    /// The window title.
    fn title(&self) -> String;
    /// What the application adds to the page head.
    fn head(&self) -> String;
    /// The body's `loading` container (the default one unless `main` replaced it).
    fn loading_html(&mut self) -> String {
        REMI_LOADING.to_owned()
    }
    /// One idle pass, after an [`UPDATE_INTERVAL`] sleep. Request
    /// handling and the preceding pass can delay it further.
    fn poll(&mut self, now: Instant);
    /// A request for `/<widget identifier>/<method>?<parameters>`: the method's answer, `None` when no
    /// such widget or method exists.
    fn attribute_call(&mut self, widget: &str, method: &str, params: &[(String, String)]) -> Option<AttributeAnswer> {
        let _ = (widget, method, params);
        None
    }
    /// The attribute-call boundary logs errors before emitting any response.
    fn attribute_call_checked(
        &mut self,
        widget: &str,
        method: &str,
        params: &[(String, String)],
    ) -> Result<Option<AttributeAnswer>, String> {
        Ok(self.attribute_call(widget, method, params))
    }
    /// The threads the application asked to start since the last call.
    fn take_threads(&mut self) -> Vec<AppThread<Self>> {
        Vec::new()
    }
}

/// The body's default loading container.
const REMI_LOADING: &str =
    "<div id=\"loading\" class=\"Container\" style=\"display:none\"><div id=\"loading-animation\" class=\"Widget\"></div></div>";

/// A browser's update channel and its first message (the whole body).
type Connection = (String, tokio_mpsc::UnboundedReceiver<Outbound>, SocketSession, Client);
type Clients = Mutex<Vec<Client>>;

#[derive(Clone)]
struct Client {
    updates: tokio_mpsc::UnboundedSender<Outbound>,
    close: SocketStop,
    membership: Weak<Clients>,
}

struct MembershipGuard(Client);

impl Drop for MembershipGuard {
    fn drop(&mut self) {
        if let Some(clients) = self.0.membership.upgrade() {
            lock(&clients).retain(|client| !client.updates.same_channel(&self.0.updates));
        }
    }
}

struct Outbound {
    text: String,
    delivered: mpsc::Sender<bool>,
}

fn broadcast(clients: &Clients, text: &str) {
    // Iterate a copy of the websocket list, so that membership can change
    // during a send from an independent worker or timer.
    let recipients = lock(clients).clone();
    for client in recipients {
        let (delivered, completion) = mpsc::channel();
        if client.updates.send(Outbound { text: text.to_owned(), delivered }).is_ok()
            && completion.recv().unwrap_or(false)
        {
            continue;
        }
        let removed = {
            let mut clients = lock(clients);
            clients.iter().position(|current| current.updates.same_channel(&client.updates)).map(|index| {
                clients.remove(index);
            })
        };
        // The close is skipped when another sender already removed it.
        // Otherwise the close completes before the next send.
        if removed.is_some() {
            client.close.close();
        }
    }
}

async fn send_outbound<S: AsyncRead + AsyncWrite + Unpin>(socket: &mut Socket<S>, outbound: Outbound) -> bool {
    let success = socket.send_text(&outbound.text).await.unwrap_or(false);
    let _ = outbound.delivered.send(success);
    success
}

enum ToUi {
    #[cfg(test)]
    ClientCount(oneshot::Sender<usize>),
    RequestReady(oneshot::Sender<()>),
    Page(oneshot::Sender<String>),
    Connected(oneshot::Sender<Connection>),
    OnClose(mpsc::Sender<Result<(), String>>),
    Message {
        text: String,
        client: Option<Client>,
        complete: oneshot::Sender<()>,
    },
    Attribute {
        widget: String,
        method: String,
        params: Vec<(String, String)>,
        reply: oneshot::Sender<Option<AttributeAnswer>>,
    },
}

type Spawn = Box<dyn FnOnce(mpsc::Receiver<ToUi>) + Send>;

/// The running UI (shared by the routes).
#[derive(Clone)]
pub struct UiHost {
    inner: Arc<HostInner>,
}

struct HostInner {
    spawn: Mutex<Option<Spawn>>,
    to_ui: OnceLock<Mutex<mpsc::Sender<ToUi>>>,
}

impl UiHost {
    /// Starts one shared application on the server thread immediately.
    pub fn new<A: UiApp, F>(factory: F) -> UiHost
    where
        F: FnOnce() -> A + Send + 'static,
    {
        Self::with_server_stop(factory, ServerStop::detached())
    }

    pub fn with_server_stop<A: UiApp, F>(factory: F, server: ServerStop) -> UiHost
    where
        F: FnOnce() -> A + Send + 'static,
    {
        let spawn: Spawn = Box::new(move |rx| {
            let result = std::thread::Builder::new()
                .name("ui".into())
                .spawn(move || run_shared_with_server(Arc::new(Mutex::new(factory())), rx, server));
            if let Err(error) = result {
                tracing::error!(%error, "UI thread not started");
            }
        });
        let host = UiHost { inner: Arc::new(HostInner { spawn: Mutex::new(Some(spawn)), to_ui: OnceLock::new() }) };
        host.sender();
        host
    }

    fn sender(&self) -> mpsc::Sender<ToUi> {
        let cell = self.inner.to_ui.get_or_init(|| {
            let (tx, rx) = mpsc::channel();
            if let Some(spawn) = lock(&self.inner.spawn).take() {
                spawn(rx);
            }
            Mutex::new(tx)
        });
        lock(cell).clone()
    }

    /// A request for `/`: the page.
    pub async fn page(&self) -> Option<String> {
        let (tx, rx) = oneshot::channel();
        self.sender().send(ToUi::Page(tx)).ok()?;
        rx.await.ok()
    }

    async fn request_ready(&self) -> bool {
        let (reply, ready) = oneshot::channel();
        if self.sender().send(ToUi::RequestReady(reply)).is_err() {
            return false;
        }
        ready.await.is_ok()
    }

    async fn connected(&self) -> Option<Connection> {
        let (tx, rx) = oneshot::channel();
        self.sender().send(ToUi::Connected(tx)).ok()?;
        rx.await.ok()
    }

    async fn attribute(
        &self,
        widget: String,
        method: String,
        params: Vec<(String, String)>,
    ) -> Option<AttributeAnswer> {
        let (reply, rx) = oneshot::channel();
        self.sender().send(ToUi::Attribute { widget, method, params, reply }).ok()?;
        rx.await.ok().flatten()
    }

    #[cfg(test)]
    pub(crate) async fn message(&self, text: String) -> bool {
        self.socket_message(text, None).await
    }

    async fn socket_message(&self, text: String, client: Option<Client>) -> bool {
        let (complete, reply) = oneshot::channel();
        if self.sender().send(ToUi::Message { text, client, complete }).is_err() {
            return false;
        }
        reply.await.is_ok()
    }

    /// Stopping the server calls the application's on_close after HTTP acceptance has stopped.
    pub fn on_close(&self) -> Result<(), String> {
        let (reply, result) = mpsc::channel();
        self.sender().send(ToUi::OnClose(reply)).map_err(|_| "UI not running".to_owned())?;
        result.recv().map_err(|_| "UI not running".to_owned())?
    }
}

/// The server thread: request handling under `update_lock` and the idle loop.
#[cfg(test)]
fn run_shared<A: UiApp>(shared: Shared<A>, rx: mpsc::Receiver<ToUi>) {
    run_shared_with_server(shared, rx, ServerStop::detached());
}

fn run_shared_with_server<A: UiApp>(shared: Shared<A>, rx: mpsc::Receiver<ToUi>, server: ServerStop) {
    let mut lifecycle = Lifecycle::new(server);
    lock(&shared).attach_shared(Arc::downgrade(&shared));
    let update_lock = lock(&shared).update_lock();
    let clients: Arc<Clients> = Arc::new(Mutex::new(Vec::new()));
    let spontaneous_clients = clients.clone();
    lock(&shared).gui().set_javascript_sink(Arc::new(move |code| {
        broadcast(&spontaneous_clients, &client::javascript_message(code));
    }));
    let body = lock(&shared).gui().create("body", "BODY");
    // The stylesheet's `.remi-main #loading` overlay rules rely on this class.
    lock(&shared).gui().add_class(body, "remi-main");
    let (loading, root) = {
        let mut app = lock(&shared);
        let root = app.main(body);
        app.gui().set_root(root);
        app.start();
        (app.loading_html(), root)
    };
    let mut next_update = Instant::now() + UPDATE_INTERVAL;
    loop {
        let timeout = if lifecycle.updates_stopped {
            UPDATE_INTERVAL
        } else {
            next_update.saturating_duration_since(Instant::now()).min(UPDATE_INTERVAL)
        };
        let received = rx.recv_timeout(timeout);
        let _update_guard = update_lock.as_ref().map(|gate| gate.lock());
        let mut app = lock(&shared);
        match received {
            #[cfg(test)]
            Ok(ToUi::ClientCount(reply)) => {
                let _ = reply.send(lock(&clients).len());
            }
            Ok(ToUi::RequestReady(reply)) => {
                let _ = reply.send(());
            }
            Ok(ToUi::Page(reply)) => {
                let inner = format!("{loading}{}", app.gui().full_html());
                let identifier = app.gui().identifier(body).to_owned();
                let head_extra = app.head();
                let title = app.title();
                let attributes = app.gui().attributes_html(body);
                let page = client::page(
                    &client::PageHead { title: &title, app_identifier: &identifier, extra: &head_extra },
                    &attributes,
                    &inner,
                );
                let _ = reply.send(page);
            }
            Ok(ToUi::Connected(reply)) => {
                // The handshake is done: the whole body, then every update.
                let html = app.gui().full_html();
                let first =
                    format!("0{},{}", app.gui().identifier(root), client::to_websocket(&format!("{loading}{html}")));
                let (tx, updates) = tokio_mpsc::unbounded_channel();
                let (close, session) = SocketStop::new();
                let _ = reply.send((
                    first,
                    updates,
                    session,
                    Client { updates: tx, close, membership: Arc::downgrade(&clients) },
                ));
            }
            Ok(ToUi::OnClose(reply)) => {
                let _ = reply.send(lifecycle.on_close(&mut *app, &clients));
            }
            Ok(ToUi::Message { text, client, complete }) => {
                // A message registers the socket only after its ACK and under
                // update_lock, including ordinary keepalive/non-callback text.
                if let Some(client) = client {
                    let mut clients = lock(&clients);
                    if !clients.iter().any(|current| current.updates.same_channel(&client.updates)) {
                        clients.push(client);
                    }
                }
                if let Incoming::Callback { widget, event, params } = client::parse_incoming(&text) {
                    match parse_parameters_checked(&params) {
                        Ok(params) => {
                            if let Some(id) = app.gui().by_identifier(&widget) {
                                let close = lifecycle.callback(&mut *app, &clients, body, id, &event, &params);
                                if let Err(error) = close.unwrap_or_else(|| app.callback(id, &event, &params)) {
                                    tracing::error!(%error,"error parsing websocket");
                                }
                            } else {
                                let error = format!("KeyError: {}", openrigcue_model::python_repr::repr_str(&widget));
                                tracing::error!(%error,"error parsing websocket");
                            }
                        }
                        Err(error @ ParameterError::ValueError(_)) => tracing::error!(%error,"error parsing websocket"),
                        Err(ParameterError::NonProgress { parameters }) => {
                            // Parsing this input never ends, with update_lock held.
                            // Keep the guard, partial parameters, reply and subsequent
                            // requests alive; no callback, GUI poll or other UI
                            // request can proceed. Wait rather than burning CPU.
                            let _stalled = (complete, parameters, params);
                            if update_lock.is_some() {
                                drop(app);
                            }
                            let mut waiting = Vec::new();
                            while let Ok(request) = rx.recv() {
                                waiting.push(request);
                            }
                            return;
                        }
                    }
                }
                let _ = complete.send(());
            }
            Ok(ToUi::Attribute { widget, method, params, reply }) => {
                let close = lifecycle.attribute(&mut *app, &clients, body, &widget, &method, &params);
                let answer = match close {
                    Some(result) => {
                        // The close methods give no content and headers to answer with, so
                        // the request ends before any header is written, leaving no wire
                        // response. All preceding lifecycle effects are kept.
                        match result {
                            Ok(()) => tracing::debug!(target: "remi.server", "attr {widget}/{method} not available"),
                            Err(error) if error.starts_with("TypeError:") || error.starts_with("AttributeError:") => {
                                tracing::debug!(target: "remi.server", "attr {widget}/{method} not available");
                            }
                            Err(error) => tracing::error!(%error, "error processing GET request"),
                        }
                        None
                    }
                    None => checked_attribute(&mut *app, &widget, &method, &params),
                };
                let _ = reply.send(answer);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        let now = Instant::now();
        let idle_due = !lifecycle.updates_stopped && now >= next_update;
        if idle_due {
            app.poll(now);
        }
        for thread in app.take_threads() {
            thread.start(Arc::downgrade(&shared));
        }
        let mut messages: Vec<String> =
            app.gui().take_javascript().iter().map(|c| client::javascript_message(c)).collect();
        if idle_due && app.gui().needs_update() {
            messages.extend(app.gui().gui_update());
        }
        drop(app);
        for message in messages {
            broadcast(&clients, &message);
        }
        if idle_due {
            // The idle loop sleeps before every pass, after idle/the GUI update
            // and all synchronous websocket sends in the previous pass.
            next_update = Instant::now() + UPDATE_INTERVAL;
        }
    }
}

fn checked_attribute<A: UiApp>(
    app: &mut A,
    widget: &str,
    method: &str,
    params: &[(String, String)],
) -> Option<AttributeAnswer> {
    match app.attribute_call_checked(widget, method, params) {
        Ok(answer) => answer,
        Err(error) => {
            if error.starts_with("TypeError:") || error.starts_with("AttributeError:") {
                tracing::debug!(target: "remi.server", "attr {widget}/{method} not available");
            } else {
                tracing::error!(%error, "error processing GET request");
            }
            None
        }
    }
}

/// The server routes: the page and the websocket at `/` (both at the same path), the static files
/// (`/<key>:<path>`, `static_file` answering them) and the widget attribute calls (`/<identifier>/<method>`).
/// Use [`serve`] to keep outcomes that close without an HTTP response.
pub fn router<S: Clone + Send + Sync + 'static>(
    host: UiHost,
    static_file: fn(&str, &str) -> Option<Response>,
) -> Router<S> {
    let page_host = host.clone();
    Router::new()
        .route(
            "/",
            get(move |request: Request| {
                let host = page_host.clone();
                async move { http::native_response(root(host, static_file, request).await) }
            })
            .head(|| async { http::native_response(StatusCode::OK.into_response()) }),
        )
        .route(
            "/{*path}",
            get(move |request: Request| {
                let host = host.clone();
                async move { http::native_response(root(host, static_file, request).await) }
            })
            .head(|| async { http::native_response(StatusCode::OK.into_response()) }),
        )
}

async fn root(host: UiHost, static_file: fn(&str, &str) -> Option<Response>, request: Request) -> Response {
    let (parts, _body) = request.into_parts();
    let upgrade = http::request_header(&parts, "upgrade").is_some_and(|value| value.eq_ignore_ascii_case(b"websocket"));
    if upgrade {
        if !session::matches_shared_client(http::request_header(&parts, "cookie")) {
            return http::close_without_response();
        }
        let upgraded = parts.extensions.get::<hyper::upgrade::OnUpgrade>().cloned();
        let key = http::request_header(&parts, "sec-websocket-key");
        let (Some(upgraded), Some(key)) = (upgraded, key) else {
            return http::close_without_response();
        };
        // The key is hashed as UTF-8 after HTTP's ISO-8859-1 header decoding.
        let key: String = key.iter().copied().map(char::from).collect();
        let mut digest = Sha1::new();
        digest.update(key.as_bytes());
        digest.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
        let accept = base64::engine::general_purpose::STANDARD.encode(digest.finalize());
        tokio::spawn(async move {
            if let Ok(stream) = upgraded.await {
                serve_socket(host, hyper_util::rt::TokioIo::new(stream)).await;
            }
        });
        return (
            StatusCode::SWITCHING_PROTOCOLS,
            [
                (header::UPGRADE, "websocket".to_owned()),
                (header::CONNECTION, "Upgrade".to_owned()),
                (header::SEC_WEBSOCKET_ACCEPT, accept),
            ],
        )
            .into_response();
    }

    // Every GET checks the already-built root under update_lock.
    if !host.request_ready().await {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let target = parts
        .extensions
        .get::<http::NativeTarget>()
        .map_or_else(|| parts.uri.path_and_query().map_or("/", |value| value.as_str()), |target| target.0.as_str());
    match paths::classify(target) {
        paths::Route::Page => match host.page().await {
            Some(page) => http::native_headers(
                page.into_response(),
                [("Set-Cookie".into(), session::PAGE_COOKIE.into()), ("Content-type".into(), "text/html".into())],
            ),
            None => (StatusCode::SERVICE_UNAVAILABLE, "UI not running").into_response(),
        },
        paths::Route::Static { namespace, file } => {
            let Some(response) = static_file(&namespace, &file) else { return http::close_without_response() };
            let headers = [("Content-type", header::CONTENT_TYPE), ("Cache-Control", header::CACHE_CONTROL)]
                .into_iter()
                .filter_map(|(spelling, name)| {
                    response
                        .headers()
                        .get(name)
                        .map(|value| (spelling.to_owned(), String::from_utf8_lossy(value.as_bytes()).into_owned()))
                })
                .collect::<Vec<_>>();
            http::native_headers(response, headers)
        }
        paths::Route::Attribute { widget, method, parameters } => {
            let Some(answer) = host.attribute(widget, method, parameters).await else {
                return http::close_without_response();
            };
            http::native_headers(answer.content.into_response(), answer.headers)
        }
        paths::Route::NoResponse => http::close_without_response(),
    }
}

/// The websocket handler: the body on handshake, every update afterwards, an ACK for each message received.
async fn serve_socket<S: AsyncRead + AsyncWrite + Unpin>(host: UiHost, stream: S) {
    let Some((first, updates, session, client)) = host.connected().await else { return };
    // Cancel the whole socket future, including a wait for its own callback.
    // Dropping it closes the stream before the session acknowledges the close.
    session.run(serve_socket_session(host, stream, first, updates, client, websocket::WRITE_READY_TIMEOUT)).await;
}

async fn serve_socket_session<S: AsyncRead + AsyncWrite + Unpin>(
    host: UiHost,
    stream: S,
    first: String,
    mut updates: tokio_mpsc::UnboundedReceiver<Outbound>,
    client: Client,
    initial_timeout: Duration,
) {
    // The reader removes itself from the application's websocket set when
    // reading ends, independently of any later spontaneous send.
    let _membership = MembershipGuard(client.clone());
    let mut socket = Socket::new(stream);
    // The handshake ignores a readiness timeout; send
    // errors still unwind before a message can register this socket.
    if !first.is_empty() && socket.send_with_timeout(&first, initial_timeout).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            update = updates.recv() => match update {
                Some(outbound) => {
                    if !send_outbound(&mut socket,outbound).await {
                        return;
                    }
                }
                None => return,
            },
            incoming = socket.recv() => match incoming {
                Ok(Some(text)) => {
                    // The message handler ignores a readiness timeout; send errors
                    // still end the reader before acquiring update_lock.
                    if socket.send_text(client::MSG_ACK).await.is_err() {
                        return;
                    }
                    // ACK precedes execution, while the next socket read waits
                    // for the message handler to return.
                    let processing = host.socket_message(text, Some(client.clone()));
                    tokio::pin!(processing);
                    loop {
                        tokio::select! {
                            biased;
                            update = updates.recv() => match update {
                                Some(outbound) => if !send_outbound(&mut socket,outbound).await { return; },
                                None => return,
                            },
                            complete = &mut processing => { if !complete { return; } break; }
                        }
                    }
                }
                Ok(None) => return,
                Err(error) => {
                    tracing::error!(%error, "Error managing incoming websocket message");
                    return;
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct IdleProbeApp {
        gui: crate::remi::Gui<Self>,
        root: Option<WidgetId>,
        passes: usize,
        polled: tokio_mpsc::UnboundedSender<Instant>,
    }

    impl HasGui for IdleProbeApp {
        fn gui(&mut self) -> &mut crate::remi::Gui<Self> {
            &mut self.gui
        }
    }

    impl UiApp for IdleProbeApp {
        fn main(&mut self, _body: WidgetId) -> WidgetId {
            let root = self.gui.create("p", "Label");
            self.gui.set_text(root, "idle");
            self.root = Some(root);
            root
        }
        fn title(&self) -> String {
            "idle fixture".into()
        }
        fn head(&self) -> String {
            String::new()
        }
        fn poll(&mut self, _now: Instant) {
            self.passes += 1;
            // A new base text each pass: an unchanged shown text would flag no update.
            self.gui.set_text(self.root.unwrap(), &format!("idle {}", self.passes));
            let _ = self.polled.send(Instant::now());
        }
    }

    fn idle_probe() -> (UiHost, tokio_mpsc::UnboundedReceiver<Instant>) {
        let (polled, received) = tokio_mpsc::unbounded_channel();
        let host = UiHost::new(move || IdleProbeApp {
            gui: crate::remi::Gui::new(Arc::new(crate::remi::Untranslated)),
            root: None,
            passes: 0,
            polled,
        });
        (host, received)
    }

    async fn client_count(host: &UiHost) -> usize {
        let (reply, received) = oneshot::channel();
        host.sender().send(ToUi::ClientCount(reply)).unwrap();
        received.await.unwrap()
    }

    #[tokio::test]
    async fn disconnected_reader_discards_membership_before_any_later_broadcast() {
        for bytes in [Vec::new(), vec![0x81, 127, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]] {
            let (called, _received) = mpsc::channel();
            let host = UiHost::new(move || FrameApp {
                gui: crate::remi::Gui::new(Arc::new(crate::remi::Untranslated)),
                called,
            });
            let (first, updates, session, client) = host.connected().await.unwrap();
            assert!(host.socket_message("connected".into(), Some(client.clone())).await);
            assert_eq!(client_count(&host).await, 1);
            let (socket, mut peer) = tokio::io::duplex(16384);
            peer.write_all(&bytes).await.unwrap();
            peer.shutdown().await.unwrap();
            session
                .run(serve_socket_session(host.clone(), socket, first, updates, client, Duration::from_secs(5)))
                .await;
            assert_eq!(client_count(&host).await, 0, "reader termination must discard without a later send");
        }
    }

    #[tokio::test]
    async fn idle_waits_one_interval_despite_pending_requests() {
        let started = Instant::now();
        let (host, mut polled) = idle_probe();
        for _ in 0..8 {
            host.page().await.unwrap();
        }
        let first = tokio::time::timeout(Duration::from_secs(5), polled.recv()).await.unwrap().unwrap();
        assert!(first.duration_since(started) >= UPDATE_INTERVAL, "requests must not run idle early");
    }

    #[tokio::test]
    async fn slow_gui_send_is_followed_by_a_full_idle_interval() {
        let (host, _polled) = idle_probe();
        host.page().await.unwrap();
        let (_, mut updates, _session, client) = host.connected().await.unwrap();
        assert!(host.socket_message("connected".into(), Some(client)).await);
        let first = tokio::time::timeout(Duration::from_secs(5), updates.recv()).await.unwrap().unwrap();
        assert!(first.text.starts_with('1'));
        tokio::time::sleep(UPDATE_INTERVAL * 2).await;
        let completed = Instant::now();
        first.delivered.send(true).unwrap();
        let second = tokio::time::timeout(Duration::from_secs(5), updates.recv()).await.unwrap().unwrap();
        let elapsed = completed.elapsed();
        second.delivered.send(true).unwrap();
        assert!(elapsed >= UPDATE_INTERVAL, "idle resumed {elapsed:?} after completing a slow GUI update");
    }

    #[tokio::test]
    async fn spontaneous_send_uses_snapshot_without_holding_client_membership_lock() {
        let (first_tx, mut first_rx) = tokio_mpsc::unbounded_channel();
        let (first_close, _first_session) = SocketStop::new();
        let clients =
            Arc::new(Mutex::new(vec![Client { updates: first_tx, close: first_close, membership: Weak::new() }]));
        let worker_clients = clients.clone();
        let worker = std::thread::spawn(move || broadcast(&worker_clients, "first pass"));
        let first = first_rx.recv().await.unwrap();
        let (second_tx, mut second_rx) = tokio_mpsc::unbounded_channel();
        let (second_close, _second_session) = SocketStop::new();
        let available = if let Ok(mut clients) = clients.try_lock() {
            clients.push(Client { updates: second_tx, close: second_close, membership: Weak::new() });
            true
        } else {
            false
        };
        first.delivered.send(true).unwrap();
        worker.join().unwrap();
        assert!(available, "membership changes must not wait for a blocked spontaneous send");
        assert!(second_rx.try_recv().is_err(), "a client added after the snapshot starts on the next send");
        let worker_clients = clients.clone();
        let worker = std::thread::spawn(move || broadcast(&worker_clients, "second pass"));
        first_rx.recv().await.unwrap().delivered.send(true).unwrap();
        second_rx.recv().await.unwrap().delivered.send(true).unwrap();
        worker.join().unwrap();
    }

    #[tokio::test]
    async fn failed_spontaneous_send_removes_then_closes_before_next_recipient() {
        let (first_tx, mut first_rx) = tokio_mpsc::unbounded_channel();
        let (first_close, first_session) = SocketStop::new();
        let (second_tx, mut second_rx) = tokio_mpsc::unbounded_channel();
        let (second_close, _second_session) = SocketStop::new();
        let clients = Arc::new(Mutex::new(vec![
            Client { updates: first_tx, close: first_close, membership: Weak::new() },
            Client { updates: second_tx, close: second_close, membership: Weak::new() },
        ]));
        let worker_clients = clients.clone();
        let worker = std::thread::spawn(move || broadcast(&worker_clients, "pass"));
        first_rx.recv().await.unwrap().delivered.send(false).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if clients.try_lock().is_ok_and(|clients| clients.len() == 1) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(second_rx.try_recv().is_err(), "next recipient must wait for failed socket close");
        drop(first_session);
        second_rx.recv().await.unwrap().delivered.send(true).unwrap();
        worker.join().unwrap();
        assert_eq!(lock(&clients).len(), 1);
    }

    #[tokio::test]
    async fn failed_snapshot_recipient_already_removed_skips_close_and_continues() {
        let (first_tx, mut first_rx) = tokio_mpsc::unbounded_channel();
        let (first_close, _first_session) = SocketStop::new();
        let (second_tx, mut second_rx) = tokio_mpsc::unbounded_channel();
        let (second_close, _second_session) = SocketStop::new();
        let clients = Arc::new(Mutex::new(vec![
            Client { updates: first_tx, close: first_close, membership: Weak::new() },
            Client { updates: second_tx, close: second_close, membership: Weak::new() },
        ]));
        let worker_clients = clients.clone();
        let worker = std::thread::spawn(move || broadcast(&worker_clients, "pass"));
        let first = first_rx.recv().await.unwrap();
        lock(&clients).remove(0);
        first.delivered.send(false).unwrap();
        // Removing a socket another sender already removed is an absorbed error that
        // continues without close. Keep the first session alive to detect a wait.
        tokio::time::timeout(Duration::from_secs(2), second_rx.recv())
            .await
            .unwrap()
            .unwrap()
            .delivered
            .send(true)
            .unwrap();
        worker.join().unwrap();
        assert_eq!(lock(&clients).len(), 1);
    }

    #[tokio::test]
    async fn native_transfer_checked_failure_reaches_http_without_headers() {
        use std::future::Future;
        use std::task::{Context, Poll};
        use tower::ServiceExt;
        let backend = crate::app::tests::FakeBackend::new();
        let mut app = crate::app::MyApp::new(backend);
        app.downloads.insert("download".into(), ("all.zip".into(), b"zip".to_vec()));
        app.export_all_downloads.insert("download".into(), 1);

        // Drive only the request metadata. No host, application or socket worker.
        let (send, receive) = mpsc::channel();
        let to_ui = OnceLock::new();
        to_ui.set(Mutex::new(send)).unwrap();
        let host = UiHost { inner: Arc::new(HostInner { spawn: Mutex::new(None), to_ui }) };
        let request = Request::builder().uri("/download/download").body(axum::body::Body::empty()).unwrap();
        let route = router::<()>(host, |_, _| None).oneshot(request);
        tokio::pin!(route);
        let mut context = Context::from_waker(std::task::Waker::noop());
        assert!(matches!(route.as_mut().poll(&mut context), Poll::Pending));
        let ToUi::RequestReady(ready) = receive.try_recv().unwrap() else { panic!("request-ready metadata") };
        ready.send(()).unwrap();
        assert!(matches!(route.as_mut().poll(&mut context), Poll::Pending));
        let ToUi::Attribute { widget, method, params, reply } = receive.try_recv().unwrap() else {
            panic!("attribute metadata")
        };
        let answer = checked_attribute(&mut app, &widget, &method, &params);
        assert!(answer.is_none());
        assert_eq!(app.export_all_downloads["download"], 2);
        assert!(reply.send(answer).is_ok());
        let response = route.await.unwrap();
        assert!(response.extensions().get::<http::CloseWithoutResponse>().is_some());
    }

    #[tokio::test]
    async fn static_and_unknown_gets_take_the_ui_lock_while_head_bypasses_it() {
        use tower::ServiceExt;
        let (called, _received) = mpsc::channel();
        let host =
            UiHost::new(move || FrameApp { gui: crate::remi::Gui::new(Arc::new(crate::remi::Untranslated)), called });
        let page = host.page().await.unwrap();
        let body = page.split("<body").nth(1).unwrap().split("id=\"").nth(1).unwrap().split('"').next().unwrap();
        let app = router::<()>(host.clone(), |_, _| Some((StatusCode::OK, "fixture").into_response()));
        let request = |method: &str, path: &str| {
            Request::builder().method(method).uri(path).body(axum::body::Body::empty()).unwrap()
        };
        assert_eq!(app.clone().oneshot(request("GET", "/res:style.css")).await.unwrap().status(), StatusCode::OK);
        let mut stalled = Box::pin(host.message(format!("callback/{body}/onclick/0|x=1")));
        assert!(tokio::time::timeout(Duration::from_millis(20), &mut stalled).await.is_err());
        for path in ["/res:style.css", "/unknown"] {
            assert!(tokio::time::timeout(Duration::from_millis(20), app.clone().oneshot(request("GET", path)))
                .await
                .is_err());
            let response = tokio::time::timeout(Duration::from_secs(5), app.clone().oneshot(request("HEAD", path)))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
        drop(stalled);
    }

    struct FrameApp {
        gui: crate::remi::Gui<Self>,
        called: mpsc::Sender<Parameters>,
    }
    impl HasGui for FrameApp {
        fn gui(&mut self) -> &mut crate::remi::Gui<Self> {
            &mut self.gui
        }
    }
    impl UiApp for FrameApp {
        fn update_lock(&self) -> Option<Arc<UpdateLock>> {
            // This fixture has no application workers; only its host uses the gate.
            Some(Arc::new(UpdateLock::default()))
        }
        fn main(&mut self, _body: WidgetId) -> WidgetId {
            let label = self.gui.create("p", "Label");
            self.gui.set_text(label, "fixture");
            label
        }
        fn title(&self) -> String {
            "fixture".into()
        }
        fn head(&self) -> String {
            String::new()
        }
        fn poll(&mut self, _now: Instant) {}
        fn callback(&mut self, _id: WidgetId, _event: &str, parameters: &Parameters) -> Result<(), String> {
            self.gui.execute_javascript("é🚗");
            self.called.send(parameters.clone()).unwrap();
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_handshaken_socket_receives_no_broadcast_before_its_first_message() {
        let (called, _received) = mpsc::channel();
        let host =
            UiHost::new(move || FrameApp { gui: crate::remi::Gui::new(Arc::new(crate::remi::Untranslated)), called });
        let page = host.page().await.unwrap();
        let body = page.split("<body").nth(1).unwrap().split("id=\"").nth(1).unwrap().split('"').next().unwrap();
        let connection = host.connected().await.unwrap();
        let mut updates = connection.1;
        let message = host.message(format!("callback/{body}/onclick/"));
        tokio::pin!(message);
        tokio::select! {
            Some(update) = updates.recv() => {
                update.delivered.send(true).unwrap();
                panic!("handshake alone registered a broadcast client");
            },
            done = &mut message => assert!(done),
        }
        assert!(tokio::time::timeout(Duration::from_millis(20), updates.recv()).await.is_err());
    }

    struct PendingApp {
        gui: crate::remi::Gui<Self>,
        label: Option<WidgetId>,
        clicks: usize,
    }
    impl HasGui for PendingApp {
        fn gui(&mut self) -> &mut crate::remi::Gui<Self> {
            &mut self.gui
        }
    }
    impl UiApp for PendingApp {
        fn update_lock(&self) -> Option<Arc<UpdateLock>> {
            Some(Arc::new(UpdateLock::default()))
        }
        fn main(&mut self, _body: WidgetId) -> WidgetId {
            let label = self.gui.create("p", "Label");
            self.gui.set_text(label, "start");
            self.label = Some(label);
            label
        }
        fn title(&self) -> String {
            "fixture".into()
        }
        fn head(&self) -> String {
            String::new()
        }
        fn poll(&mut self, _now: Instant) {}
        fn callback(&mut self, _id: WidgetId, _event: &str, _parameters: &Parameters) -> Result<(), String> {
            self.clicks += 1;
            let text = format!("click{}", self.clicks);
            self.gui.set_text(self.label.unwrap(), &text);
            Ok(())
        }
    }

    /// The page is rendered for a GET and for a websocket handshake with a throwaway changed-widget
    /// dictionary, and rendering marks every rendered widget as updated: changes waiting for the next
    /// GUI update pass are gone for the browsers already connected.
    #[tokio::test]
    async fn another_browsers_page_render_drops_updates_pending_for_connected_clients() {
        let host = UiHost::new(|| PendingApp {
            gui: crate::remi::Gui::new(Arc::new(crate::remi::Untranslated)),
            label: None,
            clicks: 0,
        });
        let page = host.page().await.unwrap();
        let body =
            page.split("<body").nth(1).unwrap().split("id=\"").nth(1).unwrap().split('"').next().unwrap().to_owned();
        let (_, mut updates, _session, client) = host.connected().await.unwrap();
        assert!(host.socket_message("connected".into(), Some(client.clone())).await);
        // Control: with no other browser the change arrives on the next update pass.
        assert!(host.socket_message(format!("callback/{body}/onclick/"), Some(client.clone())).await);
        let update = tokio::time::timeout(Duration::from_secs(2), updates.recv()).await.unwrap().unwrap();
        assert!(update.text.contains("click1"), "{}", update.text);
        update.delivered.send(true).unwrap();
        // The callback and the other browser's page request are queued together; an update pass falling
        // between the two is possible, so a few attempts are allowed and one dropped update is required.
        let mut dropped = false;
        for attempt in 2..7 {
            let (called, rendered) = tokio::join!(
                host.socket_message(format!("callback/{body}/onclick/"), Some(client.clone())),
                host.page()
            );
            assert!(called);
            assert!(rendered.unwrap().contains(&format!("click{attempt}")), "the new page shows the change");
            match tokio::time::timeout(UPDATE_INTERVAL * 4, updates.recv()).await {
                Err(_) => {
                    dropped = true;
                    break;
                }
                Ok(update) => update.unwrap().delivered.send(true).unwrap(),
            }
        }
        assert!(dropped, "a page render must clear the pending change flags as remi does");
    }

    #[tokio::test]
    async fn first_socket_message_registers_once_before_callback_output() {
        let (called, received) = mpsc::channel();
        let host =
            UiHost::new(move || FrameApp { gui: crate::remi::Gui::new(Arc::new(crate::remi::Untranslated)), called });
        let page = host.page().await.unwrap();
        let body =
            page.split("<body").nth(1).unwrap().split("id=\"").nth(1).unwrap().split('"').next().unwrap().to_owned();
        let (_, mut updates, _session, client) = host.connected().await.unwrap();
        assert!(host.socket_message("connected".into(), Some(client.clone())).await);
        assert!(host.socket_message("connected".into(), Some(client.clone())).await);
        let pending =
            tokio::spawn(async move { host.socket_message(format!("callback/{body}/onclick/"), Some(client)).await });
        let mut javascript_count = 0;
        loop {
            tokio::select! {
                Some(outbound) = updates.recv() => {
                    javascript_count += usize::from(outbound.text.starts_with('2'));
                    outbound.delivered.send(true).unwrap();
                },
                _ = async { while received.try_recv().is_err() { tokio::task::yield_now().await; } } => {
                    break;
                },
            }
        }
        assert!(pending.await.unwrap());
        assert_eq!(javascript_count, 1, "repeated messages must not duplicate the client");
    }

    struct InitialSendStream {
        input: std::io::Cursor<Vec<u8>>,
        output: Arc<Mutex<Vec<u8>>>,
        first_error: bool,
    }

    impl AsyncRead for InitialSendStream {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buffer: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            let position = self.input.position() as usize;
            let count = buffer.remaining().min(self.input.get_ref().len() - position);
            buffer.put_slice(&self.input.get_ref()[position..position + count]);
            self.input.set_position((position + count) as u64);
            std::task::Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for InitialSendStream {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            bytes: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            if bytes == b"\x81\x070FIRST!" {
                return if self.first_error {
                    std::task::Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()))
                } else {
                    std::task::Poll::Pending
                };
            }
            lock(&self.output).extend_from_slice(bytes);
            std::task::Poll::Ready(Ok(bytes.len()))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn native_socket_initial_timeout_continues_but_send_error_stops_before_registration() {
        for first_error in [false, true] {
            let (called, received) = mpsc::channel();
            let host = UiHost::new(move || FrameApp {
                gui: crate::remi::Gui::new(Arc::new(crate::remi::Untranslated)),
                called,
            });
            let page = host.page().await.unwrap();
            let body = page.split("<body").nth(1).unwrap().split("id=\"").nth(1).unwrap().split('"').next().unwrap();
            let payload = format!("callback/{body}/onclick/");
            let mut input = vec![0x81, payload.len() as u8];
            input.extend_from_slice(payload.as_bytes());
            let output = Arc::new(Mutex::new(Vec::new()));
            let stream = InitialSendStream { input: std::io::Cursor::new(input), output: output.clone(), first_error };
            let (_, updates, session, client) = host.connected().await.unwrap();
            tokio::time::timeout(
                Duration::from_secs(5),
                session.run(serve_socket_session(
                    host,
                    stream,
                    "0FIRST!".into(),
                    updates,
                    client,
                    Duration::from_millis(1),
                )),
            )
            .await
            .unwrap();
            if first_error {
                assert!(output.lock().unwrap().is_empty());
                assert!(received.try_recv().is_err());
            } else {
                assert!(output.lock().unwrap().starts_with(b"\x81\x013"), "ACK must precede callback output");
                assert!(received.try_recv().is_ok(), "timeout must not suppress the first callback");
            }
        }
    }

    #[test]
    fn a_stalled_update_lock_leaves_application_state_available() {
        let (called, received) = mpsc::channel();
        let shared =
            Arc::new(Mutex::new(FrameApp { gui: crate::remi::Gui::new(Arc::new(crate::remi::Untranslated)), called }));
        let work = shared.clone();
        let (sender, requests) = mpsc::channel();
        let worker = std::thread::spawn(move || run_shared(work, requests));
        let (reply, page) = oneshot::channel();
        sender.send(ToUi::Page(reply)).unwrap();
        let page = page.blocking_recv().unwrap();
        let body = page.split("<body").nth(1).unwrap().split("id=\"").nth(1).unwrap().split('"').next().unwrap();
        let (complete, pending) = oneshot::channel();
        sender.send(ToUi::Message { text: format!("callback/{body}/onclick/0|x=1"), client: None, complete }).unwrap();
        let (reply, mut blocked) = oneshot::channel();
        sender.send(ToUi::RequestReady(reply)).unwrap();
        std::thread::sleep(Duration::from_millis(30));
        let state_available = shared.try_lock().is_ok();
        assert!(matches!(blocked.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
        assert!(received.try_recv().is_err());
        drop(sender);
        worker.join().unwrap();
        assert!(pending.blocking_recv().is_err());
        assert!(state_available, "the remi RLock must not retain the Rust state mutex");
    }

    #[tokio::test]
    async fn http_upgrade_delivers_native_frames_through_the_production_socket_loop() {
        for api_first in [false, true] {
            let (called, received) = mpsc::channel();
            let host = UiHost::new(move || FrameApp {
                gui: crate::remi::Gui::new(Arc::new(crate::remi::Untranslated)),
                called,
            });
            let page = host.page().await.unwrap();
            let body = page.split("<body").nth(1).unwrap().split("id=\"").nth(1).unwrap().split('"').next().unwrap();
            let app = router::<()>(host, |_, _| None).route("/api/fixture", axum::routing::get(|| async { "api" }));
            let (server, mut browser) = tokio::io::duplex(65536);
            let connection = tokio::spawn(async move {
                http::serve_connection(server, app, std::future::pending()).await.unwrap();
            });
            if api_first {
                browser.write_all(b"GET /api/fixture HTTP/1.1\r\nHost: localhost\r\n\r\n").await.unwrap();
                let mut headers = Vec::new();
                while !headers.ends_with(b"\r\n\r\n") {
                    headers.push(browser.read_u8().await.unwrap());
                }
                assert!(headers.starts_with(b"HTTP/1.1 200 OK\r\n"));
                let mut body = [0; 3];
                browser.read_exact(&mut body).await.unwrap();
                assert_eq!(&body, b"api");
            }
            browser.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nCookie: remi_session=0\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n").await.unwrap();
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                headers.push(browser.read_u8().await.unwrap());
            }
            let headers = String::from_utf8(headers).unwrap();
            assert!(headers.starts_with("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: "));
            let headers = headers.to_ascii_lowercase();
            assert!(headers.starts_with("http/1.1 101 switching protocols\r\n"));
            assert!(headers.contains("sec-websocket-accept: s3pplmbitxaq9kygzzhzrbk+xoo="));
            let callback = client::to_websocket(&format!("callback/{body}/onclick/3|x=é"));
            assert!(callback.len() < 126);
            // The reader accepts binary and unmasked messages too.
            let mut packet = vec![0x82, callback.len() as u8];
            packet.extend_from_slice(callback.as_bytes());
            browser.write_all(&packet).await.unwrap();
            let mut ack = false;
            loop {
                assert_eq!(browser.read_u8().await.unwrap(), 0x81);
                let short = browser.read_u8().await.unwrap();
                let length = match short {
                    126 => browser.read_u16().await.unwrap() as usize,
                    127 => browser.read_u64().await.unwrap() as usize,
                    length => length as usize,
                };
                let mut payload = vec![0; length];
                browser.read_exact(&mut payload).await.unwrap();
                if payload == b"3" {
                    ack = true;
                }
                if payload.starts_with(b"2") {
                    assert!(ack, "ACK must precede JavaScript in the callback");
                    assert_eq!(length, 3, "header counts Unicode characters");
                    let mut remaining = [0; 4];
                    browser.read_exact(&mut remaining).await.unwrap();
                    payload.extend_from_slice(&remaining);
                    assert_eq!(payload, "2é🚗".as_bytes());
                    break;
                }
            }
            let parameters = received.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(parameters["x"], "é");
            drop(browser);
            connection.await.unwrap();
        }
    }

    #[tokio::test]
    async fn malformed_packets_and_missing_keywords_do_not_partially_resize_the_app() {
        let backend = crate::app::tests::FakeBackend::new();
        let source = backend.clone();
        let host = UiHost::new(move || crate::app::MyApp::new(source));
        let page = host.page().await.unwrap();
        let body = page.split("<body").nth(1).unwrap().split("id=\"").nth(1).unwrap().split('"').next().unwrap();
        assert!(host.message(format!("callback/{body}/onresize/10|width=1234|10|height=777")).await);
        host.page().await.unwrap();
        assert_eq!(backend.docs.lock().unwrap()["settings"]["app"]["window_width"], json!("1234"));
        for packet in ["10|width=2222||10|height=999", "10|width=4444", "10|width=2222|10|height=999|7|extra=x"] {
            assert!(host.message(format!("callback/{body}/onresize/{packet}")).await);
            host.page().await.unwrap();
            let docs = backend.docs.lock().unwrap();
            assert_eq!(docs["settings"]["app"]["window_width"], json!("1234"), "{packet}");
            assert_eq!(docs["settings"]["app"]["window_height"], json!("777"), "{packet}");
        }
        let before = backend.docs.lock().unwrap()["settings"]["app"]["device_pixel_ratio"].clone();
        assert!(host.message(format!("callback/{body}/onpixelratio/3|x=1")).await);
        host.page().await.unwrap();
        assert_eq!(backend.docs.lock().unwrap()["settings"]["app"]["device_pixel_ratio"], before);
    }

    struct WaitingApp {
        gui: crate::remi::Gui<Self>,
        entered: Option<oneshot::Sender<()>>,
        release: mpsc::Receiver<()>,
    }
    impl HasGui for WaitingApp {
        fn gui(&mut self) -> &mut crate::remi::Gui<Self> {
            &mut self.gui
        }
    }
    impl UiApp for WaitingApp {
        fn main(&mut self, _body: WidgetId) -> WidgetId {
            let label = self.gui.create("p", "Label");
            self.gui.set_text(label, "fixture");
            label
        }
        fn title(&self) -> String {
            "fixture".into()
        }
        fn head(&self) -> String {
            String::new()
        }
        fn poll(&mut self, _now: Instant) {}
        fn callback(&mut self, _id: WidgetId, _event: &str, _parameters: &Parameters) -> Result<(), String> {
            self.gui.execute_javascript("window.openrigcue_fixture = true;");
            let _ = self.entered.take().unwrap().send(());
            self.release.recv().unwrap();
            Ok(())
        }
    }

    #[tokio::test]
    async fn socket_completion_waits_for_the_callback_to_return() {
        let (entered, started) = oneshot::channel();
        let (release, wait) = mpsc::channel();
        let host = UiHost::new(move || WaitingApp {
            gui: crate::remi::Gui::new(Arc::new(crate::remi::Untranslated)),
            entered: Some(entered),
            release: wait,
        });
        let page = host.page().await.unwrap();
        let body =
            page.split("<body").nth(1).unwrap().split("id=\"").nth(1).unwrap().split('"').next().unwrap().to_owned();
        let pending = tokio::spawn(async move { host.message(format!("callback/{body}/onclick/")).await });
        tokio::time::timeout(Duration::from_secs(5), started).await.unwrap().unwrap();
        assert!(!pending.is_finished(), "queueing is not callback completion");
        release.send(()).unwrap();
        assert!(pending.await.unwrap());
    }

    #[tokio::test]
    async fn callback_javascript_is_delivered_before_the_callback_proceeds() {
        let (entered, mut started) = oneshot::channel();
        let (release, wait) = mpsc::channel();
        let host = UiHost::new(move || WaitingApp {
            gui: crate::remi::Gui::new(Arc::new(crate::remi::Untranslated)),
            entered: Some(entered),
            release: wait,
        });
        let page = host.page().await.unwrap();
        let body =
            page.split("<body").nth(1).unwrap().split("id=\"").nth(1).unwrap().split('"').next().unwrap().to_owned();
        let (_, mut updates, _session, client) = host.connected().await.unwrap();
        let pending =
            tokio::spawn(async move { host.socket_message(format!("callback/{body}/onclick/"), Some(client)).await });
        loop {
            let outbound = tokio::time::timeout(Duration::from_secs(5), updates.recv()).await.unwrap().unwrap();
            let javascript = outbound.text.starts_with('2');
            if javascript {
                assert_eq!(outbound.text, "2window.openrigcue_fixture = true;");
                assert!(
                    matches!(started.try_recv(), Err(oneshot::error::TryRecvError::Empty)),
                    "execute_javascript must still wait for the send"
                );
            }
            outbound.delivered.send(true).unwrap();
            if javascript {
                break;
            }
        }
        started.await.unwrap();
        assert!(!pending.is_finished());
        release.send(()).unwrap();
        assert!(pending.await.unwrap());
    }

    #[tokio::test]
    async fn failed_spontaneous_send_removes_the_client_and_releases_the_callback() {
        let (entered, started) = oneshot::channel();
        let (release, wait) = mpsc::channel();
        let host = UiHost::new(move || WaitingApp {
            gui: crate::remi::Gui::new(Arc::new(crate::remi::Untranslated)),
            entered: Some(entered),
            release: wait,
        });
        let page = host.page().await.unwrap();
        let body =
            page.split("<body").nth(1).unwrap().split("id=\"").nth(1).unwrap().split('"').next().unwrap().to_owned();
        let (_, mut updates, _session, client) = host.connected().await.unwrap();
        let pending =
            tokio::spawn(async move { host.socket_message(format!("callback/{body}/onclick/"), Some(client)).await });
        loop {
            let outbound = tokio::time::timeout(Duration::from_secs(5), updates.recv()).await.unwrap().unwrap();
            if outbound.text.starts_with('2') {
                drop(outbound);
                // A failed real socket writer unwinds its SocketSession before
                // the sending callback's own close of the socket can finish.
                drop(_session);
                break;
            }
            outbound.delivered.send(true).unwrap();
        }
        tokio::time::timeout(Duration::from_secs(5), started).await.unwrap().unwrap();
        assert!(updates.recv().await.is_none(), "failed client must be removed from broadcasts");
        release.send(()).unwrap();
        assert!(pending.await.unwrap());
    }
}
