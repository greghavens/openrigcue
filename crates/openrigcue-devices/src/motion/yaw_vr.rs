//! Yaw VR platforms (`Yaw3`, `Yaw2 PRO/STANDARD/ARCADE`, `Yaw VR PRO/STANDARD`):
//! UDP discovery, the TCP control channel (connect,
//! parameter dump, limits, homing, disconnect), the UDP angle stream (angles,
//! vibration and smart-plug values) and the feedback listener.
//!
//! The two threads that run for the whole process are
//! [`net::start_call_devices`] and [`net::start_read_response`]. The `YawVRGEFile` shared-memory block is [`export`].

pub mod export;
pub mod net;
pub mod protocol;

use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tracing::debug;

use net::{ControlChannel, ControlConnector};
use protocol::{angle_message, round_to, PyNumber, YawDevice, YawLimits, YawReport};

use crate::motion::clock::Clock;
use crate::motion::native::{self, NativeSetting};
use crate::motion::pt_actuator::MotionWarning;
use crate::motion::python;
use crate::motion::thanos::protocol::parse_python_int;
use crate::motion::thanos::rescale;
use crate::motion::PyFrame;
use crate::motion::{
    DatagramSocket, DriverFactory, LinkStatus, LiveSettings, MotionContext, MotionDriver, MotionError, SharedLink,
};
use num_traits::ToPrimitive;
use openrigcue_model::native_value::LegacyValue;

/// Positions arrive on `0..65536`.
const INPUT_SPAN: f64 = 65_536.0;
/// Decimal places the angles are rounded to.
const ANGLE_DECIMALS: usize = 4;
/// Test-mode yaw: slot 2 rescaled from 3276..62260 to -180..180, negated.
const TEST_YAW_IN: (f64, f64) = (3276.0, 62_260.0);
const TEST_YAW_OUT: (f64, f64) = (-180.0, 180.0);
/// Continuous yaw is wrapped once into `0 … 359.99`.
const YAW_WRAP: f64 = 360.0;
const YAW_WRAP_MAX: f64 = 359.99;
/// The vibration frequency is a constant.
pub const VIBRATION_HZ: i64 = 17;
/// Smart-plug PWM span (the speed rescaled from 0..maximum speed to 0..255).
const PWM_MAX: f64 = 255.0;
/// The pause after the park command of a disconnect.
const PARK_PAUSE: Duration = Duration::from_secs(5);
/// Model reported by real platforms, and the name of the Yaw emulator.
const YAW_DEVICE_MODEL: &str = "YAWDEVICE";
const EMULATOR_NAME: &str = "YAW_EMULATOR";

/// What the driver reads from the `Yaw VR`, `MOTIONBOX` and `MOTION_COMPENSATION` settings,
/// the tune, the telemetry and the service state.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct YawVrSettings {
    /// `udp_port` (feedback port announced in the hello).
    pub udp_port: i64,
    pub device_id: String,
    pub auto_park: bool,
    pub center_before_start_alert: bool,
    pub disable_two_way_communication: bool,
    /// `6dof_mode`.
    pub sixdof_mode: bool,
    /// `vibration_power` as an integer; `None` when it is not one.
    pub vibration_power: Option<i64>,
    /// `MOTIONBOX.min_degree_null_yaw_movement` as an integer.
    pub min_degree_null_yaw_movement: i64,
    /// The `MOTION_COMPENSATION` yaw offset and reverse switch.
    pub compensation_yaw_offset: f64,
    pub compensation_yaw_reverse: bool,
    /// The raw reverse switch (by truthiness) and the offset as a float; their errors are handled by the feedback loop.
    pub native_compensation: Option<NativeYawCompensation>,
    /// The tune's yaw motion switch as an integer.
    pub yaw_motion_switch: i64,
    /// The running game's key.
    pub game_running: String,
    /// The telemetry listener key: decides start/stop together with `game_running` and `test_mode`.
    pub listener_running: String,
    /// Whether the motion test mode is on.
    pub test_mode: bool,
    /// The telemetry channels `yaw_angle`, `speed`, `max_speed`, `rpm`, `max_rpm`.
    pub yaw_angle: f64,
    pub speed: f64,
    pub max_speed: f64,
    pub rpm: f64,
    pub max_rpm: f64,
    /// The raw setting values as the driver reads them; `None` keeps the typed values above.
    pub native: Option<NativeYawConfig>,
    /// Live state and telemetry for the steps that read them.
    pub runtime: crate::motion::driver_support::RuntimeReader,
}

/// `Yaw VR` and `MOTIONBOX` setting values as the driver reads them: the raw value (or the
/// error of its lookup) for each integer conversion or comparison, the boolean result for each switch.
#[derive(Debug, Clone, PartialEq)]
pub struct NativeYawConfig {
    /// `Yaw VR.udp_port`.
    pub udp_port: NativeSetting,
    /// `Yaw VR.device_id`.
    pub device_id: NativeSetting,
    /// `Yaw VR.auto_park` as a boolean.
    pub auto_park: Result<bool, String>,
    /// `Yaw VR.center_before_start_alert` as a boolean.
    pub center_before_start_alert: Result<bool, String>,
    /// `Yaw VR.vibration_power`.
    pub vibration_power: NativeSetting,
    /// `MOTIONBOX.min_degree_null_yaw_movement`.
    pub min_degree_null_yaw_movement: NativeSetting,
}

impl YawVrSettings {
    /// The settings with the state and telemetry as they are now (live when a source is
    /// installed).
    pub fn live(&self) -> YawVrSettings {
        let mut live = self.clone();
        if let Some(view) = self.runtime.read() {
            live.game_running = view.game_running;
            live.listener_running = view.listener_running;
            live.test_mode = view.motion_test_mode;
            live.yaw_angle = view.yaw_angle;
            live.speed = view.speed;
            live.max_speed = view.max_speed;
            live.rpm = view.rpm;
            live.max_rpm = view.max_rpm;
        }
        live
    }
}

impl YawVrSettings {
    /// `Yaw VR.auto_park` as a boolean, read at `frame` (`None`: a step whose line is
    /// not recorded).
    fn auto_park_at(&self, frame: Option<PyFrame>) -> Result<bool, MotionError> {
        match &self.native {
            Some(native) => switch(&self.runtime.switch("Yaw VR", "auto_park", &native.auto_park), frame),
            None => Ok(self.auto_park),
        }
    }

    /// `Yaw VR.center_before_start_alert` as a boolean, read at `frame`.
    fn center_at(&self, frame: Option<PyFrame>) -> Result<bool, MotionError> {
        match &self.native {
            Some(native) => switch(
                &self.runtime.switch("Yaw VR", "center_before_start_alert", &native.center_before_start_alert),
                frame,
            ),
            None => Ok(self.center_before_start_alert),
        }
    }

    /// Whether `text` equals `Yaw VR.device_id`, read at `frame`: the raw value is compared
    /// (a value that is not text is simply unequal).
    fn is_device_id(&self, text: &str, frame: PyFrame) -> Result<bool, MotionError> {
        match &self.native {
            Some(native) => {
                let raw = self
                    .runtime
                    .value("Yaw VR", "device_id", &native.device_id)
                    .map_err(|e| MotionError::Native(e).in_frame(frame))?;
                native::eq(&LegacyValue::Str(text.to_owned()), &raw).map_err(|e| e.in_frame(frame))
            }
            None => Ok(text == self.device_id),
        }
    }

    /// Whether two-way communication is enabled (`Yaw VR.disable_two_way_communication` is off), read now.
    pub(crate) fn two_way_now(&self) -> Result<bool, MotionError> {
        let disabled =
            self.runtime.switch("Yaw VR", "disable_two_way_communication", &Ok(self.disable_two_way_communication));
        Ok(!disabled.map_err(MotionError::Native)?)
    }

    /// `Yaw VR.6dof_mode` as a boolean, read now.
    pub(crate) fn sixdof_now(&self) -> Result<bool, MotionError> {
        self.runtime.switch("Yaw VR", "6dof_mode", &Ok(self.sixdof_mode)).map_err(MotionError::Native)
    }

    /// `Yaw VR.udp_port` as an integer of any width (for the feedback thread's bind).
    pub(crate) fn udp_port_value(&self) -> Result<num_bigint::BigInt, MotionError> {
        self.udp_port_int()
    }

    /// `Yaw VR.udp_port` as an integer of any width.
    fn udp_port_int(&self) -> Result<num_bigint::BigInt, MotionError> {
        match &self.native {
            Some(native) => native::to_bigint(
                &self.runtime.value("Yaw VR", "udp_port", &native.udp_port).map_err(MotionError::Native)?,
            ),
            None => Ok(self.udp_port.into()),
        }
    }

    /// `Yaw VR.vibration_power` as an integer.
    fn vibration_power_int(&self) -> Result<LegacyValue, MotionError> {
        match &self.native {
            Some(native) => native::to_int(
                &self
                    .runtime
                    .value("Yaw VR", "vibration_power", &native.vibration_power)
                    .map_err(MotionError::Native)?,
            ),
            None => self
                .vibration_power
                .map(native::int)
                .ok_or_else(|| MotionError::Protocol("vibration_power is not an integer".into())),
        }
    }

