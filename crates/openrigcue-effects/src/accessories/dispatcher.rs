//! The accessory dispatcher: refresh rates, the per-device refresh cycles and the connection scan that
//! (re)connects devices and notices unplugged ones.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tracing::debug;

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::alerts::{BeepInputs, BeepPlayer, ShiftBeep};
use super::belt::{
    BeltCalibration, BeltIdentity, BeltInputs, BeltMaxima, BeltScanContext, BeltScanner, BeltState, BeltSwitches,
    BeltTuning, BeltWatchdog, DayondBus, Homing, PublishedHoming,
};
use super::compensation::{Compensation, CompensationError, CompensationInputs, MotionSensor, RigPose};
use super::firmware::{Avrdude, IntelliboxFlasher};
use super::hapticr::{
    HaptiCr, HapticMaxima, HapticScan, HapticScanContext, HapticScanner, HapticSwitches, HapticTuning,
};
use super::iflag::{IFlagInputs, IFlagPanel, IFlagScanContext, IFlagScanner, IFlagSettings, IFlagStream};
use super::imu::{self, Imu, ImuSamples, ImuScanContext, InstallationReader, NativeInstallationReader};
use super::intellibox::{
    InitParameters, Intellibox, IntelliboxScanner, IntelliboxStream, NativeInitParameters, ScanContext, ScanSettings,
    SegmentRanges, StreamFrame,
};
use super::led::{LedByteOrder, LedConfig, LedError, LedInputs, LedMode, LedStrip, NativeLedSettings, SegmentFrame};
use super::link::{Clock, PortLister, PortOpener};
use super::port_scan::{AllowedPorts, ClaimedPorts, Device};
use super::pose_log::PoseLog;
use super::serial_events::ScanTrigger;
use super::sim3d::{
    PortRole, Sim3d, Sim3dError, Sim3dFans, Sim3dMaxima, Sim3dScanContext, Sim3dScanner, Sim3dSwitches, Sim3dTuning,
};
use super::status::{Connection, DeviceStatus, PublishedParameter, PublishedPort, PublishedStatus};
use super::wind::{
    lla_levels_for_telemetry, FanLevels, FanMode, WindError, WindInputs, WindSimulator, WindTuning,
    INTELLIBOX_PWM_RANGE, SIM3D_FAN_COUNT, SIM3D_PWM_RANGE,
};

/// Seconds between two cycles for a `refresh_rate` in Hz (`1.0 / refresh`). `None` when there is no
/// valid pause: a zero rate or a negative one;
/// that ends the refresh loop which sleeps it. The rate is not capped.
pub fn refresh_period(rate_hz: i64) -> Option<Duration> {
    (rate_hz > 0).then(|| openrigcue_model::python_time::sleep_duration(1.0 / rate_hz as f64).ok()).flatten()
}

/// The belt cycle reads these required values before the stream step,
/// including when the belt switch is off.
fn required_belt_inputs(telemetry: &Telemetry) -> Result<BeltInputs, &'static str> {
    Ok(BeltInputs {
        surge: telemetry.get_assigned("longitudinal_acceleration").ok_or("longitudinal_acceleration")?,
        sway: telemetry.get_assigned("lateral_acceleration").ok_or("lateral_acceleration")?,
        heave: telemetry.get_assigned("vertical_acceleration").ok_or("vertical_acceleration")?,
    })
}

/// The belt stream multiplies raw sway/heave by the integer -1 before its
/// switch checks. Numeric projections cannot retain these operand failures.
/// Pause of the belt, iFlag and SIM3D loops while their device is not connected.
pub const IDLE_PAUSE: Duration = Duration::from_secs(2);
/// Pause before each connection scan without device events.
pub const SCAN_PAUSE: Duration = Duration::from_secs(1);
/// Longest wait for a device event while the motion platform is a network one.
pub const NETWORK_MOTION_WAKE: Duration = Duration::from_secs(1);

/// How long the event-driven connection scan waits for a serial device event: 1 s when the configured
/// motion platform is not a serial one (so its network discovery keeps running), otherwise until an
/// event (`None`, also when the model is unknown).
pub fn serial_event_wait_timeout(motion_platform_is_serial: Option<bool>) -> Option<Duration> {
    (motion_platform_is_serial == Some(false)).then_some(NETWORK_MOTION_WAKE)
}

/// Engine rotation frequency: `rpm * (2 / 120)`, truncated.
pub fn rpm_tone(rpm: f64) -> Option<i64> {
    super::numeric::truncate(rpm * (2.0 / 120.0))
}

/// Call `cycle` until `stop` is set or the cycle has no pause to sleep, sleeping on `clock` for the pause
/// each call returns (the shape of every refresh thread: cycle, then sleep; a cycle without a valid
/// pause ends the thread).
pub fn run_periodic(stop: &AtomicBool, clock: &dyn Clock, mut cycle: impl FnMut() -> Option<Duration>) {
    while !stop.load(Ordering::SeqCst) {
        let Some(pause) = cycle() else {
            debug!("refresh loop ended: its refresh rate gives no valid sleep");
            return;
        };
        clock.sleep(pause);
    }
}

