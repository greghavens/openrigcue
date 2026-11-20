//! Read-only extraction of files from an NSIS installer (the SRS setup executable).
//!
//! Supports what the SRS installer uses: NSIS 3, non-solid LZMA blocks, ANSI or Unicode strings.
//! Nothing in the installer is executed.

use std::collections::HashMap;
use std::io::Cursor;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum NsisError {
    #[error("no NSIS header found")]
    NotNsis,
    #[error("truncated or malformed NSIS data at offset {0}")]
    Malformed(usize),
    #[error("LZMA decoding failed: {0}")]
    Lzma(String),
    #[error("file not in the installer: {0}")]
    Missing(String),
}

const SIGNATURE: u32 = 0xDEAD_BEEF;
const MAGIC: &[u8; 12] = b"NullsoftInst";
const FIRST_HEADER_LEN: usize = 28;
const ENTRY_LEN: usize = 28;
const OP_CREATE_DIR: u32 = 11;
const OP_EXTRACT_FILE: u32 = 20;
const OP_ASSIGN_VAR: u32 = 25;
/// `$INSTDIR` and `$OUTDIR` in the NSIS variable table.
const VAR_INSTDIR: u32 = 21;
const VAR_OUTDIR: u32 = 22;

/// One file the installer extracts.
struct Entry {
    /// Install-relative path as the installer spells it.
    path: String,
    /// Offset of the file's data block after the header.
    offset: usize,
}

/// An opened installer: its bytes and the file table, parsed once.
pub struct NsisArchive {
    bytes: Vec<u8>,
    data_start: usize,
    /// Keyed by the lower-cased path (Windows paths are case-insensitive).
    files: HashMap<String, Entry>,
}

impl NsisArchive {
    /// Locate the NSIS header and list the files the installer extracts.
    pub fn open(bytes: Vec<u8>) -> Result<NsisArchive, NsisError> {
        let start = find_first_header(&bytes).ok_or(NsisError::NotNsis)?;
        let header_len = read_u32(&bytes, start + 20)? as usize;
        let block_at = start + FIRST_HEADER_LEN;
        let (header, used) = read_block(&bytes, block_at, Some(header_len))?;
        let data_start = block_at + used;
        let files = list_files(&header)?;
        Ok(NsisArchive { bytes, data_start, files })
    }

    /// Install-relative paths (`/`-separated, no leading slash) of every extracted file.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.files.values().map(|e| e.path.as_str())
    }

    /// Content of the file installed at `path` (`/`-separated, case-insensitive).
    pub fn read(&self, path: &str) -> Result<Vec<u8>, NsisError> {
        let entry = self.files.get(&path.to_ascii_lowercase()).ok_or_else(|| NsisError::Missing(path.to_string()))?;
        Ok(read_block(&self.bytes, self.data_start + entry.offset, None)?.0)
    }
}

fn read_u32(b: &[u8], at: usize) -> Result<u32, NsisError> {
    b.get(at..at + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]])).ok_or(NsisError::Malformed(at))
}

fn find_first_header(b: &[u8]) -> Option<usize> {
    (0..b.len().saturating_sub(FIRST_HEADER_LEN))
        .step_by(512)
        .find(|&at| read_u32(b, at + 4).ok() == Some(SIGNATURE) && &b[at + 8..at + 20] == MAGIC)
}

/// One length-prefixed data block; returns its content and the bytes it occupied.
fn read_block(b: &[u8], at: usize, unpacked: Option<usize>) -> Result<(Vec<u8>, usize), NsisError> {
    let word = read_u32(b, at)?;
    let len = (word & 0x7FFF_FFFF) as usize;
    let body = b.get(at + 4..at + 4 + len).ok_or(NsisError::Malformed(at))?;
    let content = if word & 0x8000_0000 != 0 { inflate_lzma(body, unpacked)? } else { body.to_vec() };
    Ok((content, 4 + len))
}

