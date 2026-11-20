//! The Games and Console pages: the game table with its status column, the "config" action that patches a game's
//! files, the UDP port and port-forwarding editor, the in-app instructions, and the game status for the
//! notifications.

use std::path::{Path, PathBuf};

use openrigcue_config::import::value::{Dict, LegacyValue};
use openrigcue_config::patch::library::BUILTIN;
use openrigcue_config::patch::{
    candidate_prefixes, game_status, EntryKind, GameEntry, GameStatus, ModulePorts, PatchContext, PatchError, PortKey,
    PortLookup, SteamLibraries, WinePrefix, GAME_ENTRIES,
};
use openrigcue_config::settings::{Dismissals, NetworkPorts};
use serde::Serialize;

#[cfg(test)]
pub(crate) mod bridge_tests;
pub mod proton_bridge;

use proton_bridge::{BridgeError, BridgeSetup, SteamView};

/// Status cell of one Games-page row (`not_installed`, `instructions`, `config`, `ready_to_play`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RowStatus {
    NotInstalled,
    Instructions,
    /// Installed and openrigcue can configure it (the status button is enabled).
    Config,
    ReadyToPlay,
}

/// The page an entry is shown on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, Serialize)]
pub enum GamesPage {
    #[serde(rename = "PC")]
    Pc,
    Console,
}

/// The entry's own type, whatever the row shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryType {
    ReadyToPlay,
    Instructions,
    Config,
    Dll,
}

impl EntryType {
    fn of(kind: &EntryKind) -> EntryType {
        match kind {
            EntryKind::ReadyToPlay => EntryType::ReadyToPlay,
            EntryKind::Instructions => EntryType::Instructions,
            EntryKind::Config(_) => EntryType::Config,
            EntryKind::Dll(_) => EntryType::Dll,
        }
    }
}

/// One row of the Games or Console page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GameRow {
    pub key: &'static str,
    pub name: &'static str,
    pub status: RowStatus,
    /// The entry's type, which the port dialog receives (its hint shows
    /// for `instructions` entries, on the Console page too).
    pub entry_type: EntryType,
    /// `NETWORK_PORT` key edited from the row, shown unless the game is not installed.
    pub port_key: Option<&'static str>,
    pub port: Option<u16>,
    /// The forwarding targets of `port_key`.
    pub forwarding: Vec<String>,
    /// The game only sends telemetry when it is started through the bridge (PC page): the row offers the
    /// launch steps for a launcher other than Steam.
    pub bridge: bool,
}

/// Why a Games-page action was refused or failed.
#[derive(Debug, thiserror::Error)]
pub enum GamesError {
    /// The "Game Running" alert of a status button click.
    #[error("A game is running, so OpenRigCue cannot set the game up. Close the game, then try again.")]
    GameRunning,
    /// The alert a port button click opens while a game runs.
    #[error("Game port settings are locked while a profile is in use. Exit the game, then try again")]
    PortWhileRunning,
    #[error("no Games-page entry {0:?}")]
    UnknownEntry(String),
    #[error("entry {0:?} has nothing to configure")]
    NothingToConfigure(String),
    #[error("entry {0:?} has no UDP port setting")]
    NoPort(String),
    /// The game's `NETWORK_PORT` value is missing or not an integer when the port modal opens.
    #[error("NETWORK_PORT has no key {0:?}")]
    MissingPort(String),
    /// The forwarding address entered is not a valid IP address.
    #[error("{0} is not a correctly formed IP address")]
    InvalidIp(String),
    #[error("Forwarding to this IP and Port is already set up")]
    DuplicateTarget,
    /// The entry's patch failed (the status button then reads "Error").
    #[error("configuring {0:?} failed")]
    PatchFailed(String),
    #[error(transparent)]
    Patch(#[from] PatchError),
    #[error("no instructions page {0:?}")]
    NoInstructions(String),
    /// The game's Steam launch options cannot be changed while Steam runs: it would write its own copy back.
    #[error("Steam is running, so OpenRigCue cannot set the game up. Close Steam, then try again.")]
    SteamRunning,
    /// The bridge part of the entry's configuration failed for another reason.
    #[error("configuring {0:?} failed: {1}")]
    Bridge(String, BridgeError),
}

/// Where the game-side patch modules look for games. Their templates and plugin files are the built-in library
/// ([`openrigcue_config::patch::library`]).
pub struct GameSetup {
    steam: Option<SteamLibraries>,
    prefixes: Vec<WinePrefix>,
    loaded_ports: ModulePorts,
    /// The Proton bridge part of a game's configuration ([`proton_bridge`]).
    bridge: BridgeSetup,
}

/// The configured ports as the patch modules look them up.
fn port_lookup(ports: &NetworkPorts) -> impl Fn(PortKey) -> Option<u16> + '_ {
    move |key: PortKey| ports.get(key.config_key())
}

/// The numeric port view, alongside the stored port text that the live templates read.
struct NativePatchPorts<'a> {
    numeric: &'a NetworkPorts,
    native: &'a Dict,
}

impl PortLookup for NativePatchPorts<'_> {
    fn port(&self, key: PortKey) -> Option<u16> {
        self.numeric.get(key.config_key())
    }

    fn port_text(&self, key: PortKey) -> Option<String> {
        self.native.get("NETWORK_PORT")?.as_dict()?.get(key.config_key())?.py_str()
    }

    fn port_value(&self, key: PortKey) -> Option<LegacyValue> {
        self.native.get("NETWORK_PORT")?.as_dict()?.get(key.config_key()).cloned()
    }
}

impl GameSetup {
    /// The patch modules as the application starts them: Steam under `home`, every prefix a game can live in
    /// (Proton prefixes of every Steam library, then `$WINEPREFIX`, `~/.wine`, Lutris, Heroic and Bottles
    /// prefixes), and every module's port loaded at start-up. A `[NETWORK_PORT]` key
    /// those modules need at start-up that is missing fails the start.
    pub fn start(home: &Path, ports: &dyn openrigcue_config::patch::PortLookup) -> Result<GameSetup, PatchError> {
        let loaded_ports = ModulePorts::imported(ports)?;
        let wine_prefix = std::env::var_os("WINEPREFIX").filter(|value| !value.is_empty()).map(PathBuf::from);
        let setup = Self::with_ports(home, wine_prefix.as_deref(), loaded_ports, BridgeSetup::for_service(home));
        // A bridge folder set up by an earlier version gets this version's files, so configured games stay
        // configured after an update.
        setup.bridge.refresh(setup.steam.as_ref());
        Ok(setup)
    }

    /// As [`GameSetup::start`], but a missing start-up port only fails the modules that read it, and
    /// `$WINEPREFIX` is not consulted (for callers that cannot stop on the error, and tests).
    pub fn discover(home: &Path, ports: &NetworkPorts) -> GameSetup {
        Self::with_ports(home, None, ModulePorts::at_startup(&port_lookup(ports)), BridgeSetup::disabled(home))
    }

