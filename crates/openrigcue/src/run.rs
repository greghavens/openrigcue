//! `openrigcue run`: starting the application.
//!
//! In order: the arguments, the configuration folder, the start-up call check,
//! the folder's write probe, the configuration file reset, the log file, the single-instance check with the
//! profile hand-over, the service, the wait for the web
//! server and the desktop window, unless `--headless`.
//! The process then runs until Ctrl-C or SIGTERM, which end it with the service's clean shutdown.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Context;
use openrigcue_config::settings::paths::ConfigDir;
#[cfg(test)]
use openrigcue_config::settings::Settings;
use openrigcue_daemon::{Service, ServiceOptions};

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Configuration directory (default `$XDG_CONFIG_HOME/openrigcue`); it must exist (`--config_path`).
    #[arg(long, value_parser = existing_dir)]
    pub config_dir: Option<PathBuf>,
    /// A profile file to import (`--profile_path`): handed to the running instance when there is one.
    #[arg(long)]
    pub profile_path: Option<PathBuf>,
    /// The desktop's autostart is calling (`--windows_startup_calling`): the app starts only with "Start with
    /// login" on.
    #[arg(long)]
    pub login_calling: bool,
    /// The installer is calling (`--installer_calling`); read and not used.
    #[arg(long)]
    pub installer_calling: bool,
    /// Run the server without opening the window (`--headless`).
    #[arg(long)]
    pub headless: bool,
    /// Your Sim Racing Studio installation folder or installer, read only for the optional "Use SRS Sound
    /// Samples" import (shaker samples, shift beep, FMOD banks, voice clips) and the firmware images of the
    /// Intellibox and OBD2HUD flashers. Help pages, translations and game templates are built in.
    #[arg(long)]
    pub srs_install: Option<PathBuf>,
    /// Port of the bridge's pipe relay on 127.0.0.1; must match the bridge's `--pipe-tcp-port`.
    #[arg(long, default_value_t = openrigcue_daemon::PIPE_RELAY_TCP_PORT)]
    pub pipe_tcp_port: u16,
    /// Serve the bridge's pipe relay on its Unix sockets only (Wine with AF_UNIX); no TCP port is opened.
    #[arg(long, conflicts_with = "pipe_tcp_port")]
    pub no_pipe_tcp: bool,
}

/// The argument must name an existing folder; anything else is an error.
fn existing_dir(text: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(text);
    if path.is_dir() {
        Ok(path)
    } else {
        Err(format!("NotADirectoryError: {text}"))
    }
}

/// The service's pipe relay address for the `--pipe-tcp-port` / `--no-pipe-tcp` options.
fn pipe_relay(args: &Args) -> Option<std::net::SocketAddr> {
    (!args.no_pipe_tcp).then(|| openrigcue_daemon::pipe_relay_address(args.pipe_tcp_port))
}

pub fn config_dir(explicit: Option<PathBuf>) -> anyhow::Result<ConfigDir> {
    match explicit {
        Some(dir) => Ok(ConfigDir::at(dir)),
        None => ConfigDir::from_env().context("neither $XDG_CONFIG_HOME nor $HOME is set; pass --config-dir"),
    }
}

/// The `tracing` filter for a numeric `APP.log_level`: 10
/// Debug, 20 Info, 30 Warning, 40 Error; above 40 (Critical) nothing OpenRigCue logs, at 0 everything.
pub fn filter_for_python_level(level: u32) -> &'static str {
    match level {
        0 => "trace",
        1..=10 => "debug",
        11..=20 => "info",
        21..=30 => "warn",
        31..=40 => "error",
        _ => "off",
    }
}

/// The hook that applies `APP.log_level` to the process's log filter.
pub fn follow_log_level<S: 'static>(
    handle: tracing_subscriber::reload::Handle<tracing_subscriber::EnvFilter, S>,
) -> openrigcue_daemon::api::ui_state::LogLevelHook {
    std::sync::Arc::new(move |level| {
        let _ = handle.modify(|filter| *filter = tracing_subscriber::EnvFilter::new(filter_for_python_level(level)));
    })
}

/// What happens before the log file exists: the start-up call check, the write probe and
/// the configuration file reset. `false` when the process ends here.
#[cfg(test)]
pub fn prepare(args: &Args, dir: &ConfigDir) -> anyhow::Result<bool> {
    prepare_with_splash(args, dir, || Ok(()), |_, _| {})
}

/// `notify` receives the summary and the details of a permission error, for the dialog.
pub fn prepare_with_splash(
    args: &Args,
    dir: &ConfigDir,
    show: impl FnOnce() -> anyhow::Result<()>,
    notify: impl FnOnce(&str, &str),
) -> anyhow::Result<bool> {
    if args.login_calling && !start_with_login(dir)? {
        // An autostart call is ignored while "Start with login" is off.
        startup_event("Login startup call ignored because \"Start with login\" is off");
        return Ok(false);
    }
    if !args.headless {
        startup_event("Loading splash screen");
        show()?;
    }
    if let Err((operation, error)) = write_probe(dir.root()) {
        permission_error(dir, dir.root(), operation, &error, notify);
        return Ok(false);
    }
    startup_event("Checking reset config files");
    openrigcue_config::settings::reset_config_files(dir);
    startup_event("Reset config files check completed");
    Ok(true)
}

