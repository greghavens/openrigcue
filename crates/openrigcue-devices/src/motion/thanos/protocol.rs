//! Pure encoding and decoding of the Thanos AMC serial protocol.

use std::cmp::Ordering;
use std::fmt;

/// Serial speed of every AMC controller.
pub const BAUD: u32 = 250_000;
/// Firmware/model query, sent without terminator.
pub const FIRMWARE_QUERY: &[u8] = b"RQM";
/// Long configuration dump query.
pub const LONG_CONFIG_QUERY: &[u8] = b"CMD55";
/// Short configuration dump query.
pub const SHORT_CONFIG_QUERY: &[u8] = b"CMD56";
/// Third step of the authentication handshake.
pub const AUTH_RESULT_QUERY: &[u8] = b"CMD88";
/// Park command, accepted only in standby.
pub const PARK_IF_STANDBY: &[u8] = b"Park";
/// Parameter code that saves all parameters at once.
pub const SAVE_ALL_CODE: &str = "45";
/// Parameter code that saves all SRS parameters at once.
pub const SAVE_ALL_SRS_CODE: &str = "46";

/// Controller models that answer `RQM` with the short configuration layout.
pub const AMC_MODELS: [&str; 3] = ["AMC-AASD15A", "AMC-MDBOX", "AMC-NANO"];
/// Model reported by the `AMC-MDBOX`, which needs a longer pause after each saved parameter.
pub const MDBOX_MODEL: &str = "AMC-MDBOX";
/// First firmware that reports the vertical stroke in its short configuration.
pub const STROKE_REPORTING_FIRMWARE: &str = "2.11.fix4";

/// Appends the `LF CR` terminator.
pub fn terminated(payload: &[u8]) -> Vec<u8> {
    let mut frame = payload.to_vec();
    frame.extend_from_slice(&[0x0A, 0x0D]);
    frame
}

/// The streaming frame `FF FF` + big-endian u16 words + `0A 0D` (20 bytes for eight words),
/// of positions already reduced to 16 bits.
pub fn position_frame_words(words: &[u16]) -> Vec<u8> {
    let mut payload = vec![0xFF, 0xFF];
    for word in words {
        payload.extend_from_slice(&word.to_be_bytes());
    }
    terminated(&payload)
}

pub fn position_frame(positions: &[i64; 8]) -> Vec<u8> {
    let mut payload = vec![0xFF, 0xFF];
    for &p in positions {
        payload.extend_from_slice(&(p.rem_euclid(65_536) as u16).to_be_bytes());
    }
    terminated(&payload)
}

/// `CMD<code>` + terminator: reads one parameter.
pub fn read_parameter_frame(code: &str) -> Vec<u8> {
    terminated(format!("CMD{code}").as_bytes())
}

/// `spv<code><value>` without terminator: saves one parameter.
pub fn save_parameter_frame(code: &str, value: &str) -> Vec<u8> {
    format!("spv{code}{value}").into_bytes()
}

/// A configuration value: an integer when the text is an integer, else the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldValue {
    Int(i64),
    Text(String),
}

impl FieldValue {
    /// The value as an integer: integers pass, text must parse as one ([`parse_python_int`]).
    pub fn as_int(&self) -> Option<i64> {
        match self {
            FieldValue::Int(v) => Some(*v),
            FieldValue::Text(t) => parse_python_int(t),
        }
    }
}

impl fmt::Display for FieldValue {
    /// The value as text.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FieldValue::Int(v) => write!(f, "{v}"),
            FieldValue::Text(t) => f.write_str(t),
        }
    }
}

/// The integer of base-10 text: surrounding whitespace, an optional sign, digits with single
/// underscores between them.
pub fn parse_python_int(text: &str) -> Option<i64> {
    let integer = openrigcue_model::python_numeric::integer_text(text)?;
    if integer.trim_start_matches(['+', '-']).len() > 4300 {
        return None;
    }
    integer.parse().ok()
}

