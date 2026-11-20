//! openrigcue command-line entry point.

mod diag;
mod motion_test;
mod probe;
mod run;
mod setup;
mod srs_sync;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "openrigcue", version, about = "Motion cueing and sim effects")]
struct Cli {
    /// Log level filter, e.g. `info`, `debug`, `openrigcue_devices=trace`. Without it `run` follows the App page's
    /// Logging level (`APP.log_level`, DEBUG until it applies) and the other commands log at `info`.
    #[arg(long, global = true)]
    log: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Native controller setup, firmware maintenance and USB diagnostics.
    Tools(openrigcue_tools::cli::Options),
    /// Run the application (telemetry, motion, shakers, accessories, the UI's web server).
    Run(run::Args),
    /// Import a Sim Racing Studio documents folder as the openrigcue configuration.
    ImportSrs(setup::ImportArgs),
    /// Store your SRS Premium key privately for optional profile downloads.
    SrsKey(srs_sync::KeyArgs),
    /// Download new or changed SRS cloud profiles into a separate local library.
    SrsSync(srs_sync::SyncArgs),
    /// List the Games page entries and their status.
    Games(setup::GamesArgs),
    /// Configure a game to send telemetry (the Games page "config" action).
    Patch(setup::PatchArgs),
    /// Show where the Proton bridge is and the Steam launch option that starts it.
    BridgePath,
    /// List serial devices and identify motion controllers.
    Probe(probe::Args),
    /// List the platform models openrigcue knows.
    Platforms,
    /// Motion platform commands.
    #[command(subcommand)]
    Motion(MotionCommand),
}

#[derive(Subcommand)]
enum MotionCommand {
    /// Move one axis slowly back and forth to verify wiring and directions.
    Test(motion_test::Args),
    /// Scripted multi-axis sequence with controller feedback logged to CSV.
    Diag(diag::Args),
}

mod log_file;

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let command = match cli.command {
        Command::Run(args) => return run_app(args, cli.log),
        Command::Tools(args) => {
            return openrigcue_tools::cli::run(args).map_err(|error| anyhow::anyhow!(error.to_string()))
        }
        // Credential-bearing requests deliberately have no tracing/log subscriber.
        Command::SrsKey(args) => return srs_sync::setup_key(args),
        Command::SrsSync(args) => return srs_sync::run(args),
        command => command,
    };
    let filter = tracing_subscriber::EnvFilter::try_new(cli.log.as_deref().unwrap_or("info"))?;
    tracing_subscriber::registry().with(filter).with(tracing_subscriber::fmt::layer().with_target(false)).init();
    match command {
        Command::Run(_) | Command::Tools(_) => unreachable!("handled above"),
        Command::SrsKey(_) | Command::SrsSync(_) => unreachable!("handled above"),
        Command::ImportSrs(args) => setup::import(args),
        Command::Games(args) => setup::games(args),
        Command::Patch(args) => setup::patch(args),
        Command::BridgePath => setup::bridge_path(),
        Command::Probe(args) => probe::run(&args),
        Command::Platforms => {
            for id in openrigcue_cueing::builtin_ids() {
                let g = openrigcue_cueing::builtin(id).expect("listed")?;
                println!("{id:<60} {} actuators, {} controller(s)", g.motors, g.controllers);
            }
            Ok(())
        }
        Command::Motion(MotionCommand::Test(args)) => motion_test::run(&args),
        Command::Motion(MotionCommand::Diag(args)) => diag::run(&args),
    }
}

/// `openrigcue run`: the checks before the log file, the log file (`<documents>/log.txt`,
/// emptied at every start, logging at DEBUG until the App page's
/// Logging level applies), then the application.
fn run_app(args: run::Args, log: Option<String>) -> anyhow::Result<()> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    run::startup_event(&format!("Parsed arguments: {args:?}"));
    let dir = run::config_dir(args.config_dir.clone())?;
    run::startup_event(&format!(
        "Resolved startup paths: config_dir={:?}, log_file={:?}, executable={:?}",
        dir.root(),
        dir.root().join("log.txt"),
        std::env::current_exe().unwrap_or_default()
    ));
    let desktop = std::cell::RefCell::new(None);
    // The permission error dialog. A headless start has no desktop shell yet; the dialog is still shown when
    // a display can be reached.
    let notify = |summary: &str, details: &str| {
        let mut desktop = desktop.borrow_mut();
        if desktop.is_none() {
            *desktop = openrigcue_desktop::Desktop::new().ok();
        }
        if let Some(shell) = desktop.as_ref() {
            let shown = shell.details_dialog(run::PERMISSION_ERROR_TITLE, summary, run::PERMISSION_ERROR_HINT, details);
            if let Err(error) = shown {
                eprintln!("showing the permission error dialog: {error}");
            }
        }
    };
    if !run::prepare_with_splash(
        &args,
        &dir,
        || {
            let mut shell = openrigcue_desktop::Desktop::new()?;
            shell.splash(include_bytes!("../../openrigcue-ui/res/images/openrigcue/splash.png"), || {
                run::show_splash_screen(&dir)
                    .map_err(|error| openrigcue_desktop::DesktopError::Operation(error.to_string()))
            })?;
            *desktop.borrow_mut() = Some(shell);
            Ok(())
        },
        notify,
    )? {
        return Ok(());
    }
    let log_path = dir.root().join("log.txt");
    run::startup_event(&format!("Initializing file logger: {log_path:?}"));
    let file = match log_file::open(&log_path) {
        Ok(file) => file,
        Err(error) => {
            // A log file that cannot be opened is a permission error like an unwritable folder.
            let Some(io) = error.downcast_ref::<std::io::Error>() else { return Err(error) };
            run::permission_error(&dir, &log_path, "open log file in", io, notify);
            return Ok(());
        }
    };
    run::startup_event("File logger initialized");
    let desktop = desktop.into_inner();
    let (filter, filter_handle) = tracing_subscriber::reload::Layer::new(tracing_subscriber::EnvFilter::try_new(
        log.as_deref().unwrap_or("debug"),
    )?);
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .event_format(log_file::LineFormat)
                .with_writer(std::sync::Mutex::new(file)),
        )
        .init();
    run::log_start();
    let log_level = log.is_none().then(|| run::follow_log_level(filter_handle));
    run::run(args, log_level, desktop)
}
