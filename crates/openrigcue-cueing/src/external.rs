//! The user's mixing-table override CSVs, read once at start-up.
//!
//! The files are `motion_direction.csv`, `motion_deadzone.csv` and `motion_percentages.csv` in
//! the documents folder. If none exists the built-in tables stay. If any exists,
//! each table comes from its file (or, for a missing file, from a 7-column default) and
//! replaces the "enhanced g-forces" off table of mapping ids 2, 3 and 6 ([`MotionConfig::load`]).
//!
//! Each file is comma-separated text in the Windows ANSI code page cp1252, with line endings kept
//! inside quoted fields: the first column of every row is dropped (which fails on an empty
//! row), the first row is skipped, deadzone and percentage cells are parsed as integers, and a table of
//! exactly 6 rows is padded to 12 with `'0'` / `0` / `100` rows. Other row counts are kept as read.
//!
//! [`MotionConfig::load`]: crate::MotionConfig::load

use std::path::{Path, PathBuf};

use crate::axis::Axis;
use crate::platform::{Sign, SlotMix};
use crate::pyint::{PyInt, TableInt};

/// The override file names.
pub const DIRECTION_FILE: &str = "motion_direction.csv";
pub const DEADZONE_FILE: &str = "motion_deadzone.csv";
pub const PERCENTAGES_FILE: &str = "motion_percentages.csv";