/// One named field of a configuration dump. Per-motor fields carry the motor number 1..=4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Field {
    DataSrs,
    FirmwareVersion,
    BetaVersion,
    Model,
    Filler,
    MotorNumber,
    FilterFactor,
    SpikeRange,
    SpikeFactor,
    SpikeFilterEnabled,
    StrokeCm,
    SpikeFactorTl,
    SpikeFilterEnabledTl,
    DisableParkType,
    PlatformCheck,
    Firmware,
    MasterSlaveRole,
    ParkPosition,
    ParkMoveSpeed,
    ParkMoveTimeout,
    SixDofStartPosition,
    StartMoveStep,
    StartMoveTimeout,
    KlmMode,
    EstopMode,
    SpikeMoveSpeed,
    SpikeFineMoveSpeed,
    PulseFrequency,
    ButtonSpeed,
    ForceOfflineMode,
    Stroke(u8),
    LeadScrew(u8),
    Inline(u8),
    Ratio(u8),
}

impl Field {
    /// The key of the field in a configuration.
    pub fn name(self) -> String {
        let fixed = match self {
            Field::DataSrs => "data_srs",
            Field::FirmwareVersion => "firmware_version",
            Field::BetaVersion => "beta_version",
            Field::Model => "model",
            Field::Filler => "filler",
            Field::MotorNumber => "motor_number",
            Field::FilterFactor => "filter_factor",
            Field::SpikeRange => "spike_range",
            Field::SpikeFactor => "spike_factor",
            Field::SpikeFilterEnabled => "spike_filter_en",
            Field::StrokeCm => "stroke_cm",
            Field::SpikeFactorTl => "spike_factor_TL",
            Field::SpikeFilterEnabledTl => "spike_filter_en_TL",
            Field::DisableParkType => "disable_park_type",
            Field::PlatformCheck => "platform_check",
            Field::Firmware => "firmware",
            Field::MasterSlaveRole => "master_slave_role",
            Field::ParkPosition => "park_position",
            Field::ParkMoveSpeed => "park_move_speed",
            Field::ParkMoveTimeout => "park_move_timeout",
            Field::SixDofStartPosition => "6dof_start_pos",
            Field::StartMoveStep => "start_move_step",
            Field::StartMoveTimeout => "start_move_timeout",
            Field::KlmMode => "klm_mode",
            Field::EstopMode => "estop_mode",
            Field::SpikeMoveSpeed => "spike_move_speed",
            Field::SpikeFineMoveSpeed => "spike_fine_move_speed",
            Field::PulseFrequency => "pulse_freq",
            Field::ButtonSpeed => "button_speed",
            Field::ForceOfflineMode => "force_offline_mode",
            Field::Stroke(n) => return format!("stroke{n}"),
            Field::LeadScrew(n) => return format!("lead_screw{n}"),
            Field::Inline(n) => return format!("inline{n}"),
            Field::Ratio(n) => return format!("ratio{n}"),
        };
        fixed.to_string()
    }

    /// The parameter command that writes this field (by exact key or,
    /// for the per-motor families, by key prefix with the motor index one less than the suffix).
    pub fn parameter(self) -> Option<Parameter> {
        Some(match self {
            Field::Stroke(n) => Parameter::Stroke(n.wrapping_sub(1)),
            Field::Inline(n) => Parameter::Inline(n.wrapping_sub(1)),
            Field::Ratio(n) => Parameter::Ratio(n.wrapping_sub(1)),
            Field::LeadScrew(n) => Parameter::LeadScrew(n.wrapping_sub(1)),
            Field::ParkPosition => Parameter::ParkPosition,
            Field::DisableParkType => Parameter::DisableParkType,
            Field::SixDofStartPosition => Parameter::StandbyPosition,
            Field::ParkMoveSpeed => Parameter::ParkMoveSpeed,
            Field::StartMoveStep => Parameter::StandbySpeed,
            Field::StartMoveTimeout => Parameter::StandbyTimeout,
            Field::ParkMoveTimeout => Parameter::ParkMoveTimeout,
            Field::KlmMode => Parameter::KlmMode,
            Field::EstopMode => Parameter::EstopMode,
            Field::FilterFactor => Parameter::FilterFactor,
            Field::MotorNumber => Parameter::MotorNumber,
            Field::ForceOfflineMode => Parameter::ForceOfflineMode,
            Field::SpikeMoveSpeed => Parameter::SpikeEscSpeed,
            Field::SpikeFineMoveSpeed => Parameter::SpikeFineSpeed,
            Field::PlatformCheck => Parameter::PlatformCheck,
            _ => return None,
        })
    }
}

