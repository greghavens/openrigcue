//! iFlag: an 8x8 RGB LED panel showing race flags, spotter arrows, the gear and an rpm bar.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use openrigcue_model::native_value::{BigInt, LegacyValue};
use tracing::debug;

use super::link::{Clock, PortLister, PortOpener, PortSettings, SerialLink};
use super::numeric::{map_clamped, truncate, PyInt};
use super::port_scan::{AllowedPorts, ClaimedPorts, Device, Exclusions, ScanRules};
use super::status::PublishedParameter;

/// One 8x8 picture of colour codes.
pub type Matrix = [[u8; 8]; 8];

/// Panel colour codes used by the driver.
pub mod color {
    pub const BLACK: u8 = 0;
    pub const WHITE: u8 = 1;
    pub const RED: u8 = 2;
    pub const GREEN: u8 = 3;
    pub const BLUE: u8 = 4;
    pub const YELLOW: u8 = 5;
    pub const ORANGE: u8 = 8;
}

/// Frame toggle periods (fast 3, medium 5, slow 7, in units of 100 ms).
const FAST: u32 = 3;
const SLOW: u32 = 7;

/// A flag definition.
#[derive(Debug, Clone, Copy)]
struct FlagDef {
    name: &'static str,
    pattern: &'static str,
    colors: &'static [u8],
    speed: u32,
}

/// The flag table by code (code 36 is "spotter_left").
fn flag_definition(code: f64) -> Option<FlagDef> {
    use color::*;
    let def = |name, pattern, colors, speed| Some(FlagDef { name, pattern, colors, speed });
    let code = (code.fract() == 0.0).then_some(code as i64)?;
    match code {
        0 => def("none", "simple_flag", &[BLACK, BLACK], FAST),
        1 => def("green", "simple_flag", &[GREEN, GREEN], FAST),
        2 => def("blue", "diagonal_stripe_flag", &[BLUE, YELLOW], SLOW),
        3 => def("white", "simple_flag", &[WHITE, WHITE], SLOW),
        5 => def("red", "flashing_flag", &[BLACK, RED], SLOW),
        6 => def("yellow", "flashing_flag", &[BLACK, YELLOW], FAST),
        7 => def("yellow_waving", "waving_flag", &[BLACK, YELLOW], FAST),
        9 => def("repair", "meatball_flag", &[BLACK, ORANGE], SLOW),
        10 => def("black", "inverted_flag", &[BLACK, WHITE], SLOW),
        11 => def("checkered", "checkered_flag", &[BLACK, WHITE], SLOW),
        20 => def("start_ready", "half_flag", &[RED, BLACK], FAST),
        21 => def("start_set", "simple_flag", &[RED, RED], SLOW),
        22 => def("start_go", "simple_flag", &[GREEN, GREEN], FAST),
        30 => def("disqualify", "crossed_flag", &[BLACK, WHITE], FAST),
        31 => def("debris", "stripped_flag", &[YELLOW, RED], FAST),
        32 => def("one_lap_to_green", "inverted_flag", &[BLACK, GREEN], SLOW),
        34 => def("caution_waving", "safetycar_flag", &[BLACK, YELLOW, BLACK], SLOW),
        35 => def("furled", "furled_flag", &[BLACK, WHITE, BLACK], FAST),
        36 => def("spotter_left", "arrow_left", &[BLACK, RED, YELLOW], FAST),
        37 => def("spotter_right", "arrow_right", &[BLACK, RED, YELLOW], FAST),
        38 => def("rpm", "simple_flag", &[BLACK, BLACK], FAST),
        39 => def("gear_display", "simple_flag", &[BLACK, BLACK], FAST),
        40 => def("sli", "simple_flag", &[BLACK, BLACK], FAST),
        41 => def("spotter_both", "arrow_dual", &[BLACK, RED, YELLOW], FAST),
        50 => def("pit_hold", "square_flag", &[BLACK, RED], FAST),
        51 => def("pit_release", "square_flag", &[BLACK, BLUE], FAST),
        52 => def("pit_done", "square_flag", &[BLACK, GREEN], FAST),
        53 => def("pit_limit", "circle_flag", &[BLACK, RED, BLACK], FAST),
        _ => None,
    }
}

/// Flag pattern sources (8 or 16 rows; 16 rows hold two animation frames).
fn pattern_source(name: &str) -> Option<&'static str> {
    Some(match name {
        "simple_flag" => "00000000 00000000 00000000 00011000 00011000 00000000 00000000 00000000",
        "square_flag" => "00000000 00000000 00111100 00111100 00111100 00111100 00000000 00000000",
        "half_flag" => "00000000 00000000 00000000 00000000 11111111 11111111 11111111 11111111",
        "furled_flag" => "11111111 01222221 00122221 00012221 00001221 00000121 00000011 00000001 00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000",
        "arrow_right" => "00001222 00001122 00001112 00001111 00001112 00001122 00001222 00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000",
        "arrow_left" => "22210000 22110000 21110000 11110000 21110000 22110000 22210000 00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000",
        "arrow_dual" => "22200222 22100122 21100112 11100111 21100112 22100122 22200222 00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000",
        "inverted_flag" => "11111111 10000001 10000001 10000001 10000001 10000001 10000001 11111111 00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000",
        "flashing_flag" => "11111111 11111111 11111111 11111111 11111111 11111111 11111111 11111111 00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000",
        "waving_flag" => "00001111 00001111 00001111 00001111 00001111 00001111 00001111 00001111 11110000 11110000 11110000 11110000 11110000 11110000 11110000 11110000",
        "checkered_flag" => "11001100 11001100 00110011 00110011 11001100 11001100 00110011 00110011 00110011 00110011 11001100 11001100 00110011 00110011 11001100 11001100",
        "stripped_flag" => "11001100 11001100 11001100 11001100 11001100 11001100 11001100 11001100 00110011 00110011 00110011 00110011 00110011 00110011 00110011 00110011",
        "circle_flag" => "00111100 01122110 11222211 12222221 12222221 11222211 01122110 00111100",
        "crossed_flag" => "11000011 11100111 01111110 00111100 00111100 01111110 11100111 11000011",
        "diagonal_stripe_flag" => "00000011 00000111 00001110 00011100 00111000 01110000 11100000 11000000",
        "meatball_flag" => "00000000 00011000 00111100 01111110 01111110 00111100 00011000 00000000 00000000 00000000 00011000 00111100 00111100 00011000 00000000 00000000",
        "safetycar_flag" => "01110222 11112222 11002200 11102200 01112200 00112200 11112222 11100222 02220111 22221111 22001100 22201100 02221100 00221100 22221111 22200111",
        _ => return None,
    })
}

/// Gear glyph sources (gears -1..11), before the row order is reversed.
fn gear_source(gear: i64) -> Option<&'static str> {
    Some(match gear {
        -1 => "00000000 01111100 01100110 01100110 01111100 01111000 01101100 01100110",
        0 => "00000000 01100011 01110011 01111011 01101111 01100111 01100011 01100011",
        1 => "00000000 00011000 00011000 00111000 00011000 00011000 00011000 01111110",
        2 => "00000000 00111100 01100110 00000110 00001100 00110000 01100000 01111110",
        3 => "00000000 00111100 01100110 00000110 00011100 00000110 01100110 00111100",
        4 => "00000000 00001100 00011100 00101100 01001100 01111110 00001100 00001100",
        5 => "00000000 01111110 01100000 01111100 00000110 00000110 01100110 00111100",
        6 => "00000000 00111100 01100110 01100000 01111100 01100110 01100110 00111100",
        7 => "00000000 01111110 01100110 00001100 00001100 00011000 00011000 00011000",
        8 => "00000000 00111100 01100110 01100110 00111100 01100110 01100110 00111100",
        9 => "00000000 00111100 01100110 01100110 00111110 00000110 01100110 00111100",
        10 => "00000000 01100111 01100101 11100101 01100101 01100101 01100101 01100111",
        11 => "00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000",
        _ => return None,
    })
}

/// Parses a pattern: one row of 8 digits per space-separated word.
fn parse_rows(source: &str) -> Vec<[u8; 8]> {
    source.split(' ').map(|word| std::array::from_fn(|i| word.as_bytes()[i] - b'0')).collect()
}

/// Two quarter turns clockwise followed by a left-right mirror: the two turns put the picture
/// upside down and the mirror undoes the left-right half of that. The result is the row order reversed,
/// each row unchanged: the source's blank first row ends up as the bottom row.
fn mirrored(rows: Vec<[u8; 8]>) -> Vec<[u8; 8]> {
    rows.into_iter().rev().collect()
}

fn to_matrix(rows: &[[u8; 8]]) -> Matrix {
    std::array::from_fn(|i| rows[i])
}

/// The flag pattern rows.
fn pattern(name: &str) -> Option<Vec<[u8; 8]>> {
    Some(parse_rows(pattern_source(name)?))
}

