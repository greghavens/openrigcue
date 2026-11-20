//! Controller discovery, identification, streaming and parameter push: the connect, the
//! identification of a controller, the motion frame, the queued parameter writes, stream and test
//! frames, and the disconnect.

use std::cmp::Ordering;
use std::sync::Arc;
use std::time::Duration;

use openrigcue_model::native_value::LegacyValue;
use rand::seq::SliceRandom;
use tracing::debug;

use super::amc::{Amc, AmcError, CHALLENGE_ALPHABET, CHALLENGE_LEN};
use super::protocol::{
    self, compare_loose_versions, ConfigKind, ControllerConfig, Field, FieldValue, Parameter, AMC_MODELS, MDBOX_MODEL,
    SECONDARY_SYNC_FIELDS, STROKE_REPORTING_FIRMWARE,
};
use super::{at, has_usb_id, ActuatorLayout, VerticalPositions};
use crate::discovery::{allowed_search_com_ports, PortInfo};
use crate::motion::driver_support::CornerTravel;
use crate::motion::native::{self, NativeSetting};
use crate::motion::{
    python, python_print, ActuatorPositions, DriverFactory, LinkStatus, LiveSettings, ModelSpec, MotionContext,
    MotionDriver, MotionError, PyFrame,
};
use crate::serial::OpenOptions;
use num_bigint::BigInt;

/// The file name in the frames of a failed version comparison.
const DISTUTILS_VERSION_FILE: &str = "setuptools\\_distutils\\version.py";
/// The largest motor value of every THANOS model; also the reverse-flag pivot (`65536 - value`).
const FULL_SCALE: i64 = 65_536;
/// Twice the integer vertical spike filter is capped at this value before the spike range is computed.
const SPIKE_PRE_MAX: i64 = 64_530;
/// `disable_park_type` queued for the main and the secondary controller once the secondary is verified.
const MAIN_DISABLE_PARK_TYPE: i64 = 1;
const SECONDARY_DISABLE_PARK_TYPE: i64 = 5;
/// USB ids accepted for the main controller (FTDI FT232R, Arduino Mega 2560 R3); the secondary is FTDI only.
const FTDI: (u16, u16) = (0x0403, 0x6001);
const ARDUINO_MEGA: (u16, u16) = (0x2341, 0x0042);
/// The open options of a controller's port: 250000 baud, no read timeout, a write timeout of 3 s.
const OPEN_OPTIONS: OpenOptions =
    OpenOptions { baud_rate: protocol::BAUD, write_timeout: Some(Duration::from_secs(3)) };

/// Which controller the identification checks: main (`'M'`) or secondary (`'S'`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Main,
    Secondary,
}

impl Role {
    fn letter(self) -> char {
        match self {
            Role::Main => 'M',
            Role::Secondary => 'S',
        }
    }
}

/// A queued parameter write (an entry of the pending configuration of the main or the secondary controller).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingChange {
    pub field: Field,
    pub value: FieldValue,
}

/// The controller data: the firmware text, the controller name, the configuration of each
/// controller and the pending configuration of each.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ControllerState {
    pub firmware: String,
    pub controller: String,
    pub main_config: ControllerConfig,
    pub secondary_config: ControllerConfig,
    pub main_pending: Vec<PendingChange>,
    pub secondary_pending: Vec<PendingChange>,
}

impl ControllerState {
    fn is_thanos4u(&self) -> bool {
        self.controller == "Thanos4U(S)" || self.controller == "Thanos4U(M)"
    }

    fn is_mdbox(&self) -> bool {
        self.controller == MDBOX_MODEL
    }

    /// Queue a parameter change as the Hardware page does (replace in place or append).
    pub fn queue(&mut self, role: Role, field: Field, value: FieldValue) {
        let list = match role {
            Role::Main => &mut self.main_pending,
            Role::Secondary => &mut self.secondary_pending,
        };
        match list.iter_mut().find(|c| c.field == field) {
            Some(c) => c.value = value,
            None => list.push(PendingChange { field, value }),
        }
    }
}

/// The port cache: the persistent map (written to disk at once) of the port each device last
/// connected on, shared by the Thanos driver (`'thanos'`, `'thanos2'`) and the accessory scans. The service
/// provides the stored one; a driver without it keeps the map in memory.
pub trait ComPortCache: Send + Sync {
    /// The cached port of `key`; `None` when the key is missing.
    fn get(&self, key: &str) -> Option<String>;
    /// Stores the port of `key` (written to disk at
    /// once).
    fn set(&self, key: &str, port: &str);
    /// The cache as text.
    fn python_str(&self) -> String;
}

/// A [`ComPortCache`] held in memory, in insertion order.
#[derive(Default)]
pub struct MemoryComPortCache(std::sync::Mutex<Vec<(String, String)>>);

impl ComPortCache for MemoryComPortCache {
    fn get(&self, key: &str) -> Option<String> {
        let entries = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    fn set(&self, key: &str, port: &str) {
        let mut entries = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        match entries.iter_mut().find(|(k, _)| k == key) {
            Some((_, v)) => *v = port.to_string(),
            None => entries.push((key.to_string(), port.to_string())),
        }
    }

    fn python_str(&self) -> String {
        let entries = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let pairs: Vec<(&str, &str)> = entries.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        crate::motion::python::str_dict_repr(&pairs)
    }
}

/// The driver's handle on the port cache (keys `'thanos'` and `'thanos2'`).
#[derive(Clone)]
pub struct PortCache(Arc<dyn ComPortCache>);

impl PortCache {
    /// The cache key of the main controller's port.
    pub const MAIN: &'static str = "thanos";
    /// The cache key of the secondary controller's port.
    pub const SECONDARY: &'static str = "thanos2";

    pub fn new(store: Arc<dyn ComPortCache>) -> Self {
        PortCache(store)
    }

    /// The cached port of the main controller.
    pub fn main(&self) -> Option<String> {
        self.0.get(Self::MAIN)
    }

    /// The cached port of the secondary controller.
    pub fn secondary(&self) -> Option<String> {
        self.0.get(Self::SECONDARY)
    }

    pub fn set(&self, key: &str, port: &str) {
        self.0.set(key, port);
    }

    /// The cache as text.
    pub fn python_str(&self) -> String {
        self.0.python_str()
    }
}

impl Default for PortCache {
    fn default() -> Self {
        PortCache(Arc::new(MemoryComPortCache::default()))
    }
}

impl std::fmt::Debug for PortCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.python_str())
    }
}

/// Handles compare equal: they carry no value of their own, only access to the shared one.
impl PartialEq for PortCache {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

/// State the driver and the UI both read and write: controller data, the port cache, and the stroke
/// to write into the manufacturer's `vertical_actuators_stroke` setting.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ThanosShared {
    pub controller: ControllerState,
    pub cache: PortCache,
    /// The write the identification makes to the manufacturer's `vertical_actuators_stroke` setting:
    /// the manufacturer of the selected model and the controller's stroke `* 10`, a number or, when the
    /// controller's field is not one, its text ten times over. The daemon takes it and writes the configuration.
    pub vertical_stroke: Option<(String, LegacyValue)>,
    /// Last actuator positions published by a streamed frame (mm).
    pub vertical_positions: Option<VerticalPositions>,
}

/// What the driver reads on every call (the manufacturer's settings, `MOTION_COMPENSATION`, the tune,
/// the service state).
#[derive(Debug, Clone, PartialEq)]
pub struct NativeSpikeFilters {
    /// The horizontal then the vertical value; their lookups precede even the test for a known configuration.
    pub values: Result<[LegacyValue; 2], String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ThanosSettings {
    /// The settings section of the manufacturer: `THANOS`, or `PT-ACTUATOR` for the PT models this driver serves.
    pub section: String,
    /// `com_port` (legacy `thanos_com_port`); empty means auto search.
    pub com_port: String,
    /// `filter` (legacy `send_spike_filter`, default `'yes'`).
    pub send_spike_filter: bool,
    pub native_send_spike_filter: Option<Result<bool, String>>,
    /// `reverse_controller_port_5` … `_8`.
    pub reverse_ports: [bool; 4],
    pub native_reverse_ports: Option<[Result<bool, String>; 4]>,
    /// `MOTION_COMPENSATION.vertical_actuators_stroke`, the scale of the actuator positions.
    pub compensation_stroke_mm: i64,
    /// The stroke as a raw value, converted to an integer after the main position frame is sent.
    pub native_compensation_stroke_mm: Option<NativeSetting>,
    /// The tune's `thanos_spike_filter_horizontal`.
    pub spike_filter_horizontal: f64,
    /// The tune's `thanos_spike_filter_vertical`.
    pub spike_filter_vertical: f64,
    pub native_spike_filters: Option<NativeSpikeFilters>,
    /// Whether the motion test mode is on.
    pub test_mode: bool,
    /// Whether no game is running.
    pub no_game_running: bool,
    /// Live state for the steps that read it.
    pub runtime: crate::motion::driver_support::RuntimeReader,
}

impl Default for ThanosSettings {
    fn default() -> Self {
        ThanosSettings {
            section: "THANOS".into(),
            com_port: String::new(),
            send_spike_filter: true,
            native_send_spike_filter: None,
            reverse_ports: [false; 4],
            native_reverse_ports: None,
            compensation_stroke_mm: 1,
            native_compensation_stroke_mm: None,
            spike_filter_horizontal: 0.0,
            spike_filter_vertical: 0.0,
            native_spike_filters: None,
            test_mode: false,
            no_game_running: true,
            runtime: Default::default(),
        }
    }
}

/// Whether a test frame is sent: the motion test mode is on and no game is running, read when the
/// test frame tests them.
fn live_test_gate(settings: &ThanosSettings) -> bool {
    match settings.runtime.read() {
        Some(view) => view.motion_test_mode && view.game_running == "none",
        None => settings.test_mode && settings.no_game_running,
    }
}

fn prepare_upper_positions(settings: &ThanosSettings, motors: &LegacyValue) -> Result<[LegacyValue; 4], MotionError> {
    let mut upper: [LegacyValue; 4] = std::array::from_fn(|_| LegacyValue::None);
    for (index, result) in upper.iter_mut().enumerate() {
        let reverse = match &settings.native_reverse_ports {
            Some(native) => settings
                .runtime
                .switch(&settings.section, &format!("reverse_controller_port_{}", index + 5), &native[index])
                .map_err(MotionError::Native)?,
            None => settings.reverse_ports[index],
        };
        let value = native::item(motors, index + 4)?;
        *result = if reverse { native::sub(&native::int(FULL_SCALE), &value)? } else { value };
    }
    Ok(upper)
}

/// The spike-filter values of a motion frame from the tune's raw values:
/// twice the integer vertical value, capped at 64530 when greater; the spike range is that divided by 255,
/// truncated; then the horizontal and the vertical level,
/// each its value divided by the range, truncated. The commands send those integers: range,
/// vertical level, TL (horizontal) level, at any width.
fn native_spike_values(values: &[LegacyValue; 2]) -> Result<[BigInt; 3], MotionError> {
    let [horizontal, vertical] = values;
    let pre = native::mul(&native::to_int(vertical)?, &native::int(2))?;
    let pre = if native::gt(&pre, &native::int(SPIKE_PRE_MAX))? { native::int(SPIKE_PRE_MAX) } else { pre };
    let range = native::trunc_float(native::double(&native::truediv(&pre, &native::int(255))?)?)?;
    let level = |value: &LegacyValue| -> Result<BigInt, MotionError> {
        let quotient = native::truediv(value, &range)?;
        native::to_bigint(&native::trunc_float(native::double(&quotient)?)?)
    };
    let tl_level = level(horizontal)?;
    let vertical_level = level(vertical)?;
    Ok([native::to_bigint(&range)?, vertical_level, tl_level])
}

/// A random challenge of 16 ASCII letters and digits.
pub fn random_challenge() -> String {
    let mut rng = rand::thread_rng();
    (0..CHALLENGE_LEN).map(|_| char::from(*CHALLENGE_ALPHABET.choose(&mut rng).expect("non-empty alphabet"))).collect()
}

/// The Thanos driver: one main controller and, for Thanos4U rigs, possibly a secondary one.
pub struct ThanosDriver {
    settings: LiveSettings<ThanosSettings>,
    shared: LiveSettings<ThanosShared>,
    /// The main and the secondary controller.
    main: Option<Amc>,
    secondary: Option<Amc>,
    /// Models whose controller count was raised to 2 (for the rest of the run).
    two_controller_models: Vec<&'static str>,
    first_time_main: bool,
    first_time_secondary: bool,
    /// The spike filters last pushed by a motion frame (`(0, 0)` at start).
    previous_spike_filters: (f64, f64),
    previous_native_spike_filters: [LegacyValue; 2],
    challenge: Box<dyn FnMut() -> String + Send>,
}

impl ThanosDriver {
    pub fn new(settings: LiveSettings<ThanosSettings>, shared: LiveSettings<ThanosShared>) -> Self {
        ThanosDriver {
            settings,
            shared,
            main: None,
            secondary: None,
            two_controller_models: Vec::new(),
            first_time_main: true,
            first_time_secondary: true,
            previous_spike_filters: (0.0, 0.0),
            previous_native_spike_filters: [LegacyValue::Int(0), LegacyValue::Int(0)],
            challenge: Box::new(random_challenge),
        }
    }

