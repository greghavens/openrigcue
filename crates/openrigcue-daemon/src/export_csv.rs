//! The continuous telemetry CSV log behind the Telemetry page "Export" switch: while a game delivers telemetry
//! and the switch is on, one row of the telemetry table every 50 ms in `<documents>/export/`.
//! The log is written by its own thread.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use chrono::NaiveDateTime;
use openrigcue_model::native_value::LegacyValue;
use openrigcue_model::python_compare::native_equal;
use openrigcue_model::python_unicode::{is_alphanumeric, str_is_alphanumeric};
use openrigcue_sources::derive::{ExportRow, EXPORT_HEADER};

use crate::state::ServiceState;
use crate::telemetry::TelemetryHandle;

/// Pause after a written row: 20 rows per second at most.
pub const ROW_PERIOD: Duration = Duration::from_millis(50);
/// Pause while the switch is off or no listener runs.
pub const IDLE_PERIOD: Duration = Duration::from_secs(1);

/// The folder the log files go to: `export` in the documents folder.
pub fn export_dir(documents: &Path) -> PathBuf {
    documents.join("export")
}

/// What one pass of the export loop reads from the running service.
#[derive(Debug, Clone, Copy)]
pub struct ExportInput<'a> {
    /// Whether a telemetry listener runs.
    pub listener_running: bool,
    /// Whether the export switch is on.
    pub switch_on: bool,
    /// The id of the running game.
    pub game: &'a str,
    /// The vehicle name, `None` when there is none.
    pub vehicle: Option<&'a str>,
    /// The telemetry export table without its header row.
    pub rows: &'a [ExportRow],
}

/// What a pass did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pass {
    /// A row was appended; the loop pauses [`ROW_PERIOD`].
    Wrote,
    /// Nothing to log; the loop pauses [`IDLE_PERIOD`].
    Idle,
}

/// The export loop's state: the switch value of the previous pass and the current file name.
#[derive(Debug)]
pub struct ExportLog {
    dir: PathBuf,
    /// `None` at start and after every idle pass, so switching on again (or a new game) starts a new file.
    previous_switch: Option<bool>,
    file: Option<PathBuf>,
}

impl ExportLog {
    pub fn new(dir: PathBuf) -> ExportLog {
        ExportLog { dir, previous_switch: None, file: None }
    }

    /// The file rows are currently appended to.
    pub fn file(&self) -> Option<&Path> {
        self.file.as_deref()
    }

    /// One iteration of the export loop, without its pause.
    ///
    /// With a listener running and the switch on, a switch value different from the previous pass names a new
    /// file `export_<game>[_<alphanumerics of the vehicle>]_<%Y%m%d%H%M%S>.csv` (time of `now`). The file is opened
    /// for appending; when it is empty the header `Timestamp, <label>…` is written first, then the row
    /// `<%Y-%m-%d-%H:%M:%S>, <value>…`. The switch is remembered only after a successful write, so a failed
    /// pass names the file again next time. Otherwise the pass is idle and forgets the switch.
    pub fn pass(&mut self, input: &ExportInput<'_>, now: NaiveDateTime) -> std::io::Result<Pass> {
        let vehicle = input.vehicle.map_or(LegacyValue::Int(0), |name| LegacyValue::Str(name.to_owned()));
        self.pass_native(input, &vehicle, now)
    }

    /// [`ExportLog::pass`] for a dynamically typed vehicle value. The file name is built only for a new
    /// file. Failures keep the assigned game prefix and leave the previous-switch state unchanged.
    pub fn pass_native(
        &mut self,
        input: &ExportInput<'_>,
        vehicle: &LegacyValue,
        now: NaiveDateTime,
    ) -> std::io::Result<Pass> {
        if !(input.listener_running && input.switch_on) {
            self.previous_switch = None;
            return Ok(Pass::Idle);
        }
        if self.previous_switch != Some(input.switch_on) {
            let mut name = format!("export_{}", input.game);
            self.file = Some(self.dir.join(&name));
            if !native_equal(vehicle, &LegacyValue::Int(0)) {
                let segment = native_vehicle_segment(vehicle)?;
                name.push('_');
                name.push_str(&segment);
            }
            name.push_str(&format!("_{}.csv", now.format("%Y%m%d%H%M%S")));
            self.file = Some(self.dir.join(name));
        }
        let path = self.file.as_deref().expect("a remembered switch implies a named file");
        let mut file = OpenOptions::new().append(true).create(true).open(path)?;
        let mut text = String::new();
        if file.metadata()?.len() == 0 {
            let header = std::iter::once("Timestamp".to_owned())
                .chain(input.rows.iter().map(|row| row.label.to_owned()).filter(|label| label != EXPORT_HEADER[0]));
            text.push_str(&csv_record(header));
        }
        let values = std::iter::once(now.format("%Y-%m-%d-%H:%M:%S").to_string())
            .chain(input.rows.iter().map(|row| row.value.clone()).filter(|value| value != EXPORT_HEADER[1]));
        text.push_str(&csv_record(values));
        file.write_all(text.as_bytes())?;
        self.previous_switch = Some(input.switch_on);
        Ok(Pass::Wrote)
    }
}

