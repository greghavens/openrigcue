//! State shared by the service loops and the API.

use openrigcue_cueing::HostOs;
use openrigcue_model::Telemetry;
use serde::Serialize;
use tokio::sync::broadcast;

use crate::profile::ActiveProfile;
use crate::runtime::{Controls, SessionView, Shared};
use crate::store::ConfigStore;

/// The platform rule set the service runs under where behaviour depends on the platform: always the
/// Windows rules (every motion driver, refresh rates uncapped, the IMU scanned, the
/// shift beep played).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OriginalPlatform {
    Windows,
}

impl OriginalPlatform {
    /// The platform the configuration's first run and start checks follow.
    pub fn config_platform(self) -> openrigcue_config::import::migrate::Platform {
        openrigcue_config::import::migrate::Platform::Windows
    }

    /// For the cueing's rate caps.
    pub fn host_os(self) -> HostOs {
        HostOs::Windows
    }

    /// Whether the Windows rules apply.
    pub fn is_windows(self) -> bool {
        true
    }
}

/// A change the UI should hear about without polling.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServiceEvent {
    /// The running game changed.
    GameRunning { game: String },
    /// The active tune was reloaded for a game/vehicle.
    ProfileLoaded { game: String, vehicle: Option<String>, vehicle_level: bool },
    /// The notification list changed.
    Notifications,
    /// The game closed: the telemetry view blanks its min/max cells once.
    MinMaxReset,
    /// The in-memory configuration changed.
    Settings,
    /// The service changed the active tune in place (a hotkey's slider step or wind speed step,
    /// which also moves the visible slider); the UI re-reads the profile without a reload.
    TuneChanged,
}

/// The edited copy of the active speaker layout.
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutEdit {
    /// `SHAKEBOX.profile` the copy was loaded from.
    pub name: String,
    pub layout: openrigcue_config::tunes::SpeakerLayout,
}

/// The raw speaker layout dictionary as last loaded. Selecting a
/// different name alone does not replace this snapshot; a failed read keeps it.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedLayout {
    pub name: String,
    pub value: openrigcue_model::native_value::LegacyValue,
}

/// Everything the loops and the API share.
#[derive(Clone)]
pub struct ServiceState {
    pub config: Shared<ConfigStore>,
    pub controls: Shared<Controls>,
    pub session: Shared<SessionView>,
    /// The telemetry as of the last dispatcher tick.
    pub telemetry: Shared<Telemetry>,
    /// Presence of the telemetry dictionary's `spotter_right` key. False
    /// during startup, before a channel load, idle reset or test pattern fills it.
    pub telemetry_ready: Shared<bool>,
    /// The active game configuration and where it came from.
    pub profile: Shared<ActiveProfile>,
    /// The request to reload the profile, assigned without the UI main-loop lock.
    force_reload_profile: Shared<bool>,
    pub motion: Shared<crate::motion::MotionView>,
    pub notifications: Shared<crate::notifications::Notifications>,
    /// Marker for an unsaved matrix edit. The authoritative raw
    /// layout is shaker_layout, including edits and failed reloads.
    pub layout_edit: Shared<Option<LayoutEdit>>,
    pub shaker_layout: Shared<LoadedLayout>,
    pub platform: OriginalPlatform,
    /// The `--profile-path` argument.
    pub profile_path: Option<std::path::PathBuf>,
    pub events: broadcast::Sender<ServiceEvent>,
}

impl ServiceState {
    pub fn new(config: ConfigStore, platform: OriginalPlatform) -> ServiceState {
        let profile = ActiveProfile::standby(&config);
        let name = config.settings().shakers.speaker_layout.clone();
        let value = config
            .config()
            .metadata_table("shakers")
            .get(&name)
            .cloned()
            .unwrap_or(openrigcue_model::native_value::LegacyValue::None);
        let shaker_layout = Shared::new(LoadedLayout { name, value });
        ServiceState {
            config: Shared::new(config),
            controls: Shared::default(),
            session: Shared::default(),
            telemetry: Shared::new(Telemetry::unfilled()),
            telemetry_ready: Shared::new(false),
            profile: Shared::new(profile),
            force_reload_profile: Shared::new(false),
            motion: Shared::default(),
            notifications: Shared::default(),
            layout_edit: Shared::default(),
            shaker_layout,
            platform,
            profile_path: None,
            events: broadcast::channel(64).0,
        }
    }

    pub fn request_profile_reload(&self) {
        *self.force_reload_profile.write() = true;
    }

    /// The flag is checked, then cleared before loading. These are separate
    /// operations rather than one step under a loop lock.
    pub(crate) fn consume_profile_reload_request(&self) -> bool {
        let requested = *self.force_reload_profile.read();
        if requested {
            *self.force_reload_profile.write() = false;
        }
        requested
    }

    /// Tell subscribed UIs; nobody listening is fine.
    pub fn announce(&self, event: ServiceEvent) {
        let _ = self.events.send(event);
    }
}
