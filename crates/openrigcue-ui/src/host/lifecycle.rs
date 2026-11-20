use std::sync::{Arc, Condvar, Mutex};

use tokio::sync::watch;

use super::{lock, Clients, UiApp};
use crate::remi::parameters::Parameters;

/// The HTTP accept loop's stop request and acknowledgement. Acknowledgement is
/// separate from draining requests: a request may itself close the application.
#[derive(Clone)]
pub struct ServerStop(Arc<StopState>);

struct StopState {
    requested: watch::Sender<bool>,
    stopped: Mutex<bool>,
    changed: Condvar,
}

impl Default for ServerStop {
    fn default() -> Self {
        Self::new()
    }
}

impl ServerStop {
    pub fn new() -> Self {
        let (requested, _) = watch::channel(false);
        Self(Arc::new(StopState { requested, stopped: Mutex::new(false), changed: Condvar::new() }))
    }

    pub(super) fn detached() -> Self {
        let control = Self::new();
        control.accept_stopped();
        control
    }

    /// Server shutdown waits for the accept loop, not its handlers.
    /// Call this on the UI or launcher thread, outside the async HTTP workers.
    pub fn stop(&self) {
        self.0.requested.send_replace(true);
        let mut stopped = lock(&self.0.stopped);
        while !*stopped {
            stopped = self.0.changed.wait(stopped).unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    pub(super) async fn requested(&self) {
        let mut requested = self.0.requested.subscribe();
        while !*requested.borrow_and_update() {
            if requested.changed().await.is_err() {
                return;
            }
        }
    }

    pub(super) fn accept_stopped(&self) {
        *lock(&self.0.stopped) = true;
        self.0.changed.notify_all();
    }
}

/// A socket's independent cancellation lane. It remains readable while the
/// socket is waiting for its own UI callback to finish.
#[derive(Clone)]
pub(super) struct SocketStop(Arc<StopState>);

pub(super) struct SocketSession {
    stop: SocketStop,
    requested: watch::Receiver<bool>,
}

impl SocketStop {
    pub(super) fn new() -> (Self, SocketSession) {
        let control = ServerStop::new();
        let session = SocketSession { requested: control.0.requested.subscribe(), stop: Self(control.0.clone()) };
        (Self(control.0), session)
    }

    /// False means the ordinary reader already ended; it is no longer a member
    /// of the application's websocket set.
    pub(super) fn close(&self) -> bool {
        let mut closed = lock(&self.0.stopped);
        if *closed {
            return false;
        }
        self.0.requested.send_replace(true);
        while !*closed {
            closed = self.0.changed.wait(closed).unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        true
    }
}

impl SocketSession {
    async fn close_requested(&mut self) {
        while !*self.requested.borrow_and_update() {
            if self.requested.changed().await.is_err() {
                return;
            }
        }
    }

    pub(super) async fn run(mut self, work: impl std::future::Future<Output = ()>) {
        tokio::select! {
            () = self.close_requested() => {},
            () = work => {},
        }
        // select drops the socket work (and its stream) before self is dropped
        // and acknowledges the close to the synchronous UI callback.
    }
}

impl Drop for SocketSession {
    fn drop(&mut self) {
        *lock(&self.stop.0.stopped) = true;
        self.stop.0.changed.notify_all();
    }
}

pub(super) struct Lifecycle {
    server: ServerStop,
    pub(super) updates_stopped: bool,
}

impl Lifecycle {
    pub(super) fn new(server: ServerStop) -> Self {
        Self { server, updates_stopped: false }
    }

    pub(super) fn invoke<A: UiApp>(
        &mut self,
        app: &mut A,
        clients: &Clients,
        method: &str,
        parameters: &Parameters,
    ) -> Option<Result<(), String>> {
        let class = match method {
            "close" => "App",
            "on_close" => app.close_class_name(),
            _ => return None,
        };
        if let Some(key) = parameters.keys().next() {
            return Some(Err(if key == "self" {
                format!("TypeError: {class}.{method}() got multiple values for argument 'self'")
            } else {
                crate::remi::classes::unexpected_keyword(&format!("{class}.{method}"), &[], key)
            }));
        }
        if method == "close" {
            tracing::debug!(target: "remi.server", "shutting down...");
            self.server.stop();
        }
        Some(self.on_close(app, clients))
    }

    pub(super) fn callback<A: UiApp>(
        &mut self,
        app: &mut A,
        clients: &Clients,
        body: crate::remi::WidgetId,
        id: crate::remi::WidgetId,
        method: &str,
        parameters: &Parameters,
    ) -> Option<Result<(), String>> {
        (id == body).then(|| self.invoke(app, clients, method, parameters)).flatten()
    }

    pub(super) fn attribute<A: UiApp>(
        &mut self,
        app: &mut A,
        clients: &Clients,
        body: crate::remi::WidgetId,
        widget: &str,
        method: &str,
        parameters: &[(String, String)],
    ) -> Option<Result<(), String>> {
        (app.gui().identifier(body) == widget)
            .then(|| self.invoke(app, clients, method, &parameters.iter().cloned().collect()))
            .flatten()
    }

    pub(super) fn on_close<A: UiApp>(&mut self, app: &mut A, clients: &Clients) -> Result<(), String> {
        app.on_close()?;
        // on_close stops only the idle loop. MyApp's own main-loop thread and
        // the shared application remain available.
        self.updates_stopped = true;
        let sockets: Vec<_> = lock(clients).iter().map(|client| client.close.clone()).collect();
        for socket in sockets {
            if socket.close() {
                // A websocket close terminates the server by default.
                self.server.stop();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
