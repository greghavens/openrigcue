//! The `telemetry_snapshot` hotkey: the Telemetry page's export table written
//! once to `<documents>/export/`.
//!
//! Nothing is written unless a game runs. The file is `snapshot_<game>`, then `_<vehicle>` when there is a
//! vehicle (the name itself, unlike the continuous export which keeps only its alphanumeric characters),
//! then `_<%Y%m%d%H%M%S>.csv`. It holds every row of the telemetry export table, the header row first.
//! A failed write is reported with a fixed error text.

use std::path::{Path, PathBuf};

use chrono::NaiveDateTime;
use openrigcue_model::native_value::LegacyValue;
use openrigcue_model::python_compare::native_equal;
use openrigcue_sources::derive::{ExportRow, EXPORT_HEADER};

use crate::export_csv::csv_record;

/// The error text when the file cannot be written.
pub const SNAPSHOT_ERROR: &str = "Error: the telemetry could not be saved as CSV";

/// The file name: `snapshot_<game>[_<vehicle>]_<%Y%m%d%H%M%S>.csv`; `None` when there is no vehicle.
pub fn file_name(game: &str, vehicle: Option<&str>, now: NaiveDateTime) -> String {
    let mut name = format!("snapshot_{game}");
    if let Some(vehicle) = vehicle {
        name.push('_');
        name.push_str(vehicle);
    }
    format!("{name}_{}.csv", now.format("%Y%m%d%H%M%S"))
}

/// The table as the snapshot writes it: the header row, then one row per channel with formatted cells.
pub fn contents(rows: &[ExportRow]) -> String {
    let mut text = csv_record(EXPORT_HEADER.iter().map(|h| (*h).to_owned()));
    for row in rows {
        text.push_str(&csv_record([row.label.to_owned(), row.value.clone(), row.min.clone(), row.max.clone()]));
    }
    text
}

/// Writes the snapshot; `None` when no game runs (nothing is written). The file name is joined to the
/// folder unchecked: a vehicle name with a path separator names a file in a sub-folder, which fails when that folder
/// does not exist.
pub fn snapshot(
    game: &str,
    vehicle: Option<&str>,
    rows: &[ExportRow],
    export_dir: &Path,
    now: NaiveDateTime,
) -> Option<Result<PathBuf, String>> {
    let vehicle = vehicle.map_or(LegacyValue::Int(0), |name| LegacyValue::Str(name.to_owned()));
    snapshot_native(game, &vehicle, rows, export_dir, now)
}

/// [`snapshot`] for a dynamically typed vehicle value: a vehicle that is not text is the fixed error.
pub fn snapshot_native(
    game: &str,
    vehicle: &LegacyValue,
    rows: &[ExportRow],
    export_dir: &Path,
    now: NaiveDateTime,
) -> Option<Result<PathBuf, String>> {
    if game == "none" {
        return None;
    }
    let vehicle = if native_equal(vehicle, &LegacyValue::Int(0)) {
        None
    } else if let LegacyValue::Str(name) = vehicle {
        Some(name.as_str())
    } else {
        return Some(Err(SNAPSHOT_ERROR.to_owned()));
    };
    let path = export_dir.join(file_name(game, vehicle, now));
    Some(std::fs::write(&path, contents(rows)).map(|()| path).map_err(|error| {
        tracing::warn!(%error, "telemetry snapshot failed");
        SNAPSHOT_ERROR.to_owned()
    }))
}

/// Takes a snapshot of the service's telemetry: the written path, the error text, or `None` when no
/// game runs.
pub fn take(ctx: &crate::api::ApiContext) -> Option<Result<PathBuf, String>> {
    let session = ctx.state.session.snapshot();
    let rows = ctx.telemetry.export_rows_snapshot();
    let now = chrono::Local::now().naive_local();
    snapshot_native(session.game_key(), &session.vehicle_value(), &rows, ctx.hotkeys.export_dir(), now)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at() -> NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2026, 11, 10).unwrap().and_hms_opt(9, 8, 7).unwrap()
    }

    #[test]
    fn names() {
        assert_eq!(
            file_name("ac", Some("Ferrari 488 (GT3)"), at()),
            "snapshot_ac_Ferrari 488 (GT3)_20261110090807.csv"
        );
        assert_eq!(file_name("ac", None, at()), "snapshot_ac_20261110090807.csv");
        assert_eq!(file_name("ac", Some(""), at()), "snapshot_ac__20261110090807.csv");
    }

    #[test]
    fn native_zero_kinds_skip_the_vehicle_and_other_nontext_values_fail() {
        let dir = tempfile::tempdir().unwrap();
        for vehicle in [LegacyValue::Int(0), LegacyValue::Bool(false), LegacyValue::Float(-0.0)] {
            let path = snapshot_native("ac", &vehicle, &[], dir.path(), at()).unwrap().unwrap();
            assert_eq!(path.file_name().unwrap(), "snapshot_ac_20261110090807.csv");
        }
        for vehicle in [
            LegacyValue::None,
            LegacyValue::Bool(true),
            LegacyValue::Int(1),
            LegacyValue::Float(f64::NAN),
            LegacyValue::Bytes(Vec::new()),
            LegacyValue::Tuple(Vec::new()),
        ] {
            assert_eq!(snapshot_native("none", &vehicle, &[], dir.path(), at()), None);
            assert_eq!(snapshot_native("ac", &vehicle, &[], dir.path(), at()), Some(Err(SNAPSHOT_ERROR.to_owned())));
        }
    }

    /// The header row then the channel rows, CR LF terminated, float cells with six decimals.
    #[test]
    fn writes_the_export_table() {
        let dir = tempfile::tempdir().unwrap();
        let rows = [
            ExportRow {
                label: "Speed",
                value: "36.000000".into(),
                min: "0".into(),
                max: "120.500000".into(),
                direction_positive: None,
            },
            ExportRow { label: "Gear", value: "3".into(), min: "1".into(), max: "3".into(), direction_positive: None },
            ExportRow {
                label: "Tyre",
                value: "[1, 'a,b']".into(),
                min: "[]".into(),
                max: "[1, 'a,b']".into(),
                direction_positive: None,
            },
        ];
        assert_eq!(snapshot("none", None, &rows, dir.path(), at()), None);
        let path = snapshot("ac", Some("GT3"), &rows, dir.path(), at()).unwrap().unwrap();
        assert_eq!(path, dir.path().join("snapshot_ac_GT3_20261110090807.csv"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "Variable,Value,Min,Max\r\nSpeed,36.000000,0,120.500000\r\nGear,3,1,3\r\nTyre,\"[1, 'a,b']\",[],\"[1, 'a,b']\"\r\n"
        );
        let sub_folder = snapshot("ac", Some("../x"), &rows, dir.path(), at()).unwrap();
        assert_eq!(sub_folder, Err(SNAPSHOT_ERROR.to_owned()), "export/snapshot_ac_../ does not exist");
        let missing = snapshot("ac", None, &rows, &dir.path().join("nope"), at()).unwrap();
        assert_eq!(missing, Err(SNAPSHOT_ERROR.to_owned()));
    }
}
