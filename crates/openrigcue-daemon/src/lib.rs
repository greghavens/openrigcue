//! The openrigcue service: owns sources, motion and effects loops and serves the web UI.
//!
//! [`Service::start`] is the start-up sequence: the configuration is loaded (first-run
//! defaults when there is none), the web server starts, then the telemetry threads, the motion loops, the shaker
//! loop, the accessory threads and the input devices; the UI main loop starts with the UI application.
//! [`Service::goodbye`] ends the process at once
//! (no loop is joined and the platform is not parked).

pub mod accessories;
pub mod api;
mod audio;
pub mod compensation;
pub mod device_access;
pub mod export_csv;
pub mod games;
pub mod help_pages;
pub mod hotkeys;
pub mod hud;
pub mod i18n;
pub mod joystick;
pub mod listeners;
pub mod motion;
pub mod notifications;
pub mod profile;
pub mod runtime;
pub mod shaker_layout;
pub mod shakers;
pub mod sound_devices;
pub mod state;
pub mod store;
pub mod telemetry;
pub mod voice;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use openrigcue_config::settings::paths::ConfigDir;
use openrigcue_effects::accessories::dispatcher::{AccessoryThreads, Scanners};
use openrigcue_effects::accessories::firmware::IntelliboxFlasher;
use openrigcue_effects::accessories::link::SystemClock;
use openrigcue_effects::accessories::{belt::BeltScanner, hapticr::HapticScanner, iflag::IFlagScanner};
use openrigcue_effects::accessories::{intellibox::IntelliboxScanner, sim3d::Sim3dScanner};
use openrigcue_sources::api::JoystickInputs;
use openrigcue_sources::derive::Dispatcher;

use crate::accessories::{scan_wake, start_pose_log, PortCache, ServiceFeed};
use crate::api::{ApiContext, ApiServer};
use crate::games::GameSetup;
use crate::listeners::{ListenerInputs, SourceContext, SourceOutputs};
use crate::motion::{MotionHandle, MotionThreads};
use crate::profile::{MainLoop, MainLoopHandle};
use crate::runtime::Shared;
use crate::shakers::{ShakerThread, ShakersHandle};
use crate::state::{OriginalPlatform, ServiceState};
use crate::store::ConfigStore;
use crate::telemetry::{system_titles, TelemetryHandle, TelemetryParts, TelemetryThreads};

/// How the service runs.
#[derive(Debug, Clone)]
pub struct ServiceOptions {
    /// The configuration directory (`~/.config/openrigcue`).
    pub config_dir: ConfigDir,
    /// A profile file the UI imports at its start (`--profile-path`).
    pub profile_path: Option<PathBuf>,
    /// The user's SRS installation (directory or installer): only the optional imports read it ("Use SRS Sound
    /// Samples": shaker samples, beep, FMOD banks, voice clips) and the firmware binaries OpenRigCue cannot ship
    /// (Intellibox `.hex`, OBD2HUD `.bin`). Translations, help pages and game files are built in.
    pub srs_install: Option<PathBuf>,
    /// The home directory Steam is looked for in.
    pub home: PathBuf,
    /// `/proc`, where processes (game detection) and sockets (port owners) are read.
    pub proc_root: PathBuf,
    /// Where the bridge mirrors live and the rig pose is written (`/dev/shm/openrigcue`).
    pub mirror_dir: PathBuf,
    /// Where the TCP side of the bridge's pipe relay listens (`127.0.0.1:33010`, the bridge's `--pipe-tcp-port`
    /// default); `None` serves the pipes on their Unix sockets only.
    pub pipe_relay: Option<SocketAddr>,
    /// The runtime directory below which the pipe relay's Unix sockets are bound (`$XDG_RUNTIME_DIR`, giving
    /// `$XDG_RUNTIME_DIR/openrigcue/pipe-<name>`).
    pub runtime_dir: PathBuf,
    /// What the process running the service provides: live log level, restart.
    pub hooks: ServiceHooks,
}