/// NSIS LZMA: 5 property bytes followed by the raw stream (no size field).
fn inflate_lzma(body: &[u8], unpacked: Option<usize>) -> Result<Vec<u8>, NsisError> {
    let options = lzma_rs::decompress::Options {
        unpacked_size: lzma_rs::decompress::UnpackedSize::UseProvided(unpacked.map(|n| n as u64)),
        memlimit: None,
        allow_incomplete: unpacked.is_none(),
    };
    let mut out = Vec::new();
    lzma_rs::lzma_decompress_with_options(&mut Cursor::new(body), &mut out, &options)
        .map_err(|e| NsisError::Lzma(e.to_string()))?;
    Ok(out)
}

struct Strings<'h> {
    table: &'h [u8],
    unicode: bool,
}

impl Strings<'_> {
    /// Decode string `index`, expanding the variables known in `vars` (`$INSTDIR` is "");
    /// `None` when another variable or a language/shell code appears.
    fn get(&self, index: u32, vars: &HashMap<u32, String>) -> Option<String> {
        let mut units = Vec::new();
        if self.unicode {
            let mut at = index as usize * 2;
            while let Some(s) = self.table.get(at..at + 2) {
                let u = u16::from_le_bytes([s[0], s[1]]);
                if u == 0 {
                    break;
                }
                units.push(u32::from(u));
                at += 2;
            }
        } else {
            units.extend(self.table.get(index as usize..)?.iter().take_while(|&&c| c != 0).map(|&c| u32::from(c)));
        }
        self.expand(&units, vars)
    }

    fn expand(&self, units: &[u32], vars: &HashMap<u32, String>) -> Option<String> {
        let (lang, shell, var, skip) = if self.unicode { (1, 2, 3, 4) } else { (255, 254, 253, 252) };
        let mut out = String::new();
        let mut i = 0;
        while i < units.len() {
            let c = units[i];
            if c == skip {
                out.push(char::from_u32(*units.get(i + 1)?)?);
                i += 2;
            } else if c == var {
                out.push_str(vars.get(&self.code_value(units, i + 1)?)?);
                i += if self.unicode { 2 } else { 3 };
            } else if c == lang || c == shell {
                return None;
            } else {
                out.push(char::from_u32(c)?);
                i += 1;
            }
        }
        Some(out)
    }

    fn code_value(&self, units: &[u32], at: usize) -> Option<u32> {
        if self.unicode {
            let w = units.get(at)?;
            Some((w & 0x7F) | ((w & 0x7F00) >> 1))
        } else {
            Some(((units.get(at + 1)? & 0x7F) << 7) | (units.get(at)? & 0x7F))
        }
    }
}

/// Walk the install script: `SetOutPath` (create-dir with the out-path flag) sets `$OUTDIR`,
/// plain `StrCpy` assignments set variables, extract-file entries name files and their data
/// offsets. Paths are relative to `$INSTDIR`; the first entry for a path (in any letter case) wins.
fn list_files(header: &[u8]) -> Result<HashMap<String, Entry>, NsisError> {
    let block = |n: usize| -> Result<(usize, usize), NsisError> {
        Ok((read_u32(header, 4 + n * 8)? as usize, read_u32(header, 8 + n * 8)? as usize))
    };
    let (entries_at, entry_count) = block(2)?;
    let (strings_at, _) = block(3)?;
    let (langs_at, _) = block(4)?;
    let table = header.get(strings_at..langs_at.max(strings_at)).ok_or(NsisError::Malformed(strings_at))?;
    let unicode = table.len() >= 4 && table[0] == 0 && table[1] == 0;
    let strings = Strings { table, unicode };
    let mut vars = HashMap::from([(VAR_INSTDIR, String::new()), (VAR_OUTDIR, String::new())]);
    let mut files = HashMap::new();
    for k in 0..entry_count {
        let at = entries_at + k * ENTRY_LEN;
        let op = read_u32(header, at)?;
        let parm = |n: usize| read_u32(header, at + 4 + n * 4);
        match op {
            OP_CREATE_DIR if parm(1)? != 0 => {
                if let Some(dir) = strings.get(parm(0)?, &vars) {
                    vars.insert(VAR_OUTDIR, normalize(&dir));
                }
            }
            OP_ASSIGN_VAR if parm(2)? == 0 && parm(3)? == 0 => {
                if let Some(value) = strings.get(parm(1)?, &vars) {
                    vars.insert(parm(0)?, normalize(&value));
                } else {
                    vars.remove(&parm(0)?);
                }
            }
            OP_EXTRACT_FILE => {
                if let Some(name) = strings.get(parm(1)?, &vars) {
                    let outdir = &vars[&VAR_OUTDIR];
                    let name = normalize(&name);
                    let path = if name.starts_with(outdir.as_str()) || outdir.is_empty() {
                        name
                    } else {
                        format!("{outdir}/{name}")
                    };
                    let offset = parm(2)? as usize;
                    files.entry(path.to_ascii_lowercase()).or_insert(Entry { path, offset });
                }
            }
            _ => {}
        }
    }
    Ok(files)
}