/// What start-up has done so far, oldest first (the last 200 entries); part of a permission error's details.
static STARTUP_EVENTS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Records one start-up step with its local time.
pub fn startup_event(message: &str) {
    let mut events = STARTUP_EVENTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    events.push(format!("{}\n{message}", openrigcue_daemon::log_time_stamp()));
    let excess = events.len().saturating_sub(200);
    events.drain(..excess);
}

/// The fields of an I/O error as lines: its kind, the system error number and the system's message.
fn error_fields(error: &std::io::Error) -> String {
    let errno = error.raw_os_error().map_or_else(|| "None".to_owned(), |code| code.to_string());
    format!("exception_type: {:?}\nerrno: {errno}\nstrerror: {error}", error.kind())
}

/// "Start with login" of the stored settings (the `StartWithWindows` row; missing → `'1'`).
fn start_with_login(dir: &ConfigDir) -> anyhow::Result<bool> {
    registry_bool(dir, "StartWithWindows")
}

pub fn show_splash_screen(dir: &ConfigDir) -> anyhow::Result<bool> {
    registry_bool(dir, "ShowSplashScreen")
}

fn registry_bool(dir: &ConfigDir, key: &str) -> anyhow::Result<bool> {
    use openrigcue_model::native_value::LegacyValue;
    let value = openrigcue_config::import::registry::read_item(dir.root(), key)
        .ok()
        .flatten()
        .filter(|value| !matches!(value, LegacyValue::None))
        .unwrap_or_else(|| LegacyValue::from("1"));
    // This precedes config.ini parsing and the write probe.
    // A row that cannot be read defaults to 1; an obtained but invalid boolean is an error.
    openrigcue_cueing::str2bool(&openrigcue_cueing::PyValue::from(&value)).map_err(Into::into)
}

/// A missing folder is created (its parent must exist); an existing one gets the write probe
/// (`.srs_write_test` written empty, then removed).
fn write_probe(root: &Path) -> Result<(), (&'static str, std::io::Error)> {
    if !root.is_dir() {
        startup_event(&format!("Creating config folder: {root:?}"));
        std::fs::create_dir(root).map_err(|error| ("create", error))?;
        startup_event("Config folder created");
        return Ok(());
    }
    let probe = root.join(".openrigcue_write_test");
    startup_event(&format!("Running config folder write probe: {probe:?}"));
    std::fs::write(&probe, b"").and_then(|()| std::fs::remove_file(&probe)).map_err(|error| ("write to", error))?;
    startup_event("Config folder write probe succeeded");
    Ok(())
}

/// The permission error dialog's title.
pub const PERMISSION_ERROR_TITLE: &str = "Permission error...";

/// The line above the details of a permission error.
pub const PERMISSION_ERROR_HINT: &str = "Copy these details when you report this problem. If your antivirus shows \
     that OpenRigCue was blocked, please include a screenshot of that alert too.";

/// A folder or file that cannot be written: the log line, the message with its details on standard error and,
/// through `notify`, in the dialog.
pub fn permission_error(
    dir: &ConfigDir,
    path: &Path,
    operation: &str,
    error: &std::io::Error,
    notify: impl FnOnce(&str, &str),
) {
    startup_event(&format!(
        "Startup permission error during {operation}: {}\nexception: {error:?}\n{}",
        path.display(),
        error_fields(error)
    ));
    tracing::error!("Startup permission error during {operation} for {}: {error:?}", path.display());
    let summary = permission_summary(path, operation);
    let details = startup_error_details(dir, path, operation, error);
    eprintln!("{PERMISSION_ERROR_TITLE}\n{summary}\n\n{details}");
    notify(&summary, &details);
}

fn permission_summary(path: &Path, operation: &str) -> String {
    let kind = if path.is_file() { "file" } else { "folder" };
    format!(
        "The app cannot {operation} {}. Please make sure that this {kind} is not protected in your anti-virus \
         software, or that the application is added to the anti-virus trusted app list.",
        path.display()
    )
}

/// What a permission error's details hold: the failed operation, the error, this build, the system, the
/// configuration paths and the start-up steps recorded so far.
fn startup_error_details(dir: &ConfigDir, path: &Path, operation: &str, error: &std::io::Error) -> String {
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let mut details = vec![
        "OpenRigCue startup permission error".to_owned(),
        String::new(),
        format!("operation: {operation}"),
        format!("path: {}", path.display()),
        format!("exception: {error:?}"),
        format!("app_version: {}", env!("CARGO_PKG_VERSION")),
        "packaged: True".to_owned(),
        format!("platform: {}-{}-{}", std::env::consts::OS, kernel.trim(), std::env::consts::ARCH),
        format!("config_dir: {:?}", dir.root()),
        format!("log_file: {:?}", dir.root().join("log.txt")),
        String::new(),
        error_fields(error),
    ];
    let events = STARTUP_EVENTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if !events.is_empty() {
        details.extend([String::new(), "startup_events:".to_owned(), events.join("\n")]);
    }
    details.join("\n")
}

/// The first lines of the log (logger `OpenRigCueApp`).
pub fn log_start() {
    tracing::info!(target: "OpenRigCueApp", "Application Started");
    tracing::info!(target: "OpenRigCueApp", "Version: {}", env!("CARGO_PKG_VERSION"));
    tracing::info!(target: "OpenRigCueApp", "Packaged: True");
    let executable = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
    tracing::info!(target: "OpenRigCueApp", "Executable: {:?}", executable.clone().unwrap_or_default());
    tracing::info!(target: "OpenRigCueApp", "App path: {:?}", executable.unwrap_or_default());
}

