//! HaptiCR: four haptic motors, one per wheel, on an ESP32 driven over CmdMessenger.

use std::time::Duration;

use tracing::debug;

use openrigcue_model::native_value::{BigInt, LegacyValue};
use openrigcue_model::python_compare::native_equal;
use openrigcue_model::Telemetry;

use super::cmd_messenger::{self, Arg, CmdError, CommandSpec, Field};
use super::link::{Clock, PortLister, PortOpener, PortSettings, SerialLink};
use super::numeric::{map_clamped, DivisionByZero};
use super::open_gate::ask_identity;
use super::port_scan::{AllowedPorts, ClaimedPorts, Device, Exclusions, ScanRules};
use super::status::DeviceStatus;

use Field::{Byte as B, Text as S};

/// Command table of the HaptiCR firmware.
pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec { name: "who_are_you", fields: &[S] },
    CommandSpec { name: "i_am", fields: &[S, B, B, B, B, S] },
    CommandSpec { name: "send_message", fields: &[B, B, B, B, B, B, B, B, B] },
    CommandSpec { name: "received_std_message", fields: &[S, B, B, B, B, B, B, B, B, B] },
    CommandSpec { name: "received_remote_message", fields: &[S, B] },
    CommandSpec { name: "logger", fields: &[S] },
    CommandSpec { name: "gen_hash", fields: &[] },
    // Firmware command slot 7 (its firmware name); OpenRigCue never sends it.
    CommandSpec { name: "write_license_key", fields: &[S] },
    CommandSpec { name: "error", fields: &[S] },
];

const WHO_ARE_YOU: usize = 0;
const I_AM: usize = 1;
const SEND_MESSAGE: usize = 2;

/// `i_am` product byte of the HaptiCR.
const HAPTICR_PRODUCT: i64 = 7;
/// Minimum firmware version.
pub const SHIPPED_FIRMWARE: i64 = 1;
/// USB id of the ESP32 board's CH9102 bridge.
const USB_ID: &str = "1A86:55D4";
const BAUD_RATE: u32 = 115_200;
const READ_TIMEOUT: Duration = Duration::from_secs(1);
const SETTLE_TIME: Duration = Duration::from_secs(2);

/// Effect sliders are divided by this: slider 18 gives full scale.
const SLIDER_DIVIDER: i64 = 18;
/// PWM sent to the motor under test (`haptic_test_mode`).
const TEST_PWM: i64 = 250;

/// The wheels in motor order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wheel {
    FrontLeft,
    FrontRight,
    RearLeft,
    RearRight,
}

impl Wheel {
    /// The wheel codes in motor order: `lf`, `rf`, `lr`, `rr`.
    pub const ALL: [Wheel; 4] = [Wheel::FrontLeft, Wheel::FrontRight, Wheel::RearLeft, Wheel::RearRight];

    fn is_front(self) -> bool {
        matches!(self, Wheel::FrontLeft | Wheel::FrontRight)
    }
}

/// One effect's switch and slider (the tune keys `haptic_switch.wheel_<x>` and `haptic_effects.wheel_<x>`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EffectLevel {
    pub enabled: bool,
    /// 0..20.
    pub level: i64,
}

/// HaptiCR tuning of the game profile.
#[derive(Debug, Clone, PartialEq)]
pub struct HapticTuning {
    pub wheel_slip: EffectLevel,
    pub abs_lock: EffectLevel,
    pub gear: EffectLevel,
    pub heave: EffectLevel,
    pub rumble_strips: EffectLevel,
    pub surge_acceleration: EffectLevel,
    pub surge_deceleration: EffectLevel,
    pub traction_loss: EffectLevel,
    pub engine: EffectLevel,
    /// Engine band, percent of max rpm (`rpm_start_rumble`, `rpm_start_fading`, `rpm_stop_fading`).
    pub rpm_start_rumble: i64,
    pub rpm_start_fading: i64,
    pub rpm_stop_fading: i64,
    /// Activation thresholds in percent.
    pub wheel_slip_min_activation: i64,
    pub wheel_lock_min_activation: i64,
    pub traction_loss_min_activation: i64,
    pub native_engine_rpm: Option<NativeEngineRpm>,
    pub native_engine_control: Option<NativeEngineControl>,
    /// ABS, wheel slip, rumble strips and gear, in their early-return order.
    pub native_event_controls: Option<[NativeEngineControl; 4]>,
    /// Acceleration surge, deceleration surge, heave and traction loss.
    pub native_continuous_controls: Option<[NativeEngineControl; 4]>,
    pub native_traction_maximum: Option<super::NativeTractionMaximum>,
    pub native_map_maxima: Option<super::NativeMapMaxima>,
    /// Raw profile retained until each required operation is reached.
    pub native_profile: Option<LegacyValue>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct NativeEngineError {
    pub class: &'static str,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeEngineRpm {
    /// `rpm_start_rumble`, `rpm_start_fading` and `rpm_stop_fading` as integers of any width.
    pub start: Result<BigInt, NativeEngineError>,
    pub fade_start: Result<BigInt, NativeEngineError>,
    pub fade_end: Result<BigInt, NativeEngineError>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeEngineControl {
    pub enabled: Result<bool, NativeEngineError>,
    /// The level as an integer of any width.
    pub level: Result<BigInt, NativeEngineError>,
}

impl NativeEngineControl {
    pub(crate) fn selected_level(&self) -> Result<Option<BigInt>, NativeEngineError> {
        if self.enabled.clone()? {
            self.level.clone().map(Some)
        } else {
            Ok(None)
        }
    }
}

pub(crate) enum EngineLevelError {
    Math(DivisionByZero),
    Native(NativeEngineError),
}

impl From<DivisionByZero> for EngineLevelError {
    fn from(error: DivisionByZero) -> Self {
        Self::Math(error)
    }
}

impl From<NativeEngineError> for EngineLevelError {
    fn from(error: NativeEngineError) -> Self {
        Self::Native(error)
    }
}

/// Normalisation maxima (the tune's `max_telemetry_vars`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HapticMaxima {
    pub surge_acceleration: f64,
    pub surge_deceleration: f64,
    pub heave: f64,
    pub traction_loss: f64,
}

/// What one wheel's motor sees.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WheelInputs {
    pub abs_lock: f64,
    pub rumble_strips: f64,
    pub gear_change: bool,
    pub surge: f64,
    pub heave: f64,
    pub wheel_slip: f64,
    pub rpm: f64,
    pub max_rpm: f64,
    /// Normalised traction loss (sign selects the rear wheel).
    pub traction_loss: f64,
    /// `wheel_slip_min_activation / 25`.
    pub wheel_slip_min_activation: f64,
}

