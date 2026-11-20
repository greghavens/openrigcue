//! The telemetry main loop: listener selection, per-tick channel derivations and session bookkeeping.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use openrigcue_model::native_value::LegacyValue;
use openrigcue_model::Telemetry;

use crate::NativeRead;

use super::attitude::AttitudeSource;
use super::channels::CHANNEL_SPECS;
use super::dams::{self, DamsSettings, SlipInputs};
use super::export::{export_rows, ExportRow, SpeedUnit};
use super::history::TelemetryHistory;
use super::intellitune::{IntellituneHandle, IntellituneReference, IntellituneSamples};
use super::listener::{Listener, Rate};
use super::max_speed::MaxSpeedTracker;
use super::min_max::{MinMaxHandle, MinMaxTable};
use super::pacing::{native_tick_period, tick_period, RefreshRates};
use super::pyfloat::{round_to, truncate};
use super::steering::{native_steering_range, wind_steering_angle};
use super::test_pattern::{reset_channels, TestPattern};

/// Listener tried when no catalog game is found, then the joystick listener.
const API_LISTENER: &str = "api";
const JOYSTICK_LISTENER: &str = "joy";
/// Samples kept for the telemetry chart: one more is appended after the oldest is dropped beyond this length.
const HISTORY_LIMIT: usize = 500;
/// Reverse gear and its harmonized value.
const REVERSE_GEAR: f64 = -1.0;
const HARMONIZED_REVERSE: f64 = 12.0;

/// The two speed reads convert to an integer, including from decimal text and bytes. A typed
/// answer takes precedence over its numeric projection, even when conversion fails.
fn listener_value(listener: &dyn Listener, name: &str) -> Option<LegacyValue> {
    match listener.read_native(name) {
        NativeRead::Value(value) => Some(value),
        NativeRead::Raised => None,
        NativeRead::Unsupported => listener.channel(name).map(LegacyValue::Float),
    }
}

fn listener_rate(listener: &dyn Listener, name: &str, rate: Rate) -> Option<LegacyValue> {
    match listener.read_native(name) {
        NativeRead::Value(value) => Some(value),
        NativeRead::Raised => None,
        NativeRead::Unsupported => listener.rate(rate).map(LegacyValue::Float),
    }
}

fn listener_speed_int(listener: &dyn Listener) -> Option<LegacyValue> {
    listener_value(listener, "speed").and_then(native_integer_value)
}

fn listener_identity(listener: &dyn Listener, name: &str) -> Option<LegacyValue> {
    match listener.read_native(name) {
        NativeRead::Value(value) => Some(value),
        NativeRead::Raised => None,
        NativeRead::Unsupported => {
            if name == "track_loc" { listener.location() } else { listener.vehicle() }.map(LegacyValue::Str)
        }
    }
}

fn identity_text(value: &LegacyValue) -> Option<String> {
    if openrigcue_model::python_compare::native_equal(value, &LegacyValue::Int(0)) {
        None
    } else {
        Some(openrigcue_model::python_repr::str_value(value))
    }
}

fn native_integer_value(value: LegacyValue) -> Option<LegacyValue> {
    match value {
        LegacyValue::Int(value) => Some(LegacyValue::Int(value)),
        LegacyValue::Bool(value) => Some(LegacyValue::Int(i64::from(value))),
        LegacyValue::Float(value) => truncate(value).map(integer_view),
        LegacyValue::Str(value) => integer_text_value(&value),
        LegacyValue::Bytes(value) if value.is_ascii() => integer_text_value(std::str::from_utf8(&value).ok()?),
        _ => None,
    }
}

fn integer_text_value(value: &str) -> Option<LegacyValue> {
    let value = openrigcue_model::python_numeric::python_int_text(value)?;
    if let Ok(value) = value.parse::<i64>() {
        return Some(LegacyValue::Int(value));
    }
    value.parse::<f64>().ok().filter(|value| value.is_finite()).map(integer_view)
}

// The shared value model has bounded integers. Keep wider finite magnitudes in the
// compatibility view rather than making an otherwise valid int conversion fail.
fn integer_view(value: f64) -> LegacyValue {
    if (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&value) {
        LegacyValue::Int(value as i64)
    } else {
        LegacyValue::Float(value)
    }
}

/// A catalog game whose window or process was found by the detector, in catalog order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanHit {
    pub game_id: String,
    pub listener_id: String,
}

/// Catalog facts the dispatcher needs about a game id.
pub trait GameDirectory {
    /// Whether the catalog type of the game is `flying` (unknown games count as racing).
    fn is_flying(&self, game_id: &str) -> bool;
    /// Factory Intellitune reference values of the game, if it has a factory profile.
    fn intellitune_reference(&self, game_id: &str) -> Option<IntellituneReference>;
}

/// `config.ini` values the loop reads every tick.
#[derive(Debug, Clone, PartialEq)]
pub struct DispatchSettings {
    pub dams: DamsSettings,
    pub refresh_rates: RefreshRates,
    /// Canonical SHAKEBOX/INTELLIBOX/MOTIONBOX values; a missing entry is a required lookup failure.
    pub native_refresh_rates: Option<[Option<LegacyValue>; 3]>,
    /// `APP.measurement_unit`.
    pub speed_unit: SpeedUnit,
    /// Canonical required measurement-unit lookup; its error is acted on at export generation.
    pub native_speed_unit: Option<Result<SpeedUnit, String>>,
    /// `APP.additional_telemetry`: every channel is displayed and exported. After startup initialization,
    /// latched on ticks where the active listener changes and never cleared (see [`DispatchEvent::ConfigRefresh`]).
    pub show_all_channels: bool,
}

/// Values of the active game/vehicle profile the loop reads every tick.
#[derive(Debug, Clone, PartialEq)]
pub struct ProfileSettings {
    /// `default_max_speed_kmh`.
    pub default_max_speed_kmh: f64,
    /// `gear_shift_perc`: shift-light threshold in percent of max rpm.
    pub gear_shift_percent: f64,
    /// `motion_effects.pitch_type`.
    pub pitch_source: AttitudeSource,
    /// `motion_effects.roll_type`.
    pub roll_source: AttitudeSource,
    /// Canonical game_config when available. Its required default-speed lookup and
    /// multiplication keep their own failures rather than using a fallback value.
    pub native: Option<LegacyValue>,
}

impl ProfileSettings {
    pub fn pitch_source(&self) -> AttitudeSource {
        self.source_for("pitch_type", self.pitch_source)
    }

    pub fn roll_source(&self) -> AttitudeSource {
        self.source_for("roll_type", self.roll_source)
    }

    fn source_for(&self, key: &str, fallback: AttitudeSource) -> AttitudeSource {
        let Some(native) = &self.native else { return fallback };
        match native
            .as_dict()
            .and_then(|profile| profile.get("motion_effects"))
            .and_then(LegacyValue::as_dict)
            .and_then(|effects| effects.get(key))
        {
            None => AttitudeSource::Angle,
            Some(LegacyValue::Str(value)) if value == "angle" => AttitudeSource::Angle,
            Some(_) => AttitudeSource::Rate,
        }
    }
}

/// Runtime state of other parts of the application the loop reads every tick.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RuntimeInputs {
    /// The cannon-firing canned effect, `None` when not set.
    pub cannon_firing: Option<LegacyValue>,
    /// The missile-firing canned effect, `None` when not set.
    pub missile_firing: Option<LegacyValue>,
    /// Channel recorded for the telemetry chart.
    pub plot_channel: Option<String>,
    /// Whether IntelliTune is capturing telemetry.
    pub intellitune_capture: bool,
}

/// Fallback inputs for one tick; a live bridge can supply current values at their use boundaries.
pub struct TickInput<'a> {
    /// Running catalog games found by the detector, in catalog order (only used while no listener delivers).
    pub hits: &'a [ScanHit],
    pub settings: &'a DispatchSettings,
    pub profile: &'a ProfileSettings,
    pub runtime: &'a RuntimeInputs,
    pub games: &'a dyn GameDirectory,
}

/// Notifications a tick produces for the rest of the application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchEvent {
    /// A game was found running.
    GameRunning(String),
    /// A catalog game's listener delivered for the first time in a scan (recorded in the notifications table).
    TelemetryReceived(String),
    /// The active listener changed (also on the first tick): refresh the runtime configuration parameters, that
    /// is the configured sound card from `SHAKEBOX.soundcard`. No game profile is loaded. The dispatcher itself latches the `APP.additional_telemetry` show-all flag on this tick.
    ConfigRefresh,
    /// The game closed: the telemetry view blanks its min/max cells.
    MinMaxReset,
}

/// Result of one tick.
#[derive(Debug, Clone, PartialEq)]
pub struct TickOutcome {
    pub events: Vec<DispatchEvent>,
    /// Pause before the next tick.
    pub sleep: Duration,
}

/// Shared writes and current inputs at their use boundaries. Callbacks must not reenter the dispatcher.
pub trait DispatchPublication: Send + Sync {
    fn assigned(&self, name: &'static str, telemetry: &Telemetry);
    /// A live catalog scan, advanced only after the preceding listener's check.
    /// None keeps the caller's precomputed compatibility hits.
    fn scan_hits(&self) -> Option<Box<dyn Iterator<Item = Result<ScanHit, String>> + Send>> {
        None
    }
    fn snapshot(&self) -> Option<Telemetry> {
        None
    }
    fn game_running(&self, _game: Option<&str>) {}
    /// Outer None keeps compatibility state; Some(None) is the current stand-by value.
    fn game_snapshot(&self) -> Option<Option<String>> {
        None
    }
    fn listener_running(&self, _listener: Option<&str>) {}
    /// The scan-only SQLite assignment follows the receive step and precedes every
    /// channel getter. None leaves persistence to the compatibility event consumer.
    fn telemetry_received(&self, _listener: &str) -> Option<Result<(), String>> {
        None
    }
    fn vehicle(&self, _value: &LegacyValue, _text: Option<&str>) {}
    fn location(&self, _value: &LegacyValue, _text: Option<&str>) {}
    fn shift_light(&self, _value: bool) {}
    fn export_rows(&self, _rows: &[ExportRow]) {}
    fn reset_min_max(&self) {}
    fn show_all_channels(&self, _value: bool) {}
    fn runtime_inputs(&self) -> Option<RuntimeInputs> {
        None
    }
    fn profile_settings(&self) -> Option<ProfileSettings> {
        None
    }
    fn dispatch_settings(&self) -> Option<DispatchSettings> {
        None
    }
}

/// Activity of the previous tick; differs from both idle and every id before the first tick.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Previous {
    Startup,
    Idle,
    Active(String),
}

impl Previous {
    fn of(current: Option<&str>) -> Self {
        current.map_or(Previous::Idle, |id| Previous::Active(id.to_owned()))
    }

    fn is(&self, current: Option<&str>) -> bool {
        *self == Previous::of(current)
    }
}

/// Owns the listeners and the shared telemetry and runs the main loop one tick at a time,
/// together with the test-pattern writer.
pub struct Dispatcher {
    listeners: Vec<Box<dyn Listener>>,
    active: Option<usize>,
    ended: bool,
    activity: ListenerActivity,
    state: SessionState,
}

