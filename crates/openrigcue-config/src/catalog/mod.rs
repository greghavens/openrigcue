//! The game catalog: every supported title, how it is detected and which listener decodes it.
//!
//! Data: `data/catalog.toml` and `data/game_features.toml`.

mod features;
pub mod registry;

use std::sync::OnceLock;

use serde::Deserialize;

pub use features::{FeatureTable, GameFeatures};

/// The game category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GameKind {
    Racing,
    Flying,
    Others,
}

/// How the detector compares a window title with [`GameInfo::window_name`] (`window_match` in the
/// catalog data).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum WindowMatch {
    /// `M`: the title equals the window name.
    #[serde(rename = "M")]
    Exact,
    /// `P`: the title starts with the window name.
    #[serde(rename = "P")]
    Prefix,
    /// `S`: the title ends with the window name.
    #[serde(rename = "S")]
    Suffix,
}

/// Where a game can be played from, as listed on the Games→Console page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum GamePlatform {
    Console,
    #[serde(rename = "PC")]
    Pc,
}

/// One catalog entry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GameInfo {
    /// The game id: the key of profiles, ports and features.
    pub id: String,
    /// Display name.
    pub name: String,
    pub kind: GameKind,
    /// The listener (telemetry decoder) key; several games can share one listener.
    pub listener: String,
    /// Windows executable image name, when the game is detected by process.
    #[serde(default)]
    pub process_name: Option<String>,
    /// Window title (or title fragment) the detector looks for.
    #[serde(default)]
    pub window_name: Option<String>,
    #[serde(default)]
    pub window_match: Option<WindowMatch>,
    /// Explicit platform list; `None` means "PC only".
    #[serde(default)]
    pub platforms: Option<Vec<GamePlatform>>,
}

impl GameInfo {
    /// Whether the game is offered on the Games→Console page (its platform list names `Console`).
    pub fn supports_console(&self) -> bool {
        self.platforms.as_ref().is_some_and(|p| p.contains(&GamePlatform::Console))
    }
}

/// The built-in game catalog, in declaration order.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Catalog {
    #[serde(rename = "game")]
    games: Vec<GameInfo>,
}

impl Catalog {
    /// The catalog shipped with openrigcue (126 entries including the `emulator` stand-by pseudo-game).
    pub fn builtin() -> &'static Catalog {
        static CATALOG: OnceLock<Catalog> = OnceLock::new();
        CATALOG.get_or_init(|| {
            toml::from_str(include_str!("../../data/catalog.toml")).expect("data/catalog.toml is valid")
        })
    }

    pub fn games(&self) -> &[GameInfo] {
        &self.games
    }

    pub fn game(&self, id: &str) -> Option<&GameInfo> {
        self.games.iter().find(|g| g.id == id)
    }

    pub fn contains(&self, id: &str) -> bool {
        self.game(id).is_some()
    }

    /// Games decoded by `listener`, in catalog order.
    pub fn games_for_listener<'a>(&'a self, listener: &'a str) -> impl Iterator<Item = &'a GameInfo> {
        self.games.iter().filter(move |g| g.listener == listener)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The built-in catalog has 125 entries, declaration order kept.
    #[test]
    fn catalog_has_every_original_entry() {
        let c = Catalog::builtin();
        assert_eq!(c.games().len(), 125);
        assert_eq!(c.games()[0].id, "ac");
        assert_eq!(c.games().last().unwrap().id, "acesofthunder");
        assert!(c.contains("emulator"));
        assert!(!c.contains("condor3"), "condor3 has a master profile but no catalog entry");
    }

    /// Spot checks of individual catalog entries.
    #[test]
    fn entries_match_the_original_literals() {
        let c = Catalog::builtin();
        let ac = c.game("ac").unwrap();
        assert_eq!(ac.name, "Assetto Corsa");
        assert_eq!(ac.kind, GameKind::Racing);
        assert_eq!(ac.listener, "ac");
        assert_eq!(ac.process_name.as_deref(), Some("acs.exe"));
        assert_eq!(ac.window_name.as_deref(), Some("Assetto Corsa"));
        assert_eq!(ac.window_match, Some(WindowMatch::Exact));
        assert!(!ac.supports_console());

        let wrc = c.game("wrc2023").unwrap();
        assert_eq!(wrc.window_name.as_deref(), Some("EA SPORTS™ WRC  "), "trailing spaces are part of the title");

        let gt7 = c.game("gt7").unwrap();
        assert_eq!(gt7.platforms.as_deref(), Some(&[GamePlatform::Console][..]));
        assert!(gt7.supports_console());

        let nl2 = c.game("nl2").unwrap();
        assert_eq!(nl2.window_name.as_deref(), Some("NoLimits 2"));
        assert_eq!(nl2.window_match, Some(WindowMatch::Suffix), "window_type 'S' is str.endswith");
        let pmr = c.game("pmracing").unwrap();
        assert_eq!(pmr.window_name.as_deref(), Some("ProjectMotorRacing 64bit"));
        assert_eq!(pmr.window_match, Some(WindowMatch::Suffix));

        assert_eq!(c.games_for_listener("dirt").count(), 8);
    }
}