    /// Registry factory.
    pub fn factory(settings: LiveSettings<ThanosSettings>, shared: LiveSettings<ThanosShared>) -> DriverFactory {
        Box::new(move || Box::new(ThanosDriver::new(settings.clone(), shared.clone())))
    }

    /// Replace the challenge source (tests).
    pub fn with_challenge(mut self, challenge: impl FnMut() -> String + Send + 'static) -> Self {
        self.challenge = Box::new(challenge);
        self
    }

    fn connect_manual(&mut self, port: &str, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        debug!("The motion controller port is hardcoded {port}");
        let link = match ctx.host.open_serial(port, &OPEN_OPTIONS) {
            Ok(link) => link,
            Err(e) => {
                debug!("Failed to instance AMC in this com port {}", MotionError::from(e).python_str());
                debug!("disconnecting");
                // The main controller is disconnected; it may still be the controller of an
                // earlier connection ('failed to disconnect' is printed when there is none).
                match self.main.take() {
                    Some(previous) => previous.close(),
                    None => python_print(&["failed to disconnect"]),
                }
                return Ok(());
            }
        };
        debug!("Creating AMC Connection");
        // The new controller replaces the main one: the earlier controller is released once the new port is
        // open.
        self.main = None;
        let mut amc = Amc::new(link);
        // The identification as a main controller.
        if self.identify(&mut amc, Role::Main, ctx).map_err(|e| e.through(at("connect", 750)))? {
            debug!("connected manual");
            ctx.link.status = LinkStatus::Connected;
            ctx.link.ports.insert(0, port.to_string());
            self.main = Some(amc);
        } else {
            debug!("disconnecting");
            amc.close();
            self.give_up(ctx);
        }
        Ok(())
    }

    /// The "Trying to connect to" debug line with the port, its description and its hardware id.
    fn log_trying(info: &PortInfo, ctx: &mut MotionContext<'_>) {
        let details = ctx.host.port_details(info);
        debug!("Trying to connect to {} {} {}", info.path, details.description, details.hwid_text());
    }

    fn connect_by_scan(&mut self, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        let ports = ctx.host.serial_ports_checked()?;
        for info in &ports {
            let port = info.path.as_str();
            let details = ctx.host.port_details(info);
            // The hardware id must contain `0403:6001` or `2341:0042`.
            let has_id = |id: (u16, u16)| has_usb_id(&details, id.0, id.1).map_err(|e| e.in_frame(at("connect", 768)));
            let usb_ok = has_id(FTDI)? || has_id(ARDUINO_MEGA)?;
            if !usb_ok || !usable(port, ctx) {
                continue;
            }
            let cached = self.shared.read(|s| s.cache.main());
            if self.first_time_main && cached.as_deref().is_some_and(|c| c != port) {
                continue;
            }
            Self::log_trying(info, ctx);
            let link = match ctx.host.open_serial(port, &OPEN_OPTIONS) {
                Ok(link) => link,
                Err(e) => {
                    debug!("Failed to instance AMC in this com port {}", MotionError::from(e).python_str());
                    continue;
                }
            };
            // The new controller replaces the main one (the earlier controller is released).
            self.main = None;
            let mut amc = Amc::new(link);
            // The identification as a main controller.
            if !self.identify(&mut amc, Role::Main, ctx).map_err(|e| e.through(at("connect", 797)))? {
                debug!("disconnecting");
                continue;
            }
            let mut secondary_port = None;
            if self.shared.get().controller.controller == "Thanos4U(M)" {
                debug!("This is the main Controller");
                // Only a model with more than four actuators has, and is searched for, a secondary controller.
                if ctx.model.motors > 4 {
                    if !self.two_controller_models.contains(&ctx.model.name) {
                        self.two_controller_models.push(ctx.model.name);
                    }
                    secondary_port = self.find_secondary(port, ctx)?;
                    if secondary_port.is_none() {
                        // No secondary controller: the main one is released and the platform stays unconnected.
                        debug!("disconnecting");
                        return Ok(());
                    }
                }
            }
            debug!("connected");
            ctx.link.status = LinkStatus::Connected;
            ctx.link.ports.insert(0, port.to_string());
            let cache = self.shared.read(|s| s.cache.clone());
            cache.set(PortCache::MAIN, port);
            self.main = Some(amc);
            debug!("connection is completed");
            // The motion ports as text, in insertion order: the secondary's slot 1 first.
            let mut entries: Vec<String> = Vec::new();
            if let Some(secondary) = &secondary_port {
                entries.push(format!("1: {}", python::str_repr(secondary)));
            }
            entries.push(format!("0: {}", python::str_repr(port)));
            debug!("{{{}}}", entries.join(", "));
            debug!("Connected");
            return Ok(());
        }
        self.give_up(ctx);
        Ok(())
    }

    /// The common exit of a connect when no controller was accepted: the cached port is no
    /// longer preferred, `Not connected`, empty motion ports.
    fn give_up(&mut self, ctx: &mut MotionContext<'_>) {
        self.first_time_main = false;
        ctx.link.status = LinkStatus::NotConnected;
        ctx.link.ports.clear();
    }

    /// The secondary-controller loop of a connect over a new port listing. Returns
    /// the port of the secondary controller it accepted.
    fn find_secondary(&mut self, main_port: &str, ctx: &mut MotionContext<'_>) -> Result<Option<String>, MotionError> {
        let ports = ctx.host.serial_ports_checked()?;
        for info in &ports {
            let port = info.path.as_str();
            // Each port of the loop is logged ("trying to find secondary controller").
            debug!("trying to find secondary controller {port}");
            // The hardware id must contain `0403:6001`.
            let details = ctx.host.port_details(info);
            if !has_usb_id(&details, FTDI.0, FTDI.1).map_err(|e| e.in_frame(at("connect", 817)))? {
                debug!("invalid address");
                continue;
            }
            if port == main_port {
                debug!("conflict with main controller port");
                continue;
            }
            // A port outside the allowed list is skipped without a log line; a claimed one is logged.
            if !allowed_search_com_ports(port, &ctx.host.allowed_com_ports()) {
                continue;
            }
            if ctx.host.claimed_ports().contains(port) {
                debug!("port in use");
                continue;
            }
            if self.first_time_secondary {
                debug!("first time");
                self.first_time_secondary = false;
                let cached = self.shared.read(|s| s.cache.secondary());
                if let Some(cached) = &cached {
                    debug!("cached_com_port {cached}");
                    debug!("port_no2 {port}");
                }
                if cached.as_deref().is_some_and(|c| c != port) {
                    continue;
                }
            }
            Self::log_trying(info, ctx);
            let link = match ctx.host.open_serial(port, &OPEN_OPTIONS) {
                Ok(link) => link,
                Err(e) => {
                    debug!("Failed to instance AMC2 in this com port {}", MotionError::from(e).python_str());
                    continue;
                }
            };
            // The new controller replaces the secondary one (the earlier controller is released).
            self.secondary = None;
            let mut amc = Amc::new(link);
            // The identification as a secondary controller.
            if self.identify(&mut amc, Role::Secondary, ctx).map_err(|e| e.through(at("connect", 856)))? {
                ctx.link.ports.insert(1, port.to_string());
                let cache = self.shared.read(|s| s.cache.clone());
                cache.set(PortCache::SECONDARY, port);
                self.secondary = Some(amc);
                debug!("new com port cache");
                debug!("{}", cache.python_str());
                return Ok(Some(port.to_string()));
            }
            debug!("disconnecting");
        }
        Ok(None)
    }

    /// Identifies a controller as main or secondary. `Ok(false)` for a controller that is not accepted.
    fn identify(&mut self, amc: &mut Amc, role: Role, ctx: &mut MotionContext<'_>) -> Result<bool, MotionError> {
        debug!("checking if serial is open");
        // The port of a new controller is open.
        debug!("serial is open");
        let firmware = match amc.firmware(ctx.host.clock()) {
            Ok(f) => f,
            Err(e) => {
                debug!("failed to get the firmware str {}", amc_error(e).python_str());
                return Ok(false);
            }
        };
        debug!("get the firmware str {firmware}");
        let fields: Vec<&str> = firmware.split(':').collect();
        let model = fields[0].to_string();
        debug!("got the model model {model}");
        let mut shared = self.shared.get();
        // The frame of a step of the identification whose error leaves it.
        let left_at = |line: u32| move |e: AmcError| amc_error(e).through(at("AMC.is_connected", line));
        // The short configuration is read only for a firmware line with a third field.
        let short_config = if fields.len() > 2 {
            Some(amc.config(ConfigKind::Short, ctx.host.clock()).map_err(left_at(289))?)
        } else {
            None
        };
        if let (Role::Main, Some(cfg)) = (role, &short_config) {
            let version = text_of(cfg, Field::FirmwareVersion)?.replace('v', "");
            shared.controller.firmware = format!("{version}.{}", text_of(cfg, Field::BetaVersion)?);
        }
        // The log line queries the firmware again.
        let again = amc.firmware(ctx.host.clock()).map_err(left_at(293))?;
        debug!("connection is open {model} output {again}");

        let accepted = if AMC_MODELS.contains(&model.as_str()) {
            // The firmware version is compared with `2.11.fix4`. A comparison
            // that fails carries two frames of the version comparison, where the
            // component lists are compared.
            let reports_stroke = compare_loose_versions(&shared.controller.firmware, STROKE_REPORTING_FIRMWARE)
                .ok_or_else(|| {
                    let (ours, theirs) =
                        protocol::loose_version_mismatch(&shared.controller.firmware, STROKE_REPORTING_FIRMWARE)
                            .unwrap_or(("str", "int"));
                    MotionError::Native(format!(
                        "TypeError: '<' not supported between instances of '{ours}' and '{theirs}'"
                    ))
                    .in_frames([
                        at("AMC.is_connected", 297),
                        PyFrame::new(DISTUTILS_VERSION_FILE, "__ge__", 90),
                        PyFrame::new(DISTUTILS_VERSION_FILE, "_cmp", 343),
                    ])
                })?;
            if reports_stroke != Ordering::Less {
                // The configuration is stored; without the third field there is none to store.
                let cfg = short_config.ok_or_else(|| {
                    MotionError::Native(
                        "UnboundLocalError: local variable 'config_dict' referenced before assignment".into(),
                    )
                    .in_frame(at("AMC.is_connected", 298))
                })?;
                // The vertical stroke setting becomes the configuration's `stroke_cm` times 10.
                shared.vertical_stroke = Some(stroke_write(ctx, value_of(&cfg, Field::StrokeCm)?));
                shared.controller.main_config = cfg;
            }
            shared.controller.controller = model;
            self.shared.set(shared);
            return Ok(true);
        } else if model == format!("Thanos4U({})", role.letter()) {
            // The long configuration is read.
            let cfg = amc.config(ConfigKind::Long, ctx.host.clock()).map_err(left_at(305))?;
            match role {
                Role::Main => {
                    // The same write from the configuration's `stroke1` times 10.
                    shared.vertical_stroke = Some(stroke_write(ctx, value_of(&cfg, Field::Stroke(1))?));
                    shared.controller.controller = model;
                    shared.controller.main_config = cfg;
                }
                Role::Secondary => {
                    shared.controller.secondary_config = cfg;
                    debug!("main {}", shared.controller.main_config.python_str());
                    debug!("secondary {}", shared.controller.secondary_config.python_str());
                    queue_secondary_sync(&mut shared.controller)?;
                }
            }
            true
        } else {
            false
        };
        self.shared.set(shared);
        if !accepted {
            return Ok(false);
        }
        // The result is that of the XTEA handshake.
        let challenge = (self.challenge)();
        amc.authenticate(&challenge, ctx.host.clock()).map_err(|e| amc_error(e).through(at("AMC.is_connected", 341)))
    }

    fn has_secondary_port(ctx: &MotionContext<'_>) -> bool {
        ctx.link.ports.len() > 1
    }

    /// Filter output and reverse preparation precede the handled send block. Their errors skip
    /// the update of the last-pushed filters; the public stream/test callers suppress them. Errors of the
    /// send block are printed and the last-pushed filters are still updated.
    fn send_motor_position(&mut self, motors: &LegacyValue, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.send_motor_position_with_print(motors, ctx, python_print)
    }

    fn send_motor_position_with_print(
        &mut self,
        motors: &LegacyValue,
        ctx: &mut MotionContext<'_>,
        mut print: impl FnMut(&[&str]),
    ) -> Result<(), MotionError> {
        let mut settings = self.settings.get();
        // The current filter values are taken once from the tune as it is now.
        if let Some(native) = settings.native_spike_filters.as_mut() {
            if settings.runtime.game_config().is_some() {
                let refreshed =
                    |index: usize| native.values.clone().map(|values: [LegacyValue; 2]| values[index].clone());
                let horizontal = settings.runtime.game_value("thanos_spike_filter_horizontal", &refreshed(0));
                native.values = horizontal.and_then(|horizontal| {
                    let vertical = settings.runtime.game_value("thanos_spike_filter_vertical", &refreshed(1))?;
                    Ok([horizontal, vertical])
                });
            }
            native.values.as_ref().map_err(|detail| MotionError::Native(detail.clone()))?;
        }
        let current = (settings.spike_filter_horizontal, settings.spike_filter_vertical);
        self.push_spike_filter(&settings, current, ctx)?;
        let upper = prepare_upper_positions(&settings, motors)?;
        let result = self.stream_positions(&settings, motors, &upper, ctx);
        if let Err(e) = result {
            print(&["error", &e.python_str()]);
        }
        self.previous_spike_filters = current;
        self.previous_native_spike_filters = settings
            .native_spike_filters
            .and_then(|native| native.values.ok())
            .unwrap_or([LegacyValue::Float(current.0), LegacyValue::Float(current.1)]);
        Ok(())
    }