/// Why the external tables cannot be loaded. Each case makes motion start-up fail.
#[derive(Debug, thiserror::Error)]
pub enum ExternalMixingError {
    #[error("{file}: {source}")]
    Io {
        file: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// A decoding error: a byte cp1252 does not define.
    #[error("{file}: byte {byte:#04x} at offset {offset} is not valid cp1252")]
    Decode { file: PathBuf, byte: u8, offset: usize },
    /// A CSV syntax error.
    #[error("{file}: {message}")]
    Csv { file: PathBuf, message: &'static str },
    /// An empty row (a blank line) has no first column to drop: an index error.
    #[error("{file}: line {line} is empty (pop from empty list)")]
    EmptyRow { file: PathBuf, line: usize },
    /// A cell is not an integer: a value error.
    #[error("{file}: invalid literal for int() with base 10: {cell:?}")]
    Int { file: PathBuf, cell: String },
    /// A cell has more than 4300 digits: a value error.
    #[error("{file}: Exceeds the limit (4300 digits) for integer string conversion: value has {digits} digits")]
    TooManyDigits { file: PathBuf, digits: usize },
}

/// The three tables as loaded: rows of direction strings and of integers, in file
/// column order (direction and percentages pitch-first, deadzone in the effects-list order roll, pitch, ...).
#[derive(Debug, Clone, PartialEq)]
pub struct ExternalMixing {
    pub direction: Vec<Vec<String>>,
    pub deadzone: Vec<Vec<PyInt>>,
    pub percentages: Vec<Vec<PyInt>>,
}

/// The defaults for a file that is absent (12 rows of 7 columns).
const DEFAULT_DIRECTION: [&str; 6] =
    ["- - - 0 - - 0", "- - + - + - 0", "+ 0 - - + - -", "+ - - 0 + + 0", "+ - + - - + 0", "- 0 - - - + -"];
const DEFAULT_DEADZONE: [[i64; 7]; 6] = [
    [0, 0, 0, 0, 0, 0, 0],
    [30, 0, 0, 0, 50, 0, 0],
    [0, 0, 0, 50, 0, 0, 0],
    [0, 0, 0, 0, 0, 0, 0],
    [30, 0, 0, 0, 50, 0, 0],
    [0, 0, 0, 50, 0, 0, 0],
];
const DEFAULT_PERCENTAGES: [[i64; 7]; 6] = [
    [100, 100, 100, 100, 100, 100, 0],
    [100, 100, 100, 100, 100, 100, 0],
    [100; 7],
    [100, 100, 100, 100, 100, 100, 0],
    [100, 100, 100, 100, 100, 100, 0],
    [100; 7],
];
/// Rows a 6-row table is padded with, and the defaults' rows 6 to 11.
const FILLER_ROWS: usize = 6;

fn default_direction() -> Vec<Vec<String>> {
    let mut rows: Vec<Vec<String>> =
        DEFAULT_DIRECTION.iter().map(|r| r.split(' ').map(str::to_owned).collect()).collect();
    rows.extend((0..FILLER_ROWS).map(|_| vec!["0".to_owned(); 7]));
    rows
}

fn default_numbers(head: &[[i64; 7]; 6], filler: i64) -> Vec<Vec<PyInt>> {
    let mut rows: Vec<Vec<PyInt>> = head.iter().map(|r| r.iter().map(|&v| PyInt::from(v)).collect()).collect();
    rows.extend((0..FILLER_ROWS).map(|_| vec![PyInt::from(filler); 7]));
    rows
}

impl ExternalMixing {
    /// Loads the override tables from the documents folder `dir`: `Ok(None)` when
    /// none of the three files exists.
    pub fn load_dir(dir: &Path) -> Result<Option<Self>, ExternalMixingError> {
        let read = |name: &str| -> Result<Option<(PathBuf, Vec<u8>)>, ExternalMixingError> {
            let file = dir.join(name);
            if !file.is_file() {
                return Ok(None);
            }
            std::fs::read(&file)
                .map(|bytes| Some((file.clone(), bytes)))
                .map_err(|source| ExternalMixingError::Io { file, source })
        };
        let direction = read(DIRECTION_FILE)?;
        let deadzone = read(DEADZONE_FILE)?;
        let percentages = read(PERCENTAGES_FILE)?;
        fn view(f: &Option<(PathBuf, Vec<u8>)>) -> Option<(&Path, &[u8])> {
            f.as_ref().map(|(p, b)| (p.as_path(), b.as_slice()))
        }
        Self::from_files(view(&direction), view(&deadzone), view(&percentages))
    }

    /// The tables from the files' bytes (`None`: that file does not exist), in the order
    /// (direction, deadzone, percentages). `Ok(None)` when no file exists.
    pub fn from_files(
        direction: Option<(&Path, &[u8])>,
        deadzone: Option<(&Path, &[u8])>,
        percentages: Option<(&Path, &[u8])>,
    ) -> Result<Option<Self>, ExternalMixingError> {
        if direction.is_none() && deadzone.is_none() && percentages.is_none() {
            return Ok(None);
        }
        let direction = match direction {
            Some((file, bytes)) => pad(data_rows(file, bytes)?, || vec!["0".to_owned(); 7]),
            None => default_direction(),
        };
        let deadzone = match deadzone {
            Some((file, bytes)) => pad(int_rows(file, bytes)?, || vec![PyInt::from(0); 7]),
            None => default_numbers(&DEFAULT_DEADZONE, 0),
        };
        let percentages = match percentages {
            Some((file, bytes)) => pad(int_rows(file, bytes)?, || vec![PyInt::from(100); 7]),
            None => default_numbers(&DEFAULT_PERCENTAGES, 100),
        };
        Ok(Some(ExternalMixing { direction, deadzone, percentages }))
    }

    /// The number of rows of the longest table (rows beyond the 12 motors are never read).
    pub(crate) fn rows(&self) -> usize {
        self.direction.len().max(self.deadzone.len()).max(self.percentages.len())
    }

    /// The rows of `slot` as the mixing stage reads them: at most the first 8 cells of each table's
    /// row, an empty row where the table has none (an index error where the mixing stage reads
    /// the first missing cell), and `None` for a direction other than `+`, `-`, `0` (which yields no
    /// signal, so the signal sum fails with a type error). The deadzone row stays in the file's column
    /// order, which is the effects-list order it is indexed with.
    pub(crate) fn slot(&self, slot: usize) -> SlotMix {
        fn row<T>(table: &[Vec<T>], slot: usize) -> &[T] {
            table.get(slot).map_or(&[], |r| &r[..r.len().min(Axis::COUNT)])
        }
        SlotMix {
            direction: row(&self.direction, slot).iter().map(|cell| Sign::from_cell(cell)).collect(),
            percentages: row(&self.percentages, slot).iter().map(TableInt::new).collect(),
            deadzone: row(&self.deadzone, slot).iter().map(TableInt::new).collect(),
        }
    }
}

/// Pads a table of exactly 6 rows with 6 filler rows.
fn pad<T>(mut rows: Vec<Vec<T>>, filler: impl Fn() -> Vec<T>) -> Vec<Vec<T>> {
    if rows.len() == FILLER_ROWS {
        rows.extend((0..FILLER_ROWS).map(|_| filler()));
    }
    rows
}

/// The rows without their first column and without the first row.
fn data_rows(file: &Path, bytes: &[u8]) -> Result<Vec<Vec<String>>, ExternalMixingError> {
    let text = decode_cp1252(file, bytes)?;
    let records = csv_records(&text).map_err(|message| ExternalMixingError::Csv { file: file.to_owned(), message })?;
    let mut rows = Vec::new();
    for (index, mut record) in records.into_iter().enumerate() {
        if record.is_empty() {
            return Err(ExternalMixingError::EmptyRow { file: file.to_owned(), line: index + 1 });
        }
        record.remove(0);
        if index > 0 {
            rows.push(record);
        }
    }
    Ok(rows)
}

/// Every cell of every data row as an integer.
fn int_rows(file: &Path, bytes: &[u8]) -> Result<Vec<Vec<PyInt>>, ExternalMixingError> {
    data_rows(file, bytes)?
        .into_iter()
        .map(|row| row.into_iter().map(|cell| python_int(file, cell)).collect())
        .collect()
}

/// The most digits an integer cell may have.
const MAX_STR_DIGITS: usize = 4300;

/// A base-10 integer cell: Unicode whitespace around the number, an optional sign, ASCII digits
/// with single underscores between them, at most 4300 digits (underscores not counted).
fn python_int(file: &Path, cell: String) -> Result<PyInt, ExternalMixingError> {
    let invalid = || ExternalMixingError::Int { file: file.to_owned(), cell: cell.clone() };
    // Non-ASCII whitespace becomes ' ' (no non-ASCII decimal digit exists in cp1252); then
    // ASCII whitespace is stripped from both ends.
    let ascii: String = cell
        .chars()
        .map(|c| {
            if (c as u32) < 127 {
                c
            } else if c.is_whitespace() {
                ' '
            } else {
                '?'
            }
        })
        .collect();
    let trimmed = ascii.trim_matches(|c: char| matches!(c, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r'));
    let (negative, digits) = match trimmed.as_bytes().first() {
        Some(b'-') => (true, &trimmed[1..]),
        Some(b'+') => (false, &trimmed[1..]),
        _ => (false, trimmed),
    };
    let bytes = digits.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_digit() || !bytes[bytes.len() - 1].is_ascii_digit() {
        return Err(invalid());
    }
    let mut previous_underscore = false;
    for &b in bytes {
        if b == b'_' {
            if previous_underscore {
                return Err(invalid());
            }
            previous_underscore = true;
            continue;
        }
        if !b.is_ascii_digit() {
            return Err(invalid());
        }
        previous_underscore = false;
    }
    let digits = bytes.iter().copied().filter(u8::is_ascii_digit);
    let count = digits.clone().count();
    if count > MAX_STR_DIGITS {
        return Err(ExternalMixingError::TooManyDigits { file: file.to_owned(), digits: count });
    }
    Ok(PyInt::from_decimal(negative, digits))
}

/// cp1252 bytes 0x80..0x9F; `None` where the code page defines no character (a decoding error).
const CP1252_HIGH: [Option<char>; 32] = [
    Some('\u{20AC}'),
    None,
    Some('\u{201A}'),
    Some('\u{0192}'),
    Some('\u{201E}'),
    Some('\u{2026}'),
    Some('\u{2020}'),
    Some('\u{2021}'),
    Some('\u{02C6}'),
    Some('\u{2030}'),
    Some('\u{0160}'),
    Some('\u{2039}'),
    Some('\u{0152}'),
    None,
    Some('\u{017D}'),
    None,
    None,
    Some('\u{2018}'),
    Some('\u{2019}'),
    Some('\u{201C}'),
    Some('\u{201D}'),
    Some('\u{2022}'),
    Some('\u{2013}'),
    Some('\u{2014}'),
    Some('\u{02DC}'),
    Some('\u{2122}'),
    Some('\u{0161}'),
    Some('\u{203A}'),
    Some('\u{0153}'),
    None,
    Some('\u{017E}'),
    Some('\u{0178}'),
];

fn decode_cp1252(file: &Path, bytes: &[u8]) -> Result<String, ExternalMixingError> {
    bytes
        .iter()
        .enumerate()
        .map(|(offset, &byte)| match byte {
            0x80..=0x9F => CP1252_HIGH[usize::from(byte - 0x80)].ok_or_else(|| ExternalMixingError::Decode {
                file: file.to_owned(),
                byte,
                offset,
            }),
            _ => Ok(char::from(byte)),
        })
        .collect()
}

/// The lines of the text without newline translation: ended by `\n`, `\r` or `\r\n`, endings kept.
fn lines(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                out.push(&text[start..=i]);
                start = i + 1;
            }
            b'\r' => {
                let end = if bytes.get(i + 1) == Some(&b'\n') { i + 1 } else { i };
                out.push(&text[start..=end]);
                start = end + 1;
                i = end;
            }
            _ => {}
        }
        i += 1;
    }
    if start < bytes.len() {
        out.push(&text[start..]);
    }
    out
}

/// CSV reader states (delimiter `,`, quote character `"`, a doubled quote inside a quoted field
/// is one quote, no escape character, no initial-space skipping, lenient about stray quotes).
#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    StartRecord,
    StartField,
    InField,
    InQuotedField,
    QuoteInQuotedField,
    EatCrnl,
}

