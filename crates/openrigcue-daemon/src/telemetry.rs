//! The telemetry threads: the main loop, the window/process snapshot
//! and the LED/tachometer test pattern (the emulator).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

use openrigcue_config::catalog::{Catalog, GameKind};
use openrigcue_config::detect::{
    running_games, Snapshotter, SystemSnapshot, WindowRule, WindowTitles, SNAPSHOT_PERIOD,
};
use openrigcue_config::settings::{MeasurementUnit, Settings};
use openrigcue_config::tunes::{AxisSource, FactoryTunes, Tune};
use openrigcue_model::native_value::LegacyValue;
use openrigcue_sources::derive::{
    AttitudeSource, DamsMode, DamsSettings, DispatchEvent, DispatchPublication, DispatchSettings, Dispatcher,
    GameDirectory, IntellituneReference, ListenerActivity, ProfileSettings, RefreshRates, RuntimeInputs, ScanHit,
    SpeedUnit, TestPattern, TickInput, TEST_PATTERN_PERIOD,
};

use crate::listeners::{SourceContext, SourceOutputs};
use crate::runtime::{Controls, OutputMode, SessionView, Shared};
use crate::state::{ServiceEvent, ServiceState};

/// The dispatcher, shared by the main loop and the API (Telemetry page).
/// The emulator writes the published telemetry independently.
#[derive(Clone)]
pub struct TelemetryHandle {
    dispatcher: Arc<Mutex<Dispatcher>>,
    activity: ListenerActivity,
    export_rows: Shared<Vec<openrigcue_sources::derive::ExportRow>>,
    history: openrigcue_sources::derive::TelemetryHistory,
    extremes: openrigcue_sources::derive::MinMaxHandle,
    intellitune: openrigcue_sources::derive::IntellituneHandle,
    show_all_channels: Shared<bool>,
    publication: Shared<Option<Arc<LivePublication>>>,
}

impl TelemetryHandle {
    pub fn for_settings(dispatcher: Dispatcher, settings: &Settings) -> TelemetryHandle {
        Self::new(dispatcher.with_initial_display_all(settings.app.show_all_telemetry_channels))
    }

    pub fn new(dispatcher: Dispatcher) -> TelemetryHandle {
        TelemetryHandle {
            export_rows: Shared::new(dispatcher.export_rows().to_vec()),
            history: dispatcher.history_handle(),
            extremes: dispatcher.extremes_handle(),
            intellitune: dispatcher.intellitune_handle(),
            show_all_channels: Shared::new(dispatcher.shows_all_channels()),
            publication: Shared::new(None),
            activity: dispatcher.listener_activity(),
            dispatcher: Arc::new(Mutex::new(dispatcher)),
        }
    }

    pub fn lock(&self) -> MutexGuard<'_, Dispatcher> {
        self.dispatcher.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The last completed export table remains readable during the next status/receive/getter pass.
    pub fn export_rows_snapshot(&self) -> Vec<openrigcue_sources::derive::ExportRow> {
        self.export_rows.snapshot()
    }

    pub fn history_snapshot(&self) -> std::collections::VecDeque<LegacyValue> {
        self.history.snapshot()
    }

    pub fn clear_history(&self) {
        self.history.clear();
    }

    pub fn extremes_snapshot(&self) -> openrigcue_sources::derive::MinMaxTable {
        self.extremes.snapshot()
    }

    pub fn reset_extremes(&self) {
        self.extremes.clear();
    }

    pub fn shows_all_channels(&self) -> bool {
        *self.show_all_channels.read()
    }

    pub fn intellitune_snapshot(&self) -> openrigcue_sources::derive::IntellituneSamples {
        self.intellitune.snapshot()
    }

    pub fn take_intellitune(&self) -> openrigcue_sources::derive::IntellituneSamples {
        self.intellitune.take()
    }

    /// UI/import assignments use the same shared target as the dispatcher, without waiting for it.
    pub fn publish_vehicle_value(&self, state: &ServiceState, value: LegacyValue, text: Option<String>) {
        if let Some(publication) = self.publication.snapshot() {
            publication.vehicle(&value, text.as_deref());
        } else {
            state.session.write().set_vehicle_value(value, text.clone());
            state.telemetry.write().vehicle = text.unwrap_or_default();
        }
        let current = state.session.read().vehicle_value();
        let mut dispatcher = match self.dispatcher.try_lock() {
            Ok(dispatcher) => dispatcher,
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return,
        };
        dispatcher.synchronize_vehicle_value(current);
    }

    pub fn publish_game_value(&self, state: &ServiceState, game: Option<String>) {
        if let Some(publication) = self.publication.snapshot() {
            publication.game_running(game.as_deref());
        } else {
            state.session.write().game = game.clone();
            state.telemetry.write().game = game.clone().unwrap_or_default();
            openrigcue_sources::udp_racing::parameters::Parameters::shared()
                .set_game_running(game.as_deref().unwrap_or("none"));
        }
    }
}

struct LivePublication {
    telemetry: Shared<openrigcue_model::Telemetry>,
    ready: Shared<bool>,
    session: Shared<SessionView>,
    export_rows: Shared<Vec<openrigcue_sources::derive::ExportRow>>,
    controls: Shared<Controls>,
    profile: Shared<crate::profile::ActiveProfile>,
    show_all_channels: Shared<bool>,
    config: Shared<crate::store::ConfigStore>,
    context: Shared<SourceContext>,
    system_snapshot: Shared<SystemSnapshot>,
}

impl DispatchPublication for LivePublication {
    fn scan_hits(&self) -> Option<Box<dyn Iterator<Item = Result<ScanHit, String>> + Send>> {
        let config = self.config.clone();
        let snapshot = self.system_snapshot.clone();
        Some(Box::new(Catalog::builtin().games().iter().filter_map(move |game| {
            let raw = config.read().native_settings_tree();
            let enabled = (|| {
                let raw = openrigcue_cueing::PyValue::from(&raw);
                let features = raw.item_owned("CUSTOM_FEATURES")?;
                openrigcue_cueing::str2bool(&features.item_owned("disable_rfpro")?)
            })();
            let disabled = match enabled {
                Ok(disabled) => disabled,
                Err(error) => return Some(Err(format!("{}: {}", error.exception().name(), error.message()))),
            };
            if disabled && game.id == "rfpro" {
                return None;
            }
            let by_window = WindowRule::of(game)
                .is_some_and(|rule| snapshot.read().windows.as_ref().is_some_and(|windows| rule.matches_any(windows)));
            let found = by_window
                || game.process_name.as_ref().is_some_and(|name| snapshot.read().processes.contains_key(name));
            found.then(|| Ok(ScanHit { game_id: game.id.clone(), listener_id: game.listener.clone() }))
        })))
    }

    fn reset_min_max(&self) {
        self.controls.write().telemetry.reset_min_max = true;
    }

    fn assigned(&self, name: &'static str, source: &openrigcue_model::Telemetry) {
        {
            let mut telemetry = self.telemetry.write();
            if let Some(value) = source.native_value(name) {
                telemetry.set_native(name, value.clone());
            } else {
                *telemetry.get_mut(name).expect("dispatcher registry field") =
                    source.get(name).expect("registry field");
                telemetry.assigned(name);
            }
        }
        if name == "spotter_right" {
            *self.ready.write() = true;
        }
    }

    fn snapshot(&self) -> Option<openrigcue_model::Telemetry> {
        Some(self.telemetry.snapshot())
    }

    fn game_running(&self, game: Option<&str>) {
        self.session.write().game = game.map(str::to_owned);
        self.context.write().game_running = game.map(str::to_owned);
        self.telemetry.write().game = game.unwrap_or_default().to_owned();
        openrigcue_sources::udp_racing::parameters::Parameters::shared().set_game_running(game.unwrap_or("none"));
    }

    fn game_snapshot(&self) -> Option<Option<String>> {
        Some(self.session.read().game.clone())
    }

    fn listener_running(&self, listener: Option<&str>) {
        self.session.write().listener = listener.map(str::to_owned);
        self.context.write().listener_running = listener.map(str::to_owned);
        openrigcue_sources::udp_racing::parameters::Parameters::shared()
            .set_listener_running(listener.unwrap_or("none"));
    }

    fn telemetry_received(&self, listener: &str) -> Option<Result<(), String>> {
        Some(
            self.config
                .write()
                .record_notification_marker("notifications", listener)
                .map_err(|error| error.to_string()),
        )
    }

    fn vehicle(&self, value: &LegacyValue, text: Option<&str>) {
        self.session.write().set_vehicle_value(value.clone(), text.map(str::to_owned));
        self.telemetry.write().vehicle = text.unwrap_or_default().to_owned();
    }

    fn location(&self, value: &LegacyValue, text: Option<&str>) {
        self.session.write().set_location_value(value.clone(), text.map(str::to_owned));
        self.telemetry.write().location = text.unwrap_or_default().to_owned();
    }

    fn shift_light(&self, value: bool) {
        self.session.write().shift_light = value;
    }

    fn export_rows(&self, rows: &[openrigcue_sources::derive::ExportRow]) {
        *self.export_rows.write() = rows.to_vec();
    }

    fn show_all_channels(&self, value: bool) {
        *self.show_all_channels.write() = value;
    }

    fn runtime_inputs(&self) -> Option<RuntimeInputs> {
        Some(runtime_inputs(&self.controls.read()))
    }