    /// The spike-filter update, sent when a configuration is known and the filter changed.
    fn push_spike_filter(
        &mut self,
        settings: &ThanosSettings,
        (horizontal, vertical): (f64, f64),
        ctx: &mut MotionContext<'_>,
    ) -> Result<(), MotionError> {
        let shared = self.shared.get();
        let unchanged = match &settings.native_spike_filters {
            Some(native) => match native.values.as_ref() {
                Ok(values) => {
                    let mut equal = true;
                    for (a, b) in values.iter().zip(&self.previous_native_spike_filters) {
                        if !native::eq(a, b)? {
                            equal = false;
                            break;
                        }
                    }
                    equal
                }
                Err(_) => false,
            },
            None => self.previous_spike_filters == (horizontal, vertical),
        };
        if shared.controller.main_config.is_empty() || unchanged {
            return Ok(());
        }
        let [range, level, tl_level] = match &settings.native_spike_filters {
            Some(native) => {
                let values = native.values.as_ref().map_err(|detail| MotionError::Native(detail.clone()))?;
                native_spike_values(values)?
            }
            None => {
                let range = protocol::spike_range((vertical.trunc() as i64 * 2).min(SPIKE_PRE_MAX));
                let zero_division = || MotionError::Protocol("float division by zero".into());
                let tl_level = protocol::spike_level(horizontal, range).ok_or_else(zero_division)?;
                let level = protocol::spike_level(vertical, range).ok_or_else(zero_division)?;
                [BigInt::from(range), BigInt::from(level), BigInt::from(tl_level)]
            }
        };
        let enabled = match &settings.native_send_spike_filter {
            Some(native) => settings.runtime.filter(&settings.section, native).map_err(MotionError::Native)?,
            None => settings.send_spike_filter,
        };
        if !enabled {
            return Ok(());
        }
        let commands = [
            (Parameter::SpikeRange, range),
            (Parameter::SpikeLevel, level),
            (Parameter::TlSpikeLevel, tl_level),
            (Parameter::SpikeOnOff, BigInt::from(1)),
            (Parameter::TlSpikeOnOff, BigInt::from(1)),
        ];
        let mdbox = shared.controller.is_mdbox();
        let secondary = if Self::has_secondary_port(ctx) { self.secondary.as_mut() } else { None };
        for amc in [self.main.as_mut(), secondary].into_iter().flatten() {
            for (parameter, value) in &commands {
                amc.write_parameter_wide(*parameter, value, mdbox, ctx.host.clock()).map_err(amc_error)?;
            }
        }
        Ok(())
    }

    /// The position frames and the actuator positions: the handled send block of a motion frame.
    fn stream_positions(
        &mut self,
        settings: &ThanosSettings,
        motors: &LegacyValue,
        upper: &[LegacyValue; 4],
        ctx: &mut MotionContext<'_>,
    ) -> Result<(), MotionError> {
        // The main controller is looked up (there is none until a
        // connect set it) before the list is read; the position frame packs each value as
        // the big-endian low 16 bits of any integer.
        let main = self.main.as_mut().ok_or_else(|| {
            MotionError::Native("AttributeError: 'NoneType' object has no attribute 'send_position'".into())
        })?;
        let mut positions: Vec<LegacyValue> =
            (0..4).map(|index| native::item(motors, index)).collect::<Result<_, _>>()?;
        positions.extend(upper.iter().cloned());
        let words = positions.iter().map(native::c_uint16).collect::<Result<Vec<_>, _>>()?;
        main.send_words(&words).map_err(amc_error)?;
        // Each entry is its slot rescaled from 0..65535 to 0..the vertical stroke setting as an
        // integer; the positions are assigned once all four exist.
        let layout = ActuatorLayout::from_model_name(ctx.model.name);
        let mut corners = [0.0; 4];
        for (corner, slot) in corners.iter_mut().zip(layout.slots()) {
            let value = native::item(motors, slot)?;
            let stroke = match &settings.native_compensation_stroke_mm {
                Some(stroke) => native::to_int(
                    &settings
                        .runtime
                        .value("MOTION_COMPENSATION", "vertical_actuators_stroke", stroke)
                        .map_err(MotionError::Native)?,
                )?,
                None => native::int(settings.compensation_stroke_mm),
            };
            *corner = native::map([&value, &native::int(0), &native::int(65_535), &native::int(0), &stroke])?;
        }
        let [front_left, front_right, rear_left, rear_right] = corners;
        let positions = VerticalPositions { front_left, front_right, rear_left, rear_right };
        self.shared.update(|s| s.vertical_positions = Some(positions));
        if Self::has_secondary_port(ctx) {
            if let Some(secondary) = self.secondary.as_mut() {
                // The secondary controller gets ports 5 to 8, then four zeros.
                let mut words = upper.iter().map(native::c_uint16).collect::<Result<Vec<_>, _>>()?;
                words.extend([0; 4]);
                secondary.send_words(&words).map_err(amc_error)?;
            }
        }
        Ok(())
    }

    /// The parameter push: writes the queued parameters of each Thanos4U controller, then saves
    /// them (`spv45`, `spv46`) and re-reads the long configuration.
    fn push_pending_parameters(&mut self, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        let mut shared = self.shared.get();
        if !shared.controller.is_thanos4u() || !ctx.link.is_connected() {
            return Ok(());
        }
        let mdbox = shared.controller.is_mdbox();
        let with_secondary = Self::has_secondary_port(ctx);
        let state = &mut shared.controller;
        let targets = [
            (self.main.as_mut(), &mut state.main_pending, &mut state.main_config),
            (
                if with_secondary { self.secondary.as_mut() } else { None },
                &mut state.secondary_pending,
                &mut state.secondary_config,
            ),
        ];
        let mut result = Ok(());
        for (amc, pending, config) in targets {
            let Some(amc) = amc else { continue };
            result = push_to(amc, pending, config, mdbox, ctx);
            if result.is_err() {
                break;
            }
        }
        self.shared.update(|s| {
            s.controller.main_pending = std::mem::take(&mut shared.controller.main_pending);
            s.controller.secondary_pending = std::mem::take(&mut shared.controller.secondary_pending);
            s.controller.main_config = std::mem::take(&mut shared.controller.main_config);
            s.controller.secondary_config = std::mem::take(&mut shared.controller.secondary_config);
        });
        result
    }
}

/// One controller's part of the parameter push.
fn push_to(
    amc: &mut Amc,
    pending: &mut Vec<PendingChange>,
    config: &mut ControllerConfig,
    mdbox: bool,
    ctx: &mut MotionContext<'_>,
) -> Result<(), MotionError> {
    push_to_with_print(amc, pending, config, mdbox, ctx, python_print)
}

fn push_to_with_print(
    amc: &mut Amc,
    pending: &mut Vec<PendingChange>,
    config: &mut ControllerConfig,
    mdbox: bool,
    ctx: &mut MotionContext<'_>,
    mut print: impl FnMut(&[&str]),
) -> Result<(), MotionError> {
    let mut sent = false;
    for change in pending.iter() {
        let Some(parameter) = change.field.parameter() else {
            continue;
        };
        // The command (including its pause) precedes its printed line; a failed command prints nothing.
        // A plain key and a per-motor key each have their own frame line.
        // Only the plain commands format their value as a zero-padded number.
        let dispatch = if parameter.motor().is_some() { 1007 } else { 1000 };
        amc.write_parameter(parameter, &change.value, mdbox, ctx.host.clock())
            .map_err(|e| amc_error(e).through(at("handle_motor_cmds", dispatch)))?;
        let (name, value) = (change.field.name(), change.value.to_string());
        match parameter.motor() {
            Some(m) => print(&["sending motor", &name, &m.to_string(), &value]),
            None => print(&["sending value", &name, &value]),
        }
        sent = true;
    }
    if sent {
        pending.clear();
        // The frame of the parameter push.
        let left_at = |line: u32| move |e: AmcError| amc_error(e).through(at("handle_motor_cmds", line));
        amc.save_all(false, mdbox, ctx.host.clock()).map_err(left_at(1012))?;
        amc.save_all(true, mdbox, ctx.host.clock()).map_err(left_at(1013))?;
        *config = amc.config(ConfigKind::Long, ctx.host.clock()).map_err(left_at(1014))?;
    }
    Ok(())
}

/// The field comparison of a secondary controller's identification.
fn queue_secondary_sync(state: &mut ControllerState) -> Result<(), MotionError> {
    for field in SECONDARY_SYNC_FIELDS {
        let main = value_of(&state.main_config, field)?.clone();
        if &main != value_of(&state.secondary_config, field)? {
            state.queue(Role::Secondary, field, main);
        }
    }
    state.queue(Role::Main, Field::DisableParkType, FieldValue::Int(MAIN_DISABLE_PARK_TYPE));
    state.queue(Role::Secondary, Field::DisableParkType, FieldValue::Int(SECONDARY_DISABLE_PARK_TYPE));
    Ok(())
}

/// The scan filters of a connect: the port is allowed and not in use.
fn usable(port: &str, ctx: &MotionContext<'_>) -> bool {
    allowed_search_com_ports(port, &ctx.host.allowed_com_ports()) && !ctx.host.claimed_ports().contains(port)
}

fn amc_error(e: AmcError) -> MotionError {
    match e {
        AmcError::Link(l) => MotionError::Link(l),
        AmcError::Raised(error) => error,
        other => MotionError::Protocol(other.to_string()),
    }
}

fn value_of(cfg: &ControllerConfig, field: Field) -> Result<&FieldValue, MotionError> {
    // The key error of a configuration without that key.
    cfg.get(field).ok_or_else(|| MotionError::Native(format!("KeyError: {}", python::str_repr(&field.name()))))
}

fn text_of(cfg: &ControllerConfig, field: Field) -> Result<String, MotionError> {
    Ok(value_of(cfg, field)?.to_string())
}

/// The vertical stroke setting to write: the value times 10, in the section of the selected model's
/// manufacturer; `* 10` multiplies a number and repeats text (a configuration field that is not an integer
/// is kept as text). A product beyond 64 bits stays at the limit.
fn stroke_write(ctx: &MotionContext<'_>, value: &FieldValue) -> (String, LegacyValue) {
    let written = match value {
        FieldValue::Int(stroke) => LegacyValue::Int(stroke.saturating_mul(10)),
        FieldValue::Text(text) => LegacyValue::Str(text.repeat(10)),
    };
    (ctx.model.manufacturer.to_string(), written)
}

impl MotionDriver for ThanosDriver {
    /// Connects: the configured port when `com_port` is set, else the USB scan with the
    /// cached-port preference and the secondary search. An error is returned with the link state as it was.
    /// The controllers of an earlier connection stay open until a new port
    /// opens in their place, so a port they still hold cannot be opened again.
    fn connect(&mut self, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        let port = self.settings.get().com_port;
        if port.is_empty() {
            self.connect_by_scan(ctx)
        } else {
            self.connect_manual(&port, ctx)
        }
    }

    /// Disconnects: closes both controllers.
    fn disconnect(&mut self, _ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        for amc in [self.main.take(), self.secondary.take()].into_iter().flatten() {
            amc.close();
        }
        Ok(())
    }

    /// A stream frame; errors that escape the handled send block are suppressed silently.
    fn send_stream(&mut self, motors: &[i64], ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.send_stream_native(&mut native::list_of(motors), ctx)
    }

    /// A stream frame on the caller's list (which is only read).
    fn send_stream_native(&mut self, motors: &mut LegacyValue, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        let _ = self.send_motor_position(motors, ctx);
        Ok(())
    }

    /// A test frame: parameter push, then a frame in test mode with no game running.
    fn send_test(&mut self, motors: &[i64], ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.send_test_native(&mut native::list_of(motors), ctx)
    }

    fn send_test_native(&mut self, motors: &mut LegacyValue, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        // The parameter push.
        self.push_pending_parameters(ctx).map_err(|e| e.through(at("send_test", 1114)))?;
        let settings = self.settings.get();
        if live_test_gate(&settings) {
            let _ = self.send_motor_position(motors, ctx);
        }
        Ok(())
    }

    /// The model's controller count after a connect: 2 once a Thanos4U(M) main with more than four
    /// motors raised it for this model (for the rest of the run), else the table value.
    fn expected_controllers(&self, model: &ModelSpec) -> Option<i64> {
        let raised = self.two_controller_models.contains(&model.name);
        Some(if raised { 2 } else { model.controllers as i64 })
    }