/// The alphanumeric items of the vehicle value joined into one text; a value that cannot be iterated,
/// or an item that is not text, is an error.
fn native_vehicle_segment(vehicle: &LegacyValue) -> std::io::Result<String> {
    fn error(text: String) -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, text)
    }
    fn select(selected: &mut Vec<LegacyValue>, item: &LegacyValue) -> std::io::Result<()> {
        match item {
            LegacyValue::Str(text) => {
                if str_is_alphanumeric(text) {
                    selected.push(item.clone());
                }
                Ok(())
            }
            LegacyValue::Bytes(bytes) if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_alphanumeric) => Ok(()),
            LegacyValue::Bytes(_) => {
                selected.push(item.clone());
                Ok(())
            }
            value => Err(error(format!("'{}' object has no attribute 'isalnum'", value.type_name()))),
        }
    }
    let mut selected = Vec::new();
    match vehicle {
        LegacyValue::Str(text) => return Ok(text.chars().filter(|value| is_alphanumeric(*value)).collect()),
        LegacyValue::List(values) | LegacyValue::Tuple(values) | LegacyValue::Set(values) => {
            for value in values {
                select(&mut selected, value)?;
            }
        }
        LegacyValue::Dict(values) => {
            for key in values.keys() {
                let value = key.to_value();
                select(&mut selected, &value)?;
            }
        }
        LegacyValue::Bytes(bytes) if bytes.is_empty() => {}
        LegacyValue::Bytes(_) => return Err(error("'int' object has no attribute 'isalnum'".into())),
        value => return Err(error(format!("'{}' object is not iterable", value.type_name()))),
    }
    let mut segment = String::new();
    for (index, value) in selected.iter().enumerate() {
        if let LegacyValue::Str(text) = value {
            segment.push_str(text);
        } else {
            return Err(error(format!("sequence item {index}: expected str instance, bytes found")));
        }
    }
    Ok(segment)
}

/// The log file name: `export_<game>`; with a vehicle, `_` and the vehicle name's alphanumeric
/// characters; then always `_<%Y%m%d%H%M%S>.csv`. `None` stands for no vehicle.
pub fn file_name(game: &str, vehicle: Option<&str>, now: NaiveDateTime) -> String {
    let mut name = format!("export_{game}");
    if let Some(vehicle) = vehicle {
        name.push('_');
        name.extend(vehicle.chars().filter(|value| is_alphanumeric(*value)));
    }
    format!("{name}_{}.csv", now.format("%Y%m%d%H%M%S"))
}

/// A float table cell as text: six decimals, or `nan`, `inf`, `-inf`.
pub fn format_value(value: f64) -> String {
    if value.is_nan() {
        "nan".into()
    } else if value.is_infinite() {
        if value > 0.0 {
            "inf".into()
        } else {
            "-inf".into()
        }
    } else {
        format!("{value:.6}")
    }
}

/// One CSV record with minimal quoting: fields joined by `,`, a field
/// holding `,`, `"`, CR or LF quoted with its quotes doubled, terminated by CR LF.
pub fn csv_record(fields: impl IntoIterator<Item = String>) -> String {
    let mut line = String::new();
    for (i, field) in fields.into_iter().enumerate() {
        if i > 0 {
            line.push(',');
        }
        if field.contains([',', '"', '\r', '\n']) {
            line.push('"');
            line.push_str(&field.replace('"', "\"\""));
            line.push('"');
        } else {
            line.push_str(&field);
        }
    }
    line.push_str("\r\n");
    line
}