fn normalize(path: &str) -> String {
    path.replace('\\', "/").trim_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_nsis() {
        assert!(matches!(NsisArchive::open(vec![0u8; 4096]), Err(NsisError::NotNsis)));
    }

    #[test]
    fn unicode_string_expansion() {
        // VAR_CODE, $INSTDIR (21, encoded as 0x8095), "\sounds"
        let mut table = vec![0u8, 0];
        for u in [3u16, 0x8095].into_iter().chain("\\sounds".encode_utf16()).chain([0]) {
            table.extend(u.to_le_bytes());
        }
        let s = Strings { table: &table, unicode: true };
        let vars = HashMap::from([(VAR_INSTDIR, String::new())]);
        assert_eq!(s.get(1, &vars).as_deref(), Some("\\sounds"));
        assert_eq!(s.get(1, &HashMap::new()), None);
    }

    /// NSIS LZMA block body: 5 property bytes and the raw stream, no size field.
    fn lzma(data: &[u8]) -> Vec<u8> {
        let options =
            lzma_rs::compress::Options { unpacked_size: lzma_rs::compress::UnpackedSize::SkipWritingToHeader };
        let mut out = Vec::new();
        lzma_rs::lzma_compress_with_options(&mut Cursor::new(data), &mut out, &options).unwrap();
        out
    }

    fn block(body: &[u8], compressed: bool) -> Vec<u8> {
        let flag = if compressed { 0x8000_0000 } else { 0 };
        let mut out = (body.len() as u32 | flag).to_le_bytes().to_vec();
        out.extend(body);
        out
    }

    /// Unicode string table; returns the table and the index (in UTF-16 units) of each string.
    /// `$INSTDIR` is written as VAR_CODE + 0x8095 and `$0` as VAR_CODE + 0x8080.
    fn string_table(strings: &[&str]) -> (Vec<u8>, Vec<u32>) {
        let mut units: Vec<u16> = vec![0];
        let mut index = Vec::new();
        for s in strings {
            index.push(units.len() as u32);
            let mut rest = *s;
            while !rest.is_empty() {
                if let Some(r) = rest.strip_prefix("$INSTDIR") {
                    units.extend([3, 0x8095]);
                    rest = r;
                } else if let Some(r) = rest.strip_prefix("$0") {
                    units.extend([3, 0x8080]);
                    rest = r;
                } else {
                    let c = rest.chars().next().unwrap();
                    let mut buf = [0u16; 2];
                    units.extend_from_slice(c.encode_utf16(&mut buf));
                    rest = &rest[c.len_utf8()..];
                }
            }
            units.push(0);
        }
        (units.iter().flat_map(|u| u.to_le_bytes()).collect(), index)
    }

    fn entry(op: u32, parms: [u32; 6]) -> Vec<u8> {
        std::iter::once(op).chain(parms).flat_map(u32::to_le_bytes).collect()
    }

    /// A minimal NSIS 3 image: a 512-byte stub, the first header, an LZMA header block
    /// (entries + string table) and two data blocks, one LZMA-compressed and one stored.
    fn synthetic_installer(ogg: &[u8], wav: &[u8]) -> Vec<u8> {
        let (strings, idx) =
            string_table(&["$INSTDIR\\sounds", "$0\\suspension", "6.ogg", "$INSTDIR\\sounds\\transmission", "1.wav"]);
        let ogg_block = block(&lzma(ogg), true);
        let wav_block = block(wav, false);
        let mut entries = Vec::new();
        entries.extend(entry(OP_ASSIGN_VAR, [0, idx[0], 0, 0, 0, 0]));
        entries.extend(entry(OP_CREATE_DIR, [idx[1], 1, 0, 0, 0, 0]));
        entries.extend(entry(OP_EXTRACT_FILE, [0, idx[2], 0, 0, 0, 0]));
        entries.extend(entry(OP_CREATE_DIR, [idx[3], 1, 0, 0, 0, 0]));
        let wav_at = ogg_block.len() as u32;
        entries.extend(entry(OP_EXTRACT_FILE, [0, idx[4], wav_at, 0, 0, 0]));
        // flags, then 8 (offset, count) block headers: pages, sections, entries, strings, langs, ...
        let table_len = 4 + 8 * 8;
        let entries_at = table_len as u32;
        let strings_at = entries_at + entries.len() as u32;
        let langs_at = strings_at + strings.len() as u32;
        let mut header = vec![0u8; 4];
        for (offset, count) in [
            (0, 0u32),
            (0, 0),
            (entries_at, 5),
            (strings_at, 0),
            (langs_at, 0),
            (langs_at, 0),
            (langs_at, 0),
            (langs_at, 0),
        ] {
            header.extend(offset.to_le_bytes());
            header.extend(count.to_le_bytes());
        }
        header.extend(&entries);
        header.extend(&strings);

        let mut image = vec![b'M'; 512];
        let header_block = block(&lzma(&header), true);
        let rest = header_block.len() + ogg_block.len() + wav_block.len();
        image.extend(0u32.to_le_bytes());
        image.extend(SIGNATURE.to_le_bytes());
        image.extend(MAGIC);
        image.extend((header.len() as u32).to_le_bytes());
        image.extend((FIRST_HEADER_LEN as u32 + rest as u32).to_le_bytes());
        image.extend(header_block);
        image.extend(ogg_block);
        image.extend(wav_block);
        image
    }

    #[test]
    fn synthetic_installer_extracts_files() {
        let ogg: Vec<u8> = b"OggS".iter().copied().chain((0..3000u32).map(|i| (i * 7 % 251) as u8)).collect();
        let wav = b"RIFF....WAVEfmt ".to_vec();
        let archive = NsisArchive::open(synthetic_installer(&ogg, &wav)).unwrap();
        let mut paths: Vec<&str> = archive.paths().collect();
        paths.sort_unstable();
        assert_eq!(paths, ["sounds/suspension/6.ogg", "sounds/transmission/1.wav"]);
        assert_eq!(archive.read("sounds/suspension/6.ogg").unwrap(), ogg);
        // Case-insensitive, stored (uncompressed) block.
        assert_eq!(archive.read("Sounds/Transmission/1.WAV").unwrap(), wav);
        assert!(matches!(archive.read("sounds/suspension/5.ogg"), Err(NsisError::Missing(_))));
    }

    /// Reads the user's own installer when `OPENRIGCUE_SRS_INSTALLER` points at it.
    #[test]
    #[ignore = "needs a local SRS installer"]
    fn real_installer_lists_sounds() {
        let path = std::env::var("OPENRIGCUE_SRS_INSTALLER").expect("OPENRIGCUE_SRS_INSTALLER");
        let bytes = std::fs::read(path).unwrap();
        let archive = NsisArchive::open(bytes).unwrap();
        let ogg = archive.read("sounds/suspension/6.ogg").unwrap();
        assert_eq!(&ogg[..4], b"OggS");
    }
}
