//! Window-title rules of catalog games.

use crate::catalog::{GameInfo, WindowMatch};

/// How a catalog window name is compared with a window title.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitleMatch {
    /// `'M'`: the title equals the name.
    Exact,
    /// `'P'`: the title starts with the name.
    Prefix,
    /// `'S'`: the title ends with the name.
    Suffix,
}

impl From<WindowMatch> for TitleMatch {
    /// The comparison named by a catalog window-match code (`'S'` is a suffix match).
    fn from(code: WindowMatch) -> Self {
        match code {
            WindowMatch::Exact => TitleMatch::Exact,
            WindowMatch::Prefix => TitleMatch::Prefix,
            WindowMatch::Suffix => TitleMatch::Suffix,
        }
    }
}

/// Window title rule of a catalog game (window name and match kind).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowRule<'a> {
    pub title: &'a str,
    pub matching: TitleMatch,
}

impl<'a> WindowRule<'a> {
    /// The rule of a catalog entry; there is no rule when either field is missing.
    pub fn of(game: &'a GameInfo) -> Option<Self> {
        Some(WindowRule { title: game.window_name.as_deref()?, matching: game.window_match?.into() })
    }

    /// Whether any non-empty title satisfies the rule (case-sensitive).
    pub fn matches_any(&self, titles: &[String]) -> bool {
        titles.iter().filter(|title| !title.is_empty()).any(|title| match self.matching {
            TitleMatch::Exact => title == self.title,
            TitleMatch::Prefix => title.starts_with(self.title),
            TitleMatch::Suffix => title.ends_with(self.title),
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::catalog::{Catalog, GameKind};

    /// A catalog entry for detection tests.
    pub(crate) fn game(id: &str, process: Option<&str>, window: Option<(&str, WindowMatch)>) -> GameInfo {
        GameInfo {
            id: id.into(),
            name: id.into(),
            kind: GameKind::Racing,
            listener: id.into(),
            process_name: process.map(str::to_owned),
            window_name: window.map(|(name, _)| name.to_owned()),
            window_match: window.map(|(_, code)| code),
            platforms: None,
        }
    }

    fn rule(title: &str, matching: TitleMatch) -> WindowRule<'_> {
        WindowRule { title, matching }
    }

    fn titles(list: &[&str]) -> Vec<String> {
        list.iter().map(|title| title.to_string()).collect()
    }

    /// M exact, P prefix, S suffix, case-sensitive.
    #[test]
    fn title_rules() {
        let open = titles(&["", "rFactor v1.255", "NoLimits 2", "Live for Speed"]);
        assert!(rule("rFactor v", TitleMatch::Prefix).matches_any(&open));
        assert!(!rule("rfactor v", TitleMatch::Prefix).matches_any(&open));
        assert!(rule("Live for Speed", TitleMatch::Exact).matches_any(&open));
        assert!(!rule("Live for", TitleMatch::Exact).matches_any(&open));
        assert!(rule("2", TitleMatch::Suffix).matches_any(&open));
        assert!(!rule("NoLimits", TitleMatch::Suffix).matches_any(&open));
    }

    /// Empty titles are skipped, an empty name matches any non-empty title.
    #[test]
    fn empty_titles() {
        assert!(!rule("", TitleMatch::Prefix).matches_any(&titles(&[""])));
        assert!(rule("", TitleMatch::Prefix).matches_any(&titles(&["x"])));
    }

    /// A missing window name or match kind drops the whole window rule.
    #[test]
    fn rule_needs_both_fields() {
        let mut entry = game("x", None, Some(("x", WindowMatch::Exact)));
        assert!(WindowRule::of(&entry).is_some());
        entry.window_match = None;
        assert_eq!(WindowRule::of(&entry), None);
        let entry = GameInfo { window_name: None, ..game("x", None, Some(("x", WindowMatch::Exact))) };
        assert_eq!(WindowRule::of(&entry), None);
    }

    /// The catalog's 'S' entries (nl2, pmracing): the title must end with the name; a title
    /// that only contains it in the middle does not match.
    #[test]
    fn catalog_suffix_code_is_endswith() {
        let catalog = Catalog::builtin();
        let nl2 = WindowRule::of(catalog.game("nl2").unwrap()).unwrap();
        assert_eq!(nl2.matching, TitleMatch::Suffix);
        assert!(nl2.matches_any(&titles(&["Park - NoLimits 2"])));
        assert!(!nl2.matches_any(&titles(&["NoLimits 2 - Park"])));
    }
}
