//! What the interface shows about the running session, read from the service each main-loop pass: the
//! session state, game and vehicle, live telemetry readouts and the rig's devices.

use openrigcue_model::native_value::LegacyValue;
use openrigcue_model::python_compare::native_equal;
use openrigcue_model::python_repr::str_value;
use serde_json::Value;

use crate::app::MyApp;
use crate::backend::{num, text, Backend};

/// The state of the session, as the profile summary shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum State {
    /// No game runs.
    #[default]
    Standby,
    /// A profile is being edited from the profile list.
    Editing,
    /// A game runs but sends no telemetry.
    GameRunning,
    /// Game telemetry arrives.
    Telemetry,
}

impl State {
    /// The catalog text of the state.
    pub fn label(self) -> &'static str {
        match self {
            State::Standby => "Stand By",
            State::Editing => "Editing Profile",
            State::GameRunning => "Game Running",
            State::Telemetry => "Telemetry Data",
        }
    }
}

/// The session summary.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Session {
    pub state: State,
    /// The game's catalogue key (`none` in stand-by).
    pub game_key: String,
    /// The game's name (empty in stand-by).
    pub game: String,
    /// The vehicle the profile is for (empty when none is known).
    pub vehicle: String,
    /// The track or location the game reports.
    pub location: String,
    /// Whether the game is a racing game (or unknown); otherwise a flight or space game.
    pub racing: bool,
}

/// Reads the session summary: the same rules as the profile summary of the service's session.
pub fn read(backend: &dyn Backend) -> Session {
    let game = backend.game_value().unwrap_or(LegacyValue::Str("none".into()));
    if native_equal(&game, &LegacyValue::Str("none".into())) {
        return Session { racing: true, game_key: "none".into(), ..Session::default() };
    }
    let game_key = str_value(&game);
    let name = backend.game_name_value(&game).map(|name| str_value(&name)).unwrap_or_else(|_| game_key.clone());
    let listener = backend.session_value("listener_running").unwrap_or(LegacyValue::Str("none".into()));
    let no_listener = native_equal(&listener, &LegacyValue::Str("none".into()));
    let editing = backend.control_value(&["profile_edit"]).is_ok_and(|value| value.is_truthy());
    let state = match (no_listener, editing) {
        (true, true) => State::Editing,
        (true, false) => State::GameRunning,
        (false, _) => State::Telemetry,
    };
    let previous = || {
        backend
            .previous_vehicle_value()
            .ok()
            .filter(|value| !native_equal(value, &LegacyValue::Int(0)))
            .map(|value| str_value(&value))
            .unwrap_or_default()
    };
    let vehicle = if no_listener {
        previous()
    } else {
        match backend.session_value("vehicle_name") {
            Ok(current) if !native_equal(&current, &LegacyValue::Int(0)) => str_value(&current),
            _ => previous(),
        }
    };
    let location = backend.location_value();
    let location = if native_equal(&location, &LegacyValue::Int(0)) { String::new() } else { str_value(&location) };
    let racing = openrigcue_config::catalog::Catalog::builtin()
        .game(&game_key)
        .map(|entry| entry.kind == openrigcue_config::catalog::GameKind::Racing)
        .unwrap_or(true);
    Session { state, game_key, game: name, vehicle, location, racing }
}

/// Live values of the telemetry, in the units the instruments show.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Readouts {
    /// Whether telemetry arrives.
    pub live: bool,
    pub speed: f64,
    /// `km/h` or `mph`.
    pub metric: bool,
    pub gear: f64,
    pub rpm: f64,
    pub max_rpm: f64,
    pub pitch: f64,
    pub roll: f64,
    pub yaw: f64,
    pub lateral_g: f64,
    pub longitudinal_g: f64,
    pub vertical_g: f64,
    /// The last speeds, oldest first, for the trace.
    pub trace: Vec<f64>,
}

/// How many speeds the trace keeps.
pub const TRACE_LENGTH: usize = 40;

/// The live values of `telemetry` (the service's telemetry snapshot); `previous` keeps the trace going.
pub fn readouts(app: &MyApp, session: &Session, telemetry: Option<&Value>, previous: &Readouts) -> Readouts {
    let metric = text(app.setting(&["app", "speed_unit"])) != "mph";
    let (State::Telemetry, Some(telemetry)) = (session.state, telemetry) else {
        return Readouts { metric, ..Readouts::default() };
    };
    let value = |name: &str| telemetry.get(name).map(num).filter(|v| v.is_finite()).unwrap_or(0.0);
    let kmh = value("speed_kmh");
    let speed = if metric { kmh } else { kmh * 0.621_371 };
    let mut trace = previous.trace.clone();
    trace.push(speed);
    if trace.len() > TRACE_LENGTH {
        trace.remove(0);
    }
    Readouts {
        live: true,
        speed,
        metric,
        gear: value("gear"),
        rpm: value("rpm"),
        max_rpm: value("max_rpm"),
        pitch: value("pitch"),
        roll: value("roll"),
        yaw: value("yaw"),
        lateral_g: value("lateral_acceleration"),
        longitudinal_g: value("longitudinal_acceleration"),
        vertical_g: value("vertical_acceleration"),
        trace,
    }
}

