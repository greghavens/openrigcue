//! Notification bookkeeping kept in `metadata.db`: the tables `game_config_notifications` and
//! `notifications`. Only the key sets matter; the writers store `True` and the readers test presence.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Both key sets start empty.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Dismissals {
    /// Games whose "game not configured" notification was dismissed.
    pub game_setup: BTreeSet<String>,
    /// Keys of the `notifications` table. This one set serves three things:
    /// driver modules whose "driver missing" notification was dismissed, listeners that have
    /// received telemetry, and games whose setup-instructions notification is suppressed. A listener
    /// key that equals a game id therefore also suppresses that game's instructions.
    pub notification_keys: BTreeSet<String>,
}

impl Dismissals {
    /// The notification is shown while the running game is not among the dismissed ones.
    pub fn game_setup_dismissed(&self, game: &str) -> bool {
        self.game_setup.contains(game)
    }

    pub fn dismiss_game_setup(&mut self, game: &str) {
        self.game_setup.insert(game.to_owned());
    }

    /// Whether the "driver missing" notification of `module` was dismissed.
    pub fn driver_dismissed(&self, module: &str) -> bool {
        self.notification_keys.contains(module)
    }

    /// Records that the "driver missing" notification of `module` was dismissed.
    pub fn dismiss_driver(&mut self, module: &str) {
        self.notification_keys.insert(module.to_owned());
    }

    /// Records that a game's listener delivered telemetry.
    pub fn record_telemetry_received(&mut self, listener: &str) {
        self.notification_keys.insert(listener.to_owned());
    }

    /// The game-status lookup for an `instructions` game: when the
    /// game id is a key, the status is `dot_not_show_notification` instead of `instructions`.
    pub fn setup_instructions_suppressed(&self, game: &str) -> bool {
        self.notification_keys.contains(game)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The telemetry marker, the driver dismissal and the game-status lookup
    /// share the `notifications` table: a key written by one is seen by the others.
    #[test]
    fn notification_keys_are_one_shared_set() {
        let mut d = Dismissals::default();
        assert!(!d.setup_instructions_suppressed("ac"));
        d.record_telemetry_received("ac");
        assert!(d.setup_instructions_suppressed("ac"), "listener 'ac' received telemetry");
        assert!(d.driver_dismissed("ac"), "same table, same key");
        d.dismiss_driver("backend.controllers.smc3");
        assert!(d.setup_instructions_suppressed("backend.controllers.smc3"));
        assert!(!d.game_setup_dismissed("ac"), "game_config_notifications is a separate table");
    }
}
