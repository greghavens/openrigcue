//! The pointer chains of the community "Elite Dangerous Horizons & Odyssey plugin – XML edition" offsets file:
//! reading the archive, parsing the XML and mapping its element names to forces.

use std::io::{Cursor, Read};

use crate::process_memory::python::int_from_str;

/// The game builds the offsets file distinguishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EdBuild {
    Horizons,
    Odyssey,
}

/// The forces read from the game.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Force {
    Speed,
    Sway,
    Heave,
    Surge,
    Pitch,
    Roll,
    Yaw,
    SrvSpeed,
    SrvRoll,
    SrvPitch,
    SrvSurge,
    SrvSway,
    SrvYaw,
    SrvHeave,
}

impl Force {
    /// XML element name → force.
    fn from_tag(tag: &str) -> Option<Force> {
        Some(match tag {
            "Speed" => Force::Speed,
            "Sway" => Force::Sway,
            "Heave" => Force::Heave,
            "Surge" => Force::Surge,
            "Pitch" => Force::Pitch,
            "Roll" => Force::Roll,
            "Yaw" => Force::Yaw,
            "SRV_Speed" => Force::SrvSpeed,
            "SRV_Roll" => Force::SrvRoll,
            "SRV_Pitch" => Force::SrvPitch,
            "SRV_Surge" => Force::SrvSurge,
            "SRV_Sway" => Force::SrvSway,
            "SRV_Yaw" => Force::SrvYaw,
            "SRV_Heave" => Force::SrvHeave,
            _ => return None,
        })
    }
}

/// One force and its pointer chain (offsets from the module base).
pub type ForceChain = (Force, Vec<u64>);

/// Pointer chain of every force, per build, in file order. Empty when nothing is loaded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Offsets {
    builds: Vec<(EdBuild, Vec<ForceChain>)>,
}

impl Offsets {
    pub fn is_empty(&self) -> bool {
        self.builds.is_empty()
    }

    pub(super) fn apply(&mut self, change: OffsetChange) {
        match change {
            OffsetChange::Reset => self.builds.clear(),
            OffsetChange::Build(build, chains) => match self.builds.iter_mut().find(|(key, _)| *key == build) {
                Some(entry) => entry.1 = chains,
                None => self.builds.push((build, chains)),
            },
        }
    }

    pub(super) fn chain(&self, build: EdBuild, force: Force) -> Option<&[u64]> {
        self.chains(build)?.iter().find(|(key, _)| *key == force).map(|(_, chain)| chain.as_slice())
    }

    /// The chains of `build`; `None` when the file had no such section list (the caller treats that as an error).
    pub fn chains(&self, build: EdBuild) -> Option<&[ForceChain]> {
        self.builds.iter().find(|(b, _)| *b == build).map(|(_, chains)| chains.as_slice())
    }
}

/// Loads the pointer chains: for `Horizons` then `Odyssey`, every child element of every such section of the
/// root gives `Offsets` chain entries `Offset_0 .. Offset_{n-1}` (hexadecimal). Any unexpected content
/// (unknown element, missing or malformed attribute) makes the whole result empty.
pub fn parse_offsets_xml(xml: &str) -> Offsets {
    parse_offsets_xml_with(xml, |_| {})
}

#[derive(Debug, Clone)]
pub(super) enum OffsetChange {
    Build(EdBuild, Vec<ForceChain>),
    Reset,
}

pub(super) fn parse_offsets_xml_with(xml: &str, mut publish: impl FnMut(OffsetChange)) -> Offsets {
    let Ok(document) = roxmltree::Document::parse(xml) else {
        crate::printed::line("failed to load ed offsets");
        publish(OffsetChange::Reset);
        return Offsets::default();
    };
    let root = document.root_element();
    let mut builds = Vec::new();
    for (tag, build) in [("Horizons", EdBuild::Horizons), ("Odyssey", EdBuild::Odyssey)] {
        let mut chains: Vec<ForceChain> = Vec::new();
        for section in root.children().filter(|n| n.is_element() && element_tag(n) == tag) {
            for child in section.children().filter(roxmltree::Node::is_element) {
                let Some((force, chain)) = parse_force(&child) else {
                    crate::printed::line("failed to load ed offsets");
                    publish(OffsetChange::Reset);
                    return Offsets::default();
                };
                match chains.iter_mut().find(|(f, _)| *f == force) {
                    Some(entry) => entry.1 = chain,
                    None => chains.push((force, chain)),
                }
            }
        }
        // One publication per build, once its sections are read into the local chain list;
        // a reader may see the first build without the second.
        publish(OffsetChange::Build(build, chains.clone()));
        builds.push((build, chains));
    }
    Offsets { builds }
}

/// The element's tag, written `{namespace}name` for namespaced elements.
fn element_tag(node: &roxmltree::Node) -> String {
    match node.tag_name().namespace() {
        Some(ns) => format!("{{{ns}}}{}", node.tag_name().name()),
        None => node.tag_name().name().to_owned(),
    }
}