/// Fields the secondary controller copies from the main one when they differ.
pub const SECONDARY_SYNC_FIELDS: [Field; 14] = [
    Field::ParkPosition,
    Field::SixDofStartPosition,
    Field::ParkMoveSpeed,
    Field::StartMoveStep,
    Field::StartMoveTimeout,
    Field::ParkMoveTimeout,
    Field::KlmMode,
    Field::EstopMode,
    Field::FilterFactor,
    Field::MotorNumber,
    Field::ForceOfflineMode,
    Field::SpikeMoveSpeed,
    Field::SpikeFineMoveSpeed,
    Field::PlatformCheck,
];

/// The first short configuration layout.
pub const SHORT_LAYOUT_V1: [Field; 15] = [
    Field::DataSrs,
    Field::FirmwareVersion,
    Field::BetaVersion,
    Field::Model,
    Field::Filler,
    Field::MotorNumber,
    Field::FilterFactor,
    Field::SpikeRange,
    Field::SpikeFactor,
    Field::SpikeFilterEnabled,
    Field::StrokeCm,
    Field::SpikeFactorTl,
    Field::SpikeFilterEnabledTl,
    Field::DisableParkType,
    Field::PlatformCheck,
];

/// The second short configuration layout: v1 without `filler`.
pub const SHORT_LAYOUT_V2: [Field; 14] = [
    Field::DataSrs,
    Field::FirmwareVersion,
    Field::BetaVersion,
    Field::Model,
    Field::MotorNumber,
    Field::FilterFactor,
    Field::SpikeRange,
    Field::SpikeFactor,
    Field::SpikeFilterEnabled,
    Field::StrokeCm,
    Field::SpikeFactorTl,
    Field::SpikeFilterEnabledTl,
    Field::DisableParkType,
    Field::PlatformCheck,
];

/// The long configuration layout.
pub const LONG_LAYOUT: [Field; 42] = [
    Field::DataSrs,
    Field::Firmware,
    Field::MasterSlaveRole,
    Field::MotorNumber,
    Field::ParkPosition,
    Field::ParkMoveSpeed,
    Field::ParkMoveTimeout,
    Field::SixDofStartPosition,
    Field::StartMoveStep,
    Field::StartMoveTimeout,
    Field::DisableParkType,
    Field::Filler,
    Field::KlmMode,
    Field::SpikeRange,
    Field::SpikeFactor,
    Field::SpikeFilterEnabled,
    Field::PlatformCheck,
    Field::EstopMode,
    Field::SpikeMoveSpeed,
    Field::SpikeFineMoveSpeed,
    Field::SpikeFactorTl,
    Field::SpikeFilterEnabledTl,
    Field::PulseFrequency,
    Field::ButtonSpeed,
    Field::ForceOfflineMode,
    Field::FilterFactor,
    Field::Stroke(1),
    Field::Stroke(2),
    Field::Stroke(3),
    Field::Stroke(4),
    Field::LeadScrew(1),
    Field::LeadScrew(2),
    Field::LeadScrew(3),
    Field::LeadScrew(4),
    Field::Inline(1),
    Field::Inline(2),
    Field::Inline(3),
    Field::Inline(4),
    Field::Ratio(1),
    Field::Ratio(2),
    Field::Ratio(3),
    Field::Ratio(4),
];

/// Which dump to request: short (`CMD56`) or long (`CMD55`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigKind {
    Short,
    Long,
}

/// A parsed configuration dump: values by position of the layout.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ControllerConfig {
    entries: Vec<(Field, FieldValue)>,
}

impl ControllerConfig {
    pub fn get(&self, field: Field) -> Option<&FieldValue> {
        self.entries.iter().find(|(f, _)| *f == field).map(|(_, v)| v)
    }

    pub fn entries(&self) -> &[(Field, FieldValue)] {
        &self.entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The configuration as the log prints it: `{'<field>': <value>, …}`, text values quoted.
    pub fn python_str(&self) -> String {
        let items: Vec<String> = self
            .entries
            .iter()
            .map(|(field, value)| {
                let value = match value {
                    FieldValue::Int(v) => v.to_string(),
                    FieldValue::Text(t) => crate::motion::python::str_repr(t),
                };
                format!("{}: {value}", crate::motion::python::str_repr(&field.name()))
            })
            .collect();
        format!("{{{}}}", items.join(", "))
    }
}

/// Errors of [`parse_config`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("configuration reply has {found} fields, the layout needs {needed}")]
    TooShort { found: usize, needed: usize },
}

