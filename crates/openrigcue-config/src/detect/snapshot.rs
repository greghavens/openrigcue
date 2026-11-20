//! The once-per-second snapshot of open windows and running processes.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use super::processes::running_processes;
use crate::catalog::GameInfo;

/// Pause between two snapshots.
pub const SNAPSHOT_PERIOD: Duration = Duration::from_secs(1);

/// Open window titles and running processes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SystemSnapshot {
    /// Window titles; `None` until the first snapshot (window matching is disabled until then).
    pub windows: Option<Vec<String>>,
    /// Executable base name → pid.
    pub processes: HashMap<String, u32>,
}

/// Source of top-level window titles.
pub trait WindowTitles {
    /// Titles of the visible top-level windows, in any order.
    fn titles(&mut self) -> Vec<String>;
}

/// No window system available: no titles, detection relies on process names.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoWindowTitles;

impl WindowTitles for NoWindowTitles {
    fn titles(&mut self) -> Vec<String> {
        Vec::new()
    }
}

/// Takes snapshots from a title source and a `/proc` tree.
pub struct Snapshotter<T: WindowTitles> {
    titles: T,
    proc_root: PathBuf,
}

impl<T: WindowTitles> Snapshotter<T> {
    /// A snapshotter reading processes from `proc_root` (normally `/proc`).
    pub fn new(titles: T, proc_root: PathBuf) -> Self {
        Snapshotter { titles, proc_root }
    }

    /// One snapshot. With a forced game the window list is only that game's catalog
    /// window name (empty when the id is unknown or has no window name); processes are always enumerated, so a
    /// running game earlier in catalog order is still found first.
    pub fn take(&mut self, forced_game: Option<&str>, catalog: &[GameInfo]) -> SystemSnapshot {
        self.take_with_windows(forced_game, catalog, |_| {})
    }

    /// Publish the window-list assignment before process enumeration starts.
    pub fn take_with_windows(
        &mut self,
        forced_game: Option<&str>,
        catalog: &[GameInfo],
        publish_windows: impl FnOnce(&[String]),
    ) -> SystemSnapshot {
        let (windows, processes) = self.take_staged(forced_game, catalog, publish_windows, running_processes);
        SystemSnapshot { windows: Some(windows), processes }
    }

    fn take_staged<P>(
        &mut self,
        forced_game: Option<&str>,
        catalog: &[GameInfo],
        publish_windows: impl FnOnce(&[String]),
        processes: impl FnOnce(&std::path::Path) -> P,
    ) -> (Vec<String>, P) {
        let windows = match forced_game {
            None => self.titles.titles(),
            Some(id) => forced_window(catalog, id),
        };
        publish_windows(&windows);
        let processes = processes(&self.proc_root);
        (windows, processes)
    }
}

fn forced_window(catalog: &[GameInfo], id: &str) -> Vec<String> {
    match catalog.iter().find(|game| game.id == id).and_then(|game| game.window_name.as_ref()) {
        Some(title) => vec![title.clone()],
        None => {
            tracing::error!(game = id, "error opened window");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Catalog, WindowMatch};
    use crate::detect::game::tests::game;

    struct FixedTitles(Vec<String>);

    impl WindowTitles for FixedTitles {
        fn titles(&mut self) -> Vec<String> {
            self.0.clone()
        }
    }

    fn catalog() -> Vec<GameInfo> {
        let mut no_code = game("nocode", None, Some(("Only a name", WindowMatch::Exact)));
        no_code.window_match = None;
        vec![game("api", None, Some(("superapi1234", WindowMatch::Exact))), no_code]
    }

    /// Live titles normally, the forced game's window name otherwise, [] for unknown ids.
    #[test]
    fn forced_game_replaces_titles() {
        let mut snapshotter =
            Snapshotter::new(FixedTitles(vec!["Desktop".into()]), PathBuf::from("/nonexistent/openrigcue-proc"));
        assert_eq!(snapshotter.take(None, &catalog()).windows, Some(vec!["Desktop".to_string()]));
        assert_eq!(snapshotter.take(Some("api"), &catalog()).windows, Some(vec!["superapi1234".to_string()]));
        assert_eq!(snapshotter.take(Some("warplanes"), &catalog()).windows, Some(vec![]));
        assert_eq!(
            snapshotter.take(Some("nocode"), &catalog()).windows,
            Some(vec!["Only a name".to_string()]),
            "the forced window list needs only window_name"
        );
        let builtin = Catalog::builtin().games();
        assert_eq!(snapshotter.take(Some("ac"), builtin).windows, Some(vec!["Assetto Corsa".to_string()]));
        assert_eq!(SystemSnapshot::default().windows, None);
    }

    #[test]
    fn staged_snapshot_publishes_windows_before_process_enumeration() {
        use std::cell::RefCell;
        let published = RefCell::new(SystemSnapshot {
            windows: Some(vec!["old".into()]),
            processes: [("old.exe".into(), 1)].into(),
        });
        let mut snapshotter = Snapshotter::new(FixedTitles(vec!["new".into()]), PathBuf::from("unused"));
        let (_, processes) = snapshotter.take_staged(
            None,
            &catalog(),
            |windows| {
                published.borrow_mut().windows = Some(windows.to_vec());
            },
            |_| {
                assert_eq!(published.borrow().windows, Some(vec!["new".into()]));
                assert_eq!(published.borrow().processes.get("old.exe"), Some(&1));
                [("new.exe".to_owned(), 2)].into_iter().collect::<HashMap<_, _>>()
            },
        );
        published.borrow_mut().processes = processes;
        assert_eq!(published.borrow().processes.get("new.exe"), Some(&2));
    }

    #[test]
    fn staged_snapshot_retains_published_windows_when_process_scan_fails() {
        use std::cell::RefCell;
        let published = RefCell::new(None);
        let mut snapshotter = Snapshotter::new(FixedTitles(vec!["new".into()]), PathBuf::from("unused"));
        let (_, processes): (_, Result<(), &str>) = snapshotter.take_staged(
            Some("api"),
            &catalog(),
            |windows| {
                *published.borrow_mut() = Some(windows.to_vec());
            },
            |_| Err("scripted process failure"),
        );
        assert_eq!(processes, Err("scripted process failure"));
        assert_eq!(*published.borrow(), Some(vec!["superapi1234".to_owned()]));
    }
}