/// Why a refresh cycle produced nothing (the error is logged and the loop sleeps).
#[derive(Debug, thiserror::Error)]
pub enum CycleError {
    #[error("'{0}'")]
    MissingTelemetry(&'static str),
    #[error("rpm is not a finite number")]
    Rpm,
    #[error("{0}")]
    Native(String),
    #[error(transparent)]
    Wind(#[from] WindError),
    #[error(transparent)]
    Led(#[from] LedError),
    #[error(transparent)]
    Intellibox(#[from] super::intellibox::IntelliboxError),
    #[error(transparent)]
    Sim3d(#[from] Sim3dError),
    #[error(transparent)]
    HaptiCr(#[from] super::hapticr::HapticError),
    #[error(transparent)]
    Belt(#[from] super::belt::BeltError),
    #[error(transparent)]
    IFlag(#[from] super::iflag::IFlagError),
}

/// Wind inputs that come from the runtime parameters rather than telemetry.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindSwitches {
    pub left_right: FanMode,
    pub center: FanMode,
    pub dual_max_steering_percent: f64,
    pub ejection_requested: bool,
    /// The LLA service bypass switch.
    pub lla: bool,
}

fn fan_levels(
    wind: &Mutex<WindSimulator>,
    fan_count: i64,
    range: f64,
    t: &Telemetry,
    switches: &WindSwitches,
    tuning: &WindTuning,
) -> Result<FanLevels, CycleError> {
    if switches.lla {
        return lla_levels_for_telemetry(
            t,
            required_channel(t, "speed_kmh")?,
            required_channel(t, "steering_wheel_angle")?,
            range,
        )
        .map_err(CycleError::from);
    }
    let inputs = WindInputs {
        speed_kmh: required_channel(t, "speed_kmh")?,
        max_speed_kmh: required_channel(t, "max_speed_kmh")?,
        steering_angle: required_channel(t, "steering_wheel_angle")?,
        steering_max_angle: required_channel(t, "steering_wheel_max_angle")?,
        dual_max_steering_percent: switches.dual_max_steering_percent,
        left_right_mode: switches.left_right,
        center_mode: switches.center,
        ejection_requested: switches.ejection_requested,
    };
    wind.lock().unwrap().fan_levels_for_telemetry(fan_count, range, &inputs, tuning, t).map_err(CycleError::from)
}

fn required_channel(telemetry: &Telemetry, name: &'static str) -> Result<f64, CycleError> {
    telemetry.get_assigned(name).ok_or(CycleError::MissingTelemetry(name))
}

/// The Intellibox connection and its stream state.
#[derive(Default)]
pub struct IntelliboxSlot {
    connection: Connection<Intellibox>,
    status: PublishedStatus,
    port: String,
    port_parameter: PublishedPort,
    /// The box's model and version (both start at -1).
    model: PublishedParameter<i64>,
    version: PublishedParameter<i64>,
    stream: IntelliboxStream,
    led: LedStrip,
}

impl IntelliboxSlot {
    fn new() -> Self {
        IntelliboxSlot {
            model: PublishedParameter::new(-1),
            version: PublishedParameter::new(-1),
            ..Default::default()
        }
    }

    pub fn model(&self) -> i64 {
        self.model.get()
    }
    pub fn version(&self) -> i64 {
        self.version.get()
    }
    pub fn set_model(&mut self, model: i64) {
        self.model.set(model);
    }
    pub fn set_version(&mut self, version: i64) {
        self.version.set(version);
    }
}

/// Per-frame inputs of the Intellibox cycle.
pub struct IntelliboxInputs<'a> {
    pub telemetry: &'a Telemetry,
    pub shift_light: f64,
    pub wind: WindSwitches,
    pub tuning: &'a WindTuning,
    pub led: &'a LedConfig,
    pub native_led: Option<&'a NativeLedSettings>,
    pub led_mode: LedMode,
    pub tach_test: bool,
    pub init: InitParameters,
    pub native_init: Option<&'a NativeInitParameters>,
    /// The live configuration the Intellibox sender re-reads after each CmdMessenger call.
    pub native_reread: Option<&'a dyn Fn() -> super::intellibox::NativeHelperConfig>,
    /// The Intellibox refresh pause evaluated at the sleep, after the cycle (replaces `period`).
    pub live_period: Option<&'a dyn Fn() -> Option<Duration>>,
}

/// The belt connection and the switches it shares with the UI.
pub struct BeltSlot {
    connection: Connection<DayondBus>,
    status: PublishedStatus,
    port: String,
    port_parameter: PublishedPort,
    identity: PublishedParameter<Option<BeltIdentity>>,
    pub switches: BeltSwitches,
    state: BeltState,
    homing_status: PublishedHoming,
}

impl Default for BeltSlot {
    fn default() -> Self {
        let homing_status = PublishedHoming::default();
        Self {
            connection: Connection::default(),
            status: PublishedStatus::default(),
            port: String::new(),
            port_parameter: PublishedPort::default(),
            identity: PublishedParameter::default(),
            switches: BeltSwitches::default(),
            state: BeltState::with_homing_status(homing_status.clone()),
            homing_status,
        }
    }
}

impl BeltSlot {
    pub fn identity(&self) -> Option<BeltIdentity> {
        self.identity.get()
    }
    pub fn set_identity(&mut self, identity: Option<BeltIdentity>) {
        self.identity.set(identity);
    }

    pub fn assign_test_value(&mut self, value: LegacyValue, projection: f64) {
        self.switches.test_value = projection;
        self.state.assign_native_test_value(value, projection);
    }

    pub fn set_homing(&mut self, value: Homing) {
        self.switches.homing = value;
        self.homing_status.set(value);
    }
}

/// HaptiCR connection.
#[derive(Default)]
pub struct HapticSlot {
    connection: Connection<HaptiCr>,
    status: PublishedStatus,
    port: String,
    port_parameter: PublishedPort,
    pub model: i64,
    pub version: i64,
}

/// SIM3D connection.
#[derive(Default)]
pub struct Sim3dSlot {
    connection: Connection<Sim3d>,
    status: PublishedStatus,
    port: String,
    port_parameter: PublishedPort,
}

/// iFlag connection and drawing state.
pub struct IFlagSlot {
    connection: Connection<IFlagPanel>,
    status: PublishedStatus,
    port: String,
    port_parameter: PublishedPort,
    /// The panel's firmware version text; the published numeric value is read by iflag_version.
    firmware: PublishedParameter<Option<String>>,
    version: PublishedParameter<LegacyValue>,
    stream: IFlagStream,
}

impl IFlagSlot {
    pub fn firmware(&self) -> Option<String> {
        self.firmware.get()
    }
    pub fn set_firmware(&mut self, firmware: Option<String>) {
        self.version.set(firmware.as_deref().map_or(LegacyValue::Int(0), super::iflag::firmware_version));
        self.firmware.set(firmware);
    }
}

/// IMU connection and the motion-compensation state fed by it.
#[derive(Default)]
pub struct ImuSlot {
    connection: Connection<Imu>,
    status: PublishedStatus,
    port: String,
    port_parameter: PublishedPort,
    compensation: Compensation,
    samples: PublishedParameter<Option<ImuSamples>>,
}

impl ImuSlot {
    /// Latest sensor angles, when connected.
    pub fn angles(&self) -> Option<[f64; 3]> {
        self.connection.device().map(Imu::angles)
    }

    /// The published motion-sensor readouts.
    pub fn motion_sensor(&self) -> MotionSensor {
        *self.compensation.sensor()
    }

    /// The pose to publish for VR software.
    pub fn pose(&self) -> RigPose {
        self.compensation.pose()
    }
}

macro_rules! connection_slot {
    ($slot:ty, $device:ty) => {
        impl $slot {
            pub fn connection(&self) -> &Connection<$device> {
                &self.connection
            }
            pub fn set_connection(&mut self, next: Connection<$device>) {
                self.connection.set(next, &self.status);
            }
            pub fn port(&self) -> &str {
                &self.port
            }
            pub fn set_port(&mut self, value: String) {
                self.port_parameter.set(&value);
                self.port = value;
            }
        }
    };
}

connection_slot!(IntelliboxSlot, Intellibox);
connection_slot!(BeltSlot, DayondBus);
connection_slot!(HapticSlot, HaptiCr);
connection_slot!(Sim3dSlot, Sim3d);
connection_slot!(IFlagSlot, IFlagPanel);
impl ImuSlot {
    pub fn connection(&self) -> &Connection<Imu> {
        &self.connection
    }

    pub fn set_connection(&mut self, next: Connection<Imu>) {
        self.samples.set(next.device().map(Imu::samples));
        self.connection.set(next, &self.status);
    }

    pub fn port(&self) -> &str {
        &self.port
    }

    pub fn set_port(&mut self, value: String) {
        self.port_parameter.set(&value);
        self.port = value;
    }
}

struct ConnectionStatuses {
    intellibox: PublishedStatus,
    belt: PublishedStatus,
    hapticr: PublishedStatus,
    sim3d: PublishedStatus,
    iflag: PublishedStatus,
    imu: PublishedStatus,
}

impl ConnectionStatuses {
    fn of(&self, device: Device) -> &PublishedStatus {
        match device {
            Device::Intellibox => &self.intellibox,
            Device::Belt => &self.belt,
            Device::HaptiCr => &self.hapticr,
            Device::Sim3d => &self.sim3d,
            Device::IFlag => &self.iflag,
            Device::MotionCompensation => &self.imu,
        }
    }
}

struct ConnectionPorts {
    intellibox: PublishedPort,
    belt: PublishedPort,
    hapticr: PublishedPort,
    sim3d: PublishedPort,
    iflag: PublishedPort,
    imu: PublishedPort,
}

impl ConnectionPorts {
    fn of(&self, device: Device) -> &PublishedPort {
        match device {
            Device::Intellibox => &self.intellibox,
            Device::Belt => &self.belt,
            Device::HaptiCr => &self.hapticr,
            Device::Sim3d => &self.sim3d,
            Device::IFlag => &self.iflag,
            Device::MotionCompensation => &self.imu,
        }
    }
}

/// Every accessory, each behind its own lock so the refresh threads and the scan run
/// concurrently.
pub struct Accessories {
    statuses: ConnectionStatuses,
    ports: ConnectionPorts,
    belt_homing: PublishedHoming,
    intellibox_model: PublishedParameter<i64>,
    intellibox_version: PublishedParameter<i64>,
    belt_identity: PublishedParameter<Option<BeltIdentity>>,
    iflag_firmware: PublishedParameter<Option<String>>,
    iflag_version: PublishedParameter<LegacyValue>,
    motion_sensor_values: openrigcue_model::motion_sensor::MotionSensorValues,
    imu_samples: PublishedParameter<Option<ImuSamples>>,
    pub intellibox: Mutex<IntelliboxSlot>,
    /// Shared with the belt's receive thread, whose watchdog writes the status ([`BeltWatchdog`]).
    pub belt: Arc<Mutex<BeltSlot>>,
    pub hapticr: Mutex<HapticSlot>,
    pub sim3d: Mutex<Sim3dSlot>,
    pub iflag: Mutex<IFlagSlot>,
    pub imu: Mutex<ImuSlot>,
    /// The HUD status of the OBD2HUD thread ([`super::hud`]).
    pub hud: super::hud::HudSlot,
    /// The wind simulator, whose ejection-seat timer is shared by the Intellibox and SIM3D threads.
    wind: Mutex<WindSimulator>,
    /// The shift beep's shift-light edge state.
    beep: Mutex<ShiftBeep>,
    /// The byte order of the named LED colours (`None`: the model has no colour word).
    hex_colors: Mutex<Option<LedByteOrder>>,
}

impl Accessories {
    pub fn new(now: Instant) -> Self {
        let intellibox = IntelliboxSlot::new();
        let belt = BeltSlot::default();
        let hapticr = HapticSlot::default();
        let sim3d = Sim3dSlot::default();
        let iflag = IFlagSlot {
            connection: Connection::default(),
            status: PublishedStatus::default(),
            port: String::new(),
            port_parameter: PublishedPort::default(),
            firmware: PublishedParameter::default(),
            version: PublishedParameter::new(LegacyValue::Int(0)),
            // The iFlag animation timer starts at the wall time of `now`.
            stream: IFlagStream::new(
                super::link::local_now()
                    - chrono::TimeDelta::from_std(Instant::now().saturating_duration_since(now)).unwrap_or_default(),
            ),
        };
        let imu = ImuSlot::default();
        let statuses = ConnectionStatuses {
            intellibox: intellibox.status.clone(),
            belt: belt.status.clone(),
            hapticr: hapticr.status.clone(),
            sim3d: sim3d.status.clone(),
            iflag: iflag.status.clone(),
            imu: imu.status.clone(),
        };
        let ports = ConnectionPorts {
            intellibox: intellibox.port_parameter.clone(),
            belt: belt.port_parameter.clone(),
            hapticr: hapticr.port_parameter.clone(),
            sim3d: sim3d.port_parameter.clone(),
            iflag: iflag.port_parameter.clone(),
            imu: imu.port_parameter.clone(),
        };
        Accessories {
            statuses,
            ports,
            belt_homing: belt.homing_status.clone(),
            intellibox_model: intellibox.model.clone(),
            intellibox_version: intellibox.version.clone(),
            belt_identity: belt.identity.clone(),
            iflag_firmware: iflag.firmware.clone(),
            iflag_version: iflag.version.clone(),
            motion_sensor_values: imu.compensation.sensor_values(),
            imu_samples: imu.samples.clone(),
            intellibox: Mutex::new(intellibox),
            belt: Arc::new(Mutex::new(belt)),
            hapticr: Mutex::new(hapticr),
            sim3d: Mutex::new(sim3d),
            iflag: Mutex::new(iflag),
            imu: Mutex::new(imu),
            hud: super::hud::HudSlot::default(),
            wind: Mutex::new(WindSimulator::new()),
            beep: Mutex::new(ShiftBeep::new()),
            hex_colors: Mutex::new(LedByteOrder::from_model("RGB")),
        }
    }

    /// The shared motion parameter slot, also written by the yaw feedback producer.
    pub fn motion_sensor_values(&self) -> openrigcue_model::motion_sensor::MotionSensorValues {
        self.motion_sensor_values.clone()
    }

    /// Current raw readings without holding the connection's serial-I/O lock.
    pub fn imu_samples(&self) -> Option<ImuSamples> {
        self.imu_samples.get()
    }

    pub fn motion_sensor_value(&self, key: &str) -> Option<openrigcue_model::native_value::LegacyValue> {
        self.motion_sensor_values.get(key)
    }

    /// The current connection status of one accessory.
    pub fn connection_status(&self, device: Device) -> String {
        self.statuses.of(device).get()
    }

    /// Current held-port parameter, without taking the device's I/O lock.
    pub fn connection_port(&self, device: Device) -> String {
        self.ports.of(device).get()
    }

    pub fn belt_homing(&self) -> Homing {
        self.belt_homing.get()
    }

    /// The UI writes the shared parameter independently of serial I/O.
    pub fn set_belt_homing(&self, value: Homing) {
        self.belt_homing.set(value);
    }

    pub fn intellibox_model(&self) -> i64 {
        self.intellibox_model.get()
    }
    pub fn intellibox_version(&self) -> i64 {
        self.intellibox_version.get()
    }
    pub fn belt_identity(&self) -> Option<BeltIdentity> {
        self.belt_identity.get()
    }
    pub fn iflag_firmware(&self) -> Option<String> {
        self.iflag_firmware.get()
    }

    /// The ping's firmware version as a float, or integer zero when the text is not a number.
    pub fn iflag_version(&self) -> LegacyValue {
        self.iflag_version.get()
    }

    /// Rebuild the named LED colours for `led_model` (`LED.model`).
    ///
    /// Called at start-up (with the loaded configuration) and when the Hardware page's LED model
    /// changes; a model changed any other way keeps the previous colours until then.
    pub fn load_hex_colors(&self, led_model: &str) {
        *self.hex_colors.lock().unwrap() = LedByteOrder::from_model(led_model);
    }

    /// The byte order of the named colours as last loaded by [`Accessories::load_hex_colors`].
    pub fn hex_colors(&self) -> Option<LedByteOrder> {
        *self.hex_colors.lock().unwrap()
    }

    /// The shift beep of one shaker cycle: the shaker loop calls this after each rig pass.
    pub fn shift_beep_cycle(&self, inputs: &BeepInputs, player: &mut dyn BeepPlayer) {
        if let Some(command) = self.beep.lock().unwrap().update(inputs) {
            player.apply(command);
        }
    }

    /// Keep the motion-compensation CSV log in `log`; the daemon opens it with
    /// [`super::compensation::start_log`] when compensation starts on P6/H6 rigs.
    pub fn attach_pose_log(&self, log: PoseLog) {
        self.imu.lock().unwrap().compensation.attach_log(log);
    }

    /// The ports held by connected accessories (plus the motion platform's), for port arbitration.
    pub fn claimed_ports(&self, motion: &[String]) -> ClaimedPorts {
        ClaimedPorts {
            motion: motion.to_vec(),
            intellibox: self.connection_port(Device::Intellibox),
            belt: self.connection_port(Device::Belt),
            hapticr: self.connection_port(Device::HaptiCr),
            sim3d: self.connection_port(Device::Sim3d),
            iflag: self.connection_port(Device::IFlag),
            motion_compensation: self.connection_port(Device::MotionCompensation),
        }
    }

    /// One Intellibox cycle; returns the pause before the next one, which is `period`
    /// whether or not a box is connected (`None` ends the loop).
    ///
    /// Steps: rpm tone, fan levels (LLA bypass or
    /// the wind simulation at 10-bit PWM), and for a connected box the four LED segments and the
    /// frame; a failure is logged ("error sending data to refresh_intellibox") and the loop sleeps as usual.
    pub fn intellibox_cycle(&self, inputs: &IntelliboxInputs<'_>, period: Option<Duration>) -> Option<Duration> {
        if let Err(e) = self.intellibox_frame(inputs) {
            debug!("error sending data to refresh_intellibox: {e}");
        }
        inputs.live_period.map_or(period, |read| read())
    }

    fn intellibox_frame(&self, inputs: &IntelliboxInputs<'_>) -> Result<(), CycleError> {
        let t = inputs.telemetry;
        let rpm = super::values::channel(t, "rpm", required_channel(t, "rpm")?);
        let tone = rpm
            .py_mul(&LegacyValue::Float(2.0 / 120.0))
            .and_then(|v| v.py_integer())
            .map(LegacyValue::from_integer)
            .map_err(CycleError::Native)?;
        let mut slot = self.intellibox.lock().unwrap();
        let fans = fan_levels(&self.wind, slot.model(), INTELLIBOX_PWM_RANGE, t, &inputs.wind, inputs.tuning)?;
        let IntelliboxSlot { connection, stream, led, model, .. } = &mut *slot;
        let Some(intellibox) = connection.device_mut() else {
            return Ok(());
        };
        let led_inputs = LedInputs {
            rpm: required_channel(t, "rpm")?,
            max_rpm: required_channel(t, "max_rpm")?,
            shift_light: inputs.shift_light == 1.0,
            speed_kmh: required_channel(t, "speed_kmh")?,
            max_speed_kmh: required_channel(t, "max_speed_kmh")?,
            flag: t.flag,
            spotter_left: required_channel(t, "spotter_left")?,
            spotter_right: required_channel(t, "spotter_right")?,
            mode: inputs.led_mode,
            tach_test: inputs.tach_test,
        };
        let mut segments: [SegmentFrame; 4] = Default::default();
        for (i, s) in segments.iter_mut().enumerate() {
            *s = led.segment_frame_for_telemetry(i + 1, inputs.led, &led_inputs, t, inputs.native_led)?;
        }
        let frame = StreamFrame {
            left_fan: fans.left,
            right_fan: fans.right,
            harmonized_gear: required_channel(t, "harmonized_gear")?,
            rpm_tone: tone.py_int().unwrap_or(0),
            alert_rpm: inputs.shift_light,
            segments,
            center_fan: fans.center,
        };
        let ranges: SegmentRanges = std::array::from_fn(|i| {
            let s = &inputs.led.segments[i / 2];
            if i.is_multiple_of(2) {
                s.start
            } else {
                s.end
            }
        });
        stream.send_frame_native_channels(
            intellibox,
            model.get(),
            &frame,
            &inputs.init,
            &ranges,
            t.native_value("harmonized_gear"),
            inputs.native_led.map(|led| &led.ranges),
            inputs.native_init,
            &tone,
            inputs.native_reread,
        )?;
        Ok(())
    }

    /// One SIM3D/HaptiCR cycle; returns the pause before the next one (`None` ends the loop).
    ///
    /// A connected SIM3D gets the fan levels (three fans, 8-bit PWM)
    /// and its stream; the HaptiCR stream runs in the same loop when `APP.enable_hapticr` is on and one is
    /// connected; the pause is the SIM3D rate while the SIM3D is connected and 2 s otherwise (so a HaptiCR
    /// without SIM3D is refreshed every 2 s). The SIM3D stream receives the right fan level as its centre
    /// level and the centre level as its right one.
    pub fn sim3d_cycle(&self, inputs: &Sim3dCycleInputs<'_>, clock: &dyn Clock) -> Option<Duration> {
        let mut sim = self.sim3d.lock().unwrap();
        if sim.connection.is_connected() && !inputs.sim3d_model_set {
            // The model was set to 'None' while connected: disconnect.
            sim.set_connection(Connection::Offline(DeviceStatus::NotConnected));
            sim.set_port(String::new());
        }
        let connected = match sim.connection.device_mut() {
            Some(device) => {
                let result = fan_levels(
                    &self.wind,
                    SIM3D_FAN_COUNT,
                    SIM3D_PWM_RANGE,
                    inputs.telemetry,
                    &inputs.wind,
                    inputs.wind_tuning,
                )
                .and_then(|fans| {
                    let handed = Sim3dFans { left: fans.left, center: fans.right, right: fans.center };
                    device
                        .send_stream(
                            inputs.telemetry,
                            handed,
                            &inputs.ports,
                            &inputs.switches,
                            inputs.tuning,
                            inputs.maxima,
                            clock,
                        )
                        .map_err(CycleError::from)
                });
                if let Err(e) = result {
                    debug!("error sending data to sim3d: {e}");
                }
                true
            }
            None => false,
        };
        drop(sim);
        // After the SIM3D writes, `enable_hapticr`, the
        // telemetry, the switches and the tune are read as they are then.
        let fresh = if connected { inputs.haptic_reread.map(|read| read()) } else { None };
        let (telemetry, enabled, switches, tuning, maxima) = match &fresh {
            Some(stage) => (&stage.telemetry, stage.enabled, &stage.switches, &stage.tuning, &stage.maxima),
            None => (
                inputs.telemetry,
                inputs.hapticr_enabled,
                &inputs.haptic_switches,
                inputs.haptic_tuning,
                inputs.haptic_maxima,
            ),
        };
        let mut haptic = self.hapticr.lock().unwrap();
        if let Some(device) = haptic.connection.device_mut().filter(|_| enabled) {
            if let Err(e) = device.send_stream(telemetry, switches, tuning, maxima) {
                debug!("error sending data to hapticr: {e}");
            }
        }
        if connected {
            inputs.live_period.map_or(inputs.period, |read| read())
        } else {
            Some(IDLE_PAUSE)
        }
    }

    /// One motion-compensation pass with the connected IMU; returns the pause before the next one.
    ///
    /// See [`Compensation::cycle`]; an error ends the compensation thread.
    pub fn compensation_cycle(
        &self,
        inputs: &CompensationInputs<'_>,
        clock: &Arc<dyn Clock>,
    ) -> Result<Duration, CompensationError> {
        let mut slot = self.imu.lock().unwrap();
        let ImuSlot { connection, compensation, .. } = &mut *slot;
        compensation.cycle(connection.device(), inputs, clock)
    }

    /// One belt cycle; returns the pause before the next one (`None` ends the loop).
    pub fn belt_cycle(&self, inputs: &BeltCycleInputs<'_>) -> Option<Duration> {
        let mut slot = self.belt.lock().unwrap();
        let BeltSlot { connection, state, switches, .. } = &mut *slot;
        let Some(bus) = connection.device_mut() else {
            return Some(IDLE_PAUSE);
        };
        let frame = match required_belt_inputs(inputs.telemetry) {
            Ok(frame) => frame,
            Err(key) => {
                debug!("error sending data to belt: KeyError: '{}'", key);
                return inputs.live_period.map_or(inputs.period, |read| read());
            }
        };
        if let Err(e) = state.send_stream_for_telemetry(
            bus,
            switches,
            frame,
            inputs.tuning,
            inputs.maxima,
            inputs.calibration,
            inputs.telemetry,
            inputs.native_profile,
            inputs.native_settings,
        ) {
            debug!("error sending data to belt: {e}");
        }
        inputs.live_period.map_or(inputs.period, |read| read())
    }

    /// One iFlag cycle; returns the pause before the next one (`None` ends the loop).
    pub fn iflag_cycle(
        &self,
        settings: &IFlagSettings,
        inputs: &IFlagInputs,
        period: Option<Duration>,
        clock: &dyn Clock,
    ) -> Option<Duration> {
        self.iflag_cycle_live(settings, inputs, &|| period, clock, None)
    }

    /// [`Self::iflag_cycle`] with the configuration re-read after the panel's luma write.
    pub fn iflag_cycle_live(
        &self,
        settings: &IFlagSettings,
        inputs: &IFlagInputs,
        period: &dyn Fn() -> Option<Duration>,
        clock: &dyn Clock,
        reread: Option<&dyn Fn() -> IFlagSettings>,
    ) -> Option<Duration> {
        let mut slot = self.iflag.lock().unwrap();
        let IFlagSlot { connection, stream, .. } = &mut *slot;
        let Some(panel) = connection.device_mut() else {
            return Some(IDLE_PAUSE);
        };
        if let Err(e) = stream.draw_live(panel, settings, inputs, clock, reread) {
            debug!("error sending data to iflag: {e}");
        }
        period()
    }
}

/// Inputs of the SIM3D/HaptiCR cycle.
pub struct Sim3dCycleInputs<'a> {
    pub telemetry: &'a Telemetry,
    pub wind: WindSwitches,
    pub wind_tuning: &'a WindTuning,
    /// `SIM3D.model != 'None'`.
    pub sim3d_model_set: bool,
    /// `APP.enable_hapticr` is on (and the HaptiCR section is available).
    pub hapticr_enabled: bool,
    pub ports: [PortRole; 4],
    pub switches: Sim3dSwitches,
    pub tuning: &'a Sim3dTuning,
    pub maxima: &'a Sim3dMaxima,
    pub haptic_switches: HapticSwitches,
    pub haptic_tuning: &'a HapticTuning,
    pub haptic_maxima: &'a HapticMaxima,
    /// The HaptiCR stage as it reads its inputs after a connected SIM3D's writes.
    pub haptic_reread: Option<&'a dyn Fn() -> HapticStage>,
    /// The SIM3D refresh pause.
    pub period: Option<Duration>,
    /// The SIM3D refresh pause evaluated at the sleep, after the cycle (replaces `period`).
    pub live_period: Option<&'a dyn Fn() -> Option<Duration>>,
}

/// The HaptiCR inputs of a SIM3D/HaptiCR cycle, read live.
pub struct HapticStage {
    pub telemetry: Telemetry,
    pub enabled: bool,
    pub switches: HapticSwitches,
    pub tuning: HapticTuning,
    pub maxima: HapticMaxima,
}

/// Inputs of the belt cycle.
pub struct BeltCycleInputs<'a> {
    pub telemetry: &'a Telemetry,
    pub tuning: &'a BeltTuning,
    pub maxima: &'a BeltMaxima,
    pub calibration: &'a BeltCalibration,
    pub native_profile: Option<&'a LegacyValue>,
    pub native_settings: Option<&'a LegacyValue>,
    /// The belt refresh pause.
    pub period: Option<Duration>,
    /// The belt refresh pause evaluated at the sleep, after the cycle (replaces `period`).
    pub live_period: Option<&'a dyn Fn() -> Option<Duration>>,
}

/// Ports remembered from earlier connections.
pub trait PortMemory {
    fn recall(&self, device: Device) -> Option<String>;
    fn remember(&mut self, device: Device, port: &str);
}

/// Search settings of one device section.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceSearch {
    /// `auto_search` (and, for some devices, the section being available).
    pub enabled: bool,
    /// `com_port` ('' = any).
    pub com_port: String,
}

/// What the connection scan needs from the configuration.
#[derive(Debug, Clone)]
pub struct ScanConfig {
    pub allowed: AllowedPorts,
    pub intellibox: DeviceSearch,
    pub belt: DeviceSearch,
    pub sim3d: DeviceSearch,
    /// `SIM3D.model != 'None'`.
    pub sim3d_model_set: bool,
    /// `HAPTICR.auto_search`.
    pub hapticr: DeviceSearch,
    /// `APP.enable_hapticr` is on (and the HaptiCR section is available): the HaptiCR is searched only then.
    pub hapticr_enabled: bool,
    pub iflag: DeviceSearch,
    pub imu: DeviceSearch,
    /// `MOTION_COMPENSATION.type == 'W'`.
    pub imu_sensor_mode: bool,
}

/// The scanners' state (each device's first-scan flag, the flasher's baud rate), owned by the scan thread.
pub struct Scanners {
    pub intellibox: IntelliboxScanner,
    pub belt: BeltScanner,
    pub hapticr: HapticScanner,
    pub sim3d: Sim3dScanner,
    pub iflag: IFlagScanner,
    pub flasher: IntelliboxFlasher,
}

/// The surroundings of a scan.
pub struct ScanEnvironment<'a> {
    pub ports: &'a dyn PortLister,
    pub opener: &'a dyn PortOpener,
    pub clock: Arc<dyn Clock>,
    pub memory: &'a mut dyn PortMemory,
    pub avrdude: &'a mut dyn Avrdude,
    /// Ports of the motion platform.
    pub motion_ports: &'a [String],
    /// `MOTION_COMPENSATION.installation` as it is when the sensor configuration reads it.
    pub installation: InstallationReader,
    pub native_installation: Option<NativeInstallationReader>,
    /// Translation into the selected language (the flashing failure text).
    pub translate: &'a dyn Fn(&str) -> String,
    /// avrdude's log (`avrdude.log` in the documents folder, [`super::firmware::log_path`]).
    pub avrdude_log: std::path::PathBuf,
    /// The belt's status and held port for its receive thread's watchdog.
    pub belt_watchdog: Arc<dyn BeltWatchdog>,
}

/// Why the connection thread ends: an error of a scan step ends it.
#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    /// The port listing failed (port-loss check, or a device scan that does not handle it).
    #[error("serial ports cannot be listed: {0}")]
    Ports(#[from] std::io::Error),
    #[error(transparent)]
    Identity(super::link::PortScanError),
    /// The motion platform's step failed.
    #[error("{0}")]
    Motion(String),
    #[error(transparent)]
    NativeConfiguration(super::compensation::NativeCompensationError),
}

impl From<super::link::PortScanError> for ScanError {
    fn from(error: super::link::PortScanError) -> Self {
        match error {
            super::link::PortScanError::Listing(error) => Self::Ports(error),
            error => Self::Identity(error),
        }
    }
}

impl From<imu::ImuScanError> for ScanError {
    fn from(error: imu::ImuScanError) -> Self {
        match error {
            imu::ImuScanError::Ports(error) => Self::from(error),
            imu::ImuScanError::Native(error) => Self::NativeConfiguration(error),
        }
    }
}

/// The belt status of the slot, as the belt's receive thread sees it ([`BeltWatchdog`]).
pub struct BeltSlotWatchdog(pub std::sync::Weak<Mutex<BeltSlot>>);

impl BeltSlotWatchdog {
    /// The belt becomes "Not Connected" with no port; the bus is released, which closes its board.
    fn disconnect(slot: &mut BeltSlot) -> bool {
        if !slot.connection.is_connected() {
            return false;
        }
        slot.set_connection(Connection::Offline(DeviceStatus::NotConnected));
        slot.set_port(String::new());
        true
    }
}

impl BeltWatchdog for BeltSlotWatchdog {
    fn disconnect_if_connected(&self, stop: &AtomicBool) -> bool {
        let Some(slot) = self.0.upgrade() else { return false };
        loop {
            match slot.try_lock() {
                Ok(mut slot) => return Self::disconnect(&mut slot),
                Err(std::sync::TryLockError::Poisoned(e)) => return Self::disconnect(&mut e.into_inner()),
                Err(std::sync::TryLockError::WouldBlock) => {
                    if stop.load(Ordering::SeqCst) {
                        return false;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
    }
}

/// Intellibox baud rates tried on every scan (28800, then 9600).
const INTELLIBOX_BAUD_RATES: [u32; 2] = [28_800, 9_600];

/// Mark `connection` "Not Connected" (releasing the device) when it is connected on a port that is gone.
fn drop_if_gone<D>(connection: &mut Connection<D>, status: &PublishedStatus, port: &str, present: &[String]) {
    if connection.is_connected() && !present.iter().any(|p| p == port) {
        connection.set(Connection::Offline(DeviceStatus::NotConnected), status);
    }
}

impl Accessories {
    /// One pass of the connection thread (after its wait).
    ///
    /// Steps 3-6 of a scan pass: IMU (step 3),
    /// Intellibox at 28800 then 9600 baud, belt, SIM3D, HaptiCR (with
    /// `APP.enable_hapticr`), iFlag, each only when not connected and enabled; then every connected device
    /// whose port disappeared becomes "Not Connected" (HaptiCR, SIM3D, iFlag, IMU, belt, Intellibox).
    ///
    /// `config` reads the configuration as it is now: it is read again for every
    /// device (and for each of the two Intellibox baud rates), so a change made while an earlier device is
    /// being searched applies to the later ones.
    ///
    /// Where a device's scan does not handle a failed port listing, or the port
    /// list of the port-loss check cannot be read, the error is returned and ends the connection thread.
    pub fn scan_pass(
        &self,
        scanners: &mut Scanners,
        env: &mut ScanEnvironment<'_>,
        config: &dyn Fn() -> ScanConfig,
        rng: &mut impl rand::Rng,
    ) -> Result<(), ScanError> {
        if !self.imu.lock().unwrap().connection.is_connected() {
            let config = config();
            if config.imu.enabled && config.imu_sensor_mode {
                self.scan_imu(env, &config)?;
            }
        }
        for baud in INTELLIBOX_BAUD_RATES {
            if self.intellibox.lock().unwrap().connection.is_connected() {
                continue;
            }
            let config = config();
            if config.intellibox.enabled {
                self.scan_intellibox(scanners, env, &config, baud, rng)?;
            }
        }
        if !self.belt.lock().unwrap().connection.is_connected() {
            let config = config();
            if config.belt.enabled {
                self.scan_belt(scanners, env, &config, rng)?;
            }
        }
        if !self.sim3d.lock().unwrap().connection.is_connected() {
            let config = config();
            if config.sim3d.enabled && config.sim3d_model_set {
                self.scan_sim3d(scanners, env, &config)?;
            }
        }
        if !self.hapticr.lock().unwrap().connection.is_connected() {
            let config = config();
            if config.hapticr_enabled && config.hapticr.enabled {
                self.scan_hapticr(scanners, env, &config, rng)?;
            }
        }
        if !self.iflag.lock().unwrap().connection.is_connected() {
            let config = config();
            if config.iflag.enabled {
                self.scan_iflag(scanners, env, &config)?;
            }
        }
        self.drop_lost_ports(env.ports)?;
        Ok(())
    }

    fn scan_imu(&self, env: &mut ScanEnvironment<'_>, config: &ScanConfig) -> Result<(), ScanError> {
        let claimed = self.claimed_ports(env.motion_ports);
        let cached = env.memory.recall(Device::MotionCompensation);
        let ctx = ImuScanContext {
            ports: env.ports,
            opener: env.opener,
            clock: Arc::clone(&env.clock),
            claimed: &claimed,
            cached_port: cached.as_deref(),
            com_port: &config.imu.com_port,
            allowed: &config.allowed,
            installation: &env.installation,
        };
        let found = imu::scan_native(&ctx, env.native_installation.as_ref())?;
        let mut slot = self.imu.lock().unwrap();
        match found {
            Some(found) => {
                env.memory.remember(Device::MotionCompensation, &found.port);
                slot.set_port(found.port);
                slot.set_connection(Connection::Connected(found.imu));
            }
            None => slot.set_connection(Connection::Offline(DeviceStatus::NotConnected)),
        }
        Ok(())
    }

    fn scan_intellibox(
        &self,
        scanners: &mut Scanners,
        env: &mut ScanEnvironment<'_>,
        config: &ScanConfig,
        baud: u32,
        rng: &mut impl rand::Rng,
    ) -> Result<(), super::link::PortScanError> {
        let claimed = self.claimed_ports(env.motion_ports);
        let cached = env.memory.recall(Device::Intellibox);
        let (previous_version, previous_model) = {
            let slot = self.intellibox.lock().unwrap();
            (slot.version(), slot.model())
        };
        let ctx = ScanContext {
            ports: env.ports,
            opener: env.opener,
            clock: env.clock.as_ref(),
            claimed: &claimed,
            cached_port: cached.as_deref(),
            previous_version,
            previous_model,
        };
        let settings = ScanSettings { com_port: config.intellibox.com_port.clone(), allowed: config.allowed.clone() };
        let Scanners { intellibox, flasher, .. } = scanners;
        let avrdude = &mut *env.avrdude;
        let clock = Arc::clone(&env.clock);
        let translate = env.translate;
        let log = env.avrdude_log.clone();
        let status_slot = &self.intellibox;
        let found = intellibox.scan(&ctx, &settings, baud, rng, &mut |port| {
            flasher.flash(port, avrdude, &log, clock.as_ref(), translate, &mut |text| {
                status_slot
                    .lock()
                    .unwrap()
                    .set_connection(Connection::Offline(DeviceStatus::Progress(text.to_string())));
            });
        });
        let mut slot = self.intellibox.lock().unwrap();
        match found {
            // The Intellibox scan handled the listing error itself: nothing found, no status change.
            Err(super::link::PortScanError::Listing(_)) => {}
            Err(error) => return Err(error),
            Ok(Some(found)) => {
                env.memory.remember(Device::Intellibox, &found.port);
                slot.set_port(found.port);
                slot.set_model(found.model);
                slot.set_version(found.version);
                slot.set_connection(Connection::Connected(found.link));
            }
            Ok(None) => slot.set_connection(Connection::Offline(DeviceStatus::NotConnected)),
        }
        Ok(())
    }

    fn scan_belt(
        &self,
        scanners: &mut Scanners,
        env: &mut ScanEnvironment<'_>,
        config: &ScanConfig,
        rng: &mut impl rand::Rng,
    ) -> Result<(), super::link::PortScanError> {
        let claimed = self.claimed_ports(env.motion_ports);
        let cached = env.memory.recall(Device::Belt);
        let ctx = BeltScanContext {
            ports: env.ports,
            opener: env.opener,
            clock: Arc::clone(&env.clock),
            claimed: &claimed,
            cached_port: cached.as_deref(),
            com_port: &config.belt.com_port,
            allowed: &config.allowed,
            report: &|status| self.belt.lock().unwrap().set_connection(Connection::Offline(status)),
            watchdog: Arc::clone(&env.belt_watchdog),
        };
        let mut switches = self.belt.lock().unwrap().switches;
        let scan = scanners.belt.scan(&ctx, &mut switches, rng)?;
        if let Some(port) = &scan.remember_port {
            env.memory.remember(Device::Belt, port);
        }
        let mut slot = self.belt.lock().unwrap();
        slot.switches = switches;
        // Scanning changes mode/motor count, not the shared homing parameter. Retain UI writes
        // made while probing rather than publishing the old local copy over them.
        slot.switches.homing = self.belt_homing();
        match scan.connected {
            Some(belt) => {
                slot.set_port(belt.port);
                slot.set_identity(Some(belt.identity));
                slot.set_connection(Connection::Connected(belt.bus));
            }
            None => {
                if let Some(status) = scan.status {
                    slot.set_connection(Connection::Offline(status));
                }
            }
        }
        Ok(())
    }

    fn scan_sim3d(
        &self,
        scanners: &mut Scanners,
        env: &mut ScanEnvironment<'_>,
        config: &ScanConfig,
    ) -> Result<(), super::link::PortScanError> {
        let claimed = self.claimed_ports(env.motion_ports);
        let cached = env.memory.recall(Device::Sim3d);
        let ctx = Sim3dScanContext {
            ports: env.ports,
            opener: env.opener,
            clock: env.clock.as_ref(),
            claimed: &claimed,
            cached_port: cached.as_deref(),
            com_port: &config.sim3d.com_port,
            allowed: &config.allowed,
        };
        let found = scanners.sim3d.scan(&ctx)?;
        let mut slot = self.sim3d.lock().unwrap();
        match found {
            Some(found) => {
                env.memory.remember(Device::Sim3d, &found.port);
                slot.set_port(found.port);
                slot.set_connection(Connection::Connected(found.device));
            }
            None => slot.set_connection(Connection::Offline(DeviceStatus::NotConnected)),
        }
        Ok(())
    }

    fn scan_hapticr(
        &self,
        scanners: &mut Scanners,
        env: &mut ScanEnvironment<'_>,
        config: &ScanConfig,
        rng: &mut impl rand::Rng,
    ) -> Result<(), super::link::PortScanError> {
        let claimed = self.claimed_ports(env.motion_ports);
        let cached = env.memory.recall(Device::HaptiCr);
        let ctx = HapticScanContext {
            ports: env.ports,
            opener: env.opener,
            clock: env.clock.as_ref(),
            claimed: &claimed,
            cached_port: cached.as_deref(),
            com_port: &config.hapticr.com_port,
            allowed: &config.allowed,
            report: &|status| self.hapticr.lock().unwrap().set_connection(Connection::Offline(status)),
        };
        let scan = scanners.hapticr.scan(&ctx, rng)?;
        let mut slot = self.hapticr.lock().unwrap();
        match scan {
            HapticScan::Connected(found) => {
                env.memory.remember(Device::HaptiCr, &found.port);
                slot.set_port(found.port);
                slot.model = found.model;
                slot.version = found.version;
                slot.set_connection(Connection::Connected(found.device));
            }
            HapticScan::FirmwareTooOld | HapticScan::NotFound => {
                slot.set_connection(Connection::Offline(DeviceStatus::NotConnected))
            }
        }
        Ok(())
    }

    fn scan_iflag(
        &self,
        scanners: &mut Scanners,
        env: &mut ScanEnvironment<'_>,
        config: &ScanConfig,
    ) -> Result<(), super::link::PortScanError> {
        let claimed = self.claimed_ports(env.motion_ports);
        let cached = env.memory.recall(Device::IFlag);
        let ctx = IFlagScanContext {
            ports: env.ports,
            opener: env.opener,
            clock: Arc::clone(&env.clock),
            claimed: &claimed,
            cached_port: cached.as_deref(),
            com_port: &config.iflag.com_port,
            allowed: &config.allowed,
        };
        scanners.iflag.publish_version_to(self.iflag_version.clone());
        let found = scanners.iflag.scan(&ctx)?;
        let mut slot = self.iflag.lock().unwrap();
        match found {
            Some(found) => {
                env.memory.remember(Device::IFlag, &found.port);
                slot.set_port(found.port);
                // The ping receiver already published the numeric value; do not overwrite a newer ping.
                slot.firmware.set(Some(found.firmware));
                slot.set_connection(Connection::Connected(found.panel));
            }
            None => slot.set_connection(Connection::Offline(DeviceStatus::NotConnected)),
        }
        Ok(())
    }

    /// Mark connected devices whose port is gone as "Not Connected" (and release them).
    fn drop_lost_ports(&self, ports: &dyn PortLister) -> std::io::Result<()> {
        let present: Vec<String> = ports.ports()?.into_iter().map(|p| p.device).collect();
        let s = &mut *self.hapticr.lock().unwrap();
        drop_if_gone(&mut s.connection, &s.status, &s.port, &present);
        let s = &mut *self.sim3d.lock().unwrap();
        drop_if_gone(&mut s.connection, &s.status, &s.port, &present);
        let s = &mut *self.iflag.lock().unwrap();
        drop_if_gone(&mut s.connection, &s.status, &s.port, &present);
        let s = &mut *self.imu.lock().unwrap();
        drop_if_gone(&mut s.connection, &s.status, &s.port, &present);
        let s = &mut *self.belt.lock().unwrap();
        drop_if_gone(&mut s.connection, &s.status, &s.port, &present);
        let s = &mut *self.intellibox.lock().unwrap();
        drop_if_gone(&mut s.connection, &s.status, &s.port, &present);
        Ok(())
    }
}

/// The accessory part of a scan pass a feed calls: the scan's surroundings and the
/// configuration reader in, `Err` to end the connection thread.
pub type ScanPass<'a> = dyn FnMut(&mut ScanEnvironment<'_>, &dyn Fn() -> ScanConfig) -> Result<(), ScanError> + 'a;

/// The iFlag cycle a feed calls: settings, inputs and the refresh period in, the next pause out.
pub type IFlagCycle<'a> = dyn FnMut(
        &IFlagSettings,
        &IFlagInputs,
        &dyn Fn() -> Option<Duration>,
        Option<&dyn Fn() -> IFlagSettings>,
    ) -> Option<Duration>
    + 'a;

/// What the accessory threads read from the running application each cycle: the
/// telemetry, the runtime parameters and the configuration, which have no shared runtime type.
///
/// Each method builds the current inputs, calls the given cycle with them and returns what that cycle
/// returned: the pause before the next cycle, `None` to end that loop (a refresh rate without a
/// valid pause). A feed that cannot build the inputs returns the pause it wants instead.
pub trait AccessoryFeed: Send + Sync {
    /// Intellibox inputs and its refresh pause.
    fn intellibox(
        &self,
        cycle: &mut dyn FnMut(&IntelliboxInputs<'_>, Option<Duration>) -> Option<Duration>,
    ) -> Option<Duration>;
    fn sim3d(&self, cycle: &mut dyn FnMut(&Sim3dCycleInputs<'_>) -> Option<Duration>) -> Option<Duration>;
    fn belt(&self, cycle: &mut dyn FnMut(&BeltCycleInputs<'_>) -> Option<Duration>) -> Option<Duration>;
    /// iFlag settings, inputs and its refresh pause.
    fn iflag(&self, cycle: &mut IFlagCycle<'_>) -> Option<Duration>;
    /// Motion-compensation inputs; the cycle's error ends the compensation thread.
    fn compensation(
        &self,
        cycle: &mut dyn FnMut(&CompensationInputs<'_>) -> Result<Duration, CompensationError>,
    ) -> Result<Duration, CompensationError>;
    /// The serial event wait timeout for the current configuration (see [`serial_event_wait_timeout`]).
    fn serial_event_wait(&self) -> Option<Duration>;
    /// The motion connect alone (step 2), for a wake that only retries a network motion platform.
    fn motion_connect(&self);
    /// One scan pass after its wait: the motion connect (step 2), `pass` with the scan's
    /// surroundings and the configuration as it is when the pass reads it (steps 3-6), then the motion port
    /// check (step 7). An error ends the connection thread.
    fn scan(&self, pass: &mut ScanPass<'_>) -> Result<(), ScanError>;
}

/// How the connection thread paces its scans.
#[derive(Debug, Clone, Default)]
pub enum ScanWake {
    /// A 1 s pause before every pass (`CUSTOM_FEATURES.enable_event_driven_device_scan` off).
    #[default]
    EverySecond,
    /// Wait for a serial device event (with the feed's timeout) before every pass; the device watcher
    /// raises the trigger.
    OnSerialEvents(Arc<ScanTrigger>),
}

/// The running accessory threads.
pub struct AccessoryThreads {
    stop: Arc<AtomicBool>,
    wake: ScanWake,
    handles: Vec<JoinHandle<()>>,
}

impl AccessoryThreads {
    /// Start the refresh threads (Intellibox, SIM3D, belt, iFlag), the
    /// motion-compensation thread and the connection thread
    /// (wait as `wake` says, then one scan pass).
    ///
    /// Each loop sleeps the pause its cycle returns.
    pub fn start(
        accessories: Arc<Accessories>,
        feed: Arc<dyn AccessoryFeed>,
        mut scanners: Scanners,
        clock: Arc<dyn Clock>,
        wake: ScanWake,
    ) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let mut threads = AccessoryThreads { stop: Arc::clone(&stop), wake: wake.clone(), handles: Vec::new() };
        let context = || (Arc::clone(&accessories), Arc::clone(&feed), Arc::clone(&clock), Arc::clone(&stop));

        let (acc, feed_, clock_, stop_) = context();
        threads.spawn("refresh_intellibox", move || {
            run_periodic(&stop_, clock_.as_ref(), || {
                feed_.intellibox(&mut |inputs, period| acc.intellibox_cycle(inputs, period))
            });
        })?;
        let (acc, feed_, clock_, stop_) = context();
        threads.spawn("refresh_sim3d", move || {
            run_periodic(&stop_, clock_.as_ref(), || {
                feed_.sim3d(&mut |inputs| acc.sim3d_cycle(inputs, clock_.as_ref()))
            });
        })?;
        let (acc, feed_, clock_, stop_) = context();
        threads.spawn("refresh_belt", move || {
            run_periodic(&stop_, clock_.as_ref(), || feed_.belt(&mut |inputs| acc.belt_cycle(inputs)))
        })?;
        let (acc, feed_, clock_, stop_) = context();
        threads.spawn("refresh_iflag", move || {
            run_periodic(&stop_, clock_.as_ref(), || {
                feed_.iflag(&mut |settings, inputs, period, reread| {
                    acc.iflag_cycle_live(settings, inputs, period, clock_.as_ref(), reread)
                })
            });
        })?;
        let (acc, feed_, clock_, stop_) = context();
        threads.spawn("capture_angles", move || run_compensation(&acc, feed_.as_ref(), &clock_, &stop_))?;
        let (acc, feed_, clock_, stop_) = context();
        threads.spawn("connect_controllers", move || {
            let mut rng = rand::thread_rng();
            while !stop_.load(Ordering::SeqCst) {
                let whole_pass = wait_for_scan(&wake, feed_.as_ref(), clock_.as_ref());
                if stop_.load(Ordering::SeqCst) {
                    return;
                }
                if !whole_pass {
                    feed_.motion_connect();
                    continue;
                }
                if let Err(e) = feed_.scan(&mut |env, config| acc.scan_pass(&mut scanners, env, config, &mut rng)) {
                    debug!("connect_controllers stopped: {e}");
                    return;
                }
            }
        })?;
        Ok(threads)
    }

    fn spawn(&mut self, name: &str, body: impl FnOnce() + Send + 'static) -> std::io::Result<()> {
        self.handles.push(std::thread::Builder::new().name(name.to_string()).spawn(body)?);
        Ok(())
    }

    /// Ask every loop to end after its current cycle and wait for them.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let ScanWake::OnSerialEvents(trigger) = &self.wake {
            trigger.raise();
        }
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

/// The wait at the top of the connection loop: with the event monitor
/// a wait for the serial event (with its timeout) that then clears it, otherwise
/// a 1 s pause.
///
/// Returns whether the whole pass is due. It is not when the event wait only timed out (the wake of a
/// network motion platform): then only the motion connection is retried.
fn wait_for_scan(wake: &ScanWake, feed: &dyn AccessoryFeed, clock: &dyn Clock) -> bool {
    match wake {
        ScanWake::EverySecond => {
            clock.sleep(SCAN_PAUSE);
            true
        }
        ScanWake::OnSerialEvents(trigger) => {
            let received = trigger.wait_and_clear(feed.serial_event_wait());
            if received {
                debug!("connect_controllers: serial scan wake received");
            }
            received
        }
    }
}

/// The compensation loop: one compensation pass per pause, ended by an error.
fn run_compensation(accessories: &Accessories, feed: &dyn AccessoryFeed, clock: &Arc<dyn Clock>, stop: &AtomicBool) {
    while !stop.load(Ordering::SeqCst) {
        match feed.compensation(&mut |inputs| accessories.compensation_cycle(inputs, clock)) {
            Ok(pause) => clock.sleep(pause),
            Err(e) => {
                debug!("capture_angles stopped: {e}");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn native_imu_configuration_error_retains_scan_class_and_detail() {
        let native =
            super::super::compensation::NativeCompensationError { class: "KeyError", message: "'installation'".into() };
        let error = super::ScanError::from(super::imu::ImuScanError::Native(native.clone()));
        assert!(matches!(error, super::ScanError::NativeConfiguration(ref got) if got == &native));
        assert_eq!(error.to_string(), "'installation'");
        let listing = super::ScanError::from(super::imu::ImuScanError::Ports(
            super::super::link::PortScanError::Listing(std::io::Error::other("listing")),
        ));
        assert!(matches!(listing, super::ScanError::Ports(_)));
    }

    use super::super::compensation::{
        ActuatorPositions, AxisSettings, CompensationKind, CompensationSettings, HorizontalSurgeSettings, Rig, RigModel,
    };
    use super::super::firmware::{AvrdudeInvocation, DEFAULT_HEX_PATH};
    use super::super::imu::test_support::angle_packet;
    use super::super::led::{LedByteOrder, SegmentConfig, SegmentType};
    use super::super::link::fakes::{FakeLink, FakeOpener, FakePorts, ManualClock};
    use super::super::link::PortEntry;
    use super::*;
    use rand::SeedableRng;
    use std::collections::HashMap;
    use std::path::Path;

    /// Hz to seconds; there is no cap, so 90 Hz runs as configured; 0 and negative rates give
    /// no pause.
    #[test]
    fn live_accessory_snapshot_metadata_defaults_and_updates_are_shared() {
        let accessories = Arc::new(Accessories::new(Instant::now()));
        assert_eq!((accessories.intellibox_model(), accessories.intellibox_version()), (-1, -1));
        assert_eq!(accessories.belt_identity(), None);
        assert_eq!(accessories.iflag_firmware(), None);
        let mut intellibox = accessories.intellibox.lock().unwrap();
        let mut belt = accessories.belt.lock().unwrap();
        let mut iflag = accessories.iflag.lock().unwrap();
        intellibox.set_model(2);
        intellibox.set_version(6);
        let identity = BeltIdentity { model: 3, version: 4, serial: "test".into() };
        belt.set_identity(Some(identity.clone()));
        iflag.set_firmware(Some("1.4".into()));
        let reader = accessories.clone();
        let (send, receive) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            send.send((
                reader.intellibox_model(),
                reader.intellibox_version(),
                reader.belt_identity(),
                reader.iflag_firmware(),
                reader.iflag_version(),
            ))
            .unwrap()
        });
        let result = receive.recv_timeout(Duration::from_secs(2));
        drop((intellibox, belt, iflag));
        worker.join().unwrap();
        assert_eq!(
            result.expect("metadata reads must avoid device I/O"),
            (2, 6, Some(identity), Some("1.4".into()), LegacyValue::Float(1.4))
        );
    }

    #[test]
    fn live_motion_ports_are_readable_while_every_device_slot_is_busy() {
        use std::sync::mpsc;
        let accessories = Arc::new(Accessories::new(Instant::now()));
        let mut intellibox = accessories.intellibox.lock().unwrap();
        let mut belt = accessories.belt.lock().unwrap();
        let mut hapticr = accessories.hapticr.lock().unwrap();
        let mut sim3d = accessories.sim3d.lock().unwrap();
        let mut iflag = accessories.iflag.lock().unwrap();
        let mut imu = accessories.imu.lock().unwrap();
        intellibox.set_port("intellibox".into());
        belt.set_port("belt".into());
        hapticr.set_port("hapticr".into());
        sim3d.set_port("sim3d".into());
        iflag.set_port("iflag".into());
        imu.set_port("compensation".into());
        let reader = accessories.clone();
        let (send, receive) = mpsc::channel();
        let worker = std::thread::spawn(move || send.send(reader.claimed_ports(&["motion".into()])).unwrap());
        let result = receive.recv_timeout(Duration::from_secs(2));
        drop((intellibox, belt, hapticr, sim3d, iflag, imu));
        worker.join().unwrap();
        let ports = result.expect("reading port parameters must not wait for device I/O");
        assert_eq!(ports.motion, ["motion"]);
        assert_eq!(
            [ports.intellibox, ports.belt, ports.hapticr, ports.sim3d, ports.iflag, ports.motion_compensation],
            ["intellibox", "belt", "hapticr", "sim3d", "iflag", "compensation"]
        );
    }

    #[test]
    fn connection_status_is_readable_while_a_device_slot_is_busy() {
        use std::sync::mpsc;
        let accessories = Arc::new(Accessories::new(Instant::now()));
        let mut intellibox = accessories.intellibox.lock().unwrap();
        let mut belt = accessories.belt.lock().unwrap();
        let mut hapticr = accessories.hapticr.lock().unwrap();
        let mut sim3d = accessories.sim3d.lock().unwrap();
        let mut iflag = accessories.iflag.lock().unwrap();
        let mut imu = accessories.imu.lock().unwrap();
        intellibox.set_connection(Connection::Offline(DeviceStatus::Progress("intellibox".into())));
        belt.set_connection(Connection::Offline(DeviceStatus::Progress("belt".into())));
        hapticr.set_connection(Connection::Offline(DeviceStatus::Progress("hapticr".into())));
        sim3d.set_connection(Connection::Offline(DeviceStatus::Progress("sim3d".into())));
        iflag.set_connection(Connection::Offline(DeviceStatus::Progress("iflag".into())));
        imu.set_connection(Connection::Offline(DeviceStatus::Progress("compensation".into())));
        let reader = accessories.clone();
        let (send, receive) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            send.send(
                [
                    Device::Intellibox,
                    Device::Belt,
                    Device::HaptiCr,
                    Device::Sim3d,
                    Device::IFlag,
                    Device::MotionCompensation,
                ]
                .map(|device| reader.connection_status(device)),
            )
            .unwrap()
        });
        let result = receive.recv_timeout(Duration::from_secs(2));
        drop((intellibox, belt, hapticr, sim3d, iflag, imu));
        worker.join().unwrap();
        assert_eq!(
            result.expect("reading a parameter must not wait for device I/O"),
            ["intellibox", "belt", "hapticr", "sim3d", "iflag", "compensation"]
        );
    }

    #[test]
    fn refresh_periods() {
        assert_eq!(refresh_period(90), openrigcue_model::python_time::sleep_duration(1.0 / 90.0).ok());
        assert_eq!(refresh_period(31), openrigcue_model::python_time::sleep_duration(1.0 / 31.0).ok());
        assert_eq!(refresh_period(10), Some(Duration::from_millis(100)));
        assert_eq!(refresh_period(0), None);
        assert_eq!(refresh_period(-10), None);
        assert_eq!(rpm_tone(6000.0), Some(100));
        assert_eq!(rpm_tone(f64::NAN), None);
    }

    #[test]
    fn device_refresh_uses_native_timeout_rounding() {
        for (rate, nanos) in [(30, 33_333_334), (90, 11_111_112), (10_000_000_000_i64, 1)] {
            let nanos: u128 = if cfg!(windows) { u128::div_ceil(nanos, 100) * 100 } else { nanos };
            assert_eq!(refresh_period(rate).unwrap().as_nanos(), nanos);
        }
    }

    #[derive(Default)]
    struct Memory(HashMap<String, String>);

    impl PortMemory for Memory {
        fn recall(&self, device: Device) -> Option<String> {
            self.0.get(&format!("{device:?}")).cloned()
        }
        fn remember(&mut self, device: Device, port: &str) {
            self.0.insert(format!("{device:?}"), port.to_string());
        }
    }

    struct NoFlash;

    impl Avrdude for NoFlash {
        fn flash(&mut self, _: &AvrdudeInvocation) -> Result<String, String> {
            Ok("unexpected".into())
        }
    }

    /// `MOTION_COMPENSATION.installation = 'H'`.
    fn horizontal() -> InstallationReader {
        Arc::new(|| "H".to_owned())
    }

    /// The English UI (a text translates to itself).
    fn english(text: &str) -> String {
        text.to_owned()
    }

    fn config() -> ScanConfig {
        let on = DeviceSearch { enabled: true, com_port: String::new() };
        ScanConfig {
            allowed: AllowedPorts::default(),
            intellibox: on.clone(),
            belt: on.clone(),
            sim3d: on,
            sim3d_model_set: true,
            hapticr: DeviceSearch::default(),
            hapticr_enabled: false,
            iflag: DeviceSearch::default(),
            imu: DeviceSearch::default(),
            imu_sensor_mode: true,
        }
    }

    /// The belt status nobody watches.
    fn no_belt() -> Arc<dyn BeltWatchdog> {
        Arc::new(BeltSlotWatchdog(std::sync::Weak::new()))
    }

    fn scanners() -> Scanners {
        Scanners {
            intellibox: IntelliboxScanner::new(),
            belt: BeltScanner::new(),
            hapticr: HapticScanner::new(),
            sim3d: Sim3dScanner::new(),
            iflag: IFlagScanner::new(),
            flasher: IntelliboxFlasher::new(Path::new(DEFAULT_HEX_PATH)),
        }
    }

    fn pnp_scan_config(device: Device) -> ScanConfig {
        let mut config = config();
        config.intellibox.enabled = device == Device::Intellibox;
        config.belt.enabled = device == Device::Belt;
        config.sim3d.enabled = device == Device::Sim3d;
        config.hapticr.enabled = device == Device::HaptiCr;
        config.hapticr_enabled = device == Device::HaptiCr;
        config.iflag.enabled = device == Device::IFlag;
        config.imu.enabled = device == Device::MotionCompensation;
        config
    }

    fn pnp_candidate(device: Device) -> PortEntry {
        let (vid, pid) = match device {
            Device::Belt => (0x10c4, 0xea60),
            Device::HaptiCr => (0x1a86, 0x55d4),
            Device::IFlag => (0x2341, 0x0043),
            _ => (0x1a86, 0x7523),
        };
        PortEntry::usb("/dev/ttyUSB8", vid, pid, None, None)
    }

    #[test]
    fn native_pnp_membership_stops_each_scan_before_later_ports_and_publication() {
        use super::super::link::PortScanError;
        for device in [
            Device::Intellibox,
            Device::Belt,
            Device::Sim3d,
            Device::HaptiCr,
            Device::IFlag,
            Device::MotionCompensation,
        ] {
            let accessories = Accessories::new(Instant::now());
            let marker = DeviceStatus::Progress("previous status".into());
            match device {
                Device::Intellibox => {
                    accessories.intellibox.lock().unwrap().set_connection(Connection::Offline(marker))
                }
                Device::Belt => accessories.belt.lock().unwrap().set_connection(Connection::Offline(marker)),
                Device::Sim3d => accessories.sim3d.lock().unwrap().set_connection(Connection::Offline(marker)),
                Device::HaptiCr => accessories.hapticr.lock().unwrap().set_connection(Connection::Offline(marker)),
                Device::IFlag => accessories.iflag.lock().unwrap().set_connection(Connection::Offline(marker)),
                Device::MotionCompensation => {
                    accessories.imu.lock().unwrap().set_connection(Connection::Offline(marker))
                }
            }
            let pnp = PortEntry { device: "/dev/ttyS0".into(), description: "ttyS0".into(), hwid: None };
            let ports = FakePorts(vec![pnp, pnp_candidate(device)]);
            let opener = FakeOpener::default();
            let clock = Arc::new(ManualClock::default());
            let mut memory = Memory::default();
            if device != Device::MotionCompensation {
                memory.remember(device, "/dev/ttyS0");
            }
            let mut flash = NoFlash;
            let mut env = ScanEnvironment {
                ports: &ports,
                opener: &opener,
                clock: clock.clone(),
                memory: &mut memory,
                avrdude: &mut flash,
                motion_ports: &[],
                installation: horizontal(),
                native_installation: None,
                translate: &english,
                avrdude_log: "unused.log".into(),
                belt_watchdog: no_belt(),
            };
            let mut scans = scanners();
            let config = pnp_scan_config(device);
            let error = accessories
                .scan_pass(&mut scans, &mut env, &|| config.clone(), &mut rand::rngs::SmallRng::seed_from_u64(1))
                .unwrap_err();
            assert!(matches!(error, ScanError::Identity(PortScanError::MissingHardwareId)), "{device:?}: {error}");
            assert_eq!(error.to_string(), "TypeError: argument of type 'NoneType' is not iterable");
            assert_eq!(accessories.connection_status(device), "previous status");
            assert!(opener.opened().is_empty(), "{device:?}");
            assert_eq!(clock.elapsed(), Duration::ZERO);

            // A failure must not clear the first-scan flag: the old cached port still excludes this candidate.
            if device != Device::MotionCompensation {
                let retry = FakePorts(vec![pnp_candidate(device)]);
                env.ports = &retry;
                if device == Device::Intellibox {
                    // A scan normally makes several baud passes; observe the first one only.
                    accessories
                        .scan_intellibox(
                            &mut scans,
                            &mut env,
                            &config,
                            28800,
                            &mut rand::rngs::SmallRng::seed_from_u64(2),
                        )
                        .unwrap();
                } else {
                    accessories
                        .scan_pass(
                            &mut scans,
                            &mut env,
                            &|| config.clone(),
                            &mut rand::rngs::SmallRng::seed_from_u64(2),
                        )
                        .unwrap();
                }
                assert!(opener.opened().is_empty(), "{device:?} lost its first-scan cache gate");
                assert_eq!(env.memory.recall(device).as_deref(), Some("/dev/ttyS0"));
            }
        }
    }

    #[test]
    fn native_pnp_filtered_rows_and_literal_none_keep_ordinary_candidate_scanning() {
        for device in [
            Device::Intellibox,
            Device::Belt,
            Device::Sim3d,
            Device::HaptiCr,
            Device::IFlag,
            Device::MotionCompensation,
        ] {
            for raw_hwid in [None, Some("None".to_owned()), Some(String::new())] {
                let accessories = Accessories::new(Instant::now());
                let pnp =
                    PortEntry { device: "/dev/ttyS0".into(), description: "ttyS0".into(), hwid: raw_hwid.clone() };
                let ports = FakePorts(vec![pnp, pnp_candidate(device)]);
                let opener = FakeOpener::default();
                let mut memory = Memory::default();
                let mut flash = NoFlash;
                let mut env = ScanEnvironment {
                    ports: &ports,
                    opener: &opener,
                    clock: Arc::new(ManualClock::default()),
                    memory: &mut memory,
                    avrdude: &mut flash,
                    motion_ports: &[],
                    installation: horizontal(),
                    native_installation: None,
                    translate: &english,
                    avrdude_log: "unused.log".into(),
                    belt_watchdog: no_belt(),
                };
                let mut config = pnp_scan_config(device);
                if raw_hwid.is_none() {
                    config.allowed = AllowedPorts::parse("/dev/ttyUSB8");
                }
                accessories
                    .scan_pass(
                        &mut scanners(),
                        &mut env,
                        &|| config.clone(),
                        &mut rand::rngs::SmallRng::seed_from_u64(1),
                    )
                    .unwrap();
                assert!(!opener.opened().is_empty(), "{device:?}: {raw_hwid:?}");
                assert!(opener.opened().iter().all(|(path, _)| path == "/dev/ttyUSB8"));
            }
        }
    }

    #[test]
    fn native_pnp_intellibox_catches_only_listing_error_before_following_device() {
        struct AliasFailure;
        impl PortLister for AliasFailure {
            fn ports(&self) -> std::io::Result<Vec<PortEntry>> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "unsupported operand type(s) for +=: 'NoneType' and 'str'",
                ))
            }
        }
        let accessories = Accessories::new(Instant::now());
        accessories
            .intellibox
            .lock()
            .unwrap()
            .set_connection(Connection::Offline(DeviceStatus::Progress("retained".into())));
        let opener = FakeOpener::default();
        let mut memory = Memory::default();
        let mut flash = NoFlash;
        let mut env = ScanEnvironment {
            ports: &AliasFailure,
            opener: &opener,
            clock: Arc::new(ManualClock::default()),
            memory: &mut memory,
            avrdude: &mut flash,
            motion_ports: &[],
            installation: horizontal(),
            native_installation: None,
            translate: &english,
            avrdude_log: "unused.log".into(),
            belt_watchdog: no_belt(),
        };
        let mut config = pnp_scan_config(Device::Intellibox);
        let mut scans = scanners();
        assert!(accessories
            .scan_intellibox(&mut scans, &mut env, &config, 28800, &mut rand::rngs::SmallRng::seed_from_u64(1))
            .is_ok());
        assert_eq!(accessories.connection_status(Device::Intellibox), "retained");
        config.belt.enabled = true;
        let error = accessories
            .scan_pass(&mut scans, &mut env, &|| config.clone(), &mut rand::rngs::SmallRng::seed_from_u64(1))
            .unwrap_err();
        assert!(matches!(error, ScanError::Ports(_)));
        assert_eq!(accessories.connection_status(Device::Intellibox), "retained");
        assert!(opener.opened().is_empty());
    }

    fn led_config() -> LedConfig {
        let seg = |kind, start, end| SegmentConfig { start, end, kind, always_on: [255, 0, 0] };
        LedConfig {
            segment_count: 4,
            led_count: 150,
            brightness: 100,
            byte_order: Some(LedByteOrder::Rgb),
            always_on_order: Some(LedByteOrder::Rgb),
            segments: [
                seg(SegmentType::Rpm, 1, 50),
                seg(SegmentType::Speed, 51, 100),
                seg(SegmentType::Flag, 101, 125),
                seg(SegmentType::Off, 126, 150),
            ],
        }
    }

    #[test]
    fn native_led_failure_suppresses_intellibox_send_and_retains_prior_blink() {
        use super::super::led::{NativeLedError, NativeSegmentSettings};
        let now = Instant::now();
        let acc = Accessories::new(now);
        let link = FakeLink::new();
        {
            let mut slot = acc.intellibox.lock().unwrap();
            slot.connection = Connection::Connected(Intellibox::new(
                Box::new(link.clone()),
                super::super::intellibox::Generation::V5,
            ));
            slot.set_model(2);
        }
        let mut telemetry = Telemetry::off();
        for (name, value) in [
            ("rpm", 4000.0),
            ("max_rpm", 8000.0),
            ("speed_kmh", 75.0),
            ("max_speed_kmh", 150.0),
            ("steering_wheel_angle", 0.0),
            ("steering_wheel_max_angle", 900.0),
            ("spotter_left", 0.0),
            ("spotter_right", 0.0),
            ("harmonized_gear", 3.0),
        ] {
            telemetry.set_native(name, LegacyValue::Float(value));
        }
        let config = led_config();
        let mut native = NativeLedSettings {
            always_on: config
                .segments
                .map(|s| Ok(Some(super::super::led::color_word(s.always_on, LedByteOrder::Rgb).into()))),
            led_count: Ok(config.led_count.into()),
            ranges: super::super::led::NativeSegmentRanges::from_positions([1, 50, 51, 100, 101, 125, 126, 150]),
            segment_count: Ok(4.into()),
            segments: config.segments.map(|s| NativeSegmentSettings {
                start: Ok(s.start.into()),
                end: Ok(s.end.into()),
                kind: Ok(s.kind),
            }),
        };
        let tuning = WindTuning {
            min_percent: 0.0,
            max_percent: 100.0,
            center_min_percent: 0.0,
            center_max_percent: 100.0,
            speed_factor: 1.0,
            curving: false,
            curving_effect: 10.0,
            ejection_seat: false,
            native_speed_factor: None,
            native_curving: None,
            native_ejection: None,
            native_arguments: None,
            native_bounds: None,
        };
        let make_inputs = |native| IntelliboxInputs {
            native_init: None,
            native_reread: None,
            live_period: None,
            telemetry: &telemetry,
            shift_light: 1.0,
            wind: WindSwitches {
                left_right: FanMode::Off,
                center: FanMode::Off,
                dual_max_steering_percent: 0.0,
                ejection_requested: false,
                lla: false,
            },
            tuning: &tuning,
            led: &config,
            native_led: Some(native),
            led_mode: LedMode::Auto,
            tach_test: false,
            init: InitParameters {
                gear_shift_dim_percent: 70.0,
                led_brightness: 100,
                led_count: 150,
                fan_frequency: 3,
            },
        };
        let failure = NativeLedError { class: "KeyError", message: "'segment2_start'".into() };
        let mut failed_wind = tuning.clone();
        let wind_error = super::super::wind::NativeWindError { class: "KeyError", message: "'wind_curving'".into() };
        failed_wind.native_curving = Some(super::super::wind::NativeCurving {
            enabled: Err(wind_error.clone()),
            factor: super::super::wind::NativeSpeedFactor::Failed,
        });
        let early_native = native.clone();
        let input = IntelliboxInputs { tuning: &failed_wind, ..make_inputs(&early_native) };
        assert!(
            matches!(acc.intellibox_frame(&input), Err(CycleError::Wind(WindError::Native(error))) if error == wind_error)
        );
        assert!(link.written().is_empty(), "required curving gate fails before LEDs, initialization or stream writes");
        let ejection_error =
            super::super::wind::NativeWindError { class: "KeyError", message: "'enable_ejection_seat'".into() };
        let mut failed_ejection = tuning.clone();
        failed_ejection.native_ejection = Some(Err(ejection_error.clone()));
        let mut input = IntelliboxInputs { tuning: &failed_ejection, ..make_inputs(&early_native) };
        input.wind.ejection_requested = true;
        assert!(
            matches!(acc.intellibox_frame(&input), Err(CycleError::Wind(WindError::Native(error))) if error == ejection_error)
        );
        assert!(link.written().is_empty(), "requested ejection read fails before LEDs and stream writes");
        input.wind.lla = true;
        acc.intellibox_frame(&input).unwrap();
        assert!(!link.take_written().is_empty(), "LLA bypass skips the profile ejection gate");
        let mut missing_arguments = tuning.clone();
        let argument_error = super::super::wind::NativeWindError {
            class: "KeyError",
            message: "'power_center_wind_max_speed_perc'".into(),
        };
        missing_arguments.native_arguments = Some(Err(argument_error.clone()));
        let mut input = IntelliboxInputs { tuning: &missing_arguments, ..make_inputs(&early_native) };
        input.wind.ejection_requested = true;
        assert!(
            matches!(acc.intellibox_frame(&input), Err(CycleError::Wind(WindError::Native(error))) if error == argument_error)
        );
        assert!(link.take_written().is_empty(), "missing caller argument prevents stream output");
        input.wind.lla = true;
        acc.intellibox_frame(&input).unwrap();
        assert!(!link.take_written().is_empty(), "LLA bypass does not read ordinary fan arguments");
        let mut malformed_bounds = tuning.clone();
        malformed_bounds.native_bounds = Some(super::super::wind::NativeWindBounds {
            minimum: 0.into(),
            maximum: "100".into(),
            center_minimum: 0.into(),
            center_maximum: 100.into(),
        });
        let input = IntelliboxInputs { tuning: &malformed_bounds, ..make_inputs(&early_native) };
        assert!(matches!(acc.intellibox_frame(&input), Err(CycleError::Wind(WindError::Native(error)))
            if error.message == "can't multiply sequence by non-int of type 'float'"));
        assert!(link.take_written().is_empty(), "present malformed maximum fails before stream output");
        // Reset only state advanced by the successful bypass, then inspect the
        // failed ordinary calls' unchanged first LED phase below.
        acc.intellibox.lock().unwrap().led = LedStrip::new();
        acc.intellibox.lock().unwrap().stream = IntelliboxStream::new();
        let early_probe = LedInputs {
            rpm: 4000.0,
            max_rpm: 8000.0,
            shift_light: true,
            speed_kmh: 75.0,
            max_speed_kmh: 150.0,
            flag: 0.0,
            spotter_left: 0.0,
            spotter_right: 0.0,
            mode: LedMode::Auto,
            tach_test: false,
        };
        assert_eq!(
            acc.intellibox.lock().unwrap().led.segment_frame_native(1, &config, &early_probe, &native).unwrap().lit,
            openrigcue_model::native_value::BigInt::from(24),
            "failed wind calculation did not advance the first LED blink"
        );
        acc.intellibox.lock().unwrap().led = LedStrip::new();
        native.segments[1].start = Err(failure.clone());
        assert!(
            matches!(acc.intellibox_frame(&make_inputs(&native)), Err(CycleError::Led(LedError::Native(error))) if error == failure)
        );
        assert!(link.written().is_empty(), "segment failure precedes initialization and frame writes");
        let mut recovered = native.clone();
        recovered.segments[1].start = Ok(51.into());
        // The failed frame completed segment1, so its next RPM blink is dark.
        let mut slot = acc.intellibox.lock().unwrap();
        let probe = LedInputs {
            rpm: 4000.0,
            max_rpm: 8000.0,
            shift_light: true,
            speed_kmh: 75.0,
            max_speed_kmh: 150.0,
            flag: 0.0,
            spotter_left: 0.0,
            spotter_right: 0.0,
            mode: LedMode::Auto,
            tach_test: false,
        };
        assert_eq!(
            slot.led.segment_frame_native(1, &config, &probe, &recovered).unwrap().lit,
            openrigcue_model::native_value::BigInt::from(0)
        );
        drop(slot);
        acc.intellibox_frame(&make_inputs(&recovered)).unwrap();
        assert!(!link.written().is_empty(), "valid next frame recovers");
        link.take_written();
        acc.intellibox.lock().unwrap().stream = IntelliboxStream::new();
        let mut init = NativeInitParameters::from_parameters(&make_inputs(&recovered).init);
        init.values[0] = Ok(LegacyValue::Str("70".into()));
        init.dim_percent = Err(NativeLedError { class: "TypeError", message: "must be real number, not str".into() });
        let input = IntelliboxInputs { native_init: Some(&init), ..make_inputs(&recovered) };
        assert!(matches!(acc.intellibox_frame(&input),
            Err(CycleError::Intellibox(super::super::intellibox::IntelliboxError::NativeInitialization(error)))
            if error.class == "TypeError"));
        assert!(link.take_written().is_empty(), "native initialization fails after LED computation before writes");
        // The segment computation before that failure still toggled its blink.
        assert_eq!(
            acc.intellibox.lock().unwrap().led.segment_frame_native(1, &config, &probe, &recovered).unwrap().lit,
            openrigcue_model::native_value::BigInt::from(24)
        );
        let mut flag_failure = recovered.clone();
        flag_failure.led_count = Err(NativeLedError { class: "KeyError", message: "'nbr_leds'".into() });
        let mut changed_kind = flag_failure.clone();
        changed_kind.segments[1].kind = Ok(SegmentType::Flag);
        assert!(matches!(acc.intellibox_frame(&make_inputs(&changed_kind)),
            Err(CycleError::Led(LedError::Native(error))) if error.message == "'nbr_leds'"));
        assert!(link.take_written().is_empty(), "reached flag count failure precedes all frame writes");
        assert_eq!(
            acc.intellibox.lock().unwrap().led.segment_frame_native(1, &config, &probe, &recovered).unwrap().lit,
            openrigcue_model::native_value::BigInt::from(24),
            "earlier segment still advanced before the failed Flag segment"
        );
        let mut color_failure = recovered.clone();
        color_failure.segments[1].kind = Ok(SegmentType::SolidAlwaysOn);
        color_failure.always_on[1] =
            Err(NativeLedError { class: "IndexError", message: "list index out of range".into() });
        assert!(matches!(acc.intellibox_frame(&make_inputs(&color_failure)),
            Err(CycleError::Led(LedError::Native(error))) if error.class == "IndexError"));
        assert!(link.take_written().is_empty(), "color failure aborts before initializing or sending");
        assert_eq!(
            acc.intellibox.lock().unwrap().led.segment_frame_native(1, &config, &probe, &recovered).unwrap().lit,
            openrigcue_model::native_value::BigInt::from(24),
            "previous segment phase advanced before color failure"
        );
    }

    /// Scan: Intellibox found at 28800, remembered, streamed to; unplugged -> Not Connected.
    #[test]
    fn live_belt_homing_scan_retains_a_request_written_during_port_listing() {
        struct DuringListing<'a>(&'a Accessories);
        impl PortLister for DuringListing<'_> {
            fn ports(&self) -> std::io::Result<Vec<PortEntry>> {
                self.0.set_belt_homing(Homing::Requested);
                Ok(Vec::new())
            }
        }
        let accessories = Accessories::new(Instant::now());
        let ports = DuringListing(&accessories);
        let opener = FakeOpener::default();
        let mut memory = Memory::default();
        let mut flash = NoFlash;
        let mut environment = ScanEnvironment {
            ports: &ports,
            opener: &opener,
            clock: Arc::new(ManualClock::default()),
            memory: &mut memory,
            avrdude: &mut flash,
            motion_ports: &[],
            installation: horizontal(),
            native_installation: None,
            translate: &english,
            avrdude_log: "avrdude.log".into(),
            belt_watchdog: no_belt(),
        };
        accessories
            .scan_belt(&mut scanners(), &mut environment, &config(), &mut rand::rngs::SmallRng::seed_from_u64(5))
            .unwrap();
        assert_eq!(accessories.belt_homing(), Homing::Requested);
        assert!(opener.opened().is_empty());
    }

    #[test]
    fn scan_connects_streams_and_notices_unplug() {
        let link = FakeLink::new();
        link.queue_reply(b"1,srs;");
        link.queue_reply(b"3,\x13\x00;");
        link.queue_reply(b"10,\x02\x00;");
        let opener = FakeOpener::with("/dev/ttyUSB3", &link);
        let ports = FakePorts(vec![PortEntry::usb("/dev/ttyUSB3", 0x1a86, 0x7523, None, None)]);
        let mut memory = Memory::default();
        let mut avrdude = NoFlash;
        let now = Instant::now();
        let acc = Accessories::new(now);
        let mut sc = scanners();
        let mut env = ScanEnvironment {
            ports: &ports,
            opener: &opener,
            clock: Arc::new(ManualClock::default()),
            memory: &mut memory,
            avrdude: &mut avrdude,
            motion_ports: &[],
            installation: horizontal(),
            native_installation: None,
            translate: &english,
            avrdude_log: std::path::PathBuf::from("avrdude.log"),
            belt_watchdog: no_belt(),
        };
        let mut rng = rand::rngs::SmallRng::seed_from_u64(5);
        acc.scan_pass(&mut sc, &mut env, &config, &mut rng).unwrap();
        assert!(acc.intellibox.lock().unwrap().connection.is_connected());
        assert_eq!(acc.intellibox.lock().unwrap().model(), 2);
        assert_eq!(memory.0.get("Intellibox").map(String::as_str), Some("/dev/ttyUSB3"));
        // Only the 28800 pass ran: one port opening.
        assert_eq!(opener.opened().len(), 1);
        link.take_written();

        let mut t = Telemetry::off();
        t.speed_kmh = 75.0;
        t.max_speed_kmh = 150.0;
        let tuning = WindTuning {
            min_percent: 0.0,
            max_percent: 100.0,
            center_min_percent: 0.0,
            center_max_percent: 100.0,
            speed_factor: 1.0,
            curving: false,
            curving_effect: 10.0,
            ejection_seat: false,
            native_speed_factor: None,
            native_curving: None,
            native_ejection: None,
            native_arguments: None,
            native_bounds: None,
        };
        let inputs = IntelliboxInputs {
            native_init: None,
            native_reread: None,
            live_period: None,
            native_led: None,
            telemetry: &t,
            shift_light: 0.0,
            wind: WindSwitches {
                left_right: FanMode::Auto,
                center: FanMode::Auto,
                dual_max_steering_percent: 0.0,
                ejection_requested: false,
                lla: false,
            },
            tuning: &tuning,
            led: &led_config(),
            led_mode: LedMode::Auto,
            tach_test: false,
            init: InitParameters {
                gear_shift_dim_percent: 70.0,
                led_brightness: 100,
                led_count: 150,
                fan_frequency: 3,
            },
        };
        let period = Some(Duration::from_millis(100));
        let required = ["rpm", "speed_kmh", "max_speed_kmh", "steering_wheel_angle", "steering_wheel_max_angle"];
        for present in 0..required.len() {
            let mut partial = Telemetry::unfilled();
            for &name in &required[..present] {
                partial.set_native(name, openrigcue_model::native_value::LegacyValue::Int(1));
            }
            let probe = IntelliboxInputs { telemetry: &partial, ..inputs };
            assert_eq!(acc.intellibox_cycle(&probe, period), period);
            assert!(link.take_written().is_empty(), "missing {} must stop before serial writes", required[present]);
        }
        for key in ["speed_kmh", "max_speed_kmh"] {
            let mut invalid = Telemetry::off();
            invalid.set_native(key, openrigcue_model::native_value::LegacyValue::None);
            let probe = IntelliboxInputs {
                telemetry: &invalid,
                wind: WindSwitches { left_right: FanMode::Off, center: FanMode::Off, ..inputs.wind },
                led_mode: LedMode::Off,
                ..inputs
            };
            acc.intellibox_cycle(&probe, period);
            assert!(link.take_written().is_empty(), "None {key} must fail division even with the fans switched off");
        }
        for missing in ["max_rpm", "spotter_left", "spotter_right", "harmonized_gear"] {
            let mut partial = Telemetry::unfilled();
            for (&name, value) in Telemetry::CHANNELS.iter().zip(Telemetry::OFF_VALUES) {
                if name != missing {
                    partial.set_native(name, value.clone());
                }
            }
            let probe = IntelliboxInputs { telemetry: &partial, led_mode: LedMode::Off, tach_test: true, ..inputs };
            assert_eq!(acc.intellibox_cycle(&probe, period), period);
            assert!(
                link.take_written().is_empty(),
                "missing {missing} must stop before serial writes even for hidden LEDs"
            );
        }
        let mut lla_telemetry = Telemetry::unfilled();
        let lla_switches = WindSwitches { lla: true, ..inputs.wind };
        for name in ["speed_kmh", "steering_wheel_angle"] {
            assert!(matches!(fan_levels(&acc.wind, 2, INTELLIBOX_PWM_RANGE, &lla_telemetry, &lla_switches, &tuning),
                Err(CycleError::MissingTelemetry(key)) if key == name));
            lla_telemetry.set_native(name, openrigcue_model::native_value::LegacyValue::Int(1));
        }
        assert!(
            fan_levels(&acc.wind, 2, INTELLIBOX_PWM_RANGE, &lla_telemetry, &lla_switches, &tuning).is_ok(),
            "LLA does not require max_speed_kmh or steering_wheel_max_angle"
        );
        assert_eq!(acc.intellibox_cycle(&inputs, period), period);
        let written = link.take_written();
        assert!(written.windows(5).any(|w| w == b";4,\xff\x01"));

        let mut without_flag = Telemetry::unfilled();
        for (&name, value) in Telemetry::CHANNELS.iter().zip(Telemetry::OFF_VALUES) {
            if name != "flag" {
                without_flag.set_native(name, value.clone());
            }
        }
        assert!(
            acc.intellibox_frame(&IntelliboxInputs { telemetry: &without_flag, ..inputs }).is_ok(),
            "the original catches a missing flag and substitutes zero"
        );
        link.take_written();

        let mut memory2 = Memory::default();
        let mut avrdude2 = NoFlash;
        let empty = FakePorts(vec![]);
        let mut env2 = ScanEnvironment {
            ports: &empty,
            opener: &opener,
            clock: Arc::new(ManualClock::default()),
            memory: &mut memory2,
            avrdude: &mut avrdude2,
            motion_ports: &[],
            installation: horizontal(),
            native_installation: None,
            translate: &english,
            avrdude_log: std::path::PathBuf::from("avrdude.log"),
            belt_watchdog: no_belt(),
        };
        acc.scan_pass(&mut sc, &mut env2, &config, &mut rng).unwrap();
        assert_eq!(acc.intellibox.lock().unwrap().connection.offline_status(), Some(&DeviceStatus::NotConnected));
        // The Intellibox cycle pauses for its refresh period while disconnected too (no 2 s idle pause);
        // a rate without a valid pause ends the loop.
        assert_eq!(acc.intellibox_cycle(&inputs, period), period);
        assert_eq!(acc.intellibox_cycle(&inputs, None), None);
        assert!(link.take_written().is_empty());
    }

    /// A scan pass reads the configuration again for every device it may search (IMU, Intellibox at
    /// 28800 and again at 9600, belt, SIM3D, HaptiCR, iFlag), and not for a connected one.
    #[test]
    fn scan_reads_the_configuration_per_device() {
        let ports = FakePorts(vec![]);
        let opener = FakeOpener::default();
        let mut memory = Memory::default();
        let mut avrdude = NoFlash;
        let mut env = ScanEnvironment {
            ports: &ports,
            opener: &opener,
            clock: Arc::new(ManualClock::default()),
            memory: &mut memory,
            avrdude: &mut avrdude,
            motion_ports: &[],
            installation: horizontal(),
            native_installation: None,
            translate: &english,
            avrdude_log: std::path::PathBuf::from("avrdude.log"),
            belt_watchdog: no_belt(),
        };
        let acc = Accessories::new(Instant::now());
        let mut sc = scanners();
        let mut rng = rand::rngs::SmallRng::seed_from_u64(5);
        let reads = std::cell::Cell::new(0);
        let off = || {
            reads.set(reads.get() + 1);
            ScanConfig { intellibox: DeviceSearch::default(), belt: DeviceSearch::default(), ..config() }
        };
        acc.scan_pass(&mut sc, &mut env, &off, &mut rng).unwrap();
        assert_eq!(reads.get(), 7);
        assert!(opener.opened().is_empty());
        acc.sim3d.lock().unwrap().set_connection(Connection::Connected(Sim3d::new(super::super::sim3d::ShakeIt::new(
            Box::new(FakeLink::new()),
        ))));
        reads.set(0);
        acc.scan_pass(&mut sc, &mut env, &off, &mut rng).unwrap();
        assert_eq!(reads.get(), 6);
    }

    /// SIM3D cycle: fan levels reach the SIM3D with right and centre swapped; the HaptiCR runs in the
    /// same loop with `APP.enable_hapticr`, which pauses 2 s while the SIM3D is not connected.
    #[test]
    fn sim3d_cycle_swaps_fans_and_paces_hapticr() {
        use super::super::hapticr::EffectLevel;
        use super::super::sim3d::{BrakeEffects, SeatEffects, ShakeIt};
        let acc = Accessories::new(Instant::now());
        let sim_link = FakeLink::new();
        {
            let mut slot = acc.sim3d.lock().unwrap();
            slot.set_port("/dev/ttyUSB2".into());
            slot.set_connection(Connection::Connected(Sim3d::new(ShakeIt::new(Box::new(sim_link.clone())))));
        }
        let haptic_link = FakeLink::new();
        {
            acc.hapticr
                .lock()
                .unwrap()
                .set_connection(Connection::Connected(HaptiCr::new(Box::new(haptic_link.clone()))));
        }
        let mut t = Telemetry::off();
        t.speed_kmh = 150.0;
        t.max_speed_kmh = 150.0;
        let wind_tuning = WindTuning {
            min_percent: 0.0,
            max_percent: 100.0,
            center_min_percent: 0.0,
            center_max_percent: 20.0,
            speed_factor: 1.0,
            curving: false,
            curving_effect: 10.0,
            ejection_seat: false,
            native_speed_factor: None,
            native_curving: None,
            native_ejection: None,
            native_arguments: None,
            native_bounds: None,
        };
        let e = EffectLevel { enabled: false, level: 0 };
        let seat = SeatEffects {
            wheel_slip: e,
            rumble_strips: e,
            gear: e,
            engine: e,
            surge_acceleration: e,
            heave: e,
            traction_loss: e,
        };
        let tuning = Sim3dTuning {
            throttle: seat,
            seat,
            brake: BrakeEffects {
                wheel_lock: e,
                rumble_strips: e,
                gear: e,
                abs_lock: e,
                surge_deceleration: e,
                heave: e,
                traction_loss: e,
            },
            clutch_gear: e,
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
        };
        let haptic_tuning = HapticTuning {
            wheel_slip: e,
            abs_lock: e,
            gear: e,
            heave: e,
            rumble_strips: e,
            surge_acceleration: e,
            surge_deceleration: e,
            traction_loss: e,
            engine: e,
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
        };
        let mut inputs = Sim3dCycleInputs {
            telemetry: &t,
            wind: WindSwitches {
                left_right: FanMode::Auto,
                center: FanMode::Auto,
                dual_max_steering_percent: 0.0,
                ejection_requested: false,
                lla: false,
            },
            wind_tuning: &wind_tuning,
            sim3d_model_set: true,
            hapticr_enabled: true,
            ports: [PortRole::WindLeft, PortRole::WindRight, PortRole::WindCenter, PortRole::None],
            switches: Sim3dSwitches { test_port: 0, on: true, game_running: true },
            tuning: &tuning,
            maxima: &Sim3dMaxima { surge_acceleration: 5.0, heave: 2.0, traction_loss: 5.0 },
            haptic_switches: HapticSwitches { test_mode: 0, on: true, game_running: true },
            haptic_tuning: &haptic_tuning,
            haptic_maxima: &HapticMaxima {
                surge_acceleration: 5.0,
                surge_deceleration: 5.0,
                heave: 2.0,
                traction_loss: 5.0,
            },
            haptic_reread: None,
            period: Some(Duration::from_millis(11)),
            live_period: None,
        };
        let clock = ManualClock::default();
        assert_eq!(acc.sim3d_cycle(&inputs, &clock), Some(Duration::from_millis(11)));
        // Frame 01 01 <id> 07 03 56 53 <left> <right port> <centre port> <none> <crc>: right port shows the centre level.
        let w = sim_link.take_written();
        assert_eq!(&w[4..11], &[3, 86, 83, 255, 51, 255, 0]);
        assert!(!haptic_link.take_written().is_empty());
        inputs.sim3d_model_set = false;
        assert_eq!(acc.sim3d_cycle(&inputs, &clock), Some(IDLE_PAUSE));
        assert!(sim_link.take_written().is_empty());
        assert!(!haptic_link.take_written().is_empty());
        assert_eq!(acc.sim3d.lock().unwrap().connection.offline_status(), Some(&DeviceStatus::NotConnected));
        // `APP.enable_hapticr` off (migration v367): no HaptiCR stream.
        inputs.hapticr_enabled = false;
        assert_eq!(acc.sim3d_cycle(&inputs, &clock), Some(IDLE_PAUSE));
        assert!(haptic_link.take_written().is_empty());
    }

    fn belt_tuning() -> BeltTuning {
        BeltTuning {
            surge_enabled: true,
            surge_reverse: false,
            surge: 10.0,
            surge_smoothing: 15,
            heave_enabled: true,
            heave_factor: 10.0,
            heave_smoothing: 15,
            sway_enabled: true,
            sway_reverse: false,
            sway_factor: 10.0,
            sway_smoothing: 15,
        }
    }

    #[test]
    fn belt_cycle_checks_initial_multiplications_before_off_switch_commands() {
        use openrigcue_model::native_value::{DateTimeParts, Dict, LegacyValue};
        let acc = Accessories::new(Instant::now());
        let link = FakeLink::new();
        let bus =
            DayondBus::start(Box::new(link.clone()), Arc::new(super::super::link::SystemClock), no_belt()).unwrap();
        acc.belt.lock().unwrap().set_connection(Connection::Connected(bus));
        let tuning = belt_tuning();
        let period = Some(Duration::from_secs_f64(1.0 / 90.0));
        let cycle = |telemetry: &Telemetry| {
            acc.belt_cycle(&BeltCycleInputs {
                telemetry,
                tuning: &tuning,
                maxima: &BELT_MAXIMA,
                calibration: &BELT_CALIBRATION,
                native_profile: None,
                native_settings: None,
                period,
                live_period: None,
            })
        };
        let invalid = [
            LegacyValue::None,
            LegacyValue::Dict(Dict::new()),
            LegacyValue::Set(Vec::new()),
            LegacyValue::DateTime(DateTimeParts {
                year: 2026,
                month: 11,
                day: 11,
                hour: 0,
                minute: 0,
                second: 0,
                microsecond: 0,
                fold: false,
            }),
        ];
        for key in ["lateral_acceleration", "vertical_acceleration"] {
            for value in &invalid {
                let mut telemetry = Telemetry::off();
                telemetry.set_native(key, value.clone());
                assert_eq!(cycle(&telemetry), period);
                assert!(link.take_written().is_empty(), "{key}={value:?} fails before off-switch commands");
            }
            // Sequence repetition by -1 produces an empty sequence; it does
            // not fail before the off branch. Surge is not multiplied here.
            for value in [
                LegacyValue::Str("unused".into()),
                LegacyValue::List(Vec::new()),
                LegacyValue::Bytes(Vec::new()),
                LegacyValue::Tuple(Vec::new()),
            ] {
                let mut telemetry = Telemetry::off();
                telemetry.set_native(key, value);
                telemetry.set_native("longitudinal_acceleration", LegacyValue::None);
                assert_eq!(cycle(&telemetry), period);
                assert!(!link.take_written().is_empty());
            }
        }
    }

    #[test]
    fn belt_cycle_waits_for_all_required_telemetry_keys_before_sending_commands() {
        let acc = Accessories::new(Instant::now());
        let mut telemetry = Telemetry::unfilled();
        let tuning = belt_tuning();
        let period = Some(Duration::from_secs_f64(1.0 / 90.0));
        let cycle = |telemetry: &Telemetry| {
            acc.belt_cycle(&BeltCycleInputs {
                telemetry,
                tuning: &tuning,
                maxima: &BELT_MAXIMA,
                calibration: &BELT_CALIBRATION,
                native_profile: None,
                native_settings: None,
                period,
                live_period: None,
            })
        };
        assert_eq!(cycle(&telemetry), Some(IDLE_PAUSE));
        let link = FakeLink::new();
        let bus =
            DayondBus::start(Box::new(link.clone()), Arc::new(super::super::link::SystemClock), no_belt()).unwrap();
        acc.belt.lock().unwrap().set_connection(Connection::Connected(bus));
        for key in ["longitudinal_acceleration", "lateral_acceleration", "vertical_acceleration"] {
            assert_eq!(cycle(&telemetry), period);
            assert!(link.written().is_empty(), "no send_stream call before {key} exists");
            telemetry.set_native(key, openrigcue_model::native_value::LegacyValue::Float(0.0));
        }
        assert_eq!(cycle(&telemetry), period);
        assert!(!link.written().is_empty(), "complete telemetry reaches normal belt processing");
    }

    const BELT_MAXIMA: BeltMaxima =
        BeltMaxima { surge_acceleration: 5.0, surge_deceleration: 5.0, heave: 2.0, sway: 5.0 };
    const BELT_CALIBRATION: BeltCalibration = BeltCalibration { power: 50, retraction: 50 };

    /// Belt, iFlag and SIM3D pauses while disconnected.
    #[test]
    fn idle_pauses() {
        let acc = Accessories::new(Instant::now());
        let t = Telemetry::off();
        let tuning = belt_tuning();
        let inputs = BeltCycleInputs {
            telemetry: &t,
            tuning: &tuning,
            maxima: &BELT_MAXIMA,
            calibration: &BELT_CALIBRATION,
            native_profile: None,
            native_settings: None,
            period: None,
            live_period: None,
        };
        assert_eq!(acc.belt_cycle(&inputs), Some(IDLE_PAUSE), "the 2 s idle sleep does not use the refresh rate");
        let iflag_inputs = IFlagInputs {
            gear: 0.0,
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
        let iflag_settings = IFlagSettings {
            rotation: 0,
            brightness: 0,
            native_output: None,
            native_controls: None,
            native_flags: None,
            mirror: false,
            gear_display: true,
            rpm: true,
            sli: true,
            spotter_left: true,
            spotter_right: true,
            flags: HashMap::new(),
        };
        assert_eq!(acc.iflag_cycle(&iflag_settings, &iflag_inputs, None, &ManualClock::default()), Some(IDLE_PAUSE));
    }

    fn sensor_settings() -> CompensationSettings {
        let on = AxisSettings { enabled: true, reverse: false, offset: 0.0 };
        CompensationSettings {
            kind: CompensationKind::Sensor,
            pitch: on,
            roll: on,
            sway: on,
            surge: on,
            yaw: on,
            heave: on,
            swap_roll_pitch: false,
            pitch_factor: 100,
            roll_factor: 100,
            yaw_factor: 100,
            estimator_factor_failures: [false; 3],
            native_estimator_factors: None,
            surge_factor: 100,
            sway_factor: 100,
            height: 0.0,
            actuator_spacing_mm: 558,
            horizontal_surge: HorizontalSurgeSettings { stroke_mm: 100, reverse: false, factor_percent: 100 },
            refresh_rate: 90,
        }
    }

    const RIG: Rig = Rig { model: RigModel::Other, dof_reality: Some(false), horizontal_surge: None };

    /// Scan step 3: the IMU is searched only in 'W' mode; the connected IMU then feeds
    /// the compensation pass through the dispatcher.
    #[test]
    fn imu_scan_feeds_compensation() {
        let link = FakeLink::new();
        // The 9600 pass writes three commands; the third releases the sensor stream.
        link.queue_reply(&[]);
        link.queue_reply(&[]);
        link.queue_reply(&angle_packet(16384, -8192, 0));
        let opener = FakeOpener::with("/dev/ttyUSB0", &link);
        let ports = FakePorts(vec![PortEntry::usb("/dev/ttyUSB0", 0x1a86, 0x7523, None, None)]);
        let off = DeviceSearch::default();
        let only_imu = ScanConfig {
            intellibox: off.clone(),
            belt: off.clone(),
            sim3d: off.clone(),
            hapticr: off,
            imu: DeviceSearch { enabled: true, com_port: String::new() },
            ..config()
        };
        let mut memory = Memory::default();
        let mut avrdude = NoFlash;
        let clock: Arc<dyn Clock> = Arc::new(ManualClock::default());
        let mut env = ScanEnvironment {
            ports: &ports,
            opener: &opener,
            clock: Arc::clone(&clock),
            memory: &mut memory,
            avrdude: &mut avrdude,
            motion_ports: &[],
            installation: horizontal(),
            native_installation: None,
            translate: &english,
            avrdude_log: std::path::PathBuf::from("avrdude.log"),
            belt_watchdog: no_belt(),
        };
        let acc = Accessories::new(Instant::now());
        let mut sc = scanners();
        let mut rng = rand::rngs::SmallRng::seed_from_u64(5);
        acc.scan_pass(&mut sc, &mut env, &|| ScanConfig { imu_sensor_mode: false, ..only_imu.clone() }, &mut rng)
            .unwrap();
        assert!(opener.opened().is_empty());
        acc.scan_pass(&mut sc, &mut env, &|| only_imu.clone(), &mut rng).unwrap();
        assert!(acc.imu.lock().unwrap().connection.is_connected());
        for _ in 0..100_000 {
            if acc.imu.lock().unwrap().angles() == Some([90.0, -45.0, 0.0]) {
                break;
            }
            std::thread::yield_now();
        }
        let settings = sensor_settings();
        let positions = ActuatorPositions::default();
        let inputs = CompensationInputs {
            native_kind: None,
            settings: &settings,
            native_displacement: None,
            native_refresh_rate: None,
            native_horizontal_surge: None,
            native_axis_enabled: None,
            native_axis_controls: None,
            switch_on: true,
            rig: &RIG,
            actuators: &positions,
            heave_from_actuators: true,
            game_running: None,
            installation: &horizontal(),
            native_installation: None,
        };
        assert_eq!(acc.compensation_cycle(&inputs, &clock), Ok(Duration::from_secs_f64(1.0 / 90.0)));
        let pose = acc.imu.lock().unwrap().pose();
        assert_eq!((pose.pitch, pose.roll), (90.0, -45.0));
    }

    /// run_periodic sleeps each returned pause on the given clock and stops when asked.
    #[test]
    fn periodic_loop_sleeps_returned_pauses() {
        let clock = ManualClock::default();
        let stop = AtomicBool::new(false);
        let mut calls = 0u64;
        run_periodic(&stop, &clock, || {
            calls += 1;
            if calls == 3 {
                stop.store(true, Ordering::SeqCst);
            }
            Some(Duration::from_millis(10 * calls))
        });
        assert_eq!(calls, 3);
        assert_eq!(clock.elapsed(), Duration::from_millis(60));
        // A cycle without a valid pause ends the loop.
        let stop = AtomicBool::new(false);
        let mut calls = 0u64;
        run_periodic(&stop, &clock, || {
            calls += 1;
            (calls < 2).then_some(Duration::from_millis(5))
        });
        assert_eq!(calls, 2);
        assert_eq!(clock.elapsed(), Duration::from_millis(65));
    }

    /// The shift beep runs after each shaker rig pass.
    #[test]
    fn shift_beep_after_each_shaker_cycle() {
        use super::super::alerts::BeepCommand;
        struct Recorder(Vec<BeepCommand>);
        impl BeepPlayer for Recorder {
            fn apply(&mut self, command: BeepCommand) {
                self.0.push(command);
            }
        }
        let acc = Accessories::new(Instant::now());
        let mut player = Recorder(Vec::new());
        let on = BeepInputs { listener_running: true, shift_sound: true, shift_light: 1.0 };
        acc.shift_beep_cycle(&on, &mut player);
        acc.shift_beep_cycle(&BeepInputs { shift_light: 0.0, ..on }, &mut player);
        assert_eq!(player.0, vec![BeepCommand::StartLoop, BeepCommand::Stop]);
    }

    /// Serial event wait: 1 s for a network motion platform, else no timeout.
    #[test]
    fn serial_event_wait_timeouts() {
        assert_eq!(serial_event_wait_timeout(Some(false)), Some(Duration::from_secs(1)));
        assert_eq!(serial_event_wait_timeout(Some(true)), None);
        assert_eq!(serial_event_wait_timeout(None), None);
    }

    /// A feed that runs the belt, compensation and scan cycles with fixed inputs and counts every loop.
    struct CountingFeed {
        counts: Mutex<HashMap<&'static str, (u32, Duration)>>,
        serial_wait: Option<Duration>,
    }

    impl CountingFeed {
        fn record(&self, name: &'static str, pause: Duration) -> Duration {
            let mut counts = self.counts.lock().unwrap();
            let entry = counts.entry(name).or_insert((0, pause));
            entry.0 += 1;
            entry.1 = pause;
            pause
        }

        fn seen(&self, name: &str) -> Option<(u32, Duration)> {
            self.counts.lock().unwrap().get(name).copied()
        }
    }

    impl AccessoryFeed for CountingFeed {
        fn intellibox(
            &self,
            _: &mut dyn FnMut(&IntelliboxInputs<'_>, Option<Duration>) -> Option<Duration>,
        ) -> Option<Duration> {
            Some(self.record("intellibox", Duration::from_millis(100)))
        }
        fn sim3d(&self, _: &mut dyn FnMut(&Sim3dCycleInputs<'_>) -> Option<Duration>) -> Option<Duration> {
            Some(self.record("sim3d", IDLE_PAUSE))
        }
        fn belt(&self, cycle: &mut dyn FnMut(&BeltCycleInputs<'_>) -> Option<Duration>) -> Option<Duration> {
            let t = Telemetry::off();
            let tuning = belt_tuning();
            let inputs = BeltCycleInputs {
                telemetry: &t,
                tuning: &tuning,
                maxima: &BELT_MAXIMA,
                calibration: &BELT_CALIBRATION,
                native_profile: None,
                native_settings: None,
                period: Some(Duration::from_millis(11)),
                live_period: None,
            };
            cycle(&inputs).map(|pause| self.record("belt", pause))
        }
        fn iflag(&self, _: &mut IFlagCycle<'_>) -> Option<Duration> {
            Some(self.record("iflag", IDLE_PAUSE))
        }
        fn compensation(
            &self,
            cycle: &mut dyn FnMut(&CompensationInputs<'_>) -> Result<Duration, CompensationError>,
        ) -> Result<Duration, CompensationError> {
            let settings = sensor_settings();
            let positions = ActuatorPositions::default();
            let inputs = CompensationInputs {
                native_kind: None,
                settings: &settings,
                native_displacement: None,
                native_refresh_rate: None,
                native_horizontal_surge: None,
                native_axis_enabled: None,
                native_axis_controls: None,
                switch_on: true,
                rig: &RIG,
                actuators: &positions,
                heave_from_actuators: true,
                game_running: None,
                installation: &horizontal(),
                native_installation: None,
            };
            let pause = cycle(&inputs)?;
            Ok(self.record("compensation", pause))
        }
        fn serial_event_wait(&self) -> Option<Duration> {
            self.serial_wait
        }
        fn motion_connect(&self) {
            self.record("motion_connect", Duration::ZERO);
        }
        fn scan(&self, pass: &mut ScanPass<'_>) -> Result<(), ScanError> {
            let ports = FakePorts(vec![]);
            let opener = FakeOpener::default();
            let mut memory = Memory::default();
            let mut avrdude = NoFlash;
            let mut env = ScanEnvironment {
                ports: &ports,
                opener: &opener,
                clock: Arc::new(ManualClock::default()),
                memory: &mut memory,
                avrdude: &mut avrdude,
                motion_ports: &[],
                installation: horizontal(),
                native_installation: None,
                translate: &english,
                avrdude_log: std::path::PathBuf::from("avrdude.log"),
                belt_watchdog: no_belt(),
            };
            pass(&mut env, &config)?;
            self.record("scan", SCAN_PAUSE);
            Ok(())
        }
    }

    /// The accessory threads: every loop runs with its cycle's pause (belt 2 s while
    /// disconnected, compensation 2 s without a sensor in 'W' mode) and all of them stop on request.
    #[test]
    fn threads_run_every_loop_and_stop() {
        let feed = Arc::new(CountingFeed { counts: Mutex::new(HashMap::new()), serial_wait: None });
        let acc = Arc::new(Accessories::new(Instant::now()));
        let clock: Arc<dyn Clock> = Arc::new(ManualClock::default());
        let threads =
            AccessoryThreads::start(Arc::clone(&acc), feed.clone(), scanners(), clock, ScanWake::EverySecond).unwrap();
        let names = ["intellibox", "sim3d", "belt", "iflag", "compensation", "scan"];
        for _ in 0..1_000_000 {
            if names.iter().all(|n| feed.seen(n).is_some_and(|(count, _)| count >= 3)) {
                break;
            }
            std::thread::yield_now();
        }
        threads.stop();
        for name in names {
            assert!(feed.seen(name).is_some_and(|(count, _)| count >= 3), "{name} did not loop");
        }
        assert_eq!(feed.seen("belt").unwrap().1, IDLE_PAUSE);
        assert_eq!(feed.seen("compensation").unwrap().1, Duration::from_secs(2));
    }
    /// The connection loop with the event monitor: no pass until a serial event (no timeout with a serial
    /// motion platform), one pass per raised event, and stop() wakes the waiting thread.
    #[test]
    fn event_driven_scan_waits_for_serial_events() {
        let feed = Arc::new(CountingFeed { counts: Mutex::new(HashMap::new()), serial_wait: None });
        let acc = Arc::new(Accessories::new(Instant::now()));
        let clock: Arc<dyn Clock> = Arc::new(ManualClock::default());
        let trigger = Arc::new(ScanTrigger::new());
        let threads = AccessoryThreads::start(
            Arc::clone(&acc),
            feed.clone(),
            scanners(),
            clock,
            ScanWake::OnSerialEvents(Arc::clone(&trigger)),
        )
        .unwrap();
        for _ in 0..10_000 {
            if feed.seen("belt").is_some_and(|(count, _)| count >= 3) {
                break;
            }
            std::thread::yield_now();
        }
        assert!(feed.seen("scan").is_none(), "no device event yet");
        trigger.raise();
        for _ in 0..1_000_000 {
            if feed.seen("scan").is_some() {
                break;
            }
            std::thread::yield_now();
        }
        assert_eq!(feed.seen("scan").map(|(count, _)| count), Some(1));
        threads.stop();
    }
}