/// The test-pattern writer's listener gate, readable without locking a blocked dispatcher.
/// Selection changes before the receive step; a failed sticky check clears it.
#[derive(Clone)]
pub struct ListenerActivity(Arc<AtomicBool>);

impl ListenerActivity {
    pub fn test_pattern_allowed(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    fn select(&self, listener: Option<&str>) {
        self.0.store(listener.is_none_or(|id| matches!(id, "test" | "none")), Ordering::SeqCst);
    }
}

/// Everything the loop keeps between ticks apart from the listeners.
struct SessionState {
    telemetry: Telemetry,
    publication: Option<Arc<dyn DispatchPublication>>,
    /// Registry fields assigned by the current pass, including equal-value writes.
    telemetry_writes: Vec<&'static str>,
    /// Whether the shared dictionary has acquired `spotter_right`, the motion
    /// startup probe. Typed off values alone do not mean the dictionary was filled.
    telemetry_ready: bool,
    game: Option<String>,
    vehicle: Option<String>,
    location: Option<String>,
    vehicle_value: LegacyValue,
    location_value: LegacyValue,
    previous_listener: Previous,
    previous_game: Previous,
    previous_actual_game: Option<String>,
    max_speed: MaxSpeedTracker,
    extremes: MinMaxHandle,
    history: TelemetryHistory,
    intellitune: IntellituneHandle,
    shift_light: bool,
    wind_steering_angle: Option<u32>,
    export: Vec<ExportRow>,
    /// Every channel's `display` flag was set by a configuration refresh while `APP.additional_telemetry` was on.
    show_all_channels: bool,
    test_pattern: TestPattern,
}

impl Dispatcher {
    /// A dispatcher over the registered listeners, idle, with every channel at
    /// its value when off.
    pub fn new(listeners: Vec<Box<dyn Listener>>) -> Self {
        Dispatcher {
            listeners,
            active: None,
            ended: false,
            activity: ListenerActivity(Arc::new(AtomicBool::new(true))),
            state: SessionState {
                telemetry: Telemetry::unfilled(),
                publication: None,
                telemetry_writes: Vec::new(),
                telemetry_ready: false,
                game: None,
                vehicle: None,
                location: None,
                vehicle_value: LegacyValue::Int(0),
                location_value: LegacyValue::Int(0),
                previous_listener: Previous::Startup,
                previous_game: Previous::Startup,
                previous_actual_game: None,
                max_speed: MaxSpeedTracker::default(),
                extremes: MinMaxHandle::default(),
                history: TelemetryHistory::default(),
                intellitune: IntellituneHandle::default(),
                shift_light: false,
                wind_steering_angle: None,
                export: Vec::new(),
                show_all_channels: false,
                test_pattern: TestPattern::default(),
            },
        }
    }

    /// The configuration read runs before listener/UI startup and only turns display flags on.
    pub fn with_initial_display_all(mut self, enabled: bool) -> Self {
        self.state.show_all_channels |= enabled;
        self
    }

    /// Whether the next tick scans for games (no listener delivering), so the caller supplies fresh hits.
    pub fn is_scanning(&self) -> bool {
        self.active.is_none()
    }

    /// An unhandled listener or channel-processing failure ended the main loop.
    pub fn has_ended(&self) -> bool {
        self.ended
    }

    pub fn set_publication(&mut self, publication: Arc<dyn DispatchPublication>) {
        self.state.publication = Some(publication);
    }

    /// Runs one main-loop tick.
    pub fn tick(&mut self, input: &TickInput<'_>) -> TickOutcome {
        self.state.telemetry_writes.clear();
        let mut events = Vec::new();
        if self.ended {
            return TickOutcome { events, sleep: Duration::ZERO };
        }
        match self.active {
            Some(index) => self.keep_or_drop(index),
            None => self.scan(input.hits, &mut events),
        }
        self.state.read_shared_game();
        if self.ended {
            return TickOutcome { events, sleep: Duration::ZERO };
        }
        if let Some(index) = self.active {
            if !self.state.derive(&*self.listeners[index], input) {
                self.ended = true;
                return TickOutcome { events, sleep: Duration::ZERO };
            }
        }
        let listener = self.listener_id().map(str::to_owned);
        self.state.read_shared_game();
        let settings = self.state.dispatch_settings(input.settings);
        self.state.finish_tick(listener.as_deref(), &settings, &mut events);
        self.state.publish_identity();
        let settings = self.state.dispatch_settings(input.settings);
        let sleep = match &settings.native_refresh_rates {
            Some(rates) => match native_tick_period(rates) {
                Some(period) => period,
                None => {
                    tracing::error!("refresh-rate lookup/conversion ended the dispatcher");
                    self.ended = true;
                    return TickOutcome { events, sleep: Duration::ZERO };
                }
            },
            None => tick_period(settings.refresh_rates),
        };
        TickOutcome { events, sleep }
    }

    /// One period of the LED/tachometer test pattern (every
    /// [`super::TEST_PATTERN_PERIOD`]).
    /// Returns whether this pass assigned telemetry. Inactive passes must not
    /// republish a snapshot over another writer's changes.
    pub fn apply_test_pattern(&mut self, led_always_on: bool, tach_always_on: bool) -> bool {
        let idle = self.active.is_none();
        let changed = self.state.test_pattern.apply(idle, led_always_on, tach_always_on, &mut self.state.telemetry);
        if changed {
            for name in TestPattern::assigned_channels(idle && (led_always_on || tach_always_on)) {
                self.state.assigned(name);
            }
        }
        if idle && (led_always_on || tach_always_on) {
            self.state.telemetry_ready = true;
        }
        changed
    }

    /// The shared telemetry.
    pub fn telemetry(&self) -> &Telemetry {
        &self.state.telemetry
    }

    /// Refresh the local working frame from the shared dictionary before a pass.
    /// Listener/session bookkeeping stays local to the dispatcher.
    pub fn load_shared_telemetry(&mut self, telemetry: Telemetry, ready: bool) {
        self.state.telemetry = telemetry;
        self.state.telemetry_ready |= ready;
    }

    /// Only these registry fields were assigned by the last tick. Publishing the
    /// entire working snapshot would erase concurrent writes to untouched fields.
    pub fn telemetry_writes(&self) -> &[&'static str] {
        &self.state.telemetry_writes
    }

    pub fn listener_activity(&self) -> ListenerActivity {
        self.activity.clone()
    }

    /// Whether `spotter_right` has been assigned by a channel
    /// load, an idle reset or the test pattern. It stays present after game disconnects.
    pub fn telemetry_ready(&self) -> bool {
        self.state.telemetry_ready
    }

    /// Listener currently delivering, `None` when there is none.
    pub fn listener_id(&self) -> Option<&str> {
        self.active.map(|index| self.listeners[index].id())
    }

    /// Game found running, `None` when there is none.
    pub fn game_id(&self) -> Option<&str> {
        self.state.game.as_deref()
    }

    /// Last game that ran before the current one, `None` when none has run.
    pub fn previous_game_id(&self) -> Option<&str> {
        self.state.previous_actual_game.as_deref()
    }

    /// Vehicle identifier, `None` for 0.
    pub fn vehicle(&self) -> Option<&str> {
        self.state.vehicle.as_deref()
    }

    /// The stored vehicle name, retaining successful nontext results.
    pub fn vehicle_value(&self) -> &LegacyValue {
        &self.state.vehicle_value
    }

    /// The stored location; a failed getter stores integer zero.
    pub fn location_value(&self) -> &LegacyValue {
        &self.state.location_value
    }

    /// Writes the vehicle name from outside the loop, as the Profile Manager's vehicle Edit does;
    /// the next listener that reports a vehicle overwrites it.
    pub fn set_vehicle(&mut self, vehicle: Option<String>) {
        self.set_vehicle_value(vehicle.map_or(LegacyValue::Int(0), LegacyValue::Str));
    }

    /// An imported profile's stored vehicle value may be nontext.
    pub fn set_vehicle_value(&mut self, value: LegacyValue) {
        self.state.set_vehicle(value);
        self.state.telemetry.vehicle = self.state.vehicle.clone().unwrap_or_default();
    }

    /// Refreshes the compatibility identity without publishing an external assignment again.
    pub fn synchronize_vehicle_value(&mut self, value: LegacyValue) {
        self.state.cache_vehicle(value);
        self.state.telemetry.vehicle = self.state.vehicle.clone().unwrap_or_default();
    }

    /// Track identifier, `None` for 0.
    pub fn location(&self) -> Option<&str> {
        self.state.location.as_deref()
    }

    /// Shift light.
    pub fn shift_light(&self) -> bool {
        self.state.shift_light
    }

    /// Steering angle for the dual wind fan, set when a listener
    /// with an entry starts delivering.
    pub fn wind_steering_angle(&self) -> Option<u32> {
        self.state.wind_steering_angle
    }

    /// Per-channel extremes.
    pub fn extremes(&self) -> MinMaxTable {
        self.state.extremes.snapshot()
    }

    /// Shares the authoritative extremes independently of listener/dispatcher waits.
    pub fn extremes_handle(&self) -> MinMaxHandle {
        self.state.extremes.clone()
    }

    /// Telemetry page "Reset" button: forget every extreme.
    pub fn reset_extremes(&mut self) {
        self.state.extremes.clear();
    }

    /// Samples of the charted channel, oldest first.
    pub fn history(&self) -> VecDeque<LegacyValue> {
        self.state.history.snapshot()
    }

    /// A clone of the authoritative history handle, readable/resettable without the dispatcher mutex.
    pub fn history_handle(&self) -> TelemetryHistory {
        self.state.history.clone()
    }

    /// The charted channel changed: start a new history.
    pub fn clear_history(&mut self) {
        self.state.history.clear();
    }

    /// Intellitune samples.
    pub fn intellitune(&self) -> IntellituneSamples {
        self.state.intellitune.snapshot()
    }

    pub fn intellitune_handle(&self) -> IntellituneHandle {
        self.state.intellitune.clone()
    }

    /// Hands the Intellitune samples to the wizard and starts over.
    pub fn take_intellitune(&mut self) -> IntellituneSamples {
        self.state.intellitune.take()
    }

    /// Whether every channel is displayed (the latched `APP.additional_telemetry`), not only the registry's
    /// display channels.
    pub fn shows_all_channels(&self) -> bool {
        self.state.show_all_channels
    }

    /// Export table of the last processed tick (without a header row).
    pub fn export_rows(&self) -> &[ExportRow] {
        &self.state.export
    }

    fn index_of(&self, listener_id: &str) -> Option<usize> {
        self.listeners.iter().position(|listener| listener.id() == listener_id)
    }

    /// Sticky branch: the delivering listener stays until its liveness check fails.
    fn keep_or_drop(&mut self, index: usize) {
        let listener = &mut self.listeners[index];
        if listener.is_running() {
            listener.receive();
            self.ended = listener.failure_point() == Some(super::FailurePoint::ReceiveData);
        } else if listener.check_status_raised() {
            self.ended = true;
        } else {
            self.active = None;
            self.activity.select(None);
            if let Some(publication) = &self.state.publication {
                publication.listener_running(None);
            }
        }
    }

