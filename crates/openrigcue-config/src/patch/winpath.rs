//! Windows-style relative paths on a case-sensitive file system.

use std::fs;
use std::path::{Path, PathBuf};

/// Components of a Windows relative path (`a\b/c`), empty components dropped.
pub fn components(relative: &str) -> impl Iterator<Item = &str> {
    relative.split(['\\', '/']).filter(|part| !part.is_empty())
}

/// Joins a Windows relative path to `base`, matching each component case-insensitively when no entry has the
/// exact name (Windows paths are case-insensitive). Components that do not exist are appended as written.
pub fn resolve(base: &Path, relative: &str) -> PathBuf {
    let mut path = base.to_path_buf();
    for part in components(relative) {
        let exact = path.join(part);
        path = if exact.exists() { exact } else { case_insensitive_child(&path, part).unwrap_or(exact) };
    }
    path
}

fn case_insensitive_child(dir: &Path, name: &str) -> Option<PathBuf> {
    let wanted = name.to_lowercase();
    let mut matches: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|entry| entry.file_name().to_str().is_some_and(|found| found.to_lowercase() == wanted))
        .map(|entry| entry.path())
        .collect();
    matches.sort();
    matches.into_iter().next()
}

/// Base name of a Windows relative path (`\` separators).
pub fn file_name(relative: &str) -> &str {
    components(relative).last().unwrap_or_default()
}

/// Every component but the last `count` ones, joined with `\`.
pub fn without_last(relative: &str, count: usize) -> String {
    let parts: Vec<&str> = components(relative).collect();
    parts[..parts.len().saturating_sub(count)].join("\\")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Windows path lookups ignore case; missing tails are kept as written.
    #[test]
    fn resolves_case_insensitively() {
        let base = std::env::temp_dir().join(format!("openrigcue-config-b-winpath-{}", std::process::id()));
        fs::create_dir_all(base.join("My Games/DiRT Rally 2.0/hardwaresettings")).unwrap();
        let found = resolve(&base, "my games\\dirt rally 2.0\\HardwareSettings\\hardware_settings_config.xml");
        assert_eq!(found, base.join("My Games/DiRT Rally 2.0/hardwaresettings/hardware_settings_config.xml"));
        assert_eq!(resolve(&base, "Other\\x"), base.join("Other/x"));
    }

    /// Base name and parent folders of the plugin locations.
    #[test]
    fn path_parts() {
        assert_eq!(file_name("rFactor\\Plugins\\SRS_Rfactor1.dll"), "SRS_Rfactor1.dll");
        assert_eq!(without_last("rFactor\\Plugins\\SRS_Rfactor1.dll", 2), "rFactor");
        assert_eq!(without_last("a", 2), "");
    }
}