/// Builds the configuration from the first reply line: fields are split
/// at `':'`; the short dump uses layout v1 when field 4 is empty, else v2; the first 5 (short) or 3 (long)
/// values stay text, later ones become integers when they are integer text.
pub fn parse_config(kind: ConfigKind, line: &str) -> Result<ControllerConfig, ConfigError> {
    let data: Vec<&str> = line.split(':').collect();
    let (layout, text_fields): (&[Field], usize) = match kind {
        ConfigKind::Short => {
            let filler = data.get(4).ok_or(ConfigError::TooShort { found: data.len(), needed: 5 })?;
            let layout: &[Field] = if filler.is_empty() { &SHORT_LAYOUT_V1 } else { &SHORT_LAYOUT_V2 };
            (layout, 5)
        }
        ConfigKind::Long => (&LONG_LAYOUT, 3),
    };
    if data.len() < layout.len() {
        return Err(ConfigError::TooShort { found: data.len(), needed: layout.len() });
    }
    let entries = layout
        .iter()
        .zip(&data)
        .enumerate()
        .map(|(i, (&field, &raw))| {
            let value = match parse_python_int(raw) {
                Some(v) if i >= text_fields => FieldValue::Int(v),
                _ => FieldValue::Text(raw.to_string()),
            };
            (field, value)
        })
        .collect();
    Ok(ControllerConfig { entries })
}

/// A writable controller parameter. Per-motor variants carry the motor index 0..=3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parameter {
    MotorNumber,
    DisableParkType,
    PulseFrequency,
    ParkPosition,
    ParkMoveSpeed,
    ParkMoveTimeout,
    StandbyPosition,
    StandbySpeed,
    StandbyTimeout,
    PlatformCheck,
    RotationOffset,
    ButtonSpeed,
    EstopMode,
    KlmMode,
    SpikeRange,
    SpikeLevel,
    SpikeOnOff,
    TlSpikeLevel,
    TlSpikeOnOff,
    FilterFactor,
    ForceOfflineMode,
    SpikeEscSpeed,
    SpikeFineSpeed,
    Stroke(u8),
    LeadScrew(u8),
    Inline(u8),
    Ratio(u8),
}

/// How a value is rendered after the parameter code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ValueFormat {
    /// The value's text as it is.
    Plain,
    /// A number, zero-padded to two digits.
    Width2,
    /// A number, zero-padded to three digits.
    Width3,
    /// The value's text, zero-filled to three characters.
    ZeroFill3,
}

impl Parameter {
    /// The two-digit parameter code. Per-motor codes are `base + motor`, zero-padded to two digits.
    pub fn code(self) -> String {
        let fixed = match self {
            Parameter::MotorNumber => 1,
            Parameter::DisableParkType => 2,
            Parameter::PulseFrequency => 3,
            Parameter::ParkPosition => 4,
            Parameter::ParkMoveSpeed => 5,
            Parameter::ParkMoveTimeout => 6,
            Parameter::StandbyPosition => 7,
            Parameter::StandbySpeed => 8,
            Parameter::StandbyTimeout => 9,
            Parameter::PlatformCheck => 10,
            Parameter::RotationOffset => 11,
            Parameter::ButtonSpeed => 12,
            Parameter::EstopMode => 13,
            Parameter::KlmMode => 14,
            Parameter::SpikeRange => 15,
            Parameter::SpikeLevel => 16,
            Parameter::SpikeOnOff => 17,
            Parameter::TlSpikeLevel => 18,
            Parameter::TlSpikeOnOff => 19,
            Parameter::FilterFactor => 20,
            Parameter::ForceOfflineMode => 21,
            Parameter::SpikeEscSpeed => 22,
            Parameter::SpikeFineSpeed => 23,
            Parameter::Stroke(m) => 24 + i64::from(m),
            Parameter::LeadScrew(m) => 28 + i64::from(m),
            Parameter::Inline(m) => 32 + i64::from(m),
            Parameter::Ratio(m) => 36 + i64::from(m),
        };
        format!("{fixed:02}")
    }

