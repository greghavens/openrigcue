//! Telemetry export table, whose cells are already formatted strings.
use super::channels::CHANNEL_SPECS;
use super::min_max::MinMaxTable;
use openrigcue_model::{native_value::LegacyValue, python_repr::format_value, Telemetry};

/// Nonfloat values use their display text. Callers handle a failed conversion
/// per cell, rather than printing an unlimited compatibility representation.
fn format_cell(value: &LegacyValue) -> Result<String, String> {
    if value.type_name() == "float" {
        return Ok(format_value(&LegacyValue::Float(value.py_as_double()?)));
    }
    let text = openrigcue_model::python_repr::str_native(value)?;
    Ok(text.scalar_string().unwrap_or_else(|| format_value(value)))
}

/// Speed unit of the telemetry view (`APP.measurement_unit`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeedUnit {
    Kmh,
    Mph,
}
impl SpeedUnit {
    /// Exact required APP lookup; every obtained value other than the string K selects mph.
    pub fn from_native_config(config: &openrigcue_model::native_value::Dict) -> Result<Self, String> {
        let app = config.get("APP").ok_or_else(|| "'APP'".to_owned())?;
        let unit = app.item("measurement_unit")?;
        Ok(if matches!(unit,LegacyValue::Str(value) if value=="K") { Self::Kmh } else { Self::Mph })
    }

    fn factor(self) -> f64 {
        match self {
            Self::Kmh => 3.6,
            Self::Mph => 2.236_94,
        }
    }
}
pub const EXPORT_HEADER: [&str; 4] = ["Variable", "Value", "Min", "Max"];

/// Formatted table cells, shared by the page, snapshots and continuous CSV writer.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ExportRow {
    pub label: &'static str,
    pub value: String,
    pub min: String,
    pub max: String,
    /// Raw numeric comparison retained for UI pitch/roll arrows; not an export cell.
    pub direction_positive: Option<bool>,
}

/// Displayed channels in registry order. Missing reads become integer0; each speed
/// multiplication has its own fallback before the float-or-text formatting.
pub fn export_rows(telemetry: &Telemetry, extremes: &MinMaxTable, unit: SpeedUnit, show_all: bool) -> Vec<ExportRow> {
    export_rows_with_failures(telemetry, extremes, unit, show_all).into_iter().map(|(row, _)| row).collect()
}

fn export_rows_with_failures(
    telemetry: &Telemetry,
    extremes: &MinMaxTable,
    unit: SpeedUnit,
    show_all: bool,
) -> Vec<(ExportRow, bool)> {
    CHANNEL_SPECS
        .iter()
        .filter(|spec| show_all || spec.display)
        .map(|spec| {
            let value = telemetry.native_value(spec.name).cloned().unwrap_or_else(|| {
                telemetry.get_assigned(spec.name).map(LegacyValue::Float).unwrap_or(LegacyValue::Int(0))
            });
            let direction_positive = value.py_gt(&LegacyValue::Int(0)).ok();
            let (min, max) = if let Some(seen) = extremes.native(spec.name) {
                (seen.min.clone(), seen.max.clone())
            } else if let Some(seen) = extremes.get(spec.name) {
                (LegacyValue::Float(seen.min), LegacyValue::Float(seen.max))
            } else {
                (LegacyValue::Int(0), LegacyValue::Int(0))
            };
            let cell = |value: LegacyValue| {
                let value = if spec.name == "speed" {
                    // Each missing/nonnumeric multiply is caught independently as integer0.
                    value.number().map(|value| LegacyValue::Float(value * unit.factor())).unwrap_or(LegacyValue::Int(0))
                } else {
                    value
                };
                // Each of the three format calls has its own
                // independent failure handling.
                format_cell(&value)
            };
            // A missing speed read fails before multiplication; its fallback remains integer 0.
            let speed_missing = spec.name == "speed" && telemetry.get_assigned(spec.name).is_none();
            let value = if speed_missing { Ok("0".into()) } else { cell(value) };
            let value_format_failed = value.is_err();
            let row = ExportRow {
                label: spec.label,
                direction_positive,
                value: value.unwrap_or_default(),
                min: if spec.name == "speed" && extremes.get(spec.name).is_none() {
                    "0".into()
                } else {
                    cell(min).unwrap_or_default()
                },
                max: if spec.name == "speed" && extremes.get(spec.name).is_none() {
                    "0".into()
                } else {
                    cell(max).unwrap_or_default()
                },
            };
            (row, value_format_failed)
        })
        .collect()
}

