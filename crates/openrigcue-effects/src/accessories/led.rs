//! LED strip segments driven by the Intellibox: rpm and speed bars, flag colours, spotter warnings, with the
//! per-segment blink state, and the colour encoding sent to the strip.

use super::numeric::{map_clamped, DivisionByZero};
use openrigcue_model::{
    native_value::{BigInt, LegacyValue},
    python_compare::native_equal,
    Telemetry,
};

/// Byte order of the strip (`LED.model`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LedByteOrder {
    #[default]
    Rgb,
    Grb,
}

impl LedByteOrder {
    /// The order a model name selects (`RGB` when the setting is missing): `'RGB'` and `'GRB'`;
    /// any other model has no colour word (`None`).
    pub fn from_model(model: &str) -> Option<LedByteOrder> {
        match model {
            "RGB" => Some(LedByteOrder::Rgb),
            "GRB" => Some(LedByteOrder::Grb),
            _ => None,
        }
    }
}

/// The named colours the segment logic uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamedColor {
    Black,
    Red,
    Green,
    Blue,
    White,
    Yellow,
    Orange3,
    Gray17,
}

impl NamedColor {
    /// The colour's RGB value.
    pub fn rgb(self) -> [u8; 3] {
        match self {
            NamedColor::Black => [0, 0, 0],
            NamedColor::Red => [255, 0, 0],
            NamedColor::Green => [0, 128, 0],
            NamedColor::Blue => [0, 0, 255],
            NamedColor::White => [255, 255, 255],
            NamedColor::Yellow => [255, 255, 0],
            NamedColor::Orange3 => [205, 133, 0],
            NamedColor::Gray17 => [43, 43, 43],
        }
    }
}

/// The 24-bit colour word sent to the strip: the three bytes (r, g, b), or
/// (g, r, b) for GRB strips, most significant first.
pub fn color_word(rgb: [u8; 3], order: LedByteOrder) -> u32 {
    let [r, g, b] = rgb;
    let (first, second) = match order {
        LedByteOrder::Rgb => (r, g),
        LedByteOrder::Grb => (g, r),
    };
    u32::from(first) << 16 | u32::from(second) << 8 | u32::from(b)
}

/// `LED.segmentN_type` codes (including `SO`, which the segment
/// logic treats like "Always On").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentType {
    /// `R`
    Rpm,
    /// `S`
    Speed,
    /// `F`
    Flag,
    /// `RF`: rpm, flag colours while a flag is out.
    RpmOrFlag,
    /// `SF`
    SpeedOrFlag,
    /// `SR`
    SpotterRight,
    /// `SL`
    SpotterLeft,
    /// `SRF`: spotter right while it is active, flag otherwise.
    SpotterRightOrFlag,
    /// `SLF`
    SpotterLeftOrFlag,
    /// `O`
    Off,
    /// `SO`
    SolidAlwaysOn,
    /// Any other code: no colours are filled in and the Intellibox frame then fails.
    Unknown,
}

impl SegmentType {
    pub fn from_code(code: &str) -> Self {
        match code {
            "R" => SegmentType::Rpm,
            "S" => SegmentType::Speed,
            "F" => SegmentType::Flag,
            "RF" => SegmentType::RpmOrFlag,
            "SF" => SegmentType::SpeedOrFlag,
            "SR" => SegmentType::SpotterRight,
            "SL" => SegmentType::SpotterLeft,
            "SRF" => SegmentType::SpotterRightOrFlag,
            "SLF" => SegmentType::SpotterLeftOrFlag,
            "O" => SegmentType::Off,
            "SO" => SegmentType::SolidAlwaysOn,
            _ => SegmentType::Unknown,
        }
    }
}

/// One `segmentN_*` group of the `LED` config section (positions are the 1-based UI values).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SegmentConfig {
    pub start: i64,
    pub end: i64,
    pub kind: SegmentType,
    /// `segmentN_color`, the "Always On" colour.
    pub always_on: [u8; 3],
}

/// The `LED` config section.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LedConfig {
    pub segment_count: i64,
    pub led_count: i64,
    pub brightness: i64,
    /// The order of the named colours (cached at start and when the Hardware page changes the model);
    /// `None` for a model without a colour word.
    pub byte_order: Option<LedByteOrder>,
    /// The order of the "Always On" colour: it is computed on every frame,
    /// so it follows the current `LED.model`; `None` likewise.
    pub always_on_order: Option<LedByteOrder>,
    pub segments: [SegmentConfig; 4],
}

