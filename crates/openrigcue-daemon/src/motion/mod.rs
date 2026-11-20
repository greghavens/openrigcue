//! The motion platform: the stream loop, the
//! test loop, the motion part of the connection scan
//! and the driver table.

pub mod convert;
pub mod drivers;
pub mod yaw_threads;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use openrigcue_config::catalog::{Catalog, GameKind as CatalogKind};
use openrigcue_config::import::value::LegacyValue;
use openrigcue_config::settings::Settings;
use openrigcue_config::tunes::Tune;
use openrigcue_cueing::{
    loop_period_from_config, test_action, test_period_from_config, ClipFactors, ConfigView, CueError, GameConfigView,
    GameKind, HostOs, Listener, Missing, MotionConfig, MotionCueing, NativeConfig, NativePositions, Positions, PyValue,
    RawGameConfig, SmoothingInputs, StreamContext, TestContext, IDLE_TEST_PERIOD,
};
use openrigcue_devices::discovery::ClaimedPorts;
use openrigcue_devices::motion::{
    motion_model, ActuatorPositions, ConnectPassInputs, ControllerIdentity, LinkState, MotionError, MotionHost,
    MotionRegistry, SharedLink, SystemHost,
};
use openrigcue_model::Telemetry;
use serde::Serialize;

use self::convert::DriverRuntime;
use self::drivers::{DriverSettings, Smc3Module};
use crate::runtime::Controls;
use crate::state::ServiceState;

/// The motion state the UI shows (`motion_status`, `motion_com_port`, `motion_warning`, `smc3_version`,
/// `actuators_position`, `motion_stream`, `mc_heave`).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct MotionView {
    pub link: LinkState,
    pub warning: String,
    /// The motion error flag. It starts `false` and nothing sets it: the Dyadic, PROSIMU,
    /// SimTech Revolution and SCN6 drivers only read it in their connect gates, and
    /// `set_hardware_widget` reads it for the Motionbox tooltip. It is
    /// published so the drivers and the sidebar read the same value.
    pub error: bool,
    pub identity: Option<ControllerIdentity>,
    pub actuators: Option<ActuatorPositions>,
    /// The actuator positions as the drivers compute them: each corner's travel in millimetres (float).
    pub travel: Option<openrigcue_devices::motion::driver_support::CornerTravel>,
    /// The last frame handed to the driver.
    pub stream: Option<Positions>,
    /// The shared motor list as dynamically typed values, including wide integers and in-place driver edits.
    pub native_stream: Option<LegacyValue>,
    /// `mc_heave`: the vertical acceleration of the last stream frame, or the heave slider / 10 in test mode.
    pub heave_cue: f64,
    #[serde(skip)]
    pub native_heave_cue: Option<LegacyValue>,
    /// The actuators, the axis assignment status and the traction-loss port of the Dyadic and
    /// PROSIMU drivers (Hardware page Axis Assignment).
    pub assignment: Option<openrigcue_devices::motion::AxisAssignment>,
}

/// A [`MotionHost`] whose port-arbitration inputs the service keeps current.
pub trait PortHost: MotionHost + Send {
    /// The accessory ports and `[APP] allowed_com_ports`, before every connect pass.
    fn set_port_rules(&mut self, claimed: ClaimedPorts, allowed_com_ports: String);
}

impl PortHost for SystemHost {
    fn set_port_rules(&mut self, claimed: ClaimedPorts, allowed_com_ports: String) {
        self.claimed = claimed;
        self.allowed_com_ports = allowed_com_ports;
    }
}

/// The cueing settings and tune of the current configuration revision and profile generation.
struct CachedInputs {
    key: (u64, u64),
    settings: NativeConfig,
    tune: RawGameConfig,
}

/// The same global configuration and profile the UI changes, without retaining its write locks
/// while calculating a frame or sleeping. Derived views are cached only for their revision.
struct LiveMotionInputs {
    controls: crate::runtime::Shared<Controls>,
    session: crate::runtime::Shared<crate::runtime::SessionView>,
    config: crate::runtime::Shared<crate::store::ConfigStore>,
    profile: crate::runtime::Shared<crate::profile::ActiveProfile>,
    config_cache: Mutex<Option<(u64, Arc<NativeConfig>)>>,
    profile_cache: Mutex<Option<(u64, Arc<RawGameConfig>)>>,
}

impl LiveMotionInputs {
    fn new(state: &ServiceState) -> Self {
        Self {
            controls: state.controls.clone(),
            session: state.session.clone(),
            config: state.config.clone(),
            profile: state.profile.clone(),
            config_cache: Mutex::new(None),
            profile_cache: Mutex::new(None),
        }
    }

    fn config(&self) -> Arc<NativeConfig> {
        let config = self.config.read();
        let mut cache = lock(&self.config_cache);
        if cache.as_ref().is_none_or(|(revision, _)| *revision != config.revision()) {
            *cache = Some((config.revision(), Arc::new(NativeConfig::live(&config.native_settings_tree()))));
        }
        Arc::clone(&cache.as_ref().expect("cached above").1)
    }

    fn profile(&self) -> Arc<RawGameConfig> {
        let profile = self.profile.read();
        let mut cache = lock(&self.profile_cache);
        if cache.as_ref().is_none_or(|(generation, _)| *generation != profile.generation) {
            *cache = Some((profile.generation, Arc::new(RawGameConfig::live(&profile.native_tune()))));
        }
        Arc::clone(&cache.as_ref().expect("cached above").1)
    }
}

impl ConfigView for LiveMotionInputs {
    fn current_test_value(&self, index: usize) -> Option<PyValue> {
        self.controls.read().native_motion_test_values().get(index).cloned()
    }
    fn current_game(&self) -> Option<String> {
        Some(self.session.read().game_key().to_owned())
    }
    fn current_previous_game(&self) -> Option<String> {
        Some(self.session.read().previous_game.clone().unwrap_or_default())
    }
    fn current_listener(&self) -> Option<Listener> {
        Some(listener(self.session.read().listener_key()))
    }
    fn current_game_kind(&self) -> Option<GameKind> {
        Some(game_kind(self.session.read().game_key()))
    }
    fn current_model(&self) -> Option<Result<PyValue, CueError>> {
        Some(self.read_value("MOTIONBOX", "model"))
    }
    fn value(&self, section: &str, key: &str) -> Result<std::borrow::Cow<'_, str>, Missing> {
        self.config().value(section, key).map(|value| std::borrow::Cow::Owned(value.into_owned()))
    }

    fn read_value(&self, section: &str, key: &str) -> Result<PyValue, CueError> {
        self.config().read_value(section, key)
    }
}

impl GameConfigView for LiveMotionInputs {
    fn item(&self, key: &str) -> Option<std::borrow::Cow<'_, PyValue>> {
        self.profile().item(key).map(|value| std::borrow::Cow::Owned(value.into_owned()))
    }

    fn read_item(&self, key: &str) -> Result<std::borrow::Cow<'_, PyValue>, CueError> {
        self.profile().read_item(key).map(|value| std::borrow::Cow::Owned(value.into_owned()))
    }
}

impl SmoothingInputs for LiveMotionInputs {
    fn soft_start_manufacturer(&self) -> String {
        // The worker looks up the selected model itself when it starts, with the
        // DOF REALITY fallback when the model lookup fails.
        self.read_value("MOTIONBOX", "model")
            .ok()
            .and_then(|model| {
                model
                    .to_native()
                    .python_string()
                    .and_then(|value| value.scalar_string())
                    .and_then(|model| motion_model(&model, false).map(|model| model.manufacturer))
            })
            .unwrap_or("DOF REALITY")
            .to_owned()
    }

    fn soft_start_timer(&self, manufacturer: &str) -> Result<i128, CueError> {
        self.read_value(manufacturer, "soft_start_stop_timer")?.to_int()
    }

    fn gear_smoothing_factor(&self) -> Result<i128, CueError> {
        self.read_item("gear_smoothing_factor")?.to_int()
    }
    fn soft_start_timer_value(&self, manufacturer: &str) -> Result<PyValue, CueError> {
        self.read_value(manufacturer, "soft_start_stop_timer")?.to_integer_value()
    }
    fn gear_smoothing_factor_value(&self) -> Result<PyValue, CueError> {
        self.read_item("gear_smoothing_factor")?.to_integer_value()
    }
}

/// The cueing half of the motion state: the stream state, the platform
/// table and the settings derived from the configuration.
pub struct CueingUnit {
    cueing: MotionCueing,
    /// The mixing tables as loaded (with the documents folder's CSV tables when there are any).
    config: MotionConfig,
    cached: Option<CachedInputs>,
    live: Option<Arc<LiveMotionInputs>>,
}

/// The driver half: the driver table, its connection state and the host it talks through.
pub struct DriverUnit {
    registry: MotionRegistry,
    host: Box<dyn PortHost>,
}

/// What one motion frame reads from the rest of the service.
pub struct FrameState<'a> {
    pub settings: &'a Settings,
    pub revision: u64,
    pub tune: &'a Tune,
    pub profile: &'a crate::profile::ActiveProfile,
    pub generation: u64,
    pub controls: &'a Controls,
    pub game: &'a str,
    pub listener: &'a str,
    pub previous_game: &'a str,
    pub telemetry: &'a Telemetry,
    /// The motion error flag ([`MotionView::error`]).
    pub motion_error: bool,
}

impl FrameState<'_> {
    fn driver_runtime(&self) -> DriverRuntime<'_> {
        DriverRuntime {
            game_running: self.game,
            listener_running: self.listener,
            motion_test: self.controls.tests.motion_test,
            motion_switch: self.controls.switches.motion,
            motion_error: self.motion_error,
            telemetry: self.telemetry,
            simtech_tune: self.profile.native_simtech_tune(),
        }
    }
}

impl CueingUnit {
    /// A fresh cueing state over the built-in mixing tables (no CSV tables); the clip factors are read once.
    pub fn new(settings: &Settings) -> CueingUnit {
        CueingUnit::with_config(settings, MotionConfig::builtin().clone())
    }

    fn with_config(settings: &Settings, config: MotionConfig) -> CueingUnit {
        let mut cueing = MotionCueing::without_platform(convert::clip_factors(settings));
        cueing.set_motion_config(Arc::new(config.clone()));
        CueingUnit { cueing, config, cached: None, live: None }
    }

    /// The cueing start-up (with the mixing tables and the override CSV tables of the `documents` folder).
    /// Clip factors are converted from the stored settings one by one;
    /// a missing or invalid value fails the start-up there.
    pub fn import(settings: &Settings, documents: &std::path::Path) -> Result<CueingUnit, String> {
        Self::import_stages(settings, documents, &mut false)
    }

    fn import_stages(
        settings: &Settings,
        documents: &std::path::Path,
        yaw_imported: &mut bool,
    ) -> Result<CueingUnit, String> {
        let config = MotionConfig::load(documents).map_err(|e| e.to_string())?;
        convert::native_smc3_startup(&settings.native_snapshot(), settings.motion.reaction_speed)?;
        // In the start-up order, Yaw VR follows SMC3 and precedes the clip reads.
        *yaw_imported = true;
        let native = NativeConfig(PyValue::from(&LegacyValue::Dict(settings.native_snapshot())));
        let clips = ClipFactors::read(&native).map_err(|error| error.traceback())?;
        let mut cueing = MotionCueing::without_platform(clips);
        cueing.set_motion_config(Arc::new(config.clone()));
        Ok(CueingUnit { cueing, config, cached: None, live: None })
    }

    /// Brings the cueing settings and the selected platform up to date with the configuration and tune. A model
    /// the platform table does not know is [`MotionConfig::unlisted`]: the stream path fails with a missing-model error on it
    /// every frame and test mode computes with the `None` model's values.
    fn prepare(&mut self, frame: &FrameState<'_>) {
        let key = (frame.revision, frame.generation);
        if self.cached.as_ref().is_none_or(|c| c.key != key) {
            let model = frame.settings.motion.model.clone();
            let platform = self.config.select(&model).unwrap_or_else(|error| {
                // Only broken built-in tables fail here.
                tracing::error!(model, %error, "motion platform tables do not load");
                None
            });
            match platform {
                Some(platform) => self.cueing.set_platform(platform),
                None => self.cueing.clear_platform(),
            }
            self.cached = Some(CachedInputs {
                key,
                settings: NativeConfig(PyValue::from(&LegacyValue::Dict(frame.settings.native_snapshot()))),
                tune: RawGameConfig(PyValue::from(&frame.profile.native_tune())),
            });
        }
    }