    /// `MOTIONBOX.min_degree_null_yaw_movement` as an integer.
    fn min_degree_int(&self) -> Result<LegacyValue, MotionError> {
        match &self.native {
            Some(native) => native::to_int(
                &self
                    .runtime
                    .value("MOTIONBOX", "min_degree_null_yaw_movement", &native.min_degree_null_yaw_movement)
                    .map_err(MotionError::Native)?,
            ),
            None => Ok(native::int(self.min_degree_null_yaw_movement)),
        }
    }

    /// The tune's yaw motion switch as an integer, from the live tune when one is installed.
    fn yaw_motion_switch_int(&self) -> Result<LegacyValue, MotionError> {
        match self.runtime.game_config() {
            Some(game_config) => {
                let switch =
                    game_config.lookup_owned(&LegacyValue::from("motion_switch")).map_err(MotionError::Native)?;
                let yaw = switch.lookup_owned(&LegacyValue::from("yaw")).map_err(MotionError::Native)?;
                native::to_int(&yaw)
            }
            None => Ok(native::int(self.yaw_motion_switch)),
        }
    }
}

/// A boolean setting read at `frame`.
fn switch(value: &Result<bool, String>, frame: Option<PyFrame>) -> Result<bool, MotionError> {
    match (value, frame) {
        (Ok(value), _) => Ok(*value),
        (Err(detail), Some(frame)) => Err(crate::motion::driver_support::boolean_setting_error(detail, frame)),
        (Err(detail), None) => Err(MotionError::Native(detail.clone())),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NativeYawCompensation {
    /// `None` records a failed lookup; other values count by their truthiness.
    pub reverse: Option<bool>,
    /// `None` records a failed lookup or a value that is not a float.
    pub offset: Option<f64>,
}

/// State shared with the UI and the background loops.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct YawShared {
    /// The discovered devices, keyed by the sender IP, in discovery order.
    pub devices: Vec<(String, YawDevice)>,
    /// The platform limits; the UI edits them and the next frame sends them.
    pub limits: Option<YawLimits>,
    /// Whether the platform is centred.
    pub centered: bool,
    /// The model the device reported.
    pub device_model: String,
    /// The motion warning a connect last wrote, in English (untranslated).
    /// The shared warning itself is the holder of [`YawVrDriver::with_motion_warning`]; this copy serves a caller
    /// that has not handed the driver that holder yet.
    pub warning: String,
    /// The same warning with its translatable parts.
    pub warning_text: WarningText,
    /// Whether homing is requested: the UI asks for centring; the driver clears it.
    pub homing_requested: bool,
    /// The control channel and the start/stop state, used by the driver and by the
    /// feedback loop, which disconnects by itself.
    pub session: YawSession,
    /// The motion status and motion ports as the feedback loop reads and clears them.
    pub link: SharedLink,
    /// Latest feedback (`YawVRGEFile` fields) and the motion-compensation values derived from it.
    pub report: Option<YawReport>,
    pub sixdof: bool,
    pub raw_yaw: f64,
    pub calculated_yaw: f64,
    /// Whether a handled feedback error wrote both shared sensor rows as integer zero.
    pub compensation_zeroed: bool,
    pub sensor_values: Option<openrigcue_model::motion_sensor::MotionSensorValues>,
}

impl YawShared {
    pub fn share_motion_sensor(&mut self, values: openrigcue_model::motion_sensor::MotionSensorValues) {
        if self.report.is_some() {
            let scalar = |number| {
                if self.compensation_zeroed {
                    openrigcue_model::native_value::LegacyValue::Int(0)
                } else {
                    openrigcue_model::native_value::LegacyValue::Float(number)
                }
            };
            values.set("calculated_yaw", scalar(self.calculated_yaw));
            values.set("raw_yaw", scalar(self.raw_yaw));
        }
        self.sensor_values = Some(values);
    }
}

/// The translation keys of the Yaw VR warnings.
pub const DEVICE_ID_WARNING: &str =
    "In the config file, set device_id to the name of the Yaw VR you want to connect to";
pub const BUSY_WARNING_USER: &str = "The Yaw VR cannot be connected: the platform is currently in use by ";
pub const BUSY_WARNING_SOURCE: &str = " and is being fed data by ";

/// The motion warning a connect writes, before translation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum WarningText {
    /// No warning (empty text).
    #[default]
    None,
    /// The device id is not set: [`DEVICE_ID_WARNING`].
    DeviceIdMissing,
    /// The platform is in use: [`BUSY_WARNING_USER`], the user, [`BUSY_WARNING_SOURCE`], the source.
    Busy { user: String, source: String },
}

impl WarningText {
    /// The warning with each translation key passed through `translate`.
    pub fn render(&self, translate: impl Fn(&str) -> String) -> String {
        match self {
            WarningText::None => String::new(),
            WarningText::DeviceIdMissing => translate(DEVICE_ID_WARNING),
            WarningText::Busy { user, source } => {
                format!("{}{user}{}{source}", translate(BUSY_WARNING_USER), translate(BUSY_WARNING_SOURCE))
            }
        }
    }
}

impl YawShared {
    fn set_warning(&mut self, text: WarningText) {
        self.warning = text.render(str::to_string);
        self.warning_text = text;
    }
}

/// The session state that the driver and the feedback thread share.
#[derive(Clone, Default)]
pub struct YawSession(Arc<Mutex<SessionState>>);

#[derive(Default)]
struct SessionState {
    /// The TCP control channel.
    control: Option<Box<dyn ControlChannel>>,
    need_start: bool,
    need_stop: bool,
    started: bool,
}

impl YawSession {
    fn with<R>(&self, f: impl FnOnce(&mut SessionState) -> R) -> R {
        f(&mut self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }

    fn set_control(&self, control: Box<dyn ControlChannel>) {
        self.with(|s| s.control = Some(control));
    }

    /// Sends `bytes` on the control channel.
    /// Returns the number of bytes sent.
    fn send(&self, bytes: &[u8]) -> Result<usize, MotionError> {
        self.with(|s| match s.control.as_mut() {
            Some(control) => control.send(bytes).map(|()| bytes.len()).map_err(|e| socket_raised(&e)),
            None => Err(MotionError::Protocol("Yaw VR control channel not open".into())),
        })
    }

    /// Receives at most `max` bytes on the control channel.
    fn recv(&self, max: usize) -> Result<Vec<u8>, MotionError> {
        self.with(|s| match s.control.as_mut() {
            Some(control) => control.recv(max).map_err(|e| socket_raised(&e)),
            None => Err(MotionError::Protocol("Yaw VR control channel not open".into())),
        })
    }

    /// Whether the platform is started.
    pub fn started(&self) -> bool {
        self.with(|s| s.started)
    }
}

impl std::fmt::Debug for YawSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.with(|s| {
            f.debug_struct("YawSession")
                .field("control", &s.control.is_some())
                .field("need_start", &s.need_start)
                .field("need_stop", &s.need_stop)
                .field("started", &s.started)
                .finish()
        })
    }
}