/// The longest field the reader accepts.
const FIELD_LIMIT: usize = 131_072;

struct Parser {
    state: State,
    fields: Vec<String>,
    field: String,
    field_len: usize,
}

impl Parser {
    fn save_field(&mut self) {
        self.fields.push(std::mem::take(&mut self.field));
        self.field_len = 0;
    }

    fn add_char(&mut self, c: char) -> Result<(), &'static str> {
        if self.field_len >= FIELD_LIMIT {
            return Err("field larger than field limit (131072)");
        }
        self.field.push(c);
        self.field_len += 1;
        Ok(())
    }

    /// Advances the reader by one character; `None` is the end-of-line marker.
    fn process(&mut self, c: Option<char>) -> Result<(), &'static str> {
        let eol = c.is_none();
        let crnl = matches!(c, Some('\n' | '\r'));
        if self.state == State::StartRecord {
            if eol {
                return Ok(());
            }
            if crnl {
                self.state = State::EatCrnl;
                return Ok(());
            }
            self.state = State::StartField;
        }
        match self.state {
            State::StartRecord => unreachable!("handled above"),
            State::StartField => {
                if crnl || eol {
                    self.save_field();
                    self.state = if eol { State::StartRecord } else { State::EatCrnl };
                } else if c == Some('"') {
                    self.state = State::InQuotedField;
                } else if c == Some(',') {
                    self.save_field();
                } else if let Some(c) = c {
                    self.add_char(c)?;
                    self.state = State::InField;
                }
            }
            State::InField => {
                if crnl || eol {
                    self.save_field();
                    self.state = if eol { State::StartRecord } else { State::EatCrnl };
                } else if c == Some(',') {
                    self.save_field();
                    self.state = State::StartField;
                } else if let Some(c) = c {
                    self.add_char(c)?;
                }
            }
            State::InQuotedField => match c {
                None => {}
                Some('"') => self.state = State::QuoteInQuotedField,
                Some(c) => self.add_char(c)?,
            },
            State::QuoteInQuotedField => {
                if c == Some('"') {
                    self.add_char('"')?;
                    self.state = State::InQuotedField;
                } else if c == Some(',') {
                    self.save_field();
                    self.state = State::StartField;
                } else if crnl || eol {
                    self.save_field();
                    self.state = if eol { State::StartRecord } else { State::EatCrnl };
                } else if let Some(c) = c {
                    self.add_char(c)?;
                    self.state = State::InField;
                }
            }
            State::EatCrnl => {
                if eol {
                    self.state = State::StartRecord;
                } else if !crnl {
                    return Err(
                        "new-line character seen in unquoted field - do you need to open the file with newline=''?",
                    );
                }
            }
        }
        Ok(())
    }
}

