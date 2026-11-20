//! SIM3D pedal, seat and wind motors over the SimHub "ShakeIt" serial protocol.

use std::time::Duration;

use tracing::debug;

use openrigcue_model::native_value::LegacyValue;
use openrigcue_model::python_compare::native_equal;
use openrigcue_model::Telemetry;

use super::hapticr::{EffectLevel, EngineLevelError, NativeEngineControl, NativeEngineError, NativeEngineRpm};
use super::link::{Clock, PortLister, PortOpener, PortSettings, SerialLink};
use super::numeric::{map_clamped, DivisionByZero};
use super::port_scan::{AllowedPorts, ClaimedPorts, Device, Exclusions, ScanRules};

/// Serial speed of the ShakeIt boards.
pub const BAUD_RATE: u32 = 19_200;
/// The port's read timeout (1000 seconds); every reply
/// routine polls for waiting bytes with its own 1 s budget instead.
const PORT_TIMEOUT: Duration = Duration::from_secs(1000);
/// Reply budget of the reply readers (1 s), polled in tenths.
const REPLY_TIMEOUT: Duration = Duration::from_secs(1);

/// CRC-8, polynomial 0xD5, MSB first, init 0.
const CRC_TABLE: [u8; 256] = crc_table();

const fn crc_table() -> [u8; 256] {
    let mut table = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u8;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x80 != 0 { (crc << 1) ^ 0xD5 } else { crc << 1 };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

const REPLY_HEADER: u8 = 0x03;
const REPLY_BYTE: u8 = 0x08;

/// The ShakeIt protocol on an open port.
pub struct ShakeIt {
    link: Box<dyn SerialLink>,
    packet_id: u8,
}

#[derive(Debug, thiserror::Error)]
pub enum Sim3dError {
    #[error("serial: {0}")]
    Io(#[from] std::io::Error),
    #[error("motor value {0} does not fit in a byte")]
    ByteRange(i64),
    #[error("division by zero in the effect math")]
    Math(#[from] DivisionByZero),
    #[error(transparent)]
    NativeEngine(#[from] NativeEngineError),
    #[error("port role {0:?} is not a SIM3D output")]
    UnknownRole(String),
    #[error("test port {0} is not 1..4")]
    BadTestPort(i64),
    #[error("motor level is not a finite number")]
    NotFinite,
}

impl From<EngineLevelError> for Sim3dError {
    fn from(error: EngineLevelError) -> Self {
        match error {
            EngineLevelError::Math(error) => Self::Math(error),
            EngineLevelError::Native(error) => Self::NativeEngine(error),
        }
    }
}

impl ShakeIt {
    pub fn new(link: Box<dyn SerialLink>) -> Self {
        ShakeIt { link, packet_id: 0 }
    }

    /// A framed message: `01 01 <id> <len> <payload> <crc8(id, len, payload)>`.
    pub fn pack(&self, payload: &[u8]) -> Vec<u8> {
        let mut crc = CRC_TABLE[usize::from(self.packet_id)];
        crc = CRC_TABLE[usize::from(crc ^ payload.len() as u8)];
        for b in payload {
            crc = CRC_TABLE[usize::from(crc ^ b)];
        }
        let mut frame = vec![1, 1, self.packet_id, payload.len() as u8];
        frame.extend_from_slice(payload);
        frame.push(crc);
        frame
    }

    /// Adopt the packet id the device echoed, plus one, wrapping after 127.
    fn advance_packet_id(&mut self, echoed: u8) {
        self.packet_id = if echoed <= 127 { echoed + 1 } else { 0 };
    }

    /// Poll for waiting bytes every 0.1 s for at most 1 s until `complete(buffer)`; leading bytes before the
    /// 0x03 reply header are dropped.
    fn collect(&mut self, clock: &dyn Clock, complete: impl Fn(&[u8]) -> bool) -> std::io::Result<Vec<u8>> {
        let mut buffer = Vec::new();
        // The budget is measured on the wall clock from the start of the wait.
        let start = clock.time();
        while clock.time() - start < REPLY_TIMEOUT.as_secs_f64() {
            buffer.extend(self.link.read_available()?);
            let header = buffer.iter().position(|b| *b == REPLY_HEADER).unwrap_or(buffer.len());
            buffer.drain(..header);
            if complete(&buffer) {
                break;
            }
            clock.sleep(REPLY_TIMEOUT / 10);
        }
        Ok(buffer)
    }

    /// A void reply: `03 <id>`.
    fn void_reply(&mut self, clock: &dyn Clock) -> std::io::Result<bool> {
        let buffer = self.collect(clock, |b| b.len() >= 2)?;
        if buffer.len() >= 2 && buffer[0] == REPLY_HEADER {
            self.advance_packet_id(buffer[1]);
            return Ok(true);
        }
        Ok(false)
    }

    /// A byte reply: `03 <id> 08 <byte>`.
    fn byte_reply(&mut self, clock: &dyn Clock) -> std::io::Result<Option<u8>> {
        let buffer = self.collect(clock, |b| b.len() >= 4)?;
        if buffer.len() >= 4 && buffer[0] == REPLY_HEADER {
            self.advance_packet_id(buffer[1]);
            if buffer[2] == REPLY_BYTE {
                return Ok(Some(buffer[3]));
            }
        }
        Ok(None)
    }

    fn send(&mut self, payload: &[u8]) -> std::io::Result<bool> {
        let frame = self.pack(payload);
        Ok(self.link.write(&frame)? == frame.len())
    }

    /// The hello command: `03 '1'`; `Some(byte)` when the board answered with a byte reply.
    pub fn hello(&mut self, clock: &dyn Clock) -> std::io::Result<Option<u8>> {
        if !self.send(&[3, b'1'])? {
            return Ok(None);
        }
        self.byte_reply(clock)
    }

    /// Set the four motor levels: `03 'V' 'S' <m1> <m2> <m3> <m4>`, acknowledged by a void reply.
    pub fn set_motors(&mut self, values: &[i64; 4], clock: &dyn Clock) -> Result<bool, Sim3dError> {
        let mut payload = vec![3, b'V', b'S'];
        for v in values {
            payload.push(u8::try_from(*v).map_err(|_| Sim3dError::ByteRange(*v))?);
        }
        if !self.send(&payload)? {
            return Ok(false);
        }
        Ok(self.void_reply(clock)?)
    }
}

/// What a SIM3D port drives (`SIM3D.portN`, a rumble group label).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortRole {
    None,
    Throttle,
    Brake,
    Clutch,
    Seat,
    WindLeft,
    WindRight,
    WindCenter,
}

impl PortRole {
    pub fn from_label(label: &str) -> Option<Self> {
        Some(match label {
            "None" => PortRole::None,
            "Throttle" => PortRole::Throttle,
            "Brake" => PortRole::Brake,
            "Clutch" => PortRole::Clutch,
            "Seat" => PortRole::Seat,
            "Wind Left" => PortRole::WindLeft,
            "Wind Right" => PortRole::WindRight,
            "Wind Center" => PortRole::WindCenter,
            _ => return None,
        })
    }
}

/// One motor group's effects (`rumble_switch` / `rumble_effects` keys with the group prefix).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SeatEffects {
    pub wheel_slip: EffectLevel,
    pub rumble_strips: EffectLevel,
    pub gear: EffectLevel,
    pub engine: EffectLevel,
    pub surge_acceleration: EffectLevel,
    pub heave: EffectLevel,
    pub traction_loss: EffectLevel,
}

/// Brake pedal effects.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BrakeEffects {
    /// `brake_wheel_slip` ("Wheel Lock").
    pub wheel_lock: EffectLevel,
    pub rumble_strips: EffectLevel,
    pub gear: EffectLevel,
    pub abs_lock: EffectLevel,
    pub surge_deceleration: EffectLevel,
    pub heave: EffectLevel,
    pub traction_loss: EffectLevel,
}

/// SIM3D tuning of the game profile.
#[derive(Debug, Clone, PartialEq)]
pub struct Sim3dTuning {
    pub throttle: SeatEffects,
    pub seat: SeatEffects,
    pub brake: BrakeEffects,
    pub clutch_gear: EffectLevel,
    pub rpm_start_rumble: i64,
    pub rpm_start_fading: i64,
    pub rpm_stop_fading: i64,
    pub wheel_slip_min_activation: i64,
    pub wheel_lock_min_activation: i64,
    pub traction_loss_min_activation: i64,
    pub native_engine_rpm: Option<NativeEngineRpm>,
    pub native_engine_control: [Option<NativeEngineControl>; 2],
    pub native_event_controls: Option<NativeSim3dEvents>,
    pub native_continuous_controls: Option<NativeSim3dContinuous>,
    pub native_traction_maximum: Option<super::NativeTractionMaximum>,
    pub native_map_maxima: Option<super::NativeMapMaxima>,
    /// Raw profile retained until each required operation is reached.
    pub native_profile: Option<LegacyValue>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeSim3dContinuous {
    /// Surge, heave, then traction loss for each group.
    pub throttle: [NativeEngineControl; 3],
    pub brake: [NativeEngineControl; 3],
    pub seat: [NativeEngineControl; 3],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeSim3dEvents {
    pub throttle: [NativeEngineControl; 3],
    pub seat: [NativeEngineControl; 3],
    /// ABS, wheel lock, rumble strips, then gear change.
    pub brake: [NativeEngineControl; 4],
    pub clutch: NativeEngineControl,
}

/// Normalisation maxima (the tune's `max_telemetry_vars`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sim3dMaxima {
    pub surge_acceleration: f64,
    pub heave: f64,
    pub traction_loss: f64,
}

/// Slider divisor of the SIM3D effects (slider 20 gives full scale).
const DIVIDER: i64 = 20;
/// Level sent to the port under test (`rumble_test_mode`).
const TEST_LEVEL: i64 = 128;

/// Flags shared by the pedal and seat calculations.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Events {
    rumble_strips: bool,
    gear_change: bool,
    wheel_slip: bool,
    wheel_lock: bool,
}

/// Throttle or seat level: wheel slip, rumble strips, gear and engine return early; otherwise acceleration surge,
/// tripled heave and traction loss are summed.
#[allow(clippy::too_many_arguments)]
fn seat_level(
    effects: &SeatEffects,
    events: Events,
    telemetry: &Telemetry,
    traction: f64,
    tuning: &Sim3dTuning,
    maxima: &Sim3dMaxima,
    native_engine: Option<&NativeEngineControl>,
    native_events: Option<&[NativeEngineControl; 3]>,
    native_continuous: Option<&[NativeEngineControl; 3]>,
) -> Result<f64, Sim3dError> {
    use super::values as v;
    let profile = tuning.native_profile.as_ref();
    let mode = if std::ptr::eq(effects, &tuning.throttle) { "throttle" } else { "seat" };
    let heave = v::channel(telemetry, "vertical_acceleration", telemetry.vertical_acceleration)
        .py_mul(&LegacyValue::Int(3))
        .map_err(v::operation)?;
    let raw = v::channel(telemetry, "longitudinal_acceleration", telemetry.longitudinal_acceleration);
    let surge = if raw.py_lt(&LegacyValue::Int(0)).map_err(v::operation)? { LegacyValue::Int(0) } else { raw }
        .py_abs()
        .map_err(v::operation)?;
    for (index, (active, effect)) in [
        (events.wheel_slip, effects.wheel_slip),
        (events.rumble_strips, effects.rumble_strips),
        (events.gear_change, effects.gear),
    ]
    .into_iter()
    .enumerate()
    {
        if active {
            let key = format!("{mode}_{}", ["wheel_slip", "rumble_strips", "gear"][index]);
            let selected = v::selected(
                profile,
                ("rumble_switch", "rumble_effects"),
                &key,
                native_events.map(|controls| &controls[index]),
                effect,
            )?;
            if let Some(level) = selected {
                return v::full(&level, DIVIDER).map_err(Into::into);
            }
        }
    }
    let rpm = v::channel(telemetry, "rpm", telemetry.rpm);
    let engine_level = if rpm.py_gt(&LegacyValue::Int(0)).map_err(v::operation)? {
        v::selected(
            profile,
            ("rumble_switch", "rumble_effects"),
            &format!("{mode}_engine"),
            native_engine,
            effects.engine,
        )?
    } else {
        None
    };
    if let Some(level) = engine_level {
        let engine = v::engine(
            profile,
            "rumble_effects",
            &rpm,
            &v::channel(telemetry, "max_rpm", telemetry.max_rpm),
            &level,
            DIVIDER,
            [tuning.rpm_start_rumble, tuning.rpm_start_fading, tuning.rpm_stop_fading],
            tuning.native_engine_rpm.as_ref(),
        )
        .map_err(|error| {
            if profile.is_none() && error.class == "ZeroDivisionError" {
                Sim3dError::Math(DivisionByZero)
            } else {
                Sim3dError::NativeEngine(error)
            }
        })?;
        if let Some(level) = engine {
            return Ok(level);
        }
    }
    let mut sliders = Vec::with_capacity(3);
    for (index, (suffix, effect)) in [
        ("surge_acceleration", effects.surge_acceleration),
        ("heave", effects.heave),
        ("traction_loss", effects.traction_loss),
    ]
    .into_iter()
    .enumerate()
    {
        sliders.push(
            v::selected(
                profile,
                ("rumble_switch", "rumble_effects"),
                &format!("{mode}_{suffix}"),
                native_continuous.map(|controls| &controls[index]),
                effect,
            )?
            .unwrap_or(LegacyValue::Int(0)),
        );
    }
    let surge_maximum = v::maximum(
        profile,
        "surge_acceleration",
        maxima.surge_acceleration,
        tuning.native_map_maxima.as_ref().map(|m| &m.surge_acceleration),
    )?;
    let surge_level = v::scaled(
        map_clamped(surge.py_as_double().map_err(v::operation)?, 0.0, surge_maximum, 0.0, 255.0),
        &sliders[0],
        DIVIDER,
    )?;
    let heave_maximum =
        v::maximum(profile, "heave", maxima.heave, tuning.native_map_maxima.as_ref().map(|m| &m.heave))?;
    let heave_level = v::scaled(
        map_clamped(heave.py_as_double().map_err(v::operation)?, 0.0, heave_maximum, 0.0, 255.0),
        &sliders[1],
        DIVIDER,
    )?;
    Ok(surge_level + heave_level + v::scaled(map_clamped(traction, 0.0, 1.0, 0.0, 255.0), &sliders[2], DIVIDER)?)
}

/// Brake pedal level: no wheel-lock while accelerating; ABS, wheel lock, rumble strips and gear return early;
/// otherwise braking surge (scaled by the acceleration maximum), tripled heave and
/// traction loss are summed.
#[allow(clippy::too_many_arguments)]
#[cfg(test)]
fn brake_level(
    effects: &BrakeEffects,
    events: Events,
    telemetry: &Telemetry,
    traction: f64,
    maxima: &Sim3dMaxima,
    native_events: Option<&[NativeEngineControl; 4]>,
    native_continuous: Option<&[NativeEngineControl; 3]>,
    native_maxima: Option<&super::NativeMapMaxima>,
) -> Result<f64, Sim3dError> {
    brake_level_values(
        effects,
        events,
        telemetry,
        traction,
        maxima,
        native_events,
        native_continuous,
        native_maxima,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn brake_level_values(
    effects: &BrakeEffects,
    events: Events,
    telemetry: &Telemetry,
    traction: f64,
    maxima: &Sim3dMaxima,
    native_events: Option<&[NativeEngineControl; 4]>,
    native_continuous: Option<&[NativeEngineControl; 3]>,
    native_maxima: Option<&super::NativeMapMaxima>,
    profile: Option<&LegacyValue>,
) -> Result<f64, Sim3dError> {
    use super::values as v;
    let heave = v::channel(telemetry, "vertical_acceleration", telemetry.vertical_acceleration)
        .py_mul(&LegacyValue::Int(3))
        .map_err(v::operation)?;
    let surge = v::channel(telemetry, "longitudinal_acceleration", telemetry.longitudinal_acceleration);
    let accelerating = surge.py_gt(&LegacyValue::Int(0)).map_err(v::operation)?;
    let wheel_lock = events.wheel_lock && !accelerating;
    for (index, (active, effect)) in [
        (native_equal(&v::channel(telemetry, "abs_lock", telemetry.abs_lock), &LegacyValue::Int(1)), effects.abs_lock),
        (wheel_lock, effects.wheel_lock),
        (events.rumble_strips, effects.rumble_strips),
        (events.gear_change, effects.gear),
    ]
    .into_iter()
    .enumerate()
    {
        if active {
            let key = ["brake_abs_lock", "brake_wheel_slip", "brake_rumble_strips", "brake_gear"][index];
            let selected = v::selected(
                profile,
                ("rumble_switch", "rumble_effects"),
                key,
                native_events.map(|controls| &controls[index]),
                effect,
            )?;
            if let Some(level) = selected {
                return v::full(&level, DIVIDER).map_err(Into::into);
            }
        }
    }
    let mut sliders = Vec::with_capacity(3);
    for (index, (key, effect)) in [
        ("brake_surge_deceleration", effects.surge_deceleration),
        ("brake_heave", effects.heave),
        ("brake_traction_loss", effects.traction_loss),
    ]
    .into_iter()
    .enumerate()
    {
        sliders.push(
            v::selected(
                profile,
                ("rumble_switch", "rumble_effects"),
                key,
                native_continuous.map(|controls| &controls[index]),
                effect,
            )?
            .unwrap_or(LegacyValue::Int(0)),
        );
    }
    let braking = if surge.py_lt(&LegacyValue::Int(0)).map_err(v::operation)? {
        surge.py_abs().map_err(v::operation)?
    } else {
        LegacyValue::Int(0)
    };
    let surge_maximum = v::maximum(
        profile,
        "surge_acceleration",
        maxima.surge_acceleration,
        native_maxima.map(|m| &m.surge_acceleration),
    )?;
    let surge_level = v::scaled(
        map_clamped(braking.py_as_double().map_err(v::operation)?, 0.0, surge_maximum, 0.0, 255.0),
        &sliders[0],
        DIVIDER,
    )?;
    let heave_maximum = v::maximum(profile, "heave", maxima.heave, native_maxima.map(|m| &m.heave))?;
    let heave_level = v::scaled(
        map_clamped(heave.py_as_double().map_err(v::operation)?, 0.0, heave_maximum, 0.0, 255.0),
        &sliders[1],
        DIVIDER,
    )?;
    Ok(surge_level + heave_level + v::scaled(map_clamped(traction, 0.0, 1.0, 0.0, 255.0), &sliders[2], DIVIDER)?)
}

/// Fan levels handed over by the dispatcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sim3dFans {
    pub left: i64,
    pub center: i64,
    pub right: i64,
}

/// Switches the stream reads.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sim3dSwitches {
    /// `rumble_test_mode`: 0, or 1..4 = port under test.
    pub test_port: i64,
    /// `sim3d_switch == 1`.
    pub on: bool,
    pub game_running: bool,
}

/// An open SIM3D board with its gear-change memory.
pub struct Sim3d {
    proto: ShakeIt,
    /// `previous_gear` (starts at 0).
    previous_gear: LegacyValue,
}

impl Sim3d {
    pub fn new(proto: ShakeIt) -> Self {
        Sim3d { proto, previous_gear: LegacyValue::Int(0) }
    }

    /// Drive the four ports for one frame.
    ///
    /// The test port
    /// gets 128; with a game running each port gets its role's level, the user switch off sends zeros, no
    /// game sends nothing. The pedal, seat and wind roles are always computed.
    #[allow(clippy::too_many_arguments)]
    pub fn send_stream(
        &mut self,
        telemetry: &Telemetry,
        fans: Sim3dFans,
        ports: &[PortRole; 4],
        switches: &Sim3dSwitches,
        tuning: &Sim3dTuning,
        maxima: &Sim3dMaxima,
        clock: &dyn Clock,
    ) -> Result<(), Sim3dError> {
        if switches.test_port != 0 {
            if !(1..=4).contains(&switches.test_port) {
                return Err(Sim3dError::BadTestPort(switches.test_port));
            }
            let levels: [i64; 4] =
                std::array::from_fn(|i| if i as i64 == switches.test_port - 1 { TEST_LEVEL } else { 0 });
            self.proto.set_motors(&levels.map(clamp_level), clock)?;
            return Ok(());
        }
        if !switches.game_running {
            return Ok(());
        }
        let telemetry = super::haptic_stream_telemetry(telemetry);
        let levels = self.port_levels(&telemetry, fans, ports, tuning, maxima)?;
        if switches.on {
            self.proto.set_motors(&levels.map(clamp_level), clock)?;
        } else {
            self.proto.set_motors(&[0; 4], clock)?;
        }
        self.previous_gear = super::stream_gear(&telemetry);
        Ok(())
    }

    fn port_levels(
        &self,
        t: &Telemetry,
        fans: Sim3dFans,
        ports: &[PortRole; 4],
        tuning: &Sim3dTuning,
        maxima: &Sim3dMaxima,
    ) -> Result<[i64; 4], Sim3dError> {
        use super::values as v;
        let profile = tuning.native_profile.as_ref();
        let slip_threshold = v::activation(
            profile,
            "rumble_effects",
            "wheel_slip_min_activation",
            tuning.wheel_slip_min_activation,
            25,
        )?;
        let lock_threshold = v::activation(
            profile,
            "rumble_effects",
            "wheel_lock_min_activation",
            tuning.wheel_lock_min_activation,
            25,
        )?;
        let above = |channels: [&str; 4], threshold| -> Result<bool, Sim3dError> {
            for key in channels {
                if v::channel(t, key, t.get_assigned(key).unwrap_or(0.0))
                    .py_gt(&LegacyValue::Float(threshold))
                    .map_err(v::operation)?
                {
                    return Ok(true);
                }
            }
            Ok(false)
        };
        let events = Events {
            gear_change: !native_equal(&self.previous_gear, &super::stream_gear(t)),
            wheel_slip: above(["wheel_slip_lf", "wheel_slip_rf", "wheel_slip_lr", "wheel_slip_rr"], slip_threshold)?,
            wheel_lock: above(["wheel_slip_lf", "wheel_slip_rf", "wheel_slip_lr", "wheel_slip_rr"], lock_threshold)?,
            rumble_strips: above(["rumble_pitch_lf", "rumble_pitch_rf", "rumble_pitch_lr", "rumble_pitch_rr"], 5.0)?,
        };
        let mut traction =
            super::traction_ratio(t, maxima.traction_loss, tuning.native_traction_maximum.as_ref(), true);
        if traction
            < v::activation(
                profile,
                "rumble_effects",
                "traction_loss_min_activation",
                tuning.traction_loss_min_activation,
                100,
            )?
        {
            traction = 0.0;
        }
        let level =
            |value: f64| value.is_finite().then(|| value.trunc().clamp(0.0, 256.0) as i64).ok_or(Sim3dError::NotFinite);
        let throttle = level(seat_level(
            &tuning.throttle,
            events,
            t,
            traction,
            tuning,
            maxima,
            tuning.native_engine_control[0].as_ref(),
            tuning.native_event_controls.as_ref().map(|events| &events.throttle),
            tuning.native_continuous_controls.as_ref().map(|controls| &controls.throttle),
        )?)?;
        let brake = level(brake_level_values(
            &tuning.brake,
            events,
            t,
            traction,
            maxima,
            tuning.native_event_controls.as_ref().map(|events| &events.brake),
            tuning.native_continuous_controls.as_ref().map(|controls| &controls.brake),
            tuning.native_map_maxima.as_ref(),
            profile,
        )?)?;
        let clutch = if events.gear_change {
            let selected = v::selected(
                profile,
                ("rumble_switch", "rumble_effects"),
                "clutch_gear",
                tuning.native_event_controls.as_ref().map(|events| &events.clutch),
                tuning.clutch_gear,
            )?;
            match selected {
                Some(level) => v::full(&level, DIVIDER)?,
                None => 0.0,
            }
        } else {
            0.0
        };
        let clutch = level(clutch)?;
        let seat = level(seat_level(
            &tuning.seat,
            events,
            t,
            traction,
            tuning,
            maxima,
            tuning.native_engine_control[1].as_ref(),
            tuning.native_event_controls.as_ref().map(|events| &events.seat),
            tuning.native_continuous_controls.as_ref().map(|controls| &controls.seat),
        )?)?;
        debug!("wind {} {} {}", fans.left, fans.center, fans.right);
        let (wind_left, wind_right, wind_center) = (fans.left, fans.right, fans.center);
        Ok(ports.map(|role| match role {
            PortRole::None => 0,
            PortRole::Throttle => throttle,
            PortRole::Brake => brake,
            PortRole::Clutch => clutch,
            PortRole::Seat => seat,
            PortRole::WindLeft => wind_left,
            PortRole::WindRight => wind_right,
            PortRole::WindCenter => wind_center,
        }))
    }
}

/// Position validation: clamp to 0..=256 (256 then fails the byte conversion).
fn clamp_level(value: i64) -> i64 {
    value.clamp(0, 256)
}

/// Environment of a SIM3D scan.
pub struct Sim3dScanContext<'a> {
    pub ports: &'a dyn PortLister,
    pub opener: &'a dyn PortOpener,
    pub clock: &'a dyn Clock,
    pub claimed: &'a ClaimedPorts,
    /// The port remembered from the last successful connection.
    pub cached_port: Option<&'a str>,
    /// `SIM3D.com_port`.
    pub com_port: &'a str,
    pub allowed: &'a AllowedPorts,
}