/// Handles compare equal: they carry no value of their own, only access to the shared one.
impl PartialEq for YawSession {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

/// The body of a disconnect once the status `'Connected'` was found: stop (or stop and park,
/// then 5 s) and exit on the control channel, an error ending them silently; the platform counts as centred
/// unless the centre-before-start alert is on, a start is needed and the platform is not started. The caller then
/// writes `'Not connected'` and empties the motion ports.
/// The driver runs it from its disconnect, the feedback loop when the platform stops answering.
pub fn stop_platform(
    settings: &YawVrSettings,
    shared: &LiveSettings<YawShared>,
    clock: &mut dyn Clock,
) -> Result<(), MotionError> {
    let session = shared.read(|s| s.session.clone());
    let stop = (|| -> Result<(), MotionError> {
        if settings.auto_park_at(None)? {
            debug!("parking");
            session.send(&protocol::STOP_AND_PARK)?;
            clock.sleep(PARK_PAUSE);
        } else {
            debug!("stopping");
            session.send(&protocol::STOP)?;
        }
        debug!("exit");
        session.send(&protocol::EXIT).map(drop)
    })();
    // An error of the three commands only ends them; nothing is logged.
    let _ = stop;
    // The centred flag follows the commands: an error of its setting read ends the disconnect.
    let centered = !settings.center_at(None)?;
    shared.update(|s| s.centered = centered);
    session.with(|s| {
        s.need_start = true;
        s.started = false;
    });
    Ok(())
}

/// The UDP socket that carries the angle messages.
const UDP_SENDER: DatagramSocket = DatagramSocket("yawvr-udp_sender");

/// A float channel as an integer (truncation).
fn int_of(x: f64) -> i64 {
    x.trunc() as i64
}

/// The vibration amplitude: rpm-based when the integer maximum rpm is above 0, else speed-based, scaled to
/// 0..the integer vibration power.
pub fn vibration_amplitude(s: &YawVrSettings) -> Result<i64, MotionError> {
    let (value, max) = if int_of(s.max_rpm) > 0 { (s.rpm, s.max_rpm) } else { (s.speed, s.max_speed) };
    // The integer value rescaled from 0..the integer maximum to 0..the vibration power: the power at any
    // width, converted to a 64-bit float by the rescale with the other arguments.
    let power = s.vibration_power_int()?;
    let mapped = native::map([
        &native::int(int_of(value)),
        &native::int(0),
        &native::int(int_of(max)),
        &native::int(0),
        &power,
    ])?;
    Ok(mapped.trunc() as i64)
}

/// The smart-plug PWM: the speed rescaled from 0..maximum speed to 0..255, truncated.
pub fn smartplug_pwm(s: &YawVrSettings) -> i64 {
    rescale(s.speed, 0.0, s.max_speed, 0.0, PWM_MAX).trunc() as i64
}

/// The angle of a dynamic slot: `limit` minus the slot rescaled from 0..65536 to 0..twice the limit, rounded
/// to four decimals; the rescale takes the slot as a 64-bit float.
fn limited_angle_native(slot: &LegacyValue, limit: i64) -> Result<f64, MotionError> {
    let x = native::double(slot).map_err(|e| e.in_frame(native::FUNCTIONS_MAP))?;
    let limit = limit as f64;
    Ok(round_to(limit - rescale(x, 0.0, INPUT_SPAN, 0.0, limit * 2.0), ANGLE_DECIMALS))
}

/// A translation function: the text in the UI language.
pub type Translate = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// The Yaw VR driver.
pub struct YawVrDriver {
    settings: LiveSettings<YawVrSettings>,
    shared: LiveSettings<YawShared>,
    connector: ControlConnector,
    /// The shared motion warning every driver writes, and the translation that a connect
    /// passes the warning's fixed parts through when it writes it.
    motion_warning: Option<(MotionWarning, Translate)>,
    previous_limits: i64,
    previous_yaw: f64,
    previous_calc_yaw: Option<PyNumber>,
}

impl YawVrDriver {
    pub fn new(settings: LiveSettings<YawVrSettings>, shared: LiveSettings<YawShared>) -> Self {
        Self::with_connector(settings, shared, net::tcp_connector())
    }

    /// A driver whose control channel comes from `connector` (tests use a fake).
    pub fn with_connector(
        settings: LiveSettings<YawVrSettings>,
        shared: LiveSettings<YawShared>,
        connector: ControlConnector,
    ) -> Self {
        YawVrDriver {
            settings,
            shared,
            connector,
            motion_warning: None,
            previous_limits: 0,
            previous_yaw: 0.0,
            previous_calc_yaw: None,
        }
    }

    /// The driver writing the motion warning into `warning` (the holder the PT and VNM drivers write
    /// too), translated by `translate` at the moment a connect writes it.
    pub fn with_motion_warning(mut self, warning: MotionWarning, translate: Translate) -> Self {
        self.motion_warning = Some((warning, translate));
        self
    }

    pub fn factory(settings: LiveSettings<YawVrSettings>, shared: LiveSettings<YawShared>) -> DriverFactory {
        Box::new(move || Box::new(YawVrDriver::new(settings.clone(), shared.clone())))
    }

    /// [`Self::factory`] for drivers that write the shared motion warning
    /// ([`Self::with_motion_warning`]).
    pub fn factory_with_motion_warning(
        settings: LiveSettings<YawVrSettings>,
        shared: LiveSettings<YawShared>,
        warning: MotionWarning,
        translate: Translate,
    ) -> DriverFactory {
        Box::new(move || {
            Box::new(
                YawVrDriver::new(settings.clone(), shared.clone())
                    .with_motion_warning(warning.clone(), Arc::clone(&translate)),
            )
        })
    }

    /// Writes the motion warning of a connect: the shared holder gets the text with its fixed parts
    /// translated now; [`YawShared::warning`] / [`YawShared::warning_text`] keep the English text and its parts.
    fn publish_warning(&self, text: WarningText) {
        if let Some((warning, translate)) = &self.motion_warning {
            warning.set(text.render(|key| translate(key)));
        }
        self.shared.update(|s| s.set_warning(text));
    }

    fn session(&self) -> YawSession {
        self.shared.read(|s| s.session.clone())
    }

    fn send_control(&mut self, bytes: &[u8]) -> Result<usize, MotionError> {
        self.session().send(bytes)
    }

    fn recv_control(&mut self, max: usize) -> Result<Vec<u8>, MotionError> {
        self.session().recv(max)
    }

    /// Requests the parameter dump: logs "sending get config", then the send and
    /// its logged byte count (an error is logged as "error" with its text), then the receive and the unpack
    /// (an error is logged as "error while unpacking" with its text, the previous limits kept).
    fn read_parameters(&mut self) -> Result<(), MotionError> {
        debug!("sending get config");
        match self.send_control(&protocol::GET_PARAMETERS) {
            Ok(sent) => debug!("{sent}"),
            Err(e) => debug!("error {}", e.python_str()),
        }
        let unpacked = self.recv_control(protocol::PARAMETERS_RECV_SIZE).map_err(|e| e.python_str()).and_then(|data| {
            YawLimits::unpack(&data).ok_or_else(|| "unpack requires a buffer of 32 bytes".to_string())
        });
        match unpacked {
            Ok(limits) => {
                self.previous_limits = limits.sum();
                self.shared.update(|s| s.limits = Some(limits));
            }
            // Without the parameters the connection attempt fails; the caller retries it.
            Err(e) => {
                debug!("error while unpacking {e}");
                return Err(MotionError::Native(
                    "UnboundLocalError: local variable 'power' referenced before assignment".into(),
                )
                .in_frame(at("get_all_parameters", 172))
                .in_frame(at("connect", 120)));
            }
        }
        Ok(())
    }

    /// Sets the limits: logs "setting limits to", then the five commands,
    /// each followed by one receive and a "<name> response is" log line; the first error
    /// (a failed send or receive, or a value that does not fit its field) ends them and is logged as
    /// "set limits error" with its text.
    fn write_limits(&mut self, limits: &YawLimits) {
        debug!("setting limits to");
        let result = limits.limit_commands().into_iter().try_for_each(|(name, command)| {
            let command = command.map_err(str::to_string)?;
            self.send_control(&command).map_err(|e| e.python_str())?;
            let reply = self.recv_control(protocol::RECV_SIZE).map_err(|e| e.python_str())?;
            debug!("{name}{}", python::bytes_repr(&reply));
            Ok::<(), String>(())
        });
        if let Err(e) = result {
            debug!("set limits error {e}");
        }
    }

    /// Homing: centre and mark the platform centred; a failed send logs `'not connected'`.
    fn center(&mut self) {
        debug!("centering");
        match self.send_control(&protocol::CENTER) {
            Ok(_) => self.shared.update(|s| s.centered = true),
            Err(_) => debug!("not connected"),
        }
    }

    /// The start/stop logic of a motion frame: the state is `listener_running`, forced to `'none'`
    /// when `game_running` is empty and to `'test'` in motion test mode; start (`A1`) while that state is not
    /// `'none'`, stop (`A2`, or `A2 01` with `auto_park`) otherwise. Returns `started` as it is afterwards.
    fn update_run_state(&mut self, settings: &YawVrSettings) -> Result<bool, MotionError> {
        let settings = &settings.live();
        let running = settings.test_mode || !(settings.game_running.is_empty() || settings.listener_running == "none");
        let session = self.session();
        let (started, need_start, need_stop) = session.with(|s| {
            s.need_start = running;
            s.need_stop = !running;
            (s.started, s.need_start, s.need_stop)
        });
        let mut started = started;
        let sent_at = |line: u32| socket_error(at("send_yawvr", line));
        if started && need_stop {
            if settings.auto_park_at(Some(at("send_yawvr", 326)))? {
                debug!("stopping and parking");
                self.send_control(&protocol::STOP_AND_PARK).map_err(sent_at(327))?;
            } else {
                self.send_control(&protocol::STOP).map_err(sent_at(330))?;
                debug!("stopping");
            }
            started = false;
            session.with(|s| s.started = false);
        }
        if !started && need_start {
            debug!("starting");
            self.send_control(&protocol::START).map_err(sent_at(338))?;
            started = true;
            session.with(|s| s.started = true);
        }
        Ok(started)
    }