/// Every record of the comma-separated text.
fn csv_records(text: &str) -> Result<Vec<Vec<String>>, &'static str> {
    let mut records = Vec::new();
    let mut lines = lines(text).into_iter();
    loop {
        let mut p = Parser { state: State::StartRecord, fields: Vec::new(), field: String::new(), field_len: 0 };
        loop {
            let Some(line) = lines.next() else {
                if p.field_len != 0 || p.state == State::InQuotedField {
                    p.save_field();
                    records.push(p.fields);
                }
                return Ok(records);
            };
            for c in line.chars() {
                p.process(Some(c))?;
            }
            p.process(None)?;
            if p.state == State::StartRecord {
                break;
            }
        }
        records.push(p.fields);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path() -> &'static Path {
        Path::new("motion.csv")
    }

    fn ints(values: &[i64]) -> Vec<PyInt> {
        values.iter().map(|&v| PyInt::from(v)).collect()
    }

    #[test]
    fn csv_reader_follows_the_excel_dialect() {
        let rows = csv_records("a,b\r\n\"x,\"\"y\"\"\",z\n\n1,\"multi\nline\"\r").unwrap();
        assert_eq!(
            rows,
            vec![
                vec!["a".to_owned(), "b".to_owned()],
                vec!["x,\"y\"".to_owned(), "z".to_owned()],
                vec![],
                vec!["1".to_owned(), "multi\nline".to_owned()],
            ]
        );
        // Quote inside an unquoted field is kept; a stray character after a closing quote joins the field.
        assert_eq!(csv_records("a\"b,\"c\"d").unwrap(), vec![vec!["a\"b".to_owned(), "cd".to_owned()]]);
        // Unterminated quoted field at end of input: the field is saved.
        assert_eq!(csv_records("\"open").unwrap(), vec![vec!["open".to_owned()]]);
        assert_eq!(csv_records("x,").unwrap(), vec![vec!["x".to_owned(), String::new()]]);
    }

    #[test]
    fn python_int_accepts_what_int_accepts() {
        let ok = |s: &str| python_int(path(), s.to_owned()).unwrap();
        assert_eq!(ok(" 30 "), PyInt::from(30));
        assert_eq!(ok("+5"), PyInt::from(5));
        assert_eq!(ok("-007"), PyInt::from(-7));
        assert_eq!(ok("1_000"), PyInt::from(1000));
        assert_eq!(ok("\u{a0}50\t"), PyInt::from(50));
        assert_eq!(ok("-0"), PyInt::from(0));
        for bad in ["", " ", "1.5", "_1", "1_", "1__0", "0x10", "+-1", "1 0"] {
            assert!(matches!(python_int(path(), bad.to_owned()), Err(ExternalMixingError::Int { .. })), "{bad:?}");
        }
        // Cells are unbounded integers: 2^64 + 1 loads exactly.
        assert_eq!(ok("18446744073709551617"), PyInt::from_decimal(false, *b"18446744073709551617"));
        // The 4300-digit limit counts leading zeros, not underscores; an invalid literal is
        // reported first.
        let long = "1".repeat(4300);
        assert_eq!(ok(&format!("{}_{}", &long[..10], &long[10..])), PyInt::from_decimal(false, long.bytes()));
        let too_long = "0".repeat(4301);
        assert!(matches!(
            python_int(path(), too_long.clone()),
            Err(ExternalMixingError::TooManyDigits { digits: 4301, .. })
        ));
        assert!(matches!(python_int(path(), format!("{too_long}x")), Err(ExternalMixingError::Int { .. })));
        assert!(matches!(python_int(path(), format!(" {too_long} ")), Err(ExternalMixingError::TooManyDigits { .. })));
    }

    #[test]
    fn cp1252_decoding_rejects_undefined_bytes() {
        assert_eq!(decode_cp1252(path(), b"a\x80\xa0").unwrap(), "a\u{20ac}\u{a0}");
        assert!(matches!(
            decode_cp1252(path(), b"a\x81"),
            Err(ExternalMixingError::Decode { byte: 0x81, offset: 1, .. })
        ));
    }

    #[test]
    fn no_file_keeps_the_builtin_tables() {
        assert_eq!(ExternalMixing::from_files(None, None, None).unwrap(), None);
    }

    #[test]
    fn missing_files_use_the_seven_column_defaults() {
        let dz = b"motor,roll,pitch,yaw,sway,surge,heave,tl,tlf\n0,1,2,3,4,5,6,7,8\n";
        let ext = ExternalMixing::from_files(None, Some((path(), dz)), None).unwrap().unwrap();
        assert_eq!(ext.direction.len(), 12);
        assert_eq!(ext.direction[1], ["-", "-", "+", "-", "+", "-", "0"]);
        assert_eq!(ext.percentages[0], ints(&[100, 100, 100, 100, 100, 100, 0]));
        // One data row: not padded.
        assert_eq!(ext.deadzone, vec![ints(&[1, 2, 3, 4, 5, 6, 7, 8])]);
        // Seven-column direction and percentage rows (column 7 is missing); deadzone rows only for slot 0.
        let s0 = ext.slot(0);
        assert_eq!((s0.direction.len(), s0.percentages.len(), s0.deadzone.len()), (7, 7, 8));
        assert!(ext.slot(1).deadzone.is_empty());
    }

    #[test]
    fn six_rows_are_padded_and_slots_read_like_calculate_motor_power() {
        let header = "m,p,r,y,sw,su,h,tl,tlf\n";
        let dir = format!("{header}{}", (0..6).map(|i| format!("{i},+,-,0,+,-,+,0,-\n")).collect::<String>());
        let dz = format!("{header}{}", (0..6).map(|i| format!("{i},30,0,0,0,50,0,0,0\n")).collect::<String>());
        let pc = format!("{header}{}", (0..6).map(|i| format!("{i},100,90,80,70,60,50,40,30\n")).collect::<String>());
        let ext = ExternalMixing::from_files(
            Some((path(), dir.as_bytes())),
            Some((path(), dz.as_bytes())),
            Some((path(), pc.as_bytes())),
        )
        .unwrap()
        .unwrap();
        assert_eq!(ext.direction.len(), 12);
        let s0 = ext.slot(0);
        assert_eq!(s0.direction_of(Axis::Roll), Some(Sign::Minus));
        assert_eq!(s0.direction_of(Axis::TractionLossFront), Some(Sign::Minus));
        assert_eq!(s0.percentages[Axis::Roll.index()], TableInt::from(90));
        // Deadzone column 0 is roll.
        assert_eq!(s0.deadzone_of(Axis::Roll), Some(&TableInt::from(30)));
        assert_eq!(s0.deadzone_of(Axis::Pitch), Some(&TableInt::from(0)));
        // The padding rows have 7 columns.
        assert_eq!(ext.slot(6).percentages.len(), 7);
        // An unknown direction yields no signal.
        let bad = format!("{header}0,+,-,0,+,-,+,0,x\n");
        let ext = ExternalMixing::from_files(
            Some((path(), bad.as_bytes())),
            Some((path(), dz.as_bytes())),
            Some((path(), pc.as_bytes())),
        )
        .unwrap()
        .unwrap();
        assert_eq!(ext.slot(0).direction[7], None);
        assert_eq!(ext.slot(0).direction[6], Some(Sign::Zero));
    }

    #[test]
    fn a_blank_line_fails_like_row_pop() {
        let err = ExternalMixing::from_files(None, Some((path(), b"h\n0,1\n\n")), None).unwrap_err();
        assert!(matches!(err, ExternalMixingError::EmptyRow { line: 3, .. }));
        let err = ExternalMixing::from_files(None, Some((path(), b"h\n0,a\n")), None).unwrap_err();
        assert!(matches!(err, ExternalMixingError::Int { .. }));
    }

    #[test]
    fn load_dir_reads_the_documents_folder() {
        let dir = std::env::temp_dir().join(format!("openrigcue-cueing-ext-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(ExternalMixing::load_dir(&dir).unwrap().is_none());
        std::fs::write(dir.join(PERCENTAGES_FILE), "h\n0,1,2,3,4,5,6,7,8\n").unwrap();
        let ext = ExternalMixing::load_dir(&dir).unwrap().unwrap();
        assert_eq!(ext.percentages[0][7], PyInt::from(8));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
