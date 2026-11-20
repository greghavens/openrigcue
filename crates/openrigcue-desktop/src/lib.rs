//! Desktop services for the Linux launcher: window, tray, autostart and single-instance activation.
pub mod autostart;
mod gtk;
pub mod instance;
mod policy;
mod tray;
pub use gtk::Desktop;
pub use policy::{native_window_plan, window_plan, CloseAction, Monitor, NativeWindowPlan, WindowError, WindowPlan};
pub use tray::{Tray, TrayAction};

use openrigcue_model::native_value::Dict;
use std::sync::{Arc, Mutex};

/// Read at the callback, rather than freezing APP at window creation.
pub type ConfigReader = Arc<dyn Fn() -> Dict + Send + Sync>;

/// Last window geometry, read by the UI's Save callback. Wayland cannot supply a
/// global position, so it leaves position absent instead of overwriting X11 data.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Geometry {
    pub position: Option<(i32, i32)>,
    pub monitors: Vec<Option<Monitor>>,
}
impl Geometry {
    pub fn assignments(&self) -> Dict {
        use openrigcue_model::native_value::LegacyValue;
        let mut raw = Dict::new();
        raw.insert("APP", LegacyValue::Dict(Dict::new()));
        self.capture_into(&mut raw);
        raw
    }
    /// Two independent steps of Save: position first, then replacement of
    /// MONITORS (including removal of disconnected screens).
    pub fn capture_into(&self, config: &mut Dict) {
        use openrigcue_model::native_value::LegacyValue;
        if let Some((x, y)) = self.position {
            if let Some(app) = config.get_mut("APP") {
                if app.assign_item("window_x_pos", LegacyValue::Int(i64::from(x))).is_ok() {
                    let _ = app.assign_item("window_y_pos", LegacyValue::Int(i64::from(y)));
                }
            }
        }
        // Save assigns integers; window planning independently constructs
        // strings. They become strings on the next settings file reload.
        let monitors = self
            .monitors
            .iter()
            .flatten()
            .enumerate()
            .map(|(index, monitor)| {
                let sum =
                    i64::from(monitor.x) + i64::from(monitor.y) + i64::from(monitor.width) + i64::from(monitor.height);
                (index.to_string().into(), LegacyValue::Int(sum))
            })
            .collect();
        config.insert("MONITORS", LegacyValue::Dict(monitors));
    }
}
pub type SharedGeometry = Arc<Mutex<Option<Geometry>>>;

#[derive(Debug, thiserror::Error)]
pub enum DesktopError {
    #[error("desktop library: {0}")]
    Library(String),
    #[error("GTK could not connect to a display")]
    Display,
    #[error("desktop must be used on its initial thread")]
    Thread,
    #[error("{0}")]
    Window(#[from] WindowError),
    #[error("{0}")]
    Operation(String),
}