    /// The continuous-yaw integration of a motion frame: jumps larger than
    /// `min_degree_null_yaw_movement` between frames are ignored, smaller changes accumulate.
    fn integrate_yaw(&mut self, settings: &YawVrSettings) -> Result<PyNumber, MotionError> {
        let settings = &settings.live();
        // The yaw is the telemetry yaw angle times the tune's integer yaw motion switch.
        let line352 = at("send_yawvr", 352);
        let switch = settings.yaw_motion_switch_int().map_err(|e| e.in_frame(line352))?;
        let product = native::mul(&native::float(settings.yaw_angle), &switch).map_err(|e| e.in_frame(line352))?;
        let yaw = native::double(&product).map_err(|e| e.in_frame(line352))?;
        let delta = yaw - self.previous_yaw;
        // The delta is compared with the threshold and, only when it is not above it, with the negated
        // threshold; each comparison reads the setting.
        let line356 = at("send_yawvr", 356);
        let threshold = settings.min_degree_int().map_err(|e| e.in_frame(line356))?;
        let jump = native::gt(&native::float(delta), &threshold).map_err(|e| e.in_frame(line356))? || {
            let threshold = settings.min_degree_int().map_err(|e| e.in_frame(line356))?;
            let negative = native::neg(&threshold).map_err(|e| e.in_frame(line356))?;
            native::lt(&native::float(delta), &negative).map_err(|e| e.in_frame(line356))?
        };
        let calc = match (self.previous_calc_yaw, jump) {
            (None, _) => PyNumber::Int(0),
            (Some(previous), true) => previous,
            (Some(previous), false) => PyNumber::Float(delta + previous.value()),
        };
        self.previous_yaw = yaw;
        self.previous_calc_yaw = Some(calc);
        Ok(calc)
    }

    /// A motion frame: limits, start/stop, then the angle message.
    fn send_yawvr(&mut self, motors: &LegacyValue, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        let settings = self.settings.get();
        // The power is the first of the limits a parameter dump stores;
        // they are missing until a parameter dump was unpacked.
        let limits = self
            .shared
            .get()
            .limits
            .ok_or_else(|| MotionError::Native("KeyError: 'power'".into()).in_frame(at("send_yawvr", 273)))?;
        if limits.sum() != self.previous_limits {
            self.write_limits(&limits);
            self.previous_limits = limits.sum();
        }
        if self.shared.get().homing_requested {
            self.center();
            self.shared.update(|s| s.homing_requested = false);
        }
        if !self.shared.get().centered {
            return Ok(());
        }
        if !self.update_run_state(&settings)? {
            return Ok(());
        }
        // Slots 0 and 1 and, where the yaw comes from the list, slot 2
        // (with the yaw limit in test mode).
        // Each angle reads its slot, which the rescale then takes as a 64-bit float (any integer that fits).
        let angle = |index: usize, line: u32, limit: i64| {
            let frame = at("send_yawvr", line);
            let slot = native::item(motors, index).map_err(|e| e.in_frame(frame))?;
            limited_angle_native(&slot, limit).map_err(|e| e.in_frame(frame))
        };
        let pitch = angle(0, 343, limits.pitch_limit())?;
        let roll = angle(1, 344, limits.roll)?;
        let yaw = if limits.yaw_limit_enabled == 1 {
            PyNumber::Float(angle(2, 347, limits.yaw)?)
        } else if settings.live().test_mode {
            let frame = at("send_yawvr", 350);
            let slot = native::item(motors, 2).map_err(|e| e.in_frame(frame))?;
            let slot = native::double(&slot).map_err(|e| e.in_frames([frame, native::FUNCTIONS_MAP]))?;
            PyNumber::Float(-rescale(slot, TEST_YAW_IN.0, TEST_YAW_IN.1, TEST_YAW_OUT.0, TEST_YAW_OUT.1))
        } else {
            wrap_yaw(self.integrate_yaw(&settings)?)
        };
        let message = angle_message(
            yaw,
            pitch,
            roll,
            vibration_amplitude(&settings.live())
                .map_err(|e| e.in_frames([at("send_yawvr", 371), at("get_amplitude", 238)]))?,
            VIBRATION_HZ,
            smartplug_pwm(&settings.live()),
        );
        self.send_to_device(&settings, &message, ctx)
    }

    /// Every discovered device receives the frame when it is the only one or its address equals `device_id`.
    fn send_to_device(
        &self,
        settings: &YawVrSettings,
        message: &str,
        ctx: &mut MotionContext<'_>,
    ) -> Result<(), MotionError> {
        let devices = self.shared.get().devices;
        for (ip, _) in &devices {
            if devices.len() >= 2 && !settings.is_device_id(ip, at("send_yawvr", 377))? {
                continue;
            }
            let sent = resolve(ip, protocol::DEVICE_UDP_PORT).and_then(|addr| {
                ctx.host.send_datagram_numbered(UDP_SENDER, 0, addr, message.as_bytes()).map_err(|e| e.to_string())
            });
            // A failed send is ignored.
            let _ = sent;
        }
        Ok(())
    }

    /// Disconnects the platform.
    fn disconnect_platform(&mut self, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        if !ctx.link.is_connected() {
            return Ok(());
        }
        stop_platform(&self.settings.get(), &self.shared, ctx.host.clock())?;
        ctx.link.status = LinkStatus::NotConnected;
        ctx.link.ports.clear();
        Ok(())
    }
}

/// Continuous yaw sent once wrapped: `360 + x` below 0, `x - 360` above 359.99.
fn wrap_yaw(calc: PyNumber) -> PyNumber {
    let v = calc.value();
    if v < 0.0 {
        match calc {
            PyNumber::Int(i) => PyNumber::Int(360 + i),
            PyNumber::Float(f) => PyNumber::Float(YAW_WRAP + f),
        }
    } else if v > YAW_WRAP_MAX {
        match calc {
            PyNumber::Int(i) => PyNumber::Int(i - 360),
            PyNumber::Float(f) => PyNumber::Float(f - YAW_WRAP),
        }
    } else {
        calc
    }
}

/// The file name in the frames an error of this driver passes.
const YAWVR_FILE: &str = "backend/controllers/yawvr.py";

/// The frame for `function` and `line` in this driver's file.
const fn at(function: &'static str, line: u32) -> PyFrame {
    PyFrame::new(YAWVR_FILE, function, line)
}

/// The driver error for the control channel error `e`: a socket error's class and text (`TimeoutError: timed
/// out`, `ConnectionResetError: [Errno 104] …`), not a serial port's.
fn socket_raised(e: &std::io::Error) -> MotionError {
    MotionError::Native(python::socket_exception(e))
}

/// The error of a control channel call at `frame`: the socket error class for
/// an I/O error (`TimeoutError`, `ConnectionResetError`, …; no deeper frame).
fn socket_error(frame: PyFrame) -> impl FnOnce(MotionError) -> MotionError {
    move |error| match error {
        raised @ MotionError::Native(_) => raised.in_frame(frame),
        other => other,
    }
}

fn resolve(host: &str, port: u16) -> Result<SocketAddr, String> {
    (host, port).to_socket_addrs().map_err(|e| e.to_string())?.next().ok_or_else(|| format!("no address for {host}"))
}

impl MotionDriver for YawVrDriver {
    /// Connects: only the first discovered device is considered; a lone device is always used,
    /// with two or more it must match `device_id` by name or address. It must accept the hello with
    /// `AVAILABLE` and report its limits.
    fn connect(&mut self, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        // A new UDP sending socket on every connect.
        ctx.host.close_datagram_set(UDP_SENDER);
        let settings = self.settings.get();
        let devices = self.shared.get().devices;
        let Some((ip, device)) = devices.first() else {
            ctx.link.status = LinkStatus::NotConnected;
            ctx.link.ports.clear();
            return Ok(());
        };
        // The device name is compared with the device id setting (the address is compared the same way).
        let line79 = at("connect", 79);
        let selected =
            devices.len() == 1 || settings.is_device_id(&device.name, line79)? || settings.is_device_id(ip, line79)?;
        if !selected {
            ctx.link.status = LinkStatus::NotConnected;
            ctx.link.ports.clear();
            self.publish_warning(WarningText::DeviceIdMissing);
            return Ok(());
        }
        debug!("trying to connect");
        // The control channel connects to the device's address and integer TCP port. Text that is not an
        // integer and a port outside 0..=65535 are errors before any connection is tried.
        let raised_at = |line: u32, exception: String| MotionError::Native(exception).in_frame(at("connect", line));
        let tcp_port = parse_python_int(&device.tcp_port).ok_or_else(|| {
            let text = python::str_repr(&device.tcp_port);
            raised_at(87, format!("ValueError: invalid literal for int() with base 10: {text}"))
        })?;
        let tcp_port = u16::try_from(tcp_port)
            .map_err(|_| raised_at(87, "OverflowError: connect(): port must be 0-65535.".into()))?;
        let addr = resolve(ip, tcp_port).map_err(MotionError::Protocol)?;
        let control = (self.connector)(addr).map_err(|e| raised_at(87, python::socket_exception(&e)))?;
        self.session().set_control(control);
        debug!("telling yaw vr that srs wants to connect");
        // The hello is sent with the integer UDP port.
        let udp_port = settings.udp_port_int().map_err(|e| e.in_frame(at("connect", 89)))?;
        let hello = udp_port.to_i64().and_then(protocol::hello).ok_or_else(|| {
            raised_at(89, "struct.error: 'i' format requires -2147483648 <= number <= 2147483647".into())
        })?;
        let sent = self.send_control(&hello).map_err(socket_error(at("connect", 89)))?;
        debug!("{sent}");
        // The answer is at most 1024 bytes of ASCII text.
        let reply = self.recv_control(protocol::RECV_SIZE).map_err(socket_error(at("connect", 92)))?;
        if let Some(exception) = python::ascii_decode_error(&reply) {
            return Err(raised_at(92, exception));
        }
        let reply = String::from_utf8_lossy(&reply).into_owned();
        debug!("tcp response is {reply}");
        if !reply.contains("AVAILABLE") {
            // The status and the ports are written before the warning text reads the fields.
            let parts: Vec<&str> = reply.split(';').collect();
            ctx.link.status = LinkStatus::NotConnected;
            ctx.link.ports.clear();
            let (user, source) = match (parts.get(1), parts.get(2)) {
                (Some(u), Some(s)) => (*u, *s),
                _ => return Err(raised_at(132, "IndexError: list index out of range".into())),
            };
            let busy = WarningText::Busy { user: user.to_string(), source: source.to_string() };
            self.publish_warning(busy);
            return Ok(());
        }
        debug!("connected to {}", device.python_str());
        debug!("tcp_response is {reply}");
        // The centre-before-start alert as a boolean, read for a YAWDEVICE only.
        let centered = device.model != YAW_DEVICE_MODEL || !settings.center_at(Some(at("connect", 101)))?;
        let model = device.model.clone();
        self.shared.update(|s| {
            s.centered = centered;
            s.device_model = model;
        });
        if device.name == EMULATOR_NAME {
            self.shared.update(|s| s.limits = Some(YawLimits::EMULATOR));
        } else {
            self.read_parameters()?;
        }
        ctx.link.status = LinkStatus::Connected;
        ctx.link.ports.insert(0, ip.clone());
        self.publish_warning(WarningText::None);
        self.session().with(|s| s.need_start = true);
        Ok(())
    }