/// Raw required values consumed by the segment logic, before typed defaults. Every count and
/// position is an arbitrary-precision integer; it is narrowed only where the
/// Intellibox packer range-checks it.
#[derive(Debug, Clone, PartialEq)]
pub struct NativeLedSettings {
    pub segment_count: Result<BigInt, NativeLedError>,
    /// Read only after a combined type resolves to the Flag branch.
    pub led_count: Result<BigInt, NativeLedError>,
    /// Required colour and model reads, consumed only by Always On/SO: the colour word
    /// (`None` for a model other than RGB/GRB).
    pub always_on: [Result<ColorValue, NativeLedError>; 4],
    pub segments: [NativeSegmentSettings; 4],
    /// Unconverted values compared to detect a segment change, including hidden segments.
    pub ranges: NativeSegmentRanges,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NativeSegmentRanges {
    pub values: [Result<LegacyValue, NativeLedError>; 8],
    /// `LED.segmentN_start` / `segmentN_end` as integers, still 1-based.
    pub positions: [Result<BigInt, NativeLedError>; 8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeSegmentSettings {
    pub start: Result<BigInt, NativeLedError>,
    pub end: Result<BigInt, NativeLedError>,
    pub kind: Result<SegmentType, NativeLedError>,
}

impl NativeSegmentRanges {
    pub fn from_positions(positions: [i64; 8]) -> Self {
        Self { values: positions.map(|value| Ok(LegacyValue::Int(value))), positions: positions.map(|v| Ok(v.into())) }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct NativeLedError {
    pub class: &'static str,
    pub message: String,
}

impl NativeLedError {
    /// A `Class: message` error of the shared raw-value operations.
    pub fn operation(error: String) -> Self {
        let error = super::values::operation(error);
        NativeLedError { class: error.class, message: error.message }
    }
}

/// One colour of a segment: the colour word, or null where there is none
/// (a model other than RGB/GRB). The Intellibox packer then fails on the null.
pub type ColorValue = Option<BigInt>;

/// The sidebar LED switch: compared with 2 and 0, anything else is "auto".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedMode {
    Off,
    Auto,
    AlwaysOn,
}

impl LedMode {
    pub fn from_status(status: i64) -> Self {
        match status {
            2 => LedMode::AlwaysOn,
            0 => LedMode::Off,
            _ => LedMode::Auto,
        }
    }
}

/// Per-frame inputs of the segment logic.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LedInputs {
    pub rpm: f64,
    pub max_rpm: f64,
    pub shift_light: bool,
    pub speed_kmh: f64,
    pub max_speed_kmh: f64,
    /// Normalized flag code from telemetry.
    pub flag: f64,
    pub spotter_left: f64,
    pub spotter_right: f64,
    pub mode: LedMode,
    /// The hardware page's tach test is running (it blanks the strip).
    pub tach_test: bool,
}

/// What one segment shows: its range, how many LEDs to light, and the two alternating colours
/// (integers of any width).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SegmentFrame {
    pub start: BigInt,
    pub end: BigInt,
    pub lit: BigInt,
    /// `(even_color, odd_color)`; `None` for an unknown segment type, which has no
    /// colours (reading them is an error).
    pub colors: Option<(ColorValue, ColorValue)>,
}

impl SegmentFrame {
    /// A frame of colour words (the typed carriers of the UI and tests).
    pub fn words(start: i64, end: i64, lit: i64, colors: Option<(u32, u32)>) -> Self {
        SegmentFrame {
            start: start.into(),
            end: end.into(),
            lit: lit.into(),
            colors: colors.map(|(even, odd)| (Some(even.into()), Some(odd.into()))),
        }
    }
}

/// Blink phase of one segment (toggled on every auto frame).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Blink {
    on: bool,
}

/// Why a segment frame could not be computed (the Intellibox cycle is skipped).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LedError {
    #[error(transparent)]
    Native(#[from] NativeLedError),
    #[error("max speed is zero")]
    ZeroMaxSpeed,
    #[error("speed bar is not a finite number")]
    NotFinite,
}

impl From<DivisionByZero> for LedError {
    fn from(_: DivisionByZero) -> Self {
        LedError::ZeroMaxSpeed
    }
}

/// Segment state across frames.
#[derive(Debug, Clone, Default)]
pub struct LedStrip {
    blink: [Blink; 4],
}

/// Colour pair used by the flag segment for a flag code. `is(code)` tests whether the passed
/// value equals the integer code (raw-value equality).
fn flag_colors(is_code: &dyn Fn(i64) -> bool, blink_on: bool) -> (NamedColor, NamedColor) {
    use NamedColor::*;
    let is = |codes: &[i64]| codes.iter().any(|c| is_code(*c));
    let blinking = |lit: NamedColor| if blink_on { (lit, Black) } else { (Black, lit) };
    if is(&[0]) {
        (Black, Black)
    } else if is(&[1]) {
        (Green, Green)
    } else if is(&[2]) {
        (Blue, Blue)
    } else if is(&[3]) {
        (White, White)
    } else if is(&[5]) {
        (Red, Red)
    } else if is(&[6, 36, 21]) {
        (Yellow, Yellow)
    } else if is(&[7]) {
        (Yellow, Black)
    } else if is(&[8]) {
        (White, Gray17)
    } else if is(&[9]) {
        (Orange3, Gray17)
    } else if is(&[10, 30, 35]) {
        (Black, Red)
    } else if is(&[11]) {
        blinking(White)
    } else if is(&[12, 22]) {
        blinking(Green)
    } else if is(&[13, 34]) {
        blinking(Yellow)
    } else if is(&[14, 20]) {
        blinking(Red)
    } else if is(&[15]) {
        (Green, Yellow)
    } else {
        (Black, Black)
    }
}

/// The flag and spotter arguments: the passed telemetry values, compared with integer constants by raw-value
/// equality, or the typed projections.
struct Signals<'a> {
    inputs: &'a LedInputs,
    telemetry: Option<&'a Telemetry>,
}

impl Signals<'_> {
    fn value(&self, key: &str, projection: f64) -> LegacyValue {
        match self.telemetry {
            Some(telemetry) => super::values::channel(telemetry, key, projection),
            None => LegacyValue::Float(projection),
        }
    }
    fn flag_is(&self, code: i64) -> bool {
        native_equal(&self.value("flag", self.inputs.flag), &LegacyValue::Int(code))
    }
    fn active(&self, key: &str, projection: f64) -> bool {
        !native_equal(&self.value(key, projection), &LegacyValue::Int(0))
    }
    fn spotter_left(&self) -> bool {
        self.active("spotter_left", self.inputs.spotter_left)
    }
    fn spotter_right(&self) -> bool {
        self.active("spotter_right", self.inputs.spotter_right)
    }
}

