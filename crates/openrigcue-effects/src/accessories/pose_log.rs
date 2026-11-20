//! The motion-compensation CSV log kept on six-actuator rigs
//! (`mc_export_<YYYYmmdd_HHMMSS>.csv` in the user's documents folder).

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use chrono::{Local, NaiveDateTime};
use tracing::error;

use super::compensation::RigPose;

/// Header of the log.
pub const COLUMNS: [&str; 16] = [
    "timestamp",
    "game_running",
    "sway",
    "surge",
    "heave",
    "yaw",
    "roll",
    "pitch",
    "m0",
    "m1",
    "m2",
    "m3",
    "m4",
    "m5",
    "estimate_roll_blind_calibrated",
    "estimate_pitch_blind_calibrated",
];

/// One logged pass.
#[derive(Debug, Clone, PartialEq)]
pub struct PoseLogRow {
    /// The running game's key, `None` when there is none
    /// (written as `False`).
    pub game_running: Option<String>,
    /// The exported pose.
    pub pose: RigPose,
    /// Motor positions `m0`..`m5` (511 for a missing one).
    pub motors: [i64; 6],
    /// Roll and pitch estimated from the motor positions.
    pub estimated_roll: f64,
    pub estimated_pitch: f64,
}

/// The log file of one application run.
#[derive(Debug)]
pub struct PoseLog {
    path: PathBuf,
    /// Local wall-clock time of each row.
    now: fn() -> NaiveDateTime,
}

fn local_now() -> NaiveDateTime {
    Local::now().naive_local()
}

/// Creates the parent directories of `path`; an empty directory name is an error.
fn make_parent_dirs(path: &Path) -> io::Result<()> {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => fs::create_dir_all(dir),
        _ => Err(io::Error::new(io::ErrorKind::NotFound, "No such file or directory: ''")),
    }
}

/// CSV line of `fields`: comma separated, fields quoted only when they hold a
/// comma, a quote or a line break, CRLF ending.
fn csv_line<S: AsRef<str>>(fields: &[S]) -> String {
    let quoted: Vec<String> = fields
        .iter()
        .map(|f| {
            let f = f.as_ref();
            if f.contains([',', '"', '\r', '\n']) {
                format!("\"{}\"", f.replace('"', "\"\""))
            } else {
                f.to_string()
            }
        })
        .collect();
    format!("{}\r\n", quoted.join(","))
}