/// Glyph lookup uses the gear's exact text form: a float 3.0 and a boolean true have no glyph.
/// Listeners that report plain numbers use the numeric interpretation.
fn gear_key(gear: f64, native: Option<&LegacyValue>) -> Result<i64, IFlagError> {
    // The text form of the raw value: the decimal digit limit applies there (before the glyph lookup).
    let text = match native {
        None => gear.to_string(),
        Some(value) => {
            let text = openrigcue_model::python_repr::str_native(value).map_err(|error| {
                let error = super::values::operation(error);
                IFlagError::NativeOutput(super::hapticr::NativeEngineError {
                    class: error.class,
                    message: error.message,
                })
            })?;
            // A lone surrogate never equals a decimal key.
            text.scalar_string().unwrap_or_else(|| "\u{fffd}".into())
        }
    };
    text.parse::<i64>()
        .ok()
        .filter(|key| key.to_string() == text && gear_source(*key).is_some())
        .ok_or(IFlagError::NoGlyph(text))
}

/// Colour of rpm LED `i` of the bottom row.
const RPM_COLORS: [u8; 8] =
    [color::GREEN, color::GREEN, color::GREEN, color::YELLOW, color::YELLOW, color::YELLOW, color::RED, color::RED];

/// Flags whose picture shows the gear digit on top.
const GEAR_OVERLAY_FLAGS: [i64; 7] = [1, 3, 6, 7, 10, 32, 34];

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IFlagError {
    #[error(transparent)]
    NativeOutput(#[from] super::hapticr::NativeEngineError),
    #[error(transparent)]
    NativeControl(super::hapticr::NativeEngineError),
    #[error("gear {0} has no glyph")]
    NoGlyph(String),
    #[error("max rpm is zero")]
    ZeroMaxRpm,
    #[error("pattern {0} is missing")]
    NoPattern(&'static str),
    #[error("pattern digit has no colour")]
    NoColor,
    /// A NaN or an infinity cannot be converted to an integer.
    #[error("{0} is not a finite number")]
    NotFinite(&'static str),
}

/// Picture builder: the gear glyphs, whose bottom row the gear frame overwrites in place.
#[derive(Debug, Clone)]
pub struct Pictures {
    glyphs: HashMap<i64, Matrix>,
}

impl Default for Pictures {
    fn default() -> Self {
        let glyphs = (-1..=11).map(|g| (g, to_matrix(&mirrored(parse_rows(gear_source(g).expect("glyph")))))).collect();
        Pictures { glyphs }
    }
}

/// What the gear frame shows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GearFrame {
    pub gear: f64,
    /// 0..8 LEDs of the rpm bar.
    pub rpm_leds: i64,
    pub shift_light: bool,
}

impl Pictures {
    /// The gear digit with the rpm bar (or the blinking red shift light) as bottom row.
    /// The bar is written into the shared glyph.
    pub fn gear_frame(&mut self, frame: u8, data: &GearFrame) -> Result<Matrix, IFlagError> {
        self.gear_frame_native(frame, data, None)
    }

    pub fn gear_frame_native(
        &mut self,
        frame: u8,
        data: &GearFrame,
        native: Option<&LegacyValue>,
    ) -> Result<Matrix, IFlagError> {
        let key = gear_key(data.gear, native)?;
        let line: [u8; 8] = if data.shift_light {
            [if frame == 0 { color::RED } else { color::BLACK }; 8]
        } else {
            std::array::from_fn(|i| if (i as i64) < data.rpm_leds { RPM_COLORS[i] } else { color::BLACK })
        };
        let glyph = self.glyphs.get_mut(&key).expect("checked key");
        glyph[7] = line;
        Ok(*glyph)
    }

    /// A flag picture (frame 0 or 1 of an animated pattern) and its toggle period in 100 ms units; the gear
    /// digit is drawn on top of green, white, yellow, waving yellow, black, one-to-green and caution flags.
    /// Unknown flag codes show flag 0.
    pub fn flag(&self, frame: u8, flag: f64, gear: f64) -> Result<(Matrix, u32), IFlagError> {
        self.flag_native(frame, flag, gear, None)
    }

    pub fn flag_native(
        &self,
        frame: u8,
        flag: f64,
        gear: f64,
        native: Option<&LegacyValue>,
    ) -> Result<(Matrix, u32), IFlagError> {
        let def = flag_definition(flag).unwrap_or_else(|| flag_definition(0.0).expect("flag 0"));
        let rows = pattern(def.pattern).ok_or(IFlagError::NoPattern(def.pattern))?;
        let rows = if rows.len() >= 9 {
            if frame == 1 {
                &rows[8..]
            } else {
                &rows[..8]
            }
        } else {
            &rows[..]
        };
        let mut picture: Matrix = [[0; 8]; 8];
        for (out, row) in picture.iter_mut().zip(rows) {
            for (cell, digit) in out.iter_mut().zip(row) {
                *cell = *def.colors.get(usize::from(*digit)).ok_or(IFlagError::NoColor)?;
            }
        }
        let flag_code = (flag.fract() == 0.0).then_some(flag as i64);
        if gear != 11.0 && flag_code.is_some_and(|c| GEAR_OVERLAY_FLAGS.contains(&c)) {
            let key = gear_key(gear, native)?;
            let digit = match flag_code {
                Some(1) => color::BLUE,
                Some(3) => color::RED,
                _ => color::WHITE,
            };
            for (out, row) in picture.iter_mut().zip(&self.glyphs[&key]) {
                for (cell, g) in out.iter_mut().zip(row) {
                    if *g == 1 {
                        *cell = digit;
                    }
                }
            }
        }
        Ok((picture, def.speed))
    }
}

/// Rotates a picture 90 degrees clockwise.
fn rotate_clockwise(image: &Matrix) -> Matrix {
    let mut rotated = [[0u8; 8]; 8];
    for (i, row) in image.iter().enumerate() {
        for (j, v) in row.iter().enumerate() {
            rotated[j][7 - i] = *v;
        }
    }
    rotated
}

/// The full-image-and-draw command: mirror (optional), rotate, two pixels per byte (even pixel in the low
/// nibble), `FF 10 <32 bytes>`.
pub fn picture_command(matrix: &Matrix, mirror: bool, rotation_quarters: u32) -> Vec<u8> {
    let mut image = *matrix;
    if mirror {
        for row in image.iter_mut() {
            row.reverse();
        }
    }
    for _ in 0..rotation_quarters {
        image = rotate_clockwise(&image);
    }
    let mut command = vec![0xFF, 16];
    for row in &image {
        for pair in row.chunks(2) {
            command.push((pair[0] & 0x0F) | ((pair[1] & 0x0F) << 4));
        }
    }
    command
}

/// The luma (brightness) command: `FF FF A2 <value> 00 00 00 00`.
pub fn luma_command(value: u8) -> Vec<u8> {
    vec![0xFF, 0xFF, 0xA2, value, 0, 0, 0, 0]
}

/// Firmware version text from the panel's pings, shared with the receive thread.
#[derive(Debug, Default)]
struct PingState {
    firmware: Mutex<String>,
    version_parameter: Option<PublishedParameter<LegacyValue>>,
    stop: AtomicBool,
}

pub(super) fn firmware_version(text: &str) -> LegacyValue {
    openrigcue_model::python_numeric::parse_py_float(text).map_or(LegacyValue::Int(0), LegacyValue::Float)
}

impl PingState {
    fn accept_firmware(&self, version: String) {
        *self.firmware.lock().unwrap() = version.clone();
        if let Some(parameter) = &self.version_parameter {
            parameter.set(firmware_version(&version));
        }
    }
}

/// Parse a ping (`FF FF <major> <minor> B0 D2 00 00`); `Some("<major>.<minor>")` for an iFlag.
fn parse_ping(values: &[PyInt]) -> Option<String> {
    (values[4].equals(176) && values[5].equals(210)).then(|| format!("{}.{}", values[2], values[3]))
}

/// Receive-thread step: append the ASCII decimal lines of `bytes`, drop one leading value while the stream
/// does not start with `255 255`, and consume one ping when 8 values are there.
fn ingest_pings(pending: &mut Vec<PyInt>, bytes: &[u8]) -> Option<String> {
    // Only non-empty lines made of the digits 0-9 alone are values; any other line (a sign, a space, a
    // letter, a byte that is not ASCII) is skipped and the rest of the read is kept. The lossy decoding
    // turns bytes that are not text into a non-digit character, so such a line is skipped like the others.
    let text = String::from_utf8_lossy(bytes);
    let values: Option<Vec<PyInt>> = text
        .split("\r\n")
        .filter(|l| !l.is_empty() && l.bytes().all(|c| c.is_ascii_digit()))
        .map(super::numeric::python_int_value)
        .collect();
    pending.extend(values?);
    if pending.len() >= 2 && (!pending[0].equals(255) || !pending[1].equals(255)) {
        pending.remove(0);
    }
    if pending.len() >= 8 {
        let ping: Vec<PyInt> = pending.drain(..8).collect();
        return parse_ping(&ping);
    }
    None
}

const POLL: Duration = Duration::from_millis(300);

fn receive_loop(link: &mut dyn SerialLink, state: &PingState, clock: &dyn Clock) {
    let mut pending = Vec::new();
    while !state.stop.load(Ordering::SeqCst) {
        clock.sleep(POLL);
        match link.read_available() {
            Ok(bytes) => {
                if let Some(version) = ingest_pings(&mut pending, &bytes) {
                    state.accept_firmware(version);
                }
            }
            Err(e) => debug!("connect - failed {e}"),
        }
    }
}

/// An open panel with its receive thread.
pub struct IFlagPanel {
    link: Box<dyn SerialLink>,
    state: Arc<PingState>,
    receiver: Option<JoinHandle<()>>,
    rotation_quarters: u32,
}

impl IFlagPanel {
    /// Start the receive thread on `link`.
    pub fn start(link: Box<dyn SerialLink>, clock: Arc<dyn Clock>) -> std::io::Result<Self> {
        Self::start_with_version(link, clock, None)
    }

    fn start_with_version(
        link: Box<dyn SerialLink>,
        clock: Arc<dyn Clock>,
        version_parameter: Option<PublishedParameter<LegacyValue>>,
    ) -> std::io::Result<Self> {
        let state = Arc::new(PingState { version_parameter, ..Default::default() });
        let mut reader = link.try_clone()?;
        let rx = Arc::clone(&state);
        let receiver = std::thread::Builder::new()
            .name("iflag_recv".into())
            .spawn(move || receive_loop(reader.as_mut(), &rx, clock.as_ref()))?;
        Ok(IFlagPanel { link, state, receiver: Some(receiver), rotation_quarters: 0 })
    }

    /// The firmware version reported by the pings ('' until the first one).
    pub fn firmware(&self) -> String {
        self.state.firmware.lock().unwrap().clone()
    }

    fn write(&mut self, bytes: &[u8]) -> bool {
        match self.link.write(bytes) {
            Ok(n) => n == bytes.len(),
            Err(e) => {
                debug!("error writing serial {e}");
                false
            }
        }
    }

    /// Sets the rotation, rounded down to a multiple of 90; each picture is then turned `rotation / 90`
    /// times (none for negative values, four is a full turn), for an integer of any width.
    fn set_rotation(&mut self, degrees: &BigInt) {
        let zero = BigInt::from(0);
        self.rotation_quarters =
            if *degrees < zero { 0 } else { u32::try_from((degrees / 90u32) % 4u32).expect("remainder of four") };
    }
}

impl Drop for IFlagPanel {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.receiver.take() {
            let _ = handle.join();
        }
    }
}

/// iFlag settings (`IFLAG` section and the tune's `iflag` switches).
#[derive(Debug, Clone, PartialEq)]
pub struct IFlagSettings {
    /// `IFLAG.rotation` (0, 90, 180, 270).
    pub rotation: i64,
    /// `IFLAG.brightness` (0..100).
    pub brightness: i64,
    pub native_output: Option<NativeIFlagOutput>,
    pub native_controls: Option<NativeIFlagControls>,
    /// Present raw flag keys; only a boolean false disables a stored key.
    pub native_flags: Option<HashMap<String, bool>>,
    pub mirror: bool,
    pub gear_display: bool,
    pub rpm: bool,
    pub sli: bool,
    pub spotter_left: bool,
    pub spotter_right: bool,
    /// Per-flag switches by flag name; a missing name hides the flag.
    pub flags: HashMap<String, bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeIFlagOutput {
    /// `IFLAG.rotation` and `IFLAG.brightness` as integers of any width.
    pub rotation: Result<BigInt, super::hapticr::NativeEngineError>,
    pub brightness: Result<BigInt, super::hapticr::NativeEngineError>,
    pub mirror: Result<bool, super::hapticr::NativeEngineError>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeIFlagControls {
    pub gear_display: bool,
    pub rpm: bool,
    pub sli: Result<bool, super::hapticr::NativeEngineError>,
    pub spotter_left: bool,
    pub spotter_right: bool,
}

/// Runtime inputs of one frame.
#[derive(Debug, Clone, PartialEq)]
pub struct IFlagInputs {
    pub gear: f64,
    pub native_gear: Option<LegacyValue>,
    pub rpm: f64,
    pub max_rpm: f64,
    pub flag: f64,
    pub spotter_left: f64,
    pub spotter_right: f64,
    /// The shift-light indicator value.
    pub shift_light: f64,
    /// `iflag_test_mode == 1`.
    pub test: bool,
    /// `iflag_switch == 1`.
    pub on: bool,
    /// The raw telemetry values, compared and divided as they are (`None`: no telemetry key).
    pub native: Option<NativeIFlagTelemetry>,
}

/// The raw telemetry values the stream reads: rpm, max rpm, flag and the two spotters.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NativeIFlagTelemetry {
    pub rpm: Option<LegacyValue>,
    pub max_rpm: Option<LegacyValue>,
    pub flag: Option<LegacyValue>,
    pub spotter_left: Option<LegacyValue>,
    pub spotter_right: Option<LegacyValue>,
}

/// Animation and change-detection state of the stream.
#[derive(Debug)]
pub struct IFlagStream {
    pictures: Pictures,
    previous_rotation: BigInt,
    previous_brightness: BigInt,
    /// Local wall-clock time of the last frame toggle (the creation time at first).
    previous_toggle: chrono::NaiveDateTime,
    frame: u8,
}

impl IFlagStream {
    pub fn new(now: chrono::NaiveDateTime) -> Self {
        IFlagStream {
            pictures: Pictures::default(),
            previous_rotation: BigInt::from(-1),
            previous_brightness: BigInt::from(-1),
            previous_toggle: now,
            frame: 0,
        }
    }

    /// Whether more than `period` milliseconds passed since the last toggle.
    fn toggle_after(&mut self, now: chrono::NaiveDateTime, period_ms: f64) {
        if super::link::total_seconds(now, self.previous_toggle) * 1000.0 > period_ms {
            self.frame = 1 - self.frame;
            self.previous_toggle = now;
        }
    }

    /// Draw one frame on the panel.
    ///
    /// For a connected panel: a 1 s pause while the panel has
    /// reported no firmware text yet, then return; nothing on invalid firmware text or 1.3 or older.
    /// Orientation and brightness (0..100 -> luma 0..150) on change; then the test picture, black when
    /// switched off, or (in priority order) the shift light, spotter arrows, the
    /// game flag (if its switch is on) or the gear digit with the rpm bar, toggling animation frames every
    /// `speed * 100` ms (200 ms for the gear frame).
    pub fn draw(
        &mut self,
        panel: &mut IFlagPanel,
        settings: &IFlagSettings,
        inputs: &IFlagInputs,
        clock: &dyn Clock,
    ) -> Result<(), IFlagError> {
        self.draw_live(panel, settings, inputs, clock, None)
    }

    /// [`Self::draw`] whose reads after the luma write (the rotation, the
    /// tune's `iflag` switches and the mirror setting) see the configuration as `reread` gives it then: the
    /// serial write is where another thread's change becomes visible.
    pub fn draw_live(
        &mut self,
        panel: &mut IFlagPanel,
        settings: &IFlagSettings,
        inputs: &IFlagInputs,
        clock: &dyn Clock,
        reread: Option<&dyn Fn() -> IFlagSettings>,
    ) -> Result<(), IFlagError> {
        let mut fresh = None;
        let firmware = panel.firmware();
        if firmware.is_empty() {
            clock.sleep(Duration::from_secs(1));
            return Ok(());
        }
        let Some(version) = openrigcue_model::python_numeric::parse_py_float(&firmware) else {
            return Ok(());
        };
        if version <= 1.3 {
            return Ok(());
        }
        let rotation = match &settings.native_output {
            Some(native) => native.rotation.clone()?,
            None => settings.rotation.into(),
        };
        let brightness = || -> Result<BigInt, IFlagError> {
            Ok(match &settings.native_output {
                Some(native) => native.brightness.clone()?,
                None => settings.brightness.into(),
            })
        };
        if rotation != self.previous_rotation || brightness()? != self.previous_brightness {
            panel.set_rotation(&rotation);
            let brightness = brightness()?;
            // The rescale works in doubles: an integer beyond binary64 is an overflow error.
            let level = LegacyValue::from_integer(brightness.clone()).py_as_double().map_err(|error| {
                let error = super::values::operation(error);
                IFlagError::NativeOutput(super::hapticr::NativeEngineError {
                    class: error.class,
                    message: error.message,
                })
            })?;
            let luma = truncate(map_clamped(level, 0.0, 100.0, 0.0, 150.0)).ok_or(IFlagError::NotFinite("luma"))?;
            panel.write(&luma_command(luma as u8));
            fresh = reread.map(|read| read());
            let current = fresh.as_ref().unwrap_or(settings);
            let (rotation, brightness) = match &current.native_output {
                Some(native) if fresh.is_some() => (native.rotation.clone()?, native.brightness.clone()?),
                _ if fresh.is_some() => (current.rotation.into(), current.brightness.into()),
                _ => (rotation, brightness),
            };
            self.previous_rotation = rotation;
            self.previous_brightness = brightness;
        }
        let settings = fresh.as_ref().unwrap_or(settings);
        let picture = if inputs.test {
            self.pictures.flag(1, 22.0, 11.0)?.0
        } else if !inputs.on {
            self.pictures.flag(1, 0.0, 11.0)?.0
        } else {
            // The wall-clock time is taken where the animation needs it, after the pause.
            self.game_picture(settings, inputs, clock.local_now())?
        };
        let mirror = match &settings.native_output {
            Some(native) => native.mirror.clone()?,
            None => settings.mirror,
        };
        panel.write(&picture_command(&picture, mirror, panel.rotation_quarters));
        Ok(())
    }

    fn game_picture(
        &mut self,
        settings: &IFlagSettings,
        inputs: &IFlagInputs,
        now: chrono::NaiveDateTime,
    ) -> Result<Matrix, IFlagError> {
        if let Some(values) = &inputs.native {
            return self.game_picture_native(settings, inputs, values, now);
        }
        let native = settings.native_controls.as_ref();
        let gear_display = native.map_or(settings.gear_display, |controls| controls.gear_display);
        let gear = if gear_display { inputs.gear } else { 11.0 };
        let native_gear = gear_display.then_some(inputs.native_gear.as_ref()).flatten();
        let rpm_enabled = native.map_or(settings.rpm, |controls| controls.rpm);
        let (rpm, max_rpm) = if rpm_enabled { (inputs.rpm, inputs.max_rpm) } else { (0.0, 0.0) };
        let mut rpm_leds = 0;
        if rpm != 0.0 {
            if max_rpm == 0.0 {
                return Err(IFlagError::ZeroMaxRpm);
            }
            rpm_leds = truncate(map_clamped(rpm / max_rpm, 0.0, 1.0, 0.0, 8.0))
                .ok_or(IFlagError::NotFinite("rpm / max_rpm"))?;
        }
        // Unlike the other four control reads, a failure of this required lookup is an error.
        let sli_enabled = match native {
            Some(controls) => controls.sli.clone().map_err(IFlagError::NativeControl)?,
            None => settings.sli,
        };
        let sli = if sli_enabled { inputs.shift_light } else { 0.0 };
        let mut flag = inputs.flag;
        let definition = flag_definition(flag);
        let enabled = if let Some(flags) = &settings.native_flags {
            flags.get(definition.map_or("none", |flag| flag.name)).copied().unwrap_or(false)
        } else {
            definition.and_then(|flag| settings.flags.get(flag.name)).copied().unwrap_or(false)
        };
        if !enabled {
            flag = 0.0;
        }
        let spotter_left = if native.map_or(settings.spotter_left, |controls| controls.spotter_left) {
            inputs.spotter_left
        } else {
            0.0
        };
        let spotter_right = if native.map_or(settings.spotter_right, |controls| controls.spotter_right) {
            inputs.spotter_right
        } else {
            0.0
        };
        if sli == 1.0 {
            flag = 0.0;
        }
        if spotter_right == 1.0 && spotter_left == 1.0 {
            flag = 41.0;
        } else if spotter_right == 1.0 {
            flag = 37.0;
        } else if spotter_left == 1.0 {
            flag = 36.0;
        }
        if flag != 0.0 {
            let (first, speed) = self.pictures.flag_native(0, flag, gear, native_gear)?;
            self.toggle_after(now, f64::from(speed) * 100.0);
            return if self.frame == 0 {
                Ok(first)
            } else {
                Ok(self.pictures.flag_native(1, flag, gear, native_gear)?.0)
            };
        }
        self.toggle_after(now, 200.0);
        self.pictures.gear_frame_native(self.frame, &GearFrame { gear, rpm_leds, shift_light: sli != 0.0 }, native_gear)
    }

    /// [`Self::game_picture`] on the raw telemetry values: `rpm != 0`, `rpm / max_rpm`, the
    /// flag name lookup and the `== 1` / `!= 0` tests are raw-value operations.
    fn game_picture_native(
        &mut self,
        settings: &IFlagSettings,
        inputs: &IFlagInputs,
        values: &NativeIFlagTelemetry,
        now: chrono::NaiveDateTime,
    ) -> Result<Matrix, IFlagError> {
        use openrigcue_model::python_compare::native_equal;
        let operation = |error: String| {
            let error = super::values::operation(error);
            IFlagError::NativeOutput(super::hapticr::NativeEngineError { class: error.class, message: error.message })
        };
        let zero = LegacyValue::Int(0);
        let one = LegacyValue::Int(1);
        let native = settings.native_controls.as_ref();
        let gear_display = native.map_or(settings.gear_display, |controls| controls.gear_display);
        let gear = if gear_display { inputs.gear } else { 11.0 };
        let native_gear = gear_display.then_some(inputs.native_gear.as_ref()).flatten();
        let rpm_enabled = native.map_or(settings.rpm, |controls| controls.rpm);
        // The telemetry value when its switch is on, else 0 (a missing key gives 0).
        let caught = |enabled: bool, value: &Option<LegacyValue>| {
            if enabled {
                value.clone().unwrap_or(LegacyValue::Int(0))
            } else {
                LegacyValue::Int(0)
            }
        };
        let rpm = caught(rpm_enabled, &values.rpm);
        let max_rpm = caught(rpm_enabled, &values.max_rpm);
        let mut rpm_leds = 0;
        if !native_equal(&rpm, &zero) {
            let ratio = rpm.py_truediv(&max_rpm).and_then(|v| v.py_as_double()).map_err(operation)?;
            rpm_leds =
                truncate(map_clamped(ratio, 0.0, 1.0, 0.0, 8.0)).ok_or(IFlagError::NotFinite("rpm / max_rpm"))?;
        }
        // Unlike the other four control reads, a failure of this required lookup is an error.
        let sli_enabled = match native {
            Some(controls) => controls.sli.clone().map_err(IFlagError::NativeControl)?,
            None => settings.sli,
        };
        let sli = if sli_enabled { inputs.shift_light } else { 0.0 };
        let mut flag = values.flag.clone().unwrap_or(LegacyValue::Int(0));
        // The flag name lookup is keyed by the value (equal int/float/bool codes are the same key).
        let code = |flag: &LegacyValue| match flag.resolved() {
            LegacyValue::Bool(_) | LegacyValue::Int(_) | LegacyValue::BigInt(_) | LegacyValue::Float(_) => {
                flag.number().unwrap_or(f64::NAN)
            }
            _ => f64::NAN,
        };
        let definition = flag_definition(code(&flag));
        let name = definition.map_or("none", |flag| flag.name);
        let enabled = match &settings.native_flags {
            Some(flags) => flags.get(name).copied().unwrap_or(false),
            None => settings.flags.get(name).copied().unwrap_or(false),
        };
        if !enabled {
            flag = LegacyValue::Int(0);
        }
        let spotter_left = caught(native.map_or(settings.spotter_left, |c| c.spotter_left), &values.spotter_left);
        let spotter_right = caught(native.map_or(settings.spotter_right, |c| c.spotter_right), &values.spotter_right);
        if sli == 1.0 {
            flag = LegacyValue::Int(0);
        }
        let (left, right) = (native_equal(&spotter_left, &one), native_equal(&spotter_right, &one));
        if right && left {
            flag = LegacyValue::Int(41);
        } else if right {
            flag = LegacyValue::Int(37);
        } else if left {
            flag = LegacyValue::Int(36);
        }
        if !native_equal(&flag, &zero) {
            let flag = code(&flag);
            let (first, speed) = self.pictures.flag_native(0, flag, gear, native_gear)?;
            self.toggle_after(now, f64::from(speed) * 100.0);
            return if self.frame == 0 {
                Ok(first)
            } else {
                Ok(self.pictures.flag_native(1, flag, gear, native_gear)?.0)
            };
        }
        self.toggle_after(now, 200.0);
        self.pictures.gear_frame_native(self.frame, &GearFrame { gear, rpm_leds, shift_light: sli != 0.0 }, native_gear)
    }
}

/// Environment of an iFlag scan.
pub struct IFlagScanContext<'a> {
    pub ports: &'a dyn PortLister,
    pub opener: &'a dyn PortOpener,
    pub clock: Arc<dyn Clock>,
    pub claimed: &'a ClaimedPorts,
    /// The port remembered from the last successful connection.
    pub cached_port: Option<&'a str>,
    /// `IFLAG.com_port`.
    pub com_port: &'a str,
    pub allowed: &'a AllowedPorts,
}

const BAUD_RATE: u32 = 9600;
const PORT_TIMEOUT: Duration = Duration::from_secs(1);
/// Seconds to wait for a ping with a supported firmware.
const PING_WAIT_SECONDS: u32 = 12;

/// Whether the next iFlag scan is the first one; cleared at the end of every scan pass, with or without a panel.
#[derive(Debug)]
pub struct IFlagScanner {
    first_scan: bool,
    version_parameter: Option<PublishedParameter<LegacyValue>>,
}

impl Default for IFlagScanner {
    fn default() -> Self {
        IFlagScanner { first_scan: true, version_parameter: None }
    }
}

/// A connected panel.
pub struct ConnectedIFlag {
    pub port: String,
    pub firmware: String,
    pub panel: IFlagPanel,
}

impl IFlagScanner {
    pub(super) fn publish_version_to(&mut self, parameter: PublishedParameter<LegacyValue>) {
        self.version_parameter = Some(parameter);
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// Arduino (2341:) or WCH (1A86:) ports at 9600 baud, wait up to 12 s for
    /// a ping announcing firmware 1.3 or 1.4. On the first pass only the cached port (if any) is tried.
    pub fn scan(&mut self, ctx: &IFlagScanContext<'_>) -> Result<Option<ConnectedIFlag>, super::link::PortScanError> {
        let rules = ScanRules {
            device: Device::IFlag,
            exclusions: Exclusions::Standard,
            configured_port: ctx.com_port,
            allowed: ctx.allowed,
            claimed: ctx.claimed,
            cached_port: ctx.cached_port,
            first_scan: self.first_scan,
        };
        for port in ctx.ports.ports()? {
            if !rules.may_try(&port) || !(port.hwid_contains("2341:")? || port.hwid_contains("1A86:")?) {
                continue;
            }
            let settings = PortSettings { baud_rate: BAUD_RATE, read_timeout: Some(PORT_TIMEOUT) };
            let Ok(link) = ctx.opener.open(&port.device, settings) else { continue };
            let Ok(panel) =
                IFlagPanel::start_with_version(link, Arc::clone(&ctx.clock), self.version_parameter.clone())
            else {
                continue;
            };
            for _ in 0..PING_WAIT_SECONDS {
                let firmware = panel.firmware();
                if firmware == "1.3" || firmware == "1.4" {
                    self.first_scan = false;
                    return Ok(Some(ConnectedIFlag { port: port.device.clone(), firmware, panel }));
                }
                debug!("waiting firmware version show up");
                ctx.clock.sleep(Duration::from_secs(1));
            }
            debug!("Did not find the firmware version");
        }
        self.first_scan = false;
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn ping_publication_keeps_numeric_version_and_conversion_failure_zero() {
        let parameter = super::PublishedParameter::new(super::LegacyValue::Int(0));
        let state = super::PingState { version_parameter: Some(parameter.clone()), ..Default::default() };
        let mut pending = Vec::new();
        for (major, minor, expected) in [
            (1, 4, super::LegacyValue::Float(1.4)),
            (0, 9, super::LegacyValue::Float(0.9)),
            (1, 3, super::LegacyValue::Float(1.3)),
        ] {
            let ping = format!("255\r\n255\r\n{major}\r\n{minor}\r\n176\r\n210\r\n0\r\n0\r\n");
            state.accept_firmware(super::ingest_pings(&mut pending, ping.as_bytes()).unwrap());
            assert_eq!(parameter.get(), expected);
            assert_eq!(*state.firmware.lock().unwrap(), format!("{major}.{minor}"));
        }
        // A version text that is not a number publishes 0.
        state.accept_firmware("1.-3".into());
        assert_eq!(parameter.get(), super::LegacyValue::Int(0));
        assert_eq!(*state.firmware.lock().unwrap(), "1.-3");
    }

    use super::super::link::fakes::{FakeLink, ManualClock};
    use super::*;
    use std::time::Instant;

    /// The gear glyph (row order reversed, so the blank row is the bottom one) and
    /// the rpm bar in that bottom row.
    #[test]
    fn gear_frame_with_rpm_bar() {
        let mut pics = Pictures::default();
        assert_eq!(pics.glyphs[&1][7], [0; 8]);
        let m = pics.gear_frame(0, &GearFrame { gear: 1.0, rpm_leds: 4, shift_light: false }).unwrap();
        assert_eq!(m[0], [0, 1, 1, 1, 1, 1, 1, 0]); // the source's last row, "01111110"
        assert_eq!(m[4], [0, 0, 1, 1, 1, 0, 0, 0]); // "00111000", not reversed
        assert_eq!(m[6], [0, 0, 0, 1, 1, 0, 0, 0]);
        assert_eq!(m[7], [3, 3, 3, 5, 0, 0, 0, 0]);
        let sli = pics.gear_frame(1, &GearFrame { gear: 1.0, rpm_leds: 4, shift_light: true }).unwrap();
        assert_eq!(sli[7], [0; 8]);
        assert!(pics.gear_frame(0, &GearFrame { gear: 2.5, rpm_leds: 0, shift_light: false }).is_err());
    }

    #[test]
    fn audited_gear_types_use_the_exact_python_string_key_only_when_a_glyph_is_needed() {
        let mut pictures = Pictures::default();
        let frame = GearFrame { gear: 3.0, rpm_leds: 0, shift_light: false };
        assert!(pictures.gear_frame_native(0, &frame, Some(&LegacyValue::Int(3))).is_ok());
        assert_eq!(
            pictures.gear_frame_native(0, &frame, Some(&LegacyValue::Float(3.0))),
            Err(IFlagError::NoGlyph("3.0".into()))
        );
        assert_eq!(
            pictures.gear_frame_native(0, &frame, Some(&LegacyValue::Bool(true))),
            Err(IFlagError::NoGlyph("True".into()))
        );
        assert!(
            pictures.flag_native(0, 22.0, 3.0, Some(&LegacyValue::Float(3.0))).is_ok(),
            "a non-overlay flag does not look up the gear"
        );
        assert_eq!(
            pictures.flag_native(0, 1.0, 3.0, Some(&LegacyValue::Float(3.0))),
            Err(IFlagError::NoGlyph("3.0".into()))
        );
        let mut stream = IFlagStream::new(super::super::link::local_now());
        let mut inputs = IFlagInputs {
            gear: 3.0,
            native_gear: Some(LegacyValue::Float(3.0)),
            rpm: 0.0,
            max_rpm: 0.0,
            flag: 0.0,
            spotter_left: 0.0,
            spotter_right: 0.0,
            shift_light: 0.0,
            test: false,
            on: true,
            native: None,
        };
        let mut settings = settings();
        assert_eq!(
            stream.game_picture(&settings, &inputs, super::super::link::local_now()),
            Err(IFlagError::NoGlyph("3.0".into()))
        );
        settings.gear_display = false;
        assert!(stream.game_picture(&settings, &inputs, super::super::link::local_now()).is_ok());
        inputs.native_gear = Some(LegacyValue::Int(3));
        settings.gear_display = true;
        assert!(stream.game_picture(&settings, &inputs, super::super::link::local_now()).is_ok());
    }

    /// Flag colours, two-frame patterns, gear overlay and unknown codes.
    #[test]
    fn flag_pictures() {
        let pics = Pictures::default();
        let (green, speed) = pics.flag(0, 1.0, 11.0).unwrap();
        assert_eq!(speed, 3);
        assert!(green.iter().all(|r| r.iter().all(|c| *c == color::GREEN)));
        let (yellow0, _) = pics.flag(0, 6.0, 11.0).unwrap();
        let (yellow1, _) = pics.flag(1, 6.0, 11.0).unwrap();
        assert_eq!(yellow0[0][0], color::YELLOW);
        assert_eq!(yellow1[0][0], color::BLACK);
        let (overlay, _) = pics.flag(0, 1.0, 3.0).unwrap();
        assert_eq!(overlay[0], [3, 3, 4, 4, 4, 4, 3, 3]);
        assert_eq!(overlay[1], [3, 4, 4, 3, 3, 4, 4, 3]);
        assert_eq!(overlay[7], [3; 8]);
        let (unknown, _) = pics.flag(0, 4.0, 11.0).unwrap();
        assert!(unknown.iter().all(|r| r.iter().all(|c| *c == color::BLACK)));
        // Code 42 is not a flag: drawn like an unknown code.
        let (q, _) = pics.flag(1, 42.0, 11.0).unwrap();
        assert!(q.iter().all(|r| r.iter().all(|c| *c == color::BLACK)));
    }

    /// Full-image command packing, mirror and clockwise rotation.
    #[test]
    fn picture_packing_and_rotation() {
        let mut m: Matrix = [[0; 8]; 8];
        m[0][0] = 2;
        m[0][1] = 3;
        let cmd = picture_command(&m, false, 0);
        assert_eq!(&cmd[..3], &[0xFF, 16, 0x32]);
        assert_eq!(cmd.len(), 34);
        let mirrored = picture_command(&m, true, 0);
        assert_eq!(mirrored[5], 0x23);
        let rotated = picture_command(&m, false, 1);
        // (0,0) -> (0,7), (0,1) -> (1,7): last byte of rows 0 and 1, high nibble.
        assert_eq!(rotated[2 + 3], 0x20);
        assert_eq!(rotated[2 + 7], 0x30);
        assert_eq!(luma_command(75), vec![0xFF, 0xFF, 0xA2, 75, 0, 0, 0, 0]);
    }

    /// The rpm bar is `rpm / max_rpm` rescaled from 0..1 to 0..8 with the shared clamped rescale, where a NaN
    /// ratio becomes the upper bound: all 8 rpm LEDs are lit and the picture is sent.
    #[test]
    fn nan_rpm_lights_the_whole_bar() {
        let link = FakeLink::new();
        let clock = Arc::new(ManualClock::default());
        let mut panel = IFlagPanel::start(Box::new(link.clone()), clock).unwrap();
        *panel.state.firmware.lock().unwrap() = "1.4".into();
        let draw_clock = ManualClock::default();
        let mut stream = IFlagStream::new(draw_clock.local_now());
        let inputs = IFlagInputs {
            gear: 3.0,
            native_gear: None,
            rpm: f64::NAN,
            max_rpm: 8000.0,
            flag: 0.0,
            spotter_left: 0.0,
            spotter_right: 0.0,
            shift_light: 0.0,
            test: false,
            on: true,
            native: None,
        };
        assert_eq!(stream.draw(&mut panel, &settings(), &inputs, &draw_clock), Ok(()));
        let full_bar =
            Pictures::default().gear_frame(0, &GearFrame { gear: 3.0, rpm_leds: 8, shift_light: false }).unwrap();
        assert_eq!(full_bar[7], RPM_COLORS);
        let mut expected = luma_command(75);
        expected.extend(picture_command(&full_bar, false, 1));
        assert_eq!(link.take_written(), expected);
    }

    /// Receiving and ping parsing: ASCII decimal lines, resync on FF FF.
    #[test]
    fn ping_parsing() {
        let mut pending = Vec::new();
        assert_eq!(ingest_pings(&mut pending, b"7\r\n255\r\n255\r\n1\r\n4\r\n176\r\n"), None);
        assert!(pending[0].equals(255));
        assert_eq!(ingest_pings(&mut pending, b"210\r\n0\r\n0\r\n"), Some("1.4".to_string()));
        assert!(pending.is_empty());
        let ping = |values: [i64; 8]| values.map(PyInt::Small);
        assert_eq!(parse_ping(&ping([255, 255, 1, 3, 177, 210, 0, 0])), None);
        // A value beyond 64 bits is kept.
        let mut pending = Vec::new();
        assert_eq!(
            ingest_pings(&mut pending, b"255\r\n255\r\n123456789012345678901\r\n4\r\n176\r\n210\r\n0\r\n0\r\n"),
            Some("123456789012345678901.4".to_string())
        );
        // Lines with anything but digits are skipped one by one; the other lines of the same read count.
        let mut pending = Vec::new();
        assert_eq!(
            ingest_pings(
                &mut pending,
                b"boot\r\n255\r\n+5\r\n 12\r\n-3\r\n\xff\r\n255\r\n1\r\n4\r\n176\r\n210\r\n0\r\n0\r\n"
            ),
            Some("1.4".to_string())
        );
        assert!(pending.is_empty());
    }

    fn settings() -> IFlagSettings {
        IFlagSettings {
            rotation: 90,
            brightness: 50,
            native_output: None,
            native_controls: None,
            native_flags: None,
            mirror: false,
            gear_display: true,
            rpm: true,
            sli: true,
            spotter_left: true,
            spotter_right: true,
            flags: [("green".to_string(), true), ("spotter_left".to_string(), true)].into_iter().collect(),
        }
    }

    /// Stream: luma on change, spotter arrow beating the flag, nothing on firmware 1.3.
    #[test]
    fn stream_draws_by_priority() {
        let link = FakeLink::new();
        let clock = Arc::new(ManualClock::default());
        let mut panel = IFlagPanel::start(Box::new(link.clone()), clock).unwrap();
        *panel.state.firmware.lock().unwrap() = "1.4".into();
        let draw_clock = ManualClock::default();
        let mut stream = IFlagStream::new(draw_clock.local_now());
        let inputs = IFlagInputs {
            gear: 3.0,
            native_gear: None,
            rpm: 4000.0,
            max_rpm: 8000.0,
            flag: 1.0,
            spotter_left: 1.0,
            spotter_right: 0.0,
            shift_light: 0.0,
            test: false,
            on: true,
            native: None,
        };
        stream.draw(&mut panel, &settings(), &inputs, &draw_clock).unwrap();
        let w = link.take_written();
        assert_eq!(&w[..8], &luma_command(75)[..]);
        assert_eq!(w.len(), 8 + 34);
        stream.draw(&mut panel, &settings(), &inputs, &draw_clock).unwrap();
        let w = link.take_written();
        assert_eq!(w.len(), 34);
        let (arrow, _) = stream.pictures.flag(0, 36.0, 3.0).unwrap();
        assert_eq!(w, picture_command(&arrow, false, 1));
        *panel.state.firmware.lock().unwrap() = "1.3".into();
        stream.draw(&mut panel, &settings(), &inputs, &draw_clock).unwrap();
        assert!(link.take_written().is_empty());
        assert_eq!(draw_clock.elapsed(), Duration::ZERO);
    }

    /// The empty-firmware branch pauses then returns before orientation, luma or picture work.
    #[test]
    fn empty_firmware_pauses_one_second() {
        let link = FakeLink::new();
        let mut panel = IFlagPanel {
            link: Box::new(link.clone()),
            state: Arc::new(PingState::default()),
            receiver: None,
            rotation_quarters: 0,
        };
        let draw_clock = ManualClock::default();
        let mut stream = IFlagStream::new(draw_clock.local_now());
        let inputs = IFlagInputs {
            gear: 3.0,
            native_gear: None,
            rpm: 4000.0,
            max_rpm: 8000.0,
            flag: 0.0,
            spotter_left: 0.0,
            spotter_right: 0.0,
            shift_light: 0.0,
            test: false,
            on: true,
            native: None,
        };
        stream.draw(&mut panel, &settings(), &inputs, &draw_clock).unwrap();
        assert_eq!(draw_clock.elapsed(), Duration::from_secs(1));
        assert!(link.take_written().is_empty());
        assert_eq!(
            (stream.previous_rotation.clone(), stream.previous_brightness.clone()),
            (BigInt::from(-1), BigInt::from(-1))
        );
    }

    #[test]
    fn firmware_published_during_pause_is_used_on_next_frame() {
        struct PublishClock {
            clock: ManualClock,
            state: Arc<PingState>,
        }
        impl Clock for PublishClock {
            fn local_now(&self) -> chrono::NaiveDateTime {
                self.clock.local_now()
            }
            fn now(&self) -> Instant {
                self.clock.now()
            }
            fn sleep(&self, duration: Duration) {
                self.clock.sleep(duration);
                self.state.accept_firmware("1.4".into());
            }
        }
        let wire = FakeLink::new();
        let state = Arc::new(PingState::default());
        let mut panel = IFlagPanel {
            link: Box::new(wire.clone()),
            state: Arc::clone(&state),
            receiver: None,
            rotation_quarters: 0,
        };
        let clock = PublishClock { clock: ManualClock::default(), state };
        let mut stream = IFlagStream::new(clock.local_now());
        let inputs = IFlagInputs {
            gear: 3.0,
            native_gear: None,
            rpm: 0.0,
            max_rpm: 0.0,
            flag: 0.0,
            spotter_left: 0.0,
            spotter_right: 0.0,
            shift_light: 0.0,
            test: true,
            on: true,
            native: None,
        };
        stream.draw(&mut panel, &settings(), &inputs, &clock).unwrap();
        assert!(wire.take_written().is_empty());
        assert_eq!(
            (stream.previous_rotation.clone(), stream.previous_brightness.clone()),
            (BigInt::from(-1), BigInt::from(-1))
        );
        stream.draw(&mut panel, &settings(), &inputs, &clock).unwrap();
        assert_eq!(wire.take_written().len(), 8 + 34);
        assert_eq!(clock.clock.elapsed(), Duration::from_secs(1));
    }

    #[test]
    fn firmware_gate_handles_protocol_invalid_and_old_versions() {
        let wire = FakeLink::new();
        let mut panel = IFlagPanel {
            link: Box::new(wire.clone()),
            state: Arc::new(PingState::default()),
            receiver: None,
            rotation_quarters: 0,
        };
        let clock = ManualClock::default();
        let mut stream = IFlagStream::new(clock.local_now());
        let inputs = IFlagInputs {
            gear: 3.0,
            native_gear: None,
            rpm: 0.0,
            max_rpm: 0.0,
            flag: 0.0,
            spotter_left: 0.0,
            spotter_right: 0.0,
            shift_light: 0.0,
            test: true,
            on: true,
            native: None,
        };
        for text in ["1.-3", "1.3", "0.9"] {
            panel.state.accept_firmware(text.into());
            stream.draw(&mut panel, &settings(), &inputs, &clock).unwrap();
            assert!(wire.take_written().is_empty());
            assert_eq!(
                (stream.previous_rotation.clone(), stream.previous_brightness.clone()),
                (BigInt::from(-1), BigInt::from(-1))
            );
        }
        panel.state.accept_firmware(" １.４ ".into());
        stream.draw(&mut panel, &settings(), &inputs, &clock).unwrap();
        assert_eq!(wire.take_written().len(), 8 + 34);
        assert_eq!(clock.elapsed(), Duration::ZERO);
    }

    #[test]
    fn native_output_brightness_error_preserves_rotation_prefix() {
        let wire = FakeLink::new();
        let mut panel = IFlagPanel {
            link: Box::new(wire.clone()),
            state: Arc::new(PingState::default()),
            receiver: None,
            rotation_quarters: 0,
        };
        panel.state.accept_firmware("1.4".into());
        let clock = ManualClock::default();
        let mut stream = IFlagStream::new(clock.local_now());
        let mut config = settings();
        config.native_output = Some(NativeIFlagOutput {
            rotation: Ok(180.into()),
            brightness: Err(super::super::hapticr::NativeEngineError {
                class: "ValueError",
                message: "invalid literal for int() with base 10: '2.5'".into(),
            }),
            mirror: Ok(false),
        });
        let inputs = IFlagInputs {
            gear: 3.0,
            native_gear: None,
            rpm: 0.0,
            max_rpm: 0.0,
            flag: 0.0,
            spotter_left: 0.0,
            spotter_right: 0.0,
            shift_light: 0.0,
            test: true,
            on: true,
            native: None,
        };
        let error = stream.draw(&mut panel, &config, &inputs, &clock).unwrap_err();
        assert_eq!(error.to_string(), "invalid literal for int() with base 10: '2.5'");
        assert_eq!(panel.rotation_quarters, 2);
        assert_eq!(
            (stream.previous_rotation.clone(), stream.previous_brightness.clone()),
            (BigInt::from(-1), BigInt::from(-1))
        );
        assert!(wire.take_written().is_empty());
        config.native_output.as_mut().unwrap().brightness = Ok(50.into());
        stream.draw(&mut panel, &config, &inputs, &clock).unwrap();
        assert_eq!(wire.take_written().len(), 8 + 34);
        assert_eq!(
            (stream.previous_rotation.clone(), stream.previous_brightness.clone()),
            (BigInt::from(180), BigInt::from(50))
        );
        config.native_output.as_mut().unwrap().brightness =
            Err(super::super::hapticr::NativeEngineError { class: "KeyError", message: "'brightness'".into() });
        assert!(matches!(stream.draw(&mut panel, &config, &inputs, &clock), Err(IFlagError::NativeOutput(_))));
        assert!(wire.take_written().is_empty());
        assert_eq!(
            (stream.previous_rotation.clone(), stream.previous_brightness.clone()),
            (BigInt::from(180), BigInt::from(50))
        );
    }

    #[test]
    fn native_output_rotation_error_precedes_brightness_and_firmware_gate() {
        let wire = FakeLink::new();
        let mut panel = IFlagPanel {
            link: Box::new(wire.clone()),
            state: Arc::new(PingState::default()),
            receiver: None,
            rotation_quarters: 0,
        };
        let clock = ManualClock::default();
        let mut stream = IFlagStream::new(clock.local_now());
        let mut config = settings();
        config.native_output = Some(NativeIFlagOutput {
            rotation: Err(super::super::hapticr::NativeEngineError { class: "KeyError", message: "'rotation'".into() }),
            brightness: Err(super::super::hapticr::NativeEngineError {
                class: "KeyError",
                message: "'brightness'".into(),
            }),
            mirror: Ok(false),
        });
        let inputs = IFlagInputs {
            gear: 3.0,
            native_gear: None,
            rpm: 0.0,
            max_rpm: 0.0,
            flag: 0.0,
            spotter_left: 0.0,
            spotter_right: 0.0,
            shift_light: 0.0,
            test: true,
            on: true,
            native: None,
        };
        panel.state.accept_firmware("1.3".into());
        stream.draw(&mut panel, &config, &inputs, &clock).unwrap();
        assert!(wire.take_written().is_empty());
        panel.state.accept_firmware("1.4".into());
        assert_eq!(stream.draw(&mut panel, &config, &inputs, &clock).unwrap_err().to_string(), "'rotation'");
        assert_eq!(panel.rotation_quarters, 0);
        assert!(wire.take_written().is_empty());
    }

    #[test]
    fn native_controls_missing_sli_fails_after_luma_before_picture() {
        let wire = FakeLink::new();
        let mut panel = IFlagPanel {
            link: Box::new(wire.clone()),
            state: Arc::new(PingState::default()),
            receiver: None,
            rotation_quarters: 0,
        };
        panel.state.accept_firmware("1.4".into());
        let clock = ManualClock::default();
        let mut stream = IFlagStream::new(clock.local_now());
        let mut config = settings();
        config.native_controls = Some(NativeIFlagControls {
            gear_display: false,
            rpm: false,
            sli: Err(super::super::hapticr::NativeEngineError { class: "KeyError", message: "'sli'".into() }),
            spotter_left: false,
            spotter_right: false,
        });
        let inputs = IFlagInputs {
            gear: 3.0,
            native_gear: None,
            rpm: 0.0,
            max_rpm: 0.0,
            flag: 0.0,
            spotter_left: 0.0,
            spotter_right: 0.0,
            shift_light: 0.0,
            test: false,
            on: true,
            native: None,
        };
        let error = stream.draw(&mut panel, &config, &inputs, &clock).unwrap_err();
        assert_eq!(error.to_string(), "'sli'");
        assert_eq!(wire.take_written(), luma_command(75));
        assert_eq!(
            (stream.previous_rotation.clone(), stream.previous_brightness.clone()),
            (BigInt::from(90), BigInt::from(50))
        );
    }

    #[test]
    fn native_controls_keep_game_gate_rpm_error_and_recovery_order() {
        let wire = FakeLink::new();
        let mut panel = IFlagPanel {
            link: Box::new(wire.clone()),
            state: Arc::new(PingState::default()),
            receiver: None,
            rotation_quarters: 0,
        };
        panel.state.accept_firmware("1.4".into());
        let clock = ManualClock::default();
        let mut stream = IFlagStream::new(clock.local_now());
        let mut config = settings();
        config.native_controls = Some(NativeIFlagControls {
            gear_display: false,
            rpm: true,
            sli: Err(super::super::hapticr::NativeEngineError { class: "KeyError", message: "'sli'".into() }),
            spotter_left: false,
            spotter_right: false,
        });
        let mut inputs = IFlagInputs {
            gear: 3.0,
            native_gear: None,
            rpm: 100.0,
            max_rpm: 0.0,
            flag: 0.0,
            spotter_left: 0.0,
            spotter_right: 0.0,
            shift_light: 0.0,
            test: false,
            on: true,
            native: None,
        };
        assert_eq!(stream.draw(&mut panel, &config, &inputs, &clock), Err(IFlagError::ZeroMaxRpm));
        assert_eq!(wire.take_written(), luma_command(75));
        inputs.test = true;
        stream.draw(&mut panel, &config, &inputs, &clock).unwrap();
        assert!(!wire.take_written().is_empty());
        inputs.test = false;
        inputs.on = false;
        stream.draw(&mut panel, &config, &inputs, &clock).unwrap();
        assert!(!wire.take_written().is_empty());
        inputs.on = true;
        inputs.rpm = 0.0;
        assert!(matches!(stream.draw(&mut panel, &config, &inputs, &clock), Err(IFlagError::NativeControl(_))));
        assert!(wire.take_written().is_empty());
        config.native_controls.as_mut().unwrap().sli = Ok(false);
        stream.draw(&mut panel, &config, &inputs, &clock).unwrap();
        let expected =
            Pictures::default().gear_frame(0, &GearFrame { gear: 11.0, rpm_leds: 0, shift_light: false }).unwrap();
        assert_eq!(wire.take_written(), picture_command(&expected, false, 1));
    }

    #[test]
    fn native_controls_drive_each_picture_branch_over_typed_projection() {
        let now = super::super::link::local_now();
        let inputs = IFlagInputs {
            gear: 3.0,
            native_gear: None,
            rpm: 4000.0,
            max_rpm: 8000.0,
            flag: 0.0,
            spotter_left: 1.0,
            spotter_right: 1.0,
            shift_light: 1.0,
            test: false,
            on: true,
            native: None,
        };
        for branch in ["none", "gear", "rpm", "sli", "left", "right", "both"] {
            let mut config = settings();
            config.native_controls = Some(NativeIFlagControls {
                gear_display: branch == "gear",
                rpm: branch == "rpm",
                sli: Ok(branch == "sli"),
                spotter_left: matches!(branch, "left" | "both"),
                spotter_right: matches!(branch, "right" | "both"),
            });
            let actual = IFlagStream::new(now).game_picture(&config, &inputs, now).unwrap();
            let mut pictures = Pictures::default();
            let expected = match branch {
                "left" => pictures.flag(0, 36.0, 11.0).unwrap().0,
                "right" => pictures.flag(0, 37.0, 11.0).unwrap().0,
                "both" => pictures.flag(0, 41.0, 11.0).unwrap().0,
                _ => pictures
                    .gear_frame(
                        0,
                        &GearFrame {
                            gear: if branch == "gear" { 3.0 } else { 11.0 },
                            rpm_leds: if branch == "rpm" { 4 } else { 0 },
                            shift_light: branch == "sli",
                        },
                    )
                    .unwrap(),
            };
            assert_eq!(actual, expected, "{branch}");
        }
    }

    #[test]
    fn native_flag_identity_overrides_false_typed_projection() {
        let now = super::super::link::local_now();
        let mut config = settings();
        config.gear_display = false;
        config.flags.insert("blue".into(), false);
        config.native_flags = Some(HashMap::from([("blue".into(), true)]));
        let inputs = IFlagInputs {
            gear: 3.0,
            native_gear: None,
            rpm: 0.0,
            max_rpm: 0.0,
            flag: 2.0,
            spotter_left: 0.0,
            spotter_right: 0.0,
            shift_light: 0.0,
            test: false,
            on: true,
            native: None,
        };
        let actual = IFlagStream::new(now).game_picture(&config, &inputs, now).unwrap();
        assert_eq!(actual, Pictures::default().flag(0, 2.0, 11.0).unwrap().0);
    }

    #[test]
    fn native_flag_missing_or_false_keeps_local_fallback_and_later_priority() {
        let now = super::super::link::local_now();
        let mut config = settings();
        config.gear_display = false;
        config.flags.insert("blue".into(), true);
        config.native_flags = Some(HashMap::new());
        let mut inputs = IFlagInputs {
            gear: 3.0,
            native_gear: None,
            rpm: 0.0,
            max_rpm: 0.0,
            flag: 2.0,
            spotter_left: 0.0,
            spotter_right: 0.0,
            shift_light: 0.0,
            test: false,
            on: true,
            native: None,
        };
        let mut stream = IFlagStream::new(now);
        assert_eq!(stream.game_picture(&config, &inputs, now).unwrap(), [[0; 8]; 8]);
        config.native_flags.as_mut().unwrap().insert("blue".into(), false);
        assert_eq!(stream.game_picture(&config, &inputs, now).unwrap(), [[0; 8]; 8]);
        inputs.spotter_right = 1.0;
        assert_eq!(
            stream.game_picture(&config, &inputs, now).unwrap(),
            Pictures::default().flag(0, 37.0, 11.0).unwrap().0
        );
        inputs.spotter_right = 0.0;
        config.native_flags.as_mut().unwrap().insert("blue".into(), true);
        inputs.shift_light = 1.0;
        let expected =
            Pictures::default().gear_frame(0, &GearFrame { gear: 11.0, rpm_leds: 0, shift_light: true }).unwrap();
        assert_eq!(stream.game_picture(&config, &inputs, now).unwrap(), expected);
        inputs.shift_light = 0.0;
        inputs.flag = 999.0;
        config.native_flags.as_mut().unwrap().insert("none".into(), true);
        assert_eq!(stream.game_picture(&config, &inputs, now).unwrap(), [[0; 8]; 8]);
    }

    #[test]
    fn native_mirror_failure_follows_luma_and_keeps_output_cache() {
        let wire = FakeLink::new();
        let mut panel = IFlagPanel {
            link: Box::new(wire.clone()),
            state: Arc::new(PingState::default()),
            receiver: None,
            rotation_quarters: 0,
        };
        panel.state.accept_firmware("1.4".into());
        let clock = ManualClock::default();
        let mut stream = IFlagStream::new(clock.local_now());
        let mut config = settings();
        config.native_output = Some(NativeIFlagOutput {
            rotation: Ok(90.into()),
            brightness: Ok(50.into()),
            mirror: Err(super::super::hapticr::NativeEngineError {
                class: "Exception",
                message: "Boolean value expected. Got maybe".into(),
            }),
        });
        let inputs = IFlagInputs {
            gear: 3.0,
            native_gear: None,
            rpm: 0.0,
            max_rpm: 0.0,
            flag: 0.0,
            spotter_left: 0.0,
            spotter_right: 0.0,
            shift_light: 0.0,
            test: false,
            on: true,
            native: None,
        };
        let error = stream.draw(&mut panel, &config, &inputs, &clock).unwrap_err();
        assert_eq!(error.to_string(), "Boolean value expected. Got maybe");
        assert_eq!(wire.take_written(), luma_command(75));
        assert_eq!(
            (stream.previous_rotation.clone(), stream.previous_brightness.clone()),
            (BigInt::from(90), BigInt::from(50))
        );
    }

    #[test]
    fn native_mirror_read_keeps_firmware_picture_error_and_test_off_order() {
        let wire = FakeLink::new();
        let mut panel = IFlagPanel {
            link: Box::new(wire.clone()),
            state: Arc::new(PingState::default()),
            receiver: None,
            rotation_quarters: 0,
        };
        let clock = ManualClock::default();
        let mut stream = IFlagStream::new(clock.local_now());
        let mut config = settings();
        config.native_output = Some(NativeIFlagOutput {
            rotation: Ok(90.into()),
            brightness: Ok(50.into()),
            mirror: Err(super::super::hapticr::NativeEngineError { class: "KeyError", message: "'mirror'".into() }),
        });
        let mut inputs = IFlagInputs {
            gear: 3.0,
            native_gear: Some(LegacyValue::Float(3.0)),
            rpm: 0.0,
            max_rpm: 0.0,
            flag: 0.0,
            spotter_left: 0.0,
            spotter_right: 0.0,
            shift_light: 0.0,
            test: false,
            on: true,
            native: None,
        };
        panel.state.accept_firmware("1.3".into());
        stream.draw(&mut panel, &config, &inputs, &clock).unwrap();
        assert!(wire.take_written().is_empty());
        panel.state.accept_firmware("1.4".into());
        assert_eq!(stream.draw(&mut panel, &config, &inputs, &clock), Err(IFlagError::NoGlyph("3.0".into())));
        assert_eq!(wire.take_written(), luma_command(75));
        inputs.test = true;
        assert_eq!(stream.draw(&mut panel, &config, &inputs, &clock).unwrap_err().to_string(), "'mirror'");
        assert!(wire.take_written().is_empty());
        inputs.test = false;
        inputs.on = false;
        assert_eq!(stream.draw(&mut panel, &config, &inputs, &clock).unwrap_err().to_string(), "'mirror'");
        assert!(wire.take_written().is_empty());
        inputs.on = true;
        inputs.native_gear = Some(LegacyValue::Int(3));
        config.native_output.as_mut().unwrap().mirror = Ok(true);
        stream.draw(&mut panel, &config, &inputs, &clock).unwrap();
        let picture =
            Pictures::default().gear_frame(0, &GearFrame { gear: 3.0, rpm_leds: 0, shift_light: false }).unwrap();
        assert_eq!(wire.take_written(), picture_command(&picture, true, 1));
        assert_eq!(
            (stream.previous_rotation.clone(), stream.previous_brightness.clone()),
            (BigInt::from(90), BigInt::from(50))
        );
    }

    /// The cached port restricts the first scan pass only, even when that pass finds no panel.
    #[test]
    fn cached_port_gate_ends_after_the_first_pass_without_a_panel() {
        use super::super::link::fakes::{FakeOpener, FakePorts};
        let ports = FakePorts(vec![super::super::link::PortEntry::usb("/dev/ttyUSB5", 0x1a86, 0x7523, None, None)]);
        let opener = FakeOpener::default();
        let claimed = ClaimedPorts::default();
        let allowed = AllowedPorts::default();
        let ctx = IFlagScanContext {
            ports: &ports,
            opener: &opener,
            clock: Arc::new(ManualClock::default()),
            claimed: &claimed,
            cached_port: Some("/dev/ttyUSB4"),
            com_port: "",
            allowed: &allowed,
        };
        let mut scanner = IFlagScanner::new();
        assert!(scanner.scan(&ctx).unwrap().is_none());
        assert!(opener.opened().is_empty());
        assert!(scanner.scan(&ctx).unwrap().is_none());
        assert_eq!(opener.opened().len(), 1);
        assert_eq!(opener.opened()[0].0, "/dev/ttyUSB5");
    }
}