    fn profile_settings(&self) -> Option<ProfileSettings> {
        let active = self.profile.read();
        let mut profile = profile_settings(&active.tune);
        profile.native = Some(active.native_telemetry_tune());
        Some(profile)
    }

    fn dispatch_settings(&self) -> Option<DispatchSettings> {
        let config = self.config.read();
        Some(dispatch_settings_native(config.settings(), &config.native_settings_tree()))
    }
}

/// Catalog facts the dispatcher asks for.
pub struct CatalogDirectory {
    catalog: &'static Catalog,
    factory: &'static FactoryTunes,
}

impl Default for CatalogDirectory {
    fn default() -> Self {
        CatalogDirectory { catalog: Catalog::builtin(), factory: FactoryTunes::builtin() }
    }
}

impl GameDirectory for CatalogDirectory {
    /// Whether the game's catalog type is `flying`.
    fn is_flying(&self, game_id: &str) -> bool {
        self.catalog.game(game_id).is_some_and(|game| game.kind == GameKind::Flying)
    }

    /// The game's factory `max_telemetry_vars`.
    fn intellitune_reference(&self, game_id: &str) -> Option<IntellituneReference> {
        let ranges = &self.factory.get(game_id)?.max_telemetry_vars;
        Some(IntellituneReference {
            sway: ranges.sway,
            heave: ranges.heave,
            surge_acceleration: ranges.surge_acceleration,
            yaw: ranges.yaw,
            roll: ranges.roll,
            pitch: ranges.pitch,
            traction_loss: ranges.traction_loss,
            native: Some(
                openrigcue_config::import::shipped::ShippedData::get()
                    .master_tunes
                    .get(game_id)
                    .and_then(|value| value.as_dict())
                    .and_then(|value| value.get("max_telemetry_vars"))
                    .cloned()
                    .ok_or_else(|| "'max_telemetry_vars'".to_owned()),
            ),
        })
    }
}

/// The `config.ini` values the main loop reads every tick.
pub fn dispatch_settings(settings: &Settings) -> DispatchSettings {
    dispatch_settings_native(settings, &LegacyValue::Dict(settings.native_snapshot()))
}

fn dispatch_settings_native(settings: &Settings, native: &LegacyValue) -> DispatchSettings {
    let field = |key| native.item_owned("CUSTOM_FEATURES").and_then(|features| features.item_owned(key)).ok();
    let number = |key| field(key).as_ref().and_then(native_float);
    DispatchSettings {
        dams: DamsSettings {
            mode: DamsMode::from_config(field("dams_front_tl_mode").as_ref().and_then(native_integer)),
            factor: number("dams_front_tl_factor"),
            front_lever: number("dams_rear_tl_a").unwrap_or(0.0),
            rear_lever: number("dams_rear_tl_b").unwrap_or(0.0),
            steering_ratio: number("dams_front_steerratio"),
        },
        refresh_rates: RefreshRates {
            shakebox: i64::from(settings.shakers.refresh_rate),
            intellibox: i64::from(settings.wind.refresh_rate),
            motionbox: i64::from(settings.motion.refresh_rate),
        },
        native_refresh_rates: Some(
            ["SHAKEBOX", "INTELLIBOX", "MOTIONBOX"]
                .map(|section| native.item_owned(section).and_then(|section| section.item_owned("refresh_rate")).ok()),
        ),
        native_speed_unit: Some((|| {
            let unit = native.item_owned("APP")?.item_owned("measurement_unit")?;
            Ok(if openrigcue_model::python_compare::native_equal_checked(&unit, &LegacyValue::from("K"))? {
                SpeedUnit::Kmh
            } else {
                SpeedUnit::Mph
            })
        })()),
        speed_unit: match settings.app.speed_unit {
            MeasurementUnit::KilometresPerHour => SpeedUnit::Kmh,
            MeasurementUnit::MilesPerHour => SpeedUnit::Mph,
        },
        show_all_channels: settings.app.show_all_telemetry_channels,
    }
}

fn native_integer(value: &LegacyValue) -> Option<i64> {
    let value = value.py_integer().ok()?;
    Some(i64::try_from(value.clone()).unwrap_or_else(|_| if value < 0.into() { i64::MIN } else { i64::MAX }))
}

fn native_float(value: &LegacyValue) -> Option<f64> {
    value.py_float_native().ok()
}

/// The tune values the main loop reads every tick.
pub fn profile_settings(tune: &Tune) -> ProfileSettings {
    let source = |s: Option<AxisSource>| match s {
        None | Some(AxisSource::Angle) => AttitudeSource::Angle,
        Some(AxisSource::Rate) => AttitudeSource::Rate,
    };
    ProfileSettings {
        default_max_speed_kmh: tune.default_max_speed_kmh,
        gear_shift_percent: tune.gear_shift_perc,
        pitch_source: source(tune.motion_effects.pitch_type),
        roll_source: source(tune.motion_effects.roll_type),
        native: None,
    }
}

/// The runtime values the main loop reads every tick; the canned effects are the hotkeys'
/// (`cannon_firing`, `missile_firing`).
pub fn runtime_inputs(controls: &Controls) -> RuntimeInputs {
    RuntimeInputs {
        cannon_firing: controls.canned.cannon_firing.map(|value| LegacyValue::Int(i64::from(value))),
        missile_firing: controls.canned.missile_firing.map(|value| LegacyValue::Int(i64::from(value))),
        plot_channel: controls.telemetry.plot_channel.clone(),
        intellitune_capture: controls.telemetry.intellitune_capture,
    }
}

/// Catalog games found in a snapshot, as the dispatcher's scan reads them.
pub fn scan_hits(snapshot: &SystemSnapshot, disable_rfpro: bool) -> Vec<ScanHit> {
    running_games(Catalog::builtin().games(), snapshot, disable_rfpro)
        .into_iter()
        .map(|game| ScanHit { game_id: game.id.clone(), listener_id: game.listener.clone() })
        .collect()
}

/// A window-title source chosen at run time.
pub struct Titles(pub Box<dyn WindowTitles + Send>);

impl WindowTitles for Titles {
    fn titles(&mut self) -> Vec<String> {
        self.0.titles()
    }
}

/// The window-title source: X11/XWayland.
pub fn system_titles() -> Titles {
    Titles(Box::new(openrigcue_config::detect::X11WindowTitles::default()))
}

/// What the telemetry threads share besides the service state.
#[derive(Clone)]
pub struct TelemetryParts {
    pub dispatcher: TelemetryHandle,
    pub snapshot: Shared<SystemSnapshot>,
    pub context: Shared<SourceContext>,
    pub outputs: Shared<SourceOutputs>,
    /// The selected joystick profile, set by the Joystick page.
    pub joystick_profile: Shared<String>,
}

fn refresh_system_snapshot(
    snapshot: &Shared<SystemSnapshot>,
    take: impl FnOnce(&mut dyn FnMut(&[String])) -> SystemSnapshot,
) {
    let mut publish_windows = |windows: &[String]| {
        snapshot.write().windows = Some(windows.to_vec());
    };
    let scanned = take(&mut publish_windows);
    snapshot.write().processes = scanned.processes;
}

/// One main-loop tick: refresh runtime inputs, publish assignments as the dispatcher runs, then finish
/// bookkeeping and handle its events. Returns the pause before the next tick.
///
/// This is one iteration of the telemetry main loop, together with the reactions of the rest of the
/// application to its events.
pub fn main_loop_tick(state: &ServiceState, parts: &TelemetryParts, games: &dyn GameDirectory) -> std::time::Duration {
    let settings = {
        let config = state.config.read();
        dispatch_settings_native(config.settings(), &config.native_settings_tree())
    };
    let controls = state.controls.snapshot();
    let profile = {
        let active = state.profile.read();
        let mut profile = profile_settings(&active.tune);
        profile.native = Some(active.native_tune());
        profile
    };
    let runtime = runtime_inputs(&controls);
    refresh_context(state, parts, &controls, &profile);
    let mut dispatcher = parts.dispatcher.lock();
    let publication = Arc::new(LivePublication {
        telemetry: state.telemetry.clone(),
        ready: state.telemetry_ready.clone(),
        session: state.session.clone(),
        export_rows: parts.dispatcher.export_rows.clone(),
        controls: state.controls.clone(),
        profile: state.profile.clone(),
        show_all_channels: parts.dispatcher.show_all_channels.clone(),
        config: state.config.clone(),
        context: parts.context.clone(),
        system_snapshot: parts.snapshot.clone(),
    });
    *parts.dispatcher.publication.write() = Some(publication.clone());
    dispatcher.set_publication(publication);
    dispatcher.load_shared_telemetry(state.telemetry.snapshot(), *state.telemetry_ready.read());
    let outcome =
        dispatcher.tick(&TickInput { hits: &[], settings: &settings, profile: &profile, runtime: &runtime, games });
    if dispatcher.has_ended() {
        parts
            .outputs
            .write()
            .dispatcher_ended
            .get_or_insert_with(|| "uncaught telemetry channel processing error".into());
    }
    publish_bookkeeping(state, parts, &dispatcher);
    dispatcher.synchronize_vehicle_value(state.session.read().vehicle_value());
    drop(dispatcher);
    for event in outcome.events {
        handle_event(state, event);
    }
    outcome.sleep
}