    /// The motor index of a per-motor parameter.
    pub fn motor(self) -> Option<u8> {
        match self {
            Parameter::Stroke(m) | Parameter::LeadScrew(m) | Parameter::Inline(m) | Parameter::Ratio(m) => Some(m),
            _ => None,
        }
    }

    fn value_format(self) -> ValueFormat {
        match self {
            Parameter::PulseFrequency
            | Parameter::ParkMoveTimeout
            | Parameter::StandbyTimeout
            | Parameter::ButtonSpeed => ValueFormat::Width2,
            Parameter::ParkPosition
            | Parameter::ParkMoveSpeed
            | Parameter::StandbyPosition
            | Parameter::StandbySpeed
            | Parameter::RotationOffset
            | Parameter::SpikeRange
            | Parameter::SpikeLevel
            | Parameter::TlSpikeLevel
            | Parameter::SpikeEscSpeed
            | Parameter::SpikeFineSpeed => ValueFormat::Width3,
            Parameter::Stroke(_) => ValueFormat::ZeroFill3,
            _ => ValueFormat::Plain,
        }
    }

    /// The frame name and line of the parameter's command, for errors that pass it.
    pub fn command_site(self) -> (&'static str, u32) {
        match self {
            Parameter::MotorNumber => ("AMC.cmd_motor_number", 430),
            Parameter::Stroke(_) => ("AMC.cmd_stroke", 439),
            Parameter::LeadScrew(_) => ("AMC.cmd_lead_screw", 446),
            Parameter::Inline(_) => ("AMC.cmd_inline", 453),
            Parameter::Ratio(_) => ("AMC.cmd_ratio", 460),
            Parameter::ParkPosition => ("AMC.cmd_park_position", 466),
            Parameter::DisableParkType => ("AMC.cmd_disableparktype", 478),
            Parameter::StandbyPosition => ("AMC.cmd_standby_position", 484),
            Parameter::ParkMoveSpeed => ("AMC.cmd_park_move_speed", 490),
            Parameter::StandbyTimeout => ("AMC.cmd_standby_timeout", 496),
            Parameter::StandbySpeed => ("AMC.cmd_standby_speed", 502),
            Parameter::KlmMode => ("AMC.cmd_klm_mode", 511),
            Parameter::PulseFrequency => ("AMC.cmd_pulse_freq", 517),
            Parameter::ParkMoveTimeout => ("AMC.cmd_park_move_timeout", 523),
            Parameter::ButtonSpeed => ("AMC.cmd_buttonspeed", 529),
            Parameter::PlatformCheck => ("AMC.cmd_platform_check", 535),
            Parameter::EstopMode => ("AMC.cmd_estop_mode", 544),
            Parameter::ForceOfflineMode => ("AMC.cmd_force_offline_mode", 550),
            Parameter::FilterFactor => ("AMC.cmd_filter_factor", 559),
            Parameter::SpikeLevel => ("AMC.cmd_spike_level", 570),
            Parameter::SpikeOnOff => ("AMC.cmd_spike_on_off", 580),
            Parameter::TlSpikeLevel => ("AMC.cmd_tl_spike_level", 590),
            Parameter::TlSpikeOnOff => ("AMC.cmd_tl_spike_on_off", 600),
            Parameter::SpikeRange => ("AMC.cmd_spike_range", 611),
            Parameter::RotationOffset => ("AMC.cmd_rotation_offset", 617),
            Parameter::SpikeEscSpeed => ("AMC.cmd_spike_esc_speed", 623),
            Parameter::SpikeFineSpeed => ("AMC.cmd_spike_fine_speed", 629),
        }
    }

    /// The frame name and line where a parameter's value is formatted as a zero-padded number: where a
    /// value that is not a number is a type error. `None`
    /// for the commands that append the value's text (plain or zero-filled), which accept any value.
    pub fn format_site(self) -> Option<(&'static str, u32)> {
        Some(match self {
            Parameter::ParkPosition => ("AMC.cmd_park_position", 465),
            Parameter::StandbyPosition => ("AMC.cmd_standby_position", 483),
            Parameter::ParkMoveSpeed => ("AMC.cmd_park_move_speed", 489),
            Parameter::StandbyTimeout => ("AMC.cmd_standby_timeout", 495),
            Parameter::StandbySpeed => ("AMC.cmd_standby_speed", 501),
            Parameter::PulseFrequency => ("AMC.cmd_pulse_freq", 516),
            Parameter::ParkMoveTimeout => ("AMC.cmd_park_move_timeout", 522),
            Parameter::ButtonSpeed => ("AMC.cmd_buttonspeed", 528),
            Parameter::SpikeLevel => ("AMC.cmd_spike_level", 569),
            Parameter::TlSpikeLevel => ("AMC.cmd_tl_spike_level", 589),
            Parameter::SpikeRange => ("AMC.cmd_spike_range", 609),
            Parameter::RotationOffset => ("AMC.cmd_rotation_offset", 616),
            Parameter::SpikeEscSpeed => ("AMC.cmd_spike_esc_speed", 622),
            Parameter::SpikeFineSpeed => ("AMC.cmd_spike_fine_speed", 628),
            _ => return None,
        })
    }