    /// The computation of one stream frame on the telemetry the stream loop passes (already scanned for
    /// NaN); `None` while no game listener runs or no platform model is selected. Nothing is recorded yet
    /// ([`MotionCueing::compute`]).
    pub fn stream_positions(&mut self, frame: &FrameState<'_>, now: Instant) -> Result<Option<Positions>, CueError> {
        self.stream_positions_native(frame, now)?
            .map(|values| {
                let mut out = [0; 12];
                for (i, v) in values.into_iter().enumerate() {
                    out[i] = v
                        .integer()
                        .and_then(|v| v.try_into().ok())
                        .ok_or_else(|| CueError::overflow("Python int too large to convert to C long"))?;
                }
                Ok(out)
            })
            .transpose()
    }
    pub fn stream_positions_native(
        &mut self,
        frame: &FrameState<'_>,
        now: Instant,
    ) -> Result<Option<NativePositions>, CueError> {
        self.prepare(frame);
        let cached = self.cached.as_ref().expect("prepared above");
        let (settings, tune): (&dyn ConfigView, &dyn GameConfigView) = self
            .live
            .as_ref()
            .map_or((&cached.settings as &dyn ConfigView, &cached.tune as &dyn GameConfigView), |live| {
                (live.as_ref() as &dyn ConfigView, live.as_ref() as &dyn GameConfigView)
            });
        let context = StreamContext {
            settings,
            tune,
            listener: listener(frame.listener),
            game: frame.game,
            previous_actual_game: frame.previous_game,
            game_kind: game_kind(frame.game),
        };
        self.cueing.compute_native(frame.telemetry, &context, now)
    }

    /// The computation of one test pass: the frame of the current test sliders and its `heave`
    /// (a tenth of the heave slider, published as `mc_heave`); `None` while no platform model is selected.
    pub fn test_positions(&mut self, frame: &FrameState<'_>) -> Result<Option<(Positions, f64)>, CueError> {
        self.prepare(frame);
        let cached = self.cached.as_ref().expect("prepared above");
        let (settings, tune): (&dyn ConfigView, &dyn GameConfigView) = self
            .live
            .as_ref()
            .map_or((&cached.settings as &dyn ConfigView, &cached.tune as &dyn GameConfigView), |live| {
                (live.as_ref() as &dyn ConfigView, live.as_ref() as &dyn GameConfigView)
            });
        let context = TestContext { settings, tune, game: frame.game };
        self.cueing.test_native_frame(&frame.controls.native_motion_test_values(), &context)
    }
}

impl DriverUnit {
    /// The driver table under the Windows rules; `translate` is the UI language's translation.
    pub fn new(
        settings: &Settings,
        drivers: &DriverSettings,
        host: Box<dyn PortHost>,
        translate: openrigcue_devices::motion::yaw_vr::Translate,
    ) -> DriverUnit {
        let smc3 = Smc3Module::new(drivers.smc3.clone());
        let registry = drivers::motion_registry(drivers::driver_environment(settings), drivers, &smc3, translate);
        DriverUnit { registry, host }
    }

    /// A unit over a prepared registry (tests).
    pub fn with_registry(registry: MotionRegistry, host: Box<dyn PortHost>) -> DriverUnit {
        DriverUnit { registry, host }
    }

    fn send_stream_native(&mut self, model: &str, positions: &mut LegacyValue) -> Result<(), MotionError> {
        self.registry.send_stream_native_in_place(model, positions, self.host.as_mut())
    }
    fn send_test_native(&mut self, model: &str, positions: &mut LegacyValue) -> Result<(), MotionError> {
        self.registry.send_test_native_in_place(model, positions, self.host.as_mut())
    }
    fn debug_native(&mut self, model: &str, positions: &LegacyValue) -> Result<(), MotionError> {
        self.registry.debug_native(model, positions, self.host.as_mut())
    }

    /// Disconnects and forgets the motion ports while the motion switch is off (test loop;
    /// an error ends the test thread).
    fn switch_off(&mut self, model: &str) -> Result<(), MotionError> {
        self.registry.switch_off(model, self.host.as_mut())
    }

    fn connect_inputs<'a>(settings: &'a Settings, motion_switch_on: bool) -> ConnectPassInputs<'a> {
        ConnectPassInputs {
            model: &settings.motion.model,
            motion_switch_on,
            auto_search: settings.motion.auto_search,
            enable_debug: settings.motion.debug,
        }
    }

    /// Both motion steps of one connection scan pass ([`MotionRegistry::connect_pass`]).
    fn connect_pass(&mut self, settings: &Settings, motion_switch_on: bool, claimed: ClaimedPorts) {
        self.host.set_port_rules(claimed, settings.app.allowed_serial_ports.join(","));
        self.registry.connect_pass(&Self::connect_inputs(settings, motion_switch_on), self.host.as_mut());
    }

    /// The motion connect of one connection scan pass ([`MotionRegistry::connect_step`]).
    fn connect_step(&mut self, settings: &Settings, motion_switch_on: bool, claimed: ClaimedPorts) {
        self.host.set_port_rules(claimed, settings.app.allowed_serial_ports.join(","));
        self.registry.connect_step(&Self::connect_inputs(settings, motion_switch_on), self.host.as_mut());
    }

    /// The motion port check of one connection scan pass ([`MotionRegistry::check_ports`]).
    fn check_ports(&mut self, settings: &Settings, motion_switch_on: bool) -> Result<(), MotionError> {
        self.registry.check_ports(&Self::connect_inputs(settings, motion_switch_on), self.host.as_mut())
    }

    /// The motion controller ports.
    pub fn ports(&self) -> Vec<String> {
        self.registry.link().ports.values().cloned().collect()
    }

    fn publish(&self, model: &str, drivers: &DriverSettings, view: &mut MotionView) {
        view.link = self.registry.link().clone();
        view.warning = drivers.warning();
        view.identity = self.registry.identity(model);
        view.actuators = self.registry.actuators_position(model);
        view.travel = self.registry.actuators_travel(model);
        view.assignment = self.registry.assignment(model);
    }
}

/// The running listener as the cueing sees it: none or a game listener.
fn listener(listener_running: &str) -> Listener {
    match listener_running {
        "none" => Listener::None,
        "emulator" => Listener::Emulator,
        "test" => Listener::Test,
        _ => Listener::Game,
    }
}

/// The running game's catalog type, `racing` for an id the catalog does not know.
fn game_kind(game: &str) -> GameKind {
    match Catalog::builtin().game(game).map(|g| g.kind) {
        None | Some(CatalogKind::Racing) => GameKind::Racing,
        Some(_) => GameKind::Other,
    }
}

/// Whether the `MOTIONBOX` model's type is serial, `None` when unknown.
pub fn model_is_serial(model: &str) -> Option<bool> {
    motion_model(model, false).map(|m| m.transport == openrigcue_devices::motion::Transport::Serial)
}

/// The motion state shared by its loops, the connection scan and the API. The cueing and the drivers have
/// separate locks so a slow connect (an SMC3 port probe takes seconds) does not hold up the cueing, which
/// is computed concurrently in its own thread.
#[derive(Clone)]
pub struct MotionHandle {
    test_motion_config: Arc<MotionConfig>,
    test_clips: ClipFactors,
    cueing: Arc<Mutex<CueingUnit>>,
    drivers: Arc<Mutex<DriverUnit>>,
    settings: DriverSettings,
    /// The motion status and ports as the drivers and their own threads write them.
    link: SharedLink,
    /// `false` when the cueing start-up failed ([`MotionHandle::import`]): no stream or test thread
    /// runs and no motion driver connects.
    available: bool,
    /// Whether start-up reached the Yaw VR driver before a later failure.
    yaw_imported: bool,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn current_driver_model(state: &ServiceState) -> Result<String, MotionError> {
    let root = state.config.read().native_settings_tree();
    let config = NativeConfig::live(&root);
    let model = config
        .read_value("MOTIONBOX", "model")
        .map_err(|e| MotionError::Native(format!("{}: {}", e.exception().name(), e.message())))?;
    let raw = model.to_native().resolved();
    if let Some(text) = raw.python_string().and_then(|v| v.scalar_string()) {
        return Ok(text);
    }
    if matches!(raw, LegacyValue::List(_) | LegacyValue::Dict(_) | LegacyValue::Set(_) | LegacyValue::ByteArray(_)) {
        return Err(MotionError::Native(format!("TypeError: unhashable type: '{}'", raw.type_name())));
    }
    Err(MotionError::Native(format!("KeyError: {}", model.repr())))
}

impl MotionHandle {
    pub fn new(cueing: CueingUnit, drivers: DriverUnit, settings: DriverSettings) -> MotionHandle {
        let link = drivers.registry.shared_link();
        let test_motion_config = Arc::new(cueing.config.clone());
        let test_clips = *cueing.cueing.clip_factors();
        MotionHandle {
            test_motion_config,
            test_clips,
            cueing: Arc::new(Mutex::new(cueing)),
            drivers: Arc::new(Mutex::new(drivers)),
            settings,
            link,
            available: true,
            yaw_imported: true,
        }
    }

    /// A cueing state over the built-in tables and the driver table (Windows rules) on `host`.
    ///
    /// The SMC3 settings are filled from the configuration before the driver table is built, because the SMC3
    /// driver reads `left/right_debug_port` and `reaction_speed` when it is created.
    /// Texts the drivers write are left untranslated ([`MotionHandle::import`] translates them).
    pub fn for_settings(settings: &Settings, host: Box<dyn PortHost>) -> MotionHandle {
        Self::with_translate(settings, host, Arc::new(str::to_owned))
    }

    fn with_translate(
        settings: &Settings,
        host: Box<dyn PortHost>,
        translate: openrigcue_devices::motion::yaw_vr::Translate,
    ) -> MotionHandle {
        let driver_settings = DriverSettings::default();
        driver_settings.smc3.set(convert::smc3_settings(settings));
        let drivers = DriverUnit::new(settings, &driver_settings, host, translate);
        MotionHandle::new(CueingUnit::new(settings), drivers, driver_settings)
    }

    /// The service's setup: [`MotionHandle::for_settings`] with the cueing start-up for the documents
    /// folder ([`CueingUnit::import`]). When that fails, the error is logged and motion stays
    /// unavailable: no stream or test thread, no motion driver connect.
    pub fn import(
        settings: &Settings,
        documents: &std::path::Path,
        host: Box<dyn PortHost>,
        translate: openrigcue_devices::motion::yaw_vr::Translate,
    ) -> MotionHandle {
        let mut handle = MotionHandle::with_translate(settings, host, translate);
        handle.yaw_imported = false;
        match CueingUnit::import_stages(settings, documents, &mut handle.yaw_imported) {
            Ok(cueing) => {
                handle.test_motion_config = Arc::new(cueing.config.clone());
                handle.test_clips = *cueing.cueing.clip_factors();
                handle.cueing = Arc::new(Mutex::new(cueing));
            }
            Err(error) => {
                tracing::error!("importing the motion module failed: {error}");
                handle.available = false;
            }
        }
        handle
    }

    /// Driver shared state the UI reads (Yaw VR devices, Thanos controller, SimTech actuators).
    pub fn driver_settings(&self) -> &DriverSettings {
        &self.settings
    }

    /// The driver's current shared status, including writes by its background threads.
    pub fn link_state(&self) -> LinkState {
        self.link.get()
    }

    /// The motion controller ports, for the accessories' port arbitration.
    pub fn ports(&self) -> Vec<String> {
        lock(&self.drivers).ports()
    }

    /// Marks the platform not connected and forgets its ports, from the UI (the axis assignment confirmation).
    pub fn mark_disconnected(&self, state: &ServiceState) {
        self.link.drop_link();
        state.motion.write().link = self.link.get();
    }