/// Resolve the combined types: `RF`/`SF` show the flag while one is out, `SRF`/`SLF` show the spotter while
/// it is active and the flag otherwise.
fn effective_type(kind: SegmentType, signals: &Signals<'_>) -> SegmentType {
    let flag = || !signals.flag_is(0);
    match kind {
        SegmentType::RpmOrFlag if flag() => SegmentType::Flag,
        SegmentType::RpmOrFlag => SegmentType::Rpm,
        SegmentType::SpeedOrFlag if flag() => SegmentType::Flag,
        SegmentType::SpeedOrFlag => SegmentType::Speed,
        SegmentType::SpotterRightOrFlag if signals.spotter_right() => SegmentType::SpotterRight,
        SegmentType::SpotterLeftOrFlag if signals.spotter_left() => SegmentType::SpotterLeft,
        SegmentType::SpotterRightOrFlag | SegmentType::SpotterLeftOrFlag => SegmentType::Flag,
        other => other,
    }
}

/// The rescale works in doubles: an integer bound beyond binary64 is an overflow error.
fn bound(value: &BigInt) -> Result<f64, NativeLedError> {
    LegacyValue::from_integer(value.clone()).py_as_double().map_err(NativeLedError::operation)
}

/// The rpm bar length: rpm rescaled from `0..max_rpm` to `start..end`, truncated, minus `start`; 0 when
/// any step fails (including a bound beyond a double).
fn rpm_bar(rpm: &LegacyValue, maximum: &LegacyValue, start: &BigInt, end: &BigInt) -> BigInt {
    let integer =
        |value: &LegacyValue| value.py_integer().and_then(|value| LegacyValue::from_integer(value).py_as_double()).ok();
    let bar = (|| {
        let (rpm, maximum) = (integer(rpm)?, integer(maximum)?);
        let (low, high) = (bound(start).ok()?, bound(end).ok()?);
        LegacyValue::Float(map_clamped(rpm, 0.0, maximum, low, high)).py_integer().ok()
    })();
    bar.map_or_else(|| BigInt::from(0), |bar| bar - start)
}

/// The speed bar length: speed rescaled from `0..max_speed` to `start..end`, rounded, minus `start`;
/// a NaN or infinite rescale is an error.
fn speed_bar(speed: f64, maximum: f64, start: &BigInt, end: &BigInt) -> Result<BigInt, LedError> {
    let bar = map_clamped(speed, 0.0, maximum, bound(start)?, bound(end)?);
    if !bar.is_finite() {
        return Err(LedError::NotFinite);
    }
    let rounded = LegacyValue::Float(bar.round_ties_even()).py_integer().map_err(NativeLedError::operation)?;
    Ok(rounded - start)
}

impl LedStrip {
    pub fn new() -> Self {
        Self::default()
    }

    /// Shared-channel calls convert text/bytes to integers inside the RPM branch, where a failure gives 0.
    pub(crate) fn segment_frame_for_telemetry(
        &mut self,
        segment: usize,
        config: &LedConfig,
        inputs: &LedInputs,
        telemetry: &Telemetry,
        native: Option<&NativeLedSettings>,
    ) -> Result<SegmentFrame, LedError> {
        self.segment_frame_impl(segment, config, inputs, Some(telemetry), native)
    }

    /// What segment `segment` (1..=4) shows this frame.
    pub fn segment_frame(
        &mut self,
        segment: usize,
        config: &LedConfig,
        inputs: &LedInputs,
    ) -> Result<SegmentFrame, LedError> {
        self.segment_frame_impl(segment, config, inputs, None, None)
    }

    pub fn segment_frame_native(
        &mut self,
        segment: usize,
        config: &LedConfig,
        inputs: &LedInputs,
        native: &NativeLedSettings,
    ) -> Result<SegmentFrame, LedError> {
        self.segment_frame_impl(segment, config, inputs, None, Some(native))
    }