/// Live UI rows carry the error at the row where it occurred, so earlier widget updates survive.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LiveTableRow {
    #[serde(flatten)]
    pub cells: ExportRow,
    pub error: Option<String>,
    /// Formatting and the direction suffix share one failure handler.
    /// A conversion failure blanks the entire cell, including that suffix.
    pub value_format_failed: bool,
}

pub fn live_table_rows(
    telemetry: &Telemetry,
    extremes: &MinMaxTable,
    unit: Result<SpeedUnit, String>,
    show_all: bool,
) -> Vec<LiveTableRow> {
    let rows =
        export_rows_with_failures(telemetry, extremes, unit.as_ref().copied().unwrap_or(SpeedUnit::Mph), show_all);
    CHANNEL_SPECS
        .iter()
        .filter(|spec| show_all || spec.display)
        .zip(rows)
        .map(|(spec, (cells, value_format_failed))| {
            let error = if spec.name == "speed" {
                unit.as_ref().err().cloned()
            } else if matches!(spec.name, "pitch" | "roll") && cells.direction_positive.is_none() {
                telemetry
                    .native_value(spec.name)
                    .map(|value| format!("'>' not supported between instances of '{}' and 'int'", value.type_name()))
            } else {
                None
            };
            LiveTableRow { cells, error, value_format_failed }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_output_limit_blanks_only_the_caught_cell_and_keeps_later_rows() {
        let mut telemetry = Telemetry::off();
        let mut extremes = MinMaxTable::default();
        extremes.update_checked(&telemetry).unwrap();
        let huge = LegacyValue::from_integer(openrigcue_model::native_value::BigInt::from(10u8).pow(4300));
        telemetry.set_native("pitch", huge.clone());
        telemetry.set_native("roll", LegacyValue::Int(7));
        extremes.update_checked(&telemetry).unwrap();
        let rows = export_rows(&telemetry, &extremes, SpeedUnit::Kmh, false);
        let pitch = rows.iter().find(|row| row.label == "Pitch").unwrap();
        assert_eq!((&*pitch.value, &*pitch.min, &*pitch.max), ("", "0", ""));
        assert_eq!(pitch.direction_positive, Some(true), "comparison stays exact before str fails");
        let roll = rows.iter().find(|row| row.label == "Roll").unwrap();
        assert_eq!((&*roll.value, &*roll.min, &*roll.max), ("7", "0", "7"));
        let live = live_table_rows(&telemetry, &extremes, Ok(SpeedUnit::Kmh), false);
        let pitch = live.iter().find(|row| row.cells.label == "Pitch").unwrap();
        assert!(pitch.value_format_failed, "the UI must suppress the suffix in the same cell catch");
        assert!(pitch.error.is_none(), "caught formatting does not kill the row/UI loop");
        assert!(!live.iter().find(|row| row.cells.label == "Roll").unwrap().value_format_failed);

        telemetry.set_native("tyre_temperature_lf", LegacyValue::List(vec![huge]));
        let rows = export_rows(&telemetry, &extremes, SpeedUnit::Kmh, true);
        assert_eq!(rows.iter().find(|row| row.label == "Tyre Temperature LF").unwrap().value, "");
        let accepted = LegacyValue::from_integer(openrigcue_model::native_value::BigInt::from(10u8).pow(4299));
        assert_eq!(format_cell(&accepted).unwrap().len(), 4300);
        let hexadecimal = LegacyValue::from(format!("0x{}", "f".repeat(5000)));
        assert_eq!(format_cell(&hexadecimal).unwrap().len(), 5002);
    }

    #[test]
    fn rows_convert_speed() {
        let mut telemetry = Telemetry::off();
        telemetry.speed = 10.0;
        let mut extremes = MinMaxTable::default();
        let rows = export_rows(&telemetry, &extremes, SpeedUnit::Kmh, false);
        assert_eq!(rows.len(), 70);
        let speed = rows.iter().find(|row| row.label == "Speed").unwrap();
        assert_eq!((&*speed.value, &*speed.min, &*speed.max), ("36.000000", "0", "0"));
        extremes.update(&telemetry);
        let rows = export_rows(&telemetry, &extremes, SpeedUnit::Mph, true);
        assert_eq!(rows.len(), 79);
        let speed = rows.iter().find(|row| row.label == "Speed").unwrap();
        assert_eq!((&*speed.value, &*speed.min, &*speed.max), ("22.369400", "22.369400", "22.369400"));
        assert!(rows.iter().any(|row| row.label == "Max Rpm"));
    }
}