    /// Detection scan over the catalog hits, then the API and joystick fallbacks.
    fn scan(&mut self, hits: &[ScanHit], events: &mut Vec<DispatchEvent>) {
        let live = self.state.publication.as_ref().and_then(|publication| publication.scan_hits());
        let hits: Box<dyn Iterator<Item = Result<ScanHit, String>> + Send + '_> = match live {
            Some(hits) => hits,
            None => Box::new(hits.iter().cloned().map(Ok)),
        };
        let mut found = false;
        for hit in hits {
            let hit = match hit {
                Ok(hit) => hit,
                Err(error) => {
                    tracing::error!(%error, "game detection ended the dispatcher");
                    self.ended = true;
                    return;
                }
            };
            found = true;
            self.state.set_game(Some(hit.game_id.clone()));
            if !self.state.previous_game.is(Some(&hit.game_id)) {
                events.push(DispatchEvent::GameRunning(hit.game_id.clone()));
            }
            let Some(index) = self.index_of(&hit.listener_id) else {
                tracing::warn!(game = %hit.game_id, listener = %hit.listener_id, "no listener registered");
                continue;
            };
            if self.start(index, false, events) {
                match self.state.publication.as_ref().and_then(|value| value.telemetry_received(&hit.listener_id)) {
                    Some(Ok(())) => {}
                    Some(Err(error)) => {
                        tracing::error!(%error, "telemetry receipt assignment ended the dispatcher");
                        self.ended = true;
                    }
                    None => events.push(DispatchEvent::TelemetryReceived(hit.listener_id.clone())),
                }
                return;
            }
            if self.ended {
                return;
            }
        }
        if found {
            if let Some(publication) = &self.state.publication {
                publication.listener_running(None);
            }
            return;
        }
        for fallback in [API_LISTENER, JOYSTICK_LISTENER] {
            if let Some(index) = self.index_of(fallback) {
                if self.start(index, true, events) {
                    return;
                }
                if self.ended {
                    return;
                }
            }
        }
        self.state.set_game(None);
        if let Some(publication) = &self.state.publication {
            publication.listener_running(None);
        }
    }

    fn start(&mut self, index: usize, fallback: bool, events: &mut Vec<DispatchEvent>) -> bool {
        let listener = &mut self.listeners[index];
        if !listener.is_running() {
            if listener.check_status_raised() {
                self.ended = true;
            }
            return false;
        }
        self.active = Some(index);
        self.activity.select(Some(listener.id()));
        if fallback {
            self.state.set_game(Some(listener.id().to_owned()));
            events.push(DispatchEvent::GameRunning(listener.id().to_owned()));
        }
        if let Some(publication) = &self.state.publication {
            publication.listener_running(Some(listener.id()));
        }
        listener.receive();
        if listener.failure_point() == Some(super::FailurePoint::ReceiveData) {
            self.ended = true;
            return false;
        }
        true
    }
}

impl SessionState {
    fn set_game(&mut self, game: Option<String>) {
        self.game = game;
        if let Some(publication) = &self.publication {
            publication.game_running(self.game.as_deref());
        }
    }

    fn set_vehicle(&mut self, value: LegacyValue) {
        self.cache_vehicle(value);
        if let Some(publication) = &self.publication {
            publication.vehicle(&self.vehicle_value, self.vehicle.as_deref());
        }
    }

    fn cache_vehicle(&mut self, value: LegacyValue) {
        self.vehicle = identity_text(&value);
        self.vehicle_value = value;
    }

    fn read_shared_game(&mut self) {
        if let Some(game) = self.publication.as_ref().and_then(|publication| publication.game_snapshot()) {
            self.game = game;
        }
    }

    fn read_shared_telemetry(&mut self) {
        if let Some(telemetry) = self.publication.as_ref().and_then(|publication| publication.snapshot()) {
            self.telemetry = telemetry;
        }
    }

    fn assigned(&mut self, name: &'static str) {
        self.telemetry.assigned(name);
        if let Some(publication) = &self.publication {
            publication.assigned(name, &self.telemetry);
        }
        if !self.telemetry_writes.contains(&name) {
            self.telemetry_writes.push(name);
        }
    }

    fn assign_native(&mut self, name: &'static str, value: LegacyValue) {
        self.telemetry.set_native(name, value);
        self.assigned(name);
    }

    /// The processed part of a tick, step by step.
    fn derive(&mut self, listener: &dyn Listener, input: &TickInput<'_>) -> bool {
        if let Some(vehicle) = listener_identity(listener, "car_name") {
            self.set_vehicle(vehicle);
        }
        self.location_value = listener_identity(listener, "track_loc").unwrap_or(LegacyValue::Int(0));
        self.location = identity_text(&self.location_value);
        if let Some(publication) = &self.publication {
            publication.location(&self.location_value, self.location.as_deref());
        }
        self.update_speed(listener);
        self.update_traction_loss(listener, &input.settings.dams);
        if listener.failure_point() == Some(super::FailurePoint::MaxSpeed) {
            return false;
        }
        if !self.update_max_speed(listener, input.profile) {
            return false;
        }
        self.assign_native("steering_wheel_max_angle", native_steering_range(listener.id()));
        self.load_channels(listener);
        self.assign_native(
            "cannon_firing",
            self.runtime_inputs(input.runtime).cannon_firing.unwrap_or(LegacyValue::Int(0)),
        );
        self.assign_native(
            "missile_firing",
            self.runtime_inputs(input.runtime).missile_firing.unwrap_or(LegacyValue::Int(0)),
        );
        self.update_attitude(listener, input);
        self.read_shared_telemetry();
        if !self.record_history(self.runtime_inputs(input.runtime).plot_channel.as_deref()) {
            return false;
        }
        if let Err(error) = self.extremes.update_in_order(&self.telemetry, self.telemetry.channel_names()) {
            tracing::error!(%error, "telemetry min/max ended the dispatcher");
            return false;
        }
        self.read_shared_telemetry();
        if !self.update_gear_and_shift_light(input.profile) {
            return false;
        }
        self.read_shared_telemetry();
        self.assign_native("speed_kmh", LegacyValue::Float(self.telemetry.speed * 3.6));
        self.assign_native("max_speed_kmh", LegacyValue::Float(self.telemetry.max_speed * 3.6));
        if self.runtime_inputs(input.runtime).intellitune_capture && !self.capture_intellitune(input.games) {
            return false;
        }
        let settings = self.dispatch_settings(input.settings);
        let unit = match &settings.native_speed_unit {
            Some(Ok(unit)) => *unit,
            Some(Err(error)) => {
                tracing::error!(%error,"export measurement-unit lookup ended the dispatcher");
                return false;
            }
            None => settings.speed_unit,
        };
        self.read_shared_telemetry();
        self.export = export_rows(&self.telemetry, &self.extremes.snapshot(), unit, self.show_all_channels);
        if let Some(publication) = &self.publication {
            publication.export_rows(&self.export);
        }
        true
    }

    /// The first negative read writes 0; otherwise a second read supplies the assignment. Failures keep speed.
    fn update_speed(&mut self, listener: &dyn Listener) {
        match listener_speed_int(listener) {
            Some(speed) => {
                if speed.number().expect("integer speed") < 0.0 {
                    self.assign_native("speed", LegacyValue::Int(0));
                } else if let Some(speed) = listener_speed_int(listener) {
                    self.assign_native("speed", speed);
                } else {
                    tracing::error!(listener = listener.id(), "error while setting speed");
                }
            }
            None => tracing::error!(listener = listener.id(), "error while setting speed"),
        }
    }

    fn update_traction_loss(&mut self, listener: &dyn Listener, fallback: &DamsSettings) {
        let answer = |name| listener_value(listener, name).unwrap_or(LegacyValue::Int(0));
        let yaw = || listener_rate(listener, "yaw_rate", Rate::Yaw);
        let lateral = answer("lateral_velocity");
        let longitudinal = answer("longitudinal_velocity");
        let mut dams = self.dams_settings(fallback);
        self.read_shared_telemetry();
        let mut inputs = SlipInputs {
            lateral: lateral.number().unwrap_or(f64::NAN),
            longitudinal: longitudinal.number().unwrap_or(f64::NAN),
            speed: self.telemetry.speed,
            yaw_rate: None,
            steering: None,
        };
        let numeric_velocity = lateral.number().is_some() && longitudinal.number().is_some();
        let front = match dams.mode {
            super::DamsMode::Off => lateral.clone(),
            super::DamsMode::FrontSlip if inputs.sliding() && numeric_velocity && dams.factor.is_some() => {
                LegacyValue::Float(dams::front_traction_loss(&dams, &inputs))
            }
            super::DamsMode::FrontAndRearSlip => {
                if inputs.longitudinal != 0.0 {
                    inputs.steering = answer("steering_wheel_angle").number();
                }
                dams.front_lever = self.dams_settings(fallback).front_lever;
                self.read_shared_telemetry();
                inputs.speed = self.telemetry.speed;
                if inputs.sliding() {
                    inputs.yaw_rate = yaw().and_then(|value| value.number());
                }
                let current = self.dams_settings(fallback);
                dams.steering_ratio = current.steering_ratio;
                dams.factor = current.factor;
                if inputs.sliding()
                    && numeric_velocity
                    && inputs.yaw_rate.is_some()
                    && inputs.steering.is_some()
                    && dams.steering_ratio.is_some_and(|ratio| ratio != 0.0)
                    && dams.factor.is_some()
                {
                    LegacyValue::Float(dams::front_traction_loss(&dams, &inputs))
                } else {
                    LegacyValue::Int(0)
                }
            }
            _ => LegacyValue::Int(0),
        };
        self.assign_native("front_tl", front);
        let mut dams = self.dams_settings(fallback);
        self.read_shared_telemetry();
        inputs.speed = self.telemetry.speed;
        let rear = match dams.mode {
            super::DamsMode::Off | super::DamsMode::FrontSlip => answer("lateral_velocity"),
            super::DamsMode::FrontAndRearSlip if inputs.sliding() => {
                inputs.yaw_rate = yaw().and_then(|value| value.number());
                dams.factor = self.dams_settings(fallback).factor;
                if numeric_velocity && inputs.yaw_rate.is_some() && dams.factor.is_some() {
                    LegacyValue::Float(dams::rear_traction_loss(&dams, &inputs, None))
                } else {
                    LegacyValue::Int(0)
                }
            }
            _ => LegacyValue::Int(0),
        };
        self.assign_native("lateral_velocity", rear);
    }