fn parse_force(element: &roxmltree::Node) -> Option<(Force, Vec<u64>)> {
    let count = int_from_str(element.attribute("Offsets")?)?;
    let chain = (0..count.max(0))
        .map(|i| element.attribute(format!("Offset_{i}").as_str()).and_then(hex_offset))
        .collect::<Option<Vec<u64>>>()?;
    let force = Force::from_tag(&element_tag(element))?;
    Some((force, chain))
}

/// A hexadecimal offset: keep the low 64 bits used for pointer arithmetic. Hex input has no
/// digit limit; Unicode decimal digits and trailing numeric whitespace are accepted.
fn hex_offset(text: &str) -> Option<u64> {
    let normalized = openrigcue_model::python_numeric::decimal_ascii(text);
    // The separators 0x1c..0x1f are not accepted as whitespace here.
    let body = normalized.trim_end_matches(char::is_whitespace);
    let body = body.strip_prefix('_').unwrap_or(body);
    let mut value = 0_u64;
    let mut previous_digit = false;
    for c in body.chars() {
        if c == '_' && previous_digit {
            previous_digit = false;
        } else {
            let digit = c.to_digit(16)?;
            value = value.wrapping_mul(16).wrapping_add(u64::from(digit));
            previous_digit = true;
        }
    }
    previous_digit.then_some(value)
}