    fn actuators_position(&self) -> Option<ActuatorPositions> {
        self.actuators_travel().map(CornerTravel::to_positions)
    }

    fn actuators_travel(&self) -> Option<CornerTravel> {
        self.shared.get().vertical_positions.map(CornerTravel::from)
    }

    fn describe(&self) -> String {
        let shared = self.shared.get();
        format!("Thanos {} firmware {}", shared.controller.controller, shared.controller.firmware)
    }
}

#[cfg(test)]
mod tests {
    use super::super::amc::{upper_hex, ANSWER_KEY, REQUEST_KEY};
    use super::super::xtea;
    use super::*;
    use crate::discovery::{ClaimedPorts, UsbIdentity};
    use crate::motion::testing::{Device, FakeHost};
    use crate::motion::{motion_model, LinkState, MotionHost};

    const CHALLENGE: &str = "abcdefghABCDEFGH";

    fn usb_port(path: &str, vid: u16, pid: u16) -> PortInfo {
        PortInfo {
            path: path.into(),
            stable_path: None,
            usb: Some(UsbIdentity {
                vendor_id: vid,
                product_id: pid,
                serial_number: None,
                manufacturer: None,
                product: None,
            }),
        }
    }

    fn host_with(ports: Vec<PortInfo>) -> FakeHost {
        let mut host = FakeHost::default();
        host.ports = ports;
        host
    }

    /// Replies of a controller that passes the handshake for [`CHALLENGE`].
    fn handshake_replies() -> Vec<(Vec<u8>, Vec<u8>)> {
        let enc = upper_hex(&xtea::encrypt_ecb(&REQUEST_KEY, CHALLENGE.as_bytes()).unwrap());
        let answer = upper_hex(&xtea::encrypt_ecb(&ANSWER_KEY, CHALLENGE.as_bytes()).unwrap());
        vec![
            (
                protocol::terminated(format!("ZP{}0", &enc[..16]).as_bytes()),
                format!("1st half:{}", &enc[..16]).into_bytes(),
            ),
            (
                protocol::terminated(format!("ZP{}1", &enc[16..]).as_bytes()),
                format!("\r\n2st half:{}", &enc[16..]).into_bytes(),
            ),
            (b"CMD88".to_vec(), format!("\r\nOK\r\nTX:{answer}\r\n").into_bytes()),
        ]
    }

    /// A long configuration line (`CMD55`) with every value `v` except the listed overrides.
    fn long_config_line(v: i64, overrides: &[(usize, &str)]) -> String {
        let mut fields: Vec<String> = (0..42).map(|_| v.to_string()).collect();
        fields[0] = "SRS".into();
        for (i, text) in overrides {
            fields[*i] = text.to_string();
        }
        fields.join(":") + "\r\n"
    }

    fn thanos4u(role: char, long_config: &str) -> Device {
        let mut replies = handshake_replies();
        replies.push((b"RQM".to_vec(), format!("Thanos4U({role}):x\r\n").into_bytes()));
        replies.push((b"CMD55".to_vec(), long_config.as_bytes().to_vec()));
        Device { replies, ..Device::default() }
    }

    struct Rig {
        driver: ThanosDriver,
        settings: LiveSettings<ThanosSettings>,
        shared: LiveSettings<ThanosShared>,
        link: LinkState,
        host: FakeHost,
        model: &'static str,
    }

    impl Rig {
        fn new(host: FakeHost, model: &'static str) -> Self {
            let settings = LiveSettings::new(ThanosSettings::default());
            let shared = LiveSettings::new(ThanosShared::default());
            let driver = ThanosDriver::new(settings.clone(), shared.clone()).with_challenge(|| CHALLENGE.to_string());
            Rig { driver, settings, shared, link: LinkState::default(), host, model }
        }

        fn call<R>(&mut self, f: impl FnOnce(&mut ThanosDriver, &mut MotionContext<'_>) -> R) -> R {
            let mut ctx = MotionContext {
                model: motion_model(self.model, false).unwrap(),
                link: &mut self.link,
                host: &mut self.host,
            };
            f(&mut self.driver, &mut ctx)
        }

        fn connect(&mut self) -> Result<(), MotionError> {
            self.call(|d, ctx| d.connect(ctx))
        }

        fn stream(&mut self, motors: &[i64]) -> Result<(), MotionError> {
            self.call(|d, ctx| d.send_stream(motors, ctx))
        }

        fn test(&mut self, motors: &[i64]) -> Result<(), MotionError> {
            self.call(|d, ctx| d.send_test(motors, ctx))
        }
    }

    /// Connect: with no cached port (none or missing) the first pass
    /// tries every port; an AMC-AASD15A is accepted without the handshake; firmware >= 2.11.fix4 stores the
    /// stroke.
    #[test]
    fn connect_first_pass_without_cache_accepts_amc_model() {
        let mut host = host_with(vec![usb_port("/dev/ttyUSB3", 0x0403, 0x6001)]);
        host.devices.insert(
            "/dev/ttyUSB3".into(),
            Device::replying(&[
                (b"RQM", b"AMC-AASD15A:a:b\r\n"),
                (b"CMD56", b"SRS:v2:11.fix4:AMC-AASD15A::4:0:5:6:1:15:7:1:1:2\r\n"),
            ]),
        );
        let mut rig = Rig::new(host, "4 Actuators [2 Front + 2 Rear]");
        rig.connect().unwrap();
        assert!(rig.link.is_connected());
        assert_eq!(rig.link.ports.get(&0).map(String::as_str), Some("/dev/ttyUSB3"));
        let shared = rig.shared.get();
        assert_eq!(shared.controller.controller, "AMC-AASD15A");
        assert_eq!(shared.controller.firmware, "2.11.fix4");
        assert_eq!(shared.vertical_stroke.map(|(_, value)| value), Some(LegacyValue::Int(150)));
        assert_eq!(shared.cache.main().as_deref(), Some("/dev/ttyUSB3"));
        let writes = rig.host.log("/dev/ttyUSB3").written();
        assert_eq!(writes, vec![b"RQM".to_vec(), b"CMD56".to_vec(), b"RQM".to_vec()], "no handshake for AMC models");
        assert_eq!(rig.host.log("/dev/ttyUSB3").opens[0], OPEN_OPTIONS);
    }

    #[test]
    fn native_pnp_main_and_secondary_membership_fail_before_allow_list() {
        let port = usb_port("/dev/ttyS0", 0x0403, 0x6001);
        let mut host = host_with(vec![port.clone()]);
        let mut details = crate::discovery::ListPortInfo::from_listing(&port);
        details.hwid = None;
        host.listed_details.insert(port.path.clone(), details);
        host.allowed = "/dev/unrelated".into();
        let mut rig = Rig::new(host, "4 Actuators [2 Front + 2 Rear]");
        let error = rig.call(|driver, ctx| driver.connect_by_scan(ctx)).unwrap_err();
        assert!(
            matches!(error.root(), MotionError::Native(message) if message == "TypeError: argument of type 'NoneType' is not iterable")
        );
        assert!(rig.driver.first_time_main);
        assert!(rig.host.log("/dev/ttyS0").opens.is_empty());
        let error = rig.call(|driver, ctx| driver.find_secondary("/dev/main", ctx)).unwrap_err();
        assert_eq!(error.python_str(), "argument of type 'NoneType' is not iterable");
        assert!(rig.driver.first_time_secondary);
        assert!(rig.host.log("/dev/ttyS0").opens.is_empty());
    }

    /// Connect: on the first pass a cached port that differs skips the others; the failed scan clears
    /// the first-scan flag, sets 'Not connected' and empties the motion ports; the next pass scans every port.
    #[test]
    fn connect_first_pass_skips_ports_other_than_cache() {
        let mut host = host_with(vec![usb_port("/dev/ttyUSB3", 0x0403, 0x6001)]);
        host.devices.insert(
            "/dev/ttyUSB3".into(),
            Device::replying(&[
                (b"RQM", b"AMC-AASD15A:a:b\r\n"),
                (b"CMD56", b"SRS:v2:12:AMC-AASD15A::4:0:5:6:1:15:7:1:1:2\r\n"),
            ]),
        );
        let mut rig = Rig::new(host, "4 Actuators [2 Front + 2 Rear]");
        rig.shared.read(|s| s.cache.set(PortCache::MAIN, "/dev/ttyUSB9"));
        rig.link.status = LinkStatus::Connected;
        rig.link.ports.insert(0, "/dev/ttyUSB7".into());
        rig.connect().unwrap();
        assert!(rig.host.log("/dev/ttyUSB3").opens.is_empty());
        assert!(!rig.link.is_connected());
        assert!(rig.link.ports.is_empty(), "stale motion_com_port cleared");
        rig.connect().unwrap();
        assert!(rig.link.is_connected());
        assert_eq!(rig.link.ports.get(&0).map(String::as_str), Some("/dev/ttyUSB3"));
    }

    /// Connect: ports without the FTDI/Arduino ids, claimed ports and ports outside allowed_com_ports are
    /// never opened.
    #[test]
    fn connect_scan_filters() {
        let mut host = host_with(vec![
            usb_port("/dev/ttyUSB0", 0x1a86, 0x7523),
            usb_port("/dev/ttyUSB1", 0x0403, 0x6001),
            usb_port("/dev/ttyACM0", 0x2341, 0x0042),
        ]);
        host.claimed = ClaimedPorts { motion_compensation: Some("/dev/ttyUSB1".into()), ..ClaimedPorts::default() };
        host.allowed = "/dev/ttyUSB0, /dev/ttyUSB1".into();
        let mut rig = Rig::new(host, "4 Actuators [2 Front + 2 Rear]");
        rig.shared.read(|s| s.cache.set(PortCache::MAIN, "/dev/ttyUSB1"));
        rig.connect().unwrap();
        rig.connect().unwrap();
        for p in ["/dev/ttyUSB0", "/dev/ttyUSB1", "/dev/ttyACM0"] {
            assert!(rig.host.log(p).opens.is_empty(), "{p} must not be opened");
        }
    }

    /// The identification of a Thanos4U(M) as main: long config, stroke1 × 10, then the XTEA handshake.
    #[test]
    fn connect_thanos4u_main_runs_handshake() {
        let mut host = host_with(vec![usb_port("/dev/ttyUSB1", 0x2341, 0x0042)]);
        host.devices.insert("/dev/ttyUSB1".into(), thanos4u('M', &long_config_line(3, &[(26, "15")])));
        let mut rig = Rig::new(host, "4 Actuators [2 Front + 2 Rear]");
        rig.shared.read(|s| s.cache.set(PortCache::MAIN, "/dev/ttyUSB1"));
        rig.connect().unwrap();
        assert!(rig.link.is_connected());
        let shared = rig.shared.get();
        assert_eq!(shared.controller.controller, "Thanos4U(M)");
        assert_eq!(shared.vertical_stroke.map(|(_, value)| value), Some(LegacyValue::Int(150)));
        let writes = rig.host.log("/dev/ttyUSB1").written();
        assert_eq!(&writes[..3], &[b"RQM".to_vec(), b"RQM".to_vec(), b"CMD55".to_vec()]);
        assert!(writes[3].starts_with(b"ZP") && writes[5] == b"CMD88");
    }

    /// A Thanos4U(M) with more than four motors needs a secondary Thanos4U(S); differing fields are queued
    /// for it and disable_park_type is queued as 1 (main) and 5 (secondary).
    #[test]
    fn connect_finds_secondary_and_queues_sync() {
        let mut host =
            host_with(vec![usb_port("/dev/ttyUSB1", 0x0403, 0x6001), usb_port("/dev/ttyUSB2", 0x0403, 0x6001)]);
        host.devices.insert("/dev/ttyUSB1".into(), thanos4u('M', &long_config_line(3, &[(26, "10"), (12, "2")])));
        host.devices.insert("/dev/ttyUSB2".into(), thanos4u('S', &long_config_line(3, &[(12, "1")])));
        let mut rig = Rig::new(host, "6 Actuators [2 Front + 2 Rear + TL + Surge]");
        rig.shared.read(|s| s.cache.set(PortCache::MAIN, "/dev/ttyUSB1"));
        rig.connect().unwrap();
        let spec = motion_model(rig.model, false).unwrap();
        assert_eq!(MotionDriver::expected_controllers(&rig.driver, spec), Some(2));
        assert_eq!(rig.link.ports.get(&1).map(String::as_str), Some("/dev/ttyUSB2"));
        let state = rig.shared.get().controller;
        assert_eq!(
            state.secondary_pending,
            vec![
                PendingChange { field: Field::KlmMode, value: FieldValue::Int(2) },
                PendingChange { field: Field::DisableParkType, value: FieldValue::Int(5) },
            ]
        );
        assert_eq!(
            state.main_pending,
            vec![PendingChange { field: Field::DisableParkType, value: FieldValue::Int(1) }]
        );
        assert_eq!(rig.shared.get().cache.secondary().as_deref(), Some("/dev/ttyUSB2"));
    }