    fn update_max_speed(&mut self, listener: &dyn Listener, profile: &ProfileSettings) -> bool {
        let profile = self.profile_settings(profile);
        let default_max_speed_kmh = match &profile.native {
            Some(value) => match value.as_dict().and_then(|value| value.get("default_max_speed_kmh")).cloned() {
                Some(value) => value,
                None => {
                    tracing::error!(listener = listener.id(), "getMaxSpeed default speed lookup raised");
                    return false;
                }
            },
            None => LegacyValue::Float(profile.default_max_speed_kmh),
        };
        let Some(speed) = listener_speed_int(listener) else {
            tracing::error!(listener = listener.id(), "getMaxSpeed speed conversion raised");
            return false;
        };
        match self
            .max_speed
            .update_native(listener.id(), speed, default_max_speed_kmh, |name| listener_identity(listener, name))
        {
            Some(max_speed) => {
                self.assign_native("max_speed", max_speed);
                true
            }
            None => {
                tracing::error!(listener = listener.id(), "getMaxSpeed speed/default/identity read raised");
                false
            }
        }
    }

    /// Generic loader: every `auto_load` channel from the listener, rounded where the registry says so; the
    /// registry fallback (or the previous value) when the listener cannot provide it.
    fn load_channels(&mut self, listener: &dyn Listener) {
        for spec in CHANNEL_SPECS.iter().filter(|spec| spec.auto_load) {
            match listener.read_native(spec.name) {
                NativeRead::Value(value) => {
                    let loaded = match (value, spec.round_digits) {
                        (LegacyValue::Int(value), Some(digits)) if digits >= 0 => Some(LegacyValue::Int(value)),
                        (LegacyValue::Bool(value), Some(digits)) if digits >= 0 => {
                            Some(LegacyValue::Int(i64::from(value)))
                        }
                        (value, Some(digits)) => {
                            value.number().map(|value| LegacyValue::Float(round_to(value, digits)))
                        }
                        (LegacyValue::Str(value), None) => integer_text_value(&value),
                        (value, None) => Some(value),
                    };
                    if let Some(value) = loaded {
                        self.assign_native(spec.name, value);
                    } else if let Some(value) = spec.fallback {
                        self.assign_native(spec.name, LegacyValue::Int(value as i64));
                    }
                }
                NativeRead::Raised => {
                    if let Some(value) = spec.fallback {
                        self.assign_native(spec.name, LegacyValue::Int(value as i64));
                    }
                }
                NativeRead::Unsupported => {
                    if let Some(value) = listener.channel(spec.name) {
                        let value = spec.round_digits.map_or(value, |digits| round_to(value, digits));
                        *self.telemetry.get_mut(spec.name).expect("registry channel") = value;
                        self.assigned(spec.name);
                    } else if let Some(value) = spec.fallback {
                        // Every defined registry default is an integer literal, including raw_flag's -1.
                        self.assign_native(spec.name, LegacyValue::Int(value as i64));
                    }
                }
            }
            if spec.name == "spotter_right" {
                self.telemetry_ready = true;
            }
        }
    }

    fn update_attitude(&mut self, listener: &dyn Listener, input: &TickInput<'_>) {
        self.read_shared_game();
        let answer = |name| listener_value(listener, name);
        let rate = |name, rate| listener_rate(listener, name, rate);
        let flying = self.game.as_deref().is_some_and(|game| input.games.is_flying(game));
        let yaw = if flying {
            match listener.read_native("yaw_rate") {
                NativeRead::Value(value) => Some(value),
                NativeRead::Raised => None,
                NativeRead::Unsupported => listener.rate(Rate::Yaw).map(LegacyValue::Float).or_else(|| answer("yaw")),
            }
        } else {
            answer("yaw")
        }
        .unwrap_or(LegacyValue::Int(0));
        self.assign_native("yaw", yaw.clone());
        let yaw_angle = if flying {
            answer("yaw").unwrap_or(LegacyValue::Int(0))
        } else {
            match yaw {
                LegacyValue::Int(value) => LegacyValue::Int(if value < 0 { value + 360 } else { value }),
                LegacyValue::Bool(value) => LegacyValue::Bool(value),
                LegacyValue::Float(value) => LegacyValue::Float(if value >= 0.0 { value } else { value + 360.0 }),
                _ => LegacyValue::Int(0),
            }
        };
        self.assign_native("yaw_angle", yaw_angle);
        for (name, rate_name, rate_id) in [("pitch", "pitch_rate", Rate::Pitch), ("roll", "roll_rate", Rate::Roll)] {
            let profile = self.profile_settings(input.profile);
            let source = if name == "pitch" { profile.pitch_source() } else { profile.roll_source() };
            let value = match source {
                AttitudeSource::Angle => answer(name),
                AttitudeSource::Rate => rate(rate_name, rate_id),
            }
            .unwrap_or(LegacyValue::Int(0));
            self.assign_native(name, value);
        }
    }

    fn runtime_inputs(&self, fallback: &RuntimeInputs) -> RuntimeInputs {
        self.publication
            .as_ref()
            .and_then(|publication| publication.runtime_inputs())
            .unwrap_or_else(|| fallback.clone())
    }

    fn profile_settings(&self, fallback: &ProfileSettings) -> ProfileSettings {
        self.publication
            .as_ref()
            .and_then(|publication| publication.profile_settings())
            .unwrap_or_else(|| fallback.clone())
    }

    fn dispatch_settings(&self, fallback: &DispatchSettings) -> DispatchSettings {
        self.publication
            .as_ref()
            .and_then(|publication| publication.dispatch_settings())
            .unwrap_or_else(|| fallback.clone())
    }

    fn dams_settings(&self, fallback: &DamsSettings) -> DamsSettings {
        self.publication
            .as_ref()
            .and_then(|publication| publication.dispatch_settings())
            .map_or(*fallback, |settings| settings.dams)
    }

    fn record_history(&mut self, plot_channel: Option<&str>) -> bool {
        let Some(channel) = plot_channel else { return true };
        if self.history.len() > HISTORY_LIMIT && self.history.pop_front().is_none() {
            tracing::error!("charted history became empty before its oldest sample could be removed");
            return false;
        }
        let Some(value) = self.telemetry.get_assigned(channel) else {
            tracing::error!(channel, "charted telemetry key is absent");
            return false;
        };
        let value = self.telemetry.native_value(channel).cloned().unwrap_or(LegacyValue::Float(value));
        self.history.push_back(value);
        true
    }

    /// `harmonized_gear` (reverse → 12) and the shift light (integer rpm above integer max rpm · percent / 100).
    fn update_gear_and_shift_light(&mut self, profile: &ProfileSettings) -> bool {
        let gear = self.telemetry.native_value("gear").cloned().unwrap_or(LegacyValue::Float(self.telemetry.gear));
        let value = if openrigcue_model::python_compare::native_equal(&gear, &LegacyValue::Int(REVERSE_GEAR as i64)) {
            LegacyValue::Int(HARMONIZED_REVERSE as i64)
        } else {
            gear
        };
        self.assign_native("harmonized_gear", value);
        let integer = |name| match self.telemetry.native_value(name) {
            Some(value) => native_integer_value(value.clone()),
            None => self.telemetry.get(name).and_then(truncate).map(integer_view),
        };
        let (Some(rpm), Some(max_rpm)) = (integer("rpm"), integer("max_rpm")) else {
            tracing::error!("shift light conversion ended the dispatcher");
            return false;
        };
        let profile = self.profile_settings(profile);
        let percent = match &profile.native {
            Some(value) => value.as_dict().and_then(|profile| profile.get("gear_shift_perc")).cloned(),
            None => Some(LegacyValue::Float(profile.gear_shift_percent)),
        };
        let Some(percent) = percent.filter(|value| value.number().is_some()) else {
            tracing::error!("shift light percentage lookup/arithmetic ended the dispatcher");
            return false;
        };
        let integer_percent = match percent {
            LegacyValue::Int(value) => Some(value),
            LegacyValue::Bool(value) => Some(i64::from(value)),
            _ => None,
        };
        let threshold = match (&max_rpm, integer_percent) {
            (LegacyValue::Int(max_rpm), Some(percent)) => {
                openrigcue_model::python_numeric::int_true_divide(i128::from(*max_rpm) * i128::from(percent), 100)
                    .expect("nonzero divisor")
            }
            _ => max_rpm.number().expect("integer RPM") * percent.number().expect("numeric percent") / 100.0,
        };
        self.shift_light = matches!(
            super::min_max::numeric_cmp(&rpm, &LegacyValue::Float(threshold)),
            Ok(Some(std::cmp::Ordering::Greater))
        );
        if let Some(publication) = &self.publication {
            publication.shift_light(self.shift_light);
        }
        true
    }

    fn capture_intellitune(&mut self, games: &dyn GameDirectory) -> bool {
        self.read_shared_game();
        let Some(game) = self.game.as_deref() else {
            return true;
        };
        let reference = games.intellitune_reference(game).unwrap_or_else(|| IntellituneReference {
            sway: 0.0,
            heave: 0.0,
            surge_acceleration: 0.0,
            yaw: 0.0,
            roll: 0.0,
            pitch: 0.0,
            traction_loss: 0.0,
            native: Some(Err(format!("'{game}'"))),
        });
        self.read_shared_telemetry();
        match self.intellitune.capture_checked(&self.telemetry, &reference) {
            Ok(()) => true,
            Err(error) => {
                tracing::error!(%error,"IntelliTune capture ended the dispatcher");
                false
            }
        }
    }

    /// Stand-by reset, game-closed reset, configuration refresh and the previous-game bookkeeping at the end of
    /// every tick.
    fn finish_tick(&mut self, listener: Option<&str>, settings: &DispatchSettings, events: &mut Vec<DispatchEvent>) {
        if listener.is_none() && self.previous_listener != Previous::Idle {
            self.history.clear();
            self.set_vehicle(LegacyValue::Int(0));
            reset_channels(&mut self.telemetry);
            for &name in Telemetry::CHANNELS {
                self.assigned(name);
            }
            self.telemetry_ready = true;
            self.max_speed.reset();
        }
        if self.game.is_none() && self.previous_game != Previous::Idle {
            self.extremes.clear();
            if let Some(publication) = &self.publication {
                publication.reset_min_max();
            }
            events.push(DispatchEvent::MinMaxReset);
        }
        if !self.previous_listener.is(listener) {
            events.push(DispatchEvent::ConfigRefresh);
            self.show_all_channels |= settings.show_all_channels;
            if let Some(publication) = &self.publication {
                publication.show_all_channels(self.show_all_channels);
            }
            if let Some(id) = listener {
                match wind_steering_angle(id) {
                    Some(angle) => self.wind_steering_angle = Some(angle),
                    None => tracing::error!(listener = id, "cannot set global max steering wheel angle"),
                }
            }
        }
        self.previous_listener = Previous::of(listener);
        self.remember_previous_game();
        self.previous_game = Previous::of(self.game.as_deref());
    }

    /// The previous game: the game that ran on the previous tick, recorded when the game closes or,
    /// while one keeps running, the first time nothing is recorded yet.
    fn remember_previous_game(&mut self) {
        let previous = match &self.previous_game {
            Previous::Idle => return,
            Previous::Startup => None,
            Previous::Active(id) => Some(id.clone()),
        };
        if self.game.is_none() || self.previous_actual_game.is_none() {
            self.previous_actual_game = previous;
        }
    }

