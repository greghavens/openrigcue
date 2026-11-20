//! Intellibox: the SRS hub for two or three fans, the LED strip and the tachometer, over CmdMessenger.
//!
//! Two firmware generations exist (v5 on ATmega328P boards behind FTDI/CH340 bridges, v6 on an
//! ATmega32U4 Leonardo, USB id 2341:8036). The command tables differ in their identification commands; the
//! streaming commands share indices 4, 5, 7 and 8.

use openrigcue_model::native_value::{BigInt, LegacyValue};
use std::time::Duration;

use tracing::{debug, info};

use super::cmd_messenger::{self, Arg, CmdError, CommandSpec, Field, Message};
use super::led::SegmentFrame;
use super::link::{Clock, PortEntry, PortLister, PortOpener, PortSettings, SerialLink};
use super::numeric::{map_clamped, truncate};
use super::open_gate::{new_handshake, open_gate};
use super::port_scan::{AllowedPorts, ClaimedPorts, Device, Exclusions, ScanRules};

use Field::{Byte, Int16 as I, Int32 as L, Text as S};

/// Command table of the v5 firmware.
pub const V5_COMMANDS: &[CommandSpec] = &[
    CommandSpec { name: "who_are_you", fields: &[] },
    CommandSpec { name: "my_name_is", fields: &[S] },
    CommandSpec { name: "what_is_your_version", fields: &[] },
    CommandSpec { name: "my_version_is", fields: &[I] },
    CommandSpec { name: "send_stream", fields: &[I, I, I, I, I, L, L, L] },
    CommandSpec { name: "send_led", fields: &[I, L, I, L, I, L, I, L, L] },
    CommandSpec { name: "error", fields: &[S] },
    CommandSpec { name: "initialize_parameters", fields: &[I, I, I, I] },
    CommandSpec { name: "set_led_segments", fields: &[I, I, I, I, I, I, I, I] },
    CommandSpec { name: "what_is_your_model", fields: &[] },
    CommandSpec { name: "my_model_is", fields: &[I] },
    // Last firmware command slot (its firmware name); OpenRigCue never sends it.
    CommandSpec { name: "write_license_key", fields: &[S] },
];

/// Command table of the v6 firmware.
pub const V6_COMMANDS: &[CommandSpec] = &[
    CommandSpec { name: "who_are_you", fields: &[S] },
    CommandSpec { name: "i_am", fields: &[S, Byte, Byte, Byte, Byte, S] },
    CommandSpec { name: "what_is_your_version", fields: &[] },
    CommandSpec { name: "my_version_is", fields: &[I] },
    CommandSpec { name: "send_stream", fields: &[I, I, I, I, I, L, L, L] },
    CommandSpec { name: "send_led", fields: &[I, L, I, L, I, L, I, L, L] },
    CommandSpec { name: "error", fields: &[S] },
    CommandSpec { name: "initialize_parameters", fields: &[I, I, I, I] },
    CommandSpec { name: "set_led_segments", fields: &[I, I, I, I, I, I, I, I] },
    // Last firmware command slot (its firmware name); OpenRigCue never sends it.
    CommandSpec { name: "write_license_key", fields: &[S] },
];

const WHO_ARE_YOU: usize = 0;
const NAME_REPLY: usize = 1;
const WHAT_IS_YOUR_VERSION: usize = 2;
const SEND_STREAM: usize = 4;
const SEND_LED: usize = 5;
const INITIALIZE_PARAMETERS: usize = 7;
const SET_LED_SEGMENTS: usize = 8;
const WHAT_IS_YOUR_MODEL: usize = 9;

/// The firmware versions this build expects.
pub const SHIPPED_FIRMWARE_V5: i64 = 19;
pub const SHIPPED_FIRMWARE_V6: i64 = 1;

/// Intellibox model that has a third (centre) fan: its stream carries the centre fan in the rpm-tone field.
pub const THREE_FAN_MODEL: i64 = 6;

/// The name a genuine box reports.
const SRS_IDENTIFIER: &str = "srs";

/// Serial read timeout of the Intellibox board.
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// Wait after opening the port for the board's auto-reset.
const SETTLE_TIME: Duration = Duration::from_secs(2);

/// USB ids accepted as an Intellibox (FTDI FT232, CH340, Arduino Leonardo).
const CANDIDATE_IDS: [&str; 3] = ["0403:6001", "1A86:7523", "2341:8036"];
/// The Leonardo-based v6 board.
const V6_USB_ID: &str = "2341:8036";

/// Firmware generation of a box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Generation {
    V5,
    V6,
}

impl Generation {
    /// The Leonardo USB id (`2341:8036`) anywhere
    /// in the hardware identity string selects v6.
    pub fn for_hwid(hwid: &str) -> Self {
        if hwid.contains(V6_USB_ID) {
            Generation::V6
        } else {
            Generation::V5
        }
    }

    pub fn commands(self) -> &'static [CommandSpec] {
        match self {
            Generation::V5 => V5_COMMANDS,
            Generation::V6 => V6_COMMANDS,
        }
    }
}

/// The firmware version expected for the box on `hwid`: v6 for the Leonardo USB id, v5 otherwise.
pub fn shipped_firmware(hwid: &str) -> i64 {
    if hwid.contains(V6_USB_ID) {
        SHIPPED_FIRMWARE_V6
    } else {
        SHIPPED_FIRMWARE_V5
    }
}