    /// Connect: every listed port is named by its own 'trying to find secondary controller'
    /// line; a port that is not an FT232R, the main controller's port and a claimed port each log their reason,
    /// a port outside `allowed_com_ports` logs none. Without a secondary controller the platform stays unconnected.
    #[test]
    fn secondary_search_logs_each_port_and_its_skip_reason() {
        use crate::motion::driver_support::testing::capture_debug_messages;

        let mut host = host_with(vec![
            usb_port("/dev/ttyUSB1", 0x0403, 0x6001),
            usb_port("/dev/ttyACM0", 0x2341, 0x0042),
            usb_port("/dev/ttyUSB3", 0x0403, 0x6001),
            usb_port("/dev/ttyUSB4", 0x0403, 0x6001),
        ]);
        host.devices.insert("/dev/ttyUSB1".into(), thanos4u('M', &long_config_line(3, &[(26, "10")])));
        host.allowed = "/dev/ttyUSB1,/dev/ttyACM0,/dev/ttyUSB4".into();
        host.claimed.belt = Some("/dev/ttyUSB4".into());
        let mut rig = Rig::new(host, "6 Actuators [2 Front + 2 Rear + TL + Surge]");
        rig.shared.read(|s| s.cache.set(PortCache::MAIN, "/dev/ttyUSB1"));
        let messages = capture_debug_messages(|| rig.connect().unwrap());
        let from = messages.iter().position(|message| message == "This is the main Controller").unwrap();
        let until = messages.iter().rposition(|message| message == "disconnecting").unwrap();
        assert_eq!(
            messages[from + 1..until],
            [
                "trying to find secondary controller /dev/ttyUSB1",
                "conflict with main controller port",
                "trying to find secondary controller /dev/ttyACM0",
                "invalid address",
                "trying to find secondary controller /dev/ttyUSB3",
                "trying to find secondary controller /dev/ttyUSB4",
                "port in use",
            ]
        );
        assert!(!messages.iter().any(|message| message == "connected"));
        assert!(!rig.link.is_connected());
        assert!(!rig.link.ports.contains_key(&1));
    }

    /// The connect pass compares the held ports with the model's controller count,
    /// which the connect raised to 2 for a Thanos4U(M) with six motors (table value 1): the registry keeps
    /// the main + secondary connection instead of demoting it on the port check.
    #[test]
    fn registry_keeps_a_raised_two_controller_connection() {
        use crate::motion::{ConnectPassInputs, DriverEnvironment, DriverKind, MotionRegistry};
        let mut host =
            host_with(vec![usb_port("/dev/ttyUSB1", 0x0403, 0x6001), usb_port("/dev/ttyUSB2", 0x0403, 0x6001)]);
        host.devices.insert("/dev/ttyUSB1".into(), thanos4u('M', &long_config_line(3, &[(26, "10")])));
        host.devices.insert("/dev/ttyUSB2".into(), thanos4u('S', &long_config_line(3, &[])));
        let model = "6 Actuators [2 Front + 2 Rear + TL + Surge]";
        let spec = motion_model(model, false).unwrap();
        assert_eq!(spec.controllers, 1);
        let settings = LiveSettings::new(ThanosSettings::default());
        let shared = LiveSettings::new(ThanosShared::default());
        shared.read(|s| s.cache.set(PortCache::MAIN, "/dev/ttyUSB1"));
        let mut registry =
            MotionRegistry::new(DriverEnvironment { windows_drivers: true, ..DriverEnvironment::srs_linux() });
        let (s, sh) = (settings.clone(), shared.clone());
        registry.register(
            DriverKind::Thanos,
            Box::new(move || {
                Box::new(ThanosDriver::new(s.clone(), sh.clone()).with_challenge(|| CHALLENGE.to_string()))
            }),
        );
        let inputs = ConnectPassInputs { model, motion_switch_on: true, auto_search: true, enable_debug: false };
        registry.connect_pass(&inputs, &mut host);
        assert!(registry.link().is_connected());
        assert_eq!(registry.link().ports.len(), 2);
        registry.connect_pass(&inputs, &mut host);
        assert!(registry.link().is_connected(), "the next pass keeps it too");
        // The trait value follows the model table: raised for this model, the table value for another.
        let fresh = ThanosDriver::new(settings, shared);
        assert_eq!(MotionDriver::expected_controllers(&fresh, spec), Some(1));
    }

    /// A version comparison that fails with a type error makes the connect fail; the motion status and the
    /// motion ports keep their previous values.
    #[test]
    fn connect_error_leaves_link_state() {
        let mut host = host_with(vec![usb_port("/dev/ttyUSB3", 0x0403, 0x6001)]);
        host.devices.insert(
            "/dev/ttyUSB3".into(),
            Device::replying(&[
                (b"RQM", b"AMC-NANO:a:b\r\n"),
                (b"CMD56", b"SRS:v2:11.3:AMC-NANO::4:0:5:6:1:15:7:1:1:2\r\n"),
            ]),
        );
        let mut rig = Rig::new(host, "4 Actuators [2 Front + 2 Rear]");
        rig.shared.read(|s| s.cache.set(PortCache::MAIN, "/dev/ttyUSB3"));
        rig.link.status = LinkStatus::Connected;
        rig.link.ports.insert(0, "/dev/ttyUSB9".into());
        let before = rig.link.clone();
        assert!(rig.connect().is_err());
        assert_eq!(rig.link, before);
    }

    fn connected_amc_rig(model: &'static str) -> Rig {
        let mut host = host_with(vec![usb_port("/dev/ttyUSB3", 0x0403, 0x6001)]);
        host.devices.insert(
            "/dev/ttyUSB3".into(),
            Device::replying(&[
                (b"RQM", b"AMC-AASD15A:a:b\r\n"),
                (b"CMD56", b"SRS:v2:12:AMC-AASD15A::4:0:5:6:1:15:7:1:1:2\r\n"),
            ]),
        );
        let mut rig = Rig::new(host, model);
        rig.shared.read(|s| s.cache.set(PortCache::MAIN, "/dev/ttyUSB3"));
        rig.connect().unwrap();
        assert!(rig.link.is_connected());
        rig
    }

    fn frames_after_connect(rig: &mut Rig) -> Vec<Vec<u8>> {
        rig.host.log("/dev/ttyUSB3").written().into_iter().skip(3).collect()
    }

    /// A motion frame: reverse flags (65536 - v) on ports 5..8, the spike filter pushed once when it
    /// changes (range, level, TL level, on/off ×2), the actuator positions in mm.
    #[test]
    fn send_stream_reverse_flags_and_spike_filter() {
        let mut rig = connected_amc_rig("8 Actuators [2 Front + 2 Rear + 2 TL + Surge + Belt]");
        rig.settings.update(|s| {
            s.reverse_ports = [true, false, false, true];
            s.spike_filter_vertical = 1000.0;
            s.spike_filter_horizontal = 600.0;
            s.compensation_stroke_mm = 100;
        });
        let motors = [100, 200, 300, 65535, 1000, 2000, 3000, 0, 0, 0, 0, 0];
        rig.stream(&motors).unwrap();
        rig.stream(&motors).unwrap();
        let frames = frames_after_connect(&mut rig);
        // pre = 2000 → range trunc(2000/255) = 7; level trunc(1000/7) = 142; TL trunc(600/7) = 85
        assert_eq!(
            &frames[..5],
            &[b"spv15007".to_vec(), b"spv16142".to_vec(), b"spv18085".to_vec(), b"spv171".to_vec(), b"spv191".to_vec()]
        );
        let expected = protocol::position_frame(&[100, 200, 300, 65535, 64536, 2000, 3000, 65536]);
        assert_eq!(frames[5], expected);
        assert_eq!(frames[6], expected, "spike filter unchanged: not sent again");
        assert_eq!(frames.len(), 7);
        let p = rig.shared.get().vertical_positions.unwrap();
        assert_eq!(p.rear_right, 100.0);
        assert_eq!(rig.driver.actuators_travel().unwrap().rear_right, 100.0);
        assert_eq!(rig.driver.actuators_position().unwrap().rear_right, 100);
    }

