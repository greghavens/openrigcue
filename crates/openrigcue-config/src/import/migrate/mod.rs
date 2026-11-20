//! The configuration migration chain.
//!
//! `config.ini`, `profile.db` and the `metadata.db` tables are upgraded together with one ordered
//! list of version steps (84 → 412) gated on the version found in `APP.config_file_version`.
//! The chain is used twice: to build the first-run setup
//! (templates + every step, with the first-installation branches), and to
//! bring an imported SRS setup of any version up to the current schema before converting it.
//!
//! The chain works on [`LegacyState`] (dynamic values) because the shapes it passes through are
//! not the final schema. When the chain runs at the start of the app on its own files
//! ([`MigrationSources::effects`]), the steps' file side effects are performed
//! (deleting `<docs>/profiles.json` and `<docs>/crash.txt`, re-patching DCS). Without them (an
//! import), and for the side effects that are left out (analytics id, updater, sales-gating marker
//! files), [`MigrationReport::skipped_side_effects`] lists the ones a run would have performed.

mod steps;
mod views;

use views::{as_dict, DictView};

use std::path::{Path, PathBuf};

use crate::catalog::registry::Registry;
use crate::patch::PatchContext;

#[cfg(test)]
use super::value::LegacyValue;
use super::value::{BigInt, Dict};

/// The operating system the chain runs for; several steps branch on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Windows,
    Linux,
}

/// The stores the chain reads and writes.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LegacyState {
    /// `config.ini` as nested dicts.
    pub config: Dict,
    /// `profile.db` table `unnamed`: game id → tune dict.
    pub profiles: Dict,
    /// `metadata.db` table `shakers`: layout name → channel dict.
    pub shaker_layouts: Dict,
    /// `metadata.db` table `joystick`: profile name → axis mapping.
    pub joysticks: Dict,
    /// `metadata.db` table `game_config_notifications`: dismissed "game not configured" notices.
    pub game_config_notifications: Dict,
    /// `metadata.db` table `notifications`: dismissed drivers, telemetry markers, instructions.
    pub notifications: Dict,
    /// The launcher/registry values:
    /// `StartWithWindows`, `ShowSplashScreen`, `Soundcard`, `MotionModel`. Empty at the start
    /// (an imported folder carries no registry either); the Windows
    /// steps v230, v288 and v359 read and write it.
    pub registry: Dict,
}

/// What the imported folder's `<docs>/profiles.json` holds (v99 reads it).
#[derive(Debug, Clone, PartialEq, Default)]
pub enum LegacyLayouts {
    /// No such file: v99 seeds the shipped layouts.
    #[default]
    Absent,
    /// Startup opens this input only if execution reaches v99.
    DeferredFile(PathBuf),
    /// The file exists but does not open or parse: v99 seeds the shipped layouts.
    Unreadable,
    /// A JSON object: layout name → layout.
    Layouts(Dict),
    /// Valid JSON that is not an object (its type name): v99 fails.
    NotAnObject(&'static str),
}

/// Read-only data the steps copy from.
#[derive(Debug, Clone, Copy)]
pub struct MigrationSources<'a> {
    pub platform: Platform,
    /// The factory tune of every game.
    pub master_tunes: &'a Dict,
    /// The expert tune sets.
    pub expert_tunes: &'a Dict,
    /// Flight effect levels per built-in layout.
    pub flight_shaker_levels: &'a Dict,
    /// The shipped shaker layouts.
    pub shipped_layouts: &'a Dict,
    /// `<docs>/profiles.json` of the source folder (consumed at v99).
    pub legacy_layouts: &'a LegacyLayouts,
    /// The layout added at v345.
    pub ushake6_enhanced: &'a Dict,
    pub registry: &'a Registry,
    /// Set when the chain runs at the start of the app
    /// on the files in [`StartEffects::docs`]: the steps then perform their
    /// file side effects. `None` for an import, which changes nothing outside the four stores.
    pub effects: Option<StartEffects<'a>>,
}