/// What ends a service run.
enum Signal {
    Stop,
    Restart,
    Open,
    Menu { x: i32, y: i32 },
    Capture(mpsc::Sender<openrigcue_desktop::Geometry>),
    Activate,
    CloseWindow,
}

pub fn run(
    args: Args,
    log_level: Option<openrigcue_daemon::api::ui_state::LogLevelHook>,
    mut desktop: Option<openrigcue_desktop::Desktop>,
) -> anyhow::Result<()> {
    let mut options = ServiceOptions::for_user().context("$HOME is not set")?;
    options.pipe_relay = pipe_relay(&args);
    options.config_dir = config_dir(args.config_dir.clone())?;
    options.srs_install = match &args.srs_install {
        Some(source) => Some(
            openrigcue_daemon::import_srs_install(source, options.config_dir.root())
                .with_context(|| format!("importing SRS files from {}", source.display()))?,
        ),
        None => openrigcue_daemon::imported_srs_install(options.config_dir.root()),
    };
    let dir = options.config_dir.clone();
    // Up to six looks, 0.5 s apart, for this program running only once.
    let existing = existing_instance(
        &dir,
        args.profile_path.as_deref(),
        instances,
        std::thread::sleep,
        || openrigcue_desktop::instance::Instance::activate_existing().map_err(Into::into),
        send_profile_handoff,
    )?;
    if !begin_new_instance(existing, |major| {
        openrigcue_desktop::instance::Instance::close_existing_major(major).map(|_| ())
    }) {
        return Ok(());
    }
    if !args.headless {
        if let Err(error) = register_login(&args) {
            tracing::warn!(%error, "registering desktop login startup");
        }
    }
    let (stop_tx, stop_rx) = mpsc::channel();
    let restart_tx = stop_tx.clone();
    let tray_tx = stop_tx.clone();
    let capture_tx = stop_tx.clone();
    let activate_tx = stop_tx.clone();
    let close_window_tx = stop_tx.clone();
    ctrlc::set_handler(move || {
        let _ = stop_tx.send(Signal::Stop);
    })
    .context("installing the Ctrl-C / SIGTERM handler")?;
    options.hooks.log_level = log_level;
    let geometry = desktop.as_ref().map(|desktop| desktop.shared_geometry()).unwrap_or_default();
    options.hooks.geometry = Some(std::sync::Arc::new(move || {
        let (answer, receive) = mpsc::channel();
        capture_tx.send(Signal::Capture(answer)).ok()?;
        Some(receive.recv().ok()?.assignments())
    }));
    options.hooks.restart = Some(std::sync::Arc::new(move || {
        let _ = restart_tx.send(Signal::Restart);
    }));
    options.profile_path = args.profile_path;
    let service = Service::start(options)?;
    let desktop_action: std::sync::Arc<dyn Fn(openrigcue_desktop::TrayAction) + Send + Sync> =
        std::sync::Arc::new(move |action| {
            let signal = match action {
                openrigcue_desktop::TrayAction::Open => Signal::Open,
                openrigcue_desktop::TrayAction::Quit => Signal::Stop,
                openrigcue_desktop::TrayAction::Menu { x, y } => Signal::Menu { x, y },
            };
            let _ = tray_tx.send(signal);
        });
    let _tray = match openrigcue_desktop::Tray::start(desktop_action.clone()) {
        Ok(tray) => Some(tray),
        Err(error) => {
            tracing::warn!(%error, "desktop tray unavailable");
            None
        }
    };
    if let Some(desktop) = &mut desktop {
        let state = service.state().clone();
        desktop.set_config_reader(std::sync::Arc::new(move || state.config.read().settings().native_snapshot()));
    }
    let app = service.state().config.read().settings().app.clone();
    let mut url = format!("http://{}:{}", app.web_ui_host, app.web_ui_port);
    let mut unavailable = false;
    // Wait for the web UI, then show bundled startup help if it did not answer.
    for idx in 0..100 {
        match get(&app.web_ui_host, app.web_ui_port, "/") {
            Ok(()) => break,
            Err(error) => {
                std::thread::sleep(Duration::from_millis(100));
                tracing::debug!("Error getting url from the server:{error}");
                if idx == 99 {
                    unavailable = true;
                }
            }
        }
    }
    if args.headless {
        tracing::info!("Running in server-only mode, bypassing load_browser.");
        println!("App is running in server-only mode. Access it via: {url}");
        if unavailable {
            eprintln!(
                "OpenRigCue's web interface has not answered yet. Check {} and the APP.webui_hostname/APP.webui_port settings in {}.",
                dir.root().join("log.txt").display(),
                dir.settings_file().display()
            );
        }
    } else if let Some(desktop) = &mut desktop {
        if unavailable {
            url = startup_help_url(&dir, &url);
        }
        // The window opens whether or not the web server answered.
        if let Err(error) = desktop.open(&url) {
            tracing::error!(%error, "opening desktop window");
            drop(stop_rx);
            return Err(error.into());
        }
    }
    let _instance = match openrigcue_desktop::instance::Instance::register(
        std::sync::Arc::new(move || {
            let _ = activate_tx.send(Signal::Activate);
        }),
        std::sync::Arc::new(move || {
            let _ = close_window_tx.send(Signal::CloseWindow);
        }),
    ) {
        Ok(instance) => Some(instance),
        Err(error) => {
            tracing::warn!(%error,"registering native window activation");
            None
        }
    };
    let signal = loop {
        if desktop.as_ref().is_some_and(|desktop| desktop.dispatch_pending()) {
            break Signal::Stop;
        }
        let incoming = if desktop.is_none() {
            stop_rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected)
        } else {
            stop_rx.recv_timeout(Duration::from_millis(10))
        };
        let has_window = desktop.as_ref().is_some_and(|desktop| desktop.has_window());
        match incoming.map(|signal| window_request(signal, has_window, args.headless)) {
            Ok(Signal::Activate) => {
                if let Some(desktop) = &desktop {
                    desktop.activate_existing();
                }
            }
            Ok(Signal::CloseWindow) => {
                if let Some(desktop) = &desktop {
                    desktop.close_existing();
                }
            }
            Ok(Signal::Capture(answer)) => {
                if desktop.is_none() {
                    // Querying screens does not create a visible window. With
                    // no display, Save retains positions and resets MONITORS.
                    desktop = openrigcue_desktop::Desktop::with_geometry(geometry.clone()).ok();
                    if let Some(desktop) = &mut desktop {
                        let state = service.state().clone();
                        desktop.set_config_reader(std::sync::Arc::new(move || {
                            state.config.read().settings().native_snapshot()
                        }));
                    }
                }
                let value = desktop.as_ref().map(|desktop| desktop.capture_geometry()).unwrap_or_default();
                let _ = answer.send(value);
            }
            Ok(signal @ (Signal::Open | Signal::Menu { .. })) => {
                let result = (|| -> anyhow::Result<()> {
                    if desktop.is_none() {
                        // Headless starts skip the splash even if the tray
                        // later creates the window.
                        let mut shell = openrigcue_desktop::Desktop::with_geometry(geometry.clone())?;
                        let state = service.state().clone();
                        shell.set_config_reader(std::sync::Arc::new(move || {
                            state.config.read().settings().native_snapshot()
                        }));
                        desktop = Some(shell);
                    }
                    let shell = desktop.as_mut().expect("initialized");
                    match signal {
                        Signal::Open => shell.open(&url)?,
                        Signal::Menu { x, y } => shell.show_menu(x, y, desktop_action.clone())?,
                        _ => unreachable!("desktop command"),
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    tracing::error!(%error, "opening desktop window");
                }
            }
            Ok(signal) => break signal,
            Err(mpsc::RecvTimeoutError::Disconnected) => break Signal::Stop,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    };
    // Release pending replies once desktop dispatch has ended.
    drop(stop_rx);
    drop(desktop);
    match signal {
        Signal::Stop => service.goodbye(),
        // The factory reset: this program is executed again with the same arguments.
        Signal::Restart => {
            service.stop_native_helpers();
            restart()
        }
        Signal::Open | Signal::Menu { .. } | Signal::Capture(_) | Signal::Activate | Signal::CloseWindow => {
            unreachable!("handled in desktop dispatch")
        }
    }
}

/// What a signal asks of the desktop shell. A second launch's activation request raises the window when
/// there is one; when the window was closed while the application kept running, the request opens it again,
/// so the application can be reached without a tray. An instance started without a window opens none.
fn window_request(signal: Signal, has_window: bool, headless: bool) -> Signal {
    match signal {
        Signal::Activate if !has_window && !headless => Signal::Open,
        signal => signal,
    }
}

/// Close an existing window of this application's current major version before
/// initialization, catching lookup and close failures independently of startup.
fn begin_new_instance<E>(existing: bool, request: impl FnOnce(&str) -> Result<(), E>) -> bool {
    if existing {
        return false;
    }
    let _ = request(env!("CARGO_PKG_VERSION_MAJOR"));
    true
}

fn register_login(args: &Args) -> anyhow::Result<()> {
    let home = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .context("no desktop configuration home")?;
    let executable = std::env::var_os("APPIMAGE")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute() && path.is_file())
        .unwrap_or(std::env::current_exe()?);
    let mut arguments: Vec<std::ffi::OsString> = vec!["run".into(), "--login-calling".into()];
    // The SRS files were copied into the configuration folder; later starts need no `--srs-install`.
    if let Some(path) = &args.config_dir {
        arguments.push("--config-dir".into());
        arguments.push(std::path::absolute(path)?.into_os_string());
    }
    if args.no_pipe_tcp {
        arguments.push("--no-pipe-tcp".into());
    } else {
        arguments.extend(["--pipe-tcp-port".into(), args.pipe_tcp_port.to_string().into()]);
    }
    openrigcue_desktop::autostart::install(&home, &executable, &arguments)?;
    Ok(())
}

/// The page opened when the web server never answers.
/// A self-contained help page also works when the application's HTTP server is unavailable.
fn startup_help_url(dir: &ConfigDir, address: &str) -> String {
    fn html(text: &str) -> String {
        text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
    }
    let document = format!(
        "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><title>OpenRigCue Startup Help</title>\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <body style=\"font:18px system-ui;max-width:48rem;margin:3rem auto;padding:1rem\">\
         <h1>OpenRigCue {version}</h1><h2>The web interface has not answered yet</h2>\
         <p>OpenRigCue is still running. <a href=\"{address}\">Try opening the interface again</a>.</p>\
         <p>Check the application log: <code>{log}</code>.</p>\
         <p>The interface address is <code>{address}</code>. Check <code>webui_hostname</code> and \
         <code>webui_port</code> in the <code>APP</code> section of <code>{config}</code>, then restart OpenRigCue if you change them.</p>\
         <p>Once connected, open OpenRigCue Help from the header or Setup → App for device, audio and tuning guidance.</p>\
         </body></html>",
        version = env!("CARGO_PKG_VERSION"),
        address = html(address),
        log = html(&dir.root().join("log.txt").display().to_string()),
        config = html(&dir.settings_file().display().to_string()),
    );
    let mut url = String::from("data:text/html;charset=utf-8,");
    use std::fmt::Write;
    for byte in document.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            url.push(char::from(byte));
        } else {
            write!(&mut url, "%{byte:02X}").expect("writing to a string");
        }
    }
    url
}