    #[test]
    fn native_reverse_failure_preserves_filter_prefix_and_retries_it() {
        let mut rig = connected_amc_rig("8 Actuators [2 Front + 2 Rear + 2 TL + Surge + Belt]");
        rig.settings.update(|settings| {
            settings.spike_filter_vertical = 1000.0;
            settings.spike_filter_horizontal = 600.0;
            settings.native_reverse_ports =
                Some([Ok(false), Err("KeyError: 'reverse_controller_port_6'".into()), Ok(false), Ok(false)]);
        });
        rig.stream(&[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        assert_eq!(frames_after_connect(&mut rig).len(), 5);
        assert_eq!(rig.driver.previous_spike_filters, (0.0, 0.0));
        assert!(rig.shared.get().vertical_positions.is_none());
        rig.settings.update(|settings| settings.native_reverse_ports.as_mut().unwrap()[1] = Ok(false));
        rig.stream(&[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        let written = frames_after_connect(&mut rig);
        assert_eq!(written.len(), 11, "five filter commands retry before the first position frame");
        assert_eq!(&written[..5], &written[5..10]);
        assert_eq!(rig.driver.previous_spike_filters, (600.0, 1000.0));
        assert!(rig.shared.get().vertical_positions.is_some());
    }

    #[test]
    fn native_filter_error_prevents_commands_and_keeps_retry_snapshot() {
        let mut rig = connected_amc_rig("4 Actuators [2 Front + 2 Rear]");
        rig.settings.update(|settings| {
            settings.spike_filter_vertical = 1000.0;
            settings.spike_filter_horizontal = 600.0;
            settings.native_send_spike_filter = Some(Err("Exception: Boolean value expected. Got maybe".into()));
        });
        rig.stream(&[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        assert!(frames_after_connect(&mut rig).is_empty());
        assert_eq!(rig.driver.previous_spike_filters, (0.0, 0.0));
        rig.settings.update(|settings| settings.native_send_spike_filter = Some(Ok(true)));
        rig.stream(&[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        assert_eq!(frames_after_connect(&mut rig).len(), 6);
        assert_eq!(rig.driver.previous_spike_filters, (600.0, 1000.0));
        rig.settings.update(|settings| settings.native_send_spike_filter = Some(Err("KeyError: 'THANOS'".into())));
        rig.stream(&[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        assert_eq!(frames_after_connect(&mut rig).len(), 7, "unchanged filters skip their selector");
    }

    #[test]
    fn native_filter_selector_follows_arithmetic_and_false_keeps_position_output() {
        let mut rig = connected_amc_rig("4 Actuators [2 Front + 2 Rear]");
        rig.settings.update(|settings| {
            settings.spike_filter_vertical = 100.0;
            settings.native_send_spike_filter = Some(Err("Exception: Boolean value expected. Got invalid".into()));
        });
        let error = rig
            .call(|driver, context| driver.send_motor_position(&native::list_of(&[1, 2, 3, 4, 5, 6, 7, 8]), context))
            .unwrap_err();
        assert_eq!(error.python_str(), "float division by zero");
        assert!(frames_after_connect(&mut rig).is_empty());
        rig.settings.update(|settings| {
            settings.spike_filter_vertical = 1000.0;
            settings.native_send_spike_filter = Some(Ok(false));
        });
        rig.stream(&[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        assert_eq!(frames_after_connect(&mut rig), [protocol::position_frame(&[1, 2, 3, 4, 5, 6, 7, 8])]);
        assert_eq!(rig.driver.previous_spike_filters, (0.0, 1000.0));
    }

    #[test]
    fn native_stroke_failure_keeps_old_positions_after_main_output() {
        let mut rig = connected_amc_rig("4 Actuators [2 Front + 2 Rear]");
        rig.settings.update(|settings| settings.compensation_stroke_mm = 100);
        rig.stream(&[100, 200, 300, 400, 500, 600, 700, 800]).unwrap();
        let previous = rig.shared.get().vertical_positions;
        rig.settings.update(|settings| {
            settings.spike_filter_horizontal = 600.0;
            settings.spike_filter_vertical = 1000.0;
            settings.native_compensation_stroke_mm =
                Some(Err("ValueError: invalid literal for int() with base 10: '150.5'".into()));
        });
        let motors = [1000, 2000, 3000, 4000, 5000, 6000, 7000, 8000];
        rig.stream(&motors).unwrap();
        assert_eq!(rig.shared.get().vertical_positions, previous);
        let frames = frames_after_connect(&mut rig);
        assert_eq!(frames.len(), 7, "old main frame, five filter commands, then new main frame");
        assert_eq!(frames.last(), Some(&protocol::position_frame(&motors)));
        assert_eq!(rig.driver.previous_spike_filters, (600.0, 1000.0));
    }

    #[test]
    fn native_stroke_catch_skips_secondary_and_recovers_without_repeating_filters() {
        let mut rig = connected_amc_rig("4 Actuators [2 Front + 2 Rear]");
        rig.host.devices.insert("/dev/secondary".into(), Device::default());
        let secondary = rig.host.open_serial("/dev/secondary", &OPEN_OPTIONS).unwrap();
        rig.driver.secondary = Some(Amc::new(secondary));
        rig.link.ports.insert(1, "/dev/secondary".into());
        rig.settings.update(|settings| settings.native_compensation_stroke_mm = Some(Ok(LegacyValue::Int(100))));
        let motors = [100, 200, 300, 400, 500, 600, 700, 800];
        rig.stream(&motors).unwrap();
        let previous = rig.shared.get().vertical_positions;
        assert_eq!(rig.host.log("/dev/secondary").written().len(), 1);
        rig.settings.update(|settings| {
            settings.spike_filter_vertical = 1000.0;
            settings.native_compensation_stroke_mm = Some(Err("KeyError: 'vertical_actuators_stroke'".into()));
        });
        let mut output = Vec::new();
        rig.call(|driver, context| {
            driver.send_motor_position_with_print(&native::list_of(&motors), context, |parts| {
                output.push(parts.join(" "))
            })
        })
        .unwrap();
        assert_eq!(output, ["error 'vertical_actuators_stroke'"]);
        assert_eq!(rig.shared.get().vertical_positions, previous);
        assert_eq!(rig.host.log("/dev/secondary").written().len(), 6, "five filter commands precede failed stroke");
        assert_eq!(rig.driver.previous_spike_filters, (0.0, 1000.0));
        rig.settings.update(|settings| settings.native_compensation_stroke_mm = Some(Ok(LegacyValue::Int(250))));
        rig.stream(&motors).unwrap();
        assert_eq!(rig.host.log("/dev/secondary").written().len(), 7, "recovery sends only the position frame");
        assert_eq!(
            rig.shared.get().vertical_positions,
            VerticalPositions::from_slots(&motors, ActuatorLayout::FourCorners, 250)
        );
    }

    #[test]
    fn native_stroke_read_follows_main_send_and_outer_failure_stages() {
        let mut rig = connected_amc_rig("4 Actuators [2 Front + 2 Rear]");
        rig.settings.update(|settings| {
            settings.native_compensation_stroke_mm =
                Some(Err("ValueError: invalid literal for int() with base 10: 'bad'".into()));
            settings.native_reverse_ports = Some(std::array::from_fn(|_| Ok(false)));
        });
        rig.host.devices.insert("/dev/failing".into(), Device { fail_writes: true, ..Default::default() });
        let failing = rig.host.open_serial("/dev/failing", &OPEN_OPTIONS).unwrap();
        rig.driver.main = Some(Amc::new(failing));
        let mut output = Vec::new();
        rig.call(|driver, context| {
            driver.send_motor_position_with_print(&native::list_of(&[1; 8]), context, |parts| {
                output.push(parts.join(" "))
            })
        })
        .unwrap();
        assert_eq!(output.len(), 1);
        assert_eq!(output[0], "error Write timeout", "main write fails before the native stroke read");
        assert!(frames_after_connect(&mut rig).is_empty());
        output.clear();
        rig.settings.update(|settings| {
            settings.native_reverse_ports.as_mut().unwrap()[0] = Err("KeyError: 'reverse_controller_port_5'".into());
        });
        let error = rig
            .call(|driver, context| {
                driver.send_motor_position_with_print(&native::list_of(&[1; 8]), context, |parts| {
                    output.push(parts.join(" "))
                })
            })
            .unwrap_err();
        assert_eq!(error.python_str(), "'reverse_controller_port_5'");
        assert!(output.is_empty(), "outer reverse failure does not enter the inner catch");
    }

    #[test]
    fn native_spike_arithmetic_failure_stops_output_before_selector_and_reverse() {
        let mut rig = connected_amc_rig("4 Actuators [2 Front + 2 Rear]");
        rig.settings.update(|settings| {
            settings.spike_filter_horizontal = 600.0;
            settings.spike_filter_vertical = 1000.0;
            settings.native_spike_filters =
                Some(NativeSpikeFilters { values: Ok([LegacyValue::from("600"), LegacyValue::Int(1000)]) });
            settings.native_send_spike_filter = Some(Err("KeyError: 'filter'".into()));
            settings.native_reverse_ports = Some(std::array::from_fn(|_| Err("KeyError: 'reverse'".into())));
        });
        let mut output = Vec::new();
        let error = rig
            .call(|driver, context| {
                driver.send_motor_position_with_print(&native::list_of(&[1; 8]), context, |parts| {
                    output.push(parts.join(" "))
                })
            })
            .unwrap_err();
        assert_eq!(error.python_str(), "unsupported operand type(s) for /: 'str' and 'int'");
        assert!(frames_after_connect(&mut rig).is_empty());
        assert!(output.is_empty());
        assert_eq!(rig.driver.previous_spike_filters, (0.0, 0.0));
    }

    #[test]
    fn native_spike_required_reads_precede_empty_config_but_arithmetic_does_not() {
        let mut rig = connected_amc_rig("4 Actuators [2 Front + 2 Rear]");
        rig.shared.update(|shared| shared.controller.main_config = ControllerConfig::default());
        rig.settings.update(|settings| {
            settings.native_spike_filters =
                Some(NativeSpikeFilters { values: Err("KeyError: 'thanos_spike_filter_horizontal'".into()) });
        });
        let error =
            rig.call(|driver, context| driver.send_motor_position(&native::list_of(&[1; 8]), context)).unwrap_err();
        assert_eq!(error.python_str(), "'thanos_spike_filter_horizontal'");
        assert!(frames_after_connect(&mut rig).is_empty());
        rig.settings.update(|settings| {
            settings.native_spike_filters =
                Some(NativeSpikeFilters { values: Ok([LegacyValue::from("600"), LegacyValue::Int(1000)]) });
        });
        rig.stream(&[1; 8]).unwrap();
        assert_eq!(frames_after_connect(&mut rig).len(), 1);
        assert_eq!(rig.driver.previous_native_spike_filters, [LegacyValue::from("600"), LegacyValue::Int(1000)]);
    }

    #[test]
    fn native_spike_pair_equality_preserves_raw_kind_and_commits_after_inner_error() {
        let mut rig = connected_amc_rig("4 Actuators [2 Front + 2 Rear]");
        rig.settings.update(|settings| {
            settings.native_spike_filters =
                Some(NativeSpikeFilters { values: Ok([LegacyValue::Bool(false), LegacyValue::Float(0.0)]) });
        });
        rig.stream(&[1; 8]).unwrap();
        assert_eq!(frames_after_connect(&mut rig).len(), 1, "initial numeric zero pair skips arithmetic");
        rig.settings.update(|settings| {
            settings.native_spike_filters =
                Some(NativeSpikeFilters { values: Ok([LegacyValue::Int(600), LegacyValue::Int(1000)]) });
            settings.native_compensation_stroke_mm = Some(Err("KeyError: 'stroke'".into()));
        });
        rig.stream(&[1; 8]).unwrap();
        assert_eq!(frames_after_connect(&mut rig).len(), 7);
        rig.settings.update(|settings| {
            settings.native_spike_filters =
                Some(NativeSpikeFilters { values: Ok([LegacyValue::Float(600.0), LegacyValue::Int(1000)]) });
        });
        rig.stream(&[1; 8]).unwrap();
        assert_eq!(frames_after_connect(&mut rig).len(), 8);
        rig.settings.update(|settings| {
            settings.native_spike_filters =
                Some(NativeSpikeFilters { values: Ok([LegacyValue::from("600"), LegacyValue::Int(1000)]) });
        });
        rig.stream(&[1; 8]).unwrap();
        assert_eq!(frames_after_connect(&mut rig).len(), 8, "string with same numeric text is a changed raw value");
        assert_eq!(rig.driver.previous_native_spike_filters, [LegacyValue::Float(600.0), LegacyValue::Int(1000)]);
    }

    #[test]
    fn native_reverse_lookup_precedes_its_motor_and_keeps_test_gate() {
        let mut settings = ThanosSettings {
            native_reverse_ports: Some([
                Err("KeyError: 'reverse_controller_port_5'".into()),
                Ok(false),
                Ok(false),
                Ok(false),
            ]),
            ..Default::default()
        };
        assert_eq!(
            prepare_upper_positions(&settings, &native::list_of(&[])).unwrap_err().python_str(),
            "'reverse_controller_port_5'"
        );
        settings.native_reverse_ports.as_mut().unwrap()[0] = Ok(false);
        settings.native_reverse_ports.as_mut().unwrap()[1] = Err("Exception: Boolean value expected. Got maybe".into());
        assert!(matches!(
            prepare_upper_positions(&settings, &native::list_of(&[])),
            Err(MotionError::TooFewMotors { needed: 5, got: 0 })
        ));
        assert_eq!(
            prepare_upper_positions(&settings, &native::list_of(&[1, 2, 3, 4, 5])).unwrap_err().python_str(),
            "Boolean value expected. Got maybe"
        );
        let mut rig = connected_amc_rig("4 Actuators [2 Front + 2 Rear]");
        rig.settings.set(settings);
        rig.test(&[]).unwrap();
        assert!(frames_after_connect(&mut rig).is_empty(), "test disabled skips every reverse read");
        rig.settings.update(|settings| settings.test_mode = true);
        rig.test(&[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        assert!(frames_after_connect(&mut rig).is_empty(), "public test suppresses the outer reverse error");
        assert_eq!(rig.driver.previous_spike_filters, (0.0, 0.0));
    }

    #[test]
    fn native_reverse_stage_retains_inner_send_catch_and_filter_error_precedence() {
        let mut rig = connected_amc_rig("4 Actuators [2 Front + 2 Rear]");
        rig.settings.update(|settings| {
            settings.spike_filter_vertical = 100.0;
            settings.native_reverse_ports = Some(std::array::from_fn(|_| Err("KeyError: 'reverse'".into())));
        });
        let error = rig.call(|driver, context| driver.send_motor_position(&native::list_of(&[]), context)).unwrap_err();
        assert_eq!(error.python_str(), "float division by zero");
        assert_eq!(rig.driver.previous_spike_filters, (0.0, 0.0));
        rig.settings.update(|settings| {
            settings.spike_filter_vertical = 1000.0;
            settings.spike_filter_horizontal = 600.0;
            settings.native_reverse_ports = Some(std::array::from_fn(|_| Ok(false)));
        });
        rig.driver.main = None;
        rig.call(|driver, context| driver.send_motor_position(&native::list_of(&[1, 2, 3, 4, 5, 6, 7, 8]), context))
            .unwrap();
        assert_eq!(rig.driver.previous_spike_filters, (600.0, 1000.0), "inner send failure commits filter snapshot");
        assert!(rig.shared.get().vertical_positions.is_none());
    }

    /// A motion frame: a vertical spike filter below 255/2 gives range 0; the division by zero leaves
    /// the last-pushed filters unchanged, so every frame fails until the filter changes.
    #[test]
    fn send_stream_zero_spike_range_blocks_frames_until_changed() {
        let mut rig = connected_amc_rig("4 Actuators [2 Front + 2 Rear]");
        rig.settings.update(|s| s.spike_filter_vertical = 100.0);
        let motors = [1, 2, 3, 4, 5, 6, 7, 8];
        // The stream frame suppresses the range error and gives up the frame.
        rig.stream(&motors).unwrap();
        rig.stream(&motors).unwrap();
        assert!(frames_after_connect(&mut rig).is_empty());
        rig.settings.update(|s| {
            s.spike_filter_vertical = 0.0;
            s.spike_filter_horizontal = 0.0;
        });
        rig.stream(&motors).unwrap();
        assert_eq!(
            frames_after_connect(&mut rig),
            vec![protocol::position_frame(&[1, 2, 3, 4, 5, 6, 7, 8])],
            "(0, 0) equals previous_config: no push"
        );
    }

    /// A motion frame and the last-pushed filters: the driver starts with filters (0, 0), so a first frame with
    /// filters (0, 0) and a known config pushes nothing and sends the 20-byte position frame.
    #[test]
    fn send_stream_first_frame_with_zero_filters_skips_push() {
        let mut rig = connected_amc_rig("4 Actuators [2 Front + 2 Rear]");
        assert!(!rig.shared.get().controller.main_config.is_empty());
        let motors = [1, 2, 3, 4, 5, 6, 7, 8];
        rig.stream(&motors).unwrap();
        let frames = frames_after_connect(&mut rig);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].len(), 20);
        assert!(!frames[0].starts_with(b"spv"));
    }

    /// Test frames: frames only in test mode with no game; AMC models never push pending parameters.
    #[test]
    fn send_test_gates_on_test_mode_and_game() {
        let mut rig = connected_amc_rig("4 Actuators [2 Front + 2 Rear]");
        rig.settings.update(|s| {
            s.test_mode = true;
            s.no_game_running = false;
            s.send_spike_filter = false;
            s.spike_filter_vertical = 1000.0;
        });
        let motors = [1, 2, 3, 4, 5, 6, 7, 8];
        rig.test(&motors).unwrap();
        assert!(frames_after_connect(&mut rig).is_empty());
        rig.settings.update(|s| s.no_game_running = true);
        rig.test(&motors).unwrap();
        assert_eq!(frames_after_connect(&mut rig), vec![protocol::position_frame(&[1, 2, 3, 4, 5, 6, 7, 8])]);
    }

    /// The parameter push: queued fields are written in their parameter's format, then spv45, spv46 and CMD55;
    /// the queue is emptied and the configuration re-read.
    #[test]
    fn send_test_pushes_pending_parameters() {
        let mut host = host_with(vec![usb_port("/dev/ttyUSB1", 0x0403, 0x6001)]);
        host.devices.insert("/dev/ttyUSB1".into(), thanos4u('M', &long_config_line(3, &[(26, "15")])));
        let mut rig = Rig::new(host, "4 Actuators [2 Front + 2 Rear]");
        rig.shared.read(|s| s.cache.set(PortCache::MAIN, "/dev/ttyUSB1"));
        rig.connect().unwrap();
        let before = rig.host.log("/dev/ttyUSB1").written().len();
        rig.shared.update(|s| {
            s.controller.queue(Role::Main, Field::Stroke(2), FieldValue::Int(20));
            s.controller.queue(Role::Main, Field::ParkMoveSpeed, FieldValue::Int(2));
            s.controller.queue(Role::Main, Field::Model, FieldValue::Int(9));
        });
        rig.test(&[0; 12]).unwrap();
        let writes: Vec<Vec<u8>> = rig.host.log("/dev/ttyUSB1").written().into_iter().skip(before).collect();
        assert_eq!(
            writes,
            vec![b"spv25020".to_vec(), b"spv05002".to_vec(), b"spv45".to_vec(), b"spv46".to_vec(), b"CMD55".to_vec()]
        );
        assert!(rig.shared.get().controller.main_pending.is_empty());
    }

    #[test]
    fn command_print_follows_each_completed_write_and_pause() {
        use crate::motion::clock::Clock;
        let mut host = FakeHost::default().device(
            "fake",
            Device { replies: vec![(b"CMD55".to_vec(), long_config_line(3, &[]).into_bytes())], ..Device::default() },
        );
        let link = host.open_serial("fake", &OPEN_OPTIONS).unwrap();
        let observed_clock = host.clock.clone();
        let mut amc = Amc::new(link);
        let mut pending = vec![
            PendingChange { field: Field::Stroke(2), value: FieldValue::Int(20) },
            PendingChange { field: Field::ParkMoveSpeed, value: FieldValue::Int(2) },
        ];
        let mut config = ControllerConfig::default();
        let mut state = LinkState::default();
        let mut ctx = MotionContext {
            model: motion_model("4 Actuators [2 Front + 2 Rear]", false).unwrap(),
            link: &mut state,
            host: &mut host,
        };
        let mut prints = Vec::new();
        push_to_with_print(&mut amc, &mut pending, &mut config, false, &mut ctx, |parts| {
            prints.push((parts.join(" "), observed_clock.now()));
        })
        .unwrap();
        assert_eq!(
            prints,
            vec![
                ("sending motor stroke2 1 20".to_owned(), Duration::from_millis(100)),
                ("sending value park_move_speed 2".to_owned(), Duration::from_millis(200)),
            ]
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn command_print_stops_before_a_failed_write_and_keeps_the_pending_queue() {
        // The frames: the parameter push (per-motor key or plain key), the parameter command, its
        // read-or-write step, the save, and the serial write.
        for (failed_frame, expected, dispatch, command) in [
            (b"spv25020".as_slice(), Vec::<&str>::new(), 1007, ("AMC.cmd_stroke", 439)),
            (b"spv05002".as_slice(), vec!["sending motor stroke2 1 20"], 1000, ("AMC.cmd_park_move_speed", 490)),
        ] {
            let mut host = FakeHost::default()
                .device("fake", Device { fail_on: vec![failed_frame.to_vec()], ..Device::default() });
            let link = host.open_serial("fake", &OPEN_OPTIONS).unwrap();
            let mut amc = Amc::new(link);
            let mut pending = vec![
                PendingChange { field: Field::Stroke(2), value: FieldValue::Int(20) },
                PendingChange { field: Field::ParkMoveSpeed, value: FieldValue::Int(2) },
            ];
            let before = pending.clone();
            let mut config = ControllerConfig::default();
            let mut state = LinkState::default();
            let mut ctx = MotionContext {
                model: motion_model("4 Actuators [2 Front + 2 Rear]", false).unwrap(),
                link: &mut state,
                host: &mut host,
            };
            let mut prints = Vec::new();
            let error = push_to_with_print(&mut amc, &mut pending, &mut config, false, &mut ctx, |parts| {
                prints.push(parts.join(" "));
            })
            .unwrap_err();
            assert!(matches!(error.root(), MotionError::Link(_)), "{error:?}");
            assert_eq!(
                error.frames(),
                [
                    at("handle_motor_cmds", dispatch),
                    at(command.0, command.1),
                    at("AMC.cmd_read_or_write", 421),
                    at("AMC.save_parameter_value", 367),
                    PyFrame::new("serial\\serialwin32.py", "write", 325),
                ]
            );
            assert_eq!(prints, expected);
            assert_eq!(pending, before);
            assert_eq!(config, ControllerConfig::default());
        }
    }

    /// Manual port: identification on the configured port only, no cache update.
    #[test]
    fn connect_manual_port() {
        let mut host = FakeHost::default();
        host.devices.insert("/dev/ttyS9".into(), Device::replying(&[(b"RQM", b"AMC-MDBOX:a\r\n")]));
        let mut rig = Rig::new(host, "4 Actuators [2 Front + 2 Rear]");
        rig.settings.update(|s| s.com_port = "/dev/ttyS9".into());
        rig.connect().unwrap();
        assert!(rig.link.is_connected());
        assert_eq!(rig.shared.get().controller.controller, "AMC-MDBOX");
        assert_eq!(rig.shared.get().cache.python_str(), "{}");
        assert!(rig.host.serial_ports().is_empty());
    }

    /// Manual port that opens but is not identified as main: it is disconnected, then the first-scan flag is
    /// cleared, 'Not connected' and empty motion ports; the next auto scan no longer insists on the cached port.
    #[test]
    fn connect_manual_rejected_controller_gives_up() {
        let mut host =
            host_with(vec![usb_port("/dev/ttyUSB2", 0x0403, 0x6001), usb_port("/dev/ttyUSB3", 0x0403, 0x6001)]);
        host.devices.insert("/dev/ttyS9".into(), Device::replying(&[(b"RQM", b"OTHER:a\r\n")]));
        host.devices.insert("/dev/ttyUSB3".into(), Device::replying(&[(b"RQM", b"AMC-MDBOX:a\r\n")]));
        let mut rig = Rig::new(host, "4 Actuators [2 Front + 2 Rear]");
        rig.shared.read(|s| s.cache.set(PortCache::MAIN, "/dev/ttyUSB2"));
        rig.settings.update(|s| s.com_port = "/dev/ttyS9".into());
        rig.link.ports.insert(0, "/dev/ttyS9".into());
        rig.connect().unwrap();
        assert!(!rig.link.is_connected() && rig.link.ports.is_empty());
        rig.settings.update(|s| s.com_port = String::new());
        rig.connect().unwrap();
        assert!(rig.link.is_connected(), "cache no longer preferred");
        assert_eq!(rig.link.ports.get(&0).map(String::as_str), Some("/dev/ttyUSB3"));
    }

    /// `(function, line)` of the frames recorded for an error, outermost first.
    fn trace(error: &MotionError) -> Vec<(&'static str, u32)> {
        error.frames().iter().map(|frame| (frame.function, frame.line)).collect()
    }

    fn amc_host(replies: &[(&[u8], &[u8])]) -> FakeHost {
        let mut host = host_with(vec![usb_port("/dev/ttyUSB3", 0x0403, 0x6001)]);
        host.devices.insert("/dev/ttyUSB3".into(), Device::replying(replies));
        host
    }

    fn connect_error(replies: &[(&[u8], &[u8])]) -> MotionError {
        Rig::new(amc_host(replies), "4 Actuators [2 Front + 2 Rear]").connect().unwrap_err()
    }

    const THANOS_FRAME: &str = "  File \"backend/controllers/thanos.py\", line ";

    /// The hardware id tests of the main scan and of the secondary search
    /// are type errors for a missing hwid.
    #[test]
    fn usb_identity_errors_carry_the_connect_lines() {
        let port = usb_port("/dev/ttyS0", 0x0403, 0x6001);
        let mut host = host_with(vec![port.clone()]);
        let mut details = crate::discovery::ListPortInfo::from_listing(&port);
        details.hwid = None;
        host.listed_details.insert(port.path.clone(), details);
        let mut rig = Rig::new(host, "4 Actuators [2 Front + 2 Rear]");
        let error = rig.connect().unwrap_err();
        assert_eq!(
            error.python_traceback().unwrap(),
            format!(
                "Traceback (most recent call last):\n  File \"backend/controllers/motion_helpers.py\", line 100, in \
                 backend.controllers.motion_helpers.connect\n{THANOS_FRAME}768, in \
                 backend.controllers.thanos.connect\nTypeError: argument of type 'NoneType' is not iterable\n"
            )
        );
        let error = rig.call(|driver, ctx| driver.find_secondary("/dev/main", ctx)).unwrap_err();
        assert_eq!(trace(&error), [("connect", 817)]);
    }

    /// The short configuration read of the identification: the fifth field of a reply with fewer
    /// than five fields is an index error.
    #[test]
    fn short_configuration_without_its_fifth_field_raises_at_the_layout_test() {
        let error = connect_error(&[(b"RQM", b"AMC-AASD15A:a:b\r\n"), (b"CMD56", b"SRS:v2:11\r\n")]);
        assert_eq!(trace(&error), [("connect", 797), ("AMC.is_connected", 289), ("AMC.gen_config_dict", 226)]);
        assert_eq!(error.exception_line().as_deref(), Some("IndexError: list index out of range"));
        assert_eq!(error.python_str(), "list index out of range");
    }

    /// A configuration reply shorter than its layout: storing a field as an integer is an index error,
    /// and storing it as text while that error is handled is one again, so the
    /// traceback shows both. For the leading text fields the store fails alone.
    #[test]
    fn configuration_shorter_than_its_layout_raises_inside_the_except_block() {
        let error = connect_error(&[(b"RQM", b"AMC-AASD15A:a:b\r\n"), (b"CMD56", b"SRS:v2:11:AMC-AASD15A::4:0\r\n")]);
        assert_eq!(
            error.python_traceback().unwrap(),
            format!(
                "Traceback (most recent call last):\n{THANOS_FRAME}250, in \
                 backend.controllers.thanos.AMC.gen_config_dict\nIndexError: list index out of range\n\nDuring \
                 handling of the above exception, another exception occurred:\n\nTraceback (most recent call \
                 last):\n  File \"backend/controllers/motion_helpers.py\", line 100, in \
                 backend.controllers.motion_helpers.connect\n{THANOS_FRAME}797, in \
                 backend.controllers.thanos.connect\n{THANOS_FRAME}289, in \
                 backend.controllers.thanos.AMC.is_connected\n{THANOS_FRAME}252, in \
                 backend.controllers.thanos.AMC.gen_config_dict\nIndexError: list index out of range\n"
            )
        );
        let error = connect_error(&[(b"RQM", b"Thanos4U(M):x\r\n"), (b"CMD55", b"SRS:x\r\n")]);
        assert_eq!(trace(&error), [("connect", 797), ("AMC.is_connected", 305), ("AMC.gen_config_dict", 254)]);
        assert_eq!(error.handled_traceback().as_deref(), Some(""));
        let long = long_config_line(3, &[]);
        let cut = long.trim_end().rsplit_once(':').unwrap().0.to_string() + "\r\n";
        let error = connect_error(&[(b"RQM", b"Thanos4U(M):x\r\n"), (b"CMD55", cut.as_bytes())]);
        assert_eq!(trace(&error), [("connect", 797), ("AMC.is_connected", 305), ("AMC.gen_config_dict", 252)]);
        assert!(error.handled_traceback().unwrap().contains("line 250, in"));
    }

    /// A reply that is not UTF-8 is a decode error in the query's frame: the short and the long
    /// configuration query under the configuration read. The error of the first firmware query of
    /// the identification is handled and its text logged.
    #[test]
    fn undecodable_replies_raise_unicode_decode_error_in_the_query_method() {
        use crate::motion::driver_support::testing::capture_debug_messages;
        let error = connect_error(&[(b"RQM", b"AMC-AASD15A:a:b\r\n"), (b"CMD56", b"SRS:\xff\r\n")]);
        assert_eq!(
            trace(&error),
            [("connect", 797), ("AMC.is_connected", 289), ("AMC.gen_config_dict", 224), ("AMC.get_short_config", 396)]
        );
        assert_eq!(
            error.exception_line().as_deref(),
            Some("UnicodeDecodeError: 'utf-8' codec can't decode byte 0xff in position 4: invalid start byte")
        );
        let error = connect_error(&[(b"RQM", b"Thanos4U(M):x\r\n"), (b"CMD55", b"\xe2\x82")]);
        assert_eq!(
            trace(&error),
            [("connect", 797), ("AMC.is_connected", 305), ("AMC.gen_config_dict", 234), ("AMC.get_long_config", 387)]
        );
        assert_eq!(error.python_str(), "'utf-8' codec can't decode bytes in position 0-1: unexpected end of data");
        let mut rig = Rig::new(amc_host(&[(b"RQM", b"AMC\xe2\x28")]), "4 Actuators [2 Front + 2 Rear]");
        let logged = capture_debug_messages(|| rig.connect().unwrap());
        assert!(
            logged.contains(
                &"failed to get the firmware str 'utf-8' codec can't decode byte 0xe2 in position 3: invalid \
                  continuation byte"
                    .to_string()
            ),
            "{logged:?}"
        );
    }

    /// A firmware line without a third field leaves no configuration; an AMC model whose stored
    /// firmware is at least 2.11.fix4 then reads it in the identification, which is an error.
    #[test]
    fn amc_model_without_short_configuration_reads_the_unassigned_local() {
        let mut rig = Rig::new(amc_host(&[(b"RQM", b"AMC-AASD15A\r\n")]), "4 Actuators [2 Front + 2 Rear]");
        rig.shared.update(|s| s.controller.firmware = "2.12".into());
        let error = rig.connect().unwrap_err();
        assert_eq!(trace(&error), [("connect", 797), ("AMC.is_connected", 298)]);
        assert_eq!(
            error.exception_line().as_deref(),
            Some("UnboundLocalError: local variable 'config_dict' referenced before assignment")
        );
        rig.settings.update(|s| s.com_port = "/dev/ttyUSB3".into());
        assert_eq!(trace(&rig.connect().unwrap_err()), [("connect", 750), ("AMC.is_connected", 298)]);
    }

    /// The firmware comparison with `2.11.fix4`: comparing the component lists is a
    /// type error for the first pair that differs when one is a number and the other text, with the
    /// two frames of the version comparison.
    #[test]
    fn firmware_comparison_type_error_names_both_component_types() {
        let amc = |short: &'static [u8]| connect_error(&[(b"RQM", b"AMC-AASD15A:a:b\r\n"), (b"CMD56", short)]);
        let error = amc(b"SRS:vbeta:1:AMC-AASD15A::4:0:5:6:1:15:7:1:1:2\r\n");
        assert_eq!(
            error.exception_line().as_deref(),
            Some("TypeError: '<' not supported between instances of 'str' and 'int'")
        );
        assert_eq!(trace(&error), [("connect", 797), ("AMC.is_connected", 297), ("__ge__", 90), ("_cmp", 343)]);
        assert!(error.python_traceback().unwrap().contains(
            "  File \"setuptools\\_distutils\\version.py\", line 343, in _cmp\nTypeError: '<' not supported"
        ));
        let error = amc(b"SRS:v2:11.5:AMC-AASD15A::4:0:5:6:1:15:7:1:1:2\r\n");
        assert_eq!(error.python_str(), "'<' not supported between instances of 'int' and 'str'");
    }

    /// The hexadecimal decoding of the handshake answer in the identification:
    /// an index error without a second field, a value error with the position
    /// of the first character that is not a hexadecimal digit.
    #[test]
    fn verification_answer_errors_are_those_of_the_fromhex_statement() {
        let with_answer = |answer: &str| {
            let mut device = thanos4u('M', &long_config_line(3, &[]));
            device.replies.retain(|(command, _)| command != b"CMD88");
            device.replies.push((b"CMD88".to_vec(), format!("\r\nOK\r\n{answer}\r\n").into_bytes()));
            let mut host = host_with(vec![usb_port("/dev/ttyUSB3", 0x0403, 0x6001)]);
            host.devices.insert("/dev/ttyUSB3".into(), device);
            Rig::new(host, "4 Actuators [2 Front + 2 Rear]").connect().unwrap_err()
        };
        let frames = [("connect", 797), ("AMC.is_connected", 341), ("AMC.verification_with_encryption", 200)];
        let error = with_answer("TX");
        assert_eq!(trace(&error), frames);
        assert_eq!(error.exception_line().as_deref(), Some("IndexError: list index out of range"));
        let error = with_answer("TX:0G");
        assert_eq!(trace(&error), frames);
        assert_eq!(
            error.exception_line().as_deref(),
            Some("ValueError: non-hexadecimal number found in fromhex() arg at position 1")
        );
        assert_eq!(with_answer("TX:ABC").python_str(), "non-hexadecimal number found in fromhex() arg at position 3");
        assert_eq!(with_answer("TX:AB C").python_str(), "non-hexadecimal number found in fromhex() arg at position 4");
        assert_eq!(with_answer("TX:zzé").python_str(), "non-hexadecimal number found in fromhex() arg at position 2");
    }

    /// A position frame without a main controller is an attribute error; the send block
    /// prints `error` and the message.
    #[test]
    fn frame_without_a_main_controller_prints_the_attribute_error() {
        use crate::motion::driver_support::testing::capture_prints;
        let mut rig = Rig::new(host_with(Vec::new()), "4 Actuators [2 Front + 2 Rear]");
        let printed = capture_prints(|| rig.stream(&[0; 8]).unwrap());
        assert_eq!(printed, ["error 'NoneType' object has no attribute 'send_position'"]);
    }

    /// The parameter push of a test frame: the two save-all commands decode the controller's reply,
    /// each at its own frame line, then the long configuration is read.
    #[test]
    fn pending_parameter_errors_carry_the_handle_motor_cmds_lines() {
        let rig_with = |extra: &[(&[u8], &[u8])], long: &str| {
            let mut device = thanos4u('M', long);
            device.replies.extend(extra.iter().map(|(c, r)| (c.to_vec(), r.to_vec())));
            let mut host = host_with(vec![usb_port("/dev/ttyUSB1", 0x0403, 0x6001)]);
            host.devices.insert("/dev/ttyUSB1".into(), device);
            let mut rig = Rig::new(host, "4 Actuators [2 Front + 2 Rear]");
            rig.connect().unwrap();
            rig.shared.update(|s| s.controller.queue(Role::Main, Field::ParkMoveSpeed, FieldValue::Int(2)));
            rig
        };
        let long = long_config_line(3, &[]);
        let mut rig = rig_with(&[(b"spv45", b"\xff")], &long);
        let error = rig.test(&[0; 12]).unwrap_err();
        assert_eq!(
            trace(&error),
            [("send_test", 1114), ("handle_motor_cmds", 1012), ("AMC.cmd_save_all_parameters_at_once", 674)]
        );
        let mut rig = rig_with(&[(b"spv46", b"\xff")], &long);
        assert_eq!(
            trace(&rig.test(&[0; 12]).unwrap_err()),
            [("send_test", 1114), ("handle_motor_cmds", 1013), ("AMC.cmd_save_all_SRS_parameters_at_once", 680)]
        );
    }

    fn native_list(values: Vec<LegacyValue>) -> LegacyValue {
        LegacyValue::Object(openrigcue_model::native_object::NativeObject::new(LegacyValue::List(values)))
    }

    /// A position frame packs each position as the low 16 bits of an integer of any width
    /// (`2**80 + 5` → 5, `65536 - 2**80` reversed on port 5 → 0); the caller's list is not written.
    #[test]
    fn native_positions_are_masked_by_c_uint16_at_any_width() {
        let mut rig = connected_amc_rig("8 Actuators [2 Front + 2 Rear + 2 TL + Surge + Belt]");
        rig.settings.update(|s| s.reverse_ports = [true, false, false, false]);
        let wide = |extra: i64| LegacyValue::from_integer((BigInt::from(1) << 80) + extra);
        let mut values: Vec<LegacyValue> = (1..=12).map(native::int).collect();
        values[0] = wide(5);
        values[4] = wide(0);
        let mut list = native_list(values);
        let before = list.resolved();
        rig.call(|d, ctx| d.send_stream_native(&mut list, ctx)).unwrap();
        assert_eq!(list.resolved(), before, "send_motor_position only reads motor_pwm");
        let frames = frames_after_connect(&mut rig);
        let frame = frames.last().unwrap();
        assert_eq!(&frame[2..4], &[0, 5]);
        assert_eq!(&frame[10..12], &[0, 0], "65536 - 2**80 keeps its low 16 bits: 0");
    }

    /// The corner rescale takes its slot as a 64-bit float inside the handled send block: an integer too wide
    /// for a float prints `error <message>` after the main frame was written, skips the secondary frame and
    /// still updates the last-pushed filters.
    #[test]
    fn native_corner_map_overflow_is_caught_after_the_main_frame() {
        let mut rig = connected_amc_rig("4 Actuators [2 Front + 2 Rear]");
        rig.host.devices.insert("/dev/secondary".into(), Device::default());
        let secondary = rig.host.open_serial("/dev/secondary", &OPEN_OPTIONS).unwrap();
        rig.driver.secondary = Some(Amc::new(secondary));
        rig.link.ports.insert(1, "/dev/secondary".into());
        let mut values: Vec<LegacyValue> = (1..=12).map(native::int).collect();
        values[1] = LegacyValue::from_integer(BigInt::from(1) << 1100);
        let list = native_list(values);
        let mut output = Vec::new();
        rig.call(|d, ctx| d.send_motor_position_with_print(&list, ctx, |parts| output.push(parts.join(" ")))).unwrap();
        assert_eq!(output, ["error int too large to convert to float"]);
        assert_eq!(frames_after_connect(&mut rig).len(), 1, "the main frame precedes the corner entries");
        assert!(rig.host.log("/dev/secondary").written().is_empty());
        assert!(rig.shared.get().vertical_positions.is_none());
    }

    /// The spike-filter range and levels from the tune's raw values, at any width.
    #[test]
    fn native_spike_arithmetic_preserves_raw_kind_and_failure_order() {
        let values = |horizontal, vertical| {
            native_spike_values(&[horizontal, vertical])
                .map(|v| v.map(|n| n.to_string().parse::<i128>().unwrap()))
                .map_err(|e| e.to_string())
        };
        assert_eq!(values(LegacyValue::Int(600), LegacyValue::Int(1000)), Ok([7, 142, 85]));
        assert_eq!(values(LegacyValue::Float(600.5), LegacyValue::Float(1000.5)), Ok([7, 142, 85]));
        assert_eq!(
            values(LegacyValue::from("600"), LegacyValue::Int(1000)),
            Err("TypeError: unsupported operand type(s) for /: 'str' and 'int'".into())
        );
        assert_eq!(
            values(LegacyValue::Int(600), LegacyValue::from("1000")),
            Err("TypeError: unsupported operand type(s) for /: 'str' and 'int'".into())
        );
        assert_eq!(
            values(LegacyValue::from("bad-horizontal"), LegacyValue::from("bad-vertical")),
            Err("ValueError: invalid literal for int() with base 10: 'bad-vertical'".into())
        );
        assert_eq!(values(LegacyValue::Int(0), LegacyValue::Int(0)), Err("ZeroDivisionError: division by zero".into()));
        assert_eq!(
            values(LegacyValue::Float(0.0), LegacyValue::Int(0)),
            Err("ZeroDivisionError: float division by zero".into())
        );
        assert_eq!(
            values(LegacyValue::Float(f64::NAN), LegacyValue::Int(1000)),
            Err("ValueError: cannot convert float NaN to integer".into())
        );
        assert_eq!(
            values(LegacyValue::Float(f64::INFINITY), LegacyValue::Int(1000)),
            Err("OverflowError: cannot convert float infinity to integer".into())
        );
        assert_eq!(values(LegacyValue::Int(600), LegacyValue::Int(100_000)), Ok([253, 395, 2]));
        // `pre` is capped only from above: a wide negative vertical gives a wide negative range.
        let wide_negative = LegacyValue::from_integer(-(BigInt::from(1) << 100usize));
        let [range, level, tl] = values(LegacyValue::Int(600), wide_negative).unwrap();
        assert_eq!(range, (-(2f64.powi(101)) / 255.0) as i128);
        assert_eq!((level, tl), (127, 0));
    }

    /// The three-digit format of a wide spike range keeps every digit (the sign counts toward the width).
    #[test]
    fn native_wide_spike_values_keep_their_digits() {
        assert_eq!(Parameter::SpikeRange.format_wide(&BigInt::from(-5)), "-05");
        assert_eq!(Parameter::SpikeLevel.format_wide(&BigInt::from(7)), "007");
        assert_eq!(Parameter::SpikeLevel.format_wide(&(BigInt::from(1) << 70)), "1180591620717411303424");
        assert_eq!(Parameter::SpikeOnOff.format_wide(&BigInt::from(1)), "1");
    }

    /// A test frame reads the motion test mode and the running game when it tests them: an installed
    /// live source wins over the values the settings were refreshed with, and later changes are seen at once.
    #[test]
    fn native_test_gate_reads_parameters_live() {
        use crate::motion::driver_support::{RuntimeReader, RuntimeView};
        let reader = RuntimeReader::default();
        let settings =
            ThanosSettings { test_mode: true, no_game_running: true, runtime: reader.clone(), ..Default::default() };
        assert!(live_test_gate(&settings), "no source: the refreshed values");
        let game = std::sync::Arc::new(std::sync::Mutex::new("none".to_string()));
        let source = std::sync::Arc::clone(&game);
        reader.install(std::sync::Arc::new(move || RuntimeView {
            game_running: source.lock().unwrap().clone(),
            motion_test_mode: true,
            ..RuntimeView::default()
        }));
        assert!(live_test_gate(&settings));
        *game.lock().unwrap() = "ac".into();
        assert!(!live_test_gate(&settings), "a game started after the refresh");
        let activity = crate::motion::driver_support::HostActivity { runtime: reader, ..Default::default() };
        assert_eq!(activity.live().game_running, "ac");
        assert!(activity.live().motion_test_mode);
    }
}