    /// Copies game, vehicle and track into the shared telemetry's identity fields.
    fn publish_identity(&mut self) {
        self.telemetry.game = self.game.clone().unwrap_or_default();
        self.telemetry.vehicle = self.vehicle.clone().unwrap_or_default();
        self.telemetry.location = self.location.clone().unwrap_or_default();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::derive::dams::DamsMode;
    use crate::derive::testing::{FakeListener, FakeState};

    struct Games;

    impl GameDirectory for Games {
        fn is_flying(&self, game_id: &str) -> bool {
            game_id == "xplane"
        }
        fn intellitune_reference(&self, game_id: &str) -> Option<IntellituneReference> {
            (game_id == "dirtrally2.0").then_some(IntellituneReference {
                sway: 1.0,
                heave: 1.0,
                surge_acceleration: 1.0,
                yaw: 1.0,
                roll: 1.0,
                pitch: 1.0,
                traction_loss: 1.0,
                native: None,
            })
        }
    }

    fn settings() -> DispatchSettings {
        DispatchSettings {
            dams: DamsSettings {
                mode: DamsMode::Off,
                factor: Some(1.0),
                front_lever: 0.0,
                rear_lever: 1.44,
                steering_ratio: Some(14.5),
            },
            refresh_rates: RefreshRates { shakebox: 10, intellibox: 60, motionbox: 30 },
            native_refresh_rates: None,
            speed_unit: SpeedUnit::Kmh,
            native_speed_unit: None,
            show_all_channels: false,
        }
    }

    fn profile() -> ProfileSettings {
        ProfileSettings {
            default_max_speed_kmh: 200.0,
            gear_shift_percent: 96.0,
            pitch_source: AttitudeSource::Angle,
            roll_source: AttitudeSource::Angle,
            native: None,
        }
    }

    struct Rig {
        dispatcher: Dispatcher,
        states: Vec<(String, Arc<Mutex<FakeState>>)>,
        settings: DispatchSettings,
        profile: ProfileSettings,
        runtime: RuntimeInputs,
    }

    impl Rig {
        fn new(ids: &[&str]) -> Self {
            let mut states = Vec::new();
            let mut listeners: Vec<Box<dyn Listener>> = Vec::new();
            for id in ids {
                // Ordinary fixtures deliver a readable stationary speed. A missing speed
                // is an unhandled top-speed failure and must be requested explicitly.
                let mut fake = FakeListener::new(id);
                fake.set("speed", 0.0);
                if MaxSpeedTracker::tracks_identity(id) {
                    let mut state = fake.state.lock().unwrap();
                    state.vehicle = Some(String::new());
                    state.location = Some(String::new());
                }
                states.push((id.to_string(), fake.state.clone()));
                listeners.push(Box::new(fake));
            }
            Rig {
                dispatcher: Dispatcher::new(listeners),
                states,
                settings: settings(),
                profile: profile(),
                runtime: RuntimeInputs::default(),
            }
        }

        fn state(&self, id: &str) -> std::sync::MutexGuard<'_, FakeState> {
            self.states.iter().find(|(name, _)| name == id).unwrap().1.lock().unwrap()
        }

        fn run(&mut self, id: &str, running: bool) {
            self.state(id).running = running;
        }

        fn set(&mut self, id: &str, channel: &str, value: f64) {
            self.state(id).channels.insert(channel.to_owned(), value);
        }

        fn tick(&mut self, hits: &[(&str, &str)]) -> TickOutcome {
            let hits: Vec<ScanHit> = hits
                .iter()
                .map(|(game, listener)| ScanHit { game_id: game.to_string(), listener_id: listener.to_string() })
                .collect();
            let input = TickInput {
                hits: &hits,
                settings: &self.settings,
                profile: &self.profile,
                runtime: &self.runtime,
                games: &Games,
            };
            self.dispatcher.tick(&input)
        }
    }

    fn game(id: &str) -> DispatchEvent {
        DispatchEvent::GameRunning(id.to_owned())
    }

    #[test]
    fn huge_refresh_rate_ends_dispatcher_after_completed_frame_and_skips_next_receive() {
        let mut rig = Rig::new(&[]);
        let huge = LegacyValue::Str(format!("1{}", "0".repeat(400)));
        rig.settings.native_refresh_rates = Some([Some(huge), Some(30.into()), Some(30.into())]);
        let outcome = rig.tick(&[]);
        assert_eq!(outcome.sleep, Duration::ZERO);
        assert!(rig.dispatcher.has_ended());
        assert!(outcome.events.contains(&DispatchEvent::ConfigRefresh));
        rig.settings.native_refresh_rates = Some([Some(30.into()), Some(30.into()), Some(30.into())]);
        assert!(rig.tick(&[]).events.is_empty());
        assert!(rig.dispatcher.has_ended(), "the ended thread does not resume on valid later input");
    }

    #[test]
    fn native_none_lateral_matches_dams_handlers_and_direct_copy() {
        let mut listener = FakeListener::new("api");
        listener.state.lock().unwrap().native_channels.insert("lateral_velocity".into(), LegacyValue::None);
        listener.set("longitudinal_velocity", 10.0);
        listener.set("steering_wheel_angle", 29.0);
        listener.set_rate(Rate::Yaw, 0.5);
        for mode in [DamsMode::Off, DamsMode::FrontSlip, DamsMode::FrontAndRearSlip] {
            for speed in [0.0, 5.0] {
                let mut rig = Rig::new(&[]);
                let settings = DamsSettings { mode, ..rig.settings.dams };
                rig.dispatcher.state.assign_native("speed", LegacyValue::Float(speed));
                rig.dispatcher.state.assign_native("front_tl", LegacyValue::Float(17.0));
                rig.dispatcher.state.assign_native("lateral_velocity", LegacyValue::Float(23.0));
                rig.dispatcher.state.update_traction_loss(&listener, &settings);
                let front = if mode == DamsMode::Off { LegacyValue::None } else { LegacyValue::Int(0) };
                let rear = if mode == DamsMode::FrontAndRearSlip { LegacyValue::Int(0) } else { LegacyValue::None };
                assert_eq!(rig.dispatcher.telemetry().native_value("front_tl"), Some(&front), "{mode:?} speed={speed}");
                assert_eq!(
                    rig.dispatcher.telemetry().native_value("lateral_velocity"),
                    Some(&rear),
                    "{mode:?} speed={speed}"
                );
            }
        }
    }

    #[test]
    fn native_loads_preserve_types_and_bad_text_ends_after_history_before_minmax() {
        let mut rig = Rig::new(&["api"]);
        rig.tick(&[]); // The ordinary startup standby pass seeds registry insertion order.
        rig.run("api", true);
        rig.runtime.plot_channel = Some("speed".into());
        rig.state("api").native_channels.extend([
            ("gear".into(), LegacyValue::Float(3.0)),
            ("rpm".into(), LegacyValue::Str("４_０００".into())),
            ("pitch".into(), LegacyValue::Str("bad".into())),
            ("steering_wheel_angle".into(), LegacyValue::Str("1.25".into())),
        ]);
        let outcome = rig.tick(&[]);
        let telemetry = rig.dispatcher.telemetry();
        assert_eq!(outcome.sleep, Duration::ZERO);
        assert_eq!(telemetry.native_value("pitch"), Some(&LegacyValue::Str("bad".into())));
        assert_eq!(telemetry.native_value("gear"), Some(&LegacyValue::Float(3.0)));
        assert_eq!(telemetry.native_value("rpm"), Some(&LegacyValue::Int(4000)));
        assert_eq!(telemetry.steering_wheel_angle, 0.0, "round(str, 2) raises and takes the fallback");
        assert!(rig.dispatcher.telemetry_ready());
        assert_eq!(rig.dispatcher.history().len(), 1);
        assert!(rig.dispatcher.export_rows().is_empty());
        assert_eq!(rig.dispatcher.extremes().get("pitch"), None);
        assert_eq!(rig.state("api").receive_calls, 1);
        rig.tick(&[]);
        assert_eq!(rig.state("api").receive_calls, 1, "the ended dispatcher never receives again");
    }

    #[test]
    fn auto_load_over_limit_integer_text_retains_the_previous_value() {
        let listener = FakeListener::new("text");
        let mut dispatcher = Dispatcher::new(Vec::new());
        dispatcher.state.telemetry.set_native("rpm", LegacyValue::Int(4200));
        let over = format!("{}7", "0".repeat(4300));
        listener.state.lock().unwrap().native_channels.insert("rpm".into(), LegacyValue::Str(over));
        dispatcher.state.load_channels(&listener);
        assert_eq!(dispatcher.telemetry().native_value("rpm"), Some(&LegacyValue::Int(4200)));
        assert!(!dispatcher.state.telemetry_writes.contains(&"rpm"));
    }

    #[test]
    fn auto_load_keeps_finite_wide_text_magnitudes_and_normalizes_integer_zero() {
        let listener = FakeListener::new("text");
        let mut dispatcher = Dispatcher::new(Vec::new());
        for (text, expected) in [
            ("100000000000000000000", LegacyValue::Float(1e20)),
            ("-100000000000000000000", LegacyValue::Float(-1e20)),
            ("-0", LegacyValue::Int(0)),
        ] {
            listener.state.lock().unwrap().native_channels.insert("rpm".into(), LegacyValue::Str(text.into()));
            dispatcher.state.load_channels(&listener);
            assert_eq!(dispatcher.telemetry().native_value("rpm"), Some(&expected));
        }
        listener.state.lock().unwrap().native_channels.insert("rpm".into(), LegacyValue::Str("bad".into()));
        dispatcher.state.load_channels(&listener);
        assert_eq!(dispatcher.telemetry().native_value("rpm"), Some(&LegacyValue::Int(0)));
    }

    #[test]
    fn failed_native_auto_load_keeps_the_previous_native_value_without_a_fallback() {
        let mut rig = Rig::new(&["api"]);
        rig.run("api", true);
        rig.state("api")
            .native_channels
            .extend([("gear".into(), LegacyValue::Int(3)), ("steering_wheel_angle".into(), LegacyValue::Int(1))]);
        rig.tick(&[]);
        assert_eq!(rig.dispatcher.telemetry().native_value("harmonized_gear"), Some(&LegacyValue::Int(3)));
        assert_eq!(rig.dispatcher.telemetry().native_value("steering_wheel_angle"), Some(&LegacyValue::Int(1)));
        rig.state("api").native_channels.insert("gear".into(), LegacyValue::Str("P".into()));
        rig.tick(&[]);
        assert_eq!(rig.dispatcher.telemetry().native_value("gear"), Some(&LegacyValue::Int(3)));
    }