/// What the steps' side effects act on at the start of the app.
#[derive(Clone, Copy)]
pub struct StartEffects<'a> {
    /// The folder holding the configuration files.
    pub docs: &'a Path,
    /// Where the game-patch modules find the games, for v214 (`dcs_legacy` / `dcs`); the ports they
    /// compare and write are read from the `config.ini` being migrated. `None`: v214 is reported
    /// as skipped.
    pub patch: Option<&'a PatchContext<'a>>,
}

impl std::fmt::Debug for StartEffects<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StartEffects").field("docs", &self.docs).field("patch", &self.patch.is_some()).finish()
    }
}

/// A failed step: the start fails and, on the Windows platform, the configuration is factory-reset.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("SRS configuration migration step {version} failed: {message}")]
pub struct MigrationError {
    pub version: u32,
    pub message: String,
}

/// What a run did.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MigrationReport {
    /// `APP.config_file_version` before the run.
    pub from_version: BigInt,
    /// Versions whose step ran.
    pub applied: Vec<u32>,
    /// Side effects of applied steps that openrigcue does not perform.
    pub skipped_side_effects: Vec<String>,
}

/// The configuration schema version the chain ends at.
pub const CURRENT_VERSION: u32 = 412;

/// Retained open dictionaries handed from startup to the running service.
#[doc(hidden)]
#[derive(Debug, Default)]
pub struct RuntimeDatabases {
    pub profile: Option<super::sqlitedict::PendingTable>,
    pub metadata: std::collections::BTreeMap<String, super::sqlitedict::AutocommitTable>,
}

/// Runs every step newer than the state's `APP.config_file_version`, then marks the installation as
/// no longer first-time.
pub fn migrate(state: &mut LegacyState, sources: &MigrationSources<'_>) -> Result<MigrationReport, MigrationError> {
    migrate_inner(state, sources, None)
}

/// Runtime consumers read canonical rows after replacement instead of taking
/// an eager typed snapshot of every reopened dictionary.
pub(crate) fn migrate_for_runtime(
    state: &mut LegacyState,
    sources: &MigrationSources<'_>,
    databases: &mut RuntimeDatabases,
) -> Result<MigrationReport, MigrationError> {
    migrate_inner(state, sources, Some(databases))
}

fn migrate_inner(
    state: &mut LegacyState,
    sources: &MigrationSources<'_>,
    mut databases: Option<&mut RuntimeDatabases>,
) -> Result<MigrationReport, MigrationError> {
    steps::restore_hidden_defaults(&mut state.config);
    let mut m = steps::Migrator::new(state, sources)?;
    m.snapshot_rebuild = databases.is_none();
    if let Some(databases) = databases.as_mut() {
        m.profile_db = databases.profile.take();
        m.metadata_db = std::mem::take(&mut databases.metadata);
    }
    let from_version = m.file_version.clone();
    for step in steps::STEPS {
        if m.file_version < BigInt::from(step.version) {
            m.current = step.version;
            let fail = |message| MigrationError { version: step.version, message };
            (step.apply)(&mut m).map_err(fail)?;
            if step.advances_cursor() {
                m.set("APP", "config_file_version", i64::from(step.version).into()).map_err(fail)?;
            }
            m.report.applied.push(step.version);
        }
    }
    m.set("APP", "first_time_installation", "no".into())
        .map_err(|message| MigrationError { version: CURRENT_VERSION, message })?;
    m.drop_4d_videos().map_err(|message| MigrationError { version: CURRENT_VERSION, message })?;
    let mut report = std::mem::take(&mut m.report);
    report.from_version = from_version;
    if let Some(databases) = databases {
        databases.profile = m.profile_db.take();
        databases.metadata = std::mem::take(&mut m.metadata_db);
    }
    Ok(report)
}

#[cfg(test)]
mod tests;