/// The runtime values the listeners read during the tick.
fn refresh_context(state: &ServiceState, parts: &TelemetryParts, controls: &Controls, profile: &ProfileSettings) {
    let session = state.session.read().clone();
    let config = state.config.read();
    let settings = config.settings();
    let (pitch_type, roll_type) = (profile.pitch_source(), profile.roll_source());
    let mut context = parts.context.write();
    context.listener_running.clone_from(&session.listener);
    // The `PORT_FORWARDING` section, which UDP forwarding reads on every datagram.
    openrigcue_sources::udp_racing::forward::PortForwarding::shared().set(settings.port_forwarding.clone());
    context.game_running.clone_from(&session.game);
    context.pause_when_not_playing = settings.app.pause_telemetry_when_not_playing;
    context.window_titles.clone_from(&parts.snapshot.read().windows);
    context.dams_front_tl_mode = config
        .native_settings_tree()
        .item_owned("CUSTOM_FEATURES")
        .and_then(|features| features.item_owned("dams_front_tl_mode"))
        .ok()
        .as_ref()
        .and_then(native_integer);
    context.rollback_xplane_lateral_g = settings.features.rollback_xplane_lateral_g;
    context.joystick_enabled = controls.switches.joystick;
    let name = parts.joystick_profile.read().clone();
    let stored = config.config().metadata_table("joystick").get(&name).cloned();
    context.joystick_raw_profile = Some((name, stored));
    context.joystick_options = openrigcue_sources::api::JoystickOptions {
        sway_when_roll: settings.features.sway_when_roll_joystick,
        surge_when_pitch: settings.features.surge_when_pitch_joystick,
        pitch_read_as_angle: pitch_type == AttitudeSource::Angle,
    };
    settings.app.playstation_address.clone_into(&mut context.playstation_ip);
    context.pitch_angle = pitch_type == AttitudeSource::Angle;
    context.roll_angle = roll_type == AttitudeSource::Angle;
}

/// Remaining end-of-tick metadata. Channel and identity assignments already reached shared state.
fn publish_bookkeeping(state: &ServiceState, parts: &TelemetryParts, dispatcher: &Dispatcher) {
    if dispatcher.telemetry_ready() {
        *state.telemetry_ready.write() = true;
    }
    let outputs = parts.outputs.read();
    let mut session = state.session.write();
    let parameters = openrigcue_sources::udp_racing::parameters::Parameters::shared();
    session.previous_game = dispatcher.previous_game_id().map(str::to_owned);
    session.wind_steering_angle = dispatcher.wind_steering_angle();
    session.show_all_channels = dispatcher.shows_all_channels();
    session.fan_override = outputs.fan_override;
    drop(outputs);
    // The game errors, the one set every listener adds to; the UI main loop empties it while no game
    // runs.
    session.game_errors = parameters.game_errors();
}

fn handle_event(state: &ServiceState, event: DispatchEvent) {
    match event {
        DispatchEvent::GameRunning(game) => {
            tracing::info!(game, "game running");
            state.announce(ServiceEvent::GameRunning { game });
        }
        DispatchEvent::TelemetryReceived(listener) => {
            if let Err(error) = state.config.write().record_notification_marker("notifications", &listener) {
                tracing::warn!(%error, "cannot store the telemetry marker");
            }
        }
        DispatchEvent::ConfigRefresh => tracing::debug!("loading config file"),
        DispatchEvent::MinMaxReset => state.announce(ServiceEvent::MinMaxReset),
    }
}

/// The running telemetry threads.
pub struct TelemetryThreads {
    stop: Arc<AtomicBool>,
    handles: Vec<JoinHandle<()>>,
}

impl TelemetryThreads {
    /// Starts the main loop, the emulator and the process snapshot threads, in that order.
    pub fn start(
        state: ServiceState,
        parts: TelemetryParts,
        titles: Titles,
        proc_root: std::path::PathBuf,
    ) -> std::io::Result<TelemetryThreads> {
        let stop = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::new();
        {
            let (state, parts, stop) = (state.clone(), parts.clone(), Arc::clone(&stop));
            handles.push(spawn("MainLoopThread", move || {
                let games = CatalogDirectory::default();
                while !stop.load(Ordering::SeqCst) {
                    let pause = main_loop_tick(&state, &parts, &games);
                    if let Some(error) = parts.outputs.read().dispatcher_ended.clone() {
                        // The error leaves the main loop: the thread ends with it.
                        tracing::error!("Exception in thread MainLoopThread: {error}");
                        return;
                    }
                    std::thread::sleep(pause);
                }
            })?);
        }
        {
            let (state, parts, stop) = (state.clone(), parts.clone(), Arc::clone(&stop));
            handles.push(spawn("emulator", move || {
                let mut pattern = TestPattern::default();
                while !stop.load(Ordering::SeqCst) {
                    test_pattern_tick(&state, &parts, &mut pattern);
                    std::thread::sleep(TEST_PATTERN_PERIOD);
                }
            })?);
        }
        {
            let (state, parts, stop) = (state, parts, Arc::clone(&stop));
            handles.push(spawn("reload_processes", move || {
                let mut snapshotter = Snapshotter::new(titles, proc_root);
                while !stop.load(Ordering::SeqCst) {
                    let forced = state.controls.read().forced_game.clone();
                    refresh_system_snapshot(&parts.snapshot, |publish_windows| {
                        snapshotter.take_with_windows(forced.as_deref(), Catalog::builtin().games(), publish_windows)
                    });
                    std::thread::sleep(SNAPSHOT_PERIOD);
                }
            })?);
        }
        Ok(TelemetryThreads { stop, handles })
    }

    /// Ends every loop after its current pass and waits for it.
    pub fn stop(self) {
        self.stop.store(true, Ordering::SeqCst);
        for handle in self.handles {
            let _ = handle.join();
        }
    }
}

/// One pass of the emulator thread: the LED/tachometer pattern while their switch is "Always On".
fn test_pattern_tick(state: &ServiceState, parts: &TelemetryParts, pattern: &mut TestPattern) {
    test_pattern_tick_with(state, parts, pattern, |_| {});
}