    /// The value text appended for an integer of any width: zero-padded to two or three characters (the sign
    /// counts toward the width), zero-filled to three, or plain.
    pub fn format_wide(self, value: &num_bigint::BigInt) -> String {
        let text = value.to_string();
        match self.value_format() {
            ValueFormat::Plain => text,
            ValueFormat::ZeroFill3 | ValueFormat::Width3 => zfill(&text, 3),
            ValueFormat::Width2 => zfill(&text, 2),
        }
    }

    /// The value text appended. `None` where a number format meets a text value.
    pub fn format_value(self, value: &FieldValue) -> Option<String> {
        match self.value_format() {
            ValueFormat::Plain => Some(value.to_string()),
            ValueFormat::ZeroFill3 => Some(zfill(&value.to_string(), 3)),
            ValueFormat::Width2 => match value {
                FieldValue::Int(v) => Some(format!("{v:02}")),
                FieldValue::Text(_) => None,
            },
            ValueFormat::Width3 => match value {
                FieldValue::Int(v) => Some(format!("{v:03}")),
                FieldValue::Text(_) => None,
            },
        }
    }
}

/// `text` padded with zeros on the left to `width` characters, after a leading sign.
fn zfill(text: &str, width: usize) -> String {
    let len = text.chars().count();
    if len >= width {
        return text.to_string();
    }
    let pad = "0".repeat(width - len);
    match text.strip_prefix(['-', '+']) {
        Some(rest) => format!("{}{pad}{rest}", &text[..1]),
        None => format!("{pad}{text}"),
    }
}

/// The value sent as the spike range: `spike / 255`, truncated.
pub fn spike_range(spike: i64) -> i64 {
    (spike as f64 / 255.0).trunc() as i64
}

/// The value sent as the spike level: `spike / range`, truncated.
/// `None` when the range is zero (a division by zero).
pub fn spike_level(spike: f64, range: i64) -> Option<i64> {
    (range != 0).then(|| (spike / range as f64).trunc() as i64)
}

/// One component of a loosely structured version text.
#[derive(Debug, Clone, PartialEq, Eq)]
enum VersionPart {
    Number(i64),
    Text(String),
}

/// Splits a version text: runs of digits, runs of lower-case letters and the text between them
/// are components, `'.'` separators are dropped, components that are integers become numbers.
fn loose_version_parts(version: &str) -> Vec<VersionPart> {
    #[derive(PartialEq, Clone, Copy)]
    enum Class {
        Digit,
        Lower,
        Dot,
        Other,
    }
    let class = |c: char| match c {
        '0'..='9' => Class::Digit,
        'a'..='z' => Class::Lower,
        '.' => Class::Dot,
        _ => Class::Other,
    };
    let mut raw: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_class = None;
    for c in version.chars() {
        let k = class(c);
        let joins = current_class == Some(k) && k != Class::Dot;
        if !joins && !current.is_empty() {
            raw.push(std::mem::take(&mut current));
        }
        current.push(c);
        current_class = Some(k);
    }
    if !current.is_empty() {
        raw.push(current);
    }
    raw.into_iter()
        .filter(|p| p != ".")
        .map(|p| match parse_python_int(&p) {
            Some(n) => VersionPart::Number(n),
            None => VersionPart::Text(p),
        })
        .collect()
}

/// What an AMC greeting says about the controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bonjour {
    pub is_master: bool,
    pub is_dip3_on: bool,
}

/// Reads a greeting: whether it contains `(M)`, and whether it contains `DIP3-ON`.
pub fn parse_bonjour(message: &str) -> Bonjour {
    Bonjour { is_master: message.contains("(M)"), is_dip3_on: message.contains("DIP3-ON") }
}