    fn disconnect(&mut self, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.disconnect_platform(ctx)
    }

    /// A stream frame: a motion frame.
    fn send_stream(&mut self, motors: &[i64], ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.send_stream_native(&mut native::list_of(motors), ctx)
    }

    /// A stream frame on the caller's list (which is only read).
    fn send_stream_native(&mut self, motors: &mut LegacyValue, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.send_yawvr(motors, ctx).map_err(|e| e.through(at("send_stream", 398)))
    }

    /// A test frame: a motion frame.
    fn send_test(&mut self, motors: &[i64], ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.send_test_native(&mut native::list_of(motors), ctx)
    }

    fn send_test_native(&mut self, motors: &mut LegacyValue, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.send_yawvr(motors, ctx).map_err(|e| e.through(at("send_test", 401)))
    }

    /// The feedback loop reads the motion status and, through a disconnect, clears it and the motion ports.
    fn attach_link(&mut self, link: SharedLink) {
        self.shared.update(|s| s.link = link);
    }

    fn describe(&self) -> String {
        format!("Yaw VR {}", self.shared.get().device_model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::motion::testing::FakeHost;
    use crate::motion::{motion_model, SharedLink};
    use std::collections::VecDeque;
    use std::io;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Script {
        sent: Arc<Mutex<Vec<Vec<u8>>>>,
        replies: Arc<Mutex<VecDeque<Vec<u8>>>>,
        connected_to: Arc<Mutex<Vec<SocketAddr>>>,
    }

    impl ControlChannel for Script {
        fn send(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.sent.lock().unwrap().push(bytes.to_vec());
            Ok(())
        }
        fn recv(&mut self, _max: usize) -> io::Result<Vec<u8>> {
            self.replies.lock().unwrap().pop_front().ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "timeout"))
        }
    }

    const IP: &str = "127.0.0.1";

    fn dump(limits: &YawLimits) -> Vec<u8> {
        let mut d = vec![0u8; 32];
        d[4] = limits.power as u8;
        d[9] = limits.pitch_backward as u8;
        d[13] = limits.pitch_forward as u8;
        d[17] = limits.roll as u8;
        d[19..23].copy_from_slice(&(limits.yaw as u32).to_be_bytes());
        d[24] = limits.yaw_limit_enabled as u8;
        d[31] = limits.vibration_enabled as u8;
        d
    }

    const LIMITS: YawLimits = YawLimits {
        power: 80,
        pitch_forward: 20,
        pitch_backward: 25,
        roll: 30,
        yaw: 180,
        yaw_limit_enabled: 1,
        vibration_enabled: 1,
    };

    struct Rig {
        driver: YawVrDriver,
        settings: LiveSettings<YawVrSettings>,
        shared: LiveSettings<YawShared>,
        script: Script,
        host: FakeHost,
        /// The registry's motion status and motion ports, attached to the driver as the registry does.
        link: SharedLink,
    }

    impl Rig {
        fn new(device_name: &str, replies: &[&[u8]]) -> Self {
            let settings = LiveSettings::new(YawVrSettings {
                udp_port: 33333,
                device_id: "MyYaw".into(),
                auto_park: true,
                center_before_start_alert: true,
                vibration_power: Some(100),
                min_degree_null_yaw_movement: 30,
                yaw_motion_switch: 1,
                game_running: "none".into(),
                listener_running: "none".into(),
                ..YawVrSettings::default()
            });
            let device = YawDevice {
                model: "YAWDEVICE".into(),
                uuid: "u".into(),
                name: device_name.into(),
                tcp_port: "50020".into(),
                status: "READY".into(),
            };
            let shared = LiveSettings::new(YawShared { devices: vec![(IP.into(), device)], ..YawShared::default() });
            let script = Script::default();
            script.replies.lock().unwrap().extend(replies.iter().map(|r| r.to_vec()));
            let s = script.clone();
            let connector: ControlConnector = Box::new(move |addr| {
                s.connected_to.lock().unwrap().push(addr);
                Ok(Box::new(s.clone()) as Box<dyn ControlChannel>)
            });
            let mut driver = YawVrDriver::with_connector(settings.clone(), shared.clone(), connector);
            let link = SharedLink::default();
            driver.attach_link(link.clone());
            Rig { driver, settings, shared, script, host: FakeHost::default(), link }
        }

        fn call<R>(&mut self, f: impl FnOnce(&mut YawVrDriver, &mut MotionContext<'_>) -> R) -> R {
            let before = self.link.get();
            let mut link = before.clone();
            let mut ctx = MotionContext {
                model: motion_model("Yaw VR PRO", false).unwrap(),
                link: &mut link,
                host: &mut self.host,
            };
            let result = f(&mut self.driver, &mut ctx);
            self.link.store_call(&before, link);
            result
        }

        fn sent(&self) -> Vec<Vec<u8>> {
            self.script.sent.lock().unwrap().clone()
        }

        fn messages(&self) -> Vec<String> {
            self.host.datagrams.iter().map(|(_, m)| String::from_utf8(m.clone()).unwrap()).collect()
        }
    }

    fn connected_rig() -> Rig {
        let mut rig = Rig::new("MyYaw", &[b"AVAILABLE", &dump(&LIMITS)]);
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        assert!(rig.link.is_connected());
        rig
    }

    /// Connect: hello `30 <udp_port> SRS`, AVAILABLE, parameter dump; port = sender IP; YAWDEVICE with
    /// center_before_start_alert starts uncentred.
    #[test]
    fn connect_reads_parameters() {
        let rig = connected_rig();
        assert_eq!(rig.sent(), vec![protocol::hello(33333).unwrap(), protocol::GET_PARAMETERS.to_vec()]);
        assert_eq!(rig.script.connected_to.lock().unwrap()[0], "127.0.0.1:50020".parse().unwrap());
        assert_eq!(rig.link.get().ports.get(&0).map(String::as_str), Some(IP));
        let s = rig.shared.get();
        assert_eq!(s.limits, Some(LIMITS));
        assert!(!s.centered);
        assert_eq!(s.device_model, "YAWDEVICE");
    }

    /// Connect: a single discovered device is used whatever device_id says.
    #[test]
    fn connect_single_device_ignores_device_id() {
        let mut rig = Rig::new("Other", &[b"AVAILABLE", &dump(&LIMITS)]);
        rig.settings.update(|s| s.device_id = String::new());
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        assert!(rig.link.is_connected());
        assert_eq!(rig.sent()[0], protocol::hello(33333).unwrap());
        assert!(rig.shared.get().warning.is_empty());
    }

    /// Connect: no device → Not connected; with two devices the first must match device_id by name or IP,
    /// otherwise the device_id warning.
    #[test]
    fn connect_without_matching_device() {
        let mut rig = Rig::new("Other", &[]);
        rig.shared.update(|s| {
            let mut second = s.devices[0].1.clone();
            second.name = "MyYaw".into();
            s.devices.push(("10.0.0.9".into(), second));
        });
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        assert!(!rig.link.is_connected());
        assert!(rig.shared.get().warning.starts_with("In the config file, set device_id"));
        rig.shared.update(|s| s.devices.clear());
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        assert!(rig.sent().is_empty());
    }