/// The running export thread.
pub struct ExportThread {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

impl ExportThread {
    /// Creates `<documents>/export` if it is missing and starts the export loop.
    pub fn start(state: ServiceState, telemetry: TelemetryHandle, documents: &Path) -> std::io::Result<ExportThread> {
        let dir = export_dir(documents);
        if !dir.is_dir() {
            std::fs::create_dir_all(&dir)?;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = std::thread::Builder::new().name("threadExport".into()).spawn(move || {
            let mut log = ExportLog::new(dir);
            while !flag.load(Ordering::SeqCst) {
                let pause = match export_pass(&state, &telemetry, &mut log) {
                    Ok(pass) => {
                        if pass == Pass::Wrote {
                            ROW_PERIOD
                        } else {
                            IDLE_PERIOD
                        }
                    }
                    Err(error) => {
                        println!("error while exporting csv {error}");
                        continue;
                    }
                };
                std::thread::sleep(pause);
            }
        })?;
        Ok(ExportThread { stop, handle })
    }

    pub fn stop(self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.handle.join();
    }
}

/// One pass on the service's current session, switch and telemetry table.
fn export_pass(state: &ServiceState, telemetry: &TelemetryHandle, log: &mut ExportLog) -> std::io::Result<Pass> {
    let session = state.session.snapshot();
    let switch_on = state.controls.read().telemetry.export;
    let listener_running = session.listener.is_some();
    let rows: Vec<ExportRow> =
        if listener_running && switch_on { telemetry.export_rows_snapshot() } else { Vec::new() };
    let input = ExportInput {
        listener_running,
        switch_on,
        game: session.game_key(),
        vehicle: session.vehicle.as_deref(),
        rows: &rows,
    };
    log.pass_native(&input, &session.vehicle_value(), chrono::Local::now().naive_local())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn at(h: u32, m: u32, s: u32, ms: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 3, 7).unwrap().and_hms_milli_opt(h, m, s, ms).unwrap()
    }

    fn rows() -> Vec<ExportRow> {
        vec![
            ExportRow {
                label: "Speed",
                value: "36.000000".into(),
                min: "0.000000".into(),
                max: "40.000000".into(),
                direction_positive: None,
            },
            ExportRow { label: "Gear", value: "3".into(), min: "-1".into(), max: "5".into(), direction_positive: None },
            ExportRow {
                label: "Roll",
                value: "-0.125000".into(),
                min: "0.000000".into(),
                max: "0.000000".into(),
                direction_positive: None,
            },
        ]
    }