/// Why the downloaded archive could not be read.
#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("offsets archive unreadable: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("offsets archive member unreadable: {0}")]
    Io(#[from] std::io::Error),
    #[error("offsets XML is neither UTF-8 nor UTF-16")]
    Encoding,
    /// The XML is not well-formed; this is reported before any semantic check of its content.
    #[error("offsets XML unreadable: {0}")]
    Xml(#[from] roxmltree::Error),
}

/// Reads the archive: the first member (archive order) whose name ends with `.xml`, as text. `Ok(None)`
/// when there is none.
pub fn xml_from_archive(archive: &[u8]) -> Result<Option<String>, ArchiveError> {
    let mut zip = zip::ZipArchive::new(Cursor::new(archive))?;
    for index in 0..zip.len() {
        let mut member = zip.by_index(index)?;
        if !member.name().ends_with(".xml") {
            continue;
        }
        let mut bytes = Vec::new();
        member.read_to_end(&mut bytes)?;
        let xml = decode_xml(&bytes).ok_or(ArchiveError::Encoding)?;
        // The XML is parsed here. Syntax errors
        // are returned from here; pointer/attribute conversion errors are handled when the chains are loaded.
        roxmltree::Document::parse(&xml)?;
        return Ok(Some(xml));
    }
    Ok(None)
}

/// XML bytes as text: UTF-8 (optionally with BOM) or UTF-16 with BOM.
fn decode_xml(bytes: &[u8]) -> Option<String> {
    let utf16 = |body: &[u8], big_endian: bool| {
        let units = body.as_chunks::<2>().0.iter().map(|c| {
            if big_endian {
                u16::from_be_bytes([c[0], c[1]])
            } else {
                u16::from_le_bytes([c[0], c[1]])
            }
        });
        char::decode_utf16(units).collect::<Result<String, _>>().ok()
    };
    match bytes {
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8(rest.to_vec()).ok(),
        [0xFF, 0xFE, rest @ ..] => utf16(rest, false),
        [0xFE, 0xFF, rest @ ..] => utf16(rest, true),
        _ => String::from_utf8(bytes.to_vec()).ok(),
    }
}

/// Every `attachments/elitedangerous64_offsets-zip.<number>/` link of a page (ASCII digits).
pub fn attachment_links(page: &str) -> Vec<&str> {
    const PREFIX: &str = "attachments/elitedangerous64_offsets-zip.";
    let mut links = Vec::new();
    let mut from = 0;
    while let Some(found) = page[from..].find(PREFIX) {
        let start = from + found;
        let digits_start = start + PREFIX.len();
        let digits = page[digits_start..].bytes().take_while(u8::is_ascii_digit).count();
        let end = digits_start + digits;
        if digits > 0 && page[end..].starts_with('/') {
            links.push(&page[start..=end]);
            from = end + 1;
        } else {
            from = start + 1;
        }
    }
    links
}

/// The newest attachment of a page (`max` by attachment number, the first of equals), as an absolute URL.
pub fn newest_attachment(page: &str) -> Option<String> {
    let number = |link: &str| {
        link.rsplit('.').next().map(|n| n.trim_matches('/')).and_then(|n| n.parse::<u128>().ok()).unwrap_or(0)
    };
    let newest = attachment_links(page)
        .into_iter()
        .reduce(|best, link| if number(link) > number(best) { link } else { best })?;
    Some(format!("https://www.xsimulator.net/community/{newest}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = r#"<?xml version="1.0"?>
<Root>
  <Horizons>
    <Speed Offsets="3" Offset_0="05A0B8C0" Offset_1="10" Offset_2="1A4"/>
    <Roll Offsets="1" Offset_0="ff "/>
  </Horizons>
  <Odyssey>
    <SRV_Speed Offsets="2" Offset_0="_1f" Offset_1="8"/>
  </Odyssey>
</Root>"#;

    /// Per-build chains from Offset_0.., hexadecimal.
    #[test]
    fn load_pointers_chains() {
        let offsets = parse_offsets_xml(XML);
        let horizons = offsets.chains(EdBuild::Horizons).unwrap();
        assert_eq!(horizons[0], (Force::Speed, vec![0x05A0_B8C0, 0x10, 0x1A4]));
        assert_eq!(horizons[1], (Force::Roll, vec![0xFF]));
        assert_eq!(offsets.chains(EdBuild::Odyssey).unwrap()[0], (Force::SrvSpeed, vec![0x1F, 8]));
    }

    /// An unknown element, a missing Offset_i or a "0x"-prefixed value fails the whole load.
    #[test]
    fn load_pointers_failures() {
        assert!(parse_offsets_xml(&XML.replace("Roll", "Wobble")).is_empty());
        assert!(parse_offsets_xml(&XML.replace(r#"Offsets="3""#, r#"Offsets="4""#)).is_empty());
        assert!(parse_offsets_xml(&XML.replace("05A0B8C0", "0x05A0B8C0")).is_empty());
        crate::printed::take();
        assert!(parse_offsets_xml("<broken").is_empty());
        assert_eq!(crate::printed::take(), ["failed to load ed offsets"], "print('failed to load ed offsets')");
        // No sections at all still loads both builds, empty.
        let none = parse_offsets_xml("<Root/>");
        assert!(!none.is_empty());
        assert_eq!(none.chains(EdBuild::Horizons), Some(&[][..]));
    }

    /// The attachment with the highest number wins, as an absolute URL.
    #[test]
    fn newest_attachment_link() {
        let page = r#"<a href="attachments/elitedangerous64_offsets-zip.9/">x</a>
            attachments/elitedangerous64_offsets-zip.123/ attachments/elitedangerous64_offsets-zip.45/
            attachments/elitedangerous64_offsets-zip./ attachments/elitedangerous64_offsets-zip.7x"#;
        assert_eq!(attachment_links(page).len(), 3);
        assert_eq!(
            newest_attachment(page).as_deref(),
            Some("https://www.xsimulator.net/community/attachments/elitedangerous64_offsets-zip.123/")
        );
        assert_eq!(newest_attachment("nothing"), None);
    }

    /// The first `.xml` member in archive order is read.
    #[test]
    fn xml_member_of_archive() {
        let mut bytes = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(Cursor::new(&mut bytes));
            let options = zip::write::SimpleFileOptions::default();
            writer.start_file("readme.txt", options).unwrap();
            std::io::Write::write_all(&mut writer, b"hello").unwrap();
            writer.start_file("EliteDangerous64.xml", options).unwrap();
            std::io::Write::write_all(&mut writer, XML.as_bytes()).unwrap();
            writer.finish().unwrap();
        }
        assert_eq!(xml_from_archive(&bytes).unwrap().as_deref(), Some(XML));
        assert!(xml_from_archive(b"not a zip").is_err());
    }

    #[test]
    fn native_first_xml_parse_failure_is_not_skipped_for_a_later_member() {
        let mut bytes = Vec::new();
        let mut writer = zip::ZipWriter::new(Cursor::new(&mut bytes));
        for (name, data) in [("skip.XML", "<Root/>"), ("first.xml", "<Root>"), ("valid.xml", XML)] {
            writer.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
            std::io::Write::write_all(&mut writer, data.as_bytes()).unwrap();
        }
        writer.finish().unwrap();
        assert!(xml_from_archive(&bytes).is_err(), "parse only the first case-sensitive .xml member");
    }

    /// An `Offset_<i>` attribute is converted as hexadecimal text behind an implied `0x` prefix
    /// (one underscore may follow the prefix, trailing whitespace only).
    #[test]
    fn hex_offsets_follow_python_int() {
        assert_eq!(hex_offset("1_A"), Some(0x1A));
        assert_eq!(hex_offset("_1A"), Some(0x1A));
        assert_eq!(hex_offset(" 1A"), None);
        assert_eq!(hex_offset("1A\t"), Some(0x1A));
        assert_eq!(hex_offset("1__A"), None);
        assert_eq!(hex_offset(""), None);
    }

    #[test]
    fn hex_offsets_keep_pointer_low_bits_and_unicode_digit_grammar() {
        assert_eq!(hex_offset("10000000000000001"), Some(1));
        assert_eq!(hex_offset("1_FFFFFFFFFFFFFFFF"), Some(u64::MAX));
        assert_eq!(hex_offset("１２a"), Some(0x12a));
        assert_eq!(hex_offset("_١_a\u{a0}"), Some(0x1a));
        assert_eq!(hex_offset(&format!("{}2a", "0".repeat(4400))), Some(0x2a));
        for invalid in ["1\u{1c}", "1\u{1f}", "+1", "-1", "１__２", "Ａ", "_", "1_", " 1"] {
            assert_eq!(hex_offset(invalid), None, "{invalid:?}");
        }
    }
}