    /// Connect: a busy platform answers `<x>;<user>;<source>`; the warning names both.
    #[test]
    fn connect_busy_platform() {
        let mut rig = Rig::new("MyYaw", &[b"BUSY;Alice;10.0.0.7"]);
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        assert!(!rig.link.is_connected());
        assert_eq!(
            rig.shared.get().warning,
            "The Yaw VR cannot be connected: the platform is currently in use by Alice and is being fed data by 10.0.0.7"
        );
        let text = rig.shared.get().warning_text;
        assert_eq!(
            text.render(|key| format!("<{}>", key.trim())),
            "<The Yaw VR cannot be connected: the platform is currently in use by>Alice<and is being fed data by>10.0.0.7"
        );
    }

    /// A connect writes the one shared motion warning (the holder the PT/VNM drivers write),
    /// translated when it is written; a later successful connect clears it.
    #[test]
    fn connect_writes_the_global_motion_warning_translated() {
        let mut rig = Rig::new("MyYaw", &[b"BUSY;Alice;10.0.0.7", b"AVAILABLE", &dump(&LIMITS)]);
        let warning = MotionWarning::new("PT offline".into());
        let translate: Translate = Arc::new(|key: &str| format!("<{}>", key.trim()));
        let connector = std::mem::replace(&mut rig.driver.connector, Box::new(|_| Err(io::ErrorKind::Other.into())));
        rig.driver = YawVrDriver::with_connector(rig.settings.clone(), rig.shared.clone(), connector)
            .with_motion_warning(warning.clone(), translate);
        rig.driver.attach_link(rig.link.clone());
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        assert_eq!(
            warning.get(),
            "<The Yaw VR cannot be connected: the platform is currently in use by>Alice<and is being fed data by>10.0.0.7"
        );
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        assert!(rig.link.is_connected());
        assert_eq!(warning.get(), "");
    }

    /// Connect: only the two fixed fragments and the device_id text are translated.
    #[test]
    fn warning_text_translates_the_literal_parts() {
        assert_eq!(WarningText::None.render(|_| "x".into()), "");
        assert_eq!(WarningText::DeviceIdMissing.render(|k| k.to_uppercase()), DEVICE_ID_WARNING.to_uppercase());
    }

    /// The feedback loop disconnects only when the motion status is `'Connected'`: a timeout before
    /// a connect sends nothing and changes nothing.
    #[test]
    fn link_loss_before_connect_is_ignored() {
        let mut rig = Rig::new("MyYaw", &[b"AVAILABLE", &dump(&LIMITS)]);
        let settings = rig.settings.get();
        net::feedback_failed(&settings, &rig.shared, &|| true, &mut rig.host.clock).unwrap();
        assert!(rig.sent().is_empty());
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        assert!(rig.link.is_connected(), "the earlier timeout is not applied later");
    }

    /// The feedback loop: the disconnect also needs the selected model to be a Yaw VR model
    /// (its manufacturer is `Yaw VR`).
    #[test]
    fn link_loss_with_another_model_selected_is_ignored() {
        let mut rig = connected_rig();
        let sent_before = rig.sent().len();
        let settings = rig.settings.get();
        net::feedback_failed(&settings, &rig.shared, &|| false, &mut rig.host.clock).unwrap();
        assert_eq!(rig.sent().len(), sent_before);
        assert!(rig.link.is_connected());
    }

    /// The YAW_EMULATOR gets fixed limits (70, 30, 30, 30, 359, 1, 1); the first frame sends them with
    /// the limit commands because the last-sent sum is still 0.
    #[test]
    fn emulator_limits_and_first_set_limits() {
        let mut rig = Rig::new("YAW_EMULATOR", &[b"AVAILABLE", b"ok", b"ok", b"ok", b"ok", b"ok"]);
        rig.settings.update(|s| s.device_id = "YAW_EMULATOR".into());
        rig.shared.update(|s| s.devices[0].1.model = "EMULATOR".into());
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        assert_eq!(rig.shared.get().limits, Some(YawLimits::EMULATOR));
        assert!(rig.shared.get().centered, "only YAWDEVICE models wait for centring");
        rig.call(|d, ctx| d.send_stream(&[32768; 12], ctx)).unwrap();
        let sent = rig.sent();
        assert_eq!(&sent[1..6], &YawLimits::EMULATOR.commands().unwrap());
    }

    /// A motion frame: nothing while uncentred; a homing request centres (55 01); then the stream runs, and a
    /// game starting sends A1. Angles: the limit minus the slot rescaled from 0..65536 to 0..2·limit, four decimals.
    #[test]
    fn stream_requires_centring_then_starts() {
        let mut rig = connected_rig();
        rig.call(|d, ctx| d.send_stream(&[0, 65536, 32768], ctx)).unwrap();
        assert!(rig.host.datagrams.is_empty());
        rig.shared.update(|s| s.homing_requested = true);
        rig.settings.update(|s| {
            s.game_running = "assetto".into();
            s.listener_running = "assetto".into();
            s.speed = 50.0;
            s.max_speed = 100.0;
        });
        rig.call(|d, ctx| d.send_stream(&[0, 65536, 32768], ctx)).unwrap();
        assert_eq!(&rig.sent()[2..], &[protocol::CENTER.to_vec(), protocol::START.to_vec()]);
        assert!(!rig.shared.get().homing_requested);
        assert_eq!(rig.messages(), vec!["Y[0.0]P[25.0]R[-30.0]V[50,50,50,17]F[127,127]".to_string()]);
        assert_eq!(rig.host.datagrams[0].0, "127.0.0.1:50010".parse().unwrap());
    }

    /// Game back to 'none' with auto_park: A2 01, then no restart until a game runs again.
    #[test]
    fn stop_and_park_when_idle() {
        let mut rig = connected_rig();
        rig.shared.update(|s| s.centered = true);
        rig.settings.update(|s| s.test_mode = true);
        rig.call(|d, ctx| d.send_stream(&[0; 3], ctx)).unwrap();
        rig.settings.update(|s| s.test_mode = false);
        rig.call(|d, ctx| d.send_stream(&[0; 3], ctx)).unwrap();
        rig.call(|d, ctx| d.send_stream(&[0; 3], ctx)).unwrap();
        assert_eq!(&rig.sent()[2..], &[protocol::START.to_vec(), protocol::STOP_AND_PARK.to_vec()]);
    }

    /// A motion frame: the start/stop state is listener_running, forced to 'none' by an empty game_running and
    /// to 'test' in motion test mode; game_running itself is not compared with 'none'.
    #[test]
    fn run_state_follows_listener_running() {
        let mut rig = connected_rig();
        rig.shared.update(|s| s.centered = true);
        rig.settings.update(|s| s.game_running = "assetto".into());
        rig.call(|d, ctx| d.send_stream(&[0; 3], ctx)).unwrap();
        assert_eq!(rig.sent().len(), 2, "listener 'none': no A1");
        assert!(rig.host.datagrams.is_empty());
        rig.settings.update(|s| {
            s.game_running = "none".into();
            s.listener_running = "assetto".into();
        });
        rig.call(|d, ctx| d.send_stream(&[0; 3], ctx)).unwrap();
        assert_eq!(&rig.sent()[2..], &[protocol::START.to_vec()]);
        assert_eq!(rig.host.datagrams.len(), 1);
        rig.settings.update(|s| s.game_running = String::new());
        rig.call(|d, ctx| d.send_stream(&[0; 3], ctx)).unwrap();
        assert_eq!(
            &rig.sent()[2..],
            &[protocol::START.to_vec(), protocol::STOP_AND_PARK.to_vec()],
            "empty game_running forces 'none'"
        );
        rig.settings.update(|s| s.test_mode = true);
        rig.call(|d, ctx| d.send_stream(&[0; 3], ctx)).unwrap();
        assert_eq!(rig.sent().len(), 5, "test mode forces 'test'");
    }

    /// A motion frame returns while not started — no angle datagram while stopped, including the frame that
    /// stops the platform; a short motor list is not indexed then.
    #[test]
    fn no_datagram_while_stopped() {
        let mut rig = connected_rig();
        rig.shared.update(|s| s.centered = true);
        rig.call(|d, ctx| d.send_stream(&[0; 3], ctx)).unwrap();
        assert!(rig.host.datagrams.is_empty());
        rig.settings.update(|s| s.test_mode = true);
        rig.call(|d, ctx| d.send_stream(&[0; 3], ctx)).unwrap();
        assert_eq!(rig.host.datagrams.len(), 1);
        rig.settings.update(|s| s.test_mode = false);
        rig.call(|d, ctx| d.send_stream(&[0; 3], ctx)).unwrap();
        assert_eq!(rig.host.datagrams.len(), 1, "stop frame sends no angles");
        rig.call(|d, ctx| d.send_stream(&[0], ctx)).unwrap();
    }