    /// Where the games are, before the configuration is read: what the configuration migration step that
    /// re-patches DCS runs in ([`GameSetup::migration_context`]). Steam and the prefixes are found as in
    /// [`GameSetup::start`]; no port is loaded (the step reads the configuration being migrated).
    pub fn locate(home: &Path) -> GameSetup {
        let wine_prefix = std::env::var_os("WINEPREFIX").filter(|value| !value.is_empty()).map(PathBuf::from);
        let ports = ModulePorts::at_startup(&|_: PortKey| None);
        Self::with_ports(home, wine_prefix.as_deref(), ports, BridgeSetup::disabled(home))
    }

    /// Calls `f` with the patch context of [`GameSetup::locate`] (no configured port).
    pub fn migration_context<R>(&self, f: impl FnOnce(&PatchContext<'_>) -> R) -> R {
        let lookup = |_: PortKey| None;
        f(&self.context(&lookup))
    }

    fn with_ports(
        home: &Path,
        wine_prefix: Option<&Path>,
        loaded_ports: ModulePorts,
        bridge: BridgeSetup,
    ) -> GameSetup {
        let steam = SteamLibraries::discover(home);
        let prefixes = candidate_prefixes(home, steam.as_ref(), wine_prefix);
        GameSetup { steam, prefixes, loaded_ports, bridge }
    }

    /// The same set-up with another bridge set-up (explicit locations).
    pub fn with_bridge(mut self, bridge: BridgeSetup) -> GameSetup {
        self.bridge = bridge;
        self
    }

    /// Steam's applications and launch options now, when this program can set the bridge up at all.
    fn bridge_view(&self) -> Option<SteamView> {
        self.bridge.view(self.steam.as_ref())
    }

    /// Whether `entry`'s game is in a Steam library and Steam does not start the bridge with it yet.
    fn bridge_pending(&self, view: Option<&SteamView>, entry: &GameEntry) -> bool {
        match (view, &self.steam) {
            (Some(view), Some(steam)) if entry.platforms.pc => {
                view.app_for(entry, steam).is_some_and(|app| !view.configured(app))
            }
            _ => false,
        }
    }

    /// `game_status` with the bridge: a game whose own configuration is in order, but which Steam does not start
    /// with the bridge, is not configured yet. A forced game counts as instructions.
    fn with_bridge_status(&self, game: &str, forced: bool, status: GameStatus) -> GameStatus {
        if forced || status == GameStatus::NotPatched {
            return status;
        }
        let entries: Vec<&GameEntry> = proton_bridge::BRIDGE_GAMES
            .iter()
            .filter(|rule| rule.catalog == game)
            .filter_map(|rule| GAME_ENTRIES.iter().find(|entry| entry.key == rule.entry))
            .collect();
        if entries.is_empty() {
            // Not a game of a bridge listener: Steam's files are not read for it.
            return status;
        }
        let view = self.bridge_view();
        if entries.iter().any(|entry| self.bridge_pending(view.as_ref(), entry)) {
            GameStatus::NotPatched
        } else {
            status
        }
    }

    pub fn steam(&self) -> Option<&SteamLibraries> {
        self.steam.as_ref()
    }

    /// Where the bridge is and how a launcher other than Steam starts a game through it
    /// ([`BridgeSetup::launch`]).
    pub fn bridge_launch(&self) -> std::io::Result<Option<proton_bridge::BridgeLaunch>> {
        self.bridge.launch()
    }

    fn context<'a>(&'a self, lookup: &'a dyn openrigcue_config::patch::PortLookup) -> PatchContext<'a> {
        PatchContext {
            prefixes: &self.prefixes,
            steam: self.steam.as_ref(),
            library: &BUILTIN,
            ports: lookup,
            loaded_ports: &self.loaded_ports,
        }
    }

    /// The status of `game`, for the notification logic.
    pub fn status(
        &self,
        game: &str,
        forced: bool,
        dismissals: &Dismissals,
        ports: &NetworkPorts,
    ) -> Result<GameStatus, PatchError> {
        self.status_with_notification_lookup(game, forced, ports, &|id| dismissals.setup_instructions_suppressed(id))
    }

    pub(crate) fn status_with_notification_lookup(
        &self,
        game: &str,
        forced: bool,
        ports: &NetworkPorts,
        acknowledged: &dyn Fn(&str) -> bool,
    ) -> Result<GameStatus, PatchError> {
        let lookup = port_lookup(ports);
        let context = self.context(&lookup);
        Ok(self.with_bridge_status(game, forced, game_status(game, forced, acknowledged, &context)?))
    }

    pub(crate) fn status_native(
        &self,
        game: &str,
        forced: bool,
        ports: &NetworkPorts,
        native: &Dict,
        acknowledged: &dyn Fn(&str) -> bool,
    ) -> Result<GameStatus, PatchError> {
        let lookup = NativePatchPorts { numeric: ports, native };
        Ok(self.with_bridge_status(game, forced, game_status(game, forced, acknowledged, &self.context(&lookup))?))
    }

    /// The rows of a table in which no entry failed (fixtures).
    #[cfg(test)]
    pub(crate) fn rows(
        &self,
        page: GamesPage,
        ports: &NetworkPorts,
        forwarding: &std::collections::BTreeMap<String, Vec<String>>,
    ) -> Vec<GameRow> {
        self.load_table(page, ports, forwarding).rows
    }

    /// Loads a page's table: the entries of the page ordered by upper-cased name with their status,
    /// then the `Total <mode>Games Support: <rows - 1>` log line. An installed or patched check that fails ends
    /// the loop at that entry with the rows already appended, and the error is returned instead of the log line.
    pub fn load_table(
        &self,
        page: GamesPage,
        ports: &NetworkPorts,
        forwarding: &std::collections::BTreeMap<String, Vec<String>>,
    ) -> GamesTable {
        let lookup = port_lookup(ports);
        self.load_table_with_lookup(page, ports, forwarding, &lookup)
    }

    /// [`GameSetup::load_table`] over the stored settings snapshot the application patches with.
    pub fn load_table_native(
        &self,
        page: GamesPage,
        ports: &NetworkPorts,
        forwarding: &std::collections::BTreeMap<String, Vec<String>>,
        native: &Dict,
    ) -> GamesTable {
        self.load_table_with_lookup(page, ports, forwarding, &NativePatchPorts { numeric: ports, native })
    }

    fn load_table_with_lookup(
        &self,
        page: GamesPage,
        ports: &NetworkPorts,
        forwarding: &std::collections::BTreeMap<String, Vec<String>>,
        lookup: &dyn PortLookup,
    ) -> GamesTable {
        let context = self.context(lookup);
        let mut entries: Vec<&GameEntry> = GAME_ENTRIES
            .iter()
            .filter(|e| match page {
                GamesPage::Pc => e.platforms.pc,
                GamesPage::Console => e.platforms.console,
            })
            .collect();
        entries.sort_by_key(|e| e.name.to_uppercase());
        let bridge = if page == GamesPage::Pc { self.bridge_view() } else { None };
        let mut rows = Vec::with_capacity(entries.len());
        for entry in entries {
            let status = row_status(entry, page, |patcher| {
                let env = context.environment_for(patcher);
                let installed = patcher.installed(&env);
                Ok((installed, patcher.patched(&env)?))
            });
            let status = match status {
                Ok(status) => status,
                Err(error) => return GamesTable { rows, error: Some((entry.key, error)) },
            };
            // Under Proton the game also has to start the bridge: until Steam does, the entry is to configure.
            let status = match status {
                RowStatus::ReadyToPlay | RowStatus::Instructions if self.bridge_pending(bridge.as_ref(), entry) => {
                    RowStatus::Config
                }
                status => status,
            };
            let port_key = entry.port.filter(|_| status != RowStatus::NotInstalled);
            rows.push(GameRow {
                key: entry.key,
                name: entry.name,
                status,
                entry_type: EntryType::of(&entry.kind),
                port_key,
                port: port_key.and_then(|k| ports.get(k)),
                forwarding: port_key.and_then(|k| forwarding.get(k).cloned()).unwrap_or_default(),
                bridge: page == GamesPage::Pc && proton_bridge::needs_bridge(entry.key),
            });
        }
        let mode = match page {
            GamesPage::Pc => "PC",
            GamesPage::Console => "Console",
        };
        tracing::info!("Total {mode}Games Support: {}", rows.len() as i64 - 1);
        GamesTable { rows, error: None }
    }

    /// A status button click: refused while a game runs, else the entry is patched.
    pub fn patch(&self, key: &str, game_running: bool, ports: &NetworkPorts) -> Result<(), GamesError> {
        self.patch_with_lookup(key, game_running, &port_lookup(ports))
    }

    pub fn patch_native(
        &self,
        key: &str,
        game_running: bool,
        ports: &NetworkPorts,
        native: &Dict,
    ) -> Result<(), GamesError> {
        self.patch_with_lookup(key, game_running, &NativePatchPorts { numeric: ports, native })
    }

    fn patch_with_lookup(&self, key: &str, game_running: bool, lookup: &dyn PortLookup) -> Result<(), GamesError> {
        if game_running {
            return Err(GamesError::GameRunning);
        }
        let entry = entry(key)?;
        let view = self.bridge_view();
        let app = match (&view, &self.steam) {
            (Some(view), Some(steam)) if entry.platforms.pc => view.app_for(entry, steam),
            _ => None,
        };
        let context = self.context(lookup);
        match (entry.kind.patcher(), app) {
            (None, None) => return Err(GamesError::NothingToConfigure(key.to_owned())),
            (None, Some(_)) => {}
            // The game's own configuration is in order and only the bridge is missing: its files are not
            // written again.
            (Some(patcher), Some(_)) if patcher.patched(&context.environment_for(patcher))? => {}
            // A patch that reports failure and a patch that fails with an error both end in
            // "Error".
            (Some(patcher), _) => match patcher.patch(&context.environment_for(patcher)) {
                Ok(true) => {}
                Ok(false) => return Err(GamesError::PatchFailed(key.to_owned())),
                Err(error) => return Err(error.into()),
            },
        }
        match (app, &self.steam) {
            (Some(app), Some(steam)) => self.bridge.configure(steam, app).map_err(|error| {
                tracing::warn!(game = key, app = %app.id, %error, "setting up the Proton bridge failed");
                match error {
                    BridgeError::SteamRunning => GamesError::SteamRunning,
                    other => GamesError::Bridge(key.to_owned(), other),
                }
            }),
            _ => Ok(()),
        }
    }

    /// The patch that the port change confirmation runs after a port change of a `config` entry, when
    /// [`port_change_saves`] says the confirmation changes anything. Its result is
    /// ignored (the handler goes on to reload the table and show the restart alert); an error ends the handler.
    pub fn repatch_after_port_change(&self, key: &str, ports: &NetworkPorts) -> Result<(), GamesError> {
        self.repatch_with_lookup(key, &port_lookup(ports))
    }

    pub(crate) fn repatch_after_port_change_native(
        &self,
        key: &str,
        ports: &NetworkPorts,
        native: &Dict,
    ) -> Result<(), GamesError> {
        self.repatch_with_lookup(key, &NativePatchPorts { numeric: ports, native })
    }

    fn repatch_with_lookup(&self, key: &str, lookup: &dyn PortLookup) -> Result<(), GamesError> {
        let entry = entry(key)?;
        if let EntryKind::Config(patcher) = entry.kind {
            let context = self.context(lookup);
            patcher.patch(&context.environment_for(&patcher))?;
        }
        Ok(())
    }
}

/// What a table load left behind.
#[derive(Debug)]
pub struct GamesTable {
    /// The rows appended to the table.
    pub rows: Vec<GameRow>,
    /// The entry whose installed or patched check failed, and the error, when the loop ended early.
    pub error: Option<(&'static str, PatchError)>,
}

impl GamesTable {
    /// The error class name and message of the failure that ended the table load, where the error identifies
    /// them; `None` for a table without error and for an error that has no class name of its own.
    pub fn native_exception(&self, native_settings: &Dict) -> Option<(&'static str, String)> {
        let (game, error) = self.error.as_ref()?;
        // These sixteen entries read their game file whole, so a decoding error ends the load.
        // Forward only typed decoder sources; other errors keep the generic fallback.
        if matches!(
            *game,
            "condor"
                | "condor3"
                | "dcs"
                | "dcs_beta"
                | "fs2020_steam"
                | "fs2020_xbox"
                | "fs2024_steam"
                | "fs2024_xbox"
                | "fsx"
                | "kartkraft"
                | "p3d"
                | "p3d_v5"
                | "p3d_v6"
                | "thecrew"
                | "thecrew2"
                | "vracer"
        ) && matches!(error, PatchError::Io(_))
        {
            if let Some((class_name, message)) = error.native_exception() {
                return Some((class_name, message));
            }
        }
        // WRC converts its current port value to an integer before testing its config file.
        if *game == "wrc2023" && matches!(error, PatchError::PortInteger { .. }) {
            if let Some((class_name, message)) = error.native_exception() {
                return Some((class_name, message));
            }
        }
        // F1 2015's patched check reloads this port key before its guarded XML read.
        // Other modules can use previously loaded ports; MissingPort alone does
        // not identify their error class or a failed conversion.
        let port_key = match (game, error) {
            (&"f12015", PatchError::MissingPort(PortKey::F1)) => "f1",
            (&"wrc2023", PatchError::MissingPort(PortKey::Wrc2023)) => "wrc2023",
            _ => return None,
        };
        let key = match native_settings.get("NETWORK_PORT") {
            None => "NETWORK_PORT",
            Some(LegacyValue::Dict(section)) if !section.contains(port_key) => port_key,
            _ => return None,
        };
        Some(("KeyError", format!("'{key}'")))
    }