    /// Runs `step` with the current frame state of the service, after the drivers' settings were refreshed.
    fn with_frame<R>(&self, state: &ServiceState, step: impl FnOnce(&FrameState<'_>) -> R) -> R {
        let motion_error = state.motion.read().error;
        let telemetry = state.telemetry.snapshot();
        let session = state.session.snapshot();
        let controls = state.controls.snapshot();
        let (settings, revision) = {
            let config = state.config.read();
            (config.settings().clone(), config.revision())
        };
        let profile = state.profile.snapshot();
        let frame = FrameState {
            settings: &settings,
            revision,
            tune: &profile.tune,
            profile: &profile,
            generation: profile.generation,
            controls: &controls,
            game: session.game_key(),
            listener: session.listener_key(),
            previous_game: session.previous_game.as_deref().unwrap_or(""),
            telemetry: &telemetry,
            motion_error,
        };
        let manufacturer = motion_model(&frame.settings.motion.model, false).map_or("", |m| m.manufacturer);
        self.settings.refresh(
            frame.settings,
            manufacturer,
            frame.tune,
            &frame.driver_runtime(),
            &frame.profile.native_thanos_tune(),
        );
        step(&frame)
    }

    fn publish(&self, state: &ServiceState, drivers: &DriverUnit, model: &str, delivered: Option<(Positions, f64)>) {
        let mut view = state.motion.write();
        drivers.publish(model, &self.settings, &mut view);
        if let Some((positions, heave)) = delivered {
            view.stream = Some(positions);
            view.heave_cue = heave;
        }
    }

    fn publish_native(
        &self,
        state: &ServiceState,
        drivers: &DriverUnit,
        model: &str,
        positions: &LegacyValue,
        heave: f64,
        native_heave: &LegacyValue,
    ) {
        let mut view = state.motion.write();
        drivers.publish(model, &self.settings, &mut view);
        view.native_stream = Some(positions.clone());
        view.stream = (|| {
            let mut values = [0; 12];
            for (i, value) in values.iter_mut().enumerate() {
                *value = positions.get_index(i).ok()?.integer()?.try_into().ok()?;
            }
            Some(values)
        })();
        view.heave_cue = heave;
        view.native_heave_cue = Some(native_heave.clone());
    }

    /// One stream frame of the stream loop on the telemetry as it stands (already scanned for NaN):
    /// the frame, delivered when the controller is connected and the motion switch is on,
    /// then the driver's debug hook while the
    /// Telemetry page's capture switch is on.
    ///
    /// `motion_stream` and `mc_heave` are published before the driver call, and hold the frame as
    /// the driver left it. A driver or debug hook error is returned.
    pub fn stream_frame(&self, state: &ServiceState, now: Instant) -> Result<(), FrameError> {
        let telemetry = openrigcue_cueing::capture_motion_arguments(|key| {
            let t = state.telemetry.read();
            t.native_value(key).cloned().or_else(|| t.get_assigned(key).map(LegacyValue::Float))
        })?;
        let (positions, model, heave, native_heave) = self.with_frame(state, |frame| {
            let frame = FrameState { telemetry: &telemetry, ..*frame };
            let positions = lock(&self.cueing).stream_positions_native(&frame, now)?;
            Ok::<_, CueError>((
                positions,
                frame.settings.motion.model.clone(),
                frame.telemetry.vertical_acceleration,
                frame
                    .telemetry
                    .native_value("vertical_acceleration")
                    .cloned()
                    .unwrap_or(LegacyValue::Float(frame.telemetry.vertical_acceleration)),
            ))
        })?;
        if let Some(positions) = positions {
            let mut positions = LegacyValue::Object(openrigcue_model::native_object::NativeObject::new(
                LegacyValue::List(positions.into_iter().collect()),
            ));
            // The motion status as the drivers and their threads keep it, and
            // the motion switch, read once the frame is computed.
            let output_active = self.link.is_connected() && state.controls.read().switches.motion;
            // The driver lock is taken only to deliver, so frames keep being computed while a connect runs.
            let mut drivers = lock(&self.drivers);
            if output_active {
                self.publish_native(state, &drivers, &model, &positions, heave, &native_heave);
                let actual_model =
                    current_driver_model(state).map_err(DriverFailure::at(1044)).map_err(FrameError::Driver)?;
                let sent = drivers.send_stream_native(&actual_model, &mut positions);
                self.publish_native(state, &drivers, &model, &positions, heave, &native_heave);
                sent.map_err(DriverFailure::at(1044)).map_err(FrameError::Driver)?;
                // The last delivered frame is the list as the driver left it, once the call returned.
                let delivered = std::array::from_fn(|i| positions.get_index(i).expect("source frame has 12 slots"));
                lock(&self.cueing).cueing.record_native_delivered(&delivered);
            }
            // The capture switch, read now.
            if state.controls.read().telemetry.capture {
                let actual_model =
                    current_driver_model(state).map_err(DriverFailure::at(1051)).map_err(FrameError::Driver)?;
                drivers
                    .debug_native(&actual_model, &positions)
                    .map_err(DriverFailure::at(1051))
                    .map_err(FrameError::Driver)?;
            }
        }
        // Reached when nothing failed.
        lock(&self.cueing).cueing.record_listener(listener(state.session.read().listener_key()));
        Ok(())
    }

    /// The computation half of one test pass: the frame and its `mc_heave`.
    pub fn test_positions(&self, state: &ServiceState) -> Result<Option<(Positions, f64)>, CueError> {
        self.test_positions_native(state)?
            .map(|(values, heave)| {
                let mut positions = [0; 12];
                for (slot, value) in values.into_iter().enumerate() {
                    positions[slot] = value
                        .integer()
                        .and_then(|value| value.try_into().ok())
                        .ok_or_else(|| CueError::overflow("Python int too large to convert to C long"))?;
                }
                Ok((positions, heave))
            })
            .transpose()
    }

    pub fn test_positions_native(&self, state: &ServiceState) -> Result<Option<(NativePositions, f64)>, CueError> {
        self.with_frame(state, |frame| {
            let platform = self
                .test_motion_config
                .select(&frame.settings.motion.model)
                .map_err(|error| CueError::new(openrigcue_cueing::Exception::Exception, error.to_string()))?;
            let computation =
                openrigcue_cueing::MotionTestSnapshot::new(platform, self.test_clips, self.test_motion_config.clone());
            let live = LiveMotionInputs::new(state);
            let context = TestContext { settings: &live, tune: &live, game: frame.game };
            computation.test_native_values_frame(&frame.controls.native_motion_test_values(), &context)
        })
    }

    /// The delivery half of one test pass, after its sleep: send while no game listener runs, the
    /// controller is connected and the motion switch is on; then disconnect when the motion switch, read
    /// again, is off.
    ///
    /// Neither call is guarded: a driver error, or the lookup of a model
    /// the model table does not know (a missing-model error), is returned and ends the test thread. `motion_stream`
    /// and `mc_heave` are published before the send, as the frame the driver left.
    pub fn deliver_test(&self, state: &ServiceState, frame: Option<(Positions, f64)>) -> Result<(), DriverFailure> {
        self.deliver_test_native(state, frame.map(|(positions, heave)| (positions.map(LegacyValue::Int), heave)))
    }

    pub fn deliver_test_native(
        &self,
        state: &ServiceState,
        frame: Option<(NativePositions, f64)>,
    ) -> Result<(), DriverFailure> {
        let listener_running = state.session.read().listener_key().to_owned();
        let connected = self.link.is_connected();
        let switch_on = state.controls.read().switches.motion;
        let send = test_action(listener(&listener_running), connected, switch_on).send;
        let mut drivers = lock(&self.drivers);
        if send {
            if let Some((positions, heave)) = frame {
                let model = state.config.read().settings().motion.model.clone();
                let mut native = LegacyValue::Object(openrigcue_model::native_object::NativeObject::new(
                    LegacyValue::List(positions.into_iter().collect()),
                ));
                self.publish_native(state, &drivers, &model, &native, heave, &LegacyValue::Float(heave));
                let actual_model = current_driver_model(state).map_err(DriverFailure::at(1147))?;
                let sent = drivers.send_test_native(&actual_model, &mut native);
                self.publish_native(state, &drivers, &model, &native, heave, &LegacyValue::Float(heave));
                sent.map_err(DriverFailure::at(1147))?;
            }
        }
        if !state.controls.read().switches.motion {
            let model = state.config.read().settings().motion.model.clone();
            let result = drivers.switch_off(&model);
            self.publish(state, &drivers, &model, None);
            result.map_err(DriverFailure::at(1149))?;
        }
        Ok(())
    }

    fn connect_settings(&self, state: &ServiceState) -> (Settings, bool) {
        self.with_frame(state, |frame| (frame.settings.clone(), frame.controls.switches.motion))
    }

    /// Both motion steps of one connection scan pass back to back (the connect and the port
    /// check); a port-check error is only logged. See [`connect_step`](Self::connect_step) and
    /// [`check_ports`](Self::check_ports) for the steps the scan loop runs separately.
    pub fn connect_pass(&self, state: &ServiceState, claimed: ClaimedPorts) {
        if !self.available {
            return;
        }
        let (settings, switch_on) = self.connect_settings(state);
        let mut drivers = lock(&self.drivers);
        drivers.connect_pass(&settings, switch_on, claimed);
        self.publish(state, &drivers, &settings.motion.model, None);
        drop(drivers);
        self.apply_driver_config_writes(state);
    }

    /// Step 2 of the connection scan loop: the motion connect when not connected, the
    /// motion switch is on and auto-search is enabled.
    pub fn connect_step(&self, state: &ServiceState, claimed: ClaimedPorts) {
        if !self.available {
            return;
        }
        let (settings, switch_on) = self.connect_settings(state);
        let mut drivers = lock(&self.drivers);
        drivers.connect_step(&settings, switch_on, claimed);
        self.publish(state, &drivers, &settings.motion.model, None);
        drop(drivers);
        self.apply_driver_config_writes(state);
    }

    /// Applies the configuration write a driver's `connect` made. The Thanos driver stores the controller's
    /// stroke in its manufacturer's `vertical_actuators_stroke`; the value is
    /// written as the driver produced it — a number, or repeated text for a controller field that is not one —
    /// so what later reads it (the Motion page's spike-filter millimetres) meets the same value.
    fn apply_driver_config_writes(&self, state: &ServiceState) {
        let mut pending = None;
        self.settings.thanos_shared.update(|shared| pending = shared.vertical_stroke.take());
        let Some((manufacturer, value)) = pending else {
            return;
        };
        let mut store = state.config.write();
        let settings = store.native_settings_tree();
        let Ok(section) = settings.item_owned(&manufacturer) else {
            return;
        };
        let mut section = section;
        if section.assign_item("vertical_actuators_stroke", value).is_err() {
            return;
        }
        store.update(|configuration| {
            configuration.settings.edit_native(|raw| {
                raw.insert(manufacturer.as_str(), section.resolved());
            })
        });
    }

    /// The motion port check of the connection scan loop (after the accessory devices). An
    /// error is the missing-model error for a model the model table does not know: it ends the
    /// connection scan thread.
    pub fn check_ports(&self, state: &ServiceState) -> Result<(), MotionError> {
        if !self.available {
            return Ok(());
        }
        let (settings, switch_on) = self.connect_settings(state);
        let mut drivers = lock(&self.drivers);
        let result = drivers.check_ports(&settings, switch_on);
        self.publish(state, &drivers, &settings.motion.model, None);
        result
    }
}

/// One frame of a call trace (a frame carries no source text).
fn frame_line(file: &str, function: &str, line: u32) -> String {
    let module = file.trim_end_matches(".py").replace('/', ".");
    format!("  File \"{file}\", line {line}, in {module}.{function}\n")
}

const CONTROLLERS_FILE: &str = "backend/controllers/controllers.py";
const MOTION_HELPERS_FILE: &str = "backend/controllers/motion_helpers.py";

/// A driver call that failed, and the trace line label of that call.
#[derive(Debug)]
pub struct DriverFailure {
    pub error: MotionError,
    /// The driver call: the send or debug call of the stream path, or the send or disconnect call
    /// of the test path.
    pub line: u32,
}

impl DriverFailure {
    fn at(line: u32) -> impl FnOnce(MotionError) -> DriverFailure {
        move |error| DriverFailure { error, line }
    }