    fn input<'a>(rows: &'a [ExportRow], switch_on: bool) -> ExportInput<'a> {
        ExportInput {
            listener_running: true,
            switch_on,
            game: "dirtrally2.0",
            vehicle: Some("Ford Fiesta R5 MKII"),
            rows,
        }
    }

    #[test]
    fn export_pass_keeps_raw_vehicle_iteration_and_failed_name_prefix() {
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_model::native_value::LegacyValue;
        let tmp = tempfile::tempdir().unwrap();
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let telemetry = TelemetryHandle::new(openrigcue_sources::derive::Dispatcher::new(Vec::new()));
        state.controls.write().telemetry.export = true;
        {
            let mut session = state.session.write();
            session.game = Some("ac".into());
            session.listener = Some("ac".into());
            session.set_vehicle_value(LegacyValue::None, Some("None".into()));
        }
        let mut log = ExportLog::new(tmp.path().to_owned());
        assert!(export_pass(&state, &telemetry, &mut log).is_err(), "None is not iterable");
        assert_eq!(log.file(), Some(tmp.path().join("export_ac").as_path()));
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
        let value = LegacyValue::List(vec!["A B".into(), "XY".into(), "7".into()]);
        state.session.write().set_vehicle_value(value, Some("display projection".into()));
        assert_eq!(export_pass(&state, &telemetry, &mut log).unwrap(), Pass::Wrote);
        assert!(log.file().unwrap().file_name().unwrap().to_str().unwrap().starts_with("export_ac_XY7_"));
        let path = log.file().unwrap().to_owned();
        state.session.write().set_vehicle_value(LegacyValue::None, Some("None".into()));
        assert_eq!(export_pass(&state, &telemetry, &mut log).unwrap(), Pass::Wrote, "name is kept while enabled");
        assert_eq!(log.file(), Some(path.as_path()));
    }

    #[test]
    fn continuous_export_does_not_wait_for_the_dispatcher_mutex() {
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        let tmp = tempfile::tempdir().unwrap();
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let telemetry = TelemetryHandle::new(openrigcue_sources::derive::Dispatcher::new(Vec::new()));
        state.controls.write().telemetry.export = true;
        {
            let mut session = state.session.write();
            session.game = Some("api".into());
            session.listener = Some("api".into());
        }
        let mut log = ExportLog::new(tmp.path().to_owned());
        let locked = telemetry.lock();
        let (done, completed) = std::sync::mpsc::channel();
        let (independent, result) = std::thread::scope(|scope| {
            let reader = scope.spawn(|| {
                let result = export_pass(&state, &telemetry, &mut log);
                done.send(()).unwrap();
                result
            });
            let independent = completed.recv_timeout(std::time::Duration::from_secs(1));
            drop(locked);
            (independent, reader.join().unwrap())
        });
        assert!(independent.is_ok(), "continuous export waited for the dispatcher mutex");
        assert_eq!(result.unwrap(), Pass::Wrote);
        assert!(std::fs::read_to_string(log.file().unwrap()).unwrap().starts_with("Timestamp\r\n"));
    }

    #[test]
    fn native_vehicle_filter_materializes_before_join_and_does_not_coerce_items() {
        assert_eq!(native_vehicle_segment(&"A B-7".into()).unwrap(), "AB7");
        assert_eq!(native_vehicle_segment(&LegacyValue::Bytes(Vec::new())).unwrap(), "");
        let mut keys = openrigcue_model::native_value::Dict::new();
        keys.insert("A B", LegacyValue::Int(7));
        keys.insert("XY", LegacyValue::None);
        assert_eq!(native_vehicle_segment(&LegacyValue::Dict(keys)).unwrap(), "XY");
        let bytes = LegacyValue::Bytes(b"abc".to_vec());
        let joined = LegacyValue::Tuple(vec!["XY".into(), bytes.clone()]);
        assert_eq!(
            native_vehicle_segment(&joined).unwrap_err().to_string(),
            "sequence item 1: expected str instance, bytes found"
        );
        let late_bad_item = LegacyValue::List(vec![bytes, LegacyValue::Int(7)]);
        assert_eq!(
            native_vehicle_segment(&late_bad_item).unwrap_err().to_string(),
            "'int' object has no attribute 'isalnum'"
        );
        let skipped_bytes = LegacyValue::List(vec![LegacyValue::Bytes(b"a b".to_vec()), "XY".into()]);
        assert_eq!(native_vehicle_segment(&skipped_bytes).unwrap(), "XY");
        assert!(native_vehicle_segment(&LegacyValue::Bytes(b"abc".to_vec())).is_err());
    }

    /// The file name is `export_<game>[_<alnum(vehicle)>]_<%Y%m%d%H%M%S>.csv`; the vehicle part is
    /// left out only for vehicle 0 (`None`), an empty vehicle still adds its `_`.
    #[test]
    fn file_names() {
        let now = at(9, 5, 3, 0);
        assert_eq!(
            file_name("dirtrally2.0", Some("Ford Fiesta R5 MKII"), now),
            "export_dirtrally2.0_FordFiestaR5MKII_20260307090503.csv"
        );
        assert_eq!(
            file_name("acc", Some("Porsche 911 (992) GT3-R"), now),
            "export_acc_Porsche911992GT3R_20260307090503.csv"
        );
        assert_eq!(file_name("acc", None, now), "export_acc_20260307090503.csv");
        assert_eq!(file_name("acc", Some(""), now), "export_acc__20260307090503.csv");
    }

    #[test]
    fn file_name_uses_the_original_unicode_character_classes() {
        assert_eq!(
            file_name("ac", Some("A\u{0345}\u{1c89}\u{00bd}\u{00b2}\u{2163}\u{2ebf0}"), at(9, 5, 3, 0)),
            "export_ac_A\u{00bd}\u{00b2}\u{2163}\u{2ebf0}_20260307090503.csv"
        );
    }

    /// The header `Timestamp` + labels is written once (empty file), then one row per pass with the
    /// second-resolution timestamp and the formatted cells; records end with CR LF.
    /// The file name is `export_<game>_<alnum(vehicle)>_<timestamp>.csv`.
    #[test]
    fn header_once_then_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = ExportLog::new(tmp.path().to_path_buf());
        let rows = rows();
        assert_eq!(log.pass(&input(&rows, true), at(9, 5, 3, 10)).unwrap(), Pass::Wrote);
        assert_eq!(log.pass(&input(&rows, true), at(9, 5, 3, 60)).unwrap(), Pass::Wrote);
        let path = log.file().unwrap().to_path_buf();
        assert_eq!(path.file_name().unwrap(), "export_dirtrally2.0_FordFiestaR5MKII_20260307090503.csv");
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            "Timestamp,Speed,Gear,Roll\r\n2026-03-07-09:05:03,36.000000,3,-0.125000\r\n2026-03-07-09:05:03,36.000000,3,-0.125000\r\n"
        );
    }

    /// Switch off or no listener → idle, and the previous switch value is forgotten,
    /// so the next pass with the switch on starts a new file; the switch staying on keeps
    /// the file.
    #[test]
    fn new_file_after_idle() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = ExportLog::new(tmp.path().to_path_buf());
        let rows = rows();
        log.pass(&input(&rows, true), at(9, 0, 0, 0)).unwrap();
        log.pass(&input(&rows, true), at(9, 0, 1, 0)).unwrap();
        let first = log.file().unwrap().to_path_buf();
        assert_eq!(log.pass(&input(&rows, false), at(9, 0, 2, 0)).unwrap(), Pass::Idle);
        log.pass(&input(&rows, true), at(9, 0, 3, 0)).unwrap();
        let second = log.file().unwrap().to_path_buf();
        assert_ne!(first, second);
        assert_eq!(std::fs::read_to_string(&first).unwrap().lines().count(), 3);
        let mut stopped = input(&rows, true);
        stopped.listener_running = false;
        assert_eq!(log.pass(&stopped, at(9, 0, 4, 0)).unwrap(), Pass::Idle);
        log.pass(&input(&rows, true), at(9, 0, 5, 0)).unwrap();
        assert!(log.file().unwrap().ends_with("export_dirtrally2.0_FordFiestaR5MKII_20260307090005.csv"));
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 3);
    }

    /// A failed open is reported and the switch is not remembered, so the next pass
    /// names the file again (new timestamp).
    #[test]
    fn failed_pass_renames() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = ExportLog::new(tmp.path().join("missing"));
        let rows = rows();
        assert!(log.pass(&input(&rows, true), at(9, 0, 0, 0)).is_err());
        std::fs::create_dir(tmp.path().join("missing")).unwrap();
        log.pass(&input(&rows, true), at(9, 0, 7, 0)).unwrap();
        assert!(log.file().unwrap().ends_with("export_dirtrally2.0_FordFiestaR5MKII_20260307090007.csv"));
    }

    /// An existing non-empty file (same second) gets no second header.
    #[test]
    fn appends_without_header_to_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let rows = rows();
        let mut first = ExportLog::new(tmp.path().to_path_buf());
        first.pass(&input(&rows, true), at(9, 0, 0, 0)).unwrap();
        let mut second = ExportLog::new(tmp.path().to_path_buf());
        second.pass(&input(&rows, true), at(9, 0, 0, 500)).unwrap();
        let text = std::fs::read_to_string(first.file().unwrap()).unwrap();
        assert_eq!(text.matches("Timestamp").count(), 1);
        assert_eq!(text.lines().count(), 3);
    }

    /// Float cells have six decimals; records use minimal quoting.
    #[test]
    fn cell_formatting() {
        assert_eq!(format_value(0.0), "0.000000");
        assert_eq!(format_value(-0.0), "-0.000000");
        assert_eq!(format_value(1234.5678915), "1234.567892");
        assert_eq!(format_value(f64::NAN), "nan");
        assert_eq!(format_value(f64::NEG_INFINITY), "-inf");
        assert_eq!(
            csv_record(["a".to_owned(), "b,c".to_owned(), "say \"hi\"".to_owned(), String::new()]),
            "a,\"b,c\",\"say \"\"hi\"\"\",\r\n"
        );
    }
}