    #[test]
    fn nonfinite_rpm_ends_after_minmax_and_gear_before_kmh_export_and_finish() {
        let mut rig = Rig::new(&["api"]);
        rig.run("api", true);
        rig.set("api", "speed", 20.0);
        rig.set("api", "rpm", f64::INFINITY);
        rig.set("api", "gear", -1.0);
        let outcome = rig.tick(&[]);
        assert_eq!(outcome.sleep, Duration::ZERO);
        assert_eq!(rig.dispatcher.telemetry().harmonized_gear, HARMONIZED_REVERSE);
        assert_eq!(rig.dispatcher.telemetry().speed_kmh, 0.0);
        assert_eq!(rig.dispatcher.extremes().get("speed").unwrap().max, 20.0);
        assert_eq!(rig.dispatcher.extremes().get("rpm").unwrap().max, f64::INFINITY);
        assert!(rig.dispatcher.export_rows().is_empty());
        assert_eq!(outcome.events, [game("api")]);
    }

    #[test]
    fn fatal_check_stops_the_scan_before_other_listeners_and_tick_bookkeeping() {
        let mut rig = Rig::new(&["bad", "api", "joy"]);
        rig.state("bad").check_status_raised = true;
        rig.run("api", true);
        let outcome = rig.tick(&[("bad-game", "bad"), ("api-game", "api")]);
        assert_eq!(outcome.events, [game("bad-game")]);
        assert_eq!(outcome.sleep, Duration::ZERO);
        assert_eq!(rig.state("bad").check_calls, 1);
        assert_eq!(rig.state("api").check_calls, 0);
        assert_eq!(rig.state("joy").check_calls, 0);
        assert_eq!(rig.dispatcher.listener_id(), None);
        assert!(rig.dispatcher.export_rows().is_empty());
        assert!(!rig.dispatcher.telemetry_ready());
        assert_eq!(rig.dispatcher.extremes().get("rpm"), None);
        assert!(rig.tick(&[]).events.is_empty());
        assert_eq!(rig.state("bad").check_calls, 1, "the ended thread never checks again");
    }

    #[test]
    fn fatal_active_check_does_not_publish_an_idle_transition() {
        let mut rig = Rig::new(&["api", "joy"]);
        rig.run("api", true);
        rig.set("api", "speed", 30.0);
        rig.tick(&[]);
        let before = rig.dispatcher.telemetry().clone();
        let before_history = rig.dispatcher.history().len();
        rig.state("api").check_status_raised = true;
        assert!(rig.tick(&[]).events.is_empty());
        assert_eq!(rig.dispatcher.listener_id(), Some("api"));
        assert_eq!(rig.dispatcher.telemetry(), &before);
        assert_eq!(rig.dispatcher.history().len(), before_history);
        assert_eq!(rig.state("joy").check_calls, 0);
    }

    #[test]
    fn receive_data_failure_keeps_the_selected_listener_without_deriving_a_frame() {
        let mut rig = Rig::new(&["api", "joy"]);
        rig.run("api", true);
        rig.state("api").poll_failure = Some(super::super::FailurePoint::ReceiveData);
        let before = rig.dispatcher.telemetry().clone();
        let result = rig.tick(&[]);
        assert_eq!(result.events, [game("api")]);
        assert_eq!(rig.dispatcher.game_id(), Some("api"));
        assert_eq!(rig.dispatcher.listener_id(), Some("api"));
        assert_eq!(rig.dispatcher.telemetry(), &before);
        assert!(!rig.dispatcher.telemetry_ready());
        assert_eq!(rig.state("api").receive_calls, 1);
        assert_eq!(rig.state("joy").check_calls, 0);
        assert!(rig.tick(&[]).events.is_empty());
        assert_eq!(rig.state("api").receive_calls, 1);
    }

    #[test]
    fn max_speed_failure_keeps_only_the_values_written_before_that_call() {
        let mut rig = Rig::new(&["api"]);
        rig.run("api", true);
        rig.set("api", "lateral_velocity", 17.0);
        rig.set("api", "rpm", 9000.0);
        rig.state("api").vehicle = Some("new car".to_owned());
        rig.state("api").poll_failure = Some(super::super::FailurePoint::MaxSpeed);
        rig.dispatcher.state.telemetry.rpm = 123.0;
        rig.dispatcher.state.telemetry.max_speed = 37.0;
        let result = rig.tick(&[]);
        assert_eq!(result.events, [game("api")]);
        assert_eq!(rig.dispatcher.vehicle(), Some("new car"));
        assert_eq!(rig.dispatcher.telemetry().lateral_velocity, 17.0);
        assert_eq!(rig.dispatcher.telemetry().rpm, 123.0);
        assert_eq!(rig.dispatcher.telemetry().max_speed, 37.0);
        assert!(!rig.dispatcher.telemetry_ready());
        assert!(rig.dispatcher.history().is_empty());
        assert!(rig.dispatcher.export_rows().is_empty());
    }

    #[test]
    fn native_zero_identity_clears_the_prior_vehicle() {
        let source = super::super::testing::ScriptedReadSource::new(
            "api",
            [
                ("car_name", vec![NativeRead::Value(LegacyValue::Int(0))]),
                ("track_loc", vec![NativeRead::Value(LegacyValue::Int(0))]),
            ],
        );
        let mut rig = Rig::new(&["api"]);
        rig.dispatcher.listeners = vec![Box::new(super::super::SourceListener::new(source))];
        rig.dispatcher.set_vehicle(Some("previous".into()));
        rig.tick(&[]);
        assert_eq!(rig.dispatcher.vehicle(), None);
    }

    #[test]
    fn live_publication_keeps_the_loader_then_native_harmonized_gear_steps() {
        #[derive(Default)]
        struct Capture(Mutex<Vec<LegacyValue>>);
        impl DispatchPublication for Capture {
            fn assigned(&self, name: &'static str, telemetry: &Telemetry) {
                if name == "harmonized_gear" {
                    self.0.lock().unwrap().push(
                        telemetry.native_value(name).cloned().unwrap_or(LegacyValue::Float(telemetry.harmonized_gear)),
                    );
                }
            }
        }
        let source = super::super::testing::ScriptedReadSource::new(
            "api",
            [("gear", vec![NativeRead::Value(LegacyValue::Int(3))])],
        );
        let mut rig = Rig::new(&["api"]);
        rig.dispatcher.listeners = vec![Box::new(super::super::SourceListener::new(source))];
        let capture = Arc::new(Capture::default());
        rig.dispatcher.set_publication(capture.clone());
        rig.tick(&[]);
        assert_eq!(*capture.0.lock().unwrap(), [LegacyValue::Int(0), LegacyValue::Int(3)]);
    }

    #[test]
    fn native_identity_values_keep_successes_and_their_distinct_failure_rules() {
        use NativeRead::{Raised, Value};
        let vehicle = LegacyValue::List(vec![LegacyValue::Int(7)]);
        let source = super::super::testing::ScriptedReadSource::new(
            "api",
            [("car_name", vec![Value(vehicle.clone()), Raised]), ("track_loc", vec![Value(LegacyValue::None), Raised])],
        );
        let reads = source.reads.clone();
        let mut rig = Rig::new(&["api"]);
        rig.dispatcher.listeners = vec![Box::new(super::super::SourceListener::new(source))];
        rig.tick(&[]);
        assert_eq!(rig.dispatcher.vehicle_value(), &vehicle);
        assert_eq!(rig.dispatcher.location_value(), &LegacyValue::None);
        assert_eq!(rig.dispatcher.vehicle(), Some("[7]"));
        assert_eq!(rig.dispatcher.location(), Some("None"));
        assert_eq!(&reads.lock().unwrap()[..2], &["car_name", "track_loc"]);
        rig.tick(&[]);
        assert_eq!(rig.dispatcher.vehicle_value(), &vehicle, "failed vehicle getter retains its prior raw value");
        assert_eq!(rig.dispatcher.location_value(), &LegacyValue::Int(0));
        assert_eq!(rig.dispatcher.location(), None);
        assert_eq!(reads.lock().unwrap().iter().filter(|name| name.as_str() == "car_name").count(), 2);
        rig.dispatcher.set_vehicle(None);
        assert_eq!(rig.dispatcher.vehicle_value(), &LegacyValue::Int(0));
        rig.dispatcher.set_vehicle(Some(String::new()));
        assert_eq!(rig.dispatcher.vehicle_value(), &LegacyValue::Str(String::new()));
    }

    #[test]
    fn nonnegative_speed_is_read_again_for_assignment_before_the_maximum_read() {
        let source = super::super::testing::ScriptedReadSource::new(
            "api",
            [(
                "speed",
                vec![
                    NativeRead::Value(LegacyValue::Int(1)),
                    NativeRead::Value(LegacyValue::Int(2)),
                    NativeRead::Value(LegacyValue::Int(3)),
                ],
            )],
        );
        let reads = source.reads.clone();
        let mut rig = Rig::new(&["api"]);
        rig.dispatcher.listeners = vec![Box::new(super::super::SourceListener::new(source))];
        rig.profile.default_max_speed_kmh = 0.0;
        rig.tick(&[]);
        assert_eq!(rig.dispatcher.telemetry().native_value("speed"), Some(&LegacyValue::Int(2)));
        assert_eq!(rig.dispatcher.telemetry().native_value("max_speed"), Some(&LegacyValue::Int(3)));
        assert_eq!(reads.lock().unwrap().iter().filter(|name| name.as_str() == "speed").count(), 3);
        assert_eq!(
            reads.lock().unwrap().iter().filter(|name| name.as_str() == "car_name").count(),
            1,
            "unlisted listeners do not query identities again in getMaxSpeed"
        );
        assert_eq!(reads.lock().unwrap().iter().filter(|name| name.as_str() == "track_loc").count(), 1);
    }

    #[test]
    fn speed_assignment_reads_preserve_negative_and_second_read_failure_branches() {
        use NativeRead::{Raised, Value};
        for (speeds, expected, count) in [
            (vec![Value(LegacyValue::Int(-1)), Value(LegacyValue::Int(3))], 0, 2),
            (vec![Value(LegacyValue::Int(1)), Value(LegacyValue::Int(-2)), Value(LegacyValue::Int(3))], -2, 3),
            (vec![Value(LegacyValue::Int(1)), Raised, Value(LegacyValue::Int(3))], 17, 3),
        ] {
            let source = super::super::testing::ScriptedReadSource::new("api", [("speed", speeds)]);
            let reads = source.reads.clone();
            let mut rig = Rig::new(&["api"]);
            rig.dispatcher.listeners = vec![Box::new(super::super::SourceListener::new(source))];
            rig.dispatcher.state.telemetry.set_native("speed", LegacyValue::Int(17));
            rig.profile.default_max_speed_kmh = 0.0;
            rig.tick(&[]);
            assert!(!rig.dispatcher.has_ended());
            assert_eq!(rig.dispatcher.telemetry().native_value("speed"), Some(&LegacyValue::Int(expected)));
            assert_eq!(rig.dispatcher.telemetry().native_value("max_speed"), Some(&LegacyValue::Int(3)));
            assert_eq!(reads.lock().unwrap().iter().filter(|name| name.as_str() == "speed").count(), count);
        }
    }

