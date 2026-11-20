//! The deferred start, which mounts the interface in the page's root, and the profile-edit and notification
//! helpers the interface's handlers share.

use serde_json::Value;

use super::MyApp;
use crate::backend::BackendExt;

/// The deferred start: the interface is built in the page's root.
pub fn deferred_main(app: &mut MyApp) {
    let root = app.main_container.expect("main built");
    crate::ui::mount(app, root);
}

impl MyApp {
    /// Ends the editing of a profile: a library profile still waiting for its game or vehicle is dropped, the
    /// service clears its edit flag (and the remembered location with it), and the status is read again.
    pub fn clear_profile_edit_context(&mut self) -> Result<(), String> {
        crate::ui::screens::library::cancel(self);
        if self.backend.clear_profile_edit().map_err(|error| error.detail)? {
            self.ml.previous_location = None;
        }
        if let Ok(status) = self.backend.get("status") {
            self.docs.controls = status.get("controls").cloned().unwrap_or(Value::Null);
            self.docs.status = status;
        }
        Ok(())
    }

    /// Shows an alert notification with `title` and `message`; its `color` gives its severity, and the
    /// interface's notification rules decide how long it shows.
    pub fn alert_notification(&mut self, title: &str, message: &str, color: &str, _timeout: f64) {
        crate::ui::notices::alert(self, title, message, color);
    }
}