/// USB ids of SIM3D boards (CH340, Arduino Uno R3).
const USB_IDS: [&str; 2] = ["1A86:7523", "2341:0043"];
/// Hello attempts per port, one second apart.
const HELLO_ATTEMPTS: usize = 10;

/// Whether the next SIM3D scan is the first one.
#[derive(Debug)]
pub struct Sim3dScanner {
    first_scan: bool,
}

impl Default for Sim3dScanner {
    fn default() -> Self {
        Sim3dScanner { first_scan: true }
    }
}

/// A connected board.
pub struct ConnectedSim3d {
    pub port: String,
    pub device: Sim3d,
}

impl Sim3dScanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Called only when `SIM3D.model` is not 'None': CH340/Uno ports at 19200,
    /// up to 10 hello attempts one second apart.
    pub fn scan(&mut self, ctx: &Sim3dScanContext<'_>) -> Result<Option<ConnectedSim3d>, super::link::PortScanError> {
        let rules = ScanRules {
            device: Device::Sim3d,
            exclusions: Exclusions::RaspberryUart,
            configured_port: ctx.com_port,
            allowed: ctx.allowed,
            claimed: ctx.claimed,
            cached_port: ctx.cached_port,
            first_scan: self.first_scan,
        };
        let mut found = None;
        for port in ctx.ports.ports()? {
            if !rules.may_try(&port) || !port.hwid_contains_any(&USB_IDS)? {
                continue;
            }
            let settings = PortSettings { baud_rate: BAUD_RATE, read_timeout: Some(PORT_TIMEOUT) };
            let Ok(link) = ctx.opener.open(&port.device, settings) else { continue };
            let mut proto = ShakeIt::new(link);
            let mut connected = false;
            for attempt in 0..HELLO_ATTEMPTS {
                connected = matches!(proto.hello(ctx.clock), Ok(Some(_)));
                if !connected {
                    debug!("checking if protocol is connected, tentatives {attempt}");
                }
                ctx.clock.sleep(Duration::from_secs(1));
                if connected {
                    break;
                }
            }
            if connected {
                found = Some(ConnectedSim3d { port: port.device.clone(), device: Sim3d::new(proto) });
                break;
            }
        }
        self.first_scan = false;
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::super::link::fakes::{FakeLink, FakeOpener, FakePorts, ManualClock};
    use super::super::link::PortEntry;
    use super::*;

    /// CRC-8 table entries (poly 0xD5).
    #[test]
    fn crc_table_matches_binary_entries() {
        assert_eq!(CRC_TABLE[1], 0xD5);
        assert_eq!(CRC_TABLE[2], 0x7F);
        assert_eq!(CRC_TABLE[3], 0xAA);
        assert_eq!(CRC_TABLE[4], 0xFE);
        assert_eq!(CRC_TABLE[5], 0x2B);
        assert_eq!(CRC_TABLE[255], 0xF9);
    }

    /// Message framing and the packet-id echo rule.
    #[test]
    fn frames_and_packet_ids() {
        let link = FakeLink::new();
        let mut proto = ShakeIt::new(Box::new(link.clone()));
        proto.packet_id = 5;
        let frame = proto.pack(&[3, 10, 20, 30, 40]);
        let mut crc = 0u8;
        for b in [5u8, 5, 3, 10, 20, 30, 40] {
            crc = CRC_TABLE[usize::from(crc ^ b)];
        }
        assert_eq!(frame, vec![1, 1, 5, 5, 3, 10, 20, 30, 40, crc]);
        let clock = ManualClock::default();
        link.queue_reply(&[0xAA, 3, 9, 8, 0x42]);
        assert_eq!(proto.hello(&clock).unwrap(), Some(0x42));
        assert_eq!(proto.packet_id, 10);
        link.queue_reply(&[3, 200]);
        assert!(proto.set_motors(&[0, 1, 2, 3], &clock).unwrap());
        assert_eq!(proto.packet_id, 0);
        assert!(matches!(proto.set_motors(&[256, 0, 0, 0], &clock), Err(Sim3dError::ByteRange(256))));
        // No reply within the budget: false after 1 s of polling.
        assert!(!proto.set_motors(&[0; 4], &clock).unwrap());
    }

    fn effect(level: i64) -> EffectLevel {
        EffectLevel { enabled: true, level }
    }

    fn seat() -> SeatEffects {
        SeatEffects {
            wheel_slip: effect(10),
            rumble_strips: effect(20),
            gear: effect(10),
            engine: effect(20),
            surge_acceleration: effect(20),
            heave: effect(20),
            traction_loss: effect(20),
        }
    }

    fn tuning() -> Sim3dTuning {
        Sim3dTuning {
            throttle: seat(),
            seat: seat(),
            brake: BrakeEffects {
                wheel_lock: effect(20),
                rumble_strips: effect(20),
                gear: effect(10),
                abs_lock: effect(16),
                surge_deceleration: effect(20),
                heave: effect(20),
                traction_loss: effect(20),
            },
            clutch_gear: effect(20),
            rpm_start_rumble: 0,
            rpm_start_fading: 70,
            rpm_stop_fading: 95,
            wheel_slip_min_activation: 30,
            wheel_lock_min_activation: 50,
            traction_loss_min_activation: 40,
            native_engine_rpm: None,
            native_engine_control: [None, None],
            native_event_controls: None,
            native_continuous_controls: None,
            native_traction_maximum: None,
            native_map_maxima: None,
            native_profile: None,
        }
    }

    const MAXIMA: Sim3dMaxima = Sim3dMaxima { surge_acceleration: 5.0, heave: 2.0, traction_loss: 5.0 };

    fn quiet() -> Telemetry {
        let mut t = Telemetry::off();
        t.gear = 0.0;
        t.max_rpm = 8000.0;
        t
    }

    /// Seat, brake and clutch levels through the port routing, and the fan-name swap of the dispatcher.
    #[test]
    fn clamped_rumble_maps_apply_before_seat_and_brake_contributions_are_added() {
        let events = Events { rumble_strips: false, gear_change: false, wheel_slip: false, wheel_lock: false };
        let mut telemetry = quiet();
        telemetry.longitudinal_acceleration = 2.5;
        telemetry.vertical_acceleration = -0.5;
        assert_eq!(seat_level(&seat(), events, &telemetry, 0.0, &tuning(), &MAXIMA, None, None, None).unwrap(), 127.5);
        telemetry.longitudinal_acceleration = -2.5;
        assert_eq!(brake_level(&tuning().brake, events, &telemetry, 0.0, &MAXIMA, None, None, None).unwrap(), 127.5);
        telemetry.longitudinal_acceleration = 10.0;
        telemetry.vertical_acceleration = 0.0;
        assert_eq!(seat_level(&seat(), events, &telemetry, 0.0, &tuning(), &MAXIMA, None, None, None).unwrap(), 255.0);
        telemetry.longitudinal_acceleration = 0.0;
        telemetry.vertical_acceleration = f64::NAN;
        assert_eq!(seat_level(&seat(), events, &telemetry, 0.0, &tuning(), &MAXIMA, None, None, None).unwrap(), 255.0);
    }

    #[test]
    fn port_levels_follow_roles() {
        let sim = Sim3d::new(ShakeIt::new(Box::new(FakeLink::new())));
        let ports = [PortRole::Throttle, PortRole::Brake, PortRole::WindRight, PortRole::WindCenter];
        let mut t = quiet();
        t.longitudinal_acceleration = -2.5; // braking
        let fans = Sim3dFans { left: 10, center: 20, right: 30 };
        let levels = sim.port_levels(&t, fans, &ports, &tuning(), &MAXIMA).unwrap();
        assert_eq!(levels, [0, 127, 30, 20]);
        t.abs_lock = 1.0;
        t.longitudinal_acceleration = 0.0;
        assert_eq!(sim.port_levels(&t, fans, &ports, &tuning(), &MAXIMA).unwrap()[1], 204);
        let mut gear = quiet();
        gear.gear = 2.0;
        let clutch = sim.port_levels(&gear, fans, &[PortRole::Clutch; 4], &tuning(), &MAXIMA).unwrap();
        assert_eq!(clutch, [255; 4]);
        let mut slip = quiet();
        slip.wheel_slip_rr = 1.5; // > 30/25
        slip.longitudinal_acceleration = 1.0; // accelerating: no brake lock, throttle slip
        let l = sim.port_levels(&slip, fans, &ports, &tuning(), &MAXIMA).unwrap();
        assert_eq!(l[0], 127);
        assert_eq!(l[1], 0);
    }

    #[test]
    fn native_map_maxima_fail_after_controls_and_before_output() {
        let failure = |key| NativeEngineError { class: "KeyError", message: format!("'{key}'") };
        let mut tune = tuning();
        tune.throttle.gear.enabled = false;
        tune.brake.gear.enabled = false;
        tune.seat.gear.enabled = false;
        tune.native_map_maxima = Some(super::super::NativeMapMaxima {
            surge_acceleration: Err(failure("surge_acceleration")),
            surge_deceleration: Ok(5.0),
            heave: Err(failure("heave")),
        });
        let mut telemetry = quiet();
        telemetry.gear = 4.0;
        let wire = FakeLink::new();
        let mut sim = Sim3d::new(ShakeIt::new(Box::new(wire.clone())));
        let clock = ManualClock::default();
        let fans = Sim3dFans { left: 0, center: 0, right: 0 };
        let switches = Sim3dSwitches { test_port: 0, on: true, game_running: true };
        assert!(
            matches!(sim.send_stream(&telemetry,fans,&[PortRole::Clutch;4],&switches,&tune,&MAXIMA,&clock), Err(Sim3dError::NativeEngine(error)) if error==failure("surge_acceleration"))
        );
        tune.native_map_maxima.as_mut().unwrap().surge_acceleration = Ok(5.0);
        assert!(
            matches!(sim.send_stream(&telemetry,fans,&[PortRole::Clutch;4],&switches,&tune,&MAXIMA,&clock), Err(Sim3dError::NativeEngine(error)) if error==failure("heave"))
        );
        assert!(wire.take_written().is_empty());
        assert_eq!(sim.previous_gear, LegacyValue::Int(0));
        tune.native_map_maxima.as_mut().unwrap().heave = Ok(2.0);
        sim.send_stream(&telemetry, fans, &[PortRole::Clutch; 4], &switches, &tune, &MAXIMA, &clock).unwrap();
        assert_eq!(&wire.take_written()[4..11], &[3, 86, 83, 255, 255, 255, 255]);
        sim.send_stream(&telemetry, fans, &[PortRole::Clutch; 4], &switches, &tune, &MAXIMA, &clock).unwrap();
        assert_eq!(&wire.take_written()[4..11], &[3, 86, 83, 0, 0, 0, 0]);
    }

    #[test]
    fn brake_event_priority_follows_native_order() {
        let sim = Sim3d::new(ShakeIt::new(Box::new(FakeLink::new())));
        let mut tune = tuning();
        tune.brake.abs_lock = effect(16);
        tune.brake.wheel_lock = effect(12);
        tune.brake.rumble_strips = effect(8);
        tune.brake.gear = effect(4);
        let mut telemetry = quiet();
        telemetry.abs_lock = 1.0;
        telemetry.wheel_slip_lf = 3.0;
        telemetry.rumble_pitch_lf = 6.0;
        telemetry.gear = 4.0;
        let fans = Sim3dFans { left: 0, center: 0, right: 0 };
        for (index, expected) in [204, 153, 102, 51].into_iter().enumerate() {
            assert_eq!(
                sim.port_levels(&telemetry, fans, &[PortRole::Brake; 4], &tune, &MAXIMA).unwrap(),
                [expected; 4],
                "ABS, wheel lock, rumble strips, then gear; branch {index}"
            );
            match index {
                0 => tune.brake.abs_lock.enabled = false,
                1 => tune.brake.wheel_lock.enabled = false,
                2 => tune.brake.rumble_strips.enabled = false,
                _ => {}
            }
        }
    }

    #[test]
    fn native_continuous_controls_read_before_maps_and_skip_after_early_effects() {
        let failure = |key| NativeEngineError { class: "KeyError", message: format!("'{key}'") };
        let good = || NativeEngineControl { enabled: Ok(true), level: Ok((10).into()) };
        let mut controls = std::array::from_fn(|_| good());
        let events = Events { rumble_strips: false, gear_change: false, wheel_slip: false, wheel_lock: false };
        let tune = tuning();
        let mut telemetry = quiet();
        for index in 0..3 {
            controls[index].level = Err(failure("selected level"));
            for surge in [-1.0, 0.0, 1.0] {
                telemetry.longitudinal_acceleration = surge;
                assert!(
                    matches!(brake_level(&tune.brake, events, &telemetry, 0.0, &MAXIMA, None, Some(&controls), None),
                    Err(Sim3dError::NativeEngine(error)) if error == failure("selected level"))
                );
                assert!(
                    matches!(seat_level(&tune.seat, events, &telemetry, 0.0, &tune, &MAXIMA, None, None, Some(&controls)),
                    Err(Sim3dError::NativeEngine(error)) if error == failure("selected level"))
                );
            }
            controls[index].enabled = Ok(false);
            assert!(brake_level(&tune.brake, events, &telemetry, 0.0, &MAXIMA, None, Some(&controls), None).is_ok());
            controls[index] = good();
        }
        controls[0].level = Err(failure("surge"));
        controls[1].level = Err(failure("heave"));
        controls[2].level = Err(failure("traction"));
        assert!(matches!(brake_level(&tune.brake, events, &telemetry, 0.0, &MAXIMA, None, Some(&controls), None),
            Err(Sim3dError::NativeEngine(error)) if error == failure("surge")));
        controls[0].enabled = Ok(false);
        assert!(matches!(brake_level(&tune.brake, events, &telemetry, 0.0, &MAXIMA, None, Some(&controls), None),
            Err(Sim3dError::NativeEngine(error)) if error == failure("heave")));
        telemetry.abs_lock = 1.0;
        assert_eq!(
            brake_level(&tune.brake, events, &telemetry, 0.0, &MAXIMA, None, Some(&controls), None).unwrap(),
            204.0
        );
        let gear_event = Events { gear_change: true, ..events };
        assert_eq!(
            seat_level(&tune.seat, gear_event, &telemetry, 0.0, &tune, &MAXIMA, None, None, Some(&controls)).unwrap(),
            127.5
        );
    }

    #[test]
    fn native_continuous_failure_keeps_group_order_and_pending_gear() {
        let failure = |key| NativeEngineError { class: "KeyError", message: format!("'{key}'") };
        let good = || NativeEngineControl { enabled: Ok(true), level: Ok((10).into()) };
        let mut tune = tuning();
        tune.throttle.gear.enabled = false;
        tune.brake.gear.enabled = false;
        tune.seat.gear.enabled = false;
        tune.native_continuous_controls = Some(NativeSim3dContinuous {
            throttle: std::array::from_fn(|_| good()),
            brake: std::array::from_fn(|_| good()),
            seat: std::array::from_fn(|_| good()),
        });
        let controls = tune.native_continuous_controls.as_mut().unwrap();
        controls.throttle[1].level = Err(failure("throttle_heave"));
        controls.brake[0].level = Err(failure("brake_surge_deceleration"));
        controls.seat[2].level = Err(failure("seat_traction_loss"));
        let link = FakeLink::new();
        let mut sim = Sim3d::new(ShakeIt::new(Box::new(link.clone())));
        let clock = ManualClock::default();
        let mut telemetry = quiet();
        telemetry.gear = 4.0;
        let fans = Sim3dFans { left: 0, center: 0, right: 0 };
        let switches = Sim3dSwitches { test_port: 0, on: true, game_running: true };
        for key in ["throttle_heave", "brake_surge_deceleration", "seat_traction_loss"] {
            assert!(
                matches!(sim.send_stream(&telemetry, fans, &[PortRole::Clutch; 4], &switches, &tune, &MAXIMA, &clock),
                Err(Sim3dError::NativeEngine(error)) if error == failure(key))
            );
            assert!(link.take_written().is_empty());
            assert_eq!(sim.previous_gear, LegacyValue::Int(0));
            let controls = tune.native_continuous_controls.as_mut().unwrap();
            match key {
                "throttle_heave" => controls.throttle[1] = good(),
                "brake_surge_deceleration" => controls.brake[0] = good(),
                _ => controls.seat[2] = good(),
            }
        }
        sim.send_stream(&telemetry, fans, &[PortRole::Clutch; 4], &switches, &tune, &MAXIMA, &clock).unwrap();
        assert_eq!(&link.take_written()[4..11], &[3, 86, 83, 255, 255, 255, 255]);
        sim.send_stream(&telemetry, fans, &[PortRole::Clutch; 4], &switches, &tune, &MAXIMA, &clock).unwrap();
        assert_eq!(&link.take_written()[4..11], &[3, 86, 83, 0, 0, 0, 0]);
        tune.native_continuous_controls.as_mut().unwrap().brake[0].level = Err(failure("brake_surge_deceleration"));
        assert!(matches!(
            sim.send_stream(
                &telemetry,
                fans,
                &[PortRole::Clutch; 4],
                &Sim3dSwitches { on: false, ..switches },
                &tune,
                &MAXIMA,
                &clock
            ),
            Err(Sim3dError::NativeEngine(_))
        ));
        assert!(link.take_written().is_empty());
        sim.send_stream(
            &telemetry,
            fans,
            &[PortRole::Clutch; 4],
            &Sim3dSwitches { game_running: false, ..switches },
            &tune,
            &MAXIMA,
            &clock,
        )
        .unwrap();
        assert!(link.take_written().is_empty());
        sim.send_stream(
            &telemetry,
            fans,
            &[PortRole::Clutch; 4],
            &Sim3dSwitches { test_port: 2, ..switches },
            &tune,
            &MAXIMA,
            &clock,
        )
        .unwrap();
        assert_eq!(&link.take_written()[4..11], &[3, 86, 83, 0, 128, 0, 0]);
    }

    #[test]
    fn native_brake_controls_preserve_reached_gates() {
        let bad = NativeEngineError { class: "KeyError", message: "'unused brake control'".into() };
        let disabled = || NativeEngineControl { enabled: Ok(false), level: Err(bad.clone()) };
        let mut controls = std::array::from_fn(|_| disabled());
        let mut telemetry = quiet();
        let mut events = Events { rumble_strips: false, gear_change: false, wheel_slip: false, wheel_lock: false };
        let effects = tuning().brake;
        controls[0].enabled = Err(bad.clone());
        assert_eq!(brake_level(&effects, events, &telemetry, 0.0, &MAXIMA, Some(&controls), None, None).unwrap(), 0.0);
        telemetry.abs_lock = 1.0;
        assert!(matches!(
            brake_level(&effects, events, &telemetry, 0.0, &MAXIMA, Some(&controls), None, None),
            Err(Sim3dError::NativeEngine(error)) if error == bad
        ));
        controls[0] = disabled();
        assert_eq!(brake_level(&effects, events, &telemetry, 0.0, &MAXIMA, Some(&controls), None, None).unwrap(), 0.0);
        controls[1].enabled = Err(bad.clone());
        events.wheel_lock = true;
        telemetry.longitudinal_acceleration = 1.0;
        assert_eq!(brake_level(&effects, events, &telemetry, 0.0, &MAXIMA, Some(&controls), None, None).unwrap(), 0.0);
        telemetry.longitudinal_acceleration = 0.0;
        assert!(matches!(
            brake_level(&effects, events, &telemetry, 0.0, &MAXIMA, Some(&controls), None, None),
            Err(Sim3dError::NativeEngine(error)) if error == bad
        ));
        controls[0] = NativeEngineControl { enabled: Ok(true), level: Ok((8).into()) };
        events.rumble_strips = true;
        events.gear_change = true;
        controls[2].enabled = Err(bad.clone());
        controls[3].enabled = Err(bad);
        assert_eq!(
            brake_level(&effects, events, &telemetry, 0.0, &MAXIMA, Some(&controls), None, None).unwrap(),
            102.0
        );
    }

    #[test]
    fn native_brake_error_skips_send_and_retains_gear_until_recovery() {
        let failure = |key| NativeEngineError { class: "KeyError", message: format!("'{key}'") };
        let good = || NativeEngineControl { enabled: Ok(true), level: Ok((8).into()) };
        let mut tune = tuning();
        tune.native_event_controls = Some(NativeSim3dEvents {
            throttle: std::array::from_fn(|_| good()),
            seat: std::array::from_fn(|_| good()),
            brake: std::array::from_fn(|_| good()),
            clutch: NativeEngineControl { enabled: Ok(true), level: Err(failure("clutch_gear")) },
        });
        tune.native_event_controls.as_mut().unwrap().brake[0].level = Err(failure("brake_abs_lock"));
        let mut telemetry = quiet();
        telemetry.abs_lock = 1.0;
        telemetry.wheel_slip_lf = 3.0;
        telemetry.rumble_pitch_lf = 6.0;
        telemetry.gear = 4.0;
        let link = FakeLink::new();
        let mut sim = Sim3d::new(ShakeIt::new(Box::new(link.clone())));
        let clock = ManualClock::default();
        let fans = Sim3dFans { left: 0, center: 0, right: 0 };
        let switches = Sim3dSwitches { test_port: 0, on: true, game_running: true };
        for on in [true, false] {
            assert!(matches!(
                sim.send_stream(&telemetry, fans, &[PortRole::Brake; 4], &Sim3dSwitches { on, ..switches }, &tune, &MAXIMA, &clock),
                Err(Sim3dError::NativeEngine(error)) if error == failure("brake_abs_lock")
            ));
            assert!(link.take_written().is_empty());
            assert_eq!(sim.previous_gear, LegacyValue::Int(0));
        }
        telemetry.abs_lock = 0.0;
        tune.native_event_controls.as_mut().unwrap().brake[1].level = Err(failure("brake_wheel_slip"));
        assert!(matches!(
            sim.send_stream(&telemetry, fans, &[PortRole::Brake; 4], &switches, &tune, &MAXIMA, &clock),
            Err(Sim3dError::NativeEngine(error)) if error == failure("brake_wheel_slip")
        ));
        assert!(link.take_written().is_empty());
        tune.native_event_controls.as_mut().unwrap().brake[1].enabled = Ok(false);
        for (index, key) in [(2, "brake_rumble_strips"), (3, "brake_gear")] {
            tune.native_event_controls.as_mut().unwrap().brake[index].level = Err(failure(key));
            assert!(matches!(
                sim.send_stream(&telemetry, fans, &[PortRole::Brake; 4], &switches, &tune, &MAXIMA, &clock),
                Err(Sim3dError::NativeEngine(error)) if error == failure(key)
            ));
            assert!(link.take_written().is_empty());
            assert_eq!(sim.previous_gear, LegacyValue::Int(0));
            tune.native_event_controls.as_mut().unwrap().brake[index].enabled = Ok(false);
        }
        tune.native_event_controls.as_mut().unwrap().brake[1] = good();
        tune.native_event_controls.as_mut().unwrap().clutch = good();
        sim.send_stream(&telemetry, fans, &[PortRole::Clutch; 4], &switches, &tune, &MAXIMA, &clock).unwrap();
        assert_eq!(&link.take_written()[4..11], &[3, 86, 83, 102, 102, 102, 102]);
        sim.send_stream(&telemetry, fans, &[PortRole::Clutch; 4], &switches, &tune, &MAXIMA, &clock).unwrap();
        assert_eq!(&link.take_written()[4..11], &[3, 86, 83, 0, 0, 0, 0]);
        telemetry.abs_lock = 1.0;
        sim.send_stream(
            &telemetry,
            fans,
            &[PortRole::Brake; 4],
            &Sim3dSwitches { game_running: false, ..switches },
            &tune,
            &MAXIMA,
            &clock,
        )
        .unwrap();
        assert!(link.take_written().is_empty());
        sim.send_stream(
            &telemetry,
            fans,
            &[PortRole::Brake; 4],
            &Sim3dSwitches { test_port: 2, ..switches },
            &tune,
            &MAXIMA,
            &clock,
        )
        .unwrap();
        assert_eq!(&link.take_written()[4..11], &[3, 86, 83, 0, 128, 0, 0]);
    }

    /// The traction-loss normalisation uses 0 when its division fails,
    /// so a zero maximum keeps the frame (pedals and wind still computed).
    #[test]
    fn zero_traction_maximum_keeps_the_frame() {
        let sim = Sim3d::new(ShakeIt::new(Box::new(FakeLink::new())));
        let ports = [PortRole::Throttle, PortRole::Brake, PortRole::WindRight, PortRole::WindCenter];
        let mut t = quiet();
        t.longitudinal_acceleration = -2.5;
        t.lateral_velocity = 4.0;
        let fans = Sim3dFans { left: 10, center: 20, right: 30 };
        let maxima = Sim3dMaxima { traction_loss: 0.0, ..MAXIMA };
        assert_eq!(sim.port_levels(&t, fans, &ports, &tuning(), &maxima).unwrap(), [0, 127, 30, 20]);
    }

    #[test]
    fn native_event_controls_preserve_stage_order_and_gear_commit() {
        let failure = |name| NativeEngineError { class: "KeyError", message: format!("'{name}'") };
        let good = || NativeEngineControl { enabled: Ok(true), level: Ok((10).into()) };
        let mut tune = tuning();
        tune.native_event_controls = Some(NativeSim3dEvents {
            throttle: std::array::from_fn(|_| good()),
            seat: std::array::from_fn(|_| good()),
            brake: std::array::from_fn(|_| good()),
            clutch: good(),
        });
        tune.native_event_controls.as_mut().unwrap().brake[3].enabled = Ok(false);
        tune.native_event_controls.as_mut().unwrap().clutch.level = Err(failure("clutch_gear"));
        let mut telemetry = quiet();
        telemetry.gear = 4.0;
        let link = FakeLink::new();
        let mut sim = Sim3d::new(ShakeIt::new(Box::new(link.clone())));
        let clock = ManualClock::default();
        let fans = Sim3dFans { left: 0, center: 0, right: 0 };
        let switches = Sim3dSwitches { test_port: 0, on: true, game_running: true };
        assert!(
            matches!(sim.send_stream(&telemetry,fans,&[PortRole::Clutch;4],&switches,&tune,&MAXIMA,&clock),Err(Sim3dError::NativeEngine(error)) if error==failure("clutch_gear"))
        );
        assert!(link.take_written().is_empty());
        assert_eq!(sim.previous_gear, LegacyValue::Int(0));
        tune.native_event_controls.as_mut().unwrap().throttle[2].level = Err(failure("throttle_gear"));
        assert!(
            matches!(sim.send_stream(&telemetry,fans,&[PortRole::Clutch;4],&switches,&tune,&MAXIMA,&clock),Err(Sim3dError::NativeEngine(error)) if error==failure("throttle_gear"))
        );
        tune.native_event_controls.as_mut().unwrap().throttle[2] = good();
        tune.native_event_controls.as_mut().unwrap().clutch = good();
        tune.native_event_controls.as_mut().unwrap().seat[2].level = Err(failure("seat_gear"));
        assert!(
            matches!(sim.send_stream(&telemetry,fans,&[PortRole::Clutch;4],&switches,&tune,&MAXIMA,&clock),Err(Sim3dError::NativeEngine(error)) if error==failure("seat_gear"))
        );
        assert!(link.take_written().is_empty());
        assert_eq!(sim.previous_gear, LegacyValue::Int(0));
        tune.native_event_controls.as_mut().unwrap().seat[2] = good();
        sim.send_stream(&telemetry, fans, &[PortRole::Clutch; 4], &switches, &tune, &MAXIMA, &clock).unwrap();
        assert_eq!(&link.take_written()[4..11], &[3, 86, 83, 127, 127, 127, 127]);
        telemetry.rpm = 0.0;
        sim.send_stream(&telemetry, fans, &[PortRole::Clutch; 4], &switches, &tune, &MAXIMA, &clock).unwrap();
        assert_eq!(&link.take_written()[4..11], &[3, 86, 83, 0, 0, 0, 0]);
    }

    #[test]
    fn native_event_controls_skip_inactive_and_disabled_levels() {
        let bad = NativeEngineError { class: "ValueError", message: "bad level".into() };
        let mut tune = tuning();
        tune.native_event_controls = Some(NativeSim3dEvents {
            throttle: std::array::from_fn(|_| NativeEngineControl { enabled: Ok(false), level: Err(bad.clone()) }),
            seat: std::array::from_fn(|_| NativeEngineControl { enabled: Ok(false), level: Err(bad.clone()) }),
            brake: std::array::from_fn(|_| NativeEngineControl { enabled: Ok(false), level: Err(bad.clone()) }),
            clutch: NativeEngineControl { enabled: Ok(false), level: Err(bad.clone()) },
        });
        let mut telemetry = quiet();
        telemetry.wheel_slip_lf = 2.0;
        telemetry.rumble_pitch_lf = 6.0;
        telemetry.gear = 4.0;
        let sim = Sim3d::new(ShakeIt::new(Box::new(FakeLink::new())));
        assert!(sim
            .port_levels(
                &telemetry,
                Sim3dFans { left: 0, center: 0, right: 0 },
                &[PortRole::Throttle; 4],
                &tune,
                &MAXIMA
            )
            .is_ok());
        let control = &mut tune.native_event_controls.as_mut().unwrap().throttle[0];
        control.enabled = Ok(true);
        assert!(
            matches!(sim.port_levels(&telemetry,Sim3dFans {left:0,center:0,right:0},&[PortRole::Throttle;4],&tune,&MAXIMA),Err(Sim3dError::NativeEngine(error)) if error==bad)
        );
    }

    #[test]
    fn native_engine_control_selects_each_group_and_preserves_send_prefix() {
        let fail =
            NativeEngineError { class: "ValueError", message: "invalid literal for int() with base 10: '9.5'".into() };
        let mut tune = tuning();
        tune.throttle.gear.enabled = false;
        tune.seat.gear.enabled = false;
        tune.native_engine_control = [
            Some(NativeEngineControl { enabled: Ok(true), level: Err(fail.clone()) }),
            Some(NativeEngineControl { enabled: Ok(false), level: Err(fail.clone()) }),
        ];
        let mut telemetry = quiet();
        telemetry.rpm = 4000.0;
        let link = FakeLink::new();
        let mut sim = Sim3d::new(ShakeIt::new(Box::new(link.clone())));
        let clock = ManualClock::default();
        let fans = Sim3dFans { left: 0, center: 0, right: 0 };
        let switches = Sim3dSwitches { test_port: 0, on: true, game_running: true };
        assert!(
            matches!(sim.send_stream(&telemetry,fans,&[PortRole::Throttle;4],&switches,&tune,&MAXIMA,&clock),Err(Sim3dError::NativeEngine(error)) if error==fail)
        );
        assert!(link.take_written().is_empty());
        tune.native_engine_control[0].as_mut().unwrap().enabled = Ok(false);
        sim.send_stream(&telemetry, fans, &[PortRole::Throttle; 4], &switches, &tune, &MAXIMA, &clock).unwrap();
        assert_eq!(&link.take_written()[4..11], &[3, 86, 83, 0, 0, 0, 0]);
        tune.native_engine_control[1].as_mut().unwrap().enabled = Ok(true);
        assert!(matches!(
            sim.send_stream(&telemetry, fans, &[PortRole::Throttle; 4], &switches, &tune, &MAXIMA, &clock),
            Err(Sim3dError::NativeEngine(_))
        ));
        assert!(link.take_written().is_empty(), "seat is evaluated even when ports select throttle");
    }

    #[test]
    fn native_rpm_error_skips_motor_send_and_retains_previous_gear() {
        let link = FakeLink::new();
        let mut sim = Sim3d::new(ShakeIt::new(Box::new(link.clone())));
        let clock = ManualClock::default();
        let bad = NativeEngineError { class: "KeyError", message: "'rpm_start_rumble'".into() };
        let mut tune = tuning();
        tune.throttle.gear.enabled = false;
        tune.seat.gear.enabled = false;
        tune.native_engine_rpm =
            Some(NativeEngineRpm { start: Err(bad.clone()), fade_start: Ok((100).into()), fade_end: Ok((95).into()) });
        let mut telemetry = quiet();
        telemetry.rpm = 4000.0;
        telemetry.gear = 4.0;
        let fans = Sim3dFans { left: 0, center: 0, right: 0 };
        let switches = Sim3dSwitches { test_port: 0, on: true, game_running: true };
        for on in [true, false] {
            assert!(
                matches!(sim.send_stream(&telemetry, fans, &[PortRole::Throttle;4], &Sim3dSwitches {on,..switches}, &tune, &MAXIMA, &clock), Err(Sim3dError::NativeEngine(error)) if error == bad)
            );
            assert!(link.take_written().is_empty());
            assert_eq!(sim.previous_gear, LegacyValue::Int(0));
        }
        tune.throttle.gear.enabled = true;
        tune.seat.gear.enabled = true;
        sim.send_stream(&telemetry, fans, &[PortRole::Throttle; 4], &switches, &tune, &MAXIMA, &clock).unwrap();
        assert_eq!(&link.take_written()[4..11], &[3, 86, 83, 127, 127, 127, 127]);
        assert!(matches!(
            sim.send_stream(&telemetry, fans, &[PortRole::Throttle; 4], &switches, &tune, &MAXIMA, &clock),
            Err(Sim3dError::NativeEngine(_))
        ));
        assert!(link.take_written().is_empty());
        tune.native_engine_rpm =
            Some(NativeEngineRpm { start: Ok((30).into()), fade_start: Ok((70).into()), fade_end: Ok((95).into()) });
        sim.send_stream(&telemetry, fans, &[PortRole::Throttle; 4], &switches, &tune, &MAXIMA, &clock).unwrap();
        assert!(!link.take_written().is_empty());
        tune.native_engine_rpm.as_mut().unwrap().start = Err(bad);
        sim.send_stream(
            &telemetry,
            fans,
            &[PortRole::Throttle; 4],
            &Sim3dSwitches { game_running: false, ..switches },
            &tune,
            &MAXIMA,
            &clock,
        )
        .unwrap();
        assert!(link.take_written().is_empty());
        sim.send_stream(
            &telemetry,
            fans,
            &[PortRole::Throttle; 4],
            &Sim3dSwitches { test_port: 2, ..switches },
            &tune,
            &MAXIMA,
            &clock,
        )
        .unwrap();
        assert!(!link.take_written().is_empty());
    }

    #[test]
    fn gear_changes_compare_native_values_instead_of_the_numeric_projection() {
        let link = FakeLink::new();
        let mut sim = Sim3d::new(ShakeIt::new(Box::new(link.clone())));
        let clock = ManualClock::default();
        let switches = Sim3dSwitches { test_port: 0, on: true, game_running: true };
        for (value, changed) in super::super::gear_comparison_samples() {
            let mut telemetry = Telemetry::off();
            telemetry.set_native("gear", value.clone());
            sim.send_stream(
                &telemetry,
                Sim3dFans { left: 0, center: 0, right: 0 },
                &[PortRole::Clutch; 4],
                &switches,
                &tuning(),
                &MAXIMA,
                &clock,
            )
            .unwrap();
            let frame = link.take_written();
            let level = if changed { 255 } else { 0 };
            assert_eq!(&frame[4..11], &[3, 86, 83, level, level, level, level], "{value:?}");
        }
    }

    #[test]
    fn missing_stream_gear_uses_zero_without_filling_shared_telemetry() {
        let link = FakeLink::new();
        let mut sim = Sim3d::new(ShakeIt::new(Box::new(link.clone())));
        let clock = ManualClock::default();
        let empty = Telemetry::unfilled();
        let switches = Sim3dSwitches { test_port: 0, on: true, game_running: true };
        link.queue_reply(&[3, 0]);
        sim.send_stream(
            &empty,
            Sim3dFans { left: 0, center: 0, right: 0 },
            &[PortRole::Clutch; 4],
            &switches,
            &tuning(),
            &MAXIMA,
            &clock,
        )
        .unwrap();
        assert_eq!(link.take_written(), ShakeIt::new(Box::new(FakeLink::new())).pack(&[3, 86, 83, 0, 0, 0, 0]));
        assert!(empty.channel_names().is_empty());
        assert_eq!(empty.get_assigned("gear"), None);
    }

    /// Scan: Hello answered with a byte reply on a CH340 port.
    #[test]
    fn scan_finds_board() {
        let link = FakeLink::new();
        link.queue_reply(&[3, 0, 8, 1]);
        let opener = FakeOpener::with("/dev/ttyUSB2", &link);
        let ports = FakePorts(vec![PortEntry::usb("/dev/ttyUSB2", 0x1a86, 0x7523, None, None)]);
        let clock = ManualClock::default();
        let claimed = ClaimedPorts::default();
        let allowed = AllowedPorts::default();
        let ctx = Sim3dScanContext {
            ports: &ports,
            opener: &opener,
            clock: &clock,
            claimed: &claimed,
            cached_port: None,
            com_port: "",
            allowed: &allowed,
        };
        let found = Sim3dScanner::new().scan(&ctx).unwrap().expect("board");
        assert_eq!(found.port, "/dev/ttyUSB2");
        assert_eq!(opener.opened()[0].1.baud_rate, 19_200);
    }
}
