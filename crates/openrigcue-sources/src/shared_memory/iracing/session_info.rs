//! The iRacing session-info YAML: extracting, cleaning, parsing and caching it.

use std::collections::HashMap;

use yaml_rust2::Yaml;

use super::Raised;

/// Where the session string lives in the memory map (`session_info_offset` / `session_info_len`) and its
/// update counter (`session_info_update`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct SessionRegion {
    pub(crate) offset: i32,
    pub(crate) len: i32,
    pub(crate) update: i32,
}

impl SessionRegion {
    /// Both search positions are clamped independently, without negative-index wrapping.
    pub(crate) fn bounds(self, size: usize) -> (usize, usize) {
        let start = i64::from(self.offset).clamp(0, size as i64) as usize;
        let end = (i64::from(self.offset) + i64::from(self.len)).clamp(0, size as i64) as usize;
        (start, end.saturating_sub(start))
    }
}

/// Per-key cache entry.
#[derive(Default)]
struct Entry {
    /// The parsed data; `None` when there is none.
    data: Option<Yaml>,
    /// The previous data, present once truthy data was superseded by a session update.
    data_last: Option<Yaml>,
    /// The raw block last parsed.
    data_binary: Option<Vec<u8>>,
}

/// The session-info cache: block extraction and YAML parsing, done synchronously.
#[derive(Default)]
pub(crate) struct SessionInfo {
    last_update: i32,
    entries: HashMap<String, Entry>,
}

impl SessionInfo {
    pub(crate) fn needs_update(&self, update: i32) -> bool {
        self.last_update < update
    }

    pub(crate) fn advance(&mut self, update: i32) {
        self.last_update = update;
        for entry in self.entries.values_mut() {
            if entry.data.as_ref().is_some_and(truthy) {
                entry.data_last = entry.data.clone();
            }
            entry.data = None;
        }
    }

    pub(crate) fn cached(&self, key: &str) -> Option<Yaml> {
        self.entries.get(key)?.data.as_ref().filter(|value| truthy(value)).cloned()
    }

    /// Forgets everything (shutdown).
    pub(crate) fn clear(&mut self) {
        *self = SessionInfo::default();
    }

    /// The parsed top-level `key` block, `None` when absent.
    pub(crate) fn lookup(&mut self, memory: &[u8], region: SessionRegion, key: &str) -> Result<Option<Yaml>, Raised> {
        if self.needs_update(region.update) {
            self.advance(region.update);
        }
        self.lookup_current(memory, region, key)
    }

    pub(crate) fn lookup_current(
        &mut self,
        memory: &[u8],
        region: SessionRegion,
        key: &str,
    ) -> Result<Option<Yaml>, Raised> {
        let entry = self.entries.entry(key.to_owned()).or_default();
        if entry.data.as_ref().is_some_and(truthy) {
            return Ok(entry.data.clone());
        }
        parse_into(entry, memory, region, key)?;
        Ok(entry.data.clone())
    }
}

/// Parses the block of `key` and updates its cache entry.
fn parse_into(entry: &mut Entry, memory: &[u8], region: SessionRegion, key: &str) -> Result<(), Raised> {
    let Some(binary) = block(memory, region, key).filter(|binary| !binary.is_empty()) else {
        return Ok(());
    };
    if entry.data_binary.as_deref() == Some(binary) && entry.data_last.is_some() {
        entry.data = entry.data_last.clone();
        return Ok(());
    }
    entry.data_binary = Some(binary.to_vec());
    let source = clean_source(binary, key)?;
    let Some(result) = super::yaml11::load(&source)?.filter(truthy) else {
        return Ok(());
    };
    let Yaml::Hash(mut map) = result else { return Err(Raised) };
    let data = map.remove(&Yaml::String(key.to_owned())).ok_or(Raised)?;
    entry.data = if truthy(&data) { Some(data) } else { entry.data_last.clone().or(Some(data)) };
    Ok(())
}

/// The raw block of `key`: from the line `key:` to the first blank line, searched inside
/// the session region.
fn block<'m>(memory: &'m [u8], region: SessionRegion, key: &str) -> Option<&'m [u8]> {
    let (start, len) = region.bounds(memory.len());
    let region = &memory[start..start + len];
    let header = format!("\n{key}:\n").into_bytes();
    let header_at = find(region, &header, 0)?;
    let blank_at = find(region, b"\n\n", header_at + 1)?;
    Some(&region[header_at + 1..blank_at])
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    haystack.get(from..)?.windows(needle.len()).position(|window| window == needle).map(|at| at + from)
}