    /// `<error class>: <message>` where the error's class name is known (see
    /// [`MotionError::exception_line`]); else this error's own text.
    fn exception_line(&self) -> String {
        self.error.exception_line().unwrap_or_else(|| self.error.to_string())
    }

    /// The error's message text.
    fn python_str(&self) -> String {
        match self.error.root() {
            MotionError::UnknownModel(model) => py_repr(model),
            MotionError::NoDriver(key) => py_repr(key),
            // A typed error's message, `list index out of range`, the serial text for a port error.
            MotionError::Native(_) | MotionError::TooFewMotors { .. } | MotionError::Link(_) => self.error.python_str(),
            other => other.to_string(),
        }
    }

    /// The call trace from the path the call is in (stream or test): its frame
    /// at the line of the driver call, the frames the driver recorded for the error (none where a driver
    /// does not record them), and the error line.
    fn traceback_from(&self, function: &str) -> String {
        let driver: String = self.error.frames().iter().map(|frame| frame.text()).collect();
        format!("{}{driver}{}\n", frame_line(MOTION_HELPERS_FILE, function, self.line), self.exception_line())
    }

    /// What is printed in front of that trace when the driver failed while it handled
    /// another error: the handled error's trace and the chaining sentence (see
    /// [`MotionError::handled_traceback`]). Empty otherwise, and where the handled error is not known.
    fn handled_traceback(&self) -> String {
        self.error.handled_traceback().unwrap_or_default()
    }
}

/// Why one stream frame failed; the stream loop logs it and goes on.
#[derive(Debug)]
pub enum FrameError {
    /// A failure of the frame computation, with its error kind and frames.
    Cue(CueError),
    /// A failure of the driver's send or debug call.
    Driver(DriverFailure),
}

impl From<CueError> for FrameError {
    fn from(error: CueError) -> FrameError {
        FrameError::Cue(error)
    }
}

impl FrameError {
    /// The line the stream loop logs: the message and the formatted call trace.
    pub fn refresh_motionbox_log(&self) -> String {
        match self {
            FrameError::Cue(error) => error.refresh_motionbox_log(),
            FrameError::Driver(failure) => format!(
                "error sending data to motionbox: {}, traceback: {}Traceback (most recent call last):\n{}{}",
                failure.python_str(),
                failure.handled_traceback(),
                frame_line(CONTROLLERS_FILE, "refresh_motionbox", 418),
                failure.traceback_from("send_stream")
            ),
        }
    }
}

/// A text in quotes, with escapes.
fn py_repr(text: &str) -> String {
    if text.contains('\'') && !text.contains('"') {
        format!("\"{}\"", text.replace('\\', "\\\\"))
    } else {
        format!("'{}'", text.replace('\\', "\\\\").replace('\'', "\\'"))
    }
}

/// The report written when a thread ends with an error: `Exception in thread
/// <name>:` and the call trace (`frames`, outermost first, then the error line).
pub(crate) fn thread_died(name: &str, frames_and_exception: &str) {
    use std::io::Write;
    let _ = std::io::stderr().lock().write_all(thread_death_text(name, "", frames_and_exception).as_bytes());
}

/// The thread-death report's text: the thread line, the trace of an error that was being handled when
/// this one occurred (`handled`, with its chaining sentence; empty when there was none) and the trace.
fn thread_death_text(name: &str, handled: &str, frames_and_exception: &str) -> String {
    format!("Exception in thread {name}:\n{handled}Traceback (most recent call last):\n{frames_and_exception}")
}

/// [`thread_died`] for a driver call that failed.
fn driver_thread_died(name: &str, failure: &DriverFailure, function: &str) {
    use std::io::Write;
    let text = thread_death_text(name, &failure.handled_traceback(), &failure.traceback_from(function));
    let _ = std::io::stderr().lock().write_all(text.as_bytes());
}

/// The frames and error of an unusable `[MOTIONBOX] refresh_rate`: the division fails for 0, the
/// sleep for a negative rate. Both the stream loop and the test loop sleep `1 / refresh_rate`.
#[cfg(test)]
fn refresh_rate_exception(refresh_rate: i64, in_send_test: bool) -> String {
    let exception = if refresh_rate == 0 {
        "ZeroDivisionError: float division by zero"
    } else {
        "ValueError: sleep length must be non-negative"
    };
    let frames = if in_send_test {
        frame_line(MOTION_HELPERS_FILE, "send_test", 1141)
    } else if refresh_rate == 0 {
        frame_line(CONTROLLERS_FILE, "refresh_motionbox", 451) + &frame_line(CONTROLLERS_FILE, "get_refresh_rate", 392)
    } else {
        frame_line(CONTROLLERS_FILE, "refresh_motionbox", 451)
    };
    format!("{frames}{exception}\n")
}

/// The call trace text of a computation error, after its header line.
fn cue_frames_and_exception(error: &CueError) -> String {
    let traceback = error.traceback();
    traceback.strip_prefix("Traceback (most recent call last):\n").unwrap_or(&traceback).to_string()
}

/// The running motion loops. They end
/// when the service stops ([`MotionThreads::stop`]).
pub struct MotionThreads {
    stop: Arc<AtomicBool>,
    handles: Vec<JoinHandle<()>>,
    /// The Yaw VR discovery and feedback threads, which run for the whole process (without a stop).
    yaw_handles: Vec<JoinHandle<()>>,
    /// The `YawVRGEFile` block, removed at [`MotionThreads::stop`].
    yaw_export: Option<Arc<openrigcue_devices::motion::yaw_vr::export::GameEngineExport>>,
    smoothing: Option<MotionSmoothing>,
}

struct MotionSmoothing(MotionHandle);

impl Drop for MotionSmoothing {
    fn drop(&mut self) {
        let mut cueing = lock(&self.0.cueing);
        cueing.cueing.stop_smoothing_workers();
        cueing.live = None;
    }
}

impl MotionThreads {
    /// Starts the threads in a fixed order: the Yaw VR driver's
    /// feedback and discovery threads (see [`yaw_threads`]), the test loop,
    /// then the stream loop. With `export_dir` (the bridge's mirror
    /// directory) the `YawVRGEFile` block is written there.
    pub fn start(
        state: ServiceState,
        motion: MotionHandle,
        export_dir: Option<&std::path::Path>,
    ) -> std::io::Result<MotionThreads> {
        Self::start_with(state, motion, export_dir, yaw_threads::SystemYawNet::with_export)
    }

    /// [`start`](Self::start) with the Yaw VR loops `yaw_net` builds over the export block.
    pub fn start_with<N: yaw_threads::YawNet>(
        state: ServiceState,
        motion: MotionHandle,
        export_dir: Option<&std::path::Path>,
        yaw_net: impl FnOnce(Option<Arc<openrigcue_devices::motion::yaw_vr::export::GameEngineExport>>) -> N,
    ) -> std::io::Result<MotionThreads> {
        let stop = Arc::new(AtomicBool::new(false));
        if !motion.yaw_imported {
            return Ok(MotionThreads {
                stop,
                handles: Vec::new(),
                yaw_handles: Vec::new(),
                yaw_export: None,
                smoothing: None,
            });
        }
        let smoothing = if motion.available {
            let live = Arc::new(LiveMotionInputs::new(&state));
            let mut cueing = lock(&motion.cueing);
            cueing.cueing.start_smoothing_workers(live.clone())?;
            cueing.live = Some(live);
            Some(MotionSmoothing(motion.clone()))
        } else {
            None
        };
        let yaw_export = yaw_threads::open_game_engine_export(export_dir);
        // The Yaw VR driver reads its configuration when it starts.
        motion.with_frame(&state, |_| ());
        let selected = {
            let state = state.clone();
            move || yaw_threads::yaw_vr_selected(&state.config.read().settings().motion.model)
        };
        let yaw_handles = yaw_threads::start(&mut yaw_net(yaw_export.clone()), motion.driver_settings(), selected)?;
        let mut handles = Vec::new();
        if !motion.available {
            return Ok(MotionThreads { stop, handles, yaw_handles, yaw_export, smoothing });
        }
        {
            let (state, motion, stop) = (state.clone(), motion.clone(), Arc::clone(&stop));
            handles.push(std::thread::Builder::new().name("motion_send_test".into()).spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    if !test_pass(&state, &motion, &stop) {
                        return;
                    }
                }
            })?);
        }
        {
            let (state, motion, stop) = (state, motion, Arc::clone(&stop));
            handles.push(std::thread::Builder::new().name("refresh_motionbox".into()).spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    let Some(pause) = stream_pass(&state, &motion) else { return };
                    std::thread::sleep(pause);
                }
            })?);
        }
        Ok(MotionThreads { stop, handles, yaw_handles, yaw_export, smoothing })
    }

    /// Ends the loops with the service; the `YawVRGEFile` block goes away with them.
    pub fn stop(self) {
        self.stop.store(true, Ordering::SeqCst);
        for handle in self.handles {
            let _ = handle.join();
        }
        drop(self.smoothing);
        drop(self.yaw_handles);
        if let Some(export) = self.yaw_export {
            if let Err(error) = export.remove() {
                tracing::warn!(%error, "cannot remove the YawVRGEFile export");
            }
        }
    }
}

/// One stream loop iteration: the initial telemetry readiness probe, then the NaN scan
/// of the shared telemetry (a NaN field becomes 0
/// and is logged), the stream frame (an error is logged), then the pause for the refresh rate
/// read now. `None` when the refresh rate cannot be slept on: the thread ends there.
pub fn stream_pass(state: &ServiceState, motion: &MotionHandle) -> Option<Duration> {
    // Probe the telemetry's `spotter_right` key; on a missing
    // key sleep 0.5 and retry before scanning NaNs, sending or reading the rate.
    if state.telemetry.read().get_assigned("spotter_right").is_none() {
        return Some(Duration::from_millis(500));
    }
    if let Err(error) = clear_live_telemetry_nan(state) {
        tracing::error!("error while preraring data to motionbox: {error}, traceback: {}", error.traceback());
    }
    if let Err(error) = motion.stream_frame(state, Instant::now()) {
        tracing::error!("{}", error.refresh_motionbox_log());
    }
    let live = LiveMotionInputs::new(state);
    let config = live.config();
    match loop_period_from_config(config.as_ref(), HostOs::Windows) {
        Ok(pause) => Some(pause),
        Err(error) => {
            thread_died("refresh_motionbox", &cue_frames_and_exception(&error));
            None
        }
    }
}

/// The dictionary iterator and each assignment release the shared telemetry lock.
/// Producers can publish values between the scan's steps; no whole-scan write lock is held.
fn clear_live_telemetry_nan(state: &ServiceState) -> Result<(), CueError> {
    let at = |error: CueError, line| {
        error.at(openrigcue_cueing::Frame {
            file: CONTROLLERS_FILE,
            prefix: "backend.controllers.controllers.",
            function: "refresh_motionbox",
            line,
        })
    };
    let keys = state.telemetry.read().channel_names().to_vec();
    for key in &keys {
        let value = {
            let t = state.telemetry.read();
            if t.channel_names().len() != keys.len() {
                return Err(at(
                    CueError::new(
                        openrigcue_cueing::Exception::Named("RuntimeError"),
                        "dictionary changed size during iteration",
                    ),
                    405,
                ));
            }
            t.native_value(key).cloned().or_else(|| t.get_assigned(key).map(LegacyValue::Float)).ok_or_else(|| {
                at(
                    CueError::new(
                        openrigcue_cueing::Exception::Named("RuntimeError"),
                        "dictionary keys changed during iteration",
                    ),
                    405,
                )
            })?
        };
        let number = value.py_as_double().map_err(|error| {
            let (class, message) = error.split_once(": ").unwrap_or(("Exception", &error));
            at(
                CueError::new(
                    match class {
                        "TypeError" => openrigcue_cueing::Exception::TypeError,
                        "OverflowError" => openrigcue_cueing::Exception::OverflowError,
                        _ => openrigcue_cueing::Exception::Exception,
                    },
                    message,
                ),
                406,
            )
        })?;
        if number.is_nan() {
            state.telemetry.write().set_native(key, LegacyValue::Int(0));
            tracing::error!("found a nan in the telemetry, field: {key}");
        }
    }
    if state.telemetry.read().channel_names().len() != keys.len() {
        return Err(at(
            CueError::new(
                openrigcue_cueing::Exception::Named("RuntimeError"),
                "dictionary changed size during iteration",
            ),
            405,
        ));
    }
    Ok(())
}