    /// The failure as the handler log names it (`<class>: <message>`). An error without a class name of its own
    /// is logged as `RuntimeError: <entry>: <error>`.
    pub fn raised(&self, native_settings: &Dict) -> Option<String> {
        let (game, error) = self.error.as_ref()?;
        Some(match self.native_exception(native_settings) {
            Some((class_name, message)) => format!("{class_name}: {message}"),
            None => format!("RuntimeError: {game}: {error}"),
        })
    }
}

/// The status column of a table row.
fn row_status(
    entry: &GameEntry,
    page: GamesPage,
    check: impl Fn(&openrigcue_config::patch::Patcher) -> Result<(bool, bool), PatchError>,
) -> Result<RowStatus, PatchError> {
    if page == GamesPage::Console {
        return Ok(if entry.key == "testmode" { RowStatus::ReadyToPlay } else { RowStatus::Instructions });
    }
    Ok(match &entry.kind {
        EntryKind::ReadyToPlay => RowStatus::ReadyToPlay,
        EntryKind::Instructions => RowStatus::Instructions,
        EntryKind::Config(patcher) | EntryKind::Dll(patcher) => {
            let (installed, patched) = check(patcher)?;
            if patched {
                RowStatus::ReadyToPlay
            } else if installed {
                RowStatus::Config
            } else {
                RowStatus::NotInstalled
            }
        }
    })
}

fn entry(key: &str) -> Result<&'static GameEntry, GamesError> {
    GAME_ENTRIES.iter().find(|e| e.key == key).ok_or_else(|| GamesError::UnknownEntry(key.to_owned()))
}

/// The `NETWORK_PORT` key a Games-page row's port button edits.
pub fn port_key(key: &str) -> Result<&'static str, GamesError> {
    entry(key)?.port.ok_or_else(|| GamesError::NoPort(key.to_owned()))
}

