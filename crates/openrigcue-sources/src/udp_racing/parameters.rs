//! The process-wide parameters that listeners read or change at
//! their own times, outside the dispatcher's liveness and receive calls:
//!
//! - `game_running`: the Assetto Corsa UDP and Project Motor Racing
//!   receive threads test it whenever their loop comes round, not when the dispatcher calls them.
//! - `listener_running`: the UDP forwarder reads it on every datagram, whichever listener received it.
//! - `game_errors`: one set for the whole application. Every listener adds the text of its socket errors
//!   and any listener's good data empties it, which also removes other listeners' texts; the UI
//!   empties it as well.
//!
//! [`Parameters::shared`] is the one instance of the process. The dispatcher writes `game_running` and
//! `listener_running` into it whenever it assigns them ([`Parameters::set_game_running`],
//! [`Parameters::set_listener_running`]); the service reads the error set from it ([`Parameters::game_errors`])
//! and empties it where the UI does ([`Parameters::clear_game_errors`]). Both start as
//! `"none"`.

use std::collections::BTreeSet;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

/// The text of the socket error a listener gets when its port is already bound: Windows' `WSAEADDRINUSE`
/// text, whose Linux equivalent is `EADDRINUSE`. The UI tests the game errors for exactly this text
///.
pub const PORT_IN_USE: &str =
    "[WinError 10048] Only one usage of each socket address (protocol/network address/port) is normally permitted";

/// The text of a socket error: [`PORT_IN_USE`] for an address in use; otherwise the OS error text
/// (`[Errno N] text`, `timed out`) or the text of another failure such as a bind port out of range.
fn error_text(error: &io::Error) -> String {
    if error.kind() == io::ErrorKind::AddrInUse {
        PORT_IN_USE.to_owned()
    } else {
        crate::printed::os_error(error)
    }
}

/// The value `game_running` and `listener_running` start with.
const NONE: &str = "none";

/// Process-wide listener parameters (see the module documentation).
#[derive(Debug)]
pub struct Parameters {
    /// The running game's id.
    game_running: Mutex<String>,
    /// The running listener's id.
    listener_running: Mutex<String>,
    /// The game error texts.
    game_errors: Mutex<BTreeSet<String>>,
}

impl Default for Parameters {
    fn default() -> Self {
        Parameters {
            game_running: Mutex::new(NONE.to_owned()),
            listener_running: Mutex::new(NONE.to_owned()),
            game_errors: Mutex::default(),
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Parameters {
    /// The instance every listener of the process shares.
    pub fn shared() -> Arc<Parameters> {
        static SHARED: OnceLock<Arc<Parameters>> = OnceLock::new();
        SHARED.get_or_init(Arc::default).clone()
    }

    /// The instance a listener uses by default: the process-wide one, or in unit tests a fresh one so that
    /// tests running in parallel do not see each other's values.
    pub(crate) fn for_listener() -> Arc<Parameters> {
        #[cfg(not(test))]
        return Parameters::shared();
        #[cfg(test)]
        return Arc::default();
    }

    /// Sets the running game (`"none"` when no game runs).
    pub fn set_game_running(&self, game: &str) {
        game.clone_into(&mut lock(&self.game_running));
    }

    /// The running game.
    pub fn game_running(&self) -> String {
        lock(&self.game_running).clone()
    }

    /// Whether the running game is one of `ids`.
    pub(crate) fn game_running_is_one_of(&self, ids: &[&str]) -> bool {
        ids.contains(&lock(&self.game_running).as_str())
    }

    /// Sets the running listener (`"none"` when no listener runs).
    pub fn set_listener_running(&self, listener: &str) {
        listener.clone_into(&mut lock(&self.listener_running));
    }

    /// The running listener.
    pub fn listener_running(&self) -> String {
        lock(&self.listener_running).clone()
    }

    /// Adds a text to the game errors.
    pub fn add_game_error(&self, text: String) {
        lock(&self.game_errors).insert(text);
    }

    /// Empties the game errors.
    pub fn clear_game_errors(&self) {
        lock(&self.game_errors).clear();
    }

    /// The texts the game errors hold now.
    pub fn game_errors(&self) -> BTreeSet<String> {
        lock(&self.game_errors).clone()
    }
}

/// One listener's handle on the shared game errors set.
///
/// [`GameErrors::record`] and [`GameErrors::clear`] change the shared set; [`GameErrors::messages`] is the
/// shared set as it stood after this listener's last change (the set itself is [`Parameters::game_errors`]).
#[derive(Debug, Clone)]
pub struct GameErrors {
    parameters: Arc<Parameters>,
    seen: BTreeSet<String>,
}

impl Default for GameErrors {
    fn default() -> Self {
        GameErrors::new(Parameters::for_listener())
    }
}

impl GameErrors {
    pub fn new(parameters: Arc<Parameters>) -> Self {
        GameErrors { parameters, seen: BTreeSet::new() }
    }

    /// Adds the error's text to the game errors.
    pub fn record(&mut self, error: &io::Error) {
        self.parameters.add_game_error(error_text(error));
        self.seen = self.parameters.game_errors();
    }

    /// Empties the game errors: removes every listener's texts.
    pub fn clear(&mut self) {
        self.parameters.clear_game_errors();
        self.seen.clear();
    }

    /// The shared set as this listener left it.
    pub fn messages(&self) -> &BTreeSet<String> {
        &self.seen
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_distinct_texts_and_clears() {
        // The set keeps one copy of each text; a clear empties it.
        let mut errors = GameErrors::default();
        errors.record(&io::Error::other("bind failed"));
        errors.record(&io::Error::other("bind failed"));
        assert_eq!(errors.messages().len(), 1);
        errors.clear();
        assert!(errors.messages().is_empty());
    }

    #[test]
    fn one_set_for_every_listener() {
        // The game errors are one set: a clear by any listener removes every text.
        let parameters = Arc::new(Parameters::default());
        let mut forza = GameErrors::new(parameters.clone());
        let mut wrc = GameErrors::new(parameters.clone());
        forza.record(&io::Error::other("forza: bind failed"));
        wrc.record(&io::Error::other("wrc: bind failed"));
        assert_eq!(parameters.game_errors().len(), 2);
        assert_eq!(wrc.messages().len(), 2, "the shared set as wrc left it");
        wrc.clear();
        assert!(parameters.game_errors().is_empty(), "wrc's good data also removed forza's text");
        forza.record(&io::Error::other("forza: again"));
        parameters.clear_game_errors();
        assert!(parameters.game_errors().is_empty(), "the UI empties it too");
    }

    #[test]
    fn an_address_in_use_is_recorded_as_the_windows_text() {
        // The text of WSAEADDRINUSE, which the UI looks for in game_errors; EADDRINUSE is its Linux equivalent.
        let mut errors = GameErrors::default();
        errors.record(&io::Error::from(io::ErrorKind::AddrInUse));
        assert_eq!(errors.messages().iter().collect::<Vec<_>>(), [PORT_IN_USE]);
    }

    #[test]
    fn game_and_listener_running_start_as_none() {
        // Both start as "none".
        let parameters = Parameters::default();
        assert_eq!(parameters.game_running(), "none");
        assert_eq!(parameters.listener_running(), "none");
        assert!(!parameters.game_running_is_one_of(&["acudp"]));
        parameters.set_game_running("acudp");
        assert!(parameters.game_running_is_one_of(&["acudp"]));
        parameters.set_listener_running("acudp");
        assert_eq!(parameters.listener_running(), "acudp");
    }
}