/// The text handed to the YAML loader: undefined cp1252 bytes become spaces, trailing NULs are
/// stripped, the cp1252 text loses YAML non-printable characters, `DriverInfo` name fields are quoted, and values
/// starting with a comma are quoted.
fn clean_source(binary: &[u8], key: &str) -> Result<String, Raised> {
    let translated: Vec<u8> =
        binary.iter().map(|&b| if matches!(b, 0x81 | 0x8d | 0x8f | 0x90 | 0x9d) { b' ' } else { b }).collect();
    let end = translated.iter().rposition(|&b| b != 0).map_or(0, |last| last + 1);
    let text: String = translated[..end].iter().map(|&b| cp1252(b)).filter(|&c| yaml_printable(c)).collect();
    let mut lines = Vec::new();
    for line in text.split('\n') {
        let line = if key == "DriverInfo" { quote_driver_name(line)? } else { line.to_owned() };
        lines.push(quote_comma_value(&line));
    }
    Ok(lines.join("\n"))
}

/// Windows-1252 decoding of one byte (the five undefined bytes were already translated to spaces).
fn cp1252(byte: u8) -> char {
    const HIGH: [char; 32] = [
        '\u{20AC}', '\u{81}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}', '\u{2021}', '\u{02C6}',
        '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{8D}', '\u{017D}', '\u{8F}', '\u{90}', '\u{2018}',
        '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}', '\u{2014}', '\u{02DC}', '\u{2122}', '\u{0161}',
        '\u{203A}', '\u{0153}', '\u{9D}', '\u{017E}', '\u{0178}',
    ];
    match byte {
        0x80..=0x9f => HIGH[usize::from(byte - 0x80)],
        _ => char::from(byte),
    }
}

/// Characters YAML treats as printable, which are kept.
fn yaml_printable(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{7E}' | '\u{85}' | '\u{A0}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..)
}

/// The `DriverInfo` substitution: after `DriverSetupName`, `UserName`, `TeamName`, `AbbrevName` or `Initials`, the
/// value (quoted or not) becomes a quoted, escaped value.
/// An empty quoted value (`""`) is an error.
fn quote_driver_name(line: &str) -> Result<String, Raised> {
    const KEYS: [&str; 5] = ["DriverSetupName: ", "UserName: ", "TeamName: ", "AbbrevName: ", "Initials: "];
    for (start, _) in line.char_indices() {
        let Some(key) = KEYS.iter().find(|key| line[start..].starts_with(*key)) else { continue };
        let value_at = start + key.len();
        let rest = &line[value_at..];
        if rest.is_empty() {
            continue;
        }
        let value = if rest.len() >= 2 && rest.starts_with('"') && rest.ends_with('"') {
            let inner = &rest[1..rest.len() - 1];
            if inner.is_empty() {
                return Err(Raised);
            }
            inner
        } else {
            rest
        };
        let escaped: String =
            value.chars().flat_map(|c| (c == '"' || c == '\\').then_some('\\').into_iter().chain([c])).collect();
        return Ok(format!("{}\"{escaped}\"", &line[..value_at]));
    }
    Ok(line.to_owned())
}

/// The comma substitution: the first value in the line that starts with a comma
/// and follows a `word: ` is quoted up to the end of the line.
fn quote_comma_value(line: &str) -> String {
    let word_char = |c: char| c.is_alphanumeric() || c == '_';
    let mut search = 0;
    while let Some(found) = line[search..].find(": ,") {
        let colon = search + found;
        if line[..colon].chars().next_back().is_some_and(word_char) {
            let value_at = colon + 2;
            return format!("{}\"{}\"", &line[..value_at], &line[value_at..]);
        }
        search = colon + 1;
    }
    line.to_owned()
}