/// Process-level actions the UI can trigger.
#[derive(Clone, Default)]
pub struct ServiceHooks {
    /// Applies `APP.log_level` at start and whenever it changes.
    pub log_level: Option<api::ui_state::LogLevelHook>,
    /// Stops and starts the service again (used by the factory reset).
    pub restart: Option<api::ui_state::RestartHook>,
    pub geometry: Option<api::ui_state::GeometryHook>,
}

impl std::fmt::Debug for ServiceHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceHooks")
            .field("log_level", &self.log_level.is_some())
            .field("restart", &self.restart.is_some())
            .field("geometry", &self.geometry.is_some())
            .finish()
    }
}

/// Folder in the configuration directory that holds the files copied from the user's SRS installer or
/// installation, laid out like an installation (`sounds/`, `firmware/`).
pub const SRS_IMPORT_DIR: &str = "srs-import";

/// The import an earlier start made under `config_root`, when there is one.
pub fn imported_srs_install(config_root: &std::path::Path) -> Option<PathBuf> {
    let dir = config_root.join(SRS_IMPORT_DIR);
    dir.is_dir().then_some(dir)
}

/// The import's failure for a source without SRS files, as a sentence with its value ([`i18n::sentence`]): the
/// error of [`import_srs_install`] holds its English text, the Import button's route hands the sentence to the UI.
pub(crate) fn no_srs_files(source: &std::path::Path) -> String {
    i18n::sentence("no SRS sound or firmware files found in ^folder^", &[("folder", &source.display().to_string())])
}

/// Copy the sounds (samples, shift beep, engine banks, voice clips) and every firmware image of `source` (an SRS
/// installer or installation folder) into the configuration directory, so that every later start finds them
/// without `source`. Returns the import folder.
pub fn import_srs_install(source: &std::path::Path, config_root: &std::path::Path) -> std::io::Result<PathBuf> {
    use std::path::{Component, Path};
    fn store(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        if std::fs::read(path).ok().as_deref() == Some(bytes) {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, bytes)
    }
    fn copy_tree(from: &Path, to: &Path, copied: &mut usize) -> std::io::Result<()> {
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            let (path, target) = (entry.path(), to.join(entry.file_name()));
            if path.is_dir() {
                copy_tree(&path, &target, copied)?;
            } else if path.is_file() {
                store(&target, &std::fs::read(&path)?)?;
                *copied += 1;
            }
        }
        Ok(())
    }
    let target = config_root.join(SRS_IMPORT_DIR);
    let mut copied = 0;
    if source.is_file() {
        let archive = openrigcue_effects::shakers::nsis::NsisArchive::open(std::fs::read(source)?)
            .map_err(std::io::Error::other)?;
        let wanted: Vec<String> = archive
            .paths()
            .filter(|path| {
                let lower = path.to_ascii_lowercase();
                (lower.starts_with("sounds/") || lower.starts_with("firmware/"))
                    && Path::new(path).components().all(|part| matches!(part, Component::Normal(_)))
            })
            .map(str::to_owned)
            .collect();
        for path in wanted {
            store(&target.join(&path), &archive.read(&path).map_err(std::io::Error::other)?)?;
            copied += 1;
        }
    } else {
        // An installation folder, its `sounds` folder, or its `sounds/voices` folder.
        let nested = source.join("sounds");
        if nested.is_dir() {
            copy_tree(&nested, &target.join("sounds"), &mut copied)?;
        } else if source.file_name().is_some_and(|name| name == "voices") {
            copy_tree(source, &target.join("sounds").join("voices"), &mut copied)?;
        } else if !source.join("firmware").is_dir() {
            copy_tree(source, &target.join("sounds"), &mut copied)?;
        }
        let firmware = source.join("firmware");
        if firmware.is_dir() {
            copy_tree(&firmware, &target.join("firmware"), &mut copied)?;
        }
    }
    if copied == 0 {
        return Err(std::io::Error::new(std::io::ErrorKind::NotFound, crate::i18n::plain(&no_srs_files(source))));
    }
    Ok(target)
}