    /// Continuous yaw (yaw limit off, not testing): first frame "0" (int), small changes accumulate,
    /// jumps above min_degree_null_yaw_movement are ignored, the sent value wraps once into 0..359.99.
    #[test]
    fn continuous_yaw_integration() {
        let mut rig = Rig::new(
            "MyYaw",
            &[b"AVAILABLE", &dump(&YawLimits { yaw_limit_enabled: 0, ..LIMITS }), b"x", b"x", b"x", b"x", b"x"],
        );
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        rig.shared.update(|s| s.centered = true);
        rig.settings.update(|s| {
            s.game_running = "assetto".into();
            s.listener_running = "assetto".into();
        });
        let mut yaws = Vec::new();
        for angle in [10.0, -5.0, 50.0, 55.0] {
            rig.settings.update(|s| s.yaw_angle = angle);
            rig.call(|d, ctx| d.send_stream(&[32768; 3], ctx)).unwrap();
            let m = rig.messages().pop().unwrap();
            yaws.push(m[2..m.find(']').unwrap()].to_string());
        }
        // 10: previous None → 0; -5: delta -15 → -15 → 345; 50: delta 55 > 30 ignored → -15 → 345; 55: +5 → -10 → 350
        assert_eq!(yaws, vec!["0", "345.0", "345.0", "350.0"]);
    }

    /// Test mode yaw: slot 2 rescaled from 3276..62260 to -180..180, negated.
    #[test]
    fn test_mode_yaw() {
        let mut rig = Rig::new("MyYaw", &[b"AVAILABLE", &dump(&YawLimits { yaw_limit_enabled: 0, ..LIMITS })]);
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        rig.shared.update(|s| s.centered = true);
        rig.settings.update(|s| s.test_mode = true);
        rig.call(|d, ctx| d.send_test(&[32768, 32768, 3276], ctx)).unwrap();
        assert!(rig.messages()[0].starts_with("Y[180.0]"));
    }

    /// The amplitude and the smart-plug PWM: rpm when the integer maximum rpm is above 0, else speed; float division by 0.
    #[test]
    fn vibration_formulas() {
        let s = YawVrSettings {
            vibration_power: Some(100),
            rpm: 4000.9,
            max_rpm: 8000.0,
            speed: 10.0,
            max_speed: 0.0,
            ..YawVrSettings::default()
        };
        assert_eq!(vibration_amplitude(&s).unwrap(), 50);
        assert_eq!(smartplug_pwm(&s), 255, "x/0 is inf, clamped to 255");
        let s = YawVrSettings { max_rpm: 0.5, speed: 30.0, max_speed: 120.0, ..s };
        assert_eq!(vibration_amplitude(&s).unwrap(), 25);
        assert!(vibration_amplitude(&YawVrSettings::default()).is_err());
    }

    /// A feedback timeout while connected → a disconnect on that thread, without a frame: with auto_park
    /// A2 01, 5 s, A3; centred unless the alert; not started; then 'Not connected' and
    /// empty motion ports, which every reader of the registry's state sees at once.
    #[test]
    fn link_loss_disconnects_with_park() {
        let mut rig = connected_rig();
        let link = rig.shared.get().link;
        assert!(link.is_connected());
        let sent_before = rig.sent().len();
        let t0 = rig.host.now();
        let settings = rig.settings.get();
        net::feedback_failed(&settings, &rig.shared, &|| true, &mut rig.host.clock).unwrap();
        assert_eq!(&rig.sent()[sent_before..], &[protocol::STOP_AND_PARK.to_vec(), protocol::EXIT.to_vec()]);
        assert_eq!(rig.host.now() - t0, PARK_PAUSE);
        assert!(!rig.shared.get().centered);
        assert!(!rig.shared.get().session.started());
        assert!(!rig.link.is_connected() && rig.link.get().ports.is_empty());
        // A second timeout while not connected does nothing.
        net::feedback_failed(&settings, &rig.shared, &|| true, &mut rig.host.clock).unwrap();
        assert_eq!(rig.sent().len(), sent_before + 2);
    }

    /// A disconnect without auto_park: A2 ('stopping'), no pause, then A3.
    #[test]
    fn disconnect_without_park_stops() {
        let mut rig = connected_rig();
        rig.settings.update(|s| s.auto_park = false);
        let t0 = rig.host.now();
        rig.call(|d, ctx| d.disconnect(ctx)).unwrap();
        assert_eq!(&rig.sent()[2..], &[protocol::STOP.to_vec(), protocol::EXIT.to_vec()]);
        assert_eq!(rig.host.now(), t0);
        assert!(!rig.link.is_connected());
    }

    /// A limit change from the UI is written with the limit commands on the next frame.
    #[test]
    fn changed_limits_are_written() {
        let mut rig = Rig::new("MyYaw", &[b"AVAILABLE", &dump(&LIMITS), b"a", b"b", b"c", b"d", b"e"]);
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        let changed = YawLimits { power: 60, ..LIMITS };
        rig.shared.update(|s| s.limits = Some(changed));
        rig.call(|d, ctx| d.send_stream(&[0; 3], ctx)).unwrap();
        assert_eq!(&rig.sent()[2..7], &changed.commands().unwrap());
        rig.call(|d, ctx| d.send_stream(&[0; 3], ctx)).unwrap();
        assert_eq!(rig.sent().len(), 7, "unchanged limits are not resent");
    }

    /// `(function, line)` of the frames recorded for an error, outermost first.
    fn trace(error: &MotionError) -> Vec<(&'static str, u32)> {
        error.frames().iter().map(|frame| (frame.function, frame.line)).collect()
    }

    fn connect_error(rig: &mut Rig) -> MotionError {
        rig.call(|d, ctx| d.connect(ctx)).unwrap_err()
    }

    /// The control channel's connect: a TCP port that is not an integer or is out of range
    /// is an error before a connection is tried, a refused connection is a socket error.
    #[test]
    fn connect_statement_errors_are_those_of_int_and_the_socket() {
        let mut rig = Rig::new("MyYaw", &[]);
        rig.shared.update(|s| s.devices[0].1.tcp_port = "50o20".into());
        assert_eq!(
            connect_error(&mut rig).python_traceback().unwrap(),
            "Traceback (most recent call last):\n  File \"backend/controllers/motion_helpers.py\", line 100, in \
             backend.controllers.motion_helpers.connect\n  File \"backend/controllers/yawvr.py\", line 87, in \
             backend.controllers.yawvr.connect\nValueError: invalid literal for int() with base 10: '50o20'\n"
        );
        rig.shared.update(|s| s.devices[0].1.tcp_port = "70000".into());
        let error = connect_error(&mut rig);
        assert_eq!(trace(&error), [("connect", 87)]);
        assert_eq!(error.exception_line().as_deref(), Some("OverflowError: connect(): port must be 0-65535."));
        assert!(rig.script.connected_to.lock().unwrap().is_empty());

        rig.shared.update(|s| s.devices[0].1.tcp_port = "50020".into());
        rig.driver.connector = Box::new(|_| Err(io::Error::from_raw_os_error(111)));
        let error = connect_error(&mut rig);
        assert_eq!(trace(&error), [("connect", 87)]);
        assert_eq!(error.exception_line().as_deref(), Some("ConnectionRefusedError: [Errno 111] Connection refused"));
        assert_eq!(error.python_str(), "[Errno 111] Connection refused");
    }

    /// The errors of the hello's send and of its answer's receive and ASCII decoding.
    #[test]
    fn hello_errors_are_those_of_pack_recv_and_decode() {
        let mut rig = Rig::new("MyYaw", &[]);
        let error = connect_error(&mut rig);
        assert_eq!(trace(&error), [("connect", 92)]);
        assert_eq!(error.exception_line().as_deref(), Some("TimeoutError: timed out"));

        rig.settings.update(|s| s.udp_port = 1 << 40);
        let error = connect_error(&mut rig);
        assert_eq!(trace(&error), [("connect", 89)]);
        assert_eq!(
            error.exception_line().as_deref(),
            Some("struct.error: 'i' format requires -2147483648 <= number <= 2147483647")
        );

        let mut rig = Rig::new("MyYaw", &[b"AVAIL\xc3\xa9"]);
        let error = connect_error(&mut rig);
        assert_eq!(trace(&error), [("connect", 92)]);
        assert_eq!(
            error.exception_line().as_deref(),
            Some("UnicodeDecodeError: 'ascii' codec can't decode byte 0xc3 in position 5: ordinal not in range(128)")
        );
    }

    /// A reply without `AVAILABLE` and without its two `;` fields: the motion status and the motion ports are
    /// written before the warning text indexes the split reply.
    #[test]
    fn busy_reply_without_fields_raises_after_the_status_is_written() {
        let mut rig = Rig::new("MyYaw", &[b"BUSY;Alice"]);
        rig.link.update(|link| {
            link.status = LinkStatus::Connected;
            link.ports.insert(0, "10.0.0.1".into());
        });
        let error = connect_error(&mut rig);
        assert_eq!(trace(&error), [("connect", 132)]);
        assert_eq!(error.exception_line().as_deref(), Some("IndexError: list index out of range"));
        assert_eq!(rig.link.get().status, LinkStatus::NotConnected);
        assert!(rig.link.get().ports.is_empty());
        assert_eq!(rig.shared.get().warning, "");
    }

