//! The openrigcue UI: a server-side UI shown in the browser.
//!
//! The interface ([`ui`]), its handlers and the 0.5 s main loop run in the program ([`app::MyApp`] on the UI
//! thread of [`host`]); the browser runs one client script only, which shows the HTML the program sends and
//! forwards the user's actions. [`spec`] holds the page descriptions the interface and the service's profile
//! loading use.

pub mod app;
pub mod assets;
pub mod backend;
pub mod catalog;
pub mod host;
pub mod net;
pub mod qr;
pub mod remi;
pub mod slider;
pub mod spec;
pub mod ui;

use axum::Router;
use openrigcue_config::catalog::registry::Registry;
use serde::{Deserialize, Serialize};

/// Everything the browser builds the pages from.
#[derive(Debug, Clone, Serialize)]
pub struct UiDocument {
    pub version: &'static str,
    pub pages: Vec<spec::Page>,
    pub registry: RegistryTables,
    pub models: &'static [catalog::ModelFlags],
    pub manufacturers: Vec<catalog::ManufacturerRow>,
    /// Built-in speaker layout names and the speakers each uses.
    pub builtin_layouts: Vec<String>,
    pub speaker_use: std::collections::BTreeMap<String, std::collections::BTreeMap<&'static str, bool>>,
    /// The Routines page's routines: name and description of each.
    pub routines: &'static [(&'static str, &'static str)],
}

/// The effect tables the page logic iterates.
#[derive(Debug, Clone, Serialize)]
pub struct RegistryTables {
    pub shaker_effects: Vec<IdLabel>,
    pub motion_effects: Vec<MotionEffect>,
    pub rumble_effects: Vec<IdLabel>,
    pub haptic_effects: Vec<IdLabel>,
    pub flags: Vec<FlagRow>,
    /// The effects: the per-speaker levels of a speaker layout (custom matrix dialog).
    pub channel_effects: Vec<ChannelEffect>,
    /// The parent effects: the matrix dialog's groups, in order.
    pub channel_parents: Vec<&'static str>,
}

/// The parent effects (effect groups).
pub const CHANNEL_PARENTS: [&str; 9] = [
    "",
    "Suspension",
    "Long G Acceleration",
    "Long G Deceleration",
    "Lateral G",
    "Traction Loss Rear",
    "Wheel Slip / Lock",
    "Landing Gear",
    "Touchdown",
];

#[derive(Debug, Clone, Serialize)]
pub struct ChannelEffect {
    pub id: String,
    pub name: String,
    pub parent: String,
    pub visible: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct IdLabel {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MotionEffect {
    pub id: String,
    pub label: String,
    pub intellitune: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct FlagRow {
    pub name: String,
    pub group: String,
}

/// What the page descriptions depend on: values read once, when the pages are built.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct BuildInputs {
    /// The `smoothing_slider_step` setting of `CUSTOM_FEATURES`, as an integer.
    #[serde(default = "one")]
    pub smoothing_step: f64,
    /// Whether the `MOTIONBOX` model is H6 or P6 (Motion Compensation factor rows).
    #[serde(default)]
    pub six_dof: bool,
}

fn one() -> f64 {
    1.0
}

/// Describes the page tree the deferred start builds (Tuning, Games,
/// Setup): every page description plus the tables their logic reads.
pub fn ui_document(inputs: BuildInputs) -> UiDocument {
    let registry = Registry::builtin();
    let pages = vec![
        spec::wind_page(),
        spec::shake_page(registry),
        spec::motion_page(registry, inputs.smoothing_step.trunc()),
        spec::belt_page(),
        spec::haptic_page(registry),
        spec::sim3d_page(registry),
        spec::iflag_page(registry),
        spec::led_page(),
        spec::routines_page(),
        spec::telemetry_page(),
        spec::hardware_page(),
        spec::joystick_page(),
        spec::compensation_page(inputs.six_dof),
        spec::app_page(),
        spec::debug_page(),
    ];
    let id_label = |id: &str, label: &str| IdLabel { id: id.to_owned(), label: label.to_owned() };
    UiDocument {
        version: env!("CARGO_PKG_VERSION"),
        pages,
        registry: RegistryTables {
            shaker_effects: registry.shaker_effects.iter().map(|e| id_label(&e.id, &e.label)).collect(),
            motion_effects: registry
                .motion_effects
                .iter()
                .map(|e| MotionEffect { id: e.id.clone(), label: e.label.clone(), intellitune: e.intellitune })
                .collect(),
            rumble_effects: registry.rumble_effects.iter().map(|e| id_label(&e.id, &e.label)).collect(),
            haptic_effects: registry.haptic_effects.iter().map(|e| id_label(&e.id, &e.label)).collect(),
            flags: registry.flags.iter().map(|f| FlagRow { name: f.name.clone(), group: f.group.clone() }).collect(),
            channel_effects: registry
                .shaker_channel_effects
                .iter()
                .map(|e| ChannelEffect {
                    id: e.id.clone(),
                    name: e.name.clone(),
                    parent: e.parent.clone(),
                    visible: e.visible,
                })
                .collect(),
            channel_parents: CHANNEL_PARENTS.to_vec(),
        },
        models: catalog::models(),
        manufacturers: catalog::manufacturers(),
        builtin_layouts: registry.builtin_shaker_layouts.clone(),
        speaker_use: catalog::builtin_speaker_use(),
        routines: &spec::ROUTINES,
    }
}

/// The UI routes: the page and websocket at `/` (the application of `host`), the `res` static files
/// and the widget attribute calls.
pub fn router<S: Clone + Send + Sync + 'static>(host: host::UiHost) -> Router<S> {
    host::router(host, static_file)
}

fn static_file(key: &str, path: &str) -> Option<axum::response::Response> {
    if key == "res" {
        Some(assets::serve(path))
    } else {
        None
    }
}

#[cfg(test)]
mod tests;