    #[test]
    fn max_speed_profile_lookup_and_arithmetic_fail_on_opposite_sides_of_the_getter() {
        use openrigcue_model::native_value::Dict;
        for (profile_default, count) in [(None, 2), (Some(LegacyValue::Str("bad".into())), 3)] {
            let source = super::super::testing::ScriptedReadSource::new(
                "api",
                [("speed", vec![NativeRead::Value(LegacyValue::Int(6)); count])],
            );
            let reads = source.reads.clone();
            let mut rig = Rig::new(&["api"]);
            rig.dispatcher.listeners = vec![Box::new(super::super::SourceListener::new(source))];
            let mut profile = Dict::new();
            if let Some(value) = profile_default {
                profile.insert("default_max_speed_kmh", value);
            }
            rig.profile.native = Some(LegacyValue::Dict(profile));
            rig.tick(&[]);
            assert!(rig.dispatcher.has_ended());
            assert_eq!(rig.dispatcher.telemetry().native_value("speed"), Some(&LegacyValue::Int(6)));
            assert!(!rig.dispatcher.telemetry_ready());
            assert_eq!(reads.lock().unwrap().iter().filter(|name| name.as_str() == "speed").count(), count);
        }
    }

    #[test]
    fn failed_max_speed_identity_read_ends_after_prior_speed_and_parameter_writes() {
        use NativeRead::{Raised, Value};
        let source = super::super::testing::ScriptedReadSource::new(
            "rf1",
            [
                ("speed", vec![Value(LegacyValue::Int(10)), Value(LegacyValue::Int(11)), Value(LegacyValue::Int(12))]),
                ("car_name", vec![Value(LegacyValue::Str("new car".into()))]),
                ("track_loc", vec![Value(LegacyValue::Str("track".into())), Raised]),
            ],
        );
        let reads = source.reads.clone();
        let mut rig = Rig::new(&["rf1"]);
        rig.dispatcher.listeners = vec![Box::new(super::super::SourceListener::new(source))];
        rig.dispatcher.state.telemetry.set_native("rpm", LegacyValue::Int(123));
        rig.dispatcher.state.telemetry.set_native("max_speed", LegacyValue::Int(37));
        rig.tick(&[("fixture", "rf1")]);
        assert!(rig.dispatcher.has_ended());
        assert_eq!(rig.dispatcher.vehicle(), Some("new car"));
        assert_eq!(rig.dispatcher.location(), Some("track"));
        assert_eq!(rig.dispatcher.telemetry().native_value("speed"), Some(&LegacyValue::Int(11)));
        assert_eq!(rig.dispatcher.telemetry().native_value("max_speed"), Some(&LegacyValue::Int(37)));
        assert_eq!(rig.dispatcher.telemetry().native_value("rpm"), Some(&LegacyValue::Int(123)));
        assert!(!rig.dispatcher.telemetry_ready());
        assert!(rig.dispatcher.history().is_empty());
        let reads = reads.lock().unwrap();
        assert_eq!(reads.iter().filter(|name| name.as_str() == "speed").count(), 3);
        assert_eq!(reads.iter().filter(|name| name.as_str() == "car_name").count(), 1);
        assert_eq!(reads.iter().filter(|name| name.as_str() == "track_loc").count(), 2);
    }

    #[test]
    fn explicit_native_failures_do_not_reuse_numeric_frames_or_retry_getters() {
        let source = super::super::testing::ExplicitReadSource::default();
        let reads = source.reads.clone();
        let mut listener = super::super::SourceListener::new(source);
        listener.receive();
        assert_eq!(listener_speed_int(&listener), None);
        assert_eq!(listener_value(&listener, "rpm"), Some(LegacyValue::None));
        assert_eq!(listener_value(&listener, "gear"), Some(LegacyValue::Float(3.0)));
        assert_eq!(listener_rate(&listener, "yaw_rate", Rate::Yaw), None);
        assert_eq!(*reads.lock().unwrap(), ["speed", "rpm", "gear", "yaw_rate"]);
    }

    #[test]
    fn raised_native_flying_rate_keeps_zero_yaw_and_separate_angle_read() {
        struct Flying;
        impl GameDirectory for Flying {
            fn is_flying(&self, _: &str) -> bool {
                true
            }
            fn intellitune_reference(&self, _: &str) -> Option<IntellituneReference> {
                None
            }
        }
        let source = super::super::testing::ExplicitReadSource::default();
        let reads = source.reads.clone();
        let mut listener = super::super::SourceListener::new(source);
        listener.receive();
        let mut rig = Rig::new(&["api"]);
        rig.dispatcher.state.game = Some("plane".into());
        let input = TickInput {
            hits: &[],
            settings: &rig.settings,
            profile: &rig.profile,
            runtime: &rig.runtime,
            games: &Flying,
        };
        rig.dispatcher.state.update_attitude(&listener, &input);
        assert_eq!(rig.dispatcher.telemetry().native_value("yaw"), Some(&LegacyValue::Int(0)));
        assert_eq!(rig.dispatcher.telemetry().native_value("yaw_angle"), Some(&LegacyValue::Float(31.0)));
        assert_eq!(*reads.lock().unwrap(), ["yaw_rate", "yaw", "pitch", "roll"]);
    }

    #[test]
    fn raw_farming_and_vracer_values_fail_at_their_dispatcher_stages() {
        use crate::derive::SourceListener;
        use crate::process_memory::{
            farming_simulator::FarmingSimulator,
            vracer::{DatagramPort, VRacer},
        };
        struct Packet(Vec<u8>);
        impl DatagramPort for Packet {
            fn receive_one(&mut self) -> std::io::Result<Vec<u8>> {
                Ok(self.0.clone())
            }
        }
        let fs = "{'qx1':0,'qy1':0,'qz1':0,'qw1':1,'rpm':1200.0,'speed':6.0,'avZ1':0.0,'lvX1':1.0,'lvZ1':2.0}";
        let vr = r#"{"onTrack":true,"paused":false,"accelerationX":1.0,"accelerationY":2.0,"accelerationZ":3.0,"angularSpeedX":17.0,"angularSpeedY":0.0,"angularSpeedZ":0.0,"rotationX":0.0,"rotationY":0.0,"rotationZ":0.0,"gear":3,"rpm":1200.0,"speed":6.0}"#;
        for field in ["speed", "rpm"] {
            let fs = fs.replace(&format!("'{field}':"), &format!("'{field}':None,'unused':"));
            let vr = vr.replace(&format!("\"{field}\":"), &format!("\"{field}\":null,\"unused\":"));
            let listeners: Vec<(&str, Box<dyn Listener>)> = vec![
                ("fs", Box::new(SourceListener::new(FarmingSimulator::with_fixture_payload(&fs)))),
                ("vracer", Box::new(SourceListener::new(VRacer::with_port(Packet(vr.into_bytes()))))),
            ];
            for (id, listener) in listeners {
                let mut rig = Rig::new(&[id]);
                rig.dispatcher.listeners = vec![listener];
                rig.dispatcher.state.telemetry = Telemetry::off();
                rig.dispatcher.state.telemetry.set_native("rpm", LegacyValue::Int(123));
                rig.runtime.plot_channel = Some("speed".into());
                let outcome = rig.tick(&[(id, id)]);
                assert_eq!(outcome.sleep, Duration::ZERO, "{id} {field}");
                assert_eq!(
                    rig.dispatcher.telemetry_ready(),
                    field == "rpm",
                    "the registry marks data ready before a later min/max failure: {id} {field}"
                );
                assert!(rig.dispatcher.export_rows().is_empty());
                if field == "speed" {
                    assert_eq!(
                        rig.dispatcher.telemetry().native_value("rpm"),
                        Some(&LegacyValue::Int(123)),
                        "max-speed failure precedes registry loading"
                    );
                    assert!(rig.dispatcher.history().is_empty());
                } else {
                    assert_eq!(
                        rig.dispatcher.telemetry().native_value("rpm"),
                        Some(&LegacyValue::None),
                        "None reaches registry storage before min/max fails"
                    );
                    assert_eq!(rig.dispatcher.telemetry().native_value("speed"), Some(&LegacyValue::Int(6)));
                    assert_eq!(rig.dispatcher.history().len(), 1, "history precedes the min/max failure");
                }
            }
        }
    }

    /// The first tick is a stand-by transition: off values, min/max reset, configuration refresh.
    #[test]
    fn startup_tick_is_a_standby_transition() {
        let mut rig = Rig::new(&["api", "joy"]);
        let outcome = rig.tick(&[]);
        assert_eq!(outcome.events, [DispatchEvent::MinMaxReset, DispatchEvent::ConfigRefresh]);
        assert_eq!(outcome.sleep, Duration::from_secs_f64(1.0 / 60.0));
        assert_eq!(rig.dispatcher.telemetry(), &Telemetry::off());
        assert_eq!(rig.tick(&[]).events, []);
        assert_eq!(rig.dispatcher.game_id(), None);
    }