/// An open connection to a box.
pub struct Intellibox {
    link: Box<dyn SerialLink>,
    generation: Generation,
    /// Model and version bytes of the v6 `i_am` answer, which the version and model
    /// queries return.
    i_am: Option<IAm>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IAm {
    model: i64,
    version: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum IntelliboxError {
    #[error(transparent)]
    Command(#[from] CmdError),
    #[error("segment {0} has no colours (unknown segment type)")]
    SegmentWithoutColors(usize),
    #[error(transparent)]
    NativeRange(#[from] super::led::NativeLedError),
    #[error(transparent)]
    NativeInitialization(super::led::NativeLedError),
    #[error(transparent)]
    DiagnosticOutput(#[from] std::io::Error),
}

impl Intellibox {
    /// Wrap an open link (after the settle time).
    pub fn new(link: Box<dyn SerialLink>, generation: Generation) -> Self {
        Intellibox { link, generation, i_am: None }
    }

    fn send(&mut self, index: usize, args: &[Arg]) -> Result<(), CmdError> {
        cmd_messenger::send(self.link.as_mut(), self.generation.commands(), index, args)
    }

    fn receive(&mut self) -> Result<Option<Message>, CmdError> {
        cmd_messenger::receive(self.link.as_mut(), self.generation.commands())
    }

    /// Ask the box who it is; `"srs"` for a genuine Intellibox, `""` otherwise.
    ///
    /// v5 sends `who_are_you` and reads
    /// the `my_name_is` text; v6 sends a nonce, retries an empty read once, and for an `i_am` answer whose
    /// XTEA identifier decodes to the nonce keeps product/model/version/serial and is
    /// named `srs`. Failures yield the empty name. The status number of the answer is not judged.
    pub fn identify(&mut self, rng: &mut impl rand::Rng) -> String {
        match self.generation {
            Generation::V5 => self.identify_v5(),
            Generation::V6 => self.identify_v6(rng),
        }
    }

    fn identify_v5(&mut self) -> String {
        if let Err(e) = self.send(WHO_ARE_YOU, &[]) {
            debug!("Cannot ask the name to the Intellibox: {e}");
        }
        match self.receive() {
            Ok(Some(msg)) => msg.args.first().and_then(|v| v.as_text()).unwrap_or_default().to_string(),
            Ok(None) => String::new(),
            Err(CmdError::NotAscii) => {
                debug!("Received non-UTF8/binary data on this port - not an Intellibox");
                String::new()
            }
            Err(e) => {
                debug!("Error while receiving name from the Intellibox: {e}");
                String::new()
            }
        }
    }

    fn identify_v6(&mut self, rng: &mut impl rand::Rng) -> String {
        let handshake = new_handshake(rng);
        if let Err(e) = self.send(WHO_ARE_YOU, &[Arg::Text(handshake.clone())]) {
            debug!("error asking who are you {e}");
            return String::new();
        }
        let reply = match self.receive() {
            Ok(None) => {
                debug!("output is none trying again");
                self.receive()
            }
            other => other,
        };
        let Ok(Some(msg)) = reply else { return String::new() };
        if !msg.is(NAME_REPLY) {
            return String::new();
        }
        self.i_am = None;
        if !Self::v6_genuine(&msg, &handshake) {
            return String::new();
        }
        let field = |i: usize| msg.args.get(i).and_then(|v| v.as_int());
        let (Some(model), Some(version)) = (field(2), field(3)) else { return String::new() };
        self.i_am = Some(IAm { model, version });
        SRS_IDENTIFIER.to_string()
    }

    /// Whether a decoded `i_am` identifier is for this nonce
    /// and carries an integer status.
    fn v6_genuine(msg: &Message, handshake: &str) -> bool {
        let Some(identifier) = msg.args.first().and_then(|v| v.as_text()) else { return false };
        match open_gate(identifier, handshake) {
            Ok(status) => super::numeric::python_int_checked(&status) != Err(super::numeric::PyIntError::Invalid),
            Err(e) => {
                debug!("Intellibox handshake rejected: {e}");
                false
            }
        }
    }

    /// Ask a v5 box a numeric property; `None` when the answer is missing.
    fn ask_number(&mut self, request: usize) -> Option<i64> {
        if let Err(e) = self.send(request, &[]) {
            debug!("Cannot ask the Intellibox: {e}");
        }
        match self.receive() {
            Ok(Some(msg)) => msg.args.first().and_then(|v| v.as_int()),
            _ => None,
        }
    }

    /// Firmware version of the box; `previous` (the value already known) when the answer is missing.
    pub fn firmware_version(&mut self, previous: i64) -> i64 {
        match self.generation {
            Generation::V6 => self.i_am.as_ref().map_or(previous, |i| i.version),
            Generation::V5 => self.ask_number(WHAT_IS_YOUR_VERSION).unwrap_or(previous),
        }
    }

    /// Model number of the box (the number of fans it drives; 6 = three-fan box), `previous` when missing.
    pub fn model(&mut self, previous: i64) -> i64 {
        match self.generation {
            Generation::V6 => self.i_am.as_ref().map_or(previous, |i| i.model),
            Generation::V5 => self.ask_number(WHAT_IS_YOUR_MODEL).unwrap_or(previous),
        }
    }
}

/// Values of the `initialize_parameters` command.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InitParameters {
    /// The tune's `gear_shift_dim_perc` (tach brightness, 0..100).
    pub gear_shift_dim_percent: f64,
    /// `LED.brightness` (0..100).
    pub led_brightness: i64,
    /// `LED.nbr_leds`.
    pub led_count: i64,
    /// `INTELLIBOX.fan_frequency` (PWM frequency index).
    pub fan_frequency: i64,
}

impl InitParameters {
    /// The four payload values: dim mapped 0..100 → 0..15, brightness mapped 0..100 → 0..255 (both
    /// truncated), the LED count and the fan frequency.
    fn args(&self) -> Option<[Arg; 4]> {
        let intensity = truncate(map_clamped(self.gear_shift_dim_percent, 0.0, 100.0, 0.0, 15.0))?;
        let brightness = truncate(map_clamped(self.led_brightness as f64, 0.0, 100.0, 0.0, 255.0))?;
        Some([
            Arg::Number(intensity as f64),
            Arg::Number(brightness as f64),
            Arg::Number(self.led_count as f64),
            Arg::Number(self.fan_frequency as f64),
        ])
    }
}

/// Required raw values retained until the initialization check and its two
/// different error boundaries are reached.
#[derive(Debug, Clone, PartialEq)]
pub struct NativeInitParameters {
    pub values: [Result<LegacyValue, super::led::NativeLedError>; 4],
    pub dim_percent: Result<f64, super::led::NativeLedError>,
    /// `LED.brightness`, `LED.nbr_leds` and `INTELLIBOX.fan_frequency` as arbitrary-width integers,
    /// narrowed only by the `initialize_parameters` packer.
    pub brightness: Result<BigInt, super::led::NativeLedError>,
    pub led_count: Result<BigInt, super::led::NativeLedError>,
    pub fan_frequency: Result<BigInt, super::led::NativeLedError>,
}

impl NativeInitParameters {
    pub fn from_parameters(init: &InitParameters) -> Self {
        Self {
            values: [
                Ok(LegacyValue::Float(init.gear_shift_dim_percent)),
                Ok(LegacyValue::Int(init.led_brightness)),
                Ok(LegacyValue::Int(init.led_count)),
                Ok(LegacyValue::Int(init.fan_frequency)),
            ],
            dim_percent: Ok(init.gear_shift_dim_percent),
            brightness: Ok(init.led_brightness.into()),
            led_count: Ok(init.led_count.into()),
            fan_frequency: Ok(init.fan_frequency.into()),
        }
    }
}

/// The configuration the segment and initialization checks and commands read, as it stands when it is
/// re-read after a CmdMessenger call.
#[derive(Debug, Clone, PartialEq)]
pub struct NativeHelperConfig {
    pub ranges: super::led::NativeSegmentRanges,
    pub init: NativeInitParameters,
}

/// The 1-based UI ranges of the four segments, `[start1, end1, ..., start4, end4]`.
pub type SegmentRanges = [i64; 8];

/// One Intellibox frame.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamFrame {
    pub left_fan: i64,
    pub right_fan: i64,
    /// The harmonized gear from telemetry, truncated by the protocol layer.
    pub harmonized_gear: f64,
    /// Engine rotation frequency `rpm * 2 / 120`, truncated.
    pub rpm_tone: i64,
    /// `shift_light_indicator` (0/1).
    pub alert_rpm: f64,
    pub segments: [SegmentFrame; 4],
    pub center_fan: i64,
}

/// What the LED change check compares: lit counts and even colours of all segments, odd colour of the 4th.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LedSignature {
    lit: [BigInt; 4],
    even: [super::led::ColorValue; 4],
    last_odd: super::led::ColorValue,
}

/// Change tracking of the stream sender, kept across reconnections.
#[derive(Debug, Default)]
pub struct IntelliboxStream {
    initialized: bool,
    previous_init: Option<InitParameters>,
    previous_native_init: Option<[LegacyValue; 4]>,
    previous_segments: Option<SegmentRanges>,
    previous_native_segments: Option<[LegacyValue; 8]>,
    previous_led: Option<LedSignature>,
    frames_since_init_check: u32,
}

/// Re-check the init parameters every this many frames.
const INIT_CHECK_FRAMES: u32 = 10;

fn segment_args(ranges: &SegmentRanges) -> Vec<Arg> {
    ranges.iter().map(|v| Arg::Number((v - 1) as f64)).collect()
}

impl IntelliboxStream {
    pub fn new() -> Self {
        Self::default()
    }

    /// Initialization-parameter change check: true (and remembered) when the values changed.
    fn init_changed(&mut self, init: &InitParameters) -> bool {
        let changed = self.previous_init.as_ref() != Some(init);
        if changed {
            self.previous_init = Some(*init);
        }
        changed
    }

    fn native_init_changed(
        &mut self,
        init: &InitParameters,
        native: Option<&NativeInitParameters>,
    ) -> Result<bool, IntelliboxError> {
        let Some(native) = native else { return Ok(self.init_changed(init)) };
        let mut values = Vec::with_capacity(4);
        for value in &native.values {
            values.push(value.clone().map_err(IntelliboxError::NativeInitialization)?);
        }
        let values: [LegacyValue; 4] = values.try_into().expect("four native initialization values");
        let changed = self.previous_native_init.as_ref().is_none_or(|previous| {
            !previous
                .iter()
                .zip(&values)
                .all(|(left, right)| openrigcue_model::python_compare::native_equal(left, right))
        });
        self.previous_native_init = Some(values);
        Ok(changed)
    }

    /// Segment-range change check.
    fn segments_changed(&mut self, ranges: &SegmentRanges) -> bool {
        let changed = self.previous_segments.as_ref() != Some(ranges);
        if changed {
            self.previous_segments = Some(*ranges);
        }
        changed
    }

    fn native_segments_changed(
        &mut self,
        ranges: &SegmentRanges,
        native: Option<&super::led::NativeSegmentRanges>,
    ) -> Result<bool, IntelliboxError> {
        let Some(native) = native else { return Ok(self.segments_changed(ranges)) };
        let mut values = Vec::with_capacity(8);
        for value in &native.values {
            values.push(value.clone()?);
        }
        let values: [LegacyValue; 8] = values.try_into().expect("eight native range values");
        let changed = self.previous_native_segments.as_ref().is_none_or(|previous| {
            !previous
                .iter()
                .zip(&values)
                .all(|(left, right)| openrigcue_model::python_compare::native_equal(left, right))
        });
        self.previous_native_segments = Some(values);
        Ok(changed)
    }

    fn send_ranges(
        box_: &mut Intellibox,
        ranges: &SegmentRanges,
        native: Option<&super::led::NativeSegmentRanges>,
    ) -> Result<(), IntelliboxError> {
        let args = match native {
            None => segment_args(ranges),
            Some(native) => {
                let mut args = Vec::with_capacity(8);
                for position in &native.positions {
                    args.push(Arg::Native(LegacyValue::from_integer(position.clone()? - 1)));
                }
                args
            }
        };
        box_.send(SET_LED_SEGMENTS, &args)?;
        Ok(())
    }

    /// LED change check over the four segments.
    fn led_changed(&mut self, signature: LedSignature) -> bool {
        let changed = self.previous_led.as_ref() != Some(&signature);
        if changed {
            self.previous_led = Some(signature);
        }
        changed
    }

    fn forget_parameters(&mut self) {
        self.previous_init = None;
        self.previous_native_init = None;
        self.previous_segments = None;
        self.previous_native_segments = None;
    }

    /// Sends the `initialize_parameters` command; failures are only logged.
    fn send_init(box_: &mut Intellibox, init: &InitParameters) {
        debug!("sending initialization parameters");
        let sent = init.args().map(|args| box_.send(INITIALIZE_PARAMETERS, &args));
        if !matches!(sent, Some(Ok(()))) {
            debug!("Initialization of the parameters of the Intellibox Failed");
        }
    }

    fn send_native_init(
        box_: &mut Intellibox,
        init: &InitParameters,
        native: Option<&NativeInitParameters>,
        output: &mut dyn FnMut(&str) -> std::io::Result<()>,
    ) -> Result<(), IntelliboxError> {
        let Some(native) = native else {
            Self::send_init(box_, init);
            return Ok(());
        };
        // Both rescales and integer conversions come before the logged-only part. The rescale
        // has fixed finite output bounds, including for a NaN input.
        let dim = native.dim_percent.clone().map_err(IntelliboxError::NativeInitialization)?;
        let intensity = truncate(map_clamped(dim, 0.0, 100.0, 0.0, 15.0)).expect("bounded dim map");
        let brightness = native.brightness.clone().map_err(IntelliboxError::NativeInitialization)?;
        // The rescale works in doubles: an integer beyond binary64 is an overflow error, not merely logged.
        let brightness = LegacyValue::from_integer(brightness)
            .py_as_double()
            .map_err(|e| IntelliboxError::NativeInitialization(super::led::NativeLedError::operation(e)))?;
        let brightness = truncate(map_clamped(brightness, 0.0, 100.0, 0.0, 255.0)).expect("bounded brightness map");
        debug!("sending initialization parameters");
        let sent = (|| {
            let count = native.led_count.clone().map_err(IntelliboxError::NativeInitialization)?;
            let frequency = native.fan_frequency.clone().map_err(IntelliboxError::NativeInitialization)?;
            box_.send(
                INITIALIZE_PARAMETERS,
                &[
                    Arg::Number(intensity as f64),
                    Arg::Number(brightness as f64),
                    Arg::Native(LegacyValue::from_integer(count)),
                    Arg::Native(LegacyValue::from_integer(frequency)),
                ],
            )?;
            Ok::<_, IntelliboxError>(())
        })();
        if sent.is_err() {
            // Only this literal is printed. A failed print is an error.
            output("Initialization of the parameters of the Intellibox Failed\n")?;
        }
        Ok(())
    }

    /// Send one frame.
    ///
    /// The first frame sends the init parameters and segment ranges when they changed; every frame
    /// sends `send_stream` (centre fan in the rpm-tone field on model 6); ranges and LED colours are sent
    /// only when they changed; the init parameters are re-checked every 10 frames. A failed send forgets the
    /// remembered init and segment values so they are sent again.
    pub fn send_frame(
        &mut self,
        box_: &mut Intellibox,
        model: i64,
        frame: &StreamFrame,
        init: &InitParameters,
        ranges: &SegmentRanges,
    ) -> Result<(), IntelliboxError> {
        self.send_frame_with_gear_value(box_, model, frame, init, ranges, None)
    }

    pub(crate) fn send_frame_with_gear_value(
        &mut self,
        box_: &mut Intellibox,
        model: i64,
        frame: &StreamFrame,
        init: &InitParameters,
        ranges: &SegmentRanges,
        gear: Option<&LegacyValue>,
    ) -> Result<(), IntelliboxError> {
        self.send_frame_native_ranges(box_, model, frame, init, ranges, gear, None)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn send_frame_native_ranges(
        &mut self,
        box_: &mut Intellibox,
        model: i64,
        frame: &StreamFrame,
        init: &InitParameters,
        ranges: &SegmentRanges,
        gear: Option<&LegacyValue>,
        native: Option<&super::led::NativeSegmentRanges>,
    ) -> Result<(), IntelliboxError> {
        self.send_frame_native_initialization(box_, model, frame, init, ranges, gear, native, None)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn send_frame_native_initialization(
        &mut self,
        box_: &mut Intellibox,
        model: i64,
        frame: &StreamFrame,
        init: &InitParameters,
        ranges: &SegmentRanges,
        gear: Option<&LegacyValue>,
        native: Option<&super::led::NativeSegmentRanges>,
        native_init: Option<&NativeInitParameters>,
    ) -> Result<(), IntelliboxError> {
        self.send_frame_with_output(box_, model, frame, init, ranges, gear, native, native_init, &mut |text| {
            use std::io::Write;
            std::io::stdout().lock().write_all(text.as_bytes())
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn send_frame_with_output(
        &mut self,
        box_: &mut Intellibox,
        model: i64,
        frame: &StreamFrame,
        init: &InitParameters,
        ranges: &SegmentRanges,
        gear: Option<&LegacyValue>,
        native: Option<&super::led::NativeSegmentRanges>,
        native_init: Option<&NativeInitParameters>,
        output: &mut dyn FnMut(&str) -> std::io::Result<()>,
    ) -> Result<(), IntelliboxError> {
        self.send_frame_with_values_output(box_, model, frame, init, ranges, gear, native, native_init, None, output)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn send_frame_native_channels(
        &mut self,
        box_: &mut Intellibox,
        model: i64,
        frame: &StreamFrame,
        init: &InitParameters,
        ranges: &SegmentRanges,
        gear: Option<&LegacyValue>,
        native: Option<&super::led::NativeSegmentRanges>,
        native_init: Option<&NativeInitParameters>,
        tone: &LegacyValue,
        reread: Option<&dyn Fn() -> NativeHelperConfig>,
    ) -> Result<(), IntelliboxError> {
        self.send_frame_live(
            box_,
            model,
            frame,
            init,
            ranges,
            gear,
            native,
            native_init,
            Some(tone),
            reread,
            &mut |text| {
                use std::io::Write;
                std::io::stdout().lock().write_all(text.as_bytes())
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn send_frame_with_values_output(
        &mut self,
        box_: &mut Intellibox,
        model: i64,
        frame: &StreamFrame,
        init: &InitParameters,
        ranges: &SegmentRanges,
        gear: Option<&LegacyValue>,
        native: Option<&super::led::NativeSegmentRanges>,
        native_init: Option<&NativeInitParameters>,
        tone: Option<&LegacyValue>,
        output: &mut dyn FnMut(&str) -> std::io::Result<()>,
    ) -> Result<(), IntelliboxError> {
        self.send_frame_live(box_, model, frame, init, ranges, gear, native, native_init, tone, None, output)
    }

    /// The sender body. The values the
    /// cycle read first serve until the first CmdMessenger call; every later check re-reads the live
    /// configuration through `reread`, so another thread's configuration
    /// change is visible after each call.
    #[allow(clippy::too_many_arguments)]
    fn send_frame_live(
        &mut self,
        box_: &mut Intellibox,
        model: i64,
        frame: &StreamFrame,
        init: &InitParameters,
        ranges: &SegmentRanges,
        gear: Option<&LegacyValue>,
        native: Option<&super::led::NativeSegmentRanges>,
        native_init: Option<&NativeInitParameters>,
        tone: Option<&LegacyValue>,
        reread: Option<&dyn Fn() -> NativeHelperConfig>,
        output: &mut dyn FnMut(&str) -> std::io::Result<()>,
    ) -> Result<(), IntelliboxError> {
        let mut live: Option<NativeHelperConfig> = None;
        let mut crossed = false;
        let refresh = |live: &mut Option<NativeHelperConfig>, crossed: &mut bool| {
            if std::mem::take(crossed) {
                if let Some(read) = reread {
                    *live = Some(read());
                }
            }
        };
        if !self.initialized {
            if self.native_init_changed(init, native_init)? {
                crossed = true;
                Self::send_native_init(box_, init, native_init, output)?;
            }
            refresh(&mut live, &mut crossed);
            let current = live.as_ref().map(|c| &c.ranges).or(native);
            if self.native_segments_changed(ranges, current)? {
                if let Err(e) = Self::send_ranges(box_, ranges, current) {
                    self.forget_parameters();
                    debug!("initial set_led_segments to the intellibox failed {e}");
                }
            }
            self.initialized = true;
        }
        crossed = true;
        if let Err(e) = self.send_stream_command(box_, model, frame, gear, tone) {
            self.forget_parameters();
            debug!("send_stream to the intellibox failed {e}");
        }
        refresh(&mut live, &mut crossed);
        let current = live.as_ref().map(|c| &c.ranges).or(native);
        if self.native_segments_changed(ranges, current)? {
            crossed = true;
            if let Err(e) = Self::send_ranges(box_, ranges, current) {
                self.forget_parameters();
                self.initialized = false;
                debug!("set_led_segments to the intellibox failed {e}");
            }
        }
        let signature = led_signature(&frame.segments)?;
        let args = led_args(&signature);
        if self.led_changed(signature) {
            crossed = true;
            if let Err(e) = box_.send(SEND_LED, &args) {
                self.forget_parameters();
                self.initialized = false;
                debug!("send_led to the intellibox failed {e}");
            }
        }
        self.frames_since_init_check += 1;
        if self.frames_since_init_check == INIT_CHECK_FRAMES {
            refresh(&mut live, &mut crossed);
            let current = live.as_ref().map(|c| &c.init).or(native_init);
            if self.native_init_changed(init, current)? {
                Self::send_native_init(box_, init, current, output)?;
            }
            self.frames_since_init_check = 0;
        }
        Ok(())
    }

    fn send_stream_command(
        &self,
        box_: &mut Intellibox,
        model: i64,
        frame: &StreamFrame,
        gear: Option<&LegacyValue>,
        native_tone: Option<&LegacyValue>,
    ) -> Result<(), IntelliboxError> {
        let tone = if model == THREE_FAN_MODEL { frame.center_fan } else { frame.rpm_tone };
        let odd = |i: usize| -> Result<Arg, IntelliboxError> {
            let (_, odd) = frame.segments[i].colors.as_ref().ok_or(IntelliboxError::SegmentWithoutColors(i + 1))?;
            Ok(color_arg(odd))
        };
        let mut args = [
            Arg::Number(frame.left_fan as f64),
            Arg::Number(frame.right_fan as f64),
            Arg::Number(frame.harmonized_gear),
            Arg::Number(tone as f64),
            Arg::Number(frame.alert_rpm),
            odd(0)?,
            odd(1)?,
            odd(2)?,
        ];
        // Arguments are built before the field encoder converts them to integers.
        // Keep text/bytes until that point so a failed conversion happens after the
        // initialization and is followed by the error handling and LED operations.
        if let Some(gear) = gear {
            args[2] = Arg::Native(gear.clone());
        }
        if model != THREE_FAN_MODEL {
            if let Some(tone) = native_tone {
                args[3] = Arg::Native(tone.clone());
            }
        }
        Ok(box_.send(SEND_STREAM, &args)?)
    }
}

fn led_signature(segments: &[SegmentFrame; 4]) -> Result<LedSignature, IntelliboxError> {
    let mut even: [super::led::ColorValue; 4] = Default::default();
    let mut last_odd = None;
    for (i, s) in segments.iter().enumerate() {
        let (e, o) = s.colors.clone().ok_or(IntelliboxError::SegmentWithoutColors(i + 1))?;
        even[i] = e;
        last_odd = o;
    }
    Ok(LedSignature { lit: segments.clone().map(|s| s.lit), even, last_odd })
}

/// A segment colour as an `l` field receives it: the word, or null, which
/// the packer rejects with a type error.
fn color_arg(color: &super::led::ColorValue) -> Arg {
    Arg::Native(color.clone().map_or(LegacyValue::None, LegacyValue::from_integer))
}

/// `send_led`: `[nbr1 + 1, even1, nbr2 + 1, even2, nbr3 + 1, even3, nbr4 + 1, even4, odd4]`,
/// unbounded integers range-checked by the `i`/`l` packers.
fn led_args(signature: &LedSignature) -> Vec<Arg> {
    let mut args: Vec<Arg> = signature
        .lit
        .iter()
        .zip(&signature.even)
        .flat_map(|(lit, even)| [Arg::Native(LegacyValue::from_integer(lit + 1)), color_arg(even)])
        .collect();
    args.push(color_arg(&signature.last_odd));
    args
}

/// Settings of the `INTELLIBOX` config section that the scan uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanSettings {
    /// `INTELLIBOX.com_port` ('' = auto).
    pub com_port: String,
    pub allowed: AllowedPorts,
}

/// A box found by the scan.
pub struct Connected {
    pub port: String,
    pub model: i64,
    pub version: i64,
    pub link: Intellibox,
}

/// Everything the scan needs from its surroundings.
pub struct ScanContext<'a> {
    pub ports: &'a dyn PortLister,
    pub opener: &'a dyn PortOpener,
    pub clock: &'a dyn Clock,
    pub claimed: &'a ClaimedPorts,
    /// The port remembered from the last successful connection.
    pub cached_port: Option<&'a str>,
    /// The known version and model before the scan (both start at -1).
    pub previous_version: i64,
    pub previous_model: i64,
}

/// Port scanning state (whether this is the first scan).
#[derive(Debug)]
pub struct IntelliboxScanner {
    first_scan: bool,
}

impl Default for IntelliboxScanner {
    fn default() -> Self {
        IntelliboxScanner { first_scan: true }
    }
}

impl IntelliboxScanner {
    pub fn new() -> Self {
        Self::default()
    }

    fn candidate(port: &PortEntry, baud_rate: u32) -> Result<bool, super::link::PortScanError> {
        if port.hwid_contains(V6_USB_ID)? && baud_rate == 9600 {
            debug!("Port {}, incompatible baud rate for address {}", port.device, port.hwid_text());
            return Ok(false);
        }
        port.hwid_contains_any(&CANDIDATE_IDS)
    }

    /// One scan at `baud_rate`; the first scan only tries the cached port. A genuine box whose firmware
    /// differs from the expected one is closed and handed to `flash` (with its port), then the scan goes on.
    ///
    /// Steps: filter ports, open the board (10 s timeout,
    /// 2 s settle), require the name `srs`, compare the firmware with the expected version, read the model.
    pub fn scan(
        &mut self,
        ctx: &ScanContext<'_>,
        settings: &ScanSettings,
        baud_rate: u32,
        rng: &mut impl rand::Rng,
        flash: &mut dyn FnMut(&str),
    ) -> Result<Option<Connected>, super::link::PortScanError> {
        let rules = ScanRules {
            device: Device::Intellibox,
            exclusions: Exclusions::Standard,
            configured_port: &settings.com_port,
            allowed: &settings.allowed,
            claimed: ctx.claimed,
            cached_port: ctx.cached_port.filter(|p| !p.is_empty()),
            first_scan: self.first_scan,
        };
        let mut connected = None;
        // A failed port listing is logged ("Failed to list serial ports: ...") and ends the scan
        // before the first-scan flag is cleared and without touching the status.
        let ports = ctx.ports.ports().inspect_err(|e| debug!("Failed to list serial ports: {e}"))?;
        for port in ports {
            debug!("Inspecting port {} | desc={} | hwid={}", port.device, port.description, port.hwid_text());
            if !rules.may_try(&port) || !Self::candidate(&port, baud_rate)? {
                continue;
            }
            debug!("Candidate Intellibox port found: {} ({})", port.device, port.hwid_text());
            match self.try_port(ctx, &port, baud_rate, rng) {
                Probe::Connected(found) => {
                    connected = Some(found);
                    break;
                }
                Probe::NeedsFirmware => flash(&port.device),
                Probe::NotAnIntellibox => {}
            }
        }
        self.first_scan = false;
        Ok(connected)
    }

    fn try_port(&self, ctx: &ScanContext<'_>, port: &PortEntry, baud_rate: u32, rng: &mut impl rand::Rng) -> Probe {
        let settings = PortSettings { baud_rate, read_timeout: Some(READ_TIMEOUT) };
        let link = match ctx.opener.open(&port.device, settings) {
            Ok(link) => link,
            Err(e) => {
                debug!("Cannot open connection on {}: {e}", port.device);
                return Probe::NotAnIntellibox;
            }
        };
        ctx.clock.sleep(SETTLE_TIME);
        let mut box_ = Intellibox::new(link, Generation::for_hwid(port.hwid_text()));
        let name = box_.identify(rng);
        if name != SRS_IDENTIFIER {
            debug!("Non-Intellibox device detected on {}, closing connection", port.device);
            return Probe::NotAnIntellibox;
        }
        info!("Intellibox detected on {}", port.device);
        let found = box_.firmware_version(ctx.previous_version);
        let expected = shipped_firmware(port.hwid_text());
        if found != expected {
            info!("Firmware update required: expected={expected}, found={found}");
            return Probe::NeedsFirmware;
        }
        debug!("Firmware version matches, proceeding with initialization");
        let model = box_.model(ctx.previous_model);
        info!("Intellibox successfully connected on {}", port.device);
        Probe::Connected(Connected { port: port.device.clone(), model, version: found, link: box_ })
    }
}

/// What probing one port found.
enum Probe {
    Connected(Connected),
    NeedsFirmware,
    NotAnIntellibox,
}

#[cfg(test)]
mod tests {
    use super::super::led::SegmentFrame;
    use super::super::link::fakes::{FakeLink, FakeOpener, FakePorts, ManualClock};
    use super::super::open_gate::seal_for_tests;
    use super::*;
    use rand::SeedableRng;

    fn seg(lit: i64, even: u32, odd: u32) -> SegmentFrame {
        SegmentFrame::words(1, 50, lit, Some((even, odd)))
    }

    fn frame() -> StreamFrame {
        StreamFrame {
            left_fan: 511,
            right_fan: 300,
            harmonized_gear: 3.0,
            rpm_tone: 66,
            alert_rpm: 0.0,
            segments: [seg(24, 0xFF0000, 0xFF0000), seg(25, 0xFF, 0xFF), seg(150, 0, 0), seg(0, 0, 0)],
            center_fan: 700,
        }
    }

    #[test]
    fn native_init_cache_keeps_raw_types_and_numeric_equality() {
        let link = FakeLink::new();
        let mut device = Intellibox::new(Box::new(link.clone()), Generation::V5);
        let mut stream = IntelliboxStream::new();
        let mut native = NativeInitParameters::from_parameters(&init());
        native.values[1] = Ok(LegacyValue::Str("100".into()));
        let mut run = |stream: &mut IntelliboxStream, native: &NativeInitParameters| {
            stream
                .send_frame_with_output(
                    &mut device,
                    2,
                    &frame(),
                    &init(),
                    &RANGES,
                    None,
                    None,
                    Some(native),
                    &mut |_| panic!("valid initialization does not print"),
                )
                .unwrap();
        };
        run(&mut stream, &native);
        assert_eq!(commands(&link.take_written()), ["7", "8", "4", "5"]);
        native.values[1] = Ok(LegacyValue::Int(100));
        stream.frames_since_init_check = 9;
        run(&mut stream, &native);
        assert_eq!(commands(&link.take_written()), ["4", "7"]);
        native.values[1] = Ok(LegacyValue::Float(100.0));
        stream.frames_since_init_check = 9;
        run(&mut stream, &native);
        assert_eq!(commands(&link.take_written()), ["4"]);
        stream.forget_parameters();
        assert!(stream.previous_native_init.is_none());
    }

    #[test]
    fn native_init_pre_catch_failure_keeps_committed_cache_and_cold_retry() {
        for index in 0..2 {
            let link = FakeLink::new();
            let mut device = Intellibox::new(Box::new(link.clone()), Generation::V5);
            let mut stream = IntelliboxStream::new();
            let mut native = NativeInitParameters::from_parameters(&init());
            native.values[index] = Ok(LegacyValue::Str("bad".into()));
            let error = super::super::led::NativeLedError { class: "TypeError", message: "stage error".into() };
            if index == 0 {
                native.dim_percent = Err(error.clone());
            } else {
                native.brightness = Err(error.clone());
            }
            let result = stream.send_frame_with_output(
                &mut device,
                2,
                &frame(),
                &init(),
                &RANGES,
                None,
                None,
                Some(&native),
                &mut |_| panic!("failure is outside the print catch"),
            );
            assert!(matches!(result, Err(IntelliboxError::NativeInitialization(got)) if got == error));
            assert!(link.take_written().is_empty());
            assert!(!stream.initialized);
            assert!(stream.previous_native_init.is_some());
            stream
                .send_frame_with_output(
                    &mut device,
                    2,
                    &frame(),
                    &init(),
                    &RANGES,
                    None,
                    None,
                    Some(&native),
                    &mut |_| panic!("unchanged input skips the failed initializer"),
                )
                .unwrap();
            assert_eq!(commands(&link.take_written()), ["8", "4", "5"]);
            assert!(stream.initialized);
        }
    }

    #[test]
    fn native_init_required_failure_and_tenth_frame_error_preserve_prefixes() {
        let link = FakeLink::new();
        let mut device = Intellibox::new(Box::new(link.clone()), Generation::V5);
        let mut stream = IntelliboxStream::new();
        let good = NativeInitParameters::from_parameters(&init());
        let mut missing = good.clone();
        let error = super::super::led::NativeLedError { class: "KeyError", message: "'fan_frequency'".into() };
        missing.values[3] = Err(error.clone());
        missing.dim_percent = Err(super::super::led::NativeLedError {
            class: "TypeError",
            message: "must be real number, not str".into(),
        });
        let call = |stream: &mut IntelliboxStream, device: &mut Intellibox, native: &NativeInitParameters| {
            stream.send_frame_with_output(device, 2, &frame(), &init(), &RANGES, None, None, Some(native), &mut |_| {
                panic!("pre-catch failure does not print")
            })
        };
        assert!(matches!(call(&mut stream, &mut device, &missing),
            Err(IntelliboxError::NativeInitialization(got)) if got == error));
        assert!(stream.previous_native_init.is_none());
        assert!(link.take_written().is_empty());
        call(&mut stream, &mut device, &good).unwrap();
        link.take_written();
        let cached = stream.previous_native_init.clone();
        stream.frames_since_init_check = 9;
        assert!(call(&mut stream, &mut device, &missing).is_err());
        assert_eq!(commands(&link.take_written()), ["4"]);
        assert_eq!(stream.frames_since_init_check, 10);
        assert_eq!(stream.previous_native_init, cached);
        // The counter is not reset when the send fails; 11 is not 10.
        call(&mut stream, &mut device, &missing).unwrap();
        assert_eq!(commands(&link.take_written()), ["4"]);
        assert_eq!(stream.frames_since_init_check, 11);
    }

    #[test]
    fn native_init_count_and_frequency_failures_print_and_continue() {
        for index in 2..4 {
            let link = FakeLink::new();
            let mut device = Intellibox::new(Box::new(link.clone()), Generation::V5);
            let mut stream = IntelliboxStream::new();
            let mut native = NativeInitParameters::from_parameters(&init());
            native.values[index] = Ok(LegacyValue::Str("bad".into()));
            let error = super::super::led::NativeLedError {
                class: "ValueError",
                message: "invalid literal for int() with base 10: 'bad'".into(),
            };
            if index == 2 {
                native.led_count = Err(error);
            } else {
                native.fan_frequency = Err(error);
            }
            let mut output = Vec::new();
            stream
                .send_frame_with_output(
                    &mut device,
                    2,
                    &frame(),
                    &init(),
                    &RANGES,
                    None,
                    None,
                    Some(&native),
                    &mut |text| {
                        output.push(text.to_owned());
                        Ok(())
                    },
                )
                .unwrap();
            assert_eq!(output, ["Initialization of the parameters of the Intellibox Failed\n"]);
            assert_eq!(commands(&link.take_written()), ["8", "4", "5"]);
            assert!(stream.initialized);
            assert!(stream.previous_native_init.is_some());
            stream
                .send_frame_with_output(
                    &mut device,
                    2,
                    &frame(),
                    &init(),
                    &RANGES,
                    None,
                    None,
                    Some(&native),
                    &mut |_| panic!("unchanged input skips the failed initializer"),
                )
                .unwrap();
            assert_eq!(commands(&link.take_written()), ["4"]);
        }
    }

    #[test]
    fn native_init_failed_send_prints_once_and_preserves_numeric_payload() {
        let link = FakeLink::new();
        let mut device = Intellibox::new(Box::new(link.clone()), Generation::V5);
        let mut parameters = init();
        parameters.gear_shift_dim_percent = f64::NAN;
        parameters.led_brightness = -10;
        let native = NativeInitParameters::from_parameters(&parameters);
        let mut output = Vec::new();
        link.0.lock().unwrap().fail_writes = true;
        IntelliboxStream::send_native_init(&mut device, &parameters, Some(&native), &mut |text| {
            output.push(text.to_owned());
            Ok(())
        })
        .unwrap();
        assert_eq!(output, ["Initialization of the parameters of the Intellibox Failed\n"]);
        link.0.lock().unwrap().fail_writes = false;
        IntelliboxStream::send_native_init(&mut device, &parameters, Some(&native), &mut |_| {
            panic!("valid send does not print")
        })
        .unwrap();
        let native_bytes = link.take_written();
        let expected = parameters.args().unwrap();
        device.send(INITIALIZE_PARAMETERS, &expected).unwrap();
        assert_eq!(native_bytes, link.take_written());
        assert_eq!(expected[..2], [Arg::Number(15.0), Arg::Number(0.0)]);
    }

    #[test]
    fn native_init_failed_print_escapes_after_committing_raw_values() {
        let link = FakeLink::new();
        let mut device = Intellibox::new(Box::new(link.clone()), Generation::V5);
        let mut stream = IntelliboxStream::new();
        let mut native = NativeInitParameters::from_parameters(&init());
        native.fan_frequency = Err(super::super::led::NativeLedError { class: "ValueError", message: "bad".into() });
        let result = stream.send_frame_with_output(
            &mut device,
            2,
            &frame(),
            &init(),
            &RANGES,
            None,
            None,
            Some(&native),
            &mut |text| {
                assert_eq!(text, "Initialization of the parameters of the Intellibox Failed\n");
                Err(std::io::ErrorKind::BrokenPipe.into())
            },
        );
        assert!(
            matches!(result, Err(IntelliboxError::DiagnosticOutput(error)) if error.kind() == std::io::ErrorKind::BrokenPipe)
        );
        assert!(stream.previous_native_init.is_some());
        assert!(!stream.initialized);
        assert!(link.take_written().is_empty());
    }

    #[test]
    fn native_ranges_keep_type_sensitive_cache_and_numeric_aliases() {
        let link = FakeLink::new();
        let mut device = Intellibox::new(Box::new(link.clone()), Generation::V5);
        let mut stream = IntelliboxStream::new();
        let mut native = super::super::led::NativeSegmentRanges::from_positions(RANGES);
        stream.send_frame_native_ranges(&mut device, 2, &frame(), &init(), &RANGES, None, Some(&native)).unwrap();
        link.take_written();
        native.values[0] = Ok(LegacyValue::Str("1".into()));
        stream.send_frame_native_ranges(&mut device, 2, &frame(), &init(), &RANGES, None, Some(&native)).unwrap();
        assert_eq!(commands(&link.take_written()), ["4", "8"]);
        native.values[0] = Ok(LegacyValue::Int(1));
        stream.send_frame_native_ranges(&mut device, 2, &frame(), &init(), &RANGES, None, Some(&native)).unwrap();
        assert_eq!(commands(&link.take_written()), ["4", "8"]);
        native.values[0] = Ok(LegacyValue::Float(1.0));
        stream.send_frame_native_ranges(&mut device, 2, &frame(), &init(), &RANGES, None, Some(&native)).unwrap();
        assert_eq!(commands(&link.take_written()), ["4"]);
    }

    #[test]
    fn native_ranges_required_hidden_field_preserves_cold_and_hot_prefixes() {
        let error = super::super::led::NativeLedError { class: "KeyError", message: "'segment4_start'".into() };
        let mut native = super::super::led::NativeSegmentRanges::from_positions(RANGES);
        native.values[6] = Err(error.clone());
        let link = FakeLink::new();
        let mut device = Intellibox::new(Box::new(link.clone()), Generation::V5);
        let mut stream = IntelliboxStream::new();
        assert!(
            matches!(stream.send_frame_native_ranges(&mut device, 2, &frame(), &init(), &RANGES, None, Some(&native)),
            Err(IntelliboxError::NativeRange(ref got)) if got == &error)
        );
        assert_eq!(commands(&link.take_written()), ["7"]);
        assert!(stream.previous_native_segments.is_none());
        let good = super::super::led::NativeSegmentRanges::from_positions(RANGES);
        stream.send_frame_native_ranges(&mut device, 2, &frame(), &init(), &RANGES, None, Some(&good)).unwrap();
        link.take_written();
        let cached = stream.previous_native_segments.clone();
        assert!(stream
            .send_frame_native_ranges(&mut device, 2, &frame(), &init(), &RANGES, None, Some(&native))
            .is_err());
        assert_eq!(commands(&link.take_written()), ["4"]);
        assert_eq!(stream.previous_native_segments, cached);
    }

    #[test]
    fn native_ranges_conversion_failure_clears_cache_and_recovers() {
        let error = super::super::led::NativeLedError {
            class: "ValueError",
            message: "invalid literal for int() with base 10: 'bad'".into(),
        };
        let good = super::super::led::NativeSegmentRanges::from_positions(RANGES);
        let mut bad = good.clone();
        bad.values[6] = Ok(LegacyValue::Str("bad".into()));
        bad.positions[6] = Err(error);
        let link = FakeLink::new();
        let mut device = Intellibox::new(Box::new(link.clone()), Generation::V5);
        let mut stream = IntelliboxStream::new();
        stream.send_frame_native_ranges(&mut device, 2, &frame(), &init(), &RANGES, None, Some(&bad)).unwrap();
        assert_eq!(commands(&link.take_written()), ["7", "4", "5"]);
        assert!(!stream.initialized, "the later range retry failure leaves initialization false");
        assert!(stream.previous_native_segments.is_none());
        stream.send_frame_native_ranges(&mut device, 2, &frame(), &init(), &RANGES, None, Some(&good)).unwrap();
        assert_eq!(commands(&link.take_written()), ["7", "8", "4"]);
        assert!(stream.initialized);
    }

    #[test]
    fn native_gear_text_and_bytes_keep_the_numeric_stream_payload() {
        let link = FakeLink::new();
        let mut ib = Intellibox::new(Box::new(link.clone()), Generation::V5);
        IntelliboxStream::new().send_frame(&mut ib, 2, &frame(), &init(), &RANGES).unwrap();
        let expected = link.take_written();
        for gear in [LegacyValue::Str(" +٣ ".into()), LegacyValue::Bytes(b"+3".to_vec())] {
            let input = StreamFrame { harmonized_gear: f64::NAN, ..frame() };
            IntelliboxStream::new()
                .send_frame_with_gear_value(&mut ib, 2, &input, &init(), &RANGES, Some(&gear))
                .unwrap();
            assert_eq!(link.take_written(), expected);
        }
    }

    #[test]
    fn invalid_native_gear_keeps_initialization_and_led_failure_prefixes() {
        for gear in [LegacyValue::Str("bad".into()), LegacyValue::Bytes(vec![0xff]), LegacyValue::None] {
            let link = FakeLink::new();
            let mut ib = Intellibox::new(Box::new(link.clone()), Generation::V5);
            let mut stream = IntelliboxStream::new();
            let input = StreamFrame { harmonized_gear: f64::NAN, ..frame() };
            stream.send_frame_with_gear_value(&mut ib, 2, &input, &init(), &RANGES, Some(&gear)).unwrap();
            assert_eq!(commands(&link.take_written()), ["7", "8", "8", "5"]);
            stream.send_frame(&mut ib, 2, &frame(), &init(), &RANGES).unwrap();
            assert_eq!(commands(&link.take_written()), ["4"]);
        }
    }

    fn init() -> InitParameters {
        InitParameters { gear_shift_dim_percent: 70.0, led_brightness: 100, led_count: 150, fan_frequency: 3 }
    }

    const RANGES: SegmentRanges = [1, 50, 51, 100, 101, 125, 126, 150];

    fn commands(written: &[u8]) -> Vec<String> {
        written
            .split(|b| *b == b';')
            .filter(|f| !f.is_empty())
            .map(|f| String::from_utf8_lossy(&f[..f.iter().position(|b| *b == b',').unwrap_or(f.len())]).into_owned())
            .collect()
    }

    /// Stream sending: the first frame sends init + segments + stream + led; then only the
    /// stream until something changes; init re-check every 10 frames.
    #[test]
    fn clamped_auxiliary_maps_bound_initialization_brightness() {
        let mut parameters = init();
        parameters.gear_shift_dim_percent = 200.0;
        parameters.led_brightness = -10;
        assert_eq!(parameters.args().unwrap()[..2], [Arg::Number(15.0), Arg::Number(0.0)]);
        parameters.gear_shift_dim_percent = f64::NAN;
        parameters.led_brightness = 200;
        assert_eq!(parameters.args().unwrap()[..2], [Arg::Number(15.0), Arg::Number(255.0)]);
    }

    #[test]
    fn stream_sends_changes_only() {
        let link = FakeLink::new();
        let mut ib = Intellibox::new(Box::new(link.clone()), Generation::V5);
        let mut stream = IntelliboxStream::new();
        stream.send_frame(&mut ib, 2, &frame(), &init(), &RANGES).unwrap();
        assert_eq!(commands(&link.take_written()), vec!["7", "8", "4", "5"]);
        for _ in 0..8 {
            stream.send_frame(&mut ib, 2, &frame(), &init(), &RANGES).unwrap();
        }
        assert_eq!(commands(&link.take_written()), vec!["4"; 8]);
        let mut changed = init();
        changed.led_brightness = 50;
        stream.send_frame(&mut ib, 2, &frame(), &changed, &RANGES).unwrap();
        assert_eq!(commands(&link.take_written()), vec!["4", "7"]);
        let mut f = frame();
        f.segments[0].lit = 30.into();
        stream.send_frame(&mut ib, 2, &f, &changed, &RANGES).unwrap();
        assert_eq!(commands(&link.take_written()), vec!["4", "5"]);
    }

    /// initialize_parameters / set_led_segments / send_led / send_stream payload bytes.
    #[test]
    fn payload_bytes() {
        let link = FakeLink::new();
        let mut ib = Intellibox::new(Box::new(link.clone()), Generation::V5);
        let mut stream = IntelliboxStream::new();
        stream.send_frame(&mut ib, THREE_FAN_MODEL, &frame(), &init(), &RANGES).unwrap();
        let w = link.take_written();
        // initialize_parameters: 70 of 0..100 in 0..15 truncates to 10; then 255, 150, 3
        // (NUL bytes inside fields are escaped with '/')
        assert!(w.starts_with(b"7,\x0a/\x00,\xff/\x00,\x96/\x00,\x03/\x00;"));
        // set_led_segments: 1-based ranges minus one (0, 49, 50, 99, ...)
        assert!(w.windows(10).any(|x| x == b"8,/\x00/\x00,1/\x00"));
        // send_stream on model 6: centre fan (700 = 0x02BC) replaces the rpm tone
        let stream_cmd: Vec<u8> = b"4,\xff\x01,/,\x01,\x03/\x00,\xbc\x02,/\x00/\x00,".to_vec();
        assert!(w.windows(stream_cmd.len()).any(|x| x == stream_cmd.as_slice()));
        // send_led: nbr_leds_to_light + 1 = 25 for the first segment
        let led_cmd: Vec<u8> = b"5,\x19/\x00,/\x00/\x00\xff/\x00,".to_vec();
        assert!(w.windows(led_cmd.len()).any(|x| x == led_cmd.as_slice()));
    }

    /// send_stream with an unknown-type segment: the stream fails (state forgotten), the LED check is an error.
    #[test]
    fn segment_without_colours_fails_frame() {
        let link = FakeLink::new();
        let mut ib = Intellibox::new(Box::new(link.clone()), Generation::V5);
        let mut stream = IntelliboxStream::new();
        let mut f = frame();
        f.segments[3].colors = None;
        assert!(matches!(
            stream.send_frame(&mut ib, 2, &f, &init(), &RANGES),
            Err(IntelliboxError::SegmentWithoutColors(4))
        ));
        assert_eq!(commands(&link.take_written()), vec!["7", "8", "4"]);
        f.segments[3].colors = Some((Some(0.into()), Some(0.into())));
        f.segments[0].colors = None;
        assert!(stream.send_frame(&mut ib, 2, &f, &init(), &RANGES).is_err());
        // The stream command was not sent; the forgotten segment ranges were sent again.
        assert_eq!(commands(&link.take_written()), vec!["8"]);
        assert!(stream.previous_init.is_none());
    }

    /// The `2341:8036` identity test (table and identification) and
    /// the expected firmware version.
    #[test]
    fn generation_and_shipped_firmware() {
        assert_eq!(Generation::for_hwid("USB VID:PID=2341:8036 SER=1"), Generation::V6);
        assert_eq!(Generation::for_hwid("USB VID:PID=1A86:7523"), Generation::V5);
        assert_eq!(Generation::for_hwid("USB VID:PID=0403:6001 SER=A1"), Generation::V5);
        assert_eq!(shipped_firmware("USB VID:PID=2341:8036"), 1);
        assert_eq!(shipped_firmware("USB VID:PID=1A86:7523"), 19);
    }

    fn scan_with(reply: &[u8], hwid_port: PortEntry, baud: u32) -> (Option<Connected>, FakeLink, FakeOpener) {
        let link = FakeLink::new();
        link.queue_reply(reply);
        link.queue_reply(b"3,\x13\x00;");
        link.queue_reply(b"10,\x02\x00;");
        let opener = FakeOpener::with(&hwid_port.device, &link);
        let ports = FakePorts(vec![PortEntry::usb("/dev/ttyACM0", 0x2341, 0x0043, None, None), hwid_port]);
        let clock = ManualClock::default();
        let claimed = ClaimedPorts::default();
        let ctx = ScanContext {
            ports: &ports,
            opener: &opener,
            clock: &clock,
            claimed: &claimed,
            cached_port: None,
            previous_version: -1,
            previous_model: -1,
        };
        let settings = ScanSettings { com_port: String::new(), allowed: AllowedPorts::default() };
        let mut rng = rand::rngs::SmallRng::seed_from_u64(1);
        let outcome =
            IntelliboxScanner::new().scan(&ctx, &settings, baud, &mut rng, &mut |_| panic!("no flashing")).unwrap();
        (outcome, link, opener)
    }

    /// Scan: v5 identification, firmware 19 matches, model read, 28800 baud, 10 s timeout.
    #[test]
    fn scan_connects_genuine_v5_box() {
        let port = PortEntry::usb("/dev/ttyUSB3", 0x1a86, 0x7523, None, None);
        let (outcome, link, opener) = scan_with(b"1,srs;", port, 28800);
        let found = outcome.expect("a connection");
        assert_eq!((found.port.as_str(), found.model, found.version), ("/dev/ttyUSB3", 2, 19));
        assert_eq!(commands(&link.written()), vec!["0", "2", "9"]);
        assert_eq!(
            opener.opened(),
            vec![("/dev/ttyUSB3".to_string(), PortSettings { baud_rate: 28800, read_timeout: Some(READ_TIMEOUT) })]
        );
    }

    /// Scan: wrong name -> not found; firmware 17 != 19 -> flashed; Leonardo at 9600 skipped.
    #[test]
    fn scan_rejects_and_reports_mismatch() {
        let port = PortEntry::usb("/dev/ttyUSB3", 0x0403, 0x6001, None, None);
        assert!(scan_with(b"1,abc;", port.clone(), 28800).0.is_none());
        let link = FakeLink::new();
        link.queue_reply(b"1,srs;");
        link.queue_reply(b"3,\x11\x00;");
        let opener = FakeOpener::with("/dev/ttyUSB3", &link);
        let ports = FakePorts(vec![port]);
        let clock = ManualClock::default();
        let claimed = ClaimedPorts::default();
        let ctx = ScanContext {
            ports: &ports,
            opener: &opener,
            clock: &clock,
            claimed: &claimed,
            cached_port: None,
            previous_version: -1,
            previous_model: -1,
        };
        let settings = ScanSettings { com_port: String::new(), allowed: AllowedPorts::default() };
        let mut rng = rand::rngs::SmallRng::seed_from_u64(1);
        let mut flashed = Vec::new();
        let found = IntelliboxScanner::new()
            .scan(&ctx, &settings, 28800, &mut rng, &mut |p| flashed.push(p.to_string()))
            .unwrap();
        assert!(found.is_none());
        assert_eq!(flashed, vec!["/dev/ttyUSB3"]);
        let leonardo = PortEntry::usb("/dev/ttyACM1", 0x2341, 0x8036, None, None);
        let (outcome, _, opener) = scan_with(b"1,srs;", leonardo, 9600);
        assert!(outcome.is_none());
        assert!(opener.opened().is_empty());
    }

    /// The `i_am` answer of a v6 box: identifier = hex(XTEA("<nonce>^<status>")), product, model, version,
    /// status byte, serial (`intellibox_v6` format `sbbbbs`).
    fn v6_answer(plaintext: &str, model: u8, version: u8) -> Vec<u8> {
        let mut answer = format!("1,{},\x05,", seal_for_tests(plaintext)).into_bytes();
        answer.extend_from_slice(&[model, b',', version, b',', 1, b',']);
        answer.extend_from_slice(b"SN42;");
        answer
    }

    /// The nonce `scan_with`'s generator (seed 1) draws for the first box.
    fn first_nonce() -> String {
        new_handshake(&mut rand::rngs::SmallRng::seed_from_u64(1))
    }

    /// v6 identification through the scan: a Leonardo box answering a '^'-separated
    /// XTEA identifier for its nonce is named 'srs'; version and model come from the `i_am` answer
    /// (no further requests); the expected v6 firmware is 1.
    #[test]
    fn scan_connects_genuine_v6_box() {
        // Status 1 and 0 both connect: the status is not judged.
        for status in [1, 0] {
            let leonardo = PortEntry::usb("/dev/ttyACM1", 0x2341, 0x8036, None, None);
            let answer = v6_answer(&format!("{}^{status}", first_nonce()), 3, SHIPPED_FIRMWARE_V6 as u8);
            let (outcome, link, _) = scan_with(&answer, leonardo, 28800);
            let found = outcome.expect("a v6 connection");
            assert_eq!((found.port.as_str(), found.model, found.version), ("/dev/ttyACM1", 3, 1));
            let written = link.written();
            assert_eq!(commands(&written), vec!["0"]);
            assert_eq!(written, format!("0,{};", first_nonce()).into_bytes());
        }
    }

    /// v6 identification: a wrong nonce, a '\n' separator or a non-integer status
    /// leave the box unnamed.
    #[test]
    fn v6_identification_rejections() {
        let leonardo = PortEntry::usb("/dev/ttyACM1", 0x2341, 0x8036, None, None);
        for plaintext in
            ["xxxxxxxxxxxxxx^1".to_string(), format!("{}\n1", first_nonce()), format!("{}^x", first_nonce())]
        {
            let answer = v6_answer(&plaintext, 3, 1);
            assert!(scan_with(&answer, leonardo.clone(), 28800).0.is_none(), "{plaintext:?}");
        }
    }
}