/// One test loop pass; `false` when the thread ends (a failing frame, an unusable refresh rate or a failing
/// driver call; or the service stopping during the pause).
///
/// The model is checked first, the frame computed, then `motion_test_mode` and `refresh_rate` read
/// for the pause before delivery.
pub fn test_pass(state: &ServiceState, motion: &MotionHandle, stop: &AtomicBool) -> bool {
    let model = LiveMotionInputs::new(state).read_value("MOTIONBOX", "model");
    let model = match model {
        Ok(model) => model,
        Err(error) => {
            thread_died(
                "motion_send_test",
                &cue_frames_and_exception(&error.at(openrigcue_cueing::Frame {
                    file: "backend/controllers/motion_helpers.py",
                    prefix: "backend.controllers.motion_helpers.",
                    function: "send_test",
                    line: 1083,
                })),
            );
            return false;
        }
    };
    if model.equals_str("None") {
        std::thread::sleep(IDLE_TEST_PERIOD);
        return true;
    }
    let frame = match motion.test_positions_native(state) {
        Ok(frame) => frame,
        Err(error) => {
            thread_died("motion_send_test", &cue_frames_and_exception(&error));
            return false;
        }
    };
    let full_rate = state.controls.read().tests.motion_test;
    let live = LiveMotionInputs::new(state);
    let config = live.config();
    let pause = match test_period_from_config(full_rate, config.as_ref(), HostOs::Windows) {
        Ok(pause) => pause,
        Err(error) => {
            thread_died("motion_send_test", &cue_frames_and_exception(&error));
            return false;
        }
    };
    std::thread::sleep(pause);
    if stop.load(Ordering::SeqCst) {
        return false;
    }
    if let Err(failure) = motion.deliver_test_native(state, frame) {
        driver_thread_died("motion_send_test", &failure, "send_test");
        return false;
    }
    true
}

/// A motion host for tests: synthetic port listings, failed serial opens and no network.
#[cfg(test)]
pub(crate) mod testing {
    use std::net::SocketAddr;
    use std::time::Duration;

    use openrigcue_devices::discovery::{ClaimedPorts, PortInfo};
    use openrigcue_devices::motion::clock::Clock;
    use openrigcue_devices::motion::MotionHost;
    use openrigcue_devices::serial::{Link, LinkError, OpenOptions};

    use super::PortHost;

    /// A host with optional synthetic ports and no network: serial opens fail. Datagrams are recorded only.
    #[derive(Default)]
    pub(crate) struct OfflineHost {
        clock: StoppedClock,
        pub(crate) listed_ports: Vec<PortInfo>,
        pub(crate) rules: Option<(ClaimedPorts, String)>,
        pub(crate) datagrams: std::sync::Arc<std::sync::Mutex<Vec<SocketAddr>>>,
    }

    #[derive(Default)]
    struct StoppedClock(Duration);

    impl Clock for StoppedClock {
        fn now(&self) -> Duration {
            self.0
        }
        fn sleep(&mut self, d: Duration) {
            self.0 += d;
        }
    }

