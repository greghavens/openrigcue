//! Cross-process activation for the Linux desktop window. The second launcher
//! exits after its request; the running instance raises its window or opens it again.

use std::sync::Arc;
const NAME: &str = "org.openrigcue.Application";
const PATH: &str = "/OpenRigCue";
const INTERFACE: &str = "org.openrigcue.Application";

pub struct Instance {
    _connection: zbus::blocking::Connection,
}
struct Application {
    activate: Arc<dyn Fn() + Send + Sync>,
    close_window: Arc<dyn Fn() + Send + Sync>,
    major: String,
}

#[zbus::interface(name = "org.openrigcue.Application")]
impl Application {
    fn activate(&self) {
        (self.activate)();
    }

    /// Version-scoped close request. Only this application's registered window
    /// can receive it.
    fn close_window(&self, major: &str) -> bool {
        if major != self.major {
            return false;
        }
        (self.close_window)();
        true
    }
}

impl Instance {
    /// Register after initial window creation. Requests before there is a window
    /// find no service rather than creating a window in a headless instance.
    pub fn register(
        activate: Arc<dyn Fn() + Send + Sync>,
        close_window: Arc<dyn Fn() + Send + Sync>,
    ) -> zbus::Result<Self> {
        let connection = zbus::blocking::connection::Builder::session()?
            .name(NAME)?
            .serve_at(PATH, Application { activate, close_window, major: env!("CARGO_PKG_VERSION_MAJOR").into() })?
            .build()?;
        Ok(Self { _connection: connection })
    }

    pub fn activate_existing() -> zbus::Result<()> {
        let connection = zbus::blocking::Connection::session()?;
        match zbus::blocking::Proxy::new(&connection, NAME, PATH, INTERFACE)?.call("Activate", &()) {
            Err(zbus::Error::MethodError(name, _, _))
                if matches!(
                    name.as_str(),
                    "org.freedesktop.DBus.Error.ServiceUnknown" | "org.freedesktop.DBus.Error.NameHasNoOwner"
                ) =>
            {
                Ok(())
            }
            result => result,
        }
    }

    /// The caller catches all lookup/close errors before service initialization.
    /// An older endpoint may lack this method.
    pub fn close_existing_major(major: &str) -> zbus::Result<bool> {
        let connection = zbus::blocking::Connection::session()?;
        zbus::blocking::Proxy::new(&connection, NAME, PATH, INTERFACE)?.call("CloseWindow", &(major,))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The activation request of a second launch reaches the running instance's activation callback and
    /// nothing else.
    #[test]
    fn activation_request_runs_the_activation_callback() {
        let activations = Arc::new(AtomicUsize::new(0));
        let captured = activations.clone();
        let app = Application {
            activate: Arc::new(move || {
                captured.fetch_add(1, Ordering::SeqCst);
            }),
            close_window: Arc::new(|| panic!("activation must not close the window")),
            major: "2026".into(),
        };
        app.activate();
        assert_eq!(activations.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn replacement_close_is_scoped_to_the_registered_application_major() {
        let closes = Arc::new(AtomicUsize::new(0));
        let captured = closes.clone();
        let app = Application {
            activate: Arc::new(|| panic!("replacement must not activate")),
            close_window: Arc::new(move || {
                captured.fetch_add(1, Ordering::SeqCst);
            }),
            major: "2026".into(),
        };
        assert!(!app.close_window("2025"));
        assert!(!app.close_window(""));
        assert_eq!(closes.load(Ordering::SeqCst), 0);
        assert!(app.close_window("2026"));
        assert_eq!(closes.load(Ordering::SeqCst), 1);
    }
}