impl ServiceOptions {
    /// The defaults for the current user, or `None` when neither `$XDG_CONFIG_HOME` nor `$HOME` is set.
    pub fn for_user() -> Option<ServiceOptions> {
        Some(ServiceOptions {
            config_dir: ConfigDir::from_env()?,
            profile_path: None,
            srs_install: None,
            home: std::env::var_os("HOME").map(PathBuf::from)?,
            proc_root: PathBuf::from("/proc"),
            mirror_dir: PathBuf::from(openrigcue_sources::shared_memory::mapping::MIRROR_DIR),
            pipe_relay: Some(openrigcue_sources::process_memory::pipe_relay::default_tcp_address()),
            runtime_dir: openrigcue_sources::process_memory::pipe_relay::runtime_dir(),
            hooks: ServiceHooks::default(),
        })
    }
}

/// The bridge's default `--pipe-tcp-port` (33010), where [`ServiceOptions::pipe_relay`] listens by default.
pub const PIPE_RELAY_TCP_PORT: u16 = openrigcue_sources::process_memory::pipe_relay::RELAY_TCP_PORT;

/// The [`ServiceOptions::pipe_relay`] address for a bridge started with `--pipe-tcp-port <port>`: the bridge
/// connects to `127.0.0.1:<port>`.
pub fn pipe_relay_address(port: u16) -> SocketAddr {
    SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port))
}