    fn segment_frame_impl(
        &mut self,
        segment: usize,
        config: &LedConfig,
        inputs: &LedInputs,
        telemetry: Option<&Telemetry>,
        native: Option<&NativeLedSettings>,
    ) -> Result<SegmentFrame, LedError> {
        let segment_config = &config.segments[segment - 1];
        let signals = Signals { inputs, telemetry };
        let word = |c: NamedColor| config.byte_order.map(|order| BigInt::from(color_word(c.rgb(), order)));
        let pair = |even: NamedColor, odd: NamedColor| Some((word(even), word(odd)));
        let always_on = || -> Result<Option<(ColorValue, ColorValue)>, LedError> {
            let color = match native {
                Some(native) => native.always_on[segment - 1].clone()?,
                None => config.always_on_order.map(|order| BigInt::from(color_word(segment_config.always_on, order))),
            };
            Ok(Some((color.clone(), color)))
        };
        let count = match native {
            Some(native) => native.segment_count.clone()?,
            None => config.segment_count.into(),
        };
        if BigInt::from(segment) > count || inputs.tach_test {
            return Ok(SegmentFrame {
                start: 151.into(),
                end: 152.into(),
                lit: 0.into(),
                colors: pair(NamedColor::Black, NamedColor::Black),
            });
        }
        let (start, end, kind) = match native {
            Some(native) => {
                let native = &native.segments[segment - 1];
                (native.start.clone()?, native.end.clone()?, native.kind.clone()?)
            }
            None => (segment_config.start.into(), segment_config.end.into(), segment_config.kind),
        };
        let whole = &end - &start;
        match inputs.mode {
            LedMode::AlwaysOn => {
                return Ok(SegmentFrame { start, end, lit: whole, colors: always_on()? });
            }
            LedMode::Off => {
                return Ok(SegmentFrame { start, end, lit: whole, colors: pair(NamedColor::Black, NamedColor::Black) })
            }
            LedMode::Auto => {}
        }
        let blink_on = self.blink[segment - 1].on;
        let (lit, colors) = match effective_type(kind, &signals) {
            SegmentType::Rpm => {
                let lit = if inputs.shift_light && blink_on {
                    0.into()
                } else {
                    rpm_bar(&signals.value("rpm", inputs.rpm), &signals.value("max_rpm", inputs.max_rpm), &start, &end)
                };
                (lit, pair(NamedColor::Red, NamedColor::Red))
            }
            SegmentType::Speed => {
                let number = |key, fallback| -> Result<f64, LedError> {
                    signals.value(key, fallback).py_as_double().map_err(|e| NativeLedError::operation(e).into())
                };
                let speed = number("speed_kmh", inputs.speed_kmh)?;
                let maximum = number("max_speed_kmh", inputs.max_speed_kmh)?;
                (speed_bar(speed, maximum, &start, &end)?, pair(NamedColor::Blue, NamedColor::Blue))
            }
            SegmentType::Flag => {
                let count = match native {
                    Some(native) => native.led_count.clone()?,
                    None => config.led_count.into(),
                };
                let (even, odd) = flag_colors(&|code| signals.flag_is(code), blink_on);
                (count, pair(even, odd))
            }
            SegmentType::Off => (whole, pair(NamedColor::Black, NamedColor::Black)),
            SegmentType::SolidAlwaysOn => (whole, always_on()?),
            SegmentType::SpotterLeft => {
                let (even, odd) = spotter_colors(signals.spotter_left());
                (whole, pair(even, odd))
            }
            SegmentType::SpotterRight => {
                let (even, odd) = spotter_colors(signals.spotter_right());
                (whole, pair(even, odd))
            }
            _ => (0.into(), None),
        };
        self.blink[segment - 1].on = !blink_on;
        Ok(SegmentFrame { start, end, lit, colors })
    }
}