/// Executes this program again, with the same arguments, in this process.
fn restart() -> ! {
    use std::os::unix::process::CommandExt;
    let error = match std::env::current_exe() {
        Ok(exe) => std::process::Command::new(exe).args(std::env::args_os().skip(1)).exec(),
        Err(error) => error,
    };
    tracing::error!(%error, "restart failed");
    std::process::exit(1)
}

fn wait_for_instances(mut count: impl FnMut() -> usize, mut sleep: impl FnMut(Duration)) -> usize {
    for _ in 0..6 {
        if count() <= 1 {
            break;
        }
        sleep(Duration::from_millis(500));
    }
    count()
}

fn existing_instance(
    dir: &ConfigDir,
    profile: Option<&Path>,
    count: impl FnMut() -> usize,
    sleep: impl FnMut(Duration),
    activate: impl FnOnce() -> anyhow::Result<()>,
    send: impl FnOnce(&str) -> anyhow::Result<()>,
) -> anyhow::Result<bool> {
    if wait_for_instances(count, sleep) <= 1 {
        return Ok(false);
    }
    if let Some(profile) = profile {
        handoff_profile(dir, profile, send)?;
    } else {
        activate()?;
    }
    Ok(true)
}

fn send_profile_handoff(url: &str) -> anyhow::Result<()> {
    // The request has no timeout, follows up to 30 redirects and consumes the
    // response body; an HTTP error status is not a failure.
    reqwest::blocking::Client::builder()
        .timeout(None)
        .tcp_user_timeout(None)
        .redirect(reqwest::redirect::Policy::limited(30))
        .user_agent("python-requests/2.33.0")
        .build()?
        .get(url)
        .send()?
        .bytes()?;
    Ok(())
}

