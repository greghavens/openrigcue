//! Game-side configuration: the files each game needs changed before it sends telemetry
//! and the Games page table that lists them.
//!
//! Every module answers `installed`, `patched` and `patch`. Linux specifics:
//! - Windows user folders (`Documents`, `%APPDATA%`, `%LOCALAPPDATA%`, `~`) and `HKEY_CURRENT_USER` values are read
//!   from the game's Wine/Proton prefix ([`WinePrefix`]); the Proton prefixes of every Steam library and the
//!   prefixes of other launchers are listed by [`candidate_prefixes`], and [`PatchContext::environment_for`]
//!   picks the first prefix in which a module's game is installed.
//! - Steam games are found through `~/.local/share/Steam` (or `~/.steam/steam`, or the Flatpak location) and
//!   `steamapps/libraryfolders.vdf` ([`SteamLibraries`]).
//! - Windows path lookups are case-insensitive; text files are read and written in Windows text
//!   form (code page 1252, `\r\n` line ends).
//! - Templates and plugin files are built in ([`library`]) and
//!   read from the program.
//! - Backups are named `<name>srs_bck.<ext>` and `config_srs_bck.json`, the names existing SRS backups use.

mod bundled;
mod codec_data;
mod codemasters;
mod context;
mod env;
mod files;
mod games;
mod il2;
mod iracing;
mod lfs;
pub mod library;
mod pmracing;
mod prefix;
mod pycjk;
pub(crate) mod pycodecs;
pub(crate) mod pyidna;
mod pyjson;
mod pyunicode;
mod registry;
mod rfactor2;
mod steam;
mod tdusc;
mod template;
#[cfg(test)]
mod testing;
mod text;
mod vdf;
mod winpath;
mod wrc2023;
mod wreckfest2;
mod xml;
mod xml_names;

pub use bundled::BundledFile;
pub use codemasters::{CodemastersGame, Layout};
pub use context::PatchContext;
pub use env::{ModulePorts, PatchEnvironment, PortKey, PortLookup, PortModule, UserFolder, UserPath, IMPORT_PORT_KEYS};
pub use files::backup_path;
pub use games::{game_status, EntryKind, GameEntry, GameStatus, Patcher, Platforms, GAME_ENTRIES};
pub use prefix::{launcher_prefixes, WinePrefix};
pub use pyjson::JsonError;
pub use registry::UserHive;
pub use steam::{SteamLibraries, SteamPlugin};
pub use template::{TemplateGame, DCS_LEGACY};
pub use text::{AnsiReadDecodeError, Utf8ReadDecodeError};
pub use xml::XmlError;

/// Every prefix a game can live in, in search order: the Proton prefixes of every Steam library, then the
/// launcher prefixes ([`launcher_prefixes`]).
pub fn candidate_prefixes(
    home: &std::path::Path,
    steam: Option<&SteamLibraries>,
    wine_prefix: Option<&std::path::Path>,
) -> Vec<WinePrefix> {
    let mut prefixes = steam.map(SteamLibraries::proton_prefixes).unwrap_or_default();
    for prefix in launcher_prefixes(home, wine_prefix) {
        if !prefixes.iter().any(|known| known.root() == prefix.root()) {
            prefixes.push(prefix);
        }
    }
    prefixes
}

/// Why a patch module could not answer or apply its change.
#[derive(Debug, thiserror::Error)]
pub enum PatchError {
    /// The game (or the prefix / Steam library it lives in) was not found.
    #[error("game not installed")]
    NotInstalled,
    #[error("no [NETWORK_PORT] {} entry", .0.config_key())]
    MissingPort(PortKey),
    /// The WRC patcher could not read its configured port as an integer, before checking the target file.
    #[error("{message}")]
    PortInteger { class_name: &'static str, message: String },
    /// The settings file lacks the element the change goes into.
    #[error("settings element missing")]
    MissingElement,
    /// A JSON settings file does not have the shape the change needs.
    #[error("unexpected JSON structure")]
    UnexpectedJson,
    #[error("unsupported file encoding {0:?}")]
    UnsupportedEncoding(String),
    /// The text holds a character the file's encoding cannot represent.
    #[error("text not encodable as {0:?}")]
    Unencodable(String),
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("xml: {0}")]
    Xml(#[from] XmlError),
    #[error("json: {0}")]
    Json(#[from] pyjson::JsonError),
}

impl PatchError {
    /// Error metadata retained by a typed decoder or integer-conversion failure.
    /// Other I/O and parser errors retain their existing fallback.
    pub fn native_exception(&self) -> Option<(&'static str, String)> {
        if let Self::PortInteger { class_name, message } = self {
            return Some((class_name, message.clone()));
        }
        let Self::Io(error) = self else { return None };
        let error = error.get_ref()?;
        if let Some(error) = error.downcast_ref::<AnsiReadDecodeError>() {
            Some(("UnicodeDecodeError", error.native_message()))
        } else {
            let error = error.downcast_ref::<Utf8ReadDecodeError>()?;
            Some(("UnicodeDecodeError", error.native_message()))
        }
    }
}