impl PoseLog {
    /// Choose the log path for a run started at `started` and write the header.
    ///
    /// Used only on P6/H6 rigs, which the caller decides. The file is
    /// `mc_export_<YYYYmmdd_HHMMSS>.csv` in the documents folder (`.` without one); the folder is created
    /// and the header written to a new file; a failure
    /// is logged ("Could not create export file: ") and the rows are still attempted later.
    pub fn create(documents: Option<&Path>, started: NaiveDateTime) -> Self {
        let name = format!("mc_export_{}.csv", started.format("%Y%m%d_%H%M%S"));
        let log = PoseLog { path: documents.unwrap_or(Path::new(".")).join(name), now: local_now };
        if let Err(e) = log.write_header() {
            error!("Could not create export file: {e}");
        }
        log
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn write_header(&self) -> io::Result<()> {
        make_parent_dirs(&self.path)?;
        fs::write(&self.path, csv_line(&COLUMNS))
    }

    /// Append one row, with the header first when the file does not exist.
    ///
    /// Values are written in text form (floats with their shortest round-trip digits, `False` for a
    /// missing game).
    pub fn append(&self, row: &PoseLogRow) -> io::Result<()> {
        let exists = self.path.exists();
        make_parent_dirs(&self.path)?;
        let mut file = OpenOptions::new().create(true).append(true).open(&self.path)?;
        let mut text = String::new();
        if !exists {
            text.push_str(&csv_line(&COLUMNS));
        }
        text.push_str(&csv_line(&self.fields(row)));
        file.write_all(text.as_bytes())
    }

    fn fields(&self, row: &PoseLogRow) -> Vec<String> {
        let pose = &row.pose;
        let mut fields = vec![
            (self.now)().format("%Y-%m-%d %H:%M:%S.%6f").to_string(),
            row.game_running.clone().unwrap_or_else(|| "False".to_string()),
        ];
        fields.extend([pose.sway, pose.surge, pose.heave, pose.yaw, pose.roll, pose.pitch].map(python_float_repr));
        fields.extend(row.motors.map(|m| m.to_string()));
        fields.extend([row.estimated_roll, row.estimated_pitch].map(python_float_repr));
        fields
    }
}

/// Float text: the shortest round-trip digits, positional when the decimal exponent is in
/// -4..16 (with a trailing `.0` for whole numbers), otherwise `d.ddde±XX`.
pub fn python_float_repr(v: f64) -> String {
    if v.is_nan() {
        return "nan".to_string();
    }
    if v.is_infinite() {
        return if v > 0.0 { "inf" } else { "-inf" }.to_string();
    }
    let scientific = format!("{v:e}");
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let sign = if mantissa.starts_with('-') { "-" } else { "" };
    let point = exponent + 1;
    let body = if (-3..=16).contains(&point) {
        if point <= 0 {
            format!("0.{}{digits}", "0".repeat(point.unsigned_abs() as usize))
        } else if point as usize >= digits.len() {
            format!("{digits}{}.0", "0".repeat(point as usize - digits.len()))
        } else {
            format!("{}.{}", &digits[..point as usize], &digits[point as usize..])
        }
    } else {
        let fraction = if digits.len() > 1 { format!(".{}", &digits[1..]) } else { String::new() };
        let exp_sign = if exponent < 0 { '-' } else { '+' };
        format!("{}{fraction}e{exp_sign}{:02}", &digits[..1], exponent.unsigned_abs())
    };
    format!("{sign}{body}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Float text as written to the log.
    #[test]
    fn float_repr_matches_python() {
        let cases = [
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (5.0, "5.0"),
            (0.1, "0.1"),
            (-12.345, "-12.345"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (1.5e-7, "1.5e-07"),
            (1234567890123456.0, "1234567890123456.0"),
            (1e16, "1e+16"),
            (1.2345678901234568e17, "1.2345678901234568e+17"),
            (1e100, "1e+100"),
            (f64::NAN, "nan"),
            (f64::NEG_INFINITY, "-inf"),
        ];
        for (v, text) in cases {
            assert_eq!(python_float_repr(v), text, "{v:e}");
        }
    }

    /// CSV lines: minimal quoting, doubled quotes, CRLF.
    #[test]
    fn csv_quoting() {
        assert_eq!(csv_line(&["a", "b,c", "say \"hi\""]), "a,\"b,c\",\"say \"\"hi\"\"\"\r\n");
    }

    fn fixed_now() -> NaiveDateTime {
        NaiveDateTime::parse_from_str("2026-11-09 17:05:03.123456789", "%Y-%m-%d %H:%M:%S%.f").unwrap()
    }

    /// Log setup and appending: header at start, rows appended, header re-created
    /// when the file has gone; an empty documents folder fails the directory creation.
    #[test]
    fn log_file_rows() {
        let dir = std::env::temp_dir().join(format!("openrigcue-pose-log-{}", std::process::id()));
        let started = fixed_now();
        let mut log = PoseLog::create(Some(&dir.join("docs")), started);
        log.now = fixed_now;
        assert_eq!(log.path(), dir.join("docs").join("mc_export_20261109_170503.csv"));
        let header = csv_line(&COLUMNS);
        assert_eq!(fs::read_to_string(log.path()).unwrap(), header);
        let row = PoseLogRow {
            game_running: Some("assetto_corsa".into()),
            pose: RigPose { sway: 0.5, surge: 0.0, heave: -1.25, yaw: 0.0, roll: 1e-05, pitch: 3.0 },
            motors: [511, 600, 511, 511, 511, 511],
            estimated_roll: 0.0,
            estimated_pitch: -0.1,
        };
        log.append(&row).unwrap();
        let expected_row =
            "2026-11-09 17:05:03.123456,assetto_corsa,0.5,0.0,-1.25,0.0,1e-05,3.0,511,600,511,511,511,511,0.0,-0.1\r\n";
        assert_eq!(fs::read_to_string(log.path()).unwrap(), format!("{header}{expected_row}"));
        fs::remove_file(log.path()).unwrap();
        log.append(&PoseLogRow { game_running: None, ..row }).unwrap();
        let text = fs::read_to_string(log.path()).unwrap();
        assert!(text.starts_with(&header) && text.contains(",False,"));
        fs::remove_dir_all(&dir).unwrap();
        let bare = PoseLog::create(Some(Path::new("")), started);
        assert_eq!(
            bare.append(&PoseLogRow { game_running: None, ..row_clone() }).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    fn row_clone() -> PoseLogRow {
        PoseLogRow {
            game_running: None,
            pose: RigPose::default(),
            motors: [511; 6],
            estimated_roll: 0.0,
            estimated_pitch: 0.0,
        }
    }
}
