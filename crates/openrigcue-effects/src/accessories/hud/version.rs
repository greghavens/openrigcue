//! Version equality under PEP 440, which the
//! OBD2HUD driver uses to decide whether the HUD needs the expected firmware.

use std::fmt;

/// A PEP 440 version reduced to what equality compares: the epoch, the
/// release without trailing zeros, the normalised pre-, post- and dev-release and the local segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pep440 {
    epoch: u128,
    release: Vec<u128>,
    pre: Option<(PreKind, u128)>,
    post: Option<u128>,
    dev: Option<u128>,
    local: Option<Vec<LocalPart>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreKind {
    Alpha,
    Beta,
    Candidate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LocalPart {
    Number(u128),
    Text(String),
}

/// The text is not a PEP 440 version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidVersion(pub String);

impl fmt::Display for InvalidVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Invalid version: '{}'", self.0)
    }
}

impl std::error::Error for InvalidVersion {}

impl Pep440 {
    /// Parses a version: the PEP 440 grammar matched case-insensitively against the text without
    /// surrounding whitespace, then normalised (`alpha`→`a`, `beta`→`b`, `c`/`pre`/`preview`→`rc`,
    /// `rev`/`r`→`post`, missing numbers 0, an implicit post-release `-N`).
    pub fn parse(text: &str) -> Result<Pep440, InvalidVersion> {
        let invalid = || InvalidVersion(text.to_owned());
        let lowered = text.trim_matches(is_python_whitespace).to_ascii_lowercase();
        let mut s = Scanner { rest: lowered.as_str() };
        s.eat("v");
        let mut epoch = 0;
        let mut release = vec![s.number().ok_or_else(invalid)?];
        if s.eat("!") {
            epoch = release[0];
            release = vec![s.number().ok_or_else(invalid)?];
        }
        loop {
            let save = s.rest;
            if s.eat(".") {
                if let Some(n) = s.number() {
                    release.push(n);
                    continue;
                }
            }
            s.rest = save;
            break;
        }
        let pre = s.attempt(|s| {
            s.separator();
            let kind = s.word(&[
                ("alpha", PreKind::Alpha),
                ("a", PreKind::Alpha),
                ("beta", PreKind::Beta),
                ("b", PreKind::Beta),
                ("preview", PreKind::Candidate),
                ("pre", PreKind::Candidate),
                ("c", PreKind::Candidate),
                ("rc", PreKind::Candidate),
            ])?;
            Some((kind, s.optional_number()))
        });
        let post = s.attempt(|s| if s.eat("-") { s.number() } else { None }).or_else(|| {
            s.attempt(|s| {
                s.separator();
                s.word(&[("post", ()), ("rev", ()), ("r", ())])?;
                Some(s.optional_number())
            })
        });
        let dev = s.attempt(|s| {
            s.separator();
            s.word(&[("dev", ())])?;
            Some(s.optional_number())
        });
        let local = if s.eat("+") { Some(s.local().ok_or_else(invalid)?) } else { None };
        if !s.rest.is_empty() {
            return Err(invalid());
        }
        // Every trailing zero is stripped ("0" and "0.0" keep an empty release).
        while release.last() == Some(&0) {
            release.pop();
        }
        Ok(Pep440 { epoch, release, pre, post, dev, local })
    }
}

/// Whitespace trimmed around a version: the ASCII set including the separators 0x1c-0x1f, and Unicode
/// whitespace through `char::is_whitespace`.
fn is_python_whitespace(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\u{1c}'..='\u{1f}')
}

struct Scanner<'a> {
    rest: &'a str,
}