/// The component types (`'int'` / `'str'`) of the first pair that differs between the versions `a` and
/// `b` when one is a number and the other text: the operands named by the type error of the
/// comparison. `None` when the versions can be compared.
pub fn loose_version_mismatch(a: &str, b: &str) -> Option<(&'static str, &'static str)> {
    let (pa, pb) = (loose_version_parts(a), loose_version_parts(b));
    let differing = pa.iter().zip(&pb).find(|(x, y)| x != y)?;
    match differing {
        (VersionPart::Number(_), VersionPart::Text(_)) => Some(("int", "str")),
        (VersionPart::Text(_), VersionPart::Number(_)) => Some(("str", "int")),
        _ => None,
    }
}

/// The version `a` compared with the version `b`; `None` where they cannot be compared
/// (a number meets a text component at the same position).
pub fn compare_loose_versions(a: &str, b: &str) -> Option<Ordering> {
    let (pa, pb) = (loose_version_parts(a), loose_version_parts(b));
    for (x, y) in pa.iter().zip(&pb) {
        let ord = match (x, y) {
            (VersionPart::Number(x), VersionPart::Number(y)) => x.cmp(y),
            (VersionPart::Text(x), VersionPart::Text(y)) => x.cmp(y),
            _ => return None,
        };
        if ord != Ordering::Equal {
            return Some(ord);
        }
    }
    Some(pa.len().cmp(&pb.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The greeting: substring tests for '(M)' and 'DIP3-ON'.
    #[test]
    fn bonjour_flags() {
        assert_eq!(parse_bonjour("Thanos4U(M) DIP3-ON"), Bonjour { is_master: true, is_dip3_on: true });
        assert_eq!(parse_bonjour("Thanos4U(S) DIP3-OFF"), Bonjour { is_master: false, is_dip3_on: false });
        assert!(!parse_bonjour("(m) dip3-on").is_master, "case-sensitive");
    }

    /// The position frame: FF FF, eight big-endian words, 0A 0D; 65536 wraps to 0.
    #[test]
    fn send_position_frame_layout() {
        let f = position_frame(&[0x1234, 3276, 62259, 65535, 65536, 0, -65535, 32768]);
        assert_eq!(
            f,
            [
                0xFF, 0xFF, 0x12, 0x34, 0x0C, 0xCC, 0xF3, 0x33, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x80,
                0x00, 0x0A, 0x0D
            ]
        );
    }

    /// The framing of a parameter read and a parameter save.
    #[test]
    fn parameter_frames() {
        assert_eq!(read_parameter_frame("39"), b"CMD39\n\r");
        assert_eq!(save_parameter_frame("15", "004"), b"spv15004");
    }

    /// The value formats: two digits, three digits, plain text, and zero-filled to three for the stroke.
    #[test]
    fn cmd_value_formats() {
        let i = FieldValue::Int;
        assert_eq!(Parameter::PulseFrequency.format_value(&i(5)).unwrap(), "05");
        assert_eq!(Parameter::ParkPosition.format_value(&i(1)).unwrap(), "001");
        assert_eq!(Parameter::SpikeLevel.format_value(&i(-1)).unwrap(), "-01");
        assert_eq!(Parameter::KlmMode.format_value(&i(2)).unwrap(), "2");
        assert_eq!(Parameter::Stroke(0).format_value(&i(15)).unwrap(), "015");
        assert_eq!(Parameter::Stroke(0).format_value(&i(-5)).unwrap(), "-05");
        assert_eq!(Parameter::KlmMode.format_value(&FieldValue::Text("x".into())).unwrap(), "x");
        assert!(Parameter::ParkPosition.format_value(&FieldValue::Text("x".into())).is_none());
    }

    /// The per-motor codes and the fixed codes of every parameter.
    #[test]
    fn cmd_codes() {
        assert_eq!(Parameter::MotorNumber.code(), "01");
        assert_eq!(Parameter::SpikeFineSpeed.code(), "23");
        assert_eq!(Parameter::Stroke(0).code(), "24");
        assert_eq!(Parameter::LeadScrew(3).code(), "31");
        assert_eq!(Parameter::Inline(1).code(), "33");
        assert_eq!(Parameter::Ratio(2).code(), "38");
    }

    /// The short configuration: empty field 4 selects layout v1; first five fields stay text.
    #[test]
    fn short_config_v1_and_v2() {
        let v1 = parse_config(ConfigKind::Short, "SRS:v2:11:AMC-AASD15A::4:0:5:6:1:15:7:1:1:2").unwrap();
        assert_eq!(v1.get(Field::FirmwareVersion), Some(&FieldValue::Text("v2".into())));
        assert_eq!(v1.get(Field::BetaVersion), Some(&FieldValue::Text("11".into())));
        assert_eq!(v1.get(Field::StrokeCm), Some(&FieldValue::Int(15)));
        assert_eq!(v1.get(Field::Filler), Some(&FieldValue::Text(String::new())));
        let v2 = parse_config(ConfigKind::Short, "SRS:v2:11:AMC-NANO:4:0:5:6:1:15:7:1:1:2").unwrap();
        assert_eq!(v2.get(Field::MotorNumber), Some(&FieldValue::Text("4".into())));
        assert_eq!(v2.get(Field::FilterFactor), Some(&FieldValue::Int(0)));
        assert_eq!(v2.get(Field::Filler), None);
    }

    /// A reply with fewer fields than the layout is an error.
    #[test]
    fn short_config_too_short_is_error() {
        assert!(parse_config(ConfigKind::Short, "SRS:v2").is_err());
        assert!(parse_config(ConfigKind::Long, "a:b:c").is_err());
    }

    /// The long configuration: three text fields, then integers where the text is an integer.
    #[test]
    fn long_config_parsing() {
        let fields: Vec<String> = (0..42).map(|i| i.to_string()).collect();
        let mut line = fields.join(":");
        line = line.replacen(":11:", ": 11 :", 1).replacen(":12:", ":x:", 1);
        let c = parse_config(ConfigKind::Long, &line).unwrap();
        assert_eq!(c.get(Field::MasterSlaveRole), Some(&FieldValue::Text("2".into())));
        assert_eq!(c.get(Field::MotorNumber), Some(&FieldValue::Int(3)));
        assert_eq!(c.get(Field::Filler), Some(&FieldValue::Int(11)));
        assert_eq!(c.get(Field::KlmMode), Some(&FieldValue::Text("x".into())));
        assert_eq!(c.get(Field::Stroke(1)), Some(&FieldValue::Int(26)));
        assert_eq!(c.get(Field::Ratio(4)), Some(&FieldValue::Int(41)));
    }

    /// Integer text: whitespace, sign and underscores between digits.
    #[test]
    fn python_int_parsing() {
        assert_eq!(parse_python_int(" 12 "), Some(12));
        assert_eq!(parse_python_int("-7"), Some(-7));
        assert_eq!(parse_python_int("1_000"), Some(1000));
        assert_eq!(parse_python_int("1__0"), None);
        assert_eq!(parse_python_int("_1"), None);
        assert_eq!(parse_python_int(""), None);
        assert_eq!(parse_python_int("1.5"), None);
    }

    /// The spike range and level, including the zero range that is an error.
    #[test]
    fn spike_conversions() {
        assert_eq!(spike_range(64530), 253);
        assert_eq!(spike_range(254), 0);
        assert_eq!(spike_range(255), 1);
        assert_eq!(spike_level(1000.0, 3), Some(333));
        assert_eq!(spike_level(5.0, 7), Some(0));
        assert_eq!(spike_level(-5.0, 3), Some(-1));
        assert!(spike_level(5.0, 0).is_none());
    }

    /// The firmware comparison with '2.11.fix4'.
    #[test]
    fn loose_version_comparison() {
        let cmp = |a| compare_loose_versions(a, STROKE_REPORTING_FIRMWARE);
        assert_eq!(cmp("2.11.fix4"), Some(Ordering::Equal));
        assert_eq!(cmp("2.11.fix5"), Some(Ordering::Greater));
        assert_eq!(cmp("2.12.0"), Some(Ordering::Greater));
        assert_eq!(cmp("2.10.fix9"), Some(Ordering::Less));
        assert_eq!(cmp(""), Some(Ordering::Less));
        assert_eq!(cmp("2.11"), Some(Ordering::Less));
        assert_eq!(cmp("2.11.3"), None, "int vs str raises TypeError");
    }
}