    /// The frames of a motion frame's errors (under a stream or test frame): the limits read, the stop
    /// and start commands (/) and the motor list.
    #[test]
    fn frame_errors_carry_the_send_yawvr_lines() {
        let mut rig = connected_rig();
        rig.shared.update(|s| s.centered = true);
        rig.settings.update(|s| s.listener_running = "game".into());
        rig.settings.update(|s| s.game_running = "game".into());
        // The start command fails: the scripted channel has no reply to give, but a send succeeds, so make
        // the channel fail instead.
        let failing: Box<dyn ControlChannel> = Box::new(Failing);
        rig.shared.read(|s| s.session.set_control(failing));
        let error = rig.call(|d, ctx| d.send_stream(&[0; 3], ctx)).unwrap_err();
        assert_eq!(trace(&error), [("send_stream", 398), ("send_yawvr", 338)]);
        assert_eq!(error.exception_line().as_deref(), Some("BrokenPipeError: [Errno 32] Broken pipe"));
        // Started, then no game: the stop command (with and without parking).
        rig.shared.read(|s| s.session.with(|session| session.started = true));
        rig.settings.update(|s| s.game_running = String::new());
        let error = rig.call(|d, ctx| d.send_test(&[0; 3], ctx)).unwrap_err();
        assert_eq!(trace(&error), [("send_test", 401), ("send_yawvr", 327)]);
        rig.settings.update(|s| s.auto_park = false);
        let error = rig.call(|d, ctx| d.send_stream(&[0; 3], ctx)).unwrap_err();
        assert_eq!(trace(&error), [("send_stream", 398), ("send_yawvr", 330)]);

        let mut rig = connected_rig();
        rig.shared.update(|s| s.centered = true);
        rig.settings.update(|s| {
            s.listener_running = "game".into();
            s.game_running = "game".into();
        });
        for (motors, line) in [(&[][..], 343), (&[0][..], 344), (&[0, 0][..], 347)] {
            let error = rig.call(|d, ctx| d.send_stream(motors, ctx)).unwrap_err();
            assert_eq!(trace(&error), [("send_stream", 398), ("send_yawvr", line)]);
            assert_eq!(error.exception_line().as_deref(), Some("IndexError: list index out of range"));
        }
        rig.shared.update(|s| s.limits = None);
        let error = rig.call(|d, ctx| d.send_test(&[0; 3], ctx)).unwrap_err();
        assert_eq!(trace(&error), [("send_test", 401), ("send_yawvr", 273)]);
        assert_eq!(error.exception_line().as_deref(), Some("KeyError: 'power'"));
    }

    struct Failing;

    impl ControlChannel for Failing {
        fn send(&mut self, _: &[u8]) -> io::Result<()> {
            Err(io::Error::from_raw_os_error(32))
        }
        fn recv(&mut self, _: usize) -> io::Result<Vec<u8>> {
            Err(io::Error::from_raw_os_error(104))
        }
    }

    /// The angles give their slot to the rescale, which takes it as a 64-bit float: an integer of any
    /// width that a float holds is clamped by the rescale, one too wide is an overflow error inside it.
    #[test]
    fn native_angles_convert_each_slot_in_map() {
        let mut rig = connected_rig();
        rig.shared.update(|s| s.centered = true);
        rig.settings.update(|s| {
            s.listener_running = "game".into();
            s.game_running = "game".into();
        });
        let huge = LegacyValue::from_integer(num_bigint::BigInt::from(1) << 1100);
        let mut list =
            LegacyValue::Object(openrigcue_model::native_object::NativeObject::new(LegacyValue::List(vec![
                native::int(0),
                huge,
                native::int(0),
            ])));
        let error = rig.call(|d, ctx| d.send_stream_native(&mut list, ctx)).unwrap_err();
        assert_eq!(trace(&error), [("send_stream", 398), ("send_yawvr", 344), ("map", 204)]);
        assert_eq!(error.exception_line().as_deref(), Some("OverflowError: int too large to convert to float"));
        let wide = LegacyValue::from_integer(num_bigint::BigInt::from(1) << 80);
        let x = limited_angle_native(&wide, 30).unwrap();
        assert_eq!(x, -30.0, "a wide slot is clamped to the far end of the range");
    }

    fn native_config() -> NativeYawConfig {
        NativeYawConfig {
            udp_port: Ok(native::int(33333)),
            device_id: Ok(LegacyValue::from("MyYaw")),
            auto_park: Ok(true),
            center_before_start_alert: Ok(true),
            vibration_power: Ok(LegacyValue::from("100")),
            min_degree_null_yaw_movement: Ok(native::int(30)),
        }
    }

    /// The driver's setting reads on the raw values: the UDP port as an integer
    /// (any width until it is packed into the hello), the centre-before-start alert as a boolean for a YAWDEVICE.
    #[test]
    fn native_connect_reads_the_raw_config_at_its_statements() {
        let mut rig = Rig::new("MyYaw", &[b"AVAILABLE", &dump(&LIMITS)]);
        let wide = LegacyValue::from_integer(num_bigint::BigInt::from(1) << 70);
        rig.settings.update(|s| s.native = Some(NativeYawConfig { udp_port: Ok(wide), ..native_config() }));
        let error = connect_error(&mut rig);
        assert_eq!(trace(&error), [("connect", 89)]);
        assert_eq!(
            error.exception_line().as_deref(),
            Some("struct.error: 'i' format requires -2147483648 <= number <= 2147483647")
        );
        let mut rig = Rig::new("MyYaw", &[b"AVAILABLE", &dump(&LIMITS)]);
        rig.settings.update(|s| {
            s.native = Some(NativeYawConfig {
                center_before_start_alert: Err("Exception: Boolean value expected. Got maybe".into()),
                ..native_config()
            })
        });
        let error = connect_error(&mut rig);
        assert_eq!(trace(&error), [("connect", 101), ("str2bool", 261)]);
    }

    /// A motion frame takes the yaw angle times the integer yaw motion switch from the live tune, and
    /// the amplitude whose integer vibration power the rescale takes as a 64-bit float.
    #[test]
    fn native_frame_reads_game_config_live_and_maps_the_vibration_power() {
        let mut rig = connected_rig();
        rig.shared.update(|s| s.centered = true);
        // Continuous yaw: the platform reports no yaw limit.
        rig.shared.update(|s| s.limits.as_mut().unwrap().yaw_limit_enabled = 0);
        rig.settings.update(|s| {
            s.listener_running = "game".into();
            s.game_running = "game".into();
            s.yaw_angle = 10.0;
            s.native = Some(native_config());
        });
        let game_config = std::sync::Arc::new(std::sync::Mutex::new(LegacyValue::Dict(Default::default())));
        let source = std::sync::Arc::clone(&game_config);
        rig.settings
            .read(|s| s.runtime.install_game_config(std::sync::Arc::new(move || source.lock().unwrap().clone())));
        let mut list = LegacyValue::List(vec![native::int(32768); 3]);
        let error = rig.call(|d, ctx| d.send_stream_native(&mut list, ctx)).unwrap_err();
        assert_eq!(trace(&error), [("send_stream", 398), ("send_yawvr", 352)]);
        assert_eq!(error.exception_line().as_deref(), Some("KeyError: 'motion_switch'"));
        let mut yaw = openrigcue_model::native_value::Dict::new();
        yaw.insert("yaw", LegacyValue::from("1"));
        let mut root = openrigcue_model::native_value::Dict::new();
        root.insert("motion_switch", LegacyValue::Dict(yaw));
        *game_config.lock().unwrap() = LegacyValue::Dict(root);
        rig.call(|d, ctx| d.send_stream_native(&mut list, ctx)).unwrap();
        let huge = LegacyValue::from_integer(num_bigint::BigInt::from(1) << 1100);
        rig.settings.update(|s| s.native = Some(NativeYawConfig { vibration_power: Ok(huge), ..native_config() }));
        let error = rig.call(|d, ctx| d.send_stream_native(&mut list, ctx)).unwrap_err();
        assert_eq!(trace(&error), [("send_stream", 398), ("send_yawvr", 371), ("get_amplitude", 238), ("map", 204)]);
        assert_eq!(error.exception_line().as_deref(), Some("OverflowError: int too large to convert to float"));
        // The threshold's integer conversion fails at its own frame line.
        rig.settings.update(|s| {
            s.native =
                Some(NativeYawConfig { min_degree_null_yaw_movement: Ok(LegacyValue::from("x")), ..native_config() })
        });
        let error = rig.call(|d, ctx| d.send_stream_native(&mut list, ctx)).unwrap_err();
        assert_eq!(trace(&error), [("send_stream", 398), ("send_yawvr", 356)]);
    }
}