impl Scanner<'_> {
    fn eat(&mut self, token: &str) -> bool {
        match self.rest.strip_prefix(token) {
            Some(rest) => {
                self.rest = rest;
                true
            }
            None => false,
        }
    }

    /// `[-_\.]?`
    fn separator(&mut self) {
        let _ = self.eat("-") || self.eat("_") || self.eat(".");
    }

    /// `[0-9]+` (ASCII digits only, like the pattern's `[0-9]`).
    fn number(&mut self) -> Option<u128> {
        let len = self.rest.bytes().take_while(u8::is_ascii_digit).count();
        if len == 0 {
            return None;
        }
        let (digits, rest) = self.rest.split_at(len);
        self.rest = rest;
        // Parts are held in a u128; versions with more than 38 digits in a part are not realistic.
        Some(digits.parse().unwrap_or(u128::MAX))
    }

    /// `[-_\.]?(?P<n>[0-9]+)?`, 0 when absent.
    fn optional_number(&mut self) -> u128 {
        self.attempt(|s| {
            s.separator();
            s.number()
        })
        .unwrap_or(0)
    }

    /// The longest of `words` (they are listed so that a longer alternative precedes its prefixes, as in the
    /// regular expression).
    fn word<T: Copy>(&mut self, words: &[(&str, T)]) -> Option<T> {
        words.iter().find(|(w, _)| self.eat(w)).map(|&(_, t)| t)
    }

    /// Runs `f`, restoring the position when it fails.
    fn attempt<T>(&mut self, f: impl FnOnce(&mut Self) -> Option<T>) -> Option<T> {
        let save = self.rest;
        let result = f(self);
        if result.is_none() {
            self.rest = save;
        }
        result
    }

    /// `[a-z0-9]+(?:[-_\.][a-z0-9]+)*`, numeric parts as numbers.
    fn local(&mut self) -> Option<Vec<LocalPart>> {
        let mut parts = Vec::new();
        loop {
            let len = self.rest.bytes().take_while(|b| b.is_ascii_lowercase() || b.is_ascii_digit()).count();
            if len == 0 {
                return None;
            }
            let (part, rest) = self.rest.split_at(len);
            self.rest = rest;
            parts.push(if part.bytes().all(|b| b.is_ascii_digit()) {
                LocalPart::Number(part.parse().unwrap_or(u128::MAX))
            } else {
                LocalPart::Text(part.to_owned())
            });
            if !(self.eat("-") || self.eat("_") || self.eat(".")) {
                return Some(parts);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn same(a: &str, b: &str) -> bool {
        Pep440::parse(a).unwrap() == Pep440::parse(b).unwrap()
    }

    /// Trailing release zeros, `v` prefix, case, whitespace and spelling variants compare equal.
    #[test]
    fn normalised_forms_are_equal() {
        assert!(same("1.0.0", "1.0.0"));
        assert!(same("1.0.0", "1"));
        assert!(same("1.0.0", "1.0"));
        assert!(same("1.0.0", "v1.0.0"));
        assert!(same("1.0.0", " 1.0.0\n"));
        assert!(same("0!1.0", "1.0"));
        assert!(same("1.0alpha1", "1.0a1"));
        assert!(same("1.0-BETA.2", "1.0b2"));
        assert!(same("1.0c1", "1.0rc1"));
        assert!(same("1.0pre", "1.0rc0"));
        assert!(same("1.0-1", "1.0.post1"));
        assert!(same("1.0rev", "1.0post0"));
        assert!(same("1.0.dev", "1.0dev0"));
        assert!(same("1.0+ABC.01", "1.0+abc-1"));
        assert!(same("0", "0.0.0"));
    }

    /// The comparison the driver makes: different releases, pre/post/dev/local markers differ.
    #[test]
    fn different_versions_differ() {
        assert!(!same("1.0.0", "1.0.1"));
        assert!(!same("1.0.0", "1.0.0a1"));
        assert!(!same("1.0.0", "1.0.0.post1"));
        assert!(!same("1.0.0", "1.0.0.dev0"));
        assert!(!same("1.0.0", "1.0.0+local"));
        assert!(!same("1.0.0", "1!1.0.0"));
    }

    /// Anything outside the grammar is invalid (a firmware string that kept its last digit
    /// off, an empty string, a non-PEP 440 text).
    #[test]
    fn invalid_versions() {
        for text in ["", "1.0.", "abc", "1.0.0 beta", "1..0", "1.0+", "1.0+a..b", "١.0"] {
            assert_eq!(Pep440::parse(text), Err(InvalidVersion(text.to_owned())), "{text:?}");
        }
    }
}
