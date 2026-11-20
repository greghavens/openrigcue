//! Per-game and per-vehicle tuning: the tune schema, the factory tunes of every game, the expert
//! tune sets and the shaker speaker layouts.
//!
//! Data: `data/tunes/defaults.json` and `data/tunes/expert_profiles.json`.

mod expert;
mod layouts;
mod schema;

use std::collections::BTreeMap;
use std::sync::OnceLock;

pub use expert::{ExpertGame, ExpertProfiles, ExpertRanges, ExpertSet, ExpertTune};
pub use layouts::{ChannelEffectLevels, ShakerLayouts, SpeakerChannel, SpeakerLayout};
pub use schema::*;

use crate::import::de::{from_legacy, ConvertError};
use crate::import::shipped::ShippedData;
use crate::import::value::LegacyValue;

impl Tune {
    /// Converts a profile dict (pickled `profile.db` value, `.srs` JSON or factory data).
    pub fn from_legacy(value: &LegacyValue) -> Result<Tune, ConvertError> {
        from_legacy(value)
    }

    /// The tune as `.srs`/`profile.db` JSON (SRS key names).
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("a tune always serializes to JSON")
    }
}

/// The factory tune of every game: what "reset" restores.
#[derive(Debug, Clone, PartialEq)]
pub struct FactoryTunes {
    games: BTreeMap<String, Tune>,
}

impl FactoryTunes {
    pub fn builtin() -> &'static FactoryTunes {
        static TUNES: OnceLock<FactoryTunes> = OnceLock::new();
        TUNES.get_or_init(|| {
            let games = ShippedData::get()
                .master_tunes
                .iter()
                .map(|(game, value)| {
                    let tune = Tune::from_legacy(value).unwrap_or_else(|e| panic!("factory tune {game:?}: {e}"));
                    (game.to_text(), tune)
                })
                .collect();
            FactoryTunes { games }
        })
    }

    /// The factory tune of `game`, vehicles included.
    pub fn get(&self, game: &str) -> Option<&Tune> {
        self.games.get(game)
    }

    pub fn games(&self) -> impl Iterator<Item = (&str, &Tune)> {
        self.games.iter().map(|(k, v)| (k.as_str(), v))
    }
}

#[cfg(test)]
mod tests;