/// The settings are read before profile.tmp is written, while the required string URL fields
/// are used after profile.tmp has been written and closed.
fn handoff_profile(
    dir: &ConfigDir,
    profile: &Path,
    send: impl FnOnce(&str) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let config = match std::fs::read_to_string(dir.settings_file()) {
        Ok(text) => openrigcue_config::import::configobj::parse(&text)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Default::default(),
        Err(error) => return Err(error.into()),
    };
    let temporary = dir.root().join("profile.tmp");
    std::fs::write(&temporary, profile.to_string_lossy().as_bytes())
        .with_context(|| format!("writing {}", temporary.display()))?;
    let url = handoff_url(&config)?;
    send(&url).context("handing the profile to the running instance")
}

fn handoff_url(config: &openrigcue_model::native_value::Dict) -> anyhow::Result<String> {
    use openrigcue_cueing::{CueError, PyValue};
    use openrigcue_model::native_value::LegacyValue;
    let config = PyValue::from(&LegacyValue::Dict(config.clone()));
    let string = |value: &PyValue| -> Result<String, CueError> {
        if let PyValue::Str(value) = value {
            Ok(value.clone())
        } else {
            Err(CueError::type_error(format!("can only concatenate str (not \"{}\") to str", value.type_name())))
        }
    };
    let hostname = string(config.item("APP")?.item("webui_hostname")?)?;
    let port = string(config.item("APP")?.item("webui_port")?)?;
    Ok(format!("http://{hostname}:{port}/import/api_set_file_path?value1=existing_instance"))
}

/// The running processes whose program file has this program's name and that run the application.
fn instances() -> usize {
    let Some(name) = std::env::current_exe().ok().and_then(|exe| exe.file_name().map(|n| n.to_owned())) else {
        return 1;
    };
    count_instances(Path::new("/proc"), &name)
}

fn count_instances(proc_root: &Path, name: &std::ffi::OsStr) -> usize {
    let Ok(entries) = std::fs::read_dir(proc_root) else { return 0 };
    entries
        .flatten()
        .filter(|entry| entry.file_name().to_str().is_some_and(|pid| pid.bytes().all(|b| b.is_ascii_digit())))
        .filter(|entry| std::fs::read_link(entry.path().join("exe")).is_ok_and(|exe| exe.file_name() == Some(name)))
        .filter(|entry| std::fs::read(entry.path().join("cmdline")).is_ok_and(|line| runs_application(&line)))
        .count()
}

/// Whether a process's argument list (NUL-separated, program name first) starts the application: its
/// subcommand, the first argument that is neither the global `--log` option nor that option's value, is `run`.
/// The other subcommands of this program are not instances of the application.
fn runs_application(cmdline: &[u8]) -> bool {
    let mut arguments = cmdline.split(|byte| *byte == 0).skip(1);
    while let Some(argument) = arguments.next() {
        if argument == b"--log" {
            arguments.next();
        } else if !argument.starts_with(b"--log=") {
            return argument == b"run";
        }
    }
    false
}