/// The engine contribution shared by HaptiCR and SIM3D: zero
/// below the start, a ramp with rpm up to the fade start, a fade from full to `floor` up to the fade end,
/// and the unscaled floor above it. The rpm percentage is clamped before band selection.
pub(crate) fn engine_level(
    rpm: f64,
    max_rpm: f64,
    slider: f64,
    divider: f64,
    start_rumble: i64,
    start_fading: i64,
    stop_fading: i64,
) -> Result<Option<f64>, DivisionByZero> {
    let rpm_perc = map_clamped(rpm, 0.0, max_rpm, 0.0, 100.0);
    engine_from_percent(rpm_perc, slider, divider, start_rumble, start_fading, stop_fading)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn engine_level_native(
    rpm: f64,
    max_rpm: f64,
    slider: f64,
    divider: f64,
    start_rumble: i64,
    start_fading: i64,
    stop_fading: i64,
) -> Result<Option<f64>, EngineLevelError> {
    engine_level(rpm, max_rpm, slider, divider, start_rumble, start_fading, stop_fading).map_err(Into::into)
}

fn engine_from_percent(
    rpm_perc: f64,
    slider: f64,
    divider: f64,
    start_rumble: i64,
    start_fading: i64,
    stop_fading: i64,
) -> Result<Option<f64>, DivisionByZero> {
    let (start, fade_start, fade_end) = (start_rumble as f64, start_fading as f64, stop_fading as f64);
    let floor = super::numeric::divide(2.0 * (100.0 - fade_end), 100.0 - fade_start)?;
    Ok(if rpm_perc < start {
        Some(0.0)
    } else if rpm_perc >= start && rpm_perc <= fade_start {
        Some(map_clamped(rpm_perc, 0.0, 100.0, 0.0, 255.0) * slider / divider)
    } else if rpm_perc > fade_start && rpm_perc <= fade_end {
        Some(map_clamped(rpm_perc, fade_end, fade_start, floor * 255.0, 255.0) * slider / divider)
    } else if rpm_perc > fade_end {
        Some(floor * 255.0)
    } else {
        None
    })
}

/// Motor level of one wheel (unclamped): ABS lock, wheel slip, rumble strips, gear change and engine return
/// early in that order; otherwise acceleration (rear) or braking (front) surge, heave and traction loss
/// (traction loss < 0 on the rear left, > 0 on the rear right) are summed.
pub fn wheel_level(
    wheel: Wheel,
    inputs: &WheelInputs,
    tuning: &HapticTuning,
    maxima: &HapticMaxima,
) -> Result<f64, HapticError> {
    wheel_level_values(wheel, inputs, tuning, maxima, None)
}

fn wheel_level_values(
    wheel: Wheel,
    inputs: &WheelInputs,
    tuning: &HapticTuning,
    maxima: &HapticMaxima,
    telemetry: Option<(&Telemetry, &str, &str)>,
) -> Result<f64, HapticError> {
    use super::values as v;
    let profile = tuning.native_profile.as_ref();
    let raw =
        |key, projection| telemetry.map_or(LegacyValue::Float(projection), |(t, _, _)| v::channel(t, key, projection));
    let heave = raw("vertical_acceleration", inputs.heave).py_mul(&LegacyValue::Int(3)).map_err(v::operation)?;
    let raw_rumble = raw(telemetry.map_or("rumble_pitch_lf", |(_, _, key)| key), inputs.rumble_strips);
    let rumble =
        if raw_rumble.py_gt(&LegacyValue::Int(5)).map_err(v::operation)? { LegacyValue::Int(1) } else { raw_rumble };
    let raw_slip = raw(telemetry.map_or("wheel_slip_lf", |(_, key, _)| key), inputs.wheel_slip);
    let slip = if raw_slip.py_gt(&LegacyValue::Float(inputs.wheel_slip_min_activation)).map_err(v::operation)? {
        LegacyValue::Int(1)
    } else {
        raw_slip
    };
    let abs = if wheel.is_front() { raw("abs_lock", inputs.abs_lock) } else { LegacyValue::Float(inputs.abs_lock) };
    for (index, (active, effect)) in [
        (native_equal(&abs, &LegacyValue::Int(1)), tuning.abs_lock),
        (native_equal(&slip, &LegacyValue::Int(1)), tuning.wheel_slip),
        (native_equal(&rumble, &LegacyValue::Int(1)), tuning.rumble_strips),
        (inputs.gear_change, tuning.gear),
    ]
    .into_iter()
    .enumerate()
    {
        if active {
            let key = ["wheel_abs_lock", "wheel_wheel_slip", "wheel_rumble_strips", "wheel_gear"][index];
            let level = v::selected(
                profile,
                ("haptic_switch", "haptic_effects"),
                key,
                tuning.native_event_controls.as_ref().map(|controls| &controls[index]),
                effect,
            )?;
            if let Some(level) = level {
                return v::full(&level, SLIDER_DIVIDER).map_err(Into::into);
            }
        }
    }
    let rpm = raw("rpm", inputs.rpm);
    let engine_level = if rpm.py_gt(&LegacyValue::Int(0)).map_err(v::operation)? {
        v::selected(
            profile,
            ("haptic_switch", "haptic_effects"),
            "wheel_engine",
            tuning.native_engine_control.as_ref(),
            tuning.engine,
        )?
    } else {
        None
    };
    if let Some(level) = engine_level {
        let engine = v::engine(
            profile,
            "haptic_effects",
            &rpm,
            &raw("max_rpm", inputs.max_rpm),
            &level,
            SLIDER_DIVIDER,
            [tuning.rpm_start_rumble, tuning.rpm_start_fading, tuning.rpm_stop_fading],
            tuning.native_engine_rpm.as_ref(),
        )
        .map_err(|error| {
            if profile.is_none() && error.class == "ZeroDivisionError" {
                HapticError::Math(DivisionByZero)
            } else {
                HapticError::NativeEngine(error)
            }
        })?;
        if let Some(level) = engine {
            return Ok(level);
        }
    }
    let selected = |index: usize, effect: EffectLevel| -> Result<LegacyValue, HapticError> {
        let key = ["wheel_surge_acceleration", "wheel_surge_deceleration", "wheel_heave", "wheel_traction_loss"][index];
        Ok(v::selected(
            profile,
            ("haptic_switch", "haptic_effects"),
            key,
            tuning.native_continuous_controls.as_ref().map(|controls| &controls[index]),
            effect,
        )?
        .unwrap_or(LegacyValue::Int(0)))
    };
    let raw_surge = raw("longitudinal_acceleration", inputs.surge);
    let mut surge = 0.0;
    if !wheel.is_front() && raw_surge.py_gt(&LegacyValue::Int(0)).map_err(v::operation)? {
        let slider = selected(0, tuning.surge_acceleration)?;
        let maximum = v::maximum(
            profile,
            "surge_acceleration",
            maxima.surge_acceleration,
            tuning.native_map_maxima.as_ref().map(|m| &m.surge_acceleration),
        )?;
        surge = v::scaled(
            map_clamped(raw_surge.py_as_double().map_err(v::operation)?, 0.0, maximum, 0.0, 255.0),
            &slider,
            SLIDER_DIVIDER,
        )?;
    }
    if wheel.is_front() && raw_surge.py_lt(&LegacyValue::Int(0)).map_err(v::operation)? {
        let slider = selected(1, tuning.surge_deceleration)?;
        let maximum = v::maximum(
            profile,
            "surge_deceleration",
            maxima.surge_deceleration,
            tuning.native_map_maxima.as_ref().map(|m| &m.surge_deceleration),
        )?;
        surge = v::scaled(
            map_clamped(
                raw_surge.py_abs().and_then(|v| v.py_as_double()).map_err(v::operation)?,
                0.0,
                maximum,
                0.0,
                255.0,
            ),
            &slider,
            SLIDER_DIVIDER,
        )?;
    }
    let heave_slider = selected(2, tuning.heave)?;
    let traction_slider = selected(3, tuning.traction_loss)?;
    let mut traction = 0.0;
    let picked = (inputs.traction_loss < 0.0 && wheel == Wheel::RearLeft)
        || (inputs.traction_loss > 0.0 && wheel == Wheel::RearRight);
    if !wheel.is_front() && picked {
        traction =
            v::scaled(map_clamped(inputs.traction_loss.abs(), 0.0, 1.0, 0.0, 255.0), &traction_slider, SLIDER_DIVIDER)?;
    }
    let maximum = v::maximum(profile, "heave", maxima.heave, tuning.native_map_maxima.as_ref().map(|m| &m.heave))?;
    let heave_level = v::scaled(
        map_clamped(heave.py_as_double().map_err(v::operation)?, 0.0, maximum, 0.0, 255.0),
        &heave_slider,
        SLIDER_DIVIDER,
    )?;
    Ok(surge + heave_level + traction)
}

/// Position validation: clamp to 0..=255.
fn clamp_pwm(value: i64) -> i64 {
    value.clamp(0, 255)
}

/// Switches the stream reads.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HapticSwitches {
    /// `haptic_test_mode`: 0, or 1..4 = wheel under test.
    pub test_mode: i64,
    /// `haptic_switch == 1`.
    pub on: bool,
    /// `listener_running` is a game (not 'none', 'emulator' or 'test').
    pub game_running: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum HapticError {
    #[error(transparent)]
    Send(#[from] CmdError),
    #[error("division by zero in the effect math")]
    Math(#[from] DivisionByZero),
    #[error(transparent)]
    NativeEngine(#[from] NativeEngineError),
    #[error("test mode {0} is not a wheel")]
    BadTestMode(i64),
    #[error("motor level is not a finite number")]
    NotFinite,
}

impl From<EngineLevelError> for HapticError {
    fn from(error: EngineLevelError) -> Self {
        match error {
            EngineLevelError::Math(error) => Self::Math(error),
            EngineLevelError::Native(error) => Self::NativeEngine(error),
        }
    }
}

/// An open HaptiCR with its gear-change memory.
pub struct HaptiCr {
    link: Box<dyn SerialLink>,
    /// `previous_gear` (starts at -1).
    previous_gear: LegacyValue,
}

impl HaptiCr {
    pub fn new(link: Box<dyn SerialLink>) -> Self {
        HaptiCr { link, previous_gear: LegacyValue::Int(-1) }
    }

    /// Sends the four PWM bytes padded with five zeros.
    fn send_levels(&mut self, levels: [i64; 4]) -> Result<(), CmdError> {
        let mut args: Vec<Arg> = levels.iter().map(|v| Arg::Number(*v as f64)).collect();
        args.extend(std::iter::repeat_n(Arg::Number(0.0), 5));
        cmd_messenger::send(self.link.as_mut(), COMMANDS, SEND_MESSAGE, &args)
    }

    /// Drive the four motors for one frame.
    ///
    /// The test wheel gets 250; with a game running each wheel
    /// gets its motor level (ABS only on the front wheels, traction loss only on the rear ones), clamped; the
    /// switch off sends zeros; no game sends nothing.
    pub fn send_stream(
        &mut self,
        telemetry: &Telemetry,
        switches: &HapticSwitches,
        tuning: &HapticTuning,
        maxima: &HapticMaxima,
    ) -> Result<(), HapticError> {
        if switches.test_mode != 0 {
            let index = usize::try_from(switches.test_mode - 1)
                .ok()
                .filter(|i| *i < 4)
                .ok_or(HapticError::BadTestMode(switches.test_mode))?;
            let mut levels = [0; 4];
            levels[index] = TEST_PWM;
            self.send_levels(levels.map(clamp_pwm))?;
            return Ok(());
        }
        if !switches.game_running {
            return Ok(());
        }
        let telemetry = super::haptic_stream_telemetry(telemetry);
        let gear = super::stream_gear(&telemetry);
        let gear_change = !native_equal(&self.previous_gear, &gear);
        let slip_activation = super::values::activation(
            tuning.native_profile.as_ref(),
            "haptic_effects",
            "wheel_slip_min_activation",
            tuning.wheel_slip_min_activation,
            25,
        )?;
        let traction_activation = super::values::activation(
            tuning.native_profile.as_ref(),
            "haptic_effects",
            "traction_loss_min_activation",
            tuning.traction_loss_min_activation,
            100,
        )?;
        let mut traction =
            super::traction_ratio(&telemetry, maxima.traction_loss, tuning.native_traction_maximum.as_ref(), false);
        if traction.abs() < traction_activation {
            traction = 0.0;
        }
        let mut levels = [0i64; 4];
        // The wheels are always computed.
        for (slot, wheel) in levels.iter_mut().zip(Wheel::ALL) {
            let (slip, rumble) = match wheel {
                Wheel::FrontLeft => (telemetry.wheel_slip_lf, telemetry.rumble_pitch_lf),
                Wheel::FrontRight => (telemetry.wheel_slip_rf, telemetry.rumble_pitch_rf),
                Wheel::RearLeft => (telemetry.wheel_slip_lr, telemetry.rumble_pitch_lr),
                Wheel::RearRight => (telemetry.wheel_slip_rr, telemetry.rumble_pitch_rr),
            };
            let inputs = WheelInputs {
                abs_lock: if wheel.is_front() { telemetry.abs_lock } else { 0.0 },
                rumble_strips: rumble,
                gear_change,
                surge: telemetry.longitudinal_acceleration,
                heave: telemetry.vertical_acceleration,
                wheel_slip: slip,
                rpm: telemetry.rpm,
                max_rpm: telemetry.max_rpm,
                traction_loss: traction,
                wheel_slip_min_activation: slip_activation,
            };
            let (slip_key, rumble_key) = match wheel {
                Wheel::FrontLeft => ("wheel_slip_lf", "rumble_pitch_lf"),
                Wheel::FrontRight => ("wheel_slip_rf", "rumble_pitch_rf"),
                Wheel::RearLeft => ("wheel_slip_lr", "rumble_pitch_lr"),
                Wheel::RearRight => ("wheel_slip_rr", "rumble_pitch_rr"),
            };
            *slot = super::values::clipped_integer(wheel_level_values(
                wheel,
                &inputs,
                tuning,
                maxima,
                Some((&telemetry, slip_key, rumble_key)),
            )?)
            .ok_or(HapticError::NotFinite)?;
        }
        if switches.on {
            self.send_levels(levels.map(clamp_pwm))?;
        } else {
            self.send_levels([0; 4])?;
        }
        self.previous_gear = gear;
        Ok(())
    }
}

/// A connected HaptiCR and what it reported.
pub struct ConnectedHaptiCr {
    pub port: String,
    pub model: i64,
    pub version: i64,
    pub device: HaptiCr,
}

/// Scan outcome.
pub enum HapticScan {
    Connected(ConnectedHaptiCr),
    /// A HaptiCR with firmware older than the minimum (manual update needed).
    FirmwareTooOld,
    NotFound,
}

/// Environment of a HaptiCR scan.
pub struct HapticScanContext<'a> {
    pub ports: &'a dyn PortLister,
    pub opener: &'a dyn PortOpener,
    pub clock: &'a dyn Clock,
    pub claimed: &'a ClaimedPorts,
    /// The port remembered from the last successful connection.
    pub cached_port: Option<&'a str>,
    /// `HAPTICR.com_port`.
    pub com_port: &'a str,
    pub allowed: &'a AllowedPorts,
    /// Publishes the HaptiCR status while the scan goes on (the firmware message is set
    /// when such a device is met, before the next port is tried).
    pub report: &'a dyn Fn(DeviceStatus),
}

/// Whether the next HaptiCR scan is the first one.
#[derive(Debug)]
pub struct HapticScanner {
    first_scan: bool,
}

impl Default for HapticScanner {
    fn default() -> Self {
        HapticScanner { first_scan: true }
    }
}

impl HapticScanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// CH9102 ports at 115200, the XTEA handshake, product 7, firmware at
    /// least the minimum. The status number of the answer is not judged.
    pub fn scan(
        &mut self,
        ctx: &HapticScanContext<'_>,
        rng: &mut impl rand::Rng,
    ) -> Result<HapticScan, super::link::PortScanError> {
        let rules = ScanRules {
            device: Device::HaptiCr,
            exclusions: Exclusions::RaspberryUart,
            configured_port: ctx.com_port,
            allowed: ctx.allowed,
            claimed: ctx.claimed,
            cached_port: ctx.cached_port,
            first_scan: self.first_scan,
        };
        let mut outcome = HapticScan::NotFound;
        for port in ctx.ports.ports()? {
            if !rules.may_try(&port) || !port.hwid_contains(USB_ID)? {
                continue;
            }
            let settings = PortSettings { baud_rate: BAUD_RATE, read_timeout: Some(READ_TIMEOUT) };
            let Ok(mut link) = ctx.opener.open(&port.device, settings) else { continue };
            ctx.clock.sleep(SETTLE_TIME);
            let Some(answer) = ask_identity(link.as_mut(), COMMANDS, WHO_ARE_YOU, I_AM, rng) else { continue };
            if answer.product != HAPTICR_PRODUCT {
                debug!("This is not the HapticR, disconnecting and trying next port");
                continue;
            }
            if answer.version < SHIPPED_FIRMWARE {
                tracing::warn!("HapticR firmware update required — manual flashing needed (HapticR discontinued)");
                (ctx.report)(DeviceStatus::FirmwareUpdateRequired);
                outcome = HapticScan::FirmwareTooOld;
                continue;
            }
            outcome = HapticScan::Connected(ConnectedHaptiCr {
                port: port.device.clone(),
                model: answer.model,
                version: answer.version,
                device: HaptiCr::new(link),
            });
            break;
        }
        self.first_scan = false;
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::super::link::fakes::{FakeLink, FakeOpener, FakePorts, ManualClock};
    use super::super::link::PortEntry;
    use super::super::open_gate::{new_handshake, seal_for_tests};
    use super::*;
    use rand::SeedableRng;

    fn level(level: i64) -> EffectLevel {
        EffectLevel { enabled: true, level }
    }

    fn tuning() -> HapticTuning {
        HapticTuning {
            wheel_slip: level(10),
            abs_lock: level(18),
            gear: level(9),
            heave: level(18),
            rumble_strips: level(10),
            surge_acceleration: level(18),
            surge_deceleration: level(18),
            traction_loss: level(18),
            engine: level(18),
            rpm_start_rumble: 0,
            rpm_start_fading: 70,
            rpm_stop_fading: 95,
            wheel_slip_min_activation: 30,
            wheel_lock_min_activation: 50,
            traction_loss_min_activation: 40,
            native_engine_rpm: None,
            native_engine_control: None,
            native_event_controls: None,
            native_continuous_controls: None,
            native_traction_maximum: None,
            native_map_maxima: None,
            native_profile: None,
        }
    }

    const MAXIMA: HapticMaxima =
        HapticMaxima { surge_acceleration: 5.0, surge_deceleration: 5.0, heave: 2.0, traction_loss: 5.0 };

    fn quiet() -> WheelInputs {
        WheelInputs {
            abs_lock: 0.0,
            rumble_strips: 0.0,
            gear_change: false,
            surge: 0.0,
            heave: 0.0,
            wheel_slip: 0.0,
            rpm: 0.0,
            max_rpm: 8000.0,
            traction_loss: 0.0,
            wheel_slip_min_activation: 1.2,
        }
    }

    /// Event priority and slider scale (255 * slider / 18).
    #[test]
    fn event_effects_return_early() {
        let mut i = quiet();
        i.abs_lock = 1.0;
        i.wheel_slip = 2.0;
        assert_eq!(wheel_level(Wheel::FrontLeft, &i, &tuning(), &MAXIMA).unwrap(), 255.0);
        i.abs_lock = 0.0;
        assert_eq!(wheel_level(Wheel::FrontLeft, &i, &tuning(), &MAXIMA).unwrap(), 2550.0 / 18.0);
        let mut g = quiet();
        g.rumble_strips = 6.0;
        assert_eq!(wheel_level(Wheel::RearRight, &g, &tuning(), &MAXIMA).unwrap(), 2550.0 / 18.0);
        let mut gear = quiet();
        gear.gear_change = true;
        assert_eq!(wheel_level(Wheel::RearRight, &gear, &tuning(), &MAXIMA).unwrap(), 255.0 * 9.0 / 18.0);
    }

    /// The rpm band: ramp, fade towards floor 2*(100-95)/(100-70), unscaled floor above.
    #[test]
    fn clamped_rumble_maps_keep_each_continuous_contribution_in_range() {
        let mut inputs = quiet();
        inputs.surge = 2.5;
        inputs.heave = -0.5;
        assert_eq!(wheel_level(Wheel::RearLeft, &inputs, &tuning(), &MAXIMA).unwrap(), 127.5);
        inputs.surge = 10.0;
        inputs.heave = 0.0;
        assert_eq!(wheel_level(Wheel::RearLeft, &inputs, &tuning(), &MAXIMA).unwrap(), 255.0);
        inputs.surge = 0.0;
        inputs.heave = f64::NAN;
        assert_eq!(wheel_level(Wheel::RearLeft, &inputs, &tuning(), &MAXIMA).unwrap(), 255.0);
    }

    #[test]
    fn clamped_rumble_engine_map_handles_zero_maximum_before_the_band_selection() {
        let expected = 2.0 * 5.0 / 30.0 * 255.0;
        assert_eq!(engine_level(8000.0, 0.0, 18.0, 18.0, 0, 70, 95).unwrap(), Some(expected));
        assert_eq!(engine_level(8000.0, 8000.0, 18.0, 18.0, 0, 100, 100), Err(DivisionByZero));
    }

    #[test]
    fn engine_band() {
        let mut i = quiet();
        i.rpm = 4000.0; // 50 %
        assert_eq!(wheel_level(Wheel::FrontLeft, &i, &tuning(), &MAXIMA).unwrap(), 127.5);
        i.rpm = 7600.0; // 95 %: fade end -> floor * 255
        let floor = 2.0 * 5.0 / 30.0 * 255.0;
        assert!((wheel_level(Wheel::FrontLeft, &i, &tuning(), &MAXIMA).unwrap() - floor).abs() < 1e-9);
        i.rpm = 8000.0;
        assert!((wheel_level(Wheel::FrontLeft, &i, &tuning(), &MAXIMA).unwrap() - floor).abs() < 1e-9);
        i.max_rpm = 0.0;
        assert!((wheel_level(Wheel::FrontLeft, &i, &tuning(), &MAXIMA).unwrap() - floor).abs() < 1e-9);
    }

    /// Continuous effects: surge direction per axle, traction loss sign per rear wheel.
    #[test]
    fn continuous_effects() {
        let mut i = quiet();
        i.surge = 2.5;
        assert_eq!(wheel_level(Wheel::RearLeft, &i, &tuning(), &MAXIMA).unwrap(), 127.5);
        assert_eq!(wheel_level(Wheel::FrontLeft, &i, &tuning(), &MAXIMA).unwrap(), 0.0);
        i.surge = -2.5;
        assert_eq!(wheel_level(Wheel::FrontRight, &i, &tuning(), &MAXIMA).unwrap(), 127.5);
        // Traction loss < 0 drives the rear left ('lr'), > 0 the rear right ('rr'); never a front wheel.
        let mut t = quiet();
        t.traction_loss = -0.5;
        assert_eq!(wheel_level(Wheel::RearLeft, &t, &tuning(), &MAXIMA).unwrap(), 127.5);
        assert_eq!(wheel_level(Wheel::RearRight, &t, &tuning(), &MAXIMA).unwrap(), 0.0);
        assert_eq!(wheel_level(Wheel::FrontLeft, &t, &tuning(), &MAXIMA).unwrap(), 0.0);
        t.traction_loss = 0.5;
        assert_eq!(wheel_level(Wheel::RearRight, &t, &tuning(), &MAXIMA).unwrap(), 127.5);
        assert_eq!(wheel_level(Wheel::RearLeft, &t, &tuning(), &MAXIMA).unwrap(), 0.0);
        assert_eq!(wheel_level(Wheel::FrontRight, &t, &tuning(), &MAXIMA).unwrap(), 0.0);
        let mut h = quiet();
        h.heave = 0.5; // tripled -> 1.5 of max 2
        assert_eq!(wheel_level(Wheel::RearLeft, &h, &tuning(), &MAXIMA).unwrap(), 191.25);
    }

    #[test]
    fn native_map_maxima_follow_wheel_and_control_prefixes() {
        let failure = |key| NativeEngineError { class: "KeyError", message: format!("'{key}'") };
        let mut tune = tuning();
        tune.native_map_maxima = Some(super::super::NativeMapMaxima {
            surge_acceleration: Err(failure("surge_acceleration")),
            surge_deceleration: Err(failure("surge_deceleration")),
            heave: Ok(2.0),
        });
        let mut inputs = quiet();
        inputs.surge = 1.0;
        assert_eq!(wheel_level(Wheel::FrontLeft, &inputs, &tune, &MAXIMA).unwrap(), 0.0);
        assert!(
            matches!(wheel_level(Wheel::RearLeft, &inputs, &tune, &MAXIMA), Err(HapticError::NativeEngine(error)) if error==failure("surge_acceleration"))
        );
        inputs.surge = -1.0;
        assert_eq!(wheel_level(Wheel::RearLeft, &inputs, &tune, &MAXIMA).unwrap(), 0.0);
        assert!(
            matches!(wheel_level(Wheel::FrontLeft, &inputs, &tune, &MAXIMA), Err(HapticError::NativeEngine(error)) if error==failure("surge_deceleration"))
        );
        inputs.surge = 0.0;
        tune.heave.enabled = false;
        tune.native_map_maxima.as_mut().unwrap().heave = Err(failure("heave"));
        assert!(
            matches!(wheel_level(Wheel::FrontLeft, &inputs, &tune, &MAXIMA), Err(HapticError::NativeEngine(error)) if error==failure("heave"))
        );
        tune.native_continuous_controls =
            Some(std::array::from_fn(|_| NativeEngineControl { enabled: Ok(false), level: Ok((0).into()) }));
        tune.native_continuous_controls.as_mut().unwrap()[3].enabled = Err(failure("wheel_traction_loss"));
        assert!(
            matches!(wheel_level(Wheel::FrontLeft, &inputs, &tune, &MAXIMA), Err(HapticError::NativeEngine(error)) if error==failure("wheel_traction_loss"))
        );
        inputs.gear_change = true;
        assert_eq!(wheel_level(Wheel::FrontLeft, &inputs, &tune, &MAXIMA).unwrap(), 127.5);
    }

    #[test]
    fn native_continuous_controls_follow_wheel_gates_and_read_order() {
        let failure = |key| NativeEngineError { class: "KeyError", message: format!("'{key}'") };
        let good = || NativeEngineControl { enabled: Ok(true), level: Ok((18).into()) };
        let mut tune = tuning();
        tune.native_continuous_controls = Some(std::array::from_fn(|_| good()));
        let mut inputs = quiet();
        for (index, wheel, surge, key) in [
            (0, Wheel::RearLeft, 1.0, "wheel_surge_acceleration"),
            (1, Wheel::FrontLeft, -1.0, "wheel_surge_deceleration"),
            (2, Wheel::FrontLeft, 0.0, "wheel_heave"),
            (3, Wheel::FrontLeft, 0.0, "wheel_traction_loss"),
        ] {
            inputs.surge = surge;
            let control = &mut tune.native_continuous_controls.as_mut().unwrap()[index];
            control.level = Err(failure(key));
            assert!(matches!(wheel_level(wheel, &inputs, &tune, &MAXIMA),
                Err(HapticError::NativeEngine(error)) if error == failure(key)));
            tune.native_continuous_controls.as_mut().unwrap()[index] = good();
        }
        for index in [0, 1] {
            tune.native_continuous_controls.as_mut().unwrap()[index].enabled = Err(failure("unreached surge"));
        }
        inputs.surge = 0.0;
        assert_eq!(wheel_level(Wheel::FrontLeft, &inputs, &tune, &MAXIMA).unwrap(), 0.0);
        inputs.surge = 1.0;
        assert_eq!(wheel_level(Wheel::FrontLeft, &inputs, &tune, &MAXIMA).unwrap(), 0.0);
        inputs.surge = -1.0;
        assert_eq!(wheel_level(Wheel::RearLeft, &inputs, &tune, &MAXIMA).unwrap(), 0.0);
        inputs.surge = 0.0;
        tune.native_continuous_controls.as_mut().unwrap()[2].level = Err(failure("wheel_heave"));
        tune.native_continuous_controls.as_mut().unwrap()[3].enabled = Err(failure("wheel_traction_loss"));
        assert!(matches!(wheel_level(Wheel::FrontLeft, &inputs, &tune, &MAXIMA),
            Err(HapticError::NativeEngine(error)) if error == failure("wheel_heave")));
        tune.native_continuous_controls.as_mut().unwrap()[2].enabled = Ok(false);
        assert!(
            matches!(wheel_level(Wheel::FrontLeft, &inputs, &tune, &MAXIMA),
            Err(HapticError::NativeEngine(error)) if error == failure("wheel_traction_loss")),
            "traction is read before the front-wheel/side gate"
        );
        inputs.gear_change = true;
        assert_eq!(wheel_level(Wheel::FrontLeft, &inputs, &tune, &MAXIMA).unwrap(), 127.5);
    }

    #[test]
    fn native_continuous_failure_retains_gear_and_skips_motor_output() {
        let bad =
            NativeEngineError { class: "ValueError", message: "invalid literal for int() with base 10: '9.5'".into() };
        let mut tune = tuning();
        tune.gear.enabled = false;
        tune.native_continuous_controls =
            Some(std::array::from_fn(|_| NativeEngineControl { enabled: Ok(true), level: Ok((18).into()) }));
        tune.native_continuous_controls.as_mut().unwrap()[2].level = Err(bad.clone());
        let link = FakeLink::new();
        let mut haptic = HaptiCr::new(Box::new(link.clone()));
        let mut telemetry = Telemetry::off();
        telemetry.gear = 4.0;
        telemetry.vertical_acceleration = 0.5;
        let switches = HapticSwitches { test_mode: 0, on: true, game_running: true };
        for on in [true, false] {
            assert!(matches!(haptic.send_stream(&telemetry, &HapticSwitches { on, ..switches }, &tune, &MAXIMA),
                Err(HapticError::NativeEngine(error)) if error == bad));
            assert!(link.take_written().is_empty());
            assert_eq!(haptic.previous_gear, LegacyValue::Int(-1));
        }
        haptic.send_stream(&telemetry, &HapticSwitches { game_running: false, ..switches }, &tune, &MAXIMA).unwrap();
        assert!(link.take_written().is_empty());
        haptic.send_stream(&telemetry, &HapticSwitches { test_mode: 2, ..switches }, &tune, &MAXIMA).unwrap();
        assert_eq!(sent(&link), vec![b'2', 0, 250, 0, 0, 0, 0, 0, 0, 0]);
        tune.gear.enabled = true;
        haptic.send_stream(&telemetry, &switches, &tune, &MAXIMA).unwrap();
        assert_eq!(sent(&link), vec![b'2', 127, 127, 127, 127, 0, 0, 0, 0, 0]);
        assert_eq!(haptic.previous_gear, LegacyValue::Float(4.0));
        assert!(matches!(haptic.send_stream(&telemetry, &switches, &tune, &MAXIMA),
            Err(HapticError::NativeEngine(error)) if error == bad));
        assert!(link.take_written().is_empty());
        tune.native_continuous_controls.as_mut().unwrap()[2].level = Ok((18).into());
        haptic.send_stream(&telemetry, &switches, &tune, &MAXIMA).unwrap();
        assert_eq!(sent(&link), vec![b'2', 191, 191, 191, 191, 0, 0, 0, 0, 0]);
    }

    fn sent(link: &FakeLink) -> Vec<u8> {
        let w = link.take_written();
        w.iter().copied().filter(|b| *b != b',' && *b != b';' && *b != b'/').collect()
    }

    /// Stream: test wheel, switch off zeros, no game nothing.
    #[test]
    fn stream_paths() {
        let link = FakeLink::new();
        let mut h = HaptiCr::new(Box::new(link.clone()));
        let mut sw = HapticSwitches { test_mode: 3, on: true, game_running: true };
        let t = Telemetry::off();
        h.send_stream(&t, &sw, &tuning(), &MAXIMA).unwrap();
        assert_eq!(sent(&link), vec![b'2', 0, 0, 250, 0, 0, 0, 0, 0, 0]);
        sw.test_mode = 0;
        sw.game_running = false;
        h.send_stream(&t, &sw, &tuning(), &MAXIMA).unwrap();
        assert!(link.take_written().is_empty());
        sw.game_running = true;
        sw.on = false;
        h.send_stream(&t, &sw, &tuning(), &MAXIMA).unwrap();
        assert_eq!(sent(&link), vec![b'2', 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        // gear 11 (off value) differs from the initial -1: one gear-change frame at 255*9/18 = 127.
        sw.on = true;
        let mut t2 = Telemetry::off();
        t2.gear = 3.0;
        h.send_stream(&t2, &sw, &tuning(), &MAXIMA).unwrap();
        assert_eq!(sent(&link), vec![b'2', 127, 127, 127, 127, 0, 0, 0, 0, 0]);
    }

    /// Stream: the traction-loss normalisation uses 0 when its division fails, so a zero
    /// traction-loss maximum still drives all four motors (here: 0.5 g of heave each).
    #[test]
    fn stream_with_zero_traction_maximum() {
        let link = FakeLink::new();
        let mut h = HaptiCr::new(Box::new(link.clone()));
        let sw = HapticSwitches { test_mode: 0, on: true, game_running: true };
        let mut t = Telemetry::off();
        t.gear = -1.0;
        t.lateral_velocity = 3.0;
        t.vertical_acceleration = 0.5;
        let maxima = HapticMaxima { traction_loss: 0.0, ..MAXIMA };
        h.send_stream(&t, &sw, &tuning(), &maxima).unwrap();
        // heave 0.5 * 3 of max 2 -> 191.25 on every wheel
        assert_eq!(sent(&link), vec![b'2', 191, 191, 191, 191, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn native_event_controls_keep_priority_and_skip_unreached_failures() {
        let error = |name| NativeEngineError { class: "KeyError", message: format!("'{name}'") };
        let control = |level: i64| NativeEngineControl { enabled: Ok(true), level: Ok((level).into()) };
        let mut tune = tuning();
        tune.native_event_controls = Some([control(9), control(6), control(3), control(1)]);
        let mut input = quiet();
        input.abs_lock = 1.0;
        input.wheel_slip = 2.0;
        input.rumble_strips = 6.0;
        input.gear_change = true;
        for (index, expected) in [(0, 127.5), (1, 85.0), (2, 42.5), (3, 255.0 / 18.0)] {
            assert_eq!(wheel_level(Wheel::FrontLeft, &input, &tune, &MAXIMA).unwrap(), expected);
            tune.native_event_controls.as_mut().unwrap()[index].enabled = Ok(false);
        }
        tune.native_event_controls = Some(std::array::from_fn(|_| NativeEngineControl {
            enabled: Err(error("switch")),
            level: Err(error("level")),
        }));
        assert!(
            matches!(wheel_level(Wheel::FrontLeft,&input,&tune,&MAXIMA),Err(HapticError::NativeEngine(found)) if found==error("switch"))
        );
        let controls = tune.native_event_controls.as_mut().unwrap();
        controls[0].enabled = Ok(false);
        assert!(
            matches!(wheel_level(Wheel::FrontLeft,&input,&tune,&MAXIMA),Err(HapticError::NativeEngine(found)) if found==error("switch"))
        );
        tune.native_event_controls.as_mut().unwrap()[1].enabled = Ok(true);
        assert!(
            matches!(wheel_level(Wheel::FrontLeft,&input,&tune,&MAXIMA),Err(HapticError::NativeEngine(found)) if found==error("level"))
        );
        assert!(wheel_level(Wheel::FrontLeft, &quiet(), &tune, &MAXIMA).is_ok());
    }

    #[test]
    fn native_event_failure_keeps_whole_motor_frame_and_gear_uncommitted() {
        let link = FakeLink::new();
        let mut device = HaptiCr::new(Box::new(link.clone()));
        let mut tune = tuning();
        tune.native_event_controls =
            Some(std::array::from_fn(|_| NativeEngineControl { enabled: Ok(true), level: Ok((9).into()) }));
        tune.native_event_controls.as_mut().unwrap()[1].level =
            Err(NativeEngineError { class: "ValueError", message: "bad slip level".into() });
        let mut telemetry = Telemetry::off();
        telemetry.gear = 4.0;
        telemetry.wheel_slip_rf = 2.0;
        let switches = HapticSwitches { test_mode: 0, on: true, game_running: true };
        assert!(matches!(device.send_stream(&telemetry, &switches, &tune, &MAXIMA), Err(HapticError::NativeEngine(_))));
        assert!(link.take_written().is_empty(), "earlier wheel calculation never partially sends");
        assert_eq!(device.previous_gear, LegacyValue::Int(-1));
        telemetry.wheel_slip_rf = 0.0;
        device.send_stream(&telemetry, &switches, &tune, &MAXIMA).unwrap();
        assert_eq!(sent(&link), vec![b'2', 127, 127, 127, 127, 0, 0, 0, 0, 0]);
        assert_eq!(device.previous_gear, LegacyValue::Float(4.0));
    }

    #[test]
    fn native_engine_control_is_reached_after_events_and_before_rpm_fields() {
        let fail = |name| NativeEngineError { class: "KeyError", message: format!("'{name}'") };
        let mut tune = tuning();
        tune.engine.enabled = false;
        tune.native_engine_control =
            Some(NativeEngineControl { enabled: Err(fail("wheel_engine")), level: Err(fail("level")) });
        tune.native_engine_rpm = Some(NativeEngineRpm {
            start: Err(fail("rpm_start_rumble")),
            fade_start: Ok((70).into()),
            fade_end: Ok((95).into()),
        });
        let mut input = quiet();
        input.rpm = 4000.0;
        assert!(
            matches!(wheel_level(Wheel::FrontLeft,&input,&tune,&MAXIMA),Err(HapticError::NativeEngine(error)) if error == fail("wheel_engine"))
        );
        tune.native_engine_control.as_mut().unwrap().enabled = Ok(false);
        assert_eq!(wheel_level(Wheel::FrontLeft, &input, &tune, &MAXIMA).unwrap(), 0.0);
        tune.native_engine_control.as_mut().unwrap().enabled = Ok(true);
        assert!(
            matches!(wheel_level(Wheel::FrontLeft,&input,&tune,&MAXIMA),Err(HapticError::NativeEngine(error)) if error == fail("level"))
        );
        input.abs_lock = 1.0;
        assert!(wheel_level(Wheel::FrontLeft, &input, &tune, &MAXIMA).is_ok());
        input.abs_lock = 0.0;
        input.rpm = 0.0;
        assert!(wheel_level(Wheel::FrontLeft, &input, &tune, &MAXIMA).is_ok());
        input.rpm = 4000.0;
        tune.native_engine_control.as_mut().unwrap().level = Ok((9).into());
        assert!(
            matches!(wheel_level(Wheel::FrontLeft,&input,&tune,&MAXIMA),Err(HapticError::NativeEngine(error)) if error == fail("rpm_start_rumble"))
        );
    }

    #[test]
    fn native_rpm_fields_precede_floor_division_in_required_order() {
        let error = |name| NativeEngineError { class: "KeyError", message: format!("'{name}'") };
        let mut native = NativeEngineRpm {
            start: Err(error("rpm_start_rumble")),
            fade_start: Err(error("rpm_start_fading")),
            fade_end: Err(error("rpm_stop_fading")),
        };
        let mut tune = tuning();
        tune.engine.enabled = true;
        tune.native_engine_rpm = Some(native.clone());
        let mut input = quiet();
        input.rpm = 1.0;
        for name in ["rpm_start_rumble", "rpm_start_fading", "rpm_stop_fading"] {
            tune.native_engine_rpm = Some(native.clone());
            assert!(
                matches!(wheel_level(Wheel::FrontLeft, &input, &tune, &MAXIMA), Err(HapticError::NativeEngine(found)) if found == error(name))
            );
            match name {
                "rpm_start_rumble" => native.start = Ok(0.into()),
                "rpm_start_fading" => native.fade_start = Ok(100.into()),
                _ => native.fade_end = Ok(95.into()),
            }
        }
        tune.native_engine_rpm = Some(native);
        assert!(matches!(wheel_level(Wheel::FrontLeft, &input, &tune, &MAXIMA), Err(HapticError::Math(_))));
        input.rpm = 0.0;
        assert!(wheel_level(Wheel::FrontLeft, &input, &tune, &MAXIMA).is_ok());
    }

    #[test]
    fn native_rpm_errors_follow_effect_returns_and_preserve_failed_frame_gear() {
        let bad =
            NativeEngineError { class: "ValueError", message: "invalid literal for int() with base 10: '30.5'".into() };
        let mut tune = tuning();
        tune.native_engine_rpm =
            Some(NativeEngineRpm { start: Err(bad.clone()), fade_start: Ok((100).into()), fade_end: Ok((95).into()) });
        let mut input = quiet();
        input.rpm = 4000.0;
        assert!(
            matches!(wheel_level(Wheel::FrontLeft, &input, &tune, &MAXIMA), Err(HapticError::NativeEngine(error)) if error == bad)
        );
        input.abs_lock = 1.0;
        assert!(wheel_level(Wheel::FrontLeft, &input, &tune, &MAXIMA).is_ok());
        let link = FakeLink::new();
        let mut device = HaptiCr::new(Box::new(link.clone()));
        let mut telemetry = Telemetry::off();
        telemetry.rpm = 4000.0;
        telemetry.max_rpm = 8000.0;
        telemetry.gear = 4.0;
        tune.gear.enabled = false;
        for on in [true, false] {
            assert!(matches!(
                device.send_stream(
                    &telemetry,
                    &HapticSwitches { test_mode: 0, on, game_running: true },
                    &tune,
                    &MAXIMA
                ),
                Err(HapticError::NativeEngine(_))
            ));
            assert!(link.take_written().is_empty());
            assert_eq!(device.previous_gear, LegacyValue::Int(-1));
        }
        tune.gear.enabled = true;
        device
            .send_stream(&telemetry, &HapticSwitches { test_mode: 0, on: true, game_running: true }, &tune, &MAXIMA)
            .unwrap();
        assert_eq!(sent(&link), vec![b'2', 127, 127, 127, 127, 0, 0, 0, 0, 0]);
        assert!(matches!(
            device.send_stream(
                &telemetry,
                &HapticSwitches { test_mode: 0, on: true, game_running: true },
                &tune,
                &MAXIMA
            ),
            Err(HapticError::NativeEngine(_))
        ));
        assert!(link.take_written().is_empty());
        tune.native_engine_rpm =
            Some(NativeEngineRpm { start: Ok((30).into()), fade_start: Ok((70).into()), fade_end: Ok((95).into()) });
        device
            .send_stream(&telemetry, &HapticSwitches { test_mode: 0, on: true, game_running: true }, &tune, &MAXIMA)
            .unwrap();
        assert!(!link.take_written().is_empty());
        tune.native_engine_rpm.as_mut().unwrap().start = Err(bad);
        device
            .send_stream(&telemetry, &HapticSwitches { test_mode: 0, on: true, game_running: false }, &tune, &MAXIMA)
            .unwrap();
        assert!(link.take_written().is_empty());
        device
            .send_stream(&telemetry, &HapticSwitches { test_mode: 2, on: true, game_running: true }, &tune, &MAXIMA)
            .unwrap();
        assert!(!link.take_written().is_empty());
    }

    #[test]
    fn gear_changes_compare_native_values_instead_of_the_numeric_projection() {
        let link = FakeLink::new();
        let mut h = HaptiCr::new(Box::new(link.clone()));
        let switches = HapticSwitches { test_mode: 0, on: true, game_running: true };
        for (value, changed) in super::super::gear_comparison_samples() {
            let mut telemetry = Telemetry::off();
            telemetry.set_native("gear", value.clone());
            h.send_stream(&telemetry, &switches, &tuning(), &MAXIMA).unwrap();
            let level = if changed { 127 } else { 0 };
            assert_eq!(sent(&link), vec![b'2', level, level, level, level, 0, 0, 0, 0, 0], "{value:?}");
        }
    }

    #[test]
    fn missing_stream_values_use_the_local_zero_copy_and_keep_failed_frames_uncommitted() {
        let link = FakeLink::new();
        let mut h = HaptiCr::new(Box::new(link.clone()));
        let sw = HapticSwitches { test_mode: 0, on: true, game_running: true };
        let empty = Telemetry::unfilled();
        h.send_stream(&empty, &sw, &tuning(), &MAXIMA).unwrap();
        assert_eq!(sent(&link), vec![b'2', 127, 127, 127, 127, 0, 0, 0, 0, 0]);
        let mut zero_gear = Telemetry::unfilled();
        zero_gear.set_native("gear", openrigcue_model::native_value::LegacyValue::Int(0));
        h.send_stream(&zero_gear, &sw, &tuning(), &MAXIMA).unwrap();
        assert_eq!(sent(&link), vec![b'2', 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert!(empty.channel_names().is_empty(), "local fallbacks never fill the shared input");

        let mut running = Telemetry::unfilled();
        running.set_native("gear", openrigcue_model::native_value::LegacyValue::Int(4));
        running.set_native("rpm", openrigcue_model::native_value::LegacyValue::Int(800));
        let mut engine_tuning = tuning();
        engine_tuning.gear.enabled = false;
        engine_tuning.rpm_start_fading = 100;
        for on in [true, false] {
            let switches = HapticSwitches { on, ..sw };
            assert!(matches!(h.send_stream(&running, &switches, &engine_tuning, &MAXIMA), Err(HapticError::Math(_))));
            assert!(link.take_written().is_empty(), "the engine floor division fails before output");
        }
        assert_eq!(running.get_assigned("max_rpm"), None);
        running.set_native("max_rpm", openrigcue_model::native_value::LegacyValue::Int(8000));
        h.send_stream(&running, &sw, &tuning(), &MAXIMA).unwrap();
        assert_eq!(sent(&link), vec![b'2', 127, 127, 127, 127, 0, 0, 0, 0, 0]);
        h.send_stream(&running, &sw, &tuning(), &MAXIMA).unwrap();
        assert_eq!(sent(&link), vec![b'2', 25, 25, 25, 25, 0, 0, 0, 0, 0]);
    }

    /// Scan: a HaptiCR with firmware 0 sets the firmware message at once, the scan goes on and
    /// ends without a device (the caller then writes "Not Connected").
    #[test]
    fn scan_reports_old_firmware() {
        let link = FakeLink::new();
        let mut nonce_rng = rand::rngs::SmallRng::seed_from_u64(9);
        let nonce = new_handshake(&mut nonce_rng);
        link.queue_reply(format!("1,{},\x07,\x02,/\x00,\x01,SN;", seal_for_tests(&format!("{nonce}^1"))).as_bytes());
        let opener = FakeOpener::with("/dev/ttyACM2", &link);
        let ports = FakePorts(vec![PortEntry::usb("/dev/ttyACM2", 0x1a86, 0x55d4, None, None)]);
        let clock = ManualClock::default();
        let claimed = ClaimedPorts::default();
        let allowed = AllowedPorts::default();
        let reported = std::cell::RefCell::new(Vec::new());
        let ctx = HapticScanContext {
            ports: &ports,
            opener: &opener,
            clock: &clock,
            claimed: &claimed,
            cached_port: None,
            com_port: "",
            allowed: &allowed,
            report: &|status| reported.borrow_mut().push(status),
        };
        let mut rng = rand::rngs::SmallRng::seed_from_u64(9);
        assert!(matches!(HapticScanner::new().scan(&ctx, &mut rng).unwrap(), HapticScan::FirmwareTooOld));
        assert_eq!(*reported.borrow(), vec![DeviceStatus::FirmwareUpdateRequired]);
    }

    /// Scan: CH9102 port, product 7, firmware >= 1; a status number other than 1 is
    /// accepted too.
    #[test]
    fn scan_identifies_hapticr() {
        for status in ["1", "0"] {
            let link = FakeLink::new();
            let mut nonce_rng = rand::rngs::SmallRng::seed_from_u64(9);
            let nonce = new_handshake(&mut nonce_rng);
            link.queue_reply(
                format!("1,{},\x07,\x02,\x01,\x01,SN;", seal_for_tests(&format!("{nonce}^{status}"))).as_bytes(),
            );
            let opener = FakeOpener::with("/dev/ttyACM2", &link);
            let ports = FakePorts(vec![PortEntry::usb("/dev/ttyACM2", 0x1a86, 0x55d4, None, None)]);
            let clock = ManualClock::default();
            let claimed = ClaimedPorts::default();
            let allowed = AllowedPorts::default();
            let ctx = HapticScanContext {
                ports: &ports,
                opener: &opener,
                clock: &clock,
                claimed: &claimed,
                cached_port: None,
                com_port: "",
                allowed: &allowed,
                report: &|_| {},
            };
            let mut rng = rand::rngs::SmallRng::seed_from_u64(9);
            match HapticScanner::new().scan(&ctx, &mut rng).unwrap() {
                HapticScan::Connected(c) => assert_eq!((c.port.as_str(), c.model, c.version), ("/dev/ttyACM2", 2, 1)),
                _ => panic!("expected a HaptiCR (status {status})"),
            }
        }
    }
}
