//! Which catalog games are running.

use super::game::WindowRule;
use super::snapshot::SystemSnapshot;
use crate::catalog::GameInfo;

/// Catalog id skipped when `CUSTOM_FEATURES.disable_rfpro` is set (rFpro shares `rFactor.exe` with rFactor 1).
const RFPRO: &str = "rfpro";

/// Catalog games whose window or process is in the snapshot, in catalog order (normally
/// `Catalog::builtin().games()`). A game is found by its window rule first, by its exact process name otherwise.
///
/// The dispatcher then tries the listeners of these games in this order
/// (`openrigcue_sources::derive::Dispatcher`).
pub fn running_games<'a>(catalog: &'a [GameInfo], snapshot: &SystemSnapshot, disable_rfpro: bool) -> Vec<&'a GameInfo> {
    catalog
        .iter()
        .filter(|game| !(disable_rfpro && game.id == RFPRO))
        .filter(|game| is_running(game, snapshot))
        .collect()
}

fn is_running(game: &GameInfo, snapshot: &SystemSnapshot) -> bool {
    let by_window = match (WindowRule::of(game), &snapshot.windows) {
        (Some(rule), Some(titles)) => rule.matches_any(titles),
        _ => false,
    };
    by_window || game.process_name.as_ref().is_some_and(|name| snapshot.processes.contains_key(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Catalog, WindowMatch};
    use crate::detect::game::tests::game;

    fn catalog() -> Vec<GameInfo> {
        vec![
            game("rfpro", Some("rFactor.exe"), Some(("rFproDX11", WindowMatch::Prefix))),
            game("rf1", Some("rFactor.exe"), Some(("rFactor v", WindowMatch::Prefix))),
            game("lfs", Some("LFS"), Some(("Live for Speed", WindowMatch::Exact))),
            game("wrs", None, Some(("World Racing Series", WindowMatch::Exact))),
        ]
    }

    fn snapshot(windows: Option<&[&str]>, processes: &[&str]) -> SystemSnapshot {
        SystemSnapshot {
            windows: windows.map(|list| list.iter().map(|title| title.to_string()).collect()),
            processes: processes.iter().map(|name| (name.to_string(), 1)).collect(),
        }
    }

    fn ids(found: Vec<&GameInfo>) -> Vec<&str> {
        found.into_iter().map(|game| game.id.as_str()).collect()
    }

    /// Shared rFactor.exe finds rfpro first, disable_rfpro skips it.
    #[test]
    fn shared_process_in_catalog_order() {
        let snap = snapshot(Some(&[]), &["rFactor.exe"]);
        assert_eq!(ids(running_games(&catalog(), &snap, false)), ["rfpro", "rf1"]);
        assert_eq!(ids(running_games(&catalog(), &snap, true)), ["rf1"]);
    }

    /// Process names match exactly ("LFS" never equals "LFS.exe"); windows only after the first
    /// snapshot.
    #[test]
    fn exact_process_names_and_window_titles() {
        assert!(running_games(&catalog(), &snapshot(Some(&[]), &["LFS.exe", "lfs"]), false).is_empty());
        let live = snapshot(Some(&["Live for Speed", "World Racing Series"]), &[]);
        assert_eq!(ids(running_games(&catalog(), &live, false)), ["lfs", "wrs"]);
        let before_first = snapshot(None, &[]);
        assert!(running_games(&catalog(), &before_first, false).is_empty());
    }

    /// The scan walks the built-in catalog in declaration order; acs.exe finds ac.
    #[test]
    fn builtin_catalog() {
        let games = Catalog::builtin().games();
        let found = running_games(games, &snapshot(Some(&[]), &["acs.exe"]), false);
        assert!(ids(found).contains(&"ac"));
        assert!(running_games(games, &snapshot(Some(&[]), &[]), false).is_empty());
    }
}