    #[test]
    fn shared_telemetry_reset_publishes_before_later_standby_bookkeeping() {
        struct Capture(std::sync::Mutex<Vec<&'static str>>);
        impl DispatchPublication for Capture {
            fn assigned(&self, _: &'static str, _: &Telemetry) {}
            fn reset_min_max(&self) {
                self.0.lock().unwrap().push("reset");
            }
            fn show_all_channels(&self, _: bool) {
                self.0.lock().unwrap().push("show");
            }
        }
        let mut rig = Rig::new(&["api", "joy"]);
        let capture = std::sync::Arc::new(Capture(std::sync::Mutex::new(Vec::new())));
        rig.dispatcher.set_publication(capture.clone());
        rig.tick(&[]);
        assert_eq!(*capture.0.lock().unwrap(), ["reset", "show"]);
        rig.tick(&[]);
        assert_eq!(*capture.0.lock().unwrap(), ["reset", "show"], "steady standby must not reassert reset");
    }

    /// Scan: every found game is published once, the first delivering listener wins and is marked.
    #[test]
    fn scan_picks_first_delivering_listener() {
        let mut rig = Rig::new(&["rfpro", "rf1", "api", "joy"]);
        rig.tick(&[]);
        rig.run("rf1", true);
        let outcome = rig.tick(&[("rfpro", "rfpro"), ("rf1", "rf1")]);
        assert_eq!(
            outcome.events,
            [game("rfpro"), game("rf1"), DispatchEvent::TelemetryReceived("rf1".into()), DispatchEvent::ConfigRefresh]
        );
        assert_eq!(rig.dispatcher.listener_id(), Some("rf1"));
        assert_eq!(rig.dispatcher.game_id(), Some("rf1"));
        assert_eq!(rig.dispatcher.wind_steering_angle(), Some(30));
        assert_eq!(rig.state("rf1").receive_calls, 1);
        assert!(!rig.dispatcher.is_scanning());
    }

    /// Scan: a found game without data keeps the last found key as the running game; api/joy are not tried.
    #[test]
    fn found_game_without_data() {
        let mut rig = Rig::new(&["dirt", "api", "joy"]);
        rig.tick(&[]);
        rig.run("api", true);
        let outcome = rig.tick(&[("dirtrally2.0", "dirt")]);
        assert_eq!(outcome.events, [game("dirtrally2.0")]);
        assert_eq!(rig.dispatcher.game_id(), Some("dirtrally2.0"));
        assert_eq!(rig.dispatcher.listener_id(), None);
        assert_eq!(rig.tick(&[("dirtrally2.0", "dirt")]).events, []);
    }

    /// Scan: API fallback (published on every selection), then the joystick, else none.
    #[test]
    fn api_and_joystick_fallbacks() {
        let mut rig = Rig::new(&["api", "joy"]);
        rig.run("joy", true);
        let outcome = rig.tick(&[]);
        assert_eq!(outcome.events, [game("joy"), DispatchEvent::ConfigRefresh]);
        assert_eq!(rig.dispatcher.listener_id(), Some("joy"));
        assert_eq!(rig.dispatcher.wind_steering_angle(), Some(20));
        rig.run("joy", false);
        rig.run("api", true);
        let outcome = rig.tick(&[]);
        assert_eq!(outcome.events, [DispatchEvent::ConfigRefresh]);
        let outcome = rig.tick(&[]);
        assert_eq!(outcome.events, [game("api"), DispatchEvent::ConfigRefresh]);
        assert_eq!(rig.dispatcher.game_id(), Some("api"));
    }

    /// Sticky branch: hits are ignored while the listener delivers; losing it resets to the off values.
    #[test]
    fn sticky_listener_and_standby_reset() {
        let mut rig = Rig::new(&["dirt", "f1", "api", "joy"]);
        rig.run("dirt", true);
        rig.state("dirt").vehicle = Some("car".into());
        rig.set("dirt", "speed", 30.0);
        rig.set("dirt", "rpm", 5000.0);
        rig.tick(&[]);
        rig.tick(&[("dirt2", "dirt")]);
        rig.run("f1", true);
        assert_eq!(rig.tick(&[("f12017", "f1")]).events, []);
        assert_eq!(rig.dispatcher.listener_id(), Some("dirt"));
        assert_eq!(rig.dispatcher.telemetry().rpm, 5000.0);
        assert_eq!(rig.dispatcher.vehicle(), Some("car"));

        rig.run("dirt", false);
        let outcome = rig.tick(&[("f12017", "f1")]);
        assert_eq!(outcome.events, [DispatchEvent::ConfigRefresh]);
        assert_eq!(rig.dispatcher.listener_id(), None);
        assert_eq!(rig.dispatcher.game_id(), Some("dirt2"));
        assert_eq!(rig.dispatcher.vehicle(), None);
        let mut off = Telemetry::off();
        off.game = "dirt2".into();
        assert_eq!(rig.dispatcher.telemetry(), &off);
        assert!(rig.dispatcher.is_scanning());
    }

    /// Channel processing: speed truncation/clamp, loader fallbacks and rounding, gear, shift light, km/h.
    #[test]
    fn processed_tick_derivations() {
        let mut rig = Rig::new(&["dirt", "api", "joy"]);
        rig.run("dirt", true);
        rig.set("dirt", "speed", 12.9);
        rig.set("dirt", "gear", -1.0);
        rig.set("dirt", "rpm", 9600.9);
        rig.set("dirt", "max_rpm", 10000.0);
        rig.set("dirt", "steering_wheel_angle", 0.125);
        rig.set("dirt", "lateral_velocity", 2.0);
        rig.runtime.cannon_firing = Some(LegacyValue::Int(1));
        rig.tick(&[("dirt2", "dirt")]);
        let t = rig.dispatcher.telemetry().clone();
        assert_eq!((t.speed, t.speed_kmh), (12.0, 12.0 * 3.6));
        assert_eq!((t.max_speed, t.max_speed_kmh), (56.0, 56.0 * 3.6));
        assert_eq!((t.gear, t.harmonized_gear), (-1.0, 12.0));
        assert_eq!(t.steering_wheel_angle, 0.12);
        assert_eq!(t.steering_wheel_max_angle, 0.0);
        assert_eq!((t.raw_flag, t.rumble_pitch_lf, t.flag), (-1.0, 0.0, 0.0));
        assert_eq!((t.front_tl, t.lateral_velocity), (2.0, 2.0));
        assert_eq!((t.cannon_firing, t.missile_firing), (1.0, 0.0));
        assert!(!rig.dispatcher.shift_light());

        rig.set("dirt", "rpm", 9601.0);
        rig.set("dirt", "speed", -3.7);
        rig.state("dirt").channels.remove("max_rpm");
        rig.tick(&[]);
        let t = rig.dispatcher.telemetry();
        assert_eq!((t.speed, t.max_rpm), (0.0, 10000.0));
        assert!(rig.dispatcher.shift_light());

        rig.state("dirt").channels.remove("speed");
        rig.set("dirt", "gear", 3.0);
        rig.tick(&[]);
        assert_eq!(rig.dispatcher.telemetry().speed, 0.0);
        assert_eq!(rig.dispatcher.telemetry().harmonized_gear, 12.0);
        assert!(rig.dispatcher.has_ended(), "missing speed raises before the next gear assignment");
    }

    /// Yaw: flying games take the yaw rate as yaw; others normalise the heading.
    #[test]
    fn flying_yaw_by_catalog_type() {
        let mut rig = Rig::new(&["xplane", "api", "joy"]);
        rig.run("xplane", true);
        rig.set("xplane", "yaw", -90.0);
        rig.state("xplane").rates.insert("yaw_rate", 4.0);
        rig.tick(&[("xplane", "xplane")]);
        assert_eq!((rig.dispatcher.telemetry().yaw, rig.dispatcher.telemetry().yaw_angle), (4.0, -90.0));

        let mut ground = Rig::new(&["dirt", "api", "joy"]);
        ground.run("dirt", true);
        ground.set("dirt", "yaw", -90.0);
        ground.tick(&[("dirt2", "dirt")]);
        assert_eq!((ground.dispatcher.telemetry().yaw, ground.dispatcher.telemetry().yaw_angle), (-90.0, 270.0));
    }

    /// Min/max and transitions: min/max before the km/h update, cleared when the game closes;
    /// the previous game records the closed game.
    #[test]
    fn extremes_and_previous_game() {
        let mut rig = Rig::new(&["dirt", "api", "joy"]);
        rig.run("dirt", true);
        rig.set("dirt", "speed", 10.0);
        rig.tick(&[("dirt2", "dirt")]);
        assert_eq!(rig.dispatcher.extremes().get("speed_kmh"), None, "not assigned until after first min/max pass");
        rig.tick(&[]);
        assert_eq!(rig.dispatcher.extremes().get("speed_kmh").map(|e| e.max), Some(36.0));
        assert_eq!(rig.dispatcher.previous_game_id(), Some("dirt2"));
        assert_eq!(
            rig.dispatcher.export_rows().iter().find(|r| r.label == "Speed").map(|r| r.value.as_str()),
            Some("36.000000")
        );

        rig.run("dirt", false);
        rig.tick(&[]);
        let outcome = rig.tick(&[]);
        assert_eq!(outcome.events, [DispatchEvent::MinMaxReset]);
        assert_eq!(rig.dispatcher.extremes().get("speed"), None);
        assert_eq!(rig.dispatcher.previous_game_id(), Some("dirt2"));
    }

    /// Chart history keeps at most 501 samples.
    #[test]
    fn chart_history_limit() {
        let mut rig = Rig::new(&["dirt", "api", "joy"]);
        rig.run("dirt", true);
        rig.runtime.plot_channel = Some("rpm".into());
        for rpm in 0..600 {
            rig.set("dirt", "rpm", f64::from(rpm));
            rig.tick(&[("dirt2", "dirt")]);
        }
        let history = rig.dispatcher.history();
        assert_eq!(history.len(), 501);
        assert_eq!(
            (history.front(), history.back()),
            (Some(&LegacyValue::Float(99.0)), Some(&LegacyValue::Float(599.0)))
        );
    }

    /// Intellitune: captured only while enabled and a factory reference exists.
    #[test]
    fn intellitune_capture() {
        let mut rig = Rig::new(&["dirt", "api", "joy"]);
        rig.tick(&[]);
        rig.run("dirt", true);
        rig.set("dirt", "lateral_acceleration", -2.0);
        rig.tick(&[("dirtrally2.0", "dirt")]);
        assert_eq!(rig.dispatcher.intellitune().samples("lateral_acceleration").unwrap(), &[] as &[f64]);
        rig.runtime.intellitune_capture = true;
        rig.tick(&[]);
        assert_eq!(rig.dispatcher.intellitune().samples("lateral_acceleration").unwrap(), &[2.0]);
        assert_eq!(rig.dispatcher.take_intellitune().samples("pitch").unwrap(), &[0.0]);
        assert!(rig.dispatcher.intellitune().samples("pitch").unwrap().is_empty());
    }

    /// Test pattern: the pattern runs while no listener delivers, resets the off values once when one starts,
    /// then leaves the telemetry alone.
    #[test]
    fn test_pattern_only_when_idle() {
        let mut rig = Rig::new(&["dirt", "api", "joy"]);
        rig.tick(&[]);
        rig.dispatcher.apply_test_pattern(false, true);
        assert_eq!(rig.dispatcher.telemetry().rpm, 11000.0);
        rig.run("dirt", true);
        rig.set("dirt", "rpm", 3000.0);
        rig.tick(&[("dirt2", "dirt")]);
        rig.dispatcher.apply_test_pattern(false, true);
        assert_eq!((rig.dispatcher.telemetry().rpm, rig.dispatcher.telemetry().max_rpm), (0.0, 20000.0));
        rig.tick(&[]);
        rig.dispatcher.apply_test_pattern(false, true);
        assert_eq!(rig.dispatcher.telemetry().rpm, 3000.0);
    }

    /// The show-all display flag is latched only on listener
    /// changes and is never undone, and the export of the same tick runs before it.
    #[test]
    fn show_all_channels_is_latched_on_config_refresh() {
        let displayed = CHANNEL_SPECS.iter().filter(|spec| spec.display).count();
        let all = CHANNEL_SPECS.len();
        assert!(displayed < all);

        let mut rig = Rig::new(&["dirt", "api", "joy"]);
        rig.run("dirt", true);
        rig.settings.show_all_channels = true;
        rig.tick(&[("dirt2", "dirt")]);
        assert_eq!(rig.dispatcher.export_rows().len(), displayed);
        assert!(rig.dispatcher.shows_all_channels());
        rig.tick(&[]);
        assert_eq!(rig.dispatcher.export_rows().len(), all);
        rig.settings.show_all_channels = false;
        rig.tick(&[]);
        assert_eq!(rig.dispatcher.export_rows().len(), all);

        let mut later = Rig::new(&["dirt", "api", "joy"]);
        later.run("dirt", true);
        later.tick(&[("dirt2", "dirt")]);
        later.settings.show_all_channels = true;
        later.tick(&[]);
        assert_eq!(later.dispatcher.export_rows().len(), displayed);
        assert!(!later.dispatcher.shows_all_channels());
        later.run("dirt", false);
        later.tick(&[]);
        assert!(later.dispatcher.shows_all_channels());
    }
}