/// The gear as the instruments show it.
pub fn gear_text(gear: f64) -> String {
    match gear as i64 {
        -1 => "R".into(),
        0 => "N".into(),
        g if (1..=10).contains(&g) => g.to_string(),
        _ => "—".into(),
    }
}

/// One device of the rig status.
#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    /// The hardware status key (`Motionbox`, `Shakebox`, …).
    pub key: &'static str,
    /// The catalog text of its name.
    pub name: &'static str,
    pub icon: &'static str,
    /// `green`, `yellow`, `red`, `orange` or `pink` (the hardware status colour), empty before the first pass.
    pub color: String,
    /// The service's explanation of the status.
    pub detail: String,
    /// The run-time switch the device's mode button cycles, and its states.
    pub mode: Option<Mode>,
    /// The current mode index.
    pub mode_index: Option<usize>,
}

/// The two-state device switches.
pub const ON_OFF: &[&str] = &["false", "true"];
/// The three-state output switches.
pub const OFF_AUTO_ON: &[&str] = &["off", "auto", "always_on"];

/// The devices of the rig status, in order: the three main outputs first.
/// A device switch: the run-time switch name and its states.
type Mode = (&'static str, &'static [&'static str]);

const DEVICES: [(&str, &str, &str, Option<Mode>); 12] = [
    ("Motionbox", "Motion platform", "motion", Some(("motion", ON_OFF))),
    ("Shakebox", "Bass shakers", "shake", Some(("shakers", ON_OFF))),
    ("Intellibox", "Wind fans", "wind", None),
    ("WindMode", "Wind L/R", "wind", Some(("wind", OFF_AUTO_ON))),
    ("WindCenterMode", "Wind Center", "wind", Some(("wind_center", OFF_AUTO_ON))),
    ("LedMode", "LED", "led", Some(("led", OFF_AUTO_ON))),
    ("MCMode", "Motion Compensation", "compensation", Some(("compensation", ON_OFF))),
    ("Belt", "Belt", "belt", Some(("belt", ON_OFF))),
    ("SIM3D", "SIM3D", "shake", Some(("sim3d", ON_OFF))),
    ("Hud", "HUD", "hud", Some(("hud", ON_OFF))),
    ("iFlag", "iFlag", "flag", Some(("iflag", ON_OFF))),
    ("HapticR", "HapticR", "shake", Some(("haptic", ON_OFF))),
];

/// The devices with their last known status and mode.
pub fn devices(app: &MyApp) -> Vec<Device> {
    let hapticr = app
        .backend
        .native_setting("APP", "enable_hapticr")
        .ok()
        .and_then(|value| crate::backend::native_bool(&value).ok())
        .unwrap_or(false);
    DEVICES
        .iter()
        .filter(|(key, ..)| *key != "HapticR" || hapticr)
        .map(|(key, name, icon, mode)| {
            let (color, detail) = app.ml.hardware_states.get(*key).cloned().unwrap_or_default();
            let mode_index = mode.and_then(|(control, cycle)| {
                let current = app.backend.control_value(&["switches", control]).ok()?;
                Some(
                    (0..cycle.len())
                        .position(|index| native_equal(&current, &LegacyValue::Int(index as i64)))
                        .unwrap_or(0),
                )
            });
            Device { key, name, icon, color, detail, mode: *mode, mode_index }
        })
        .collect()
}

/// The catalog text of a device mode.
pub fn mode_label(cycle: &[&str], index: usize) -> &'static str {
    match cycle.get(index).copied().unwrap_or("false") {
        "true" => "On",
        "auto" => "Auto",
        "always_on" => "Always On",
        _ => "Off",
    }
}

/// The catalog text of a device status colour.
pub fn status_label(color: &str) -> &'static str {
    match color {
        "green" => "Connected",
        "yellow" => "Needs attention",
        "orange" => "Switched off",
        "pink" => "Updating",
        "" => "Checking",
        _ => "Not connected",
    }
}

/// Steps a device switch to its next state, wrapping after the last: the stored run-time switch.
pub fn cycle_mode(app: &mut MyApp, control: &str, cycle: &[&str]) -> Result<(), String> {
    let current = app.backend.control_value(&["switches", control]).map_err(|error| error.detail)?;
    let index = (0..cycle.len()).position(|index| native_equal(&current, &LegacyValue::Int(index as i64)));
    let next = cycle[index.map_or(0, |i| (i + 1) % cycle.len())];
    let value = match next {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        other => Value::String(other.to_owned()),
    };
    app.set_control_checked(&["switches", control], value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gears_read_as_the_dashboard_shows_them() {
        assert_eq!(gear_text(-1.0), "R");
        assert_eq!(gear_text(0.0), "N");
        assert_eq!(gear_text(4.0), "4");
        assert_eq!(gear_text(11.0), "—");
    }

    #[test]
    fn device_modes_and_statuses_have_catalog_labels() {
        assert_eq!(mode_label(ON_OFF, 1), "On");
        assert_eq!(mode_label(OFF_AUTO_ON, 2), "Always On");
        assert_eq!(status_label("green"), "Connected");
        assert_eq!(status_label("red"), "Not connected");
    }
}