    impl MotionHost for OfflineHost {
        fn clock(&mut self) -> &mut dyn Clock {
            &mut self.clock
        }
        fn serial_ports(&mut self) -> Vec<PortInfo> {
            self.listed_ports.clone()
        }
        fn open_serial(&mut self, path: &str, _options: &OpenOptions) -> Result<Box<dyn Link>, LinkError> {
            Err(LinkError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, path.to_owned())))
        }
        fn claimed_ports(&self) -> ClaimedPorts {
            self.rules.as_ref().map(|r| r.0.clone()).unwrap_or_default()
        }
        fn allowed_com_ports(&self) -> String {
            self.rules.as_ref().map(|r| r.1.clone()).unwrap_or_default()
        }
        fn send_datagram(&mut self, target: SocketAddr, _payload: &[u8]) -> std::io::Result<()> {
            self.datagrams.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(target);
            Err(std::io::Error::other("offline"))
        }
    }

    impl PortHost for OfflineHost {
        fn set_port_rules(&mut self, claimed: ClaimedPorts, allowed_com_ports: String) {
            self.rules = Some((claimed, allowed_com_ports));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::OfflineHost;
    use super::*;
    use crate::state::OriginalPlatform;
    use crate::store::ConfigStore;
    use openrigcue_config::settings::Configuration;
    use openrigcue_devices::motion::thanos::ThanosShared;
    use openrigcue_devices::motion::{DriverKind, LinkStatus, LiveSettings, MotionContext, MotionDriver, MotionError};

    type Frames = Arc<Mutex<Vec<(&'static str, Vec<i64>)>>>;

    #[test]
    fn native_axis_confirmation_reconnect_does_not_wait_for_driver_io() {
        use std::sync::mpsc;
        let state = state("P3");
        let (motion, _) = connected(&state);
        let driver = lock(&motion.drivers);
        let writer = motion.clone();
        let service = state.clone();
        let (send, receive) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            writer.mark_disconnected(&service);
            send.send(()).unwrap();
        });
        let result = receive.recv_timeout(Duration::from_secs(2));
        drop(driver);
        worker.join().unwrap();
        result.expect("writing connection parameters must not wait for driver I/O");
        assert_eq!(motion.link_state().status, LinkStatus::NotConnected);
        assert!(motion.link_state().ports.is_empty());
        assert_eq!(state.motion.read().link.status, LinkStatus::NotConnected);
        assert!(state.motion.read().link.ports.is_empty());
    }

    #[test]
    fn link_state_reads_the_current_shared_driver_write() {
        let motion = MotionHandle::for_settings(&Settings::default(), Box::new(OfflineHost::default()));
        for status in [LinkStatus::Connected, LinkStatus::NotConnectedAfterWriteError, LinkStatus::NotConnected] {
            motion.link.update(|link| link.status = status);
            assert_eq!(motion.link_state().status, status);
        }
    }

    /// A controller that connects at once and records every frame.
    struct RecordingDriver(Frames);

    impl MotionDriver for RecordingDriver {
        fn connect(&mut self, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
            ctx.link.status = LinkStatus::Connected;
            ctx.link.ports.insert(0, "/dev/fake0".into());
            Ok(())
        }
        fn disconnect(&mut self, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
            ctx.link.status = LinkStatus::NotConnected;
            self.0.lock().unwrap().push(("disconnect", Vec::new()));
            Ok(())
        }
        fn send_stream(&mut self, motors: &[i64], _ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
            self.0.lock().unwrap().push(("stream", motors.to_vec()));
            Ok(())
        }
        fn send_test(&mut self, motors: &[i64], _ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
            self.0.lock().unwrap().push(("test", motors.to_vec()));
            Ok(())
        }
        fn describe(&self) -> String {
            "recording".into()
        }
    }

    fn state(model: &str) -> ServiceState {
        let mut config = Configuration::first_run();
        config.settings.motion.model = model.into();
        ServiceState::new(ConfigStore::in_memory(config), OriginalPlatform::Windows)
    }

    fn offline(state: &ServiceState) -> MotionHandle {
        MotionHandle::for_settings(state.config.read().settings(), Box::new(OfflineHost::default()))
    }

    /// A handle whose SMC3 driver is the recording one, already connected.
    fn connected(state: &ServiceState) -> (MotionHandle, Frames) {
        let frames: Frames = Arc::default();
        let settings = state.config.read().settings().clone();
        let mut registry = MotionRegistry::new(drivers::driver_environment(&settings));
        let sink = Arc::clone(&frames);
        registry.register(DriverKind::Smc3, Box::new(move || Box::new(RecordingDriver(Arc::clone(&sink)))));
        let mut host = OfflineHost::default();
        registry.connect(&settings.motion.model, &mut host).unwrap();
        let handle = MotionHandle::new(
            CueingUnit::new(&settings),
            DriverUnit::with_registry(registry, Box::new(host)),
            DriverSettings::default(),
        );
        handle.publish(state, &lock(&handle.drivers), &settings.motion.model, None);
        (handle, frames)
    }

    fn race(state: &ServiceState) {
        *state.telemetry.write() = Telemetry::off();
        let mut session = state.session.write();
        session.listener = Some("dirt".into());
        session.game = Some("dirt4".into());
    }

    #[test]
    fn motion_live_inputs_follow_edits_and_frames_release_configuration_locks() {
        let state = state("P3");
        let motion = offline(&state);
        let live = LiveMotionInputs::new(&state);
        assert_eq!(live.soft_start_manufacturer(), "DOF REALITY");
        assert_eq!(live.soft_start_timer("DOF REALITY"), Ok(4));
        let before = live.gear_smoothing_factor().unwrap();
        motion.with_frame(&state, |_| {
            // These writes deadlock if with_frame retains either read guard during the frame.
            state.config.write().update(|config| {
                config.settings.edit_native(|root| {
                    root.get_mut("DOF REALITY")
                        .unwrap()
                        .as_dict_mut()
                        .unwrap()
                        .insert("soft_start_stop_timer", LegacyValue::from("7"));
                });
            });
            let mut profile = state.profile.write();
            let mut native = profile.native_tune().resolved();
            native.as_dict_mut().unwrap().insert("gear_smoothing_factor", LegacyValue::Int(before as i64 + 1));
            profile.replace_native_tune(native);
            profile.generation += 1;
        });
        assert_eq!(live.soft_start_timer("DOF REALITY"), Ok(7));
        assert_eq!(live.gear_smoothing_factor(), Ok(before + 1));
        state.config.write().update(|config| config.settings.motion.model = "unlisted".into());
        assert_eq!(live.soft_start_manufacturer(), "DOF REALITY", "unknown-model fallback");
    }

    #[test]
    fn native_live_roots_observe_alias_edits_without_revision_or_generation_changes() {
        let state = state("P3");
        let live = LiveMotionInputs::new(&state);
        let _ = live.read_value("MOTIONBOX", "refresh_rate").unwrap();
        let revision = state.config.read().revision();
        let config = state.config.read().native_settings_tree();
        config.item_owned("MOTIONBOX").unwrap().assign_item("refresh_rate", LegacyValue::Int(73)).unwrap();
        assert_eq!(state.config.read().revision(), revision);
        assert_eq!(live.read_value("MOTIONBOX", "refresh_rate").unwrap(), PyValue::Int(73));
        let generation = state.profile.read().generation;
        let mut profile = state.profile.read().native_tune();
        profile.assign_item("motion_max_power", LegacyValue::Int(37)).unwrap();
        assert_eq!(state.profile.read().generation, generation);
        assert_eq!(*live.read_item("motion_max_power").unwrap(), PyValue::Int(37));
    }

    #[test]
    fn native_motor_list_is_published_before_io_and_keeps_partial_edits_on_failure() {
        use std::sync::mpsc;
        struct PartialDriver {
            changed: mpsc::Sender<LegacyValue>,
            resume: mpsc::Receiver<()>,
        }
        impl MotionDriver for PartialDriver {
            fn connect(&mut self, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
                ctx.link.status = LinkStatus::Connected;
                Ok(())
            }
            fn disconnect(&mut self, _: &mut MotionContext<'_>) -> Result<(), MotionError> {
                Ok(())
            }
            fn send_stream(&mut self, _: &[i64], _: &mut MotionContext<'_>) -> Result<(), MotionError> {
                panic!("native output must never enter the bounded compatibility bridge")
            }
            fn send_test(&mut self, _: &[i64], _: &mut MotionContext<'_>) -> Result<(), MotionError> {
                unreachable!()
            }
            fn send_stream_native(
                &mut self,
                motors: &mut LegacyValue,
                _: &mut MotionContext<'_>,
            ) -> Result<(), MotionError> {
                let wide = LegacyValue::Int(i64::MAX).py_mul(&LegacyValue::Int(2)).unwrap();
                motors.assign_index(0, wide).unwrap();
                self.changed.send(motors.clone()).unwrap();
                self.resume.recv().unwrap();
                Err(MotionError::Native("ValueError: private partial-write fixture".into()))
            }
            fn describe(&self) -> String {
                "private partial-write fixture".into()
            }
        }
        let state = state("P3");
        race(&state);
        let settings = state.config.read().settings().clone();
        let (changed, receive) = mpsc::channel();
        let (resume, wait) = mpsc::channel();
        let mut registry = MotionRegistry::new(drivers::driver_environment(&settings));
        let fixture = Arc::new(Mutex::new(Some(PartialDriver { changed, resume: wait })));
        registry.register(DriverKind::Smc3, Box::new(move || Box::new(fixture.lock().unwrap().take().unwrap())));
        let mut host = OfflineHost::default();
        registry.connect("P3", &mut host).unwrap();
        let motion = MotionHandle::new(
            CueingUnit::new(&settings),
            DriverUnit::with_registry(registry, Box::new(host)),
            DriverSettings::default(),
        );
        let worker_state = state.clone();
        let worker = std::thread::spawn(move || motion.stream_frame(&worker_state, Instant::now()));
        let result = receive.recv_timeout(Duration::from_secs(30));
        let seen = result.as_ref().ok().map(|alias| {
            let published = state.motion.read().native_stream.clone().expect("publication before driver");
            (published.same_object(alias), published.get_index(0).unwrap())
        });
        resume.send(()).ok();
        let result = worker.join().unwrap();
        assert!(result.is_err());
        let (same_object, value) = seen.expect("driver entry");
        assert!(same_object);
        assert_eq!(value.integer().unwrap().to_string(), "18446744073709551614");
        assert_eq!(state.motion.read().native_stream.as_ref().unwrap().get_index(0).unwrap(), value);
        assert!(state.motion.read().stream.is_none(), "wide value has no bounded projection");
        assert!(matches!(result, Err(FrameError::Driver(_))));
    }

    #[test]
    #[ignore = "explicit private optimized native service-frame throughput measurement"]
    fn native_service_frame_budget() {
        for model in ["P3", "H6"] {
            let state = state(model);
            race(&state);
            let motion = offline(&state);
            lock(&motion.cueing).live = Some(Arc::new(LiveMotionInputs::new(&state)));
            for _ in 0..5 {
                motion.stream_frame(&state, Instant::now()).unwrap();
            }
            let start = Instant::now();
            for _ in 0..100 {
                motion.stream_frame(&state, Instant::now()).unwrap();
            }
            let elapsed = start.elapsed();
            eprintln!(
                "native service-frame budget {model}: frames=100 elapsed_us={} mean_us={:.3}",
                elapsed.as_micros(),
                elapsed.as_secs_f64() * 10_000.0
            );
        }
    }

    #[test]
    fn production_motion_threads_own_live_helpers_and_release_them_at_shutdown() {
        // No selected model, no real ports, no Yaw VR socket: only the production loop wiring.
        let state = state("None");
        let motion = offline(&state);
        let threads = MotionThreads::start_with(state, motion.clone(), None, |_| yaw_threads::NoYawNet).unwrap();
        let live = {
            let cueing = lock(&motion.cueing);
            Arc::downgrade(cueing.live.as_ref().expect("production starts live smoothing helpers"))
        };
        assert!(live.upgrade().is_some());
        threads.stop();
        assert!(lock(&motion.cueing).live.is_none());
        assert!(live.upgrade().is_none(), "stopped helpers must not retain the live source handles");
    }

    /// Test loop: with a model the test frame is computed even in stand-by; centred sliders give the
    /// centre of the actuator range (P3: 2047). Not connected: nothing sent.
    #[test]
    fn send_test_frame_in_standby() {
        let state = state("P3");
        let motion = offline(&state);
        let frame = motion.test_positions(&state).unwrap().unwrap();
        assert_eq!(frame.0[0], 2047);
        motion.deliver_test(&state, Some(frame)).unwrap();
        assert_eq!(state.motion.read().stream, None);
    }

    /// Test loop: connected, switch on, no game listener: the frame goes to the driver's send_test and
    /// becomes motion_stream; mc_heave is the heave slider / 10.
    #[test]
    fn send_test_delivers_in_standby() {
        let state = state("P3");
        state.controls.write().tests.motion_sliders.heave = 30.0;
        let (motion, frames) = connected(&state);
        let positions = motion.test_positions(&state).unwrap();
        motion.deliver_test(&state, positions).unwrap();
        let recorded = frames.lock().unwrap().clone();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].0, "test");
        assert_eq!(state.motion.read().stream.map(|p| p.to_vec()), Some(recorded[0].1.clone()));
        assert_eq!(state.motion.read().heave_cue, 3.0);
        race(&state);
        motion.deliver_test(&state, positions).unwrap();
        assert_eq!(frames.lock().unwrap().len(), 1, "no test frame while a game listener runs");
    }

    /// Test loop: the motion switch off disconnects and forgets the ports, every pass.
    #[test]
    fn send_test_switch_off_disconnects() {
        let state = state("P3");
        let (motion, frames) = connected(&state);
        state.controls.write().switches.motion = false;
        let positions = motion.test_positions(&state).unwrap();
        motion.deliver_test(&state, positions).unwrap();
        assert_eq!(frames.lock().unwrap().last().unwrap().0, "disconnect");
        let view = state.motion.read();
        assert!(!view.link.is_connected());
        assert!(view.link.ports.is_empty());
    }

    /// Test loop: no frame while MOTIONBOX.model is 'None'.
    #[test]
    fn send_test_without_model() {
        let state = state("None");
        assert_eq!(offline(&state).test_positions(&state).unwrap(), None);
    }

    /// Test loop: a model the model table does not know computes with the `None` model's
    /// values, publishes `motion_stream` and `mc_heave`, then fails with a missing-model error at the send (or at the disconnect
    /// with the switch off), which ends the thread; while not connected with the switch on nothing is looked up.
    #[test]
    fn send_test_l1147_l1149_unknown_model_ends_the_thread() {
        let state = state("P3");
        let (motion, frames) = connected(&state);
        state.config.write().update(|c| c.settings.motion.model = "no such model".into());
        state.controls.write().tests.motion_sliders.heave = 50.0;
        let frame = motion.test_positions(&state).unwrap();
        assert!(frame.is_some(), "computed with the 'None' model's values");
        let failure = motion.deliver_test(&state, frame).unwrap_err();
        assert!(matches!(failure, DriverFailure { error: MotionError::UnknownModel(_), line: 1147 }));
        assert_eq!(
            failure.traceback_from("send_test"),
            "  File \"backend/controllers/motion_helpers.py\", line 1147, in \
             backend.controllers.motion_helpers.send_test\nKeyError: 'no such model'\n"
        );
        assert_eq!(state.motion.read().stream, frame.map(|f| f.0), "motion_stream set before the send");
        assert_eq!(state.motion.read().heave_cue, 5.0);
        state.controls.write().switches.motion = false;
        let failure = motion.deliver_test(&state, frame).unwrap_err();
        assert!(matches!(failure, DriverFailure { error: MotionError::UnknownModel(_), line: 1149 }));
        assert!(frames.lock().unwrap().is_empty());
        state.controls.write().switches.motion = true;
        lock(&motion.drivers).registry.mark_disconnected();
        motion.publish(&state, &lock(&motion.drivers), "P3", None);
        assert!(motion.deliver_test(&state, frame).is_ok());
    }

    /// Stream loop: a model the model table does not know fails with a missing-model error on every frame while a game
    /// listener runs; the loop logs the message and the call trace.
    #[test]
    fn send_stream_l827_unknown_model_raises_every_frame() {
        let state = state("no such model");
        let motion = offline(&state);
        *state.telemetry.write() = Telemetry::off();
        motion.stream_frame(&state, Instant::now()).unwrap();
        race(&state);
        let error = motion.stream_frame(&state, Instant::now()).unwrap_err();
        assert_eq!(
            error.refresh_motionbox_log(),
            "error sending data to motionbox: 'no such model', traceback: Traceback (most recent call last):\n  \
             File \"backend/controllers/controllers.py\", line 418, in \
             backend.controllers.controllers.refresh_motionbox\n  File \"backend/controllers/motion_helpers.py\", \
             line 827, in backend.controllers.motion_helpers.send_stream\nKeyError: 'no such model'\n"
        );
        assert!(motion.stream_frame(&state, Instant::now()).is_err());
    }

    #[test]
    fn native_driver_errors_render_body_in_log_and_class_in_traceback() {
        let error = FrameError::Driver(DriverFailure::at(1044)(MotionError::Native("KeyError: 'scn6_speed'".into())));
        assert_eq!(
            error.refresh_motionbox_log(),
            "error sending data to motionbox: 'scn6_speed', traceback: Traceback (most recent call last):\n  \
             File \"backend/controllers/controllers.py\", line 418, in \
             backend.controllers.controllers.refresh_motionbox\n  File \"backend/controllers/motion_helpers.py\", \
             line 1044, in backend.controllers.motion_helpers.send_stream\nKeyError: 'scn6_speed'\n"
        );
    }

    /// An error a driver hits while handling another error (the Thanos configuration reply parser,
    /// which indexes a short reply again in its fallback) is printed after the trace of the error
    /// that was being handled, by the frame log and by the thread's death alike.
    #[test]
    fn exception_raised_while_handling_another_prints_the_handled_one_first() {
        use openrigcue_devices::motion::PyFrame;
        let at = |function, line| PyFrame::new("backend/controllers/thanos.py", function, line);
        let index_error = || MotionError::Native("IndexError: list index out of range".into());
        let raised = || {
            index_error()
                .in_frames([at("send_test", 1114), at("handle_motor_cmds", 1014), at("AMC.gen_config_dict", 252)])
                .while_handling(index_error().in_frame(at("AMC.gen_config_dict", 250)))
        };
        let handled = "Traceback (most recent call last):\n  File \"backend/controllers/thanos.py\", line 250, in \
                       backend.controllers.thanos.AMC.gen_config_dict\nIndexError: list index out of \
                       range\n\nDuring handling of the above exception, another exception occurred:\n\n";
        let driver = "  File \"backend/controllers/thanos.py\", line 1114, in backend.controllers.thanos.send_test\n  \
                      File \"backend/controllers/thanos.py\", line 1014, in \
                      backend.controllers.thanos.handle_motor_cmds\n  File \"backend/controllers/thanos.py\", line \
                      252, in backend.controllers.thanos.AMC.gen_config_dict\nIndexError: list index out of range\n";
        let failure = DriverFailure::at(1147)(raised());
        assert_eq!(
            thread_death_text("motion_send_test", &failure.handled_traceback(), &failure.traceback_from("send_test")),
            format!(
                "Exception in thread motion_send_test:\n{handled}Traceback (most recent call last):\n  File \
                 \"backend/controllers/motion_helpers.py\", line 1147, in \
                 backend.controllers.motion_helpers.send_test\n{driver}"
            )
        );
        assert_eq!(
            FrameError::Driver(DriverFailure::at(1044)(raised())).refresh_motionbox_log(),
            format!(
                "error sending data to motionbox: list index out of range, traceback: {handled}Traceback (most \
                 recent call last):\n  File \"backend/controllers/controllers.py\", line 418, in \
                 backend.controllers.controllers.refresh_motionbox\n  File \
                 \"backend/controllers/motion_helpers.py\", line 1044, in \
                 backend.controllers.motion_helpers.send_stream\n{driver}"
            )
        );
        // An error with no handled one prints nothing in front.
        assert_eq!(DriverFailure::at(1147)(index_error()).handled_traceback(), "");
    }

    #[test]
    fn driver_frames_follow_the_helper_frame_in_the_logged_traceback() {
        // The stream loop's frame, the stream path's frame, the frames the driver recorded for the
        // error, then the error line. The test loop ends its thread with the same frames.
        use openrigcue_devices::motion::PyFrame;
        let raised = || {
            MotionError::Native("ValueError: invalid literal for int() with base 16: b'ZZ'".into()).in_frames([
                PyFrame::new("backend/controllers/dyadic.py", "send_stream", 404),
                PyFrame::new("backend/controllers/dyadic.py", "send_actuator_pos", 359),
                PyFrame::new("backend/controllers/dyadic.py", "write_velocity", 20),
                PyFrame::new("backend/controllers/pytbmscom.py", "TMBSCOMProtocol.write_velocity", 673),
                PyFrame::new("backend/controllers/pytbmscom.py", "TMBSCOMProtocol.parseADirectResponseFormat", 217),
            ])
        };
        let driver =
            "  File \"backend/controllers/dyadic.py\", line 404, in backend.controllers.dyadic.send_stream\n  \
                      File \"backend/controllers/dyadic.py\", line 359, in \
                      backend.controllers.dyadic.send_actuator_pos\n  File \"backend/controllers/dyadic.py\", line 20, \
                      in backend.controllers.dyadic.write_velocity\n  File \"backend/controllers/pytbmscom.py\", line \
                      673, in backend.controllers.pytbmscom.TMBSCOMProtocol.write_velocity\n  File \
                      \"backend/controllers/pytbmscom.py\", line 217, in \
                      backend.controllers.pytbmscom.TMBSCOMProtocol.parseADirectResponseFormat\n";
        let exception = "ValueError: invalid literal for int() with base 16: b'ZZ'\n";
        let error = FrameError::Driver(DriverFailure::at(1044)(raised()));
        assert_eq!(
            error.refresh_motionbox_log(),
            format!(
                "error sending data to motionbox: invalid literal for int() with base 16: b'ZZ', traceback: Traceback \
                 (most recent call last):\n  File \"backend/controllers/controllers.py\", line 418, in \
                 backend.controllers.controllers.refresh_motionbox\n  File \"backend/controllers/motion_helpers.py\", \
                 line 1044, in backend.controllers.motion_helpers.send_stream\n{driver}{exception}"
            )
        );
        assert_eq!(
            DriverFailure::at(1147)(raised()).traceback_from("send_test"),
            format!(
                "  File \"backend/controllers/motion_helpers.py\", line 1147, in \
                 backend.controllers.motion_helpers.send_test\n{driver}{exception}"
            )
        );
    }

    #[test]
    fn native_settings_errors_reach_motion_without_typed_fallbacks() {
        let state = state("P3");
        state.config.write().update(|configuration| {
            let mut raw = configuration.settings.native_snapshot();
            raw.get_mut("CUSTOM_FEATURES")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .insert("enable_legacy_gear_smoothing", "maybe".into());
            configuration.settings = Settings::from_legacy(&raw).0;
        });
        race(&state);
        let FrameError::Cue(error) = offline(&state).stream_frame(&state, Instant::now()).unwrap_err() else {
            panic!("expected a cueing exception");
        };
        assert_eq!(error.to_string(), "Boolean value expected. Got maybe");
        assert_eq!(
            error.frames().map(|frame| (frame.function, frame.line)).collect::<Vec<_>>(),
            [("send_stream", 785), ("str2bool", 261)]
        );
    }

    #[test]
    fn native_thanos_spike_frame_refresh_preserves_stored_profile_values() {
        use openrigcue_model::native_value::LegacyValue;
        let state = state("P3");
        {
            let mut profile = state.profile.write();
            let mut raw = profile.native_tune().resolved();
            let fields = raw.as_dict_mut().unwrap();
            fields.insert("thanos_spike_filter_horizontal", "600".into());
            fields.insert("thanos_spike_filter_vertical", 1000.into());
            fields.insert("unrelated_thanos_fixture", 73.into());
            profile.replace_native_tune(raw);
        }
        let motion = offline(&state);
        motion.with_frame(&state, |_| {});
        let native = motion.driver_settings().thanos.get().native_spike_filters.unwrap();
        assert_eq!(native.values, Ok([LegacyValue::from("600"), LegacyValue::Int(1000)]));
        // Frame publication retains both raw operands. The driver owns calculation
        // after its current-controller/configuration gate; no arithmetic is performed here.
        assert_eq!(
            state.profile.read().native_tune().item_owned("unrelated_thanos_fixture").unwrap(),
            LegacyValue::Int(73)
        );
        assert!(state.profile.read().native_thanos_tune().item_owned("unrelated_thanos_fixture").is_err());
        {
            let mut profile = state.profile.write();
            let mut raw = profile.native_tune().resolved();
            raw.as_dict_mut().unwrap().remove("thanos_spike_filter_horizontal");
            raw.as_dict_mut().unwrap().remove("thanos_spike_filter_vertical");
            profile.replace_native_tune(raw);
        }
        motion.with_frame(&state, |_| {});
        assert_eq!(
            motion.driver_settings().thanos.get().native_spike_filters.unwrap().values,
            Err("KeyError: 'thanos_spike_filter_horizontal'".into())
        );
        {
            let mut profile = state.profile.write();
            let mut raw = profile.native_tune().resolved();
            raw.as_dict_mut().unwrap().insert("thanos_spike_filter_horizontal", 600.into());
            profile.replace_native_tune(raw);
        }
        motion.with_frame(&state, |_| {});
        assert_eq!(
            motion.driver_settings().thanos.get().native_spike_filters.unwrap().values,
            Err("KeyError: 'thanos_spike_filter_vertical'".into())
        );
        state.profile.write().replace_native_tune(LegacyValue::None);
        motion.with_frame(&state, |_| {});
        assert_eq!(
            motion.driver_settings().thanos.get().native_spike_filters.unwrap().values,
            Err("TypeError: 'NoneType' object is not subscriptable".into())
        );
    }

    #[test]
    fn native_simtech_inputs_frame_refresh_keeps_raw_profile_failures() {
        use openrigcue_model::native_value::LegacyValue;
        let state = state("P3");
        {
            let mut profile = state.profile.write();
            let mut raw = profile.native_tune().resolved();
            let fields = raw.as_dict_mut().unwrap();
            fields.insert("scn6_speed", LegacyValue::None);
            fields.remove("scn6_acceleration");
            fields.insert("unrelated_fixture", "kept outside the selected input".into());
            profile.replace_native_tune(raw);
        }
        let motion = offline(&state);
        motion.with_frame(&state, |frame| {
            assert!(frame.driver_runtime().simtech_tune.item("unrelated_fixture").is_err());
        });
        let values = motion.driver_settings().simtech.get();
        // The driver converts at the point of use, after its
        // speed/control gates. Refresh must retain the actual empty operand.
        assert_eq!(values.scn6_speed, Ok(LegacyValue::None));
        assert_eq!(values.scn6_acceleration.unwrap_err(), "KeyError: 'scn6_acceleration'");
        state.profile.write().replace_native_tune(LegacyValue::None);
        motion.with_frame(&state, |_| {});
        assert_eq!(
            motion.driver_settings().simtech.get().scn6_speed.unwrap_err(),
            "TypeError: 'NoneType' object is not subscriptable"
        );
    }

    #[test]
    fn native_profile_missing_and_none_errors_are_not_filled_from_factory_defaults() {
        let state = state("P3");
        race(&state);
        let motion = offline(&state);
        {
            let mut profile = state.profile.write();
            let mut raw = profile.native_tune().resolved();
            let fields = raw.as_dict_mut().unwrap();
            fields.remove("motion_max_horizontal");
            profile.replace_native_tune(raw);
            profile.generation += 1;
        }
        motion.stream_frame(&state, Instant::now()).unwrap();
        {
            let mut profile = state.profile.write();
            let mut raw = profile.native_tune().resolved();
            raw.as_dict_mut().unwrap().remove("motion_max_power");
            profile.replace_native_tune(raw);
            profile.generation += 1;
        }
        let FrameError::Cue(error) = motion.stream_frame(&state, Instant::now()).unwrap_err() else {
            panic!("expected a cueing exception");
        };
        assert_eq!(error.to_string(), "'motion_max_power'");
        assert_eq!(
            error.frames().map(|frame| (frame.function, frame.line)).collect::<Vec<_>>(),
            [("send_stream", 854), ("apply_max_power", 477)]
        );
        {
            let mut profile = state.profile.write();
            profile.replace_native_tune(LegacyValue::None);
            profile.generation += 1;
        }
        let FrameError::Cue(error) = motion.stream_frame(&state, Instant::now()).unwrap_err() else {
            panic!("expected a cueing exception");
        };
        assert_eq!(error.exception(), openrigcue_cueing::Exception::TypeError);
        assert_eq!(error.to_string(), "'NoneType' object is not subscriptable");
    }

    #[test]
    fn smc3_native_import_failure_precedes_later_motion_imports() {
        let mut settings = Configuration::first_run().settings;
        settings.edit_native(|root| {
            root.get_mut("DOF REALITY").unwrap().as_dict_mut().unwrap().insert("left_debug_port", "20017.0".into());
        });
        let directory = tempfile::tempdir().unwrap();
        let handle = MotionHandle::import(
            &settings,
            directory.path(),
            Box::new(OfflineHost::default()),
            Arc::new(str::to_owned),
        );
        assert!(!handle.available, "SMC3 int() failure prevents motion_helpers import");
        assert!(!handle.yaw_imported, "SMC3 failure occurs before the Yaw module import");
        let error = CueingUnit::import(&settings, directory.path()).err().unwrap();
        assert!(error.contains("line 16, in init backend.controllers.motion_helpers"));
        assert!(error.contains("line 26, in init backend.controllers.smc3"));
        assert!(error.contains("invalid literal for int() with base 10: '20017.0'"));
        // The early return must precede both the export file and the supplied network constructor.
        let threads =
            MotionThreads::start_with(state("P3"), handle, Some(directory.path()), |_| -> yaw_threads::NoYawNet {
                panic!("unreached Yaw import cannot construct a network or worker")
            })
            .unwrap();
        assert!(threads.handles.is_empty() && threads.yaw_handles.is_empty());
        assert!(!directory.path().join("YawVRGEFile").exists());
    }

    #[test]
    fn smc3_native_import_keeps_earlier_external_configuration_failure() {
        let mut settings = Configuration::first_run().settings;
        settings.edit_native(|root| {
            root.get_mut("DOF REALITY").unwrap().as_dict_mut().unwrap().insert("left_debug_port", "bad-port".into());
        });
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("motion_deadzone.csv"), "\n").unwrap();
        let error = CueingUnit::import(&settings, directory.path()).err().unwrap();
        assert!(error.contains("line 1 is empty"), "{error}");
        assert!(!error.contains("bad-port"), "external motion config import precedes SMC3");
        let handle = MotionHandle::import(
            &settings,
            directory.path(),
            Box::new(OfflineHost::default()),
            Arc::new(str::to_owned),
        );
        assert!(!handle.available && !handle.yaw_imported);
    }

    #[test]
    fn invalid_native_clip_factor_fails_the_motion_import() {
        let mut raw = Configuration::first_run().settings.native_snapshot();
        raw.get_mut("MOTIONBOX").unwrap().as_dict_mut().unwrap().insert("pitch_clip_factor", "bad".into());
        let settings = Settings::from_legacy(&raw).0;
        let tmp = tempfile::tempdir().unwrap();
        let error = CueingUnit::import(&settings, tmp.path()).err().expect("raw clip value must raise at import");
        assert!(error.contains("line 60"), "{error}");
        assert!(error.contains("could not convert string to float: 'bad'"), "{error}");
        let handle =
            MotionHandle::import(&settings, tmp.path(), Box::new(OfflineHost::default()), Arc::new(str::to_owned));
        assert!(!handle.available, "failed import leaves motion unavailable");
        assert!(handle.yaw_imported, "later clip failure retains the prior Yaw module import");
    }

    /// Stream loop: a NaN field of the shared telemetry becomes 0 there, before the stream frame, so
    /// mc_heave is the cleaned vertical acceleration.
    #[test]
    fn refresh_motionbox_l405_l408_clears_nan_in_the_global_telemetry() {
        let state = state("P3");
        let (motion, _frames) = connected(&state);
        race(&state);
        state.telemetry.write().set_native("spotter_right", LegacyValue::Int(0));
        state.telemetry.write().set_native("vertical_acceleration", LegacyValue::Float(f64::NAN));
        state.config.write().update(|c| c.settings.motion.refresh_rate = 1000);
        assert!(stream_pass(&state, &motion).is_some());
        assert_eq!(state.telemetry.read().vertical_acceleration, 0.0);
        assert_eq!(state.motion.read().heave_cue, 0.0);
    }

    #[test]
    fn live_nan_scan_keeps_prefix_assignments_and_actual_exception_frame() {
        let state = state("P3");
        *state.telemetry.write() = Telemetry::unfilled();
        state.telemetry.write().set_native("roll", LegacyValue::Float(f64::NAN));
        state.telemetry.write().set_native("pitch", LegacyValue::None);
        state.telemetry.write().set_native("yaw", LegacyValue::Float(f64::NAN));
        let error = clear_live_telemetry_nan(&state).unwrap_err();
        assert_eq!(state.telemetry.read().native_value("roll"), Some(&LegacyValue::Int(0)));
        assert!(state.telemetry.read().yaw.is_nan());
        assert_eq!(error.exception(), openrigcue_cueing::Exception::TypeError);
        assert_eq!(error.to_string(), "must be real number, not NoneType");
        assert_eq!(cue_frames_and_exception(&error), "  File \"backend/controllers/controllers.py\", line 406, in backend.controllers.controllers.refresh_motionbox\nTypeError: must be real number, not NoneType\n");
    }

    #[test]
    fn motion_waits_for_telemetry_before_sending_or_reading_the_refresh_rate() {
        let state = state("P3");
        let (motion, frames) = connected(&state);
        race(&state);
        *state.telemetry.write() = Telemetry::unfilled();
        state.telemetry.write().set_native("vertical_acceleration", LegacyValue::Float(f64::NAN));
        state.config.write().update(|c| c.settings.motion.refresh_rate = 0);
        assert_eq!(stream_pass(&state, &motion), Some(Duration::from_millis(500)));
        assert!(state.telemetry.read().vertical_acceleration.is_nan());
        assert!(lock(&frames).is_empty());

        // A first channel publication makes the key available. The next probe
        // now reaches the NaN scan, send and invalid-rate error in that order.
        *state.telemetry.write() = Telemetry::off();
        state.telemetry.write().set_native("vertical_acceleration", LegacyValue::Float(f64::NAN));
        assert_eq!(stream_pass(&state, &motion), None);
        assert_eq!(state.telemetry.read().vertical_acceleration, 0.0);
        assert!(!lock(&frames).is_empty());
    }

    /// Stream frame: while the capture switch is on, every frame of a game listener goes to the SMC3
    /// driver's debug hook, connected or not: the UDP mirror to 127.0.0.1:left/right_debug_port.
    #[test]
    fn send_stream_l1050_debug_status_mirrors_every_frame() {
        let state = state("P3");
        let host = OfflineHost::default();
        let sent = Arc::clone(&host.datagrams);
        let motion = MotionHandle::for_settings(state.config.read().settings(), Box::new(host));
        race(&state);
        motion.stream_frame(&state, Instant::now()).unwrap();
        assert!(sent.lock().unwrap().is_empty());
        state.controls.write().telemetry.capture = true;
        motion.stream_frame(&state, Instant::now()).unwrap();
        let targets: Vec<String> = sent.lock().unwrap().iter().map(ToString::to_string).collect();
        assert_eq!(targets, ["127.0.0.1:20017", "127.0.0.1:20018"]);
    }

    /// Stream frame: no frame without a game listener; with one, the frame reaches the driver
    /// when connected with the motion switch on, and mc_heave is the vertical acceleration.
    #[test]
    fn send_stream_delivery_rules() {
        let state = state("P3");
        let (motion, frames) = connected(&state);
        *state.telemetry.write() = Telemetry::off();
        motion.stream_frame(&state, Instant::now()).unwrap();
        assert!(frames.lock().unwrap().is_empty());
        race(&state);
        state.telemetry.write().vertical_acceleration = 0.4;
        motion.stream_frame(&state, Instant::now()).unwrap();
        assert_eq!(frames.lock().unwrap().len(), 1);
        assert_eq!(frames.lock().unwrap()[0].0, "stream");
        assert_eq!(state.motion.read().heave_cue, 0.4);
        state.controls.write().switches.motion = false;
        motion.stream_frame(&state, Instant::now()).unwrap();
        assert_eq!(frames.lock().unwrap().len(), 1, "motion switch off: nothing sent");
    }

    /// Without a connection the stream frame is computed but not delivered.
    #[test]
    fn send_stream_not_connected() {
        let state = state("P3");
        let motion = offline(&state);
        race(&state);
        motion.stream_frame(&state, Instant::now()).unwrap();
        assert_eq!(state.motion.read().stream, None);
    }

    /// Connection scan: the accessory ports and allowed_com_ports reach the host before the motion connect;
    /// with no controller present nothing connects.
    #[test]
    fn connect_pass_sets_port_rules() {
        let state = state("P3");
        state.config.write().update(|c| c.settings.app.allowed_serial_ports = vec!["/dev/ttyUSB7".into()]);
        let motion = offline(&state);
        let claimed = ClaimedPorts { belt: Some("/dev/ttyACM0".into()), ..ClaimedPorts::default() };
        motion.connect_pass(&state, claimed.clone());
        let drivers = lock(&motion.drivers);
        assert_eq!(drivers.host.claimed_ports(), claimed);
        assert_eq!(drivers.host.allowed_com_ports(), "/dev/ttyUSB7");
        assert!(!state.motion.read().link.is_connected());
    }

    /// A Thanos `connect` that reaches its connection check (the stroke write into
    /// `THANOS.vertical_actuators_stroke`) and then fails, as the verification can.
    struct StrokeWritingDriver(LiveSettings<ThanosShared>, LegacyValue);

    impl MotionDriver for StrokeWritingDriver {
        fn connect(&mut self, _ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
            let value = self.1.clone();
            self.0.update(|shared| shared.vertical_stroke = Some(("THANOS".into(), value)));
            Err(MotionError::Native("ValueError: verification".into()))
        }
        fn disconnect(&mut self, _ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
            Ok(())
        }
        fn send_stream(&mut self, _motors: &[i64], _ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
            Ok(())
        }
        fn send_test(&mut self, _motors: &[i64], _ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
            Ok(())
        }
        fn describe(&self) -> String {
            "stroke".into()
        }
    }

    /// The configuration assignment in the connection check happens before the step that fails, so it stays made;
    /// the value is what `* 10` produced (a number, or the controller's text ten times), and only once.
    #[test]
    fn thanos_stroke_write_reaches_the_configuration_even_when_connect_then_fails() {
        for written in [LegacyValue::Int(150), LegacyValue::Str("ab".repeat(10))] {
            let state = state("4 Actuators [2 Front + 2 Rear]");
            state.controls.write().switches.motion = true;
            let settings = state.config.read().settings().clone();
            assert!(settings.motion.auto_search);
            let driver_settings = DriverSettings::default();
            let mut registry = MotionRegistry::new(drivers::driver_environment(&settings));
            let (shared, value) = (driver_settings.thanos_shared.clone(), written.clone());
            registry.register(
                DriverKind::Thanos,
                Box::new(move || Box::new(StrokeWritingDriver(shared.clone(), value.clone()))),
            );
            let motion = MotionHandle::new(
                CueingUnit::new(&settings),
                DriverUnit::with_registry(registry, Box::new(OfflineHost::default())),
                driver_settings,
            );
            let stroke = |state: &ServiceState| {
                let snapshot = state.config.read().settings().native_snapshot();
                snapshot
                    .get("THANOS")
                    .and_then(|s| s.as_dict())
                    .and_then(|s| s.get("vertical_actuators_stroke"))
                    .cloned()
            };
            assert_ne!(stroke(&state), Some(written.clone()));
            motion.connect_step(&state, ClaimedPorts::default());
            assert!(!state.motion.read().link.is_connected());
            assert_eq!(stroke(&state), Some(written.clone()));
            assert_eq!(motion.driver_settings().thanos_shared.get().vertical_stroke, None, "taken once");
            let revision = state.config.read().revision();
            motion.driver_settings().thanos_shared.update(|shared| shared.vertical_stroke = None);
            motion.connect_pass(&state, ClaimedPorts::default());
            assert_eq!(state.config.read().revision(), revision + 1, "the new pass wrote its own stroke once");
        }
    }

    /// Listener and game kind mapping of the stream frame's inputs.
    #[test]
    fn stream_inputs() {
        assert_eq!(listener("none"), Listener::None);
        assert_eq!(listener("dirt"), Listener::Game);
        assert_eq!(game_kind("dcs"), GameKind::Other);
        assert_eq!(game_kind("acc"), GameKind::Racing);
        assert_eq!(game_kind("unknown"), GameKind::Racing);
        assert_eq!(model_is_serial("P3"), Some(true));
        assert_eq!(model_is_serial("nonsense"), None);
    }

    /// SMC3 start-up: the debug ports and the previous reaction speed come from the configuration
    /// at start-up, so the SMC3 settings are filled before the driver table (whose SMC3 driver reads them) is
    /// built. Under the Windows rules: no pre-connection reset, exclusive opens.
    #[test]
    fn smc3_settings_are_read_before_the_driver_table_is_built() {
        let mut config = Configuration::first_run();
        config.settings.dof_reality.left_debug_port = 30_017;
        config.settings.motion.reaction_speed = 80;
        let motion = MotionHandle::for_settings(&config.settings, Box::new(OfflineHost::default()));
        let smc3 = motion.driver_settings().smc3.get();
        assert_eq!((smc3.left_debug_port, smc3.reaction_speed), (30_017, 80));
        assert!(!smc3.pre_connection_reset, "the Windows build has no pre-connection reset");
        assert!(smc3.exclusive_open, "COM ports open exclusively");
    }

    /// The thread-ending errors of an unusable refresh rate (stream loop and test loop).
    #[test]
    fn refresh_rate_exceptions() {
        assert_eq!(
            refresh_rate_exception(0, false),
            "  File \"backend/controllers/controllers.py\", line 451, in backend.controllers.controllers.refresh_motionbox\n  \
             File \"backend/controllers/controllers.py\", line 392, in backend.controllers.controllers.get_refresh_rate\n\
             ZeroDivisionError: float division by zero\n"
        );
        assert_eq!(
            refresh_rate_exception(-5, true),
            "  File \"backend/controllers/motion_helpers.py\", line 1141, in backend.controllers.motion_helpers.send_test\n\
             ValueError: sleep length must be non-negative\n"
        );
    }

    /// The cueing start-up without CSV tables: the built-in tables select a listed model.
    #[test]
    fn import_of_the_motion_module() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Configuration::first_run().settings;
        let unit = CueingUnit::import(&settings, dir.path()).unwrap();
        assert!(unit.config.select("P3").unwrap().is_some());
        assert!(
            MotionHandle::import(&settings, dir.path(), Box::new(OfflineHost::default()), Arc::new(str::to_owned))
                .available
        );
    }

    /// The Yaw VR export at start-up: the zeroed `YawVRGEFile` block is there while the loops run, whatever model is
    /// selected, and goes away with them.
    #[test]
    fn yaw_vr_block_lives_with_the_motion_threads() {
        let dir = tempfile::tempdir().unwrap();
        let block = dir.path().join("YawVRGEFile");
        for state in [state("None"), state("P3")] {
            let threads =
                MotionThreads::start_with(state.clone(), offline(&state), Some(dir.path()), |_| yaw_threads::NoYawNet)
                    .unwrap();
            assert_eq!(std::fs::read(&block).unwrap(), [0; 17]);
            threads.stop();
            assert!(!block.exists());
        }
    }
}