/// A port button click: with a game running the "Sorry game port settings cannot be changed…"
/// alert opens instead of the modal; otherwise the modal opens on the configured port
/// (an error for a missing key) and the configured forwarding list
/// (empty when `PORT_FORWARDING` lacks the key).
pub fn open_port_modal(
    port_key: &str,
    game_running: bool,
    ports: &NetworkPorts,
    forwarding: &std::collections::BTreeMap<String, Vec<String>>,
) -> Result<(u16, Vec<String>), GamesError> {
    if game_running {
        return Err(GamesError::PortWhileRunning);
    }
    let port = ports.get(port_key).ok_or_else(|| GamesError::MissingPort(port_key.to_owned()))?;
    Ok((port, forwarding.get(port_key).cloned().unwrap_or_default()))
}

/// Adds a forwarding target: the address must be a valid IP address ("`<text>` is not a
/// valid ip address"), then `<text>:<port>` is added unless the list already holds it ("This IP and
/// Port are already being forwarded").
pub fn add_port_assignment(list: &mut Vec<String>, ip: &str, port: u16) -> Result<(), GamesError> {
    if !is_python_ip_address(ip) {
        return Err(GamesError::InvalidIp(ip.to_owned()));
    }
    let forward_to = format!("{ip}:{port}");
    if list.contains(&forward_to) {
        return Err(GamesError::DuplicateTarget);
    }
    list.push(forward_to);
    Ok(())
}

/// Whether the port change confirmation sets the port and the list in memory, writes the `PORT_FORWARDING`
/// section of the file (the new `NETWORK_PORT` is not written), re-patches and asks for a restart: only when the
/// port differs from the configured one or the forwarding list differs from the configured one.
///
/// The modal edits the configured list itself (the same list object,
/// whenever the key exists), so with an existing key the forwarding comparison is always
/// equal and only a port change counts; without the key the edited list is compared with an empty one.
/// `current_port` is `None` when `NETWORK_PORT` lacks the key.
pub fn port_change_saves(
    new_port: u16,
    new_forwarding: &[String],
    current_port: Option<u16>,
    current_forwarding: Option<&[String]>,
) -> Option<bool> {
    let current_port = current_port?;
    let forwarding_changed = match current_forwarding {
        Some(_) => false,
        None => !new_forwarding.is_empty(),
    };
    Some(current_port != new_port || forwarding_changed)
}

/// Whether `text` is an IP address: a strict dotted-quad IPv4 address (no leading zeros), or an IPv6
/// address (`::` once, hextets of 1–4 hex digits, an embedded IPv4 tail, an optional `%zone`).
fn is_python_ip_address(text: &str) -> bool {
    is_python_ipv4(text) || is_python_ipv6(text)
}

fn is_python_ipv4(text: &str) -> bool {
    let octets: Vec<&str> = text.split('.').collect();
    octets.len() == 4
        && octets.iter().all(|octet| {
            !octet.is_empty()
                && octet.len() <= 3
                && octet.bytes().all(|b| b.is_ascii_digit())
                && !(octet.len() > 1 && octet.starts_with('0'))
                && octet.parse::<u16>().is_ok_and(|value| value <= 255)
        })
}

/// Whether `text` is an IPv6 address (no `/`; an optional scope after `%`).
fn is_python_ipv6(text: &str) -> bool {
    if text.contains('/') {
        return false;
    }
    let address = match text.split_once('%') {
        Some((address, scope)) => {
            if scope.is_empty() || scope.contains('%') {
                return false;
            }
            address
        }
        None => text,
    };
    if address.is_empty() {
        return false;
    }
    let mut parts: Vec<String> = address.split(':').map(str::to_owned).collect();
    if parts.len() < 3 {
        return false;
    }
    if parts.last().is_some_and(|last| last.contains('.')) {
        let tail = parts.pop().unwrap_or_default();
        if !is_python_ipv4(&tail) {
            return false;
        }
        parts.push("0".to_owned());
        parts.push("0".to_owned());
    }
    const HEXTETS: usize = 8;
    if parts.len() > HEXTETS + 1 {
        return false;
    }
    let inner = &parts[1..parts.len() - 1];
    let skips: Vec<usize> = inner.iter().enumerate().filter(|(_, p)| p.is_empty()).map(|(i, _)| i + 1).collect();
    if skips.len() > 1 {
        return false;
    }
    let (high, low) = match skips.first() {
        Some(&skip) => {
            let mut high = skip;
            let mut low = parts.len() - skip - 1;
            if parts[0].is_empty() {
                high -= 1;
                if high != 0 {
                    return false;
                }
            }
            if parts[parts.len() - 1].is_empty() {
                low -= 1;
                if low != 0 {
                    return false;
                }
            }
            if HEXTETS < high + low + 1 {
                return false;
            }
            (high, low)
        }
        None => {
            if parts.len() != HEXTETS || parts[0].is_empty() || parts[HEXTETS - 1].is_empty() {
                return false;
            }
            (parts.len(), 0)
        }
    };
    let hextet = |part: &String| !part.is_empty() && part.len() <= 4 && part.bytes().all(|b| b.is_ascii_hexdigit());
    parts[..high].iter().all(hextet) && parts[parts.len() - low..].iter().all(hextet)
}

