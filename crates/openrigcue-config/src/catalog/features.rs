//! Which effects and flags each game provides (`data/game_features.toml`).

use std::sync::OnceLock;

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct FeatureRow {
    id: String,
    enabled: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct FeatureFile {
    columns: Vec<String>,
    #[serde(rename = "game")]
    games: Vec<FeatureRow>,
}

/// The features one game provides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameFeatures {
    enabled: Vec<String>,
}

impl GameFeatures {
    /// Whether the game provides the feature named by a column of the table.
    pub fn supports(&self, feature: &str) -> bool {
        self.enabled.iter().any(|f| f == feature)
    }

    pub fn enabled(&self) -> &[String] {
        &self.enabled
    }
}

/// The parsed feature table.
///
/// The header row names the columns; every later row maps its first cell (the game id) to one
/// boolean per column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureTable {
    columns: Vec<String>,
    games: Vec<(String, GameFeatures)>,
}

impl FeatureTable {
    pub fn builtin() -> &'static FeatureTable {
        static TABLE: OnceLock<FeatureTable> = OnceLock::new();
        TABLE.get_or_init(|| {
            let file: FeatureFile = toml::from_str(include_str!("../../data/game_features.toml"))
                .expect("data/game_features.toml is valid");
            FeatureTable {
                columns: file.columns,
                games: file.games.into_iter().map(|r| (r.id, GameFeatures { enabled: r.enabled })).collect(),
            }
        })
    }

    /// Column names in header order.
    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    /// Features of `game`; `None` for games without a row.
    pub fn game(&self, game: &str) -> Option<&GameFeatures> {
        self.games.iter().find(|(id, _)| id == game).map(|(_, f)| f)
    }

    pub fn game_ids(&self) -> impl Iterator<Item = &str> {
        self.games.iter().map(|(id, _)| id.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins rows of the feature table.
    #[test]
    fn table_matches_csv() {
        let t = FeatureTable::builtin();
        assert_eq!(t.columns().len(), 48);
        assert_eq!(t.columns()[0], "speed");
        assert_eq!(t.columns().last().unwrap(), "turbulence");
        assert_eq!(t.game_ids().count(), 108);
        let ww1 = t.game("warplanes_ww1").unwrap();
        assert!(ww1.supports("cannon_firing"));
        assert!(ww1.supports("pitch"));
        assert!(!ww1.supports("speed"));
        assert!(!ww1.supports("missile_firing"));
        assert!(t.game("not_a_game").is_none());
    }
}
