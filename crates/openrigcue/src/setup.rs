//! `openrigcue import-srs`, `openrigcue games`, `openrigcue patch` and `openrigcue bridge-path`.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use openrigcue_config::catalog::Catalog;
use openrigcue_config::detect::Snapshotter;
use openrigcue_config::import::import_srs_folder;
use openrigcue_config::import::migrate::Platform;
use openrigcue_config::settings::paths::ConfigDir;
use openrigcue_config::settings::{Configuration, RESET_NOTICE_TEXT, RESET_NOTICE_TITLE};
use openrigcue_daemon::games::proton_bridge::{self, BRIDGE_EXE, BRIDGE_RUN};
use openrigcue_daemon::games::{GameSetup, GamesPage};
use openrigcue_daemon::telemetry::{scan_hits, system_titles};

use crate::run::config_dir;

#[derive(clap::Args)]
pub struct ImportArgs {
    /// The SRS documents folder (`Documents\SimRacingStudio 2.0`) or a copy of it.
    pub path: PathBuf,
    #[arg(long)]
    pub config_dir: Option<PathBuf>,
    /// Replace an existing openrigcue configuration.
    #[arg(long)]
    pub force: bool,
}

/// Converts an SRS folder and writes it as the openrigcue configuration.
pub fn import(args: ImportArgs) -> anyhow::Result<()> {
    let dir = config_dir(args.config_dir)?;
    if dir.settings_file().exists() && !args.force {
        bail!("{} already holds a configuration; pass --force to replace it", dir.root().display());
    }
    let imported = import_srs_folder(&args.path, Platform::Windows)
        .with_context(|| format!("importing {}", args.path.display()))?;
    imported.configuration.save(&dir)?;
    println!(
        "imported {} (config version {} migrated through {} steps) into {}",
        args.path.display(),
        imported.report.from_version,
        imported.report.applied.len(),
        dir.root().display()
    );
    for warning in &imported.warnings {
        println!("  {}: {}", warning.location, warning.message);
    }
    for effect in &imported.report.skipped_side_effects {
        println!("  not performed: {effect}");
    }
    Ok(())
}

#[derive(clap::Args)]
pub struct GamesArgs {
    /// List the Console page instead of the PC page.
    #[arg(long)]
    pub console: bool,
    #[arg(long)]
    pub config_dir: Option<PathBuf>,
}

#[derive(clap::Args)]
pub struct PatchArgs {
    /// Games-page entry key, for example `dirtrally2.0_steam` or `f12024` (see `openrigcue games`).
    pub game: String,
    #[arg(long)]
    pub config_dir: Option<PathBuf>,
}

/// The game-side patch modules over OpenRigCue's built-in game files (nothing is written for them). Fails
/// when a `[NETWORK_PORT]` key the modules read at start is missing.
fn setup(config: &Configuration) -> anyhow::Result<GameSetup> {
    let home = std::env::var_os("HOME").map(PathBuf::from).context("$HOME is not set")?;
    GameSetup::start(&home, &config.settings).context("loading the game configuration modules")
}

/// Whether a game is running, as the main loop's scan decides it: a catalog game found by
/// its window title or process name (one snapshot of `/proc` and the window list).
fn game_running(config: &Configuration) -> bool {
    let catalog = Catalog::builtin();
    let snapshot = Snapshotter::new(system_titles(), PathBuf::from("/proc")).take(None, catalog.games());
    !scan_hits(&snapshot, config.settings.features.disable_rfpro).is_empty()
}

/// The configuration as the application starts it: missing files are filled from the built-in templates and
/// written; a failed start resets the files.
fn open_configuration(dir: &ConfigDir) -> anyhow::Result<Configuration> {
    let opened = Configuration::open_for_runtime(dir, Platform::Windows, None)
        .with_context(|| format!("starting the configuration in {}", dir.root().display()))?;
    if let Some(reason) = &opened.reset {
        eprintln!("{RESET_NOTICE_TITLE}\n{RESET_NOTICE_TEXT}\n({reason})");
    }
    Ok(opened.configuration)
}

/// The Games page table with each entry's status.
pub fn games(args: GamesArgs) -> anyhow::Result<()> {
    let dir = config_dir(args.config_dir)?;
    let config = open_configuration(&dir)?;
    let setup = setup(&config)?;
    let page = if args.console { GamesPage::Console } else { GamesPage::Pc };
    let native = config.settings.native_snapshot();
    let table =
        setup.load_table_native(page, &config.settings.network_ports, &config.settings.port_forwarding, &native);
    for row in &table.rows {
        let port = row.port.map_or(String::new(), |p| format!("port {p}"));
        println!("{:<28} {:<45} {:<14} {port}", row.key, row.name, format!("{:?}", row.status));
    }
    // The table ends at the entry whose installed/patched check failed: the rows before it are shown and
    // that error is the command's failure.
    if let Some(raised) = table.raised(&native) {
        anyhow::bail!("{raised}");
    }
    Ok(())
}

/// The Games-page "config" action for one entry: refused while a catalog game is running.
pub fn patch(args: PatchArgs) -> anyhow::Result<()> {
    let dir = config_dir(args.config_dir)?;
    let config = open_configuration(&dir)?;
    let setup = setup(&config)?;
    setup.patch_native(
        &args.game,
        game_running(&config),
        &config.settings.network_ports,
        &config.settings.native_snapshot(),
    )?;
    println!("{}: configured", args.game);
    Ok(())
}

/// Prints where the bridge and its Steam launch wrapper are, and the launch option to use. Run from the AppImage,
/// whose mount point changes on every start, the two files are first copied to `$XDG_DATA_HOME/openrigcue/bridge`
/// (Steam's runtime container sees everything under `$HOME`). The Games page's "config" action of a game that
/// needs the bridge writes the same launch option itself; this command is for games it does not find in Steam.
pub fn bridge_path() -> anyhow::Result<()> {
    let candidates = proton_bridge::packaged_dirs();
    let Some(found) = candidates.iter().find(|dir| dir.join(BRIDGE_RUN).is_file()) else {
        bail!(
            "{BRIDGE_RUN} not found in {}",
            candidates.iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join(", ")
        );
    };
    let dir = if std::env::var_os("APPIMAGE").is_some() { install_bridge(found)? } else { found.canonicalize()? };
    let exe = dir.join(BRIDGE_EXE);
    let run = dir.join(BRIDGE_RUN);
    println!("bridge:        {}{}", exe.display(), if exe.is_file() { "" } else { " (missing)" });
    println!("launch helper: {}", run.display());
    println!("Steam launch options: \"{}\" %command%", run.display());
    Ok(())
}

/// Copies the bridge files out of the AppImage to a stable folder.
fn install_bridge(from: &Path) -> anyhow::Result<PathBuf> {
    let data = match std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME").context("$HOME is not set")?).join(".local/share"),
    };
    let dir = data.join("openrigcue").join("bridge");
    proton_bridge::install(from, &dir).with_context(|| format!("copying the bridge to {}", dir.display()))?;
    Ok(dir)
}