fn test_pattern_tick_with(
    state: &ServiceState,
    parts: &TelemetryParts,
    pattern: &mut TestPattern,
    mut after_assignment: impl FnMut(&'static str),
) {
    if pattern.apply_live_with(
        || parts.dispatcher.activity.test_pattern_allowed(),
        || state.controls.read().switches.led == OutputMode::AlwaysOn,
        || state.controls.read().switches.tach == OutputMode::AlwaysOn,
        |name, value| {
            state.telemetry.write().set_native(name, value);
            after_assignment(name);
        },
    ) {
        *state.telemetry_ready.write() = true;
    }
}

fn spawn(name: &str, body: impl FnOnce() + Send + 'static) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new().name(name.to_owned()).spawn(body)
}

#[cfg(test)]
mod dams_tests;
#[cfg(test)]
mod pacing_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::OriginalPlatform;
    use crate::store::ConfigStore;
    use openrigcue_config::settings::Configuration;
    use openrigcue_model::Telemetry;
    use openrigcue_sources::derive::{Listener, Rate};

    struct Scripted {
        id: &'static str,
        running: bool,
        bad_native: bool,
    }

    impl Listener for Scripted {
        fn id(&self) -> &str {
            self.id
        }
        fn is_running(&mut self) -> bool {
            self.running
        }
        fn receive(&mut self) {}
        fn channel(&self, name: &str) -> Option<f64> {
            match name {
                "speed" => Some(30.0),
                "rpm" => Some(5000.0),
                "max_rpm" => Some(8000.0),
                "gear" => Some(3.0),
                _ => Some(0.0),
            }
        }
        fn native_channel(&self, name: &str) -> Option<openrigcue_model::native_value::LegacyValue> {
            (self.bad_native && name == "pitch").then(|| openrigcue_model::native_value::LegacyValue::Str("bad".into()))
        }
        fn rate(&self, _rate: Rate) -> Option<f64> {
            Some(0.0)
        }
        fn vehicle(&self) -> Option<String> {
            Some("Car".into())
        }
        fn location(&self) -> Option<String> {
            Some("Track".into())
        }
    }

    fn parts(listeners: Vec<Box<dyn Listener>>) -> TelemetryParts {
        TelemetryParts {
            dispatcher: TelemetryHandle::new(Dispatcher::new(listeners)),
            snapshot: Shared::default(),
            context: Shared::default(),
            outputs: Shared::default(),
            joystick_profile: Shared::new("Default".to_owned()),
        }
    }

    fn state() -> ServiceState {
        ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows)
    }

    struct ReceiptProbe {
        first_getter: Arc<std::sync::atomic::AtomicUsize>,
        gate: Option<(std::sync::mpsc::Sender<()>, Mutex<std::sync::mpsc::Receiver<()>>)>,
    }

    impl Listener for ReceiptProbe {
        fn id(&self) -> &str {
            "ac"
        }
        fn is_running(&mut self) -> bool {
            true
        }
        fn receive(&mut self) {}
        fn channel(&self, name: &str) -> Option<f64> {
            Scripted { id: "ac", running: true, bad_native: false }.channel(name)
        }
        fn rate(&self, _rate: Rate) -> Option<f64> {
            Some(0.0)
        }
        fn vehicle(&self) -> Option<String> {
            if self.first_getter.fetch_add(1, Ordering::SeqCst) == 0 {
                if let Some((entered, release)) = &self.gate {
                    entered.send(()).unwrap();
                    release.lock().unwrap().recv_timeout(std::time::Duration::from_secs(5)).unwrap();
                }
            }
            Some("Car".into())
        }
        fn location(&self) -> Option<String> {
            Some("Track".into())
        }
    }

    fn detected_receipt_probe(probe: ReceiptProbe) -> TelemetryParts {
        let parts = parts(vec![Box::new(probe)]);
        parts
            .snapshot
            .write()
            .processes
            .insert(Catalog::builtin().game("acc").unwrap().process_name.clone().unwrap(), 1);
        parts
    }

    #[test]
    fn receipt_marker_is_visible_before_the_first_channel_getter() {
        use std::sync::mpsc;
        use std::time::Duration;
        let state = state();
        let (entered, waiting) = mpsc::channel();
        let (release, resume) = mpsc::channel();
        let parts = detected_receipt_probe(ReceiptProbe {
            first_getter: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            gate: Some((entered, Mutex::new(resume))),
        });
        let marker = std::thread::scope(|scope| {
            let worker = scope.spawn(|| main_loop_tick(&state, &parts, &CatalogDirectory::default()));
            waiting.recv_timeout(Duration::from_secs(5)).unwrap();
            let marker = state.config.read().config().dismissals.driver_dismissed("ac");
            release.send(()).unwrap();
            worker.join().unwrap();
            marker
        });
        assert!(marker, "successful receive commits the marker before car_name is read");
    }

    #[test]
    fn receipt_marker_failure_stops_before_channels_and_sticky_receive_does_not_rewrite_it() {
        use openrigcue_config::settings::paths::ConfigDir;
        let temporary = tempfile::tempdir().unwrap();
        let directory = ConfigDir::at(temporary.path().join("openrigcue"));
        let mut store = ConfigStore::load(directory.clone()).unwrap();
        store.save().unwrap();
        store.record_notification_marker("notifications", "initial").unwrap();
        let state = ServiceState::new(store, OriginalPlatform::Windows);
        let getters = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let parts = detected_receipt_probe(ReceiptProbe { first_getter: getters.clone(), gate: None });
        let connection = rusqlite::Connection::open(directory.metadata_file()).unwrap();
        connection.execute_batch("CREATE TRIGGER reject_marker BEFORE INSERT ON notifications WHEN NEW.key='ac' BEGIN SELECT RAISE(ABORT,'receipt fixture failure'); END;").unwrap();
        state.telemetry.write().speed = 123.0;
        main_loop_tick(&state, &parts, &CatalogDirectory::default());
        assert!(parts.dispatcher.lock().has_ended(), "uncaught receipt storage error ends the dispatcher");
        assert_eq!(getters.load(Ordering::SeqCst), 0, "failed marker prevents the first getter");
        assert_eq!(state.telemetry.read().speed, 123.0);
        assert_eq!(state.session.read().listener.as_deref(), Some("ac"));
        assert!(!state.config.read().config().dismissals.driver_dismissed("ac"));

        connection.execute_batch("DROP TRIGGER reject_marker").unwrap();
        let parts = detected_receipt_probe(ReceiptProbe { first_getter: getters.clone(), gate: None });
        main_loop_tick(&state, &parts, &CatalogDirectory::default());
        assert!(state.config.read().config().dismissals.driver_dismissed("ac"));
        assert!(!parts.dispatcher.lock().has_ended());
        connection.execute_batch("CREATE TRIGGER reject_marker BEFORE INSERT ON notifications WHEN NEW.key='ac' BEGIN SELECT RAISE(ABORT,'sticky must not rewrite'); END;").unwrap();
        main_loop_tick(&state, &parts, &CatalogDirectory::default());
        assert!(!parts.dispatcher.lock().has_ended(), "sticky receive does not repeat scan-only marker");
        assert!(getters.load(Ordering::SeqCst) >= 2);
    }

    struct BlockingChannel {
        name: &'static str,
        paused: AtomicBool,
        entered: std::sync::mpsc::Sender<()>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
    }

    impl BlockingChannel {
        fn pause(&self, name: &str) {
            if name == self.name && !self.paused.swap(true, Ordering::Relaxed) {
                self.entered.send(()).unwrap();
                self.release.lock().unwrap().recv_timeout(std::time::Duration::from_secs(5)).unwrap();
            }
        }
    }

    impl Listener for BlockingChannel {
        fn id(&self) -> &str {
            "api"
        }
        fn is_running(&mut self) -> bool {
            true
        }
        fn receive(&mut self) {}
        fn channel(&self, name: &str) -> Option<f64> {
            self.pause(name);
            match name {
                "pitch" => Some(2.0),
                "roll" => Some(3.0),
                "lateral_velocity" => Some(2.0),
                "longitudinal_velocity" => Some(3.0),
                _ => Scripted { id: "api", running: true, bad_native: false }.channel(name),
            }
        }
        fn rate(&self, rate: Rate) -> Option<f64> {
            self.pause(match rate {
                Rate::Pitch => "pitch_rate",
                Rate::Roll => "roll_rate",
                Rate::Yaw => "yaw_rate",
            });
            Some(match rate {
                Rate::Pitch => 5.0,
                Rate::Roll => 6.0,
                Rate::Yaw => 0.0,
            })
        }
        fn vehicle(&self) -> Option<String> {
            Some("Car".into())
        }
        fn location(&self) -> Option<String> {
            Some("Track".into())
        }
    }

    #[test]
    fn completed_writes_are_visible_during_a_later_getter_and_keep_newer_shared_values() {
        use std::sync::mpsc;
        use std::time::Duration;
        for (blocked_channel, prior_rpm, expected_ready) in [("rpm", 123.0, false), ("yaw", 5000.0, true)] {
            let state = state();
            state.telemetry.write().set_native("speed", LegacyValue::Int(999));
            state.telemetry.write().set_native("rpm", LegacyValue::Int(123));
            let (entered, waiting) = mpsc::channel();
            let (release, resume) = mpsc::channel();
            let parts = parts(vec![Box::new(BlockingChannel {
                name: blocked_channel,
                paused: AtomicBool::new(false),
                entered,
                release: Mutex::new(resume),
            })]);
            let (early, session, ready) = std::thread::scope(|scope| {
                let worker = scope.spawn(|| main_loop_tick(&state, &parts, &CatalogDirectory::default()));
                waiting.recv_timeout(Duration::from_secs(5)).unwrap();
                let early = state.telemetry.snapshot();
                let session = state.session.snapshot();
                let ready = *state.telemetry_ready.read();
                state.telemetry.write().set_native("speed", LegacyValue::Int(77));
                release.send(()).unwrap();
                worker.join().unwrap();
                (early, session, ready)
            });
            assert_eq!(early.speed, 30.0, "speed precedes the blocked {blocked_channel} getter");
            assert_eq!(early.rpm, prior_rpm, "RPM assignment relative to {blocked_channel}");
            assert_eq!(session.game.as_deref(), Some("api"));
            assert_eq!(session.listener.as_deref(), Some("api"));
            assert_eq!(session.vehicle_value(), LegacyValue::Str("Car".into()));
            assert_eq!(session.location_value(), LegacyValue::Str("Track".into()));
            assert_eq!(ready, expected_ready, "spotter_right insertion relative to {blocked_channel}");
            let after = state.telemetry.snapshot();
            assert_eq!(after.native_value("speed"), Some(&LegacyValue::Int(77)));
            assert_eq!(after.speed_kmh, 77.0 * 3.6, "later derivation reads the current shared value");
            assert_eq!(after.rpm, 5000.0);
        }
    }

    #[test]
    fn runtime_controls_are_read_when_used_after_a_blocked_getter() {
        use std::sync::mpsc;
        use std::time::Duration;

        struct Games;
        impl GameDirectory for Games {
            fn is_flying(&self, _: &str) -> bool {
                false
            }
            fn intellitune_reference(&self, _: &str) -> Option<IntellituneReference> {
                Some(IntellituneReference {
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

        for (blocked_channel, expected_firing) in [("rpm", 1.0), ("yaw", 0.0)] {
            let state = state();
            let (entered, waiting) = mpsc::channel();
            let (release, resume) = mpsc::channel();
            let parts = parts(vec![Box::new(BlockingChannel {
                name: blocked_channel,
                paused: AtomicBool::new(false),
                entered,
                release: Mutex::new(resume),
            })]);
            std::thread::scope(|scope| {
                let worker = scope.spawn(|| main_loop_tick(&state, &parts, &Games));
                waiting.recv_timeout(Duration::from_secs(5)).unwrap();
                {
                    let mut controls = state.controls.write();
                    controls.canned.cannon_firing = Some(1);
                    controls.canned.missile_firing = Some(1);
                    controls.telemetry.plot_channel = Some("rpm".into());
                    controls.telemetry.intellitune_capture = true;
                }
                release.send(()).unwrap();
                worker.join().unwrap();
            });
            let telemetry = state.telemetry.snapshot();
            assert_eq!(telemetry.cannon_firing, expected_firing, "cannon assignment relative to {blocked_channel}");
            assert_eq!(telemetry.missile_firing, expected_firing, "missile assignment relative to {blocked_channel}");
            let dispatcher = parts.dispatcher.lock();
            assert!(!dispatcher.has_ended());
            assert_eq!(dispatcher.history().back().and_then(LegacyValue::number), Some(5000.0));
            assert_eq!(dispatcher.intellitune().samples("yaw"), Some([0.0].as_slice()));
        }
    }

    #[test]
    fn active_tune_is_read_at_each_derived_value_after_a_blocked_getter() {
        use std::sync::mpsc;
        use std::time::Duration;

        for (blocked_channel, expected_max_speed, expected_pitch) in
            [("lateral_velocity", 100.0, 5.0), ("rpm", 50.0, 5.0), ("yaw", 50.0, 5.0), ("pitch", 50.0, 2.0)]
        {
            let state = state();
            {
                let mut active = state.profile.write();
                active.tune.default_max_speed_kmh = 180.0;
                active.tune.gear_shift_perc = 100.0;
                active.tune.motion_effects.pitch_type = Some(AxisSource::Angle);
                active.tune.motion_effects.roll_type = Some(AxisSource::Angle);
            }
            let (entered, waiting) = mpsc::channel();
            let (release, resume) = mpsc::channel();
            let parts = parts(vec![Box::new(BlockingChannel {
                name: blocked_channel,
                paused: AtomicBool::new(false),
                entered,
                release: Mutex::new(resume),
            })]);
            std::thread::scope(|scope| {
                let worker = scope.spawn(|| main_loop_tick(&state, &parts, &CatalogDirectory::default()));
                waiting.recv_timeout(Duration::from_secs(5)).unwrap();
                {
                    let mut active = state.profile.write();
                    active.tune.default_max_speed_kmh = 360.0;
                    active.tune.gear_shift_perc = 0.0;
                    active.tune.motion_effects.pitch_type = Some(AxisSource::Rate);
                    active.tune.motion_effects.roll_type = Some(AxisSource::Rate);
                }
                release.send(()).unwrap();
                worker.join().unwrap();
            });
            assert!(!parts.dispatcher.lock().has_ended());
            let telemetry = state.telemetry.snapshot();
            assert_eq!(telemetry.max_speed, expected_max_speed, "max-speed lookup relative to {blocked_channel}");
            assert_eq!(telemetry.pitch, expected_pitch, "pitch selector relative to {blocked_channel}");
            assert_eq!(telemetry.roll, 6.0, "roll selector follows pitch getter");
            assert!(state.session.read().shift_light, "shift threshold follows {blocked_channel}");
        }
    }

    #[test]
    fn native_tune_replacement_during_a_getter_keeps_types_defaults_and_failure_order() {
        use openrigcue_config::import::value::Dict;
        use std::sync::mpsc;
        use std::time::Duration;

        let mut native_selectors = Dict::new();
        native_selectors.insert("pitch_type", LegacyValue::None);
        native_selectors.insert("roll_type", LegacyValue::Str("anything but angle".into()));
        for (effects, pitch, roll) in [
            (LegacyValue::Dict(native_selectors), 5.0, 6.0),
            (LegacyValue::Int(5), 2.0, 3.0),
            (LegacyValue::Dict(Dict::new()), 2.0, 3.0),
        ] {
            let state = state();
            let (entered, waiting) = mpsc::channel();
            let (release, resume) = mpsc::channel();
            let parts = parts(vec![Box::new(BlockingChannel {
                name: "rpm",
                paused: AtomicBool::new(false),
                entered,
                release: Mutex::new(resume),
            })]);
            std::thread::scope(|scope| {
                let worker = scope.spawn(|| main_loop_tick(&state, &parts, &CatalogDirectory::default()));
                waiting.recv_timeout(Duration::from_secs(5)).unwrap();
                // The stored tune is an object handle; its resolved dictionary is the copy edited here
                // (top-level keys only) and installed by `replace_native_tune`.
                let mut native = state.profile.read().native_tune().resolved();
                let fields = native.as_dict_mut().unwrap();
                fields.insert("motion_effects", effects);
                fields.insert("gear_shift_perc", LegacyValue::Str("0".into()));
                state.profile.write().replace_native_tune(native);
                release.send(()).unwrap();
                worker.join().unwrap();
            });
            assert!(parts.dispatcher.lock().has_ended(), "text shift percentage retains its arithmetic failure");
            let telemetry = state.telemetry.snapshot();
            assert_eq!((telemetry.pitch, telemetry.roll), (pitch, roll));
            assert_eq!(telemetry.native_value("harmonized_gear"), Some(&LegacyValue::Float(3.0)));
            assert!(!telemetry.channel_names().contains(&"speed_kmh"), "shift failure precedes km/h assignment");
        }
    }

    #[test]
    fn configuration_is_read_at_dams_export_and_pause_stages_after_a_blocked_getter() {
        use std::sync::mpsc;
        use std::time::Duration;

        for (blocked_channel, expected_front) in [("lateral_velocity", 33.690_067_525_979_785), ("rpm", 2.0)] {
            let state = state();
            state.config.write().update(|config| {
                config.settings.features.dams_front_traction_loss_mode = 0;
                config.settings.app.speed_unit = MeasurementUnit::KilometresPerHour;
                config.settings.app.show_all_telemetry_channels = false;
                config.settings.shakers.refresh_rate = 100;
                config.settings.wind.refresh_rate = 100;
                config.settings.motion.refresh_rate = 100;
            });
            let (entered, waiting) = mpsc::channel();
            let (release, resume) = mpsc::channel();
            let parts = parts(vec![Box::new(BlockingChannel {
                name: blocked_channel,
                paused: AtomicBool::new(false),
                entered,
                release: Mutex::new(resume),
            })]);
            let pause = std::thread::scope(|scope| {
                let worker = scope.spawn(|| main_loop_tick(&state, &parts, &CatalogDirectory::default()));
                waiting.recv_timeout(Duration::from_secs(5)).unwrap();
                state.config.write().update(|config| {
                    config.settings.features.dams_front_traction_loss_mode = 1;
                    config.settings.features.dams_front_traction_loss_factor = 1.0;
                    config.settings.app.speed_unit = MeasurementUnit::MilesPerHour;
                    config.settings.app.show_all_telemetry_channels = true;
                    config.settings.shakers.refresh_rate = 50;
                    config.settings.wind.refresh_rate = 50;
                    config.settings.motion.refresh_rate = 50;
                });
                release.send(()).unwrap();
                worker.join().unwrap()
            });
            assert!(!parts.dispatcher.lock().has_ended());
            assert!(
                (state.telemetry.read().front_tl - expected_front).abs() < 1e-12,
                "DAMS mode relative to {blocked_channel}"
            );
            assert_eq!(pause, Duration::from_millis(20), "refresh rates are read at the end of the pass");
            assert!(parts.dispatcher.shows_all_channels(), "display-all latches the current setting");
            let rows = parts.dispatcher.export_rows_snapshot();
            assert_eq!(rows.iter().find(|row| row.label == "Speed").unwrap().value, "67.108200");
        }
    }

    #[test]
    fn dams_keeps_its_selected_front_mode_but_reads_late_factor_and_rear_mode() {
        use std::sync::mpsc;
        use std::time::Duration;
        let state = state();
        state.config.write().update(|config| {
            config.settings.features.dams_front_traction_loss_mode = 2;
            config.settings.features.dams_front_traction_loss_factor = 1.0;
            config.settings.features.dams_front_steering_ratio = 1.0;
        });
        let (entered, waiting) = mpsc::channel();
        let (release, resume) = mpsc::channel();
        let parts = parts(vec![Box::new(BlockingChannel {
            name: "yaw_rate",
            paused: AtomicBool::new(false),
            entered,
            release: Mutex::new(resume),
        })]);
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| main_loop_tick(&state, &parts, &CatalogDirectory::default()));
            waiting.recv_timeout(Duration::from_secs(5)).unwrap();
            state.config.write().update(|config| {
                config.settings.features.dams_front_traction_loss_mode = 0;
                config.settings.features.dams_front_traction_loss_factor = 2.0;
            });
            release.send(()).unwrap();
            worker.join().unwrap();
        });
        assert!(!parts.dispatcher.lock().has_ended());
        let telemetry = state.telemetry.snapshot();
        assert!(
            (telemetry.front_tl - 67.380_135_051_959_57).abs() < 1e-12,
            "front retains mode 2 and uses the later factor"
        );
        assert_eq!(telemetry.lateral_velocity, 2.0, "rear mode is read after the front assignment");
    }

    #[test]
    fn native_identity_values_reach_the_published_session() {
        struct IdentitySource;
        impl openrigcue_sources::Source for IdentitySource {
            fn id(&self) -> &'static str {
                "api"
            }
            fn is_running(&mut self) -> Result<bool, openrigcue_sources::SourceError> {
                Ok(true)
            }
            fn poll(&mut self, t: &mut Telemetry) -> Result<bool, openrigcue_sources::SourceError> {
                t.speed = 30.0;
                t.rpm = 5000.0;
                t.max_rpm = 8000.0;
                t.gear = 3.0;
                Ok(true)
            }
            fn native_channel(&self, name: &str) -> Option<LegacyValue> {
                match name {
                    "car_name" => Some(LegacyValue::None),
                    "track_loc" => Some(LegacyValue::List(vec![LegacyValue::Str("track".into())])),
                    _ => None,
                }
            }
        }
        let state = state();
        let parts = parts(vec![Box::new(openrigcue_sources::derive::SourceListener::new(IdentitySource))]);
        main_loop_tick(&state, &parts, &CatalogDirectory::default());
        let session = state.session.snapshot();
        assert_eq!(session.vehicle_value(), LegacyValue::None);
        assert_eq!(session.location_value(), LegacyValue::List(vec![LegacyValue::Str("track".into())]));
        assert_eq!(session.vehicle.as_deref(), Some("None"));
        assert_eq!(session.location.as_deref(), Some("['track']"));
    }

    #[test]
    fn native_integer_limit_keeps_long_zeroes_from_bypassing_conversion_failure() {
        let within = format!("{}7", "0".repeat(4299));
        assert_eq!(native_integer(&LegacyValue::Str(within.clone())), Some(7));
        assert_eq!(native_integer(&LegacyValue::Bytes(within.clone().into_bytes())), Some(7));
        assert_eq!(native_integer(&LegacyValue::Str(format!("0{within}"))), None);
        assert_eq!(native_integer(&LegacyValue::Bytes(format!("0{within}").into_bytes())), None);
    }

    #[test]
    fn native_channel_failure_is_published_and_stops_the_worker_boundary() {
        let state = state();
        let parts = parts(vec![Box::new(Scripted { id: "api", running: true, bad_native: true })]);
        // Exercise the actual service tick, including shared-frame loading,
        // a channel read failure and publication of its preceding assignments.
        assert_eq!(main_loop_tick(&state, &parts, &CatalogDirectory::default()), std::time::Duration::ZERO);
        assert!(parts.outputs.read().dispatcher_ended.is_some());
        assert!(parts.dispatcher.lock().has_ended());
        assert_eq!(
            state.telemetry.read().native_value("pitch"),
            Some(&openrigcue_model::native_value::LegacyValue::Str("bad".into()))
        );
    }

    #[test]
    fn native_default_speed_failure_reaches_the_production_dispatcher() {
        use openrigcue_model::native_value::LegacyValue;
        let state = state();
        let parts = parts(vec![Box::new(Scripted { id: "api", running: true, bad_native: false })]);
        // The stored tune is an object handle; its resolved dictionary is the copy edited here.
        let mut native = state.profile.read().native_tune().resolved();
        native.as_dict_mut().unwrap().insert("default_max_speed_kmh", LegacyValue::Str("200".into()));
        state.profile.write().replace_native_tune(native);
        assert_eq!(main_loop_tick(&state, &parts, &CatalogDirectory::default()), std::time::Duration::ZERO);
        assert!(parts.dispatcher.lock().has_ended());
        assert!(parts.outputs.read().dispatcher_ended.is_some());
        let telemetry = state.telemetry.read();
        assert_eq!(telemetry.native_value("speed"), Some(&LegacyValue::Int(30)));
        assert!(!telemetry.channel_names().contains(&"rpm"), "the failure precedes generic loads");
        assert!(!*state.telemetry_ready.read());
    }

    /// Main loop: a detected game whose listener delivers becomes the running game and listener, its telemetry
    /// is published, and the delivery marker reaches the notifications table.
    #[test]
    fn detected_game_delivers() {
        let state = state();
        assert!(!*state.telemetry_ready.read());
        let parts = parts(vec![Box::new(Scripted { id: "dirt", running: true, bad_native: false })]);
        let snapshot = SystemSnapshot { windows: Some(vec!["DiRT 4".into()]), processes: Default::default() };
        *parts.snapshot.write() = snapshot;
        let games = CatalogDirectory::default();
        main_loop_tick(&state, &parts, &games);
        let session = state.session.snapshot();
        assert_eq!(session.listener.as_deref(), Some("dirt"));
        assert_eq!(session.vehicle.as_deref(), Some("Car"));
        assert!(state.config.read().config().dismissals.notification_keys.contains("dirt"));
        assert_eq!(state.telemetry.read().speed, 30.0);
        assert!(*state.telemetry_ready.read());
    }

    #[test]
    fn selected_game_reaches_listener_preparation_before_its_first_status_check() {
        use crate::listeners::{GameListener, GameSource};
        struct SelectedSource {
            game: Option<String>,
            context: Shared<SourceContext>,
            checks: Arc<Mutex<Vec<Option<String>>>>,
            received: Arc<Mutex<Vec<Option<String>>>>,
            running: Arc<AtomicBool>,
        }
        impl openrigcue_sources::Source for SelectedSource {
            fn id(&self) -> &'static str {
                "ac"
            }
            fn is_running(&mut self) -> Result<bool, openrigcue_sources::SourceError> {
                Ok(self.running.load(Ordering::Relaxed) && self.game.as_deref() == Some("acc"))
            }
            fn poll(&mut self, telemetry: &mut Telemetry) -> Result<bool, openrigcue_sources::SourceError> {
                self.received.lock().unwrap().push(self.context.read().listener_running.clone());
                *telemetry = Telemetry::off();
                telemetry.speed = 20.0;
                telemetry.rpm = 5000.0;
                telemetry.max_rpm = 8000.0;
                telemetry.gear = 3.0;
                telemetry.vehicle = "Car".into();
                telemetry.location = "Track".into();
                Ok(true)
            }
        }
        impl GameSource for SelectedSource {
            fn prepare(&mut self, context: &SourceContext) {
                self.game.clone_from(&context.game_running);
                self.checks.lock().unwrap().push(self.game.clone());
            }
        }
        let state = state();
        let context = Shared::default();
        let outputs = Shared::default();
        let checks = Arc::new(Mutex::new(Vec::new()));
        let received = Arc::new(Mutex::new(Vec::new()));
        let running = Arc::new(AtomicBool::new(true));
        let source = SelectedSource {
            game: None,
            context: context.clone(),
            checks: checks.clone(),
            received: received.clone(),
            running: running.clone(),
        };
        let listener = GameListener::new(source, context.clone(), outputs.clone());
        let mut parts = parts(vec![Box::new(listener)]);
        parts.context = context;
        parts.outputs = outputs;
        parts
            .snapshot
            .write()
            .processes
            .insert(Catalog::builtin().game("acc").unwrap().process_name.clone().unwrap(), 1);
        main_loop_tick(&state, &parts, &CatalogDirectory::default());
        assert_eq!(*checks.lock().unwrap(), [Some("acc".into())]);
        assert_eq!(*received.lock().unwrap(), [Some("ac".into())]);
        assert_eq!(state.telemetry.read().speed, 20.0, "the first detected tick delivers with the selected game");
        assert_eq!(parts.context.read().game_running, state.session.read().game);
        assert_eq!(parts.context.read().listener_running, state.session.read().listener);
        running.store(false, Ordering::Relaxed);
        main_loop_tick(&state, &parts, &CatalogDirectory::default());
        assert_eq!(parts.context.read().game_running, state.session.read().game);
        assert_eq!(parts.context.read().listener_running, None);
    }

    /// Main-loop pacing: 1 / max(30, SHAKEBOX, INTELLIBOX, MOTIONBOX refresh rates).
    #[test]
    fn pacing_follows_the_refresh_rates() {
        let state = state();
        state.config.write().update(|c| c.settings.motion.refresh_rate = 100);
        let parts = parts(Vec::new());
        let pause = main_loop_tick(&state, &parts, &CatalogDirectory::default());
        assert_eq!(pause, std::time::Duration::from_secs_f64(1.0 / 100.0));
        assert!(*state.telemetry_ready.read(), "the first standby reset fills the original dictionary");
    }

    /// Emulator thread: with the tachometer "Always On" and no listener the pattern is published.
    #[test]
    fn tach_pattern_reaches_the_published_telemetry() {
        let state = state();
        let parts = parts(Vec::new());
        state.controls.write().switches.tach = OutputMode::AlwaysOn;
        let mut pattern = TestPattern::default();
        test_pattern_tick(&state, &parts, &mut pattern);
        let telemetry: Telemetry = state.telemetry.snapshot();
        assert_eq!((telemetry.rpm, telemetry.max_rpm), (11000.0, 10999.0));
        assert!(*state.telemetry_ready.read());
    }

    #[test]
    fn emulator_releases_published_telemetry_between_dictionary_assignments() {
        let state = state();
        let parts = parts(Vec::new());
        state.controls.write().switches.tach = OutputMode::AlwaysOn;
        state.telemetry.write().set_native("vertical_acceleration", LegacyValue::Int(91));
        let mut pattern = TestPattern::default();
        let mut order = Vec::new();
        test_pattern_tick_with(&state, &parts, &mut pattern, |name| {
            order.push(name);
            // Both operations require the writer's per-key lock to be released.
            if name == "speed" {
                assert_eq!(state.telemetry.read().native_value("vertical_acceleration"), Some(&LegacyValue::Int(91)));
                assert!(!*state.telemetry_ready.read(), "readiness follows the complete pass");
            }
            if name == "rpm" {
                state.telemetry.write().set_native("rpm", LegacyValue::Int(4321));
            }
        });
        assert_eq!(order, TestPattern::assigned_channels(true).collect::<Vec<_>>());
        assert_eq!(state.telemetry.read().native_value("rpm"), Some(&LegacyValue::Int(4321)));
        assert_eq!(state.telemetry.read().native_value("vertical_acceleration"), Some(&LegacyValue::Int(0)));
        assert!(*state.telemetry_ready.read());
    }

    #[test]
    fn inactive_pattern_retains_other_writers_values_and_readiness() {
        let state = state();
        let parts = parts(Vec::new());
        let mut pattern = TestPattern::default();
        state.telemetry.write().set_native("vertical_acceleration", LegacyValue::Int(42));
        *state.telemetry_ready.write() = true;
        test_pattern_tick(&state, &parts, &mut pattern);
        let telemetry = state.telemetry.read();
        assert_eq!(telemetry.native_value("vertical_acceleration"), Some(&LegacyValue::Int(42)));
        assert_eq!(telemetry.channel_names(), ["vertical_acceleration"]);
        assert!(*state.telemetry_ready.read());
    }

    #[test]
    fn idle_tick_retains_other_writers_channel_changes() {
        let state = state();
        let parts = parts(Vec::new());
        main_loop_tick(&state, &parts, &CatalogDirectory::default());
        state.telemetry.write().set_native("rpm", LegacyValue::Int(1500));
        main_loop_tick(&state, &parts, &CatalogDirectory::default());
        assert_eq!(state.telemetry.read().native_value("rpm"), Some(&LegacyValue::Int(1500)));
        assert_eq!(parts.dispatcher.lock().telemetry().native_value("rpm"), Some(&LegacyValue::Int(1500)));
    }

    #[test]
    fn pattern_runs_during_blocked_check_and_later_listener_writes_take_effect() {
        use std::sync::mpsc;
        use std::time::Duration;

        struct BlockingStatus {
            armed: bool,
            running: bool,
            block_receive: bool,
            entered: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
        }
        impl Listener for BlockingStatus {
            fn id(&self) -> &str {
                "api"
            }
            fn is_running(&mut self) -> bool {
                if self.armed {
                    if !self.block_receive {
                        self.entered.send(()).unwrap();
                        self.release.recv_timeout(Duration::from_secs(5)).unwrap();
                    }
                    return self.running;
                }
                self.armed = true;
                false
            }
            fn receive(&mut self) {
                if self.block_receive {
                    self.entered.send(()).unwrap();
                    self.release.recv_timeout(Duration::from_secs(5)).unwrap();
                }
            }
            fn channel(&self, name: &str) -> Option<f64> {
                Scripted { id: "api", running: true, bad_native: false }.channel(name)
            }
            fn rate(&self, _: Rate) -> Option<f64> {
                Some(0.0)
            }
            fn vehicle(&self) -> Option<String> {
                None
            }
            fn location(&self) -> Option<String> {
                None
            }
        }
        for (running, block_receive) in [(false, false), (true, false), (true, true)] {
            let state = state();
            let (entered, wait) = mpsc::channel();
            let (release, resume) = mpsc::channel();
            let parts = parts(vec![Box::new(BlockingStatus {
                armed: false,
                running,
                block_receive,
                entered,
                release: resume,
            })]);
            main_loop_tick(&state, &parts, &CatalogDirectory::default());
            state.controls.write().switches.tach = OutputMode::AlwaysOn;
            let mut pattern = TestPattern::default();
            test_pattern_tick(&state, &parts, &mut pattern);
            if running && !block_receive {
                // A later listener write equal to its starting value must still
                // replace the pattern written during its blocked status check.
                state.telemetry.write().set_native("rpm", LegacyValue::Float(5000.0));
            }
            std::thread::scope(|scope| {
                let main_loop = scope.spawn(|| main_loop_tick(&state, &parts, &CatalogDirectory::default()));
                wait.recv_timeout(Duration::from_secs(5)).unwrap();
                let (done, completion) = mpsc::channel();
                let (state, parts, pattern) = (&state, &parts, &mut pattern);
                let emulator = scope.spawn(move || {
                    test_pattern_tick(state, parts, pattern);
                    done.send(()).unwrap();
                });
                let independent = completion.recv_timeout(Duration::from_secs(1));
                let rpm_before_release = state.telemetry.read().rpm;
                release.send(()).unwrap();
                main_loop.join().unwrap();
                emulator.join().unwrap();
                assert!(independent.is_ok(), "emulator waited for the blocked listener check");
                assert_eq!(rpm_before_release, if block_receive { 0.0 } else { 11000.0 });
            });
            assert_eq!(state.telemetry.read().rpm, if running { 5000.0 } else { 11000.0 });
        }
    }

    #[test]
    fn pattern_preserves_identity_and_published_key_order_and_restores_once() {
        let state = state();
        let parts = parts(Vec::new());
        let mut pattern = TestPattern::default();
        // The two snapshots can differ after writes from another service loop.
        // The emulator assigns registry fields in the shared dictionary.
        {
            let mut telemetry = state.telemetry.write();
            telemetry.game = "other-game".into();
            telemetry.vehicle = "other-car".into();
            telemetry.location = "other-track".into();
            telemetry.set_native("pitch", LegacyValue::Int(5));
        }
        state.controls.write().switches.tach = OutputMode::AlwaysOn;
        test_pattern_tick(&state, &parts, &mut pattern);
        {
            let telemetry = state.telemetry.read();
            assert_eq!(
                (&*telemetry.game, &*telemetry.vehicle, &*telemetry.location),
                ("other-game", "other-car", "other-track")
            );
            assert_eq!(
                &telemetry.channel_names()[..9],
                &[
                    "pitch",
                    "speed",
                    "max_speed",
                    "speed_kmh",
                    "max_speed_kmh",
                    "gear",
                    "harmonized_gear",
                    "rpm",
                    "max_rpm"
                ]
            );
        }
        state.controls.write().switches.tach = OutputMode::Off;
        test_pattern_tick(&state, &parts, &mut pattern);
        assert_eq!(state.telemetry.read().native_value("rpm"), Some(&LegacyValue::Int(0)));
        assert_eq!(state.telemetry.read().native_value("max_rpm"), Some(&LegacyValue::Int(20000)));
        state.telemetry.write().set_native("rpm", LegacyValue::Int(1500));
        test_pattern_tick(&state, &parts, &mut pattern);
        assert_eq!(state.telemetry.read().native_value("rpm"), Some(&LegacyValue::Int(1500)));
    }

    /// The game directory: catalog type and factory max_telemetry_vars.
    #[test]
    fn catalog_directory() {
        let games = CatalogDirectory::default();
        assert!(games.is_flying("dcs"));
        assert!(!games.is_flying("dirt4"));
        assert!(!games.is_flying("unknown"));
        let reference = games.intellitune_reference("dirtrally2.0").unwrap();
        assert_eq!(reference.pitch, FactoryTunes::builtin().get("dirtrally2.0").unwrap().max_telemetry_vars.pitch);
        assert!(games.intellitune_reference("unknown").is_none());
    }

    /// Main-loop scan: catalog order decides, `rfpro` shares `rFactor.exe` with `rf1` and is skipped with
    /// `CUSTOM_FEATURES.disable_rfpro`.
    #[test]
    fn scan_hits_in_catalog_order() {
        let snapshot = SystemSnapshot {
            windows: Some(Vec::new()),
            processes: [("rFactor.exe".to_owned(), 7)].into_iter().collect(),
        };
        let ids = |disable| scan_hits(&snapshot, disable).into_iter().map(|h| h.game_id).collect::<Vec<_>>();
        assert_eq!(ids(false).first().map(String::as_str), Some("rfpro"));
        assert!(!ids(true).contains(&"rfpro".to_owned()));
        assert!(ids(true).contains(&"rf1".to_owned()));
    }

    struct ScanScript {
        id: &'static str,
        running: bool,
        checks: Arc<Mutex<Vec<&'static str>>>,
        on_check: Box<dyn FnMut() + Send>,
    }

    impl Listener for ScanScript {
        fn id(&self) -> &str {
            self.id
        }
        fn is_running(&mut self) -> bool {
            self.checks.lock().unwrap().push(self.id);
            (self.on_check)();
            self.running
        }
        fn receive(&mut self) {}
        fn channel(&self, name: &str) -> Option<f64> {
            Scripted { id: self.id, running: self.running, bad_native: false }.channel(name)
        }
        fn rate(&self, rate: Rate) -> Option<f64> {
            Scripted { id: self.id, running: self.running, bad_native: false }.rate(rate)
        }
        fn vehicle(&self) -> Option<String> {
            Some("Car".into())
        }
        fn location(&self) -> Option<String> {
            Some("Track".into())
        }
    }

    #[test]
    fn native_scan_required_feature_failure_precedes_all_candidates_and_fallback() {
        for value in [Some(LegacyValue::Str("maybe".into())), Some(LegacyValue::None), None] {
            let state = state();
            state.config.write().update(|config| {
                config.settings.edit_native(|raw| {
                    let features = raw.get_mut("CUSTOM_FEATURES").unwrap().as_dict_mut().unwrap();
                    match value {
                        Some(value) => {
                            features.insert("disable_rfpro", value);
                        }
                        None => {
                            features.remove("disable_rfpro");
                        }
                    }
                });
            });
            let checks = Arc::new(Mutex::new(Vec::new()));
            let parts = parts(vec![Box::new(ScanScript {
                id: "api",
                running: true,
                checks: checks.clone(),
                on_check: Box::new(|| {}),
            })]);
            assert_eq!(main_loop_tick(&state, &parts, &CatalogDirectory::default()), std::time::Duration::ZERO);
            assert!(parts.dispatcher.lock().has_ended());
            assert!(checks.lock().unwrap().is_empty());
            assert!(state.session.read().game.is_none());
            assert!(parts.outputs.read().dispatcher_ended.is_some());
        }
    }

    #[test]
    fn native_scan_reads_later_windows_and_processes_after_a_failed_listener() {
        for by_window in [false, true] {
            let state = state();
            let snapshot = Shared::new(SystemSnapshot {
                windows: Some(Vec::new()),
                processes: [("acs.exe".into(), 7)].into_iter().collect(),
            });
            let checks = Arc::new(Mutex::new(Vec::new()));
            let changed = snapshot.clone();
            let mut parts = parts(vec![
                Box::new(ScanScript {
                    id: "ac",
                    running: false,
                    checks: checks.clone(),
                    on_check: Box::new(move || {
                        if by_window {
                            changed.write().windows = Some(vec!["Live for Speed".into()]);
                        } else {
                            changed.write().processes.insert("LFS".into(), 8);
                        }
                    }),
                }),
                Box::new(ScanScript { id: "lfs", running: true, checks: checks.clone(), on_check: Box::new(|| {}) }),
            ]);
            parts.snapshot = snapshot;
            main_loop_tick(&state, &parts, &CatalogDirectory::default());
            assert_eq!(*checks.lock().unwrap(), ["ac", "lfs"]);
            assert_eq!(state.session.read().game.as_deref(), Some("lfs"));
            assert_eq!(state.session.read().listener.as_deref(), Some("lfs"));
            assert!(!parts.dispatcher.lock().has_ended());
        }
    }

    #[test]
    fn native_scan_later_feature_failure_keeps_the_first_game_publication() {
        let state = state();
        let config = state.config.clone();
        let checks = Arc::new(Mutex::new(Vec::new()));
        let parts = parts(vec![
            Box::new(ScanScript {
                id: "ac",
                running: false,
                checks: checks.clone(),
                on_check: Box::new(move || {
                    config.write().update(|config| {
                        config.settings.edit_native(|raw| {
                            raw.get_mut("CUSTOM_FEATURES")
                                .unwrap()
                                .as_dict_mut()
                                .unwrap()
                                .insert("disable_rfpro", LegacyValue::Str("maybe".into()));
                        });
                    });
                }),
            }),
            Box::new(ScanScript { id: "lfs", running: true, checks: checks.clone(), on_check: Box::new(|| {}) }),
        ]);
        parts.snapshot.write().processes = [("acs.exe".into(), 7), ("LFS".into(), 8)].into_iter().collect();
        assert_eq!(main_loop_tick(&state, &parts, &CatalogDirectory::default()), std::time::Duration::ZERO);
        assert!(parts.dispatcher.lock().has_ended());
        assert_eq!(*checks.lock().unwrap(), ["ac"]);
        assert_eq!(state.session.read().game.as_deref(), Some("ac"));
        assert!(state.session.read().listener.is_none());
        assert!(!state.config.read().config().dismissals.notification_keys.contains("lfs"));
    }

    #[test]
    fn native_scan_saved_feature_spellings_and_sticky_listener_keep_their_boundaries() {
        for (text, expected) in [("yes", "rf1"), ("Y", "rf1"), ("1", "rf1"), ("no", "rfpro"), ("0", "rfpro")] {
            let state = state();
            let parsed =
                openrigcue_config::import::configobj::parse(&format!("[CUSTOM_FEATURES]\ndisable_rfpro = {text}\n"))
                    .unwrap();
            let value = parsed.get("CUSTOM_FEATURES").unwrap().item("disable_rfpro").unwrap().clone();
            state.config.write().update(|config| {
                config.settings.edit_native(|raw| {
                    raw.get_mut("CUSTOM_FEATURES").unwrap().as_dict_mut().unwrap().insert("disable_rfpro", value);
                });
            });
            let checks = Arc::new(Mutex::new(Vec::new()));
            let parts = parts(
                ["rfpro", "rf1"]
                    .into_iter()
                    .map(|id| {
                        Box::new(ScanScript { id, running: true, checks: checks.clone(), on_check: Box::new(|| {}) })
                            as Box<dyn Listener>
                    })
                    .collect(),
            );
            parts.snapshot.write().processes.insert("rFactor.exe".into(), 3);
            main_loop_tick(&state, &parts, &CatalogDirectory::default());
            assert_eq!(state.session.read().game.as_deref(), Some(expected), "{text}");
            assert_eq!(*checks.lock().unwrap(), [expected]);
            // A delivering listener takes the sticky branch without reading the scan-only flag.
            state.config.write().update(|config| {
                config.settings.edit_native(|raw| {
                    raw.get_mut("CUSTOM_FEATURES")
                        .unwrap()
                        .as_dict_mut()
                        .unwrap()
                        .insert("disable_rfpro", LegacyValue::Str("maybe".into()));
                });
            });
            main_loop_tick(&state, &parts, &CatalogDirectory::default());
            assert!(!parts.dispatcher.lock().has_ended());
            assert_eq!(*checks.lock().unwrap(), [expected, expected]);
        }
    }

    /// The listeners' runtime values: game, pause flag, window titles and the joystick switch reach the context.
    #[test]
    fn listener_context_follows_the_service() {
        let state = state();
        state.session.write().game = Some("acc".into());
        state.controls.write().switches.joystick = true;
        let parts = parts(Vec::new());
        *parts.snapshot.write() =
            SystemSnapshot { windows: Some(vec!["Falcon BMS".into()]), processes: Default::default() };
        let controls = state.controls.snapshot();
        refresh_context(&state, &parts, &controls, &profile_settings(&state.profile.read().tune));
        let context = parts.context.snapshot();
        assert_eq!(context.game_running.as_deref(), Some("acc"));
        assert!(context.pause_when_not_playing);
        assert!(context.joystick_enabled);
        assert_eq!(context.window_titles, Some(vec!["Falcon BMS".to_owned()]));
        assert!(context.joystick_options.pitch_read_as_angle);
    }

    #[test]
    fn scanner_publication_exposes_windows_before_replacing_processes() {
        let snapshot = Shared::new(SystemSnapshot {
            windows: Some(vec!["old".into()]),
            processes: [("old.exe".into(), 1)].into(),
        });
        let read_windows = crate::listeners::windows_reader(snapshot.clone());
        refresh_system_snapshot(&snapshot, |publish| {
            publish(&["new".into()]);
            assert_eq!(read_windows(), Some(vec!["new".into()]));
            assert_eq!(snapshot.read().processes.get("old.exe"), Some(&1));
            SystemSnapshot {
                windows: Some(vec!["unused returned copy".into()]),
                processes: [("new.exe".into(), 2)].into(),
            }
        });
        assert_eq!(snapshot.read().windows, Some(vec!["new".into()]));
        assert_eq!(snapshot.read().processes.get("new.exe"), Some(&2));
        assert!(!snapshot.read().processes.contains_key("old.exe"));
    }

    /// The DAMS settings: `dams_rear_tl_a` is created by no migration, so its lever reads 0.
    #[test]
    fn dams_settings_from_custom_features() {
        let settings = Settings::default();
        let dispatch = dispatch_settings(&settings);
        assert_eq!(dispatch.dams.mode, DamsMode::Off);
        assert_eq!(dispatch.dams.front_lever, 0.0);
        assert_eq!(dispatch.dams.rear_lever, 1.44);
        assert_eq!(dispatch.dams.steering_ratio, Some(14.5));
    }

    #[test]
    fn telemetry_reads_the_live_config_sections_after_an_alias_assignment() {
        let state = state();
        let parts = parts(Vec::new());
        main_loop_tick(&state, &parts, &CatalogDirectory::default());
        let publication = parts.dispatcher.publication.snapshot().unwrap();
        let config = state.config.read().native_settings_tree();
        let mut features = config.item_owned("CUSTOM_FEATURES").unwrap();
        let mut app = config.item_owned("APP").unwrap();
        let mut motion = config.item_owned("MOTIONBOX").unwrap();
        features.assign_item("dams_front_tl_mode", LegacyValue::Int(2)).unwrap();
        features.assign_item("dams_front_tl_factor", LegacyValue::Float(3.5)).unwrap();
        app.assign_item("measurement_unit", LegacyValue::from("M")).unwrap();
        motion.assign_item("refresh_rate", LegacyValue::from("71")).unwrap();

        let dispatch = publication.dispatch_settings().unwrap();
        assert_eq!(dispatch.dams.mode, DamsMode::FrontAndRearSlip);
        assert_eq!(dispatch.dams.factor, Some(3.5));
        assert_eq!(dispatch.native_speed_unit, Some(Ok(SpeedUnit::Mph)));
        assert_eq!(dispatch.native_refresh_rates.unwrap()[2], Some(LegacyValue::from("71")));
        let controls = state.controls.snapshot();
        let profile = profile_settings(&state.profile.read().tune);
        refresh_context(&state, &parts, &controls, &profile);
        assert_eq!(parts.context.read().dams_front_tl_mode, Some(2));

        // A later read sees the same section's next assignment, rather than
        // recreating its old settings projection over the live value.
        app.assign_item("measurement_unit", LegacyValue::from("K")).unwrap();
        assert_eq!(publication.dispatch_settings().unwrap().native_speed_unit, Some(Ok(SpeedUnit::Kmh)));
        let huge = LegacyValue::from_integer(LegacyValue::from("9".repeat(80)).py_integer().unwrap());
        features.assign_item("dams_front_tl_mode", huge).unwrap();
        assert_eq!(publication.dispatch_settings().unwrap().dams.mode, DamsMode::Off);
    }

    #[test]
    fn native_attitude_selectors_reach_listener_context_and_joystick() {
        let state = state();
        let parts = parts(Vec::new());
        let controls = Controls::default();
        let mut profile = profile_settings(&state.profile.read().tune);
        profile.native =
            Some(LegacyValue::from(serde_json::json!({"motion_effects":{"pitch_type":null,"roll_type":"ANGLE"}})));
        refresh_context(&state, &parts, &controls, &profile);
        let context = parts.context.snapshot();
        assert!(!context.pitch_angle && !context.roll_angle && !context.joystick_options.pitch_read_as_angle);
        profile.native = Some(LegacyValue::from(serde_json::json!({"motion_effects":null})));
        refresh_context(&state, &parts, &controls, &profile);
        let context = parts.context.snapshot();
        assert!(context.pitch_angle && context.roll_angle && context.joystick_options.pitch_read_as_angle);
    }
}