/// A game's instructions: the built-in page `<name>[_console].html`
/// ([`crate::help_pages`]) with `^port^` replaced by the `NETWORK_PORT` of the derived listener key and `^ip^` by
/// the computer's address.
pub fn instructions(name: &str, console: bool, ports: &NetworkPorts, host_ip: &str) -> Result<String, GamesError> {
    instructions_with(name, console, host_ip, |key| ports.get(key).map(|port| port.to_string()))
}

/// The UI callback keeps the stored port text, including signs and leading zeroes.
pub fn instructions_native(
    name: &str,
    console: bool,
    host_ip: &str,
    native_port: impl FnOnce(&str) -> Option<LegacyValue>,
) -> Result<String, GamesError> {
    instructions_with(name, console, host_ip, |key| {
        native_port(key).map(|value| openrigcue_model::python_repr::str_value(&value))
    })
}

fn instructions_with(
    name: &str,
    console: bool,
    host_ip: &str,
    port: impl FnOnce(&str) -> Option<String>,
) -> Result<String, GamesError> {
    let file = if console { format!("{name}_console") } else { name.to_owned() };
    let page = crate::help_pages::page(&file).ok_or_else(|| GamesError::NoInstructions(file.clone()))?;
    let page = match port(&instructions_port_key(&file)) {
        Some(port) => page.replace("^port^", &port),
        None => page.to_owned(),
    };
    Ok(page.replace("^ip^", host_ip))
}

/// The `NETWORK_PORT` key of an instructions page: the page name up to its first `_`, with
/// the shared keys substituted (F1 2016/2017 → `f1`, the Forza titles → `forza`).
fn instructions_port_key(name: &str) -> String {
    let mut key = name.split('_').next().unwrap_or(name).to_owned();
    for (from, to) in [
        ("f12016", "f1"),
        ("f12017", "f1"),
        ("forzahorizon4", "forza"),
        ("forzahorizon5", "forza"),
        ("forzahorizon6", "forza"),
        ("forzamotorsports7", "forza"),
        ("forzamotorsports8", "forza"),
    ] {
        key = key.replace(from, to);
    }
    key
}

