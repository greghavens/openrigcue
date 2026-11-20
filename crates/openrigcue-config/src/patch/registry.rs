//! String values of a Wine prefix's `HKEY_CURRENT_USER` hive (`user.reg`), which the
//! patch modules read registry values from.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;

/// String values per key; key paths and value names are compared case-insensitively, as by the Windows registry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserHive {
    keys: HashMap<String, HashMap<String, String>>,
}

impl UserHive {
    /// Loads a `user.reg` file.
    pub fn load(path: &Path) -> io::Result<Self> {
        let bytes = fs::read(path)?;
        Ok(Self::parse(&String::from_utf8_lossy(&bytes)))
    }

    /// Parses the text of a Wine registry file; only string values (`"…"`, `str(2):"…"`) are kept.
    pub fn parse(text: &str) -> Self {
        let mut keys: HashMap<String, HashMap<String, String>> = HashMap::new();
        let mut current: Option<String> = None;
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix('[') {
                current = rest.rfind(']').map(|end| normalize_key(&unescape(&rest[..end])));
            } else if let (Some(key), Some((name, value))) = (&current, parse_value_line(line)) {
                keys.entry(key.clone()).or_default().insert(name.to_lowercase(), value);
            }
        }
        UserHive { keys }
    }

    /// A string value; `name` is `""` for the key's default value. Empty components of `key` (a doubled
    /// backslash) are ignored.
    pub fn string_value(&self, key: &str, name: &str) -> Option<&str> {
        self.keys.get(&normalize_key(key))?.get(&name.to_lowercase()).map(String::as_str)
    }
}

fn normalize_key(key: &str) -> String {
    key.split('\\').filter(|part| !part.is_empty()).collect::<Vec<_>>().join("\\").to_lowercase()
}

/// `"name"="value"`, `"name"=str(2):"value"` or `@="value"`.
fn parse_value_line(line: &str) -> Option<(String, String)> {
    let (name, rest) = if let Some(rest) = line.strip_prefix('@') {
        (String::new(), rest)
    } else {
        let (name, rest) = quoted(line.strip_prefix('"')?)?;
        (name, rest)
    };
    let data = rest.strip_prefix('=')?;
    let data = data.strip_prefix("str(2):").unwrap_or(data);
    let (value, _) = quoted(data.strip_prefix('"')?)?;
    Some((name, value))
}

/// Reads an escaped string up to its closing quote; returns it and the text after the quote.
fn quoted(text: &str) -> Option<(String, &str)> {
    let mut escaped = false;
    for (index, c) in text.char_indices() {
        match (escaped, c) {
            (true, _) => escaped = false,
            (false, '\\') => escaped = true,
            (false, '"') => return Some((unescape(&text[..index]), &text[index + 1..])),
            _ => {}
        }
    }
    None
}

/// Wine registry escapes: `\\`, `\"`, the C letter escapes, `\xHHHH` and octal `\NNN`.
fn unescape(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let Some(code) = chars.next() else { break };
        let decoded = match code {
            'a' => Some('\u{7}'),
            'b' => Some('\u{8}'),
            'e' => Some('\u{1b}'),
            'f' => Some('\u{c}'),
            'n' => Some('\n'),
            'r' => Some('\r'),
            't' => Some('\t'),
            'v' => Some('\u{b}'),
            'x' => numeric(&mut chars, 16, 4),
            '0'..='7' => {
                let first = code.to_digit(8).unwrap_or(0);
                numeric_from(first, &mut chars, 8, 2)
            }
            other => Some(other),
        };
        out.extend(decoded);
    }
    out
}

fn numeric(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, radix: u32, max: usize) -> Option<char> {
    numeric_from(0, chars, radix, max)
}

fn numeric_from(
    start: u32,
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    radix: u32,
    max: usize,
) -> Option<char> {
    let mut value = start;
    for _ in 0..max {
        let Some(digit) = chars.peek().and_then(|c| c.to_digit(radix)) else {
            break;
        };
        value = value * radix + digit;
        chars.next();
    }
    char::from_u32(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER_REG: &str = "WINE REGISTRY Version 2\n;; All keys relative to \\\\User\\\\S-1-5-21-0-0-0-1000\n\n\
[Software\\\\Condor2] 1700000000\n#time=1d0\n\"InstallDir\"=\"C:\\\\Condor2\"\n\n\
[Software\\\\Live for Speed] 1700000000\n@=\"C:\\\\LFS\\\\\"\n\"Other\"=dword:00000001\n\n\
[Software\\\\Microsoft\\\\Windows\\\\CurrentVersion\\\\Explorer\\\\Shell Folders] 1700000000\n\
\"Personal\"=str(2):\"C:\\\\users\\\\steamuser\\\\Documents\"\n\"Quote\"=\"a\\\"b\\x41\"\n";

    /// Registry value lookups: named and default string values, case-insensitive keys.
    #[test]
    fn string_values() {
        let hive = UserHive::parse(USER_REG);
        assert_eq!(hive.string_value("Software\\\\Condor2", "InstallDir"), Some("C:\\Condor2"));
        assert_eq!(hive.string_value("software\\condor2", "installdir"), Some("C:\\Condor2"));
        assert_eq!(hive.string_value("Software\\Live for Speed", ""), Some("C:\\LFS\\"));
        assert_eq!(hive.string_value("Software\\Live for Speed", "Other"), None);
        let folders = "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Shell Folders";
        assert_eq!(hive.string_value(folders, "Personal"), Some("C:\\users\\steamuser\\Documents"));
        assert_eq!(hive.string_value(folders, "Quote"), Some("a\"bA"));
        assert_eq!(hive.string_value("Software\\Condor3", "InstallDir"), None);
    }
}