/// A GET of `http://<host>:<port><path>`; `Ok` once the server answered.
fn get(host: &str, port: u16, path: &str) -> std::io::Result<()> {
    let mut stream = TcpStream::connect((host, port))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    write!(stream, "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n")?;
    let mut status = [0u8; 12];
    stream.read_exact(&mut status)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn prior_window_close_precedes_initialization_and_ignores_unavailable_endpoint() {
        use std::cell::RefCell;
        for existing in [false, true] {
            for failed in [false, true] {
                let calls = RefCell::new(Vec::new());
                if begin_new_instance(existing, |major| {
                    assert_eq!(major, env!("CARGO_PKG_VERSION_MAJOR"));
                    calls.borrow_mut().push("close");
                    if failed {
                        Err("unavailable")
                    } else {
                        Ok(())
                    }
                }) {
                    calls.borrow_mut().push("initialize");
                }
                assert_eq!(*calls.borrow(), if existing { vec![] } else { vec!["close", "initialize"] });
            }
        }
    }

    /// A second launch asks the running instance to activate. With a window that stays a request to raise it;
    /// after the window was closed it becomes the request that opens the window; a server-only instance and
    /// every other signal are left as they are.
    #[test]
    fn second_launch_activation_reopens_a_closed_window() {
        assert!(matches!(window_request(Signal::Activate, true, false), Signal::Activate));
        assert!(matches!(window_request(Signal::Activate, false, false), Signal::Open));
        assert!(matches!(window_request(Signal::Activate, false, true), Signal::Activate));
        assert!(matches!(window_request(Signal::Activate, true, true), Signal::Activate));
        assert!(matches!(window_request(Signal::CloseWindow, false, false), Signal::CloseWindow));
        assert!(matches!(window_request(Signal::Stop, false, false), Signal::Stop));
        assert!(matches!(window_request(Signal::Menu { x: 3, y: 4 }, false, false), Signal::Menu { x: 3, y: 4 }));
        // The launcher side: without a profile to hand over, the second launch sends the activation request.
        let requested = std::cell::Cell::new(0);
        let dir = ConfigDir::at(scratch("activation-request"));
        let existing = existing_instance(
            &dir,
            None,
            || 2,
            |_| {},
            || {
                requested.set(requested.get() + 1);
                Ok(())
            },
            |_| panic!("no profile request"),
        );
        assert!(existing.unwrap());
        assert_eq!(requested.get(), 1);
        std::fs::remove_dir_all(dir.root()).unwrap();
    }

    #[test]
    fn brand_startup_help_is_local_and_escapes_the_actual_config_paths() {
        let dir = ConfigDir::at("/tmp/OpenRigCue <&> \"configuration\"");
        let address = "http://localhost:33733/?check=<&>\"";
        let url = startup_help_url(&dir, address);
        let encoded = url.strip_prefix("data:text/html;charset=utf-8,").unwrap();
        let mut decoded = Vec::new();
        let mut bytes = encoded.as_bytes().iter().copied();
        while let Some(byte) = bytes.next() {
            decoded.push(if byte == b'%' {
                let high = char::from(bytes.next().unwrap()).to_digit(16).unwrap();
                let low = char::from(bytes.next().unwrap()).to_digit(16).unwrap();
                ((high << 4) | low) as u8
            } else {
                byte
            });
        }
        let document = String::from_utf8(decoded).unwrap();
        assert!(document.contains(&format!("<h1>OpenRigCue {}</h1>", env!("CARGO_PKG_VERSION"))));
        assert!(document.contains("/tmp/OpenRigCue &lt;&amp;&gt; &quot;configuration&quot;/log.txt"));
        assert!(document.contains("/tmp/OpenRigCue &lt;&amp;&gt; &quot;configuration&quot;/config.ini"));
        assert!(document.contains("href=\"http://localhost:33733/?check=&lt;&amp;&gt;&quot;\""));
        assert!(document.contains("webui_hostname") && document.contains("webui_port"));
        assert!(!document.contains("simracingstudio") && !document.contains("github.com"));
    }

    #[test]
    fn profile_handoff_preserves_read_write_lookup_and_request_failure_order() {
        let tmp = scratch("handoff-failures");
        let dir = ConfigDir::at(&tmp);
        let profile = Path::new("/fixture/tune.srs");
        let pending = tmp.join("profile.tmp");
        std::fs::write(&pending, "prior").unwrap();
        std::fs::write(dir.settings_file(), "[broken").unwrap();
        assert!(handoff_profile(&dir, profile, |_| panic!("request after parse failure")).is_err());
        assert_eq!(std::fs::read_to_string(&pending).unwrap(), "prior");
        std::fs::remove_file(&pending).unwrap();
        std::fs::create_dir(&pending).unwrap();
        std::fs::write(dir.settings_file(), "").unwrap();
        let error = handoff_profile(&dir, profile, |_| panic!("request after write failure")).unwrap_err();
        assert!(error.downcast_ref::<std::io::Error>().is_some());
        std::fs::remove_dir(&pending).unwrap();
        for source in [None, Some("[APP]\nwebui_hostname = localhost\n")] {
            match source {
                None => std::fs::remove_file(dir.settings_file()).unwrap(),
                Some(text) => std::fs::write(dir.settings_file(), text).unwrap(),
            }
            let error =
                handoff_profile(&dir, profile, |_| panic!("request after required lookup failure")).unwrap_err();
            assert!(error.downcast_ref::<openrigcue_cueing::CueError>().is_some());
            assert_eq!(std::fs::read_to_string(&pending).unwrap(), "/fixture/tune.srs");
        }
        std::fs::write(dir.settings_file(), "[APP]\nwebui_hostname = 127.0.0.1\nwebui_port = 33733\n").unwrap();
        let error = handoff_profile(&dir, profile, |url| {
            assert_eq!(std::fs::read_to_string(&pending).unwrap(), "/fixture/tune.srs");
            assert!(url.ends_with("/import/api_set_file_path?value1=existing_instance"));
            anyhow::bail!("injected requests.get")
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("injected requests.get"));
        assert!(existing_instance(
            &dir,
            None,
            || 2,
            |_| {},
            || anyhow::bail!("injected windows"),
            |_| panic!("no profile request")
        )
        .is_err());
        std::fs::remove_dir_all(tmp).unwrap();
    }

    #[test]
    fn splash_precedes_the_write_probe_and_is_skipped_in_headless_mode() {
        let temporary = scratch("splash-order");
        let dir = ConfigDir::at(temporary.join("new"));
        let failure = prepare_with_splash(
            &parse(&[]).unwrap(),
            &dir,
            || {
                assert!(!dir.root().exists());
                anyhow::bail!("splash fixture failed")
            },
            |_, _| panic!("no permission error"),
        );
        assert!(failure.is_err());
        assert!(!dir.root().exists(), "failed splash must precede all configuration writes");
        assert!(prepare_with_splash(
            &parse(&["--headless"]).unwrap(),
            &dir,
            || panic!("headless loaded splash"),
            |_, _| panic!("no permission error")
        )
        .unwrap());
        assert!(dir.root().is_dir());
        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[derive(Parser)]
    struct Command {
        #[command(flatten)]
        args: Args,
    }

    /// An empty folder of its own below the system's temporary folder.
    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("openrigcue-run-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn parse(argv: &[&str]) -> Result<Args, clap::Error> {
        Command::try_parse_from(std::iter::once("run").chain(argv.iter().copied())).map(|c| c.args)
    }

    /// The default relay address is the bridge's `--pipe-tcp-port` default (33010).
    #[test]
    fn pipe_relay_follows_the_bridge_port() {
        assert_eq!(pipe_relay(&parse(&[]).unwrap()), Some("127.0.0.1:33010".parse().unwrap()));
        assert_eq!(
            pipe_relay(&parse(&["--pipe-tcp-port", "34000"]).unwrap()),
            Some("127.0.0.1:34000".parse().unwrap())
        );
        assert_eq!(pipe_relay(&parse(&["--no-pipe-tcp"]).unwrap()), None);
    }

    /// `--config_path` must be an existing folder; the flags are off unless given.
    #[test]
    fn launcher_arguments() {
        let tmp = scratch("arguments");
        let dir = tmp.to_str().unwrap();
        let args = parse(&["--config-dir", dir, "--headless", "--profile-path", "a.srs"]).unwrap();
        assert_eq!(args.config_dir.as_deref(), Some(tmp.as_path()));
        assert!(args.headless && !args.login_calling && !args.installer_calling);
        assert_eq!(args.profile_path, Some(PathBuf::from("a.srs")));
        assert!(parse(&["--config-dir", &format!("{dir}/missing")]).is_err());
        std::fs::remove_dir_all(tmp).unwrap();
    }

    /// The write probe: a missing folder is created when its parent exists, an existing one is
    /// written to and left as it was; a folder whose parent is missing cannot be created.
    #[test]
    fn write_probe_rules() {
        let tmp = scratch("probe");
        let root = tmp.join("openrigcue");
        write_probe(&root).unwrap();
        assert!(root.is_dir());
        write_probe(&root).unwrap();
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
        assert_eq!(write_probe(&tmp.join("a/b")).unwrap_err().0, "create");
        std::fs::remove_dir_all(tmp).unwrap();
    }

    /// The start-up call is ignored only with "Start with login" stored off; a fresh folder starts.
    #[test]
    fn login_calling_follows_start_with_login() {
        let tmp = scratch("login");
        let dir = ConfigDir::at(tmp.join("openrigcue"));
        let args = parse(&["--login-calling"]).unwrap();
        assert!(start_with_login(&dir).unwrap());
        std::fs::create_dir_all(dir.root()).unwrap();
        let mut settings = Settings::default();
        settings.app.start_with_login = false;
        settings.save(&dir.settings_file()).unwrap();
        assert!(!prepare(&args, &dir).unwrap());
        let registry =
            openrigcue_config::import::sqlitedict::PendingTable::open(&dir.registry_file(), "registry").unwrap();
        registry.execute_batch("INSERT INTO registry (key,value) VALUES ('opaque',X'ff')").unwrap();
        registry.commit().unwrap();
        assert!(!prepare(&args, &dir).unwrap(), "an unrelated bad pickle cannot hide the off flag");
        drop(registry);
        std::fs::remove_file(dir.settings_file()).unwrap();
        assert!(!prepare(&args, &dir).unwrap(), "the registry off flag precedes a missing INI");
        std::fs::write(dir.settings_file(), "[broken").unwrap();
        assert!(!prepare(&args, &dir).unwrap(), "the registry off flag precedes a corrupt INI");
        std::fs::remove_dir_all(tmp).unwrap();
    }

    #[test]
    fn login_flag_native_values_and_invalid_values_precede_ini_access() {
        use openrigcue_config::import::sqlitedict;
        use openrigcue_model::native_value::LegacyValue;
        let tmp = scratch("native-login");
        let dir = ConfigDir::at(&tmp);
        let args = parse(&["--login-calling"]).unwrap();
        for (value, expected) in [
            (LegacyValue::from("0"), Some(false)),
            (LegacyValue::from("1"), Some(true)),
            (LegacyValue::Bool(false), Some(false)),
            (LegacyValue::Int(0), Some(false)),
            (LegacyValue::Int(1), Some(true)),
            (LegacyValue::None, Some(true)),
            (LegacyValue::from("no"), Some(false)),
            (LegacyValue::from("maybe"), None),
            (LegacyValue::Float(1.0), None),
            (LegacyValue::List(Vec::new()), None),
        ] {
            sqlitedict::set_item(&dir.registry_file(), "registry", "StartWithWindows", &value).unwrap();
            match expected {
                Some(expected) => assert_eq!(start_with_login(&dir).unwrap(), expected, "{value:?}"),
                None => {
                    assert!(prepare(&args, &dir).is_err(), "{value:?}");
                    assert!(!dir.settings_file().exists());
                    assert!(!dir.root().join("log.txt").exists());
                }
            }
        }
        assert!(prepare(&parse(&[]).unwrap(), &dir).unwrap(), "manual launch does not inspect a bad login flag");
        std::fs::remove_dir_all(tmp).unwrap();
    }

    /// The instance count is the number of processes whose program file has the name and whose subcommand is
    /// `run`, in a fake `/proc`.
    #[test]
    fn instances_by_program_name() {
        let tmp = scratch("proc");
        for (pid, exe, cmdline) in [
            ("1", "/usr/bin/openrigcue", "openrigcue\0run\0"),
            ("22", "/opt/x/openrigcue", "/opt/x/openrigcue\0--log\0debug\0run\0--headless\0"),
            ("23", "/opt/x/openrigcue", "openrigcue\0--log=run\0run\0"),
            ("3", "/usr/bin/bash", "bash\0run\0"),
            ("4", "/usr/bin/openrigcue", "openrigcue\0srs-sync\0"),
            ("5", "/usr/bin/openrigcue", "openrigcue\0--log\0run\0games\0"),
            ("6", "/usr/bin/openrigcue", "openrigcue\0motion\0test\0run\0"),
            ("7", "/usr/bin/openrigcue", "openrigcue\0tools\0"),
            ("8", "/usr/bin/openrigcue", ""),
        ] {
            std::fs::create_dir(tmp.join(pid)).unwrap();
            std::os::unix::fs::symlink(exe, tmp.join(pid).join("exe")).unwrap();
            std::fs::write(tmp.join(pid).join("cmdline"), cmdline).unwrap();
        }
        // A process without a readable argument list is not counted.
        std::fs::create_dir(tmp.join("9")).unwrap();
        std::os::unix::fs::symlink("/usr/bin/openrigcue", tmp.join("9").join("exe")).unwrap();
        std::fs::create_dir(tmp.join("self")).unwrap();
        assert_eq!(count_instances(&tmp, std::ffi::OsStr::new("openrigcue")), 3);
        std::fs::remove_dir_all(tmp).unwrap();
    }

    /// A permission error reaches the dialog callback with the summary and the details, which name the
    /// operation, the path, the error and the start-up steps taken before it.
    #[test]
    fn permission_error_notifies_with_summary_and_details() {
        let tmp = scratch("permission");
        let dir = ConfigDir::at(tmp.join("missing/openrigcue"));
        let seen = std::cell::RefCell::new(None);
        let started = prepare_with_splash(
            &parse(&[]).unwrap(),
            &dir,
            || Ok(()),
            |summary, details| *seen.borrow_mut() = Some((summary.to_owned(), details.to_owned())),
        );
        assert!(!started.unwrap());
        let (summary, details) = seen.into_inner().expect("the dialog callback ran");
        let root = dir.root().display().to_string();
        assert_eq!(
            summary,
            format!(
                "The app cannot create {root}. Please make sure that this folder is not protected in your \
                 anti-virus software, or that the application is added to the anti-virus trusted app list."
            )
        );
        assert!(details.starts_with("OpenRigCue startup permission error\n\noperation: create\n"), "{details}");
        for line in [
            format!("path: {root}"),
            format!("app_version: {}", env!("CARGO_PKG_VERSION")),
            format!("config_dir: {:?}", dir.root()),
            format!("log_file: {:?}", dir.root().join("log.txt")),
            "exception_type: NotFound".to_owned(),
            "errno: 2".to_owned(),
            "startup_events:".to_owned(),
            "Loading splash screen".to_owned(),
            format!("Creating config folder: {:?}", dir.root()),
            format!("Startup permission error during create: {root}"),
        ] {
            assert!(details.lines().any(|found| found == line), "missing {line:?} in {details}");
        }
        // A file names itself as a file in the summary.
        let log = tmp.join("log.txt");
        std::fs::write(&log, "").unwrap();
        assert!(permission_summary(&log, "open log file in").contains("Please make sure that this file is not"));
        std::fs::remove_dir_all(tmp).unwrap();
    }
}

#[cfg(test)]
mod log_level_tests {
    use super::filter_for_python_level;

    /// The numeric log levels: Critical 50, Error 40, Warning 30, Info 20, Debug 10.
    #[test]
    fn python_levels() {
        let levels: Vec<&str> = [50, 40, 30, 20, 10, 0].into_iter().map(filter_for_python_level).collect();
        assert_eq!(levels, ["off", "error", "warn", "info", "debug", "trace"]);
        assert_eq!(filter_for_python_level(25), "warn");
    }
}