/// Truthiness of a parsed YAML value.
pub(crate) fn truthy(value: &Yaml) -> bool {
    match value {
        Yaml::Null | Yaml::BadValue => false,
        Yaml::Boolean(b) => *b,
        Yaml::Integer(i) => *i != 0,
        Yaml::Real(_) => value.as_f64().is_some_and(|f| f != 0.0),
        Yaml::String(s) => !s.is_empty(),
        Yaml::Array(items) => !items.is_empty(),
        Yaml::Hash(map) => !map.is_empty(),
        Yaml::Alias(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(text: &str) -> (Vec<u8>, SessionRegion) {
        let mut memory = vec![0u8; 16];
        memory.extend_from_slice(text.as_bytes());
        memory.extend_from_slice(&[0; 8]);
        let region = SessionRegion { offset: 16, len: (text.len() + 8) as i32, update: 1 };
        (memory, region)
    }

    const SESSION: &str = "---\nWeekendInfo:\n TrackDisplayShortName: Spa\n\nDriverInfo:\n DriverCarIdx: 1\n DriverCarSLBlinkRPM: 7400.000\n Drivers:\n - CarIdx: 0\n   UserName: \"Pace Car\"\n   CarScreenName: Safety Car\n - CarIdx: 1\n   UserName: J. \"Fast\" Doe\n   CarScreenName: Mazda MX-5 Cup\n\n...\n";

    /// A top-level block is parsed to its mapping; an absent key is None.
    #[test]
    fn lookup_parses_blocks() {
        let (memory, region) = image(SESSION);
        let mut info = SessionInfo::default();
        let driver = info.lookup(&memory, region, "DriverInfo").unwrap().unwrap();
        assert_eq!(driver["DriverCarIdx"].as_i64(), Some(1));
        assert_eq!(driver["DriverCarSLBlinkRPM"].as_f64(), Some(7400.0));
        assert_eq!(driver["Drivers"][1]["CarScreenName"].as_str(), Some("Mazda MX-5 Cup"));
        assert_eq!(driver["Drivers"][1]["UserName"].as_str(), Some("J. \"Fast\" Doe"));
        let weekend = info.lookup(&memory, region, "WeekendInfo").unwrap().unwrap();
        assert_eq!(weekend["TrackDisplayShortName"].as_str(), Some("Spa"));
        assert_eq!(info.lookup(&memory, region, "SessionNum").unwrap(), None);
    }

    /// A block is cached until session_info_update increases; then re-read from memory.
    #[test]
    fn cache_until_update() {
        let (memory, mut region) = image(SESSION);
        let mut info = SessionInfo::default();
        info.lookup(&memory, region, "WeekendInfo").unwrap();
        let (changed, _) = image(&SESSION.replace("Spa", "Monza"));
        assert_eq!(
            info.lookup(&changed, region, "WeekendInfo").unwrap().unwrap()["TrackDisplayShortName"].as_str(),
            Some("Spa")
        );
        region.update = 2;
        assert_eq!(
            info.lookup(&changed, region, "WeekendInfo").unwrap().unwrap()["TrackDisplayShortName"].as_str(),
            Some("Monza")
        );
    }

    #[test]
    fn session_search_clamps_negative_positions_without_wrapping() {
        let memory = b"\nWeekendInfo:\n TrackDisplayShortName: Spa\n\n";
        let region = SessionRegion { offset: -8, len: memory.len() as i32 + 8, update: 1 };
        let mut info = SessionInfo::default();
        assert_eq!(
            info.lookup(memory, region, "WeekendInfo").unwrap().unwrap()["TrackDisplayShortName"].as_str(),
            Some("Spa")
        );
        assert_eq!(SessionRegion { offset: 12, len: -20, update: 0 }.bounds(memory.len()), (12, 0));
        assert_eq!(SessionRegion { offset: -8, len: 8, update: 0 }.bounds(memory.len()), (0, 0));
        assert_eq!(
            SessionRegion { offset: i32::MAX, len: i32::MAX, update: 0 }.bounds(memory.len()),
            (memory.len(), 0)
        );
    }

    /// After an update with an unchanged block, the previous data is reused.
    #[test]
    fn unchanged_block_reuses_last_data() {
        let (memory, mut region) = image(SESSION);
        let mut info = SessionInfo::default();
        info.lookup(&memory, region, "DriverInfo").unwrap();
        region.update = 5;
        assert!(info.lookup(&memory, region, "DriverInfo").unwrap().is_some());
    }

    /// Name substitution: an empty quoted UserName is an error.
    #[test]
    fn empty_quoted_name_raises() {
        assert!(quote_driver_name(" UserName: \"\"").is_err());
        assert_eq!(quote_driver_name("   UserName: A \\ B").unwrap(), "   UserName: \"A \\\\ B\"");
        assert_eq!(quote_driver_name(" Initials: ").unwrap(), " Initials: ");
    }

    /// Values starting with a comma are quoted.
    #[test]
    fn comma_values_quoted() {
        assert_eq!(quote_comma_value(" CarClassShortName: ,x,y"), " CarClassShortName: \",x,y\"");
        assert_eq!(quote_comma_value(" Plain: value"), " Plain: value");
        assert_eq!(quote_comma_value(" X: ,"), " X: \",\"");
    }

    /// cp1252 decoding with the undefined bytes translated to spaces.
    #[test]
    fn cp1252_decoding() {
        assert_eq!(clean_source(b"A: \x80\x81\xe9", "WeekendInfo").unwrap(), "A: \u{20AC} \u{e9}");
    }
}