/// Why the service did not start.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("configuration: {0}")]
    Config(#[from] openrigcue_config::StoreError),
    /// The configuration files could not be read at start-up (see [`store::ConfigStore::open`]).
    #[error("{0}")]
    ConfigStart(#[from] openrigcue_config::settings::StartFailure),
    /// A `[NETWORK_PORT]` key a game configuration module needs at start-up is missing.
    #[error("game configuration modules: {0}")]
    GameModules(#[from] openrigcue_config::patch::PatchError),
    #[error("port cache: {0}")]
    PortCache(#[from] crate::accessories::PortCacheError),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// The running service. Its loops run until the process ends ([`Service::goodbye`]); the fields keep them.
pub struct Service {
    state: ServiceState,
    api: ApiServer,
    port_cache: PortCache,
    _telemetry: TelemetryThreads,
    _hardware: HardwareThreads,
    _pipe_relay: Option<openrigcue_sources::process_memory::pipe_relay::TcpRelay>,
    /// The telemetry export thread (Telemetry page "Export" switch).
    _export: crate::export_csv::ExportThread,
    /// The sound-device monitor (shaker sound-card notifications).
    _sound_devices: Option<crate::sound_devices::DeviceMonitor>,
    /// The input device loops (hotkeys, controller buttons).
    _hotkeys: hotkeys::HotkeyThreads,
    /// The OBD2HUD thread.
    _hud: Option<openrigcue_effects::accessories::hud::HudThread>,
}

/// The loops that talk to devices.
struct HardwareThreads {
    _motion: MotionThreads,
    _shakers: ShakerThread,
    _accessories: AccessoryThreads,
    _scan_stop: Arc<AtomicBool>,
    /// The VR rig-pose publisher (`vr_rig_pose`).
    _rig_pose: crate::compensation::RigPoseThread,
}

/// The joystick mapping thread reads the selected profile only after its
/// enabled check. Dispatcher status/poll refreshes must not perform that read.
pub(crate) fn joystick_runtime_reader(
    state: ServiceState,
    selected: Shared<String>,
) -> openrigcue_sources::api::joystick::JoystickRuntimeReader {
    let metadata_state = state.clone();
    let feature_state = state.clone();
    openrigcue_sources::api::joystick::JoystickRuntimeReader::with_profile_lookup(
        move || openrigcue_sources::api::joystick::JoystickRuntime {
            enabled: state.controls.read().switches.joystick,
            name: selected.read().clone(),
            profile: None,
        },
        move |name| metadata_state.config.read().metadata_row("joystick", name).map_err(|error| error.to_string()),
    )
    .with_feature_lookup(move |key| {
        feature_state
            .config
            .read()
            .native_settings_tree()
            .item_owned("CUSTOM_FEATURES")
            .and_then(|features| features.item_owned(key))
            .ok()
    })
}

impl Service {
    /// Starts everything (see the crate documentation for the order).
    pub fn start(options: ServiceOptions) -> Result<Service, StartError> {
        let platform = OriginalPlatform::Windows;
        // A configuration migration step re-patches DCS where the game is installed.
        let mut store = GameSetup::locate(&options.home).migration_context(|patch| {
            ConfigStore::open(options.config_dir.clone(), platform.config_platform(), Some(patch))
        })?;
        // A new configuration starts in the session's language when it is one of the UI languages.
        if store.first_installation() {
            if let Some(language) = crate::i18n::session_language(|name| std::env::var(name).ok()) {
                store.update(|config| config.settings.app.language = language);
                store.write_settings_native();
            }
        }
        let cache_database = store.port_cache_database()?;
        let mut state = ServiceState::new(store, platform);
        state.profile_path.clone_from(&options.profile_path);
        let documents = options.config_dir.root().to_path_buf();
        let port_cache = PortCache::from_open_table(
            &options.config_dir.metadata_file(),
            cache_database,
            Some(&documents.join(crate::accessories::PORT_CACHE_FILE)),
        )?;
        let host_ip = games::host_ip();

        // Game configuration uses only OpenRigCue's built-in files; it never reads an SRS installation.
        let setup = GameSetup::start(&options.home, state.config.read().settings())?;

        let joystick_inputs = JoystickInputs::default();
        let joystick_profile = Shared::new(String::from(openrigcue_sources::api::joystick::page::DEFAULT_PROFILE));
        let context: Shared<SourceContext> = Shared::default();
        let outputs: Shared<SourceOutputs> = Shared::default();
        let process_snapshot: Shared<openrigcue_config::detect::SystemSnapshot> = Shared::default();
        openrigcue_sources::udp_racing::forward::PortForwarding::shared()
            .set_reader(listeners::forwarding_reader(state.config.clone()));
        let imported = {
            let config = state.config.read();
            listeners::import_listeners(
                &ListenerInputs {
                    settings: config.settings(),
                    documents: &documents,
                    runtime_dir: &options.runtime_dir,
                    steam: setup.steam(),
                    joystick_inputs: joystick_inputs.clone(),
                    joystick_runtime: Some(joystick_runtime_reader(state.clone(), joystick_profile.clone())),
                    network_ports_reader: Some(listeners::network_ports_reader(state.config.clone())),
                    pause_reader: Some(listeners::pause_reader(state.config.clone())),
                    game_reader: Some({
                        let session = state.session.clone();
                        Arc::new(move || session.read().game_key().to_owned())
                    }),
                    iracing_dams_reader: Some(listeners::iracing_dams_reader(state.config.clone())),
                    xplane_lateral_reader: Some(listeners::xplane_lateral_reader(state.config.clone())),
                    console_reader: Some(listeners::console_reader(state.config.clone())),
                    windows_reader: Some(listeners::windows_reader(process_snapshot.clone())),
                    context: context.clone(),
                    outputs: outputs.clone(),
                },
                Some(listeners::motion_model_reader(state.config.clone())),
                Some(listeners::elite_pid_reader(process_snapshot.clone())),
            )
        };
        if let Some(failure) = &imported.failure {
            // A listener that fails to load is logged with its trace. No listener is then registered
            // and the telemetry main loop does not start; the listeners loaded before the failing one stay
            // loaded for the life of the process, with their threads.
            tracing::debug!("error during post processing {}", failure.text);
            tracing::debug!(
                "Trace: backend.listeners.{} (NETWORK_PORT[{}]): {}",
                failure.listener,
                failure.key,
                failure.text
            );
        }
        std::mem::forget(imported.detached);
        let listeners = imported.registered;
        let parts = TelemetryParts {
            dispatcher: TelemetryHandle::for_settings(Dispatcher::new(listeners), state.config.read().settings()),
            snapshot: process_snapshot,
            context,
            outputs,
            joystick_profile: joystick_profile.clone(),
        };
        state.session.write().show_all_channels = parts.dispatcher.shows_all_channels();

        let accessories =
            Arc::new(openrigcue_effects::accessories::dispatcher::Accessories::new(std::time::Instant::now()));
        let ui = Arc::new(
            api::ui_state::UiServices::new(
                state.config.read().settings(),
                options.hooks.log_level.clone(),
                options.hooks.restart.clone(),
            )
            .with_geometry_hook(options.hooks.geometry.clone())
            // The user's own translations: `locales/<code>.json` in the configuration directory, laid over
            // the built-in files.
            .with_locale_folder(options.config_dir.root().join("locales")),
        );
        let motion = {
            let config = state.config.read();
            let ui = Arc::clone(&ui);
            MotionHandle::import(
                config.settings(),
                &documents,
                Box::new(openrigcue_devices::motion::SystemHost::default()),
                Arc::new(move |text: &str| ui.translate(text)),
            )
        };
        motion.driver_settings().read_runtime_from(&state);
        motion
            .driver_settings()
            .yaw_shared
            .update(|shared| shared.share_motion_sensor(accessories.motion_sensor_values()));
        let shakers = ShakersHandle::new(
            &state,
            openrigcue_effects::shakers::SampleBank::synthesized(),
            Box::new(crate::shakers::SilentBeep),
            true,
        )
        // Built-in sounds unless "Use SRS Sound Samples" is on; the shift beep (an imported file) follows the same
        // switch. An import runs on a worker thread, the service starts with the built-in sounds meanwhile.
        .with_srs_sounds(&state, options.srs_install.clone());
        let main_loop = MainLoopHandle::new(MainLoop::new(setup, options.proc_root.clone()));

        let compensation =
            crate::compensation::VrCompensation::for_service(options.config_dir.root(), &options.mirror_dir);
        let web_port = native_api_port(state.config.read().settings())?;
        // Persistent Linux loader/SteamVR integration is an application-start action.
        // Constructors and read-only API calls deliberately never install anything.
        compensation.initialize_native_integrations();
        let voice =
            crate::voice::Voice::for_service(platform.is_windows(), true, options.srs_install.as_deref(), &documents);
        let context = ApiContext {
            state: state.clone(),
            telemetry: parts.dispatcher.clone(),
            motion: motion.clone(),
            shakers: shakers.clone(),
            accessories: Arc::clone(&accessories),
            compensation: compensation.clone(),
            main_loop: main_loop.clone(),
            srs_install: options.srs_install.clone(),
            host_ip,
            voice,
            ui,
            joystick: joystick::JoystickService::new(state.clone(), joystick_inputs.clone(), joystick_profile),
            hotkeys: hotkeys::Hotkeys::new(joystick_inputs.clone(), &documents),
        };
        let api = ApiServer::start_imported(context.clone(), web_port)?;

        let pipe_relay = options.pipe_relay.and_then(start_pipe_relay);
        let export = crate::export_csv::ExportThread::start(state.clone(), parts.dispatcher.clone(), &documents)?;
        let telemetry = TelemetryThreads::start(state.clone(), parts, system_titles(), options.proc_root.clone())?;
        let sound_devices = crate::sound_devices::DeviceMonitor::start(
            state.clone(),
            shakers.clone(),
            crate::sound_devices::DeviceSources::system(),
        )?;
        // The HUD thread starts here, with its first scan.
        let hud = hud::start(hud::ServiceHud::new(
            state.clone(),
            Arc::clone(&accessories),
            Arc::clone(&context.ui),
            options.srs_install.clone(),
            Arc::new(AtomicBool::new(false)),
        ))?;
        let hardware = start_hardware(
            &state,
            &options,
            motion,
            shakers,
            accessories,
            compensation,
            Arc::clone(&context.ui),
            port_cache.clone(),
        )?;
        // The input device loops (controller and keyboard) start last.
        let hotkeys = hotkeys::HotkeyThreads::start(context)?;
        Ok(Service {
            state,
            api,
            port_cache,
            _telemetry: telemetry,
            _pipe_relay: pipe_relay,
            _hardware: hardware,
            _export: export,
            _sound_devices: sound_devices,
            _hotkeys: hotkeys,
            _hud: hud,
        })
    }

    pub fn state(&self) -> &ServiceState {
        &self.state
    }

    /// The address the API listens on.
    pub fn address(&self) -> SocketAddr {
        self.api.address
    }

    /// Runs the launcher's table/cache close sequence, then the
    /// process ends at once, no loop is joined and the platform is not parked: with a SIMTECHREVOLUTION model by
    /// killing itself, otherwise by an immediate exit with status 0. An unknown model counts as `DOF REALITY`.
    pub fn goodbye(&self) -> ! {
        self.stop_native_helpers();
        close_launcher_files(&self.state, &self.port_cache);
        let model = self.state.config.read().settings().motion.model.clone();
        let manufacturer =
            openrigcue_devices::motion::motion_model(&model, true).map_or("DOF REALITY", |m| m.manufacturer);
        if manufacturer == "SIMTECHREVOLUTION" {
            let _ = std::process::Command::new("kill").arg("-9").arg(std::process::id().to_string()).status();
        }
        std::process::exit(0)
    }

    /// Reap the sound-server subscription helper before immediate exit or in-process exec.
    pub fn stop_native_helpers(&self) {
        if let Some(monitor) = &self._sound_devices {
            monitor.stop_native_helper();
        }
    }
}

/// The launcher's close sequence (failures are logged), also available to device-free fixtures.
pub(crate) fn close_launcher_files(state: &ServiceState, port_cache: &PortCache) {
    state.config.write().close_launcher_files();
    if let Err(error) = port_cache.close() {
        tracing::debug!(%error, "Port cache close raised");
    }
}

/// The time of a `log.txt` line: month-day, time and milliseconds, in local time.
pub fn log_time_stamp() -> String {
    chrono::Local::now().format("%m-%d %H:%M:%S%.3f").to_string()
}

/// The web UI listen address: `APP.webui_port` on every interface (`0.0.0.0`).
pub fn api_address(app: &openrigcue_config::settings::AppSettings) -> SocketAddr {
    SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, app.web_ui_port))
}

/// The configured web UI port as a dynamically typed integer: the settings lookup and integer conversion
/// happen before the certificate checks. Socket-range validation follows those checks.
fn native_api_port(
    settings: &openrigcue_config::settings::Settings,
) -> std::io::Result<openrigcue_model::native_value::LegacyValue> {
    let native = openrigcue_model::native_value::LegacyValue::Dict(settings.native_snapshot());
    let value = openrigcue_cueing::PyValue::from(&native);
    value
        .item_owned("APP")
        .and_then(|app| app.item_owned("webui_port"))
        .and_then(|port| port.to_integer_value())
        .map(|port| port.to_native())
        .map_err(|error| std::io::Error::other(format!("{}: {}", error.exception().name(), error.message())))
}

/// The controllers threads: motion stream and test, shakers, accessories with the connection scan.
#[allow(clippy::too_many_arguments)]
fn start_hardware(
    state: &ServiceState,
    options: &ServiceOptions,
    motion: MotionHandle,
    shakers: crate::shakers::ShakersHandle,
    accessories: Arc<openrigcue_effects::accessories::dispatcher::Accessories>,
    compensation: crate::compensation::VrCompensation,
    ui: Arc<api::ui_state::UiServices>,
    port_cache: PortCache,
) -> Result<HardwareThreads, StartError> {
    // The documents folder is the configuration directory.
    let documents = options.config_dir.root();
    let motion_threads = MotionThreads::start(state.clone(), motion.clone(), Some(&options.mirror_dir))?;
    let shaker_thread = ShakerThread::start(state.clone(), shakers, Arc::clone(&accessories))?;
    let model = state.config.read().settings().motion.model.clone();
    start_pose_log(&accessories, &model, documents);
    let scan_stop = Arc::new(AtomicBool::new(false));
    let wake = scan_wake(state.config.read().settings(), Arc::clone(&scan_stop));
    // The application folder: the `--srs-install` folder when one is given, else the program's folder.
    let srs_app_path = options
        .srs_install
        .clone()
        .or_else(|| {
            openrigcue_effects::accessories::firmware::bundled_firmware_root(
                std::env::var_os("APPDIR").as_deref().map(std::path::Path::new),
                std::env::current_exe().ok().as_deref(),
                std::path::Path::new("firmware/intellibox/intellibox.hex"),
            )
        })
        .or_else(app_dir)
        .unwrap_or_default();
    let scanners = Scanners {
        intellibox: IntelliboxScanner::new(),
        belt: BeltScanner::new(),
        hapticr: HapticScanner::new(),
        sim3d: Sim3dScanner::new(),
        iflag: IFlagScanner::new(),
        flasher: IntelliboxFlasher::from_source(&srs_app_path, &documents.join("firmware-import")).with_native_conf(
            openrigcue_effects::accessories::firmware::bundled_avrdude_conf(
                std::env::var_os("APPDIR").as_deref().map(std::path::Path::new),
                std::env::current_exe().ok().as_deref(),
            ),
        ),
    };
    // The port cache, shared by the accessory scans and the Thanos driver.
    let thanos_cache = openrigcue_devices::motion::thanos::PortCache::new(Arc::new(port_cache.clone()));
    motion.driver_settings().thanos_shared.update(|shared| shared.cache = thanos_cache);
    let feed = Arc::new(
        ServiceFeed::new(state.clone(), motion, Arc::clone(&accessories), port_cache, Some(options.mirror_dir.clone()))
            .with_ui(ui)
            .with_vr(compensation.clone()),
    );
    let rig_pose = crate::compensation::RigPoseThread::start(crate::compensation::publisher::PublisherContext {
        vr: compensation,
        state: state.clone(),
        feed: Arc::clone(&feed),
        accessories: Arc::clone(&accessories),
    })?;
    let accessory_threads = AccessoryThreads::start(accessories, feed, scanners, Arc::new(SystemClock), wake)?;
    Ok(HardwareThreads {
        _motion: motion_threads,
        _shakers: shaker_thread,
        _accessories: accessory_threads,
        _scan_stop: scan_stop,
        _rig_pose: rig_pose,
    })
}

/// The folder of the running executable.
fn app_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(std::path::Path::to_path_buf)
}

/// The TCP side of the pipe relay, which the bridge uses under Wine without AF_UNIX (DynamicCab and Farming
/// Simulator pipes; their listeners exist from start-up). A port already in
/// use is logged and leaves the Unix sockets as the only way in.
fn start_pipe_relay(address: SocketAddr) -> Option<openrigcue_sources::process_memory::pipe_relay::TcpRelay> {
    openrigcue_sources::process_memory::pipe_relay::TcpRelay::start(address)
        .map_err(|error| tracing::warn!(%address, %error, "pipe relay TCP port unavailable"))
        .ok()
}

#[cfg(test)]
mod srs_import_tests {
    use super::{import_srs_install, imported_srs_install, SRS_IMPORT_DIR};

    /// One import leaves a copy that later starts find without the source.
    #[test]
    fn an_installation_folder_is_copied_once_and_found_again() {
        let tmp = tempfile::tempdir().unwrap();
        let (install, config) = (tmp.path().join("install"), tmp.path().join("config"));
        for (file, bytes) in [
            ("sounds/transmission/2.wav", &b"beep"[..]),
            ("sounds/voices/hello.mp3", b"clip"),
            ("firmware/intellibox/intellibox.hex", b"hex"),
            ("firmware/obd2hud/firmware.bin", b"bin"),
            ("avr_helpers/avrdude.conf", b"conf"),
            ("avr_helpers/avrdude.exe", b"program"),
        ] {
            let path = install.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
        std::fs::create_dir_all(&config).unwrap();
        assert_eq!(imported_srs_install(&config), None);
        let imported = import_srs_install(&install, &config).unwrap();
        assert_eq!(imported, config.join(SRS_IMPORT_DIR));
        std::fs::remove_dir_all(&install).unwrap();
        assert_eq!(imported_srs_install(&config), Some(imported.clone()));
        assert_eq!(std::fs::read(imported.join("sounds/transmission/2.wav")).unwrap(), b"beep");
        assert_eq!(std::fs::read(imported.join("sounds/voices/hello.mp3")).unwrap(), b"clip");
        assert_eq!(std::fs::read(imported.join("firmware/intellibox/intellibox.hex")).unwrap(), b"hex");
        assert_eq!(std::fs::read(imported.join("firmware/obd2hud/firmware.bin")).unwrap(), b"bin");
        assert!(!imported.join("avr_helpers").exists());
    }

    #[test]
    fn a_source_without_srs_files_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let empty = tmp.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(import_srs_install(&empty, tmp.path()).unwrap_err().kind(), std::io::ErrorKind::NotFound);
        assert_eq!(imported_srs_install(tmp.path()), None);
    }

    /// The user's own installer (`OPENRIGCUE_SRS_INSTALLER`): every firmware image and the sounds arrive, and
    /// nothing else.
    #[test]
    #[ignore = "needs a local SRS installer"]
    fn real_installer_is_imported() {
        let installer = std::env::var("OPENRIGCUE_SRS_INSTALLER").expect("OPENRIGCUE_SRS_INSTALLER");
        let tmp = tempfile::tempdir().unwrap();
        let imported = import_srs_install(std::path::Path::new(&installer), tmp.path()).unwrap();
        for file in [
            "firmware/intellibox/intellibox.hex",
            "firmware/intellibox_v6/intellibox_v6.hex",
            "firmware/obd2hud/firmware.bin",
            "firmware/belt/belt.bin",
            "firmware/iflag/iflag_uno_colorduino.hex",
            "firmware/iflag/iflag_mega_ws2812.hex",
            "sounds/transmission/2.wav",
        ] {
            assert!(imported.join(file).is_file(), "{file} was not imported");
        }
        assert!(!imported.join("avr_helpers").exists());
    }
}

#[cfg(test)]
mod api_address_tests {
    use super::api_address;
    use openrigcue_config::settings::AppSettings;

    /// The server binds `0.0.0.0` on `APP.webui_port`; `webui_hostname` is not the bind
    /// address.
    #[test]
    fn api_address_is_every_interface() {
        let mut app = AppSettings::default();
        assert_eq!(api_address(&app), "0.0.0.0:33733".parse().unwrap());
        app.web_ui_host = "srs.local".into();
        app.web_ui_port = 8080;
        assert_eq!(api_address(&app), "0.0.0.0:8080".parse().unwrap());
    }

    #[test]
    fn native_web_port_cannot_fall_back_to_the_typed_default() {
        use openrigcue_config::settings::Settings;
        for (raw, expected) in [("not-a-port", None), ("70000", None), ("-1", None), ("+033733", Some(33733))] {
            let settings = Settings::default();
            let mut native = settings.native_snapshot();
            native.get_mut("APP").unwrap().as_dict_mut().unwrap().insert("webui_port", raw.into());
            let settings = Settings::from_legacy(&native).0;
            let actual = super::native_api_port(&settings)
                .and_then(|port| super::api::start_policy::imported_native_address(&port))
                .ok()
                .map(|address| address.port());
            assert_eq!(actual, expected, "raw {raw} must not become a default listen port");
        }
        for (raw, expected) in [
            ("[APP]\n", "KeyError: 'webui_port'"),
            ("[APP]\nwebui_port = bad\n", "ValueError: invalid literal for int() with base 10: 'bad'"),
        ] {
            let native = openrigcue_config::import::configobj::parse(raw).unwrap();
            let settings = Settings::from_legacy(&native).0;
            assert_eq!(super::native_api_port(&settings).unwrap_err().to_string(), expected);
        }
    }

    #[test]
    fn unbounded_native_web_port_reaches_the_socket_range_boundary() {
        use openrigcue_config::settings::Settings;
        use openrigcue_model::native_value::LegacyValue;
        let text = "9".repeat(400);
        let settings = Settings::default();
        let mut native = settings.native_snapshot();
        native.get_mut("APP").unwrap().as_dict_mut().unwrap().insert("webui_port", text.clone().into());
        let settings = Settings::from_legacy(&native).0;
        let port = super::native_api_port(&settings).unwrap();
        assert_eq!(port.py_str().unwrap(), text);
        assert!(matches!(port, LegacyValue::BigInt(_)));
        assert_eq!(
            super::api::start_policy::imported_native_address(&port).unwrap_err().to_string(),
            "OverflowError: bind(): port must be 0-65535."
        );
    }
}

#[cfg(test)]
mod joystick_runtime_tests;