/// Spotter segment colours: yellow/orange alternating while the spotter is active, dark otherwise.
fn spotter_colors(active: bool) -> (NamedColor, NamedColor) {
    if active {
        (NamedColor::Yellow, NamedColor::Orange3)
    } else {
        (NamedColor::Black, NamedColor::Black)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(even: u32, odd: u32) -> Option<(ColorValue, ColorValue)> {
        Some((Some(even.into()), Some(odd.into())))
    }

    fn segment(kind: SegmentType, start: i64, end: i64) -> SegmentConfig {
        SegmentConfig { start, end, kind, always_on: [255, 0, 0] }
    }

    fn config() -> LedConfig {
        LedConfig {
            segment_count: 3,
            led_count: 150,
            brightness: 100,
            byte_order: Some(LedByteOrder::Rgb),
            always_on_order: Some(LedByteOrder::Rgb),
            segments: [
                segment(SegmentType::Rpm, 1, 50),
                segment(SegmentType::Speed, 51, 100),
                segment(SegmentType::Flag, 101, 125),
                segment(SegmentType::Flag, 126, 150),
            ],
        }
    }

    fn inputs() -> LedInputs {
        LedInputs {
            rpm: 4000.0,
            max_rpm: 8000.0,
            shift_light: false,
            speed_kmh: 75.0,
            max_speed_kmh: 150.0,
            flag: 0.0,
            spotter_left: 0.0,
            spotter_right: 0.0,
            mode: LedMode::Auto,
            tach_test: false,
        }
    }

    #[test]
    fn native_always_on_error_is_consumed_only_by_color_branches() {
        let config = config();
        let error = NativeLedError { class: "IndexError", message: "list index out of range".into() };
        let mut native = NativeLedSettings {
            always_on: std::array::from_fn(|_| Err(error.clone())),
            led_count: Ok((150).into()),
            segment_count: Ok(3.into()),
            ranges: NativeSegmentRanges::from_positions([1, 50, 51, 100, 101, 125, 126, 150]),
            segments: config.segments.map(|s| NativeSegmentSettings {
                start: Ok(s.start.into()),
                end: Ok(s.end.into()),
                kind: Ok(s.kind),
            }),
        };
        let mut strip = LedStrip::new();
        assert_eq!(
            strip.segment_frame_native(1, &config, &LedInputs { mode: LedMode::AlwaysOn, ..inputs() }, &native),
            Err(error.clone().into())
        );
        assert!(!strip.blink[0].on);
        assert!(strip.segment_frame_native(1, &config, &LedInputs { mode: LedMode::Off, ..inputs() }, &native).is_ok());
        assert!(strip
            .segment_frame_native(4, &config, &LedInputs { mode: LedMode::AlwaysOn, ..inputs() }, &native)
            .is_ok());
        assert!(strip
            .segment_frame_native(
                1,
                &config,
                &LedInputs { mode: LedMode::AlwaysOn, tach_test: true, ..inputs() },
                &native
            )
            .is_ok());
        for kind in [
            SegmentType::Rpm,
            SegmentType::Speed,
            SegmentType::Flag,
            SegmentType::Off,
            SegmentType::SpotterLeft,
            SegmentType::SpotterRight,
            SegmentType::Unknown,
        ] {
            native.segments[0].kind = Ok(kind);
            assert!(strip.segment_frame_native(1, &config, &inputs(), &native).is_ok());
        }
        native.segments[1].kind = Ok(SegmentType::SolidAlwaysOn);
        let phase = strip.blink.map(|b| b.on);
        assert_eq!(strip.segment_frame_native(2, &config, &inputs(), &native), Err(error.into()));
        assert_eq!(strip.blink.map(|b| b.on), phase);
        native.always_on[1] = Ok(Some(0x01000000.into()));
        assert_eq!(
            strip.segment_frame_native(2, &config, &inputs(), &native).unwrap().colors,
            words(0x01000000, 0x01000000)
        );
        assert!(strip.blink[1].on);
        native.always_on[1] = Ok(None);
        assert_eq!(strip.segment_frame_native(2, &config, &inputs(), &native).unwrap().colors, Some((None, None)));
        let wide = BigInt::from(1u8) << 100usize;
        native.always_on[1] = Ok(Some(wide.clone()));
        assert_eq!(
            strip.segment_frame_native(2, &config, &inputs(), &native).unwrap().colors,
            Some((Some(wide.clone()), Some(wide))),
            "a wide long_format() word reaches the packer unchanged"
        );
    }

    #[test]
    fn native_flag_count_is_read_only_in_reached_flag_branches() {
        let config = config();
        let error =
            NativeLedError { class: "ValueError", message: "invalid literal for int() with base 10: 'bad'".into() };
        let mut native = NativeLedSettings {
            always_on: config.segments.map(|s| Ok(Some(color_word(s.always_on, LedByteOrder::Rgb).into()))),
            led_count: Err(error.clone()),
            segment_count: Ok(3.into()),
            ranges: NativeSegmentRanges::from_positions([1, 50, 51, 100, 101, 125, 126, 150]),
            segments: config.segments.map(|s| NativeSegmentSettings {
                start: Ok(s.start.into()),
                end: Ok(s.end.into()),
                kind: Ok(s.kind),
            }),
        };
        for mode in [LedMode::Off, LedMode::AlwaysOn] {
            assert!(LedStrip::new().segment_frame_native(3, &config, &LedInputs { mode, ..inputs() }, &native).is_ok());
        }
        assert!(LedStrip::new().segment_frame_native(4, &config, &inputs(), &native).is_ok());
        assert!(LedStrip::new()
            .segment_frame_native(3, &config, &LedInputs { tach_test: true, ..inputs() }, &native)
            .is_ok());
        for kind in [
            SegmentType::Rpm,
            SegmentType::Speed,
            SegmentType::Off,
            SegmentType::SolidAlwaysOn,
            SegmentType::SpotterLeft,
            SegmentType::SpotterRight,
            SegmentType::Unknown,
        ] {
            native.segments[0].kind = Ok(kind);
            assert!(LedStrip::new().segment_frame_native(1, &config, &inputs(), &native).is_ok());
        }
        for (kind, reached) in [
            (SegmentType::Flag, inputs()),
            (SegmentType::RpmOrFlag, LedInputs { flag: 1.0, ..inputs() }),
            (SegmentType::SpeedOrFlag, LedInputs { flag: 1.0, ..inputs() }),
            (SegmentType::SpotterLeftOrFlag, inputs()),
            (SegmentType::SpotterRightOrFlag, inputs()),
        ] {
            native.segments[0].kind = Ok(kind);
            let mut strip = LedStrip::new();
            assert_eq!(strip.segment_frame_native(1, &config, &reached, &native), Err(error.clone().into()));
            assert!(!strip.blink[0].on, "failed flag lookup precedes phase publication");
        }
        for (kind, bypass) in [
            (SegmentType::RpmOrFlag, inputs()),
            (SegmentType::SpeedOrFlag, inputs()),
            (SegmentType::SpotterLeftOrFlag, LedInputs { spotter_left: 1.0, ..inputs() }),
            (SegmentType::SpotterRightOrFlag, LedInputs { spotter_right: 1.0, ..inputs() }),
        ] {
            native.segments[0].kind = Ok(kind);
            assert!(LedStrip::new().segment_frame_native(1, &config, &bypass, &native).is_ok());
        }
    }

    #[test]
    fn native_flag_count_failure_retains_prior_phase_then_recovers() {
        let config = config();
        let error = NativeLedError { class: "KeyError", message: "'nbr_leds'".into() };
        let mut native = NativeLedSettings {
            always_on: config.segments.map(|s| Ok(Some(color_word(s.always_on, LedByteOrder::Rgb).into()))),
            led_count: Err(error.clone()),
            segment_count: Ok(3.into()),
            ranges: NativeSegmentRanges::from_positions([1, 50, 51, 100, 101, 125, 126, 150]),
            segments: config.segments.map(|s| NativeSegmentSettings {
                start: Ok(s.start.into()),
                end: Ok(s.end.into()),
                kind: Ok(s.kind),
            }),
        };
        let mut strip = LedStrip::new();
        let input = LedInputs { shift_light: true, flag: 11.0, ..inputs() };
        strip.segment_frame_native(1, &config, &input, &native).unwrap();
        assert_eq!(strip.segment_frame_native(3, &config, &input, &native), Err(error.into()));
        assert_eq!(strip.blink.map(|b| b.on), [true, false, false, false]);
        native.led_count = Ok((-1).into());
        let recovered = strip.segment_frame_native(3, &config, &input, &native).unwrap();
        assert_eq!(recovered.lit, BigInt::from(-1));
        assert_eq!(recovered.colors, words(0, 0xffffff), "first successful phase is retained");
        assert_eq!(strip.blink.map(|b| b.on), [true, false, true, false]);
        assert_eq!(strip.segment_frame_native(1, &config, &input, &native).unwrap().lit, BigInt::from(0));
    }

    #[test]
    fn native_led_count_precedes_hidden_and_tach_gates() {
        let config = config();
        let mut native = NativeLedSettings {
            always_on: config.segments.map(|s| Ok(Some(color_word(s.always_on, LedByteOrder::Rgb).into()))),
            led_count: Ok((config.led_count).into()),
            ranges: NativeSegmentRanges::from_positions([1, 50, 51, 100, 101, 125, 126, 150]),
            segment_count: Ok(3.into()),
            segments: config.segments.map(|s| NativeSegmentSettings {
                start: Ok(s.start.into()),
                end: Ok(s.end.into()),
                kind: Ok(s.kind),
            }),
        };
        let error = NativeLedError { class: "KeyError", message: "'nbr_segments'".into() };
        native.segment_count = Err(error.clone());
        let mut strip = LedStrip::new();
        assert_eq!(strip.segment_frame_native(4, &config, &inputs(), &native), Err(error.clone().into()));
        assert_eq!(
            strip.segment_frame_native(1, &config, &LedInputs { tach_test: true, ..inputs() }, &native),
            Err(error.into())
        );
        native.segment_count = Ok((-1).into());
        native.segments[0].start = Err(NativeLedError { class: "KeyError", message: "'start'".into() });
        assert_eq!(strip.segment_frame_native(1, &config, &inputs(), &native).unwrap().start, BigInt::from(151));
        native.segment_count = Ok((3).into());
        assert_eq!(
            strip.segment_frame_native(1, &config, &LedInputs { tach_test: true, ..inputs() }, &native).unwrap().start,
            BigInt::from(151)
        );
    }

    #[test]
    fn native_led_geometry_and_required_type_follow_source_order() {
        let config = config();
        let error = |key| NativeLedError { class: "KeyError", message: format!("'{key}'") };
        let mut native = NativeLedSettings {
            always_on: config.segments.map(|s| Ok(Some(color_word(s.always_on, LedByteOrder::Rgb).into()))),
            led_count: Ok((config.led_count).into()),
            ranges: NativeSegmentRanges::from_positions([1, 50, 51, 100, 101, 125, 126, 150]),
            segment_count: Ok(3.into()),
            segments: config.segments.map(|s| NativeSegmentSettings {
                start: Ok(s.start.into()),
                end: Ok(s.end.into()),
                kind: Ok(s.kind),
            }),
        };
        let mut strip = LedStrip::new();
        native.segments[0] =
            NativeSegmentSettings { start: Err(error("start")), end: Err(error("end")), kind: Err(error("type")) };
        let off = LedInputs { mode: LedMode::Off, ..inputs() };
        assert_eq!(strip.segment_frame_native(1, &config, &off, &native), Err(error("start").into()));
        native.segments[0].start = Ok(7.into());
        assert_eq!(strip.segment_frame_native(1, &config, &off, &native), Err(error("end").into()));
        native.segments[0].end = Ok(12.into());
        assert_eq!(strip.segment_frame_native(1, &config, &off, &native), Err(error("type").into()));
        native.segments[0].kind = Ok(SegmentType::Unknown);
        let frame = strip.segment_frame_native(1, &config, &off, &native).unwrap();
        assert_eq!((frame.start, frame.end, frame.lit), (7.into(), 12.into(), 5.into()));
        native.segments[0].kind = Ok(SegmentType::SolidAlwaysOn);
        let frame = strip.segment_frame_native(1, &config, &inputs(), &native).unwrap();
        assert_eq!(frame.lit, BigInt::from(5));
        assert_eq!(frame.colors, words(0xFF0000, 0xFF0000));
        let wide = BigInt::from(1u8) << 80usize;
        native.segments[0].start = Ok(-wide.clone());
        native.segments[0].end = Ok(wide.clone());
        let frame = strip.segment_frame_native(1, &config, &off, &native).unwrap();
        assert_eq!(frame.lit, &wide * 2, "end - start is the exact Python integer");
        assert_eq!((frame.start, frame.end), (-wide.clone(), wide.clone()));
        // The rescale works in doubles: a speed bound beyond binary64 is an overflow error in the reached branch.
        native.segments[0].kind = Ok(SegmentType::Speed);
        native.segments[0].end = Ok(BigInt::from(1u8) << 1100usize);
        let error = strip.segment_frame_native(1, &config, &inputs(), &native).unwrap_err();
        assert_eq!(
            error,
            LedError::Native(NativeLedError {
                class: "OverflowError",
                message: "int too large to convert to float".into()
            })
        );
        // ... and inside the rpm branch it gives the zero bar.
        native.segments[0].kind = Ok(SegmentType::Rpm);
        assert_eq!(strip.segment_frame_native(1, &config, &inputs(), &native).unwrap().lit, BigInt::from(0));
    }

    #[test]
    fn native_led_later_failure_retains_earlier_blink_only() {
        let config = config();
        let mut native = NativeLedSettings {
            always_on: config.segments.map(|s| Ok(Some(color_word(s.always_on, LedByteOrder::Rgb).into()))),
            led_count: Ok((config.led_count).into()),
            ranges: NativeSegmentRanges::from_positions([1, 50, 51, 100, 101, 125, 126, 150]),
            segment_count: Ok(3.into()),
            segments: config.segments.map(|s| NativeSegmentSettings {
                start: Ok(s.start.into()),
                end: Ok(s.end.into()),
                kind: Ok(s.kind),
            }),
        };
        let mut strip = LedStrip::new();
        strip.segment_frame_native(1, &config, &inputs(), &native).unwrap();
        let error =
            NativeLedError { class: "ValueError", message: "invalid literal for int() with base 10: 'bad'".into() };
        native.segments[1].start = Err(error.clone());
        assert_eq!(strip.segment_frame_native(2, &config, &inputs(), &native), Err(error.into()));
        assert_eq!(strip.blink.map(|b| b.on), [true, false, false, false]);
        native.segments[1].start = Ok(51.into());
        strip.segment_frame_native(2, &config, &inputs(), &native).unwrap();
        assert_eq!(strip.blink.map(|b| b.on), [true, true, false, false]);
    }

    #[test]
    fn native_rpm_text_and_bytes_match_integer_bar_values() {
        use openrigcue_model::{native_value::LegacyValue, Telemetry};
        let expected = LedStrip::new().segment_frame(1, &config(), &inputs()).unwrap();
        for (rpm, maximum) in [
            (LegacyValue::Int(4000), LegacyValue::Str("8_000".into())),
            (LegacyValue::Str(" +4_000 ".into()), LegacyValue::Bytes(b"8000".to_vec())),
            (LegacyValue::Bytes(b"4000".to_vec()), LegacyValue::Int(8000)),
            (LegacyValue::Int(4000), LegacyValue::Str("\u{a0}+٨_٠٠٠\u{2007}".into())),
            (LegacyValue::Int(4000), LegacyValue::Str(format!("{}8000", "0".repeat(4296)))),
        ] {
            let mut telemetry = Telemetry::off();
            telemetry.set_native("rpm", rpm);
            telemetry.set_native("max_rpm", maximum);
            let input = LedInputs { rpm: telemetry.rpm, max_rpm: telemetry.max_rpm, ..inputs() };
            let actual = LedStrip::new().segment_frame_for_telemetry(1, &config(), &input, &telemetry, None).unwrap();
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn native_rpm_conversion_errors_keep_the_zero_bar_and_its_color() {
        let mut telemetry = Telemetry::off();
        telemetry.set_native("rpm", LegacyValue::Int(4000));
        let expected = SegmentFrame::words(1, 50, 0, Some((0xff0000, 0xff0000)));
        for maximum in [
            LegacyValue::None,
            LegacyValue::Str("8__000".into()),
            LegacyValue::Bytes("٨٠٠٠".as_bytes().to_vec()),
            LegacyValue::Float(f64::NAN),
            LegacyValue::Float(f64::INFINITY),
            LegacyValue::Str(format!("{}8000", "0".repeat(4297))),
        ] {
            telemetry.set_native("max_rpm", maximum);
            let input = LedInputs { rpm: telemetry.rpm, max_rpm: telemetry.max_rpm, ..inputs() };
            assert_eq!(
                LedStrip::new().segment_frame_for_telemetry(1, &config(), &input, &telemetry, None).unwrap(),
                expected
            );
        }
    }

    /// Colour words for RGB and GRB strips; colour table values.
    #[test]
    fn colour_words() {
        assert_eq!(color_word(NamedColor::Orange3.rgb(), LedByteOrder::Rgb), 0xCD8500);
        assert_eq!(color_word(NamedColor::Orange3.rgb(), LedByteOrder::Grb), 0x85CD00);
        assert_eq!(color_word(NamedColor::Green.rgb(), LedByteOrder::Rgb), 0x008000);
        assert_eq!(color_word(NamedColor::Gray17.rgb(), LedByteOrder::Rgb), 0x2B2B2B);
    }

    /// 'RGB' and 'GRB' give a word, any other model `None`.
    #[test]
    fn byte_order_of_the_model() {
        assert_eq!(LedByteOrder::from_model("RGB"), Some(LedByteOrder::Rgb));
        assert_eq!(LedByteOrder::from_model("GRB"), Some(LedByteOrder::Grb));
        assert_eq!(LedByteOrder::from_model("rgb"), None);
        assert_eq!(LedByteOrder::from_model(""), None);
    }

    /// The named colours use the cached byte order,
    /// the "Always On" colour the order at the time of the frame; a model without a word gives
    /// null colour values (which the packer rejects), not missing colours.
    #[test]
    fn named_and_always_on_orders() {
        let mut strip = LedStrip::new();
        let mut c = config();
        c.always_on_order = Some(LedByteOrder::Grb);
        c.segments[0].always_on = [255, 128, 0];
        let mut i = inputs();
        i.mode = LedMode::AlwaysOn;
        assert_eq!(strip.segment_frame(1, &c, &i).unwrap().colors, words(0x80FF00, 0x80FF00));
        i.mode = LedMode::Auto;
        assert_eq!(strip.segment_frame(1, &c, &i).unwrap().colors, words(0xFF0000, 0xFF0000));
        c.byte_order = None;
        assert_eq!(strip.segment_frame(1, &c, &i).unwrap().colors, Some((None, None)));
        assert_eq!(strip.segment_frame(4, &c, &i).unwrap().colors, Some((None, None)));
        c.always_on_order = None;
        i.mode = LedMode::AlwaysOn;
        assert_eq!(strip.segment_frame(1, &c, &i).unwrap().colors, Some((None, None)));
    }

    /// 'R' and 'S' bars.
    #[test]
    fn clamped_led_maps_zero_rpm_maximum_selects_the_upper_bar() {
        let mut input = inputs();
        input.max_rpm = 0.0;
        assert_eq!(LedStrip::new().segment_frame(1, &config(), &input).unwrap().lit, BigInt::from(49));
        input.rpm = 0.0;
        assert_eq!(LedStrip::new().segment_frame(1, &config(), &input).unwrap().lit, BigInt::from(49));
    }

    #[test]
    fn clamped_led_maps_zero_speed_maximum_selects_the_upper_bar() {
        let mut input = inputs();
        input.max_speed_kmh = 0.0;
        assert_eq!(LedStrip::new().segment_frame(2, &config(), &input).unwrap().lit, BigInt::from(49));
        input.speed_kmh = f64::NAN;
        assert_eq!(LedStrip::new().segment_frame(2, &config(), &input).unwrap().lit, BigInt::from(49));
    }

    #[test]
    fn clamped_led_maps_limit_numeric_values_before_bar_rounding() {
        let mut input = inputs();
        input.rpm = -4000.0;
        input.speed_kmh = -75.0;
        let mut strip = LedStrip::new();
        assert_eq!(strip.segment_frame(1, &config(), &input).unwrap().lit, BigInt::from(0));
        assert_eq!(strip.segment_frame(2, &config(), &input).unwrap().lit, BigInt::from(0));
        input.rpm = 16000.0;
        input.speed_kmh = 300.0;
        assert_eq!(strip.segment_frame(1, &config(), &input).unwrap().lit, BigInt::from(49));
        assert_eq!(strip.segment_frame(2, &config(), &input).unwrap().lit, BigInt::from(49));
    }

    #[test]
    fn rpm_and_speed_bars() {
        let mut strip = LedStrip::new();
        let rpm = strip.segment_frame(1, &config(), &inputs()).unwrap();
        // 4000 of 0..8000 in 1..50 is 25.5, truncated to 25; minus 1 = 24
        assert_eq!(rpm, SegmentFrame::words(1, 50, 24, Some((0xFF0000, 0xFF0000))));
        let speed = strip.segment_frame(2, &config(), &inputs()).unwrap();
        // 75 of 0..150 in 51..100 is 75.5, rounded to 76; minus 51
        assert_eq!(speed.lit, BigInt::from(25));
        assert_eq!(speed.colors, words(0x0000FF, 0x0000FF));
        let mut i = inputs();
        i.max_rpm = 0.0;
        assert_eq!(strip.segment_frame(1, &config(), &i).unwrap().lit, BigInt::from(49));
        i.max_speed_kmh = 0.0;
        assert_eq!(strip.segment_frame(2, &config(), &i).unwrap().lit, BigInt::from(49));
    }

    /// Shift light: the rpm bar goes dark on alternate frames.
    #[test]
    fn shift_light_blinks_rpm_bar() {
        let mut strip = LedStrip::new();
        let mut i = inputs();
        i.shift_light = true;
        let lit: Vec<BigInt> = (0..4).map(|_| strip.segment_frame(1, &config(), &i).unwrap().lit).collect();
        assert_eq!(lit, [24, 0, 24, 0].map(BigInt::from).to_vec());
    }

    /// The flag table, including the blinking codes and the whole-strip LED count.
    #[test]
    fn flag_colours() {
        let mut strip = LedStrip::new();
        let mut i = inputs();
        let mut colors = |flag: f64, strip: &mut LedStrip| {
            i.flag = flag;
            strip.segment_frame(3, &config(), &i).unwrap()
        };
        let green = colors(1.0, &mut strip);
        assert_eq!(green.lit, BigInt::from(150));
        assert_eq!(green.colors, words(0x008000, 0x008000));
        assert_eq!(colors(36.0, &mut strip).colors, words(0xFFFF00, 0xFFFF00));
        assert_eq!(colors(9.0, &mut strip).colors, words(0xCD8500, 0x2B2B2B));
        assert_eq!(colors(35.0, &mut strip).colors, words(0x000000, 0xFF0000));
        assert_eq!(colors(4.0, &mut strip).colors, words(0, 0));
        // Blinking white: the phase toggles on every auto frame of this segment.
        let mut strip = LedStrip::new();
        assert_eq!(colors(11.0, &mut strip).colors, words(0x000000, 0xFFFFFF));
        assert_eq!(colors(11.0, &mut strip).colors, words(0xFFFFFF, 0x000000));
        assert_eq!(colors(20.0, &mut strip).colors, words(0x000000, 0xFF0000));
        assert_eq!(colors(15.0, &mut strip).colors, words(0x008000, 0xFFFF00));
    }

    /// Switch modes, combined types, spotters and hidden segments.
    #[test]
    fn modes_and_combined_types() {
        let mut strip = LedStrip::new();
        let mut cfg = config();
        let mut i = inputs();
        i.mode = LedMode::AlwaysOn;
        assert_eq!(
            strip.segment_frame(1, &cfg, &i).unwrap(),
            SegmentFrame::words(1, 50, 49, Some((0xFF0000, 0xFF0000)))
        );
        i.mode = LedMode::Off;
        assert_eq!(strip.segment_frame(1, &cfg, &i).unwrap().colors, words(0, 0));
        assert_eq!(strip.segment_frame(4, &cfg, &i).unwrap(), SegmentFrame::words(151, 152, 0, Some((0, 0))));
        i.mode = LedMode::Auto;
        cfg.segments[0].kind = SegmentType::RpmOrFlag;
        i.flag = 5.0;
        assert_eq!(strip.segment_frame(1, &cfg, &i).unwrap().lit, BigInt::from(150));
        cfg.segments[0].kind = SegmentType::SpotterLeftOrFlag;
        i.spotter_left = 1.0;
        assert_eq!(strip.segment_frame(1, &cfg, &i).unwrap().colors, words(0xFFFF00, 0xCD8500));
        cfg.segments[0].kind = SegmentType::Unknown;
        assert_eq!(strip.segment_frame(1, &cfg, &i).unwrap().colors, None);
        i.tach_test = true;
        assert_eq!(strip.segment_frame(1, &cfg, &i).unwrap().start, BigInt::from(151));
        assert_eq!(SegmentType::from_code("SLF"), SegmentType::SpotterLeftOrFlag);
    }
}