/// The computer's address: the local address of a UDP socket "connected" to 10.255.255.255:1, or `Unknown`.
pub fn host_ip() -> String {
    std::net::UdpSocket::bind(("0.0.0.0", 0))
        .and_then(|socket| {
            socket.connect(("10.255.255.255", 1))?;
            socket.local_addr()
        })
        .map(|address| address.ip().to_string())
        .unwrap_or_else(|_| "Unknown".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn by_key(key: &str) -> &'static GameEntry {
        entry(key).unwrap()
    }

    fn crew_port_settings(ini: &str) -> (NetworkPorts, Dict) {
        use openrigcue_config::settings::Settings;
        let parsed =
            openrigcue_config::import::configobj::parse(&format!("[NETWORK_PORT]\nthecrew = {ini}\n")).unwrap();
        let value = parsed.get("NETWORK_PORT").unwrap().as_dict().unwrap().get("thecrew").unwrap().clone();
        let mut native = Settings::default().native_snapshot();
        native.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert("thecrew", value);
        let (settings, _) = Settings::from_legacy(&native);
        (settings.network_ports.clone(), settings.native_snapshot())
    }

    fn crew_setup(prefix_root: &Path, ports: &NetworkPorts) -> GameSetup {
        std::fs::create_dir_all(prefix_root.join("drive_c/users/steamuser/Documents")).unwrap();
        GameSetup {
            steam: None,
            prefixes: vec![WinePrefix::open(prefix_root.to_owned())],
            loaded_ports: ModulePorts::at_startup(ports),
            bridge: BridgeSetup::disabled(prefix_root),
        }
    }

    #[test]
    fn native_cached_module_text_reaches_table_patch_and_confirmation() {
        use openrigcue_config::settings::Settings;
        let directory = tempfile::tempdir().unwrap();
        let prefix_root = directory.path().join("pfx");
        std::fs::create_dir_all(prefix_root.join("drive_c/users/steamuser/Documents")).unwrap();
        let prefix = WinePrefix::open(prefix_root);
        let file = prefix.documents().join("My Games/F1 2016/hardwaresettings/hardware_settings_config.xml");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"<hardware_settings_config><motion/></hardware_settings_config>").unwrap();
        let mut settings = Settings::default();
        settings.edit_native(|raw| {
            raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert("f1", "001337".into());
        });
        let setup = GameSetup {
            steam: None,
            prefixes: vec![prefix],
            loaded_ports: ModulePorts::imported(&settings).unwrap(),
            bridge: BridgeSetup::disabled(directory.path()),
        };
        setup.patch_native("f12016", false, &settings.network_ports, &settings.native_snapshot()).unwrap();
        assert!(std::fs::read_to_string(&file).unwrap().contains("port=\"001337\""));
        settings.edit_native(|raw| {
            raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert("f1", 1338.into());
        });
        let table = setup.load_table_native(
            GamesPage::Pc,
            &settings.network_ports,
            &Default::default(),
            &settings.native_snapshot(),
        );
        assert!(table.error.is_none());
        assert_eq!(table.rows.iter().find(|row| row.key == "f12016").unwrap().status, RowStatus::ReadyToPlay);
        setup.patch_native("f12016", false, &settings.network_ports, &settings.native_snapshot()).unwrap();
        assert!(std::fs::read_to_string(&file).unwrap().contains("port=\"1338\""));
        settings.edit_native(|raw| {
            raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert("f1", 70000.into());
        });
        setup.repatch_after_port_change_native("f12016", &settings.network_ports, &settings.native_snapshot()).unwrap();
        assert!(std::fs::read_to_string(&file).unwrap().contains("port=\"70000\""));
        assert!(settings.network_port("f1").is_none(), "socket range semantics are unchanged");
    }

    #[test]
    fn native_wrc_integer_reaches_status_patch_and_confirmation() {
        use openrigcue_config::settings::Settings;
        let directory = tempfile::tempdir().unwrap();
        let prefix_root = directory.path().join("pfx");
        std::fs::create_dir_all(prefix_root.join("drive_c/users/steamuser/Documents")).unwrap();
        let prefix = WinePrefix::open(prefix_root);
        let telemetry = prefix.documents().join("My Games/WRC/telemetry");
        std::fs::create_dir_all(telemetry.join("udp")).unwrap();
        let file = telemetry.join("config.json");
        std::fs::write(&file, br#"{"udp":{"packets":[]}}"#).unwrap();
        let mut settings = Settings::default();
        settings.edit_native(|raw| {
            raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert("wrc2023", "+0070000".into());
        });
        let setup = GameSetup {
            steam: None,
            prefixes: vec![prefix],
            loaded_ports: ModulePorts::imported(&settings).unwrap(),
            bridge: BridgeSetup::disabled(directory.path()),
        };
        setup.patch_native("wrc2023", false, &settings.network_ports, &settings.native_snapshot()).unwrap();
        assert!(std::fs::read_to_string(&file).unwrap().contains("\"port\": 70000"));
        assert_eq!(
            setup
                .status_native("wrc2023", false, &settings.network_ports, &settings.native_snapshot(), &|_| false)
                .unwrap(),
            GameStatus::Patched
        );
        let table = setup.load_table_native(
            GamesPage::Pc,
            &settings.network_ports,
            &Default::default(),
            &settings.native_snapshot(),
        );
        assert!(table.error.is_none());
        assert_eq!(table.rows.iter().find(|row| row.key == "wrc2023").unwrap().status, RowStatus::ReadyToPlay);
        settings.edit_native(|raw| {
            raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert("wrc2023", (-1).into());
        });
        setup
            .repatch_after_port_change_native("wrc2023", &settings.network_ports, &settings.native_snapshot())
            .unwrap();
        assert!(std::fs::read_to_string(&file).unwrap().contains("\"port\": -1"));
        assert!(settings.network_port("wrc2023").is_none(), "numeric socket projection is unchanged");
        let before = std::fs::read(&file).unwrap();
        settings.edit_native(|raw| {
            raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert("wrc2023", "bad".into());
        });
        let table = setup.load_table_native(
            GamesPage::Pc,
            &settings.network_ports,
            &Default::default(),
            &settings.native_snapshot(),
        );
        assert_eq!(table.error.as_ref().unwrap().0, "wrc2023");
        assert_eq!(table.rows.last().unwrap().key, "dirtshowdown");
        assert_eq!(std::fs::read(&file).unwrap(), before, "conversion failure precedes all patch I/O");
    }

    fn crew_template(port: &str) -> String {
        String::from_utf8(openrigcue_config::patch::library::file("ExtraConfig.xml").unwrap().to_vec())
            .unwrap()
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .replace("^port^", port)
    }

    #[test]
    fn native_crew_table_and_notifications_preserve_text_but_rows_keep_numeric_ports() {
        for (ini, expected) in [("001337", "001337"), ("+001337", "+001337"), ("' 001337 '", " 001337 ")] {
            let directory = tempfile::tempdir().unwrap();
            let (ports, native) = crew_port_settings(ini);
            let setup = crew_setup(directory.path(), &ports);
            for folder in ["The Crew", "The Crew 2"] {
                let folder = setup.prefixes[0].documents().join(folder);
                std::fs::create_dir_all(&folder).unwrap();
                std::fs::write(folder.join("ExtraConfig.xml"), crew_template(expected)).unwrap();
            }
            let table = setup.load_table_native(GamesPage::Pc, &ports, &Default::default(), &native);
            assert!(table.error.is_none());
            for key in ["thecrew", "thecrew2"] {
                let row = table.rows.iter().find(|row| row.key == key).unwrap();
                assert_eq!(row.status, RowStatus::ReadyToPlay);
                assert_eq!(row.port, Some(1337), "row/socket projection stays numeric");
                assert_eq!(setup.status_native(key, false, &ports, &native, &|_| false).unwrap(), GameStatus::Patched);
            }
        }
    }

    #[test]
    fn native_crew_status_button_patch_keeps_backup_and_dll_confirmation_noop() {
        let directory = tempfile::tempdir().unwrap();
        let (ports, native) = crew_port_settings("+001337");
        let setup = crew_setup(directory.path(), &ports);
        for (key, folder) in [("thecrew", "The Crew"), ("thecrew2", "The Crew 2")] {
            let folder = setup.prefixes[0].documents().join(folder);
            std::fs::create_dir_all(&folder).unwrap();
            let file = folder.join("ExtraConfig.xml");
            std::fs::write(&file, b"previous settings").unwrap();
            assert!(matches!(setup.patch_native(key, true, &ports, &native), Err(GamesError::GameRunning)));
            assert_eq!(std::fs::read(&file).unwrap(), b"previous settings");
            setup.patch_native(key, false, &ports, &native).unwrap();
            assert_eq!(std::fs::read(folder.join("ExtraConfigsrs_bck.xml")).unwrap(), b"previous settings");
            assert_eq!(std::fs::read_to_string(&file).unwrap(), crew_template("+001337").replace('\n', "\r\n"));
            assert_eq!(setup.status_native(key, false, &ports, &native, &|_| false).unwrap(), GameStatus::Patched);
            let (new_ports, new_native) = crew_port_settings("1338");
            assert_eq!(
                setup.status_native(key, false, &new_ports, &new_native, &|_| false).unwrap(),
                GameStatus::NotPatched
            );
            std::fs::write(&file, b"DLL confirmation must not repatch").unwrap();
            setup.repatch_after_port_change(key, &new_ports).unwrap();
            setup.repatch_after_port_change_native(key, &new_ports, &new_native).unwrap();
            assert_eq!(std::fs::read(&file).unwrap(), b"DLL confirmation must not repatch");
        }
    }

    #[test]
    fn native_template_config_confirmation_int_matches_the_typed_repatch() {
        use openrigcue_config::import::value::LegacyValue;
        use openrigcue_config::settings::Settings;
        let directory = tempfile::tempdir().unwrap();
        let (_, mut native) = crew_port_settings("1337");
        native.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert("kartkraft", "+005000".into());
        let (settings, _) = Settings::from_legacy(&native);
        let setup = crew_setup(directory.path(), &settings.network_ports);
        let folder = setup.prefixes[0].local_app_data().join("project_k/Saved/Config/WindowsNoEditor");
        std::fs::create_dir_all(&folder).unwrap();
        let file = folder.join("Game.ini");
        setup.patch_native("kartkraft", false, &settings.network_ports, &settings.native_snapshot()).unwrap();
        assert!(std::fs::read_to_string(&file).unwrap().contains("+005000"));
        let table = setup.load_table_native(
            GamesPage::Pc,
            &settings.network_ports,
            &Default::default(),
            &settings.native_snapshot(),
        );
        assert_eq!(table.rows.iter().find(|row| row.key == "kartkraft").unwrap().status, RowStatus::ReadyToPlay);
        for port in [0, 6001, 65535] {
            // The confirmation assigns the integer port before patching.
            native.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert("kartkraft", LegacyValue::Int(port));
            let (settings, _) = Settings::from_legacy(&native);
            assert_eq!(settings.network_ports.get("kartkraft"), Some(port as u16));
            setup.repatch_after_port_change("kartkraft", &settings.network_ports).unwrap();
            let typed_write = std::fs::read(&file).unwrap();
            setup.patch_native("kartkraft", false, &settings.network_ports, &settings.native_snapshot()).unwrap();
            assert_eq!(std::fs::read(&file).unwrap(), typed_write);
            assert_eq!(
                setup
                    .status_native("kartkraft", false, &settings.network_ports, &settings.native_snapshot(), &|_| false)
                    .unwrap(),
                GameStatus::Patched
            );
        }
    }

    #[test]
    fn native_template_config_confirmation_preserves_integers_outside_socket_range() {
        use openrigcue_config::import::value::LegacyValue;
        use openrigcue_config::settings::Settings;
        for port in [70000, -1] {
            let directory = tempfile::tempdir().unwrap();
            let (_, mut native) = crew_port_settings("1337");
            // A validator-accepted loaded string reaches the unchanged Port spinbox value.
            // A new forwarding row makes Confirm assign the same port as an integer before patching.
            native.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert("kartkraft", port.to_string().into());
            let (loaded, _) = Settings::from_legacy(&native);
            let mut assigned = loaded.native_snapshot();
            assigned
                .get_mut("NETWORK_PORT")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .insert("kartkraft", LegacyValue::Int(port));
            let (settings, _) = Settings::from_legacy(&assigned);
            let setup = crew_setup(directory.path(), &settings.network_ports);
            let folder = setup.prefixes[0].local_app_data().join("project_k/Saved/Config/WindowsNoEditor");
            std::fs::create_dir_all(&folder).unwrap();
            let file = folder.join("Game.ini");
            std::fs::write(&file, b"previous settings").unwrap();
            setup
                .repatch_after_port_change_native("kartkraft", &settings.network_ports, &settings.native_snapshot())
                .unwrap();
            assert_eq!(std::fs::read(folder.join("Gamesrs_bck.ini")).unwrap(), b"previous settings");
            assert!(std::fs::read_to_string(&file).unwrap().contains(&format!("OutputEndpoints=127.0.0.1:{port}")));
        }
    }

    /// load_table status column: PC config/dll entries are not_installed, config when installed, ready_to_play
    /// when patched (checked on its own); Console rows are instructions except testmode.
    #[test]
    fn startup_checks_canonical_import_time_ports() {
        use openrigcue_config::import::value::LegacyValue;
        use openrigcue_config::settings::Settings;
        let mut raw = Settings::default().native_snapshot();
        raw.get_mut("NETWORK_PORT").and_then(LegacyValue::as_dict_mut).unwrap().remove("dirt");
        let (settings, _) = Settings::from_legacy(&raw);
        assert!(settings.network_ports.get("dirt").is_none());
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(GameSetup::start(tmp.path(), &settings), Err(PatchError::MissingPort(PortKey::Dirt))));
    }

    #[test]
    fn status_column() {
        let dirt = by_key("dirt4");
        let pc = |check: (bool, bool)| row_status(dirt, GamesPage::Pc, |_| Ok(check)).unwrap();
        assert_eq!(pc((false, false)), RowStatus::NotInstalled);
        assert_eq!(pc((true, false)), RowStatus::Config);
        assert_eq!(pc((false, true)), RowStatus::ReadyToPlay);
        let unchecked = |entry, page| row_status(entry, page, |_| unreachable!()).unwrap();
        assert_eq!(unchecked(by_key("beamng"), GamesPage::Pc), RowStatus::Instructions);
        assert_eq!(unchecked(by_key("acc"), GamesPage::Pc), RowStatus::ReadyToPlay);
        assert_eq!(unchecked(by_key("f12019"), GamesPage::Console), RowStatus::Instructions);
        assert_eq!(unchecked(by_key("testmode"), GamesPage::Console), RowStatus::ReadyToPlay);
        assert!(row_status(dirt, GamesPage::Pc, |_| Err(PatchError::MissingElement)).is_err());
    }

    /// load_table: a patched check that fails ends the table; the rows before it stay.
    #[test]
    fn failing_patched_ends_the_table() {
        let tmp = tempfile::tempdir().unwrap();
        let documents =
            tmp.path().join(".local/share/Steam/steamapps/compatdata/1/pfx/drive_c/users/steamuser/Documents");
        let kartkraft = documents.parent().unwrap().join("AppData/Local/project_k/Saved/Config/WindowsNoEditor");
        std::fs::create_dir_all(&kartkraft).unwrap();
        // 0x81 is undefined in Windows-1252: reading the game's file fails.
        std::fs::write(kartkraft.join("Game.ini"), b"\x81").unwrap();
        let ports = NetworkPorts::default();
        let setup = GameSetup::discover(tmp.path(), &ports);
        let table = setup.load_table(GamesPage::Pc, &ports, &Default::default());
        assert!(matches!(table.error, Some(("kartkraft", _))));
        let rows = setup.rows(GamesPage::Pc, &ports, &Default::default());
        assert_eq!(rows, table.rows);
        assert!(!rows.is_empty());
        assert!(rows.iter().all(|row| row.name.to_uppercase().as_str() < "KARTKRAFT"));
        let all = GAME_ENTRIES.iter().filter(|e| e.platforms.pc && e.name.to_uppercase().as_str() < "KARTKRAFT");
        assert_eq!(rows.len(), all.count());
    }

    /// The port confirmation checks nothing; the port button refuses while a game runs and needs the
    /// configured port; adding a forwarding target checks the address and duplicates.
    #[test]
    fn port_modal_rules() {
        // Confirm checks nothing but the row's port key.
        assert_eq!(port_key("dirt4").unwrap(), "dirt");
        assert!(matches!(port_key("acc"), Err(GamesError::NoPort(_))));
        assert!(matches!(port_key("nope"), Err(GamesError::UnknownEntry(_))));

        let ports = NetworkPorts::default();
        let mut forwarding = std::collections::BTreeMap::new();
        assert!(matches!(open_port_modal("dirt", true, &ports, &forwarding), Err(GamesError::PortWhileRunning)));
        assert_eq!(open_port_modal("dirt", false, &ports, &forwarding).unwrap(), (ports.get("dirt").unwrap(), vec![]));
        forwarding.insert("dirt".to_owned(), vec!["127.0.0.1:10000".to_owned()]);
        assert_eq!(open_port_modal("dirt", false, &ports, &forwarding).unwrap().1, ["127.0.0.1:10000"]);
        assert!(matches!(open_port_modal("missing", false, &ports, &forwarding), Err(GamesError::MissingPort(_))));

        let mut list = vec![];
        add_port_assignment(&mut list, "127.0.0.1", 10000).unwrap();
        add_port_assignment(&mut list, "::1", 5).unwrap();
        assert_eq!(list, ["127.0.0.1:10000", "::1:5"]);
        assert!(matches!(add_port_assignment(&mut list, "127.0.0.1", 10000), Err(GamesError::DuplicateTarget)));
        assert_eq!(
            add_port_assignment(&mut list, "01.2.3.4", 5).unwrap_err().to_string(),
            "01.2.3.4 is not a correctly formed IP address"
        );
        assert_eq!(list.len(), 2);
    }

    /// IP address validation of strings.
    #[test]
    fn python_ip_addresses() {
        for valid in [
            "127.0.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "::",
            "::1",
            "1::",
            "1:2:3:4:5:6:7:8",
            "1:2:3:4:5:6:7::",
            "::ffff:1.2.3.4",
            "fe80::1%eth0",
            "1:2:3:4:5:6:1.2.3.4",
            "ABCD::ef",
        ] {
            assert!(is_python_ip_address(valid), "{valid}");
        }
        for invalid in [
            "",
            "1.2.3",
            "1.2.3.4.5",
            "01.2.3.4",
            "256.1.1.1",
            "1.2.3.4 ",
            " 1.2.3.4",
            ":::",
            "1:2",
            "1::2::3",
            "fe80::1%a/b",
            "12345::",
            "1:2:3:4:5:6:7:8:9",
            "::1.2.3",
            "fe80::1%",
            "a%b%c::",
            "1:2:3:4:5:6:7:1.2.3.4",
            ":1::",
            "1::2:",
        ] {
            assert!(!is_python_ip_address(invalid), "{invalid}");
        }
    }

    /// The port confirmation: nothing happens unless the port changed, or (for a key without a
    /// forwarding list) targets were added.
    #[test]
    fn port_change_effect() {
        let targets = ["127.0.0.1:10000".to_owned()];
        assert_eq!(port_change_saves(20777, &[], Some(20777), Some(&[])), Some(false));
        assert_eq!(port_change_saves(20777, &targets, Some(20777), Some(&[])), Some(false));
        assert_eq!(port_change_saves(20778, &[], Some(20777), Some(&targets)), Some(true));
        assert_eq!(port_change_saves(20777, &targets, Some(20777), None), Some(true));
        assert_eq!(port_change_saves(20777, &[], Some(20777), None), Some(false));
        assert_eq!(port_change_saves(1, &[], None, None), None);
    }

    /// Loading the patch modules at start-up fails on a missing start-up port.
    #[test]
    fn start_needs_import_ports() {
        let tmp = tempfile::tempdir().unwrap();
        let ports = NetworkPorts::default();
        assert!(GameSetup::start(tmp.path(), &ports).is_ok());
        let without_dirt: serde_json::Map<String, serde_json::Value> =
            ports.iter().filter(|(key, _)| *key != "dirt").map(|(k, v)| (k.to_owned(), v.into())).collect();
        let without_dirt: NetworkPorts = serde_json::from_value(serde_json::Value::Object(without_dirt)).unwrap();
        assert!(matches!(GameSetup::start(tmp.path(), &without_dirt), Err(PatchError::MissingPort(PortKey::Dirt))));
    }

    /// The status button: refused with a game running; ready_to_play entries have nothing to patch.
    #[test]
    fn patch_refusals() {
        let tmp = tempfile::tempdir().unwrap();
        let setup = GameSetup::discover(tmp.path(), &NetworkPorts::default());
        let ports = NetworkPorts::default();
        assert!(matches!(setup.patch("dirt4", true, &ports), Err(GamesError::GameRunning)));
        assert!(matches!(setup.patch("acc", false, &ports), Err(GamesError::NothingToConfigure(_))));
        assert!(matches!(setup.patch("nope", false, &ports), Err(GamesError::UnknownEntry(_))));
        assert!(setup.patch("dirt4", false, &ports).is_err());
    }

    /// The instructions: console pages, F1 2016/2017 and Forza port keys, the ^ip^ placeholder,
    /// all from the built-in pages.
    #[test]
    fn instructions_substitution() {
        let ports = NetworkPorts::default();
        let forza = instructions("forzahorizon5", false, &ports, "10.0.0.2").unwrap();
        assert!(forza.contains("10001"));
        assert!(!forza.contains('^'));
        assert!(forza.contains("including the Steam version"));
        assert!(forza.contains("Steam on Linux/Proton"));
        assert!(!forza.contains("NOT compatible"));
        assert!(!forza.contains("go.simracing.studio"));
        let f1 = instructions("f12016", true, &ports, "10.0.0.2").unwrap();
        assert!(f1.contains("20778") && f1.contains("10.0.0.2"));
        // The Oculus page has a fixed port and no placeholder.
        assert!(instructions("epic_oculus", true, &ports, "x").unwrap().contains("33001"));
        assert!(matches!(instructions("missing", false, &ports, "x"), Err(GamesError::NoInstructions(_))));
    }

    #[test]
    fn native_instructions_keep_port_text_and_source_missing_lookup_fallback() {
        let mut raw = openrigcue_config::settings::Settings::default().native_snapshot();
        let ports = raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap();
        for (key, value) in [("f1", LegacyValue::from("+0020778")), ("forza", LegacyValue::from("0010001"))] {
            ports.insert(key, value);
        }
        let native_port = |raw: &Dict, key: &str| raw.get("NETWORK_PORT")?.as_dict()?.get(key).cloned();
        let page = instructions_native("f12016", true, "10.0.0.2", |key| native_port(&raw, key)).unwrap();
        assert!(page.contains("+0020778"), "native ConfigObj text reaches the help page unchanged");
        assert!(!page.contains("^ip^"));
        assert!(instructions_native("forzahorizon5", false, "host", |key| native_port(&raw, key))
            .unwrap()
            .contains("0010001"));
        for (value, text) in [
            (LegacyValue::None, "None"),
            (LegacyValue::Bool(true), "True"),
            (LegacyValue::Int(-70000), "-70000"),
            (LegacyValue::Float(70000.5), "70000.5"),
            (LegacyValue::List(vec![LegacyValue::Int(1)]), "[1]"),
        ] {
            raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert("f1", value);
            assert!(instructions_native("f12017", true, "host", |key| native_port(&raw, key)).unwrap().contains(text));
        }
        raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().remove("f1");
        let page = instructions_native("f12016", true, "host", |key| native_port(&raw, key)).unwrap();
        assert!(page.contains("^port^") && page.contains("host") && !page.contains("^ip^"));
        raw.remove("NETWORK_PORT");
        assert!(instructions_native("f12017", true, "host", |key| native_port(&raw, key)).unwrap().contains("^port^"));
    }

    #[test]
    fn native_instructions_reads_help_before_the_caught_port_lookup() {
        assert!(matches!(
            instructions_native("missing", false, "host", |_| panic!("missing page precedes native port read")),
            Err(GamesError::NoInstructions(_))
        ));
        let page = instructions_native("f12016", true, "host", |key| {
            assert_eq!(key, "f1");
            None
        })
        .unwrap();
        assert!(page.contains("^port^") && page.contains("host"));
        for name in ["forzahorizon4", "forzahorizon5", "forzahorizon6", "forzamotorsports7", "forzamotorsports8"] {
            assert_eq!(instructions_port_key(name), "forza");
        }
        assert_eq!(instructions_port_key("epic_oculus_console"), "epic");
    }

    /// load_table: rows sorted by upper-cased name, console page lists only console entries.
    #[test]
    fn rows_are_sorted_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let setup = GameSetup::discover(tmp.path(), &NetworkPorts::default());
        let rows = setup.rows(GamesPage::Console, &NetworkPorts::default(), &Default::default());
        assert_eq!(rows.len(), 22);
        assert_eq!(rows[0].key, "testmode");
        let names: Vec<String> = rows.iter().map(|r| r.name.to_uppercase()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
    }
}
