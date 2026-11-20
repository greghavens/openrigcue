//! iRacing (listener `iracing`): the iRacing SDK memory map.
//!
//! The listener and the memory-map client it relies on: the simulator status probe, the
//! data-valid event, start-up and shutdown, the connected state, variable reads from the
//! second-newest buffer and the session-info YAML.

mod data_valid_event;
mod memory;
mod native;
mod session_info;
mod sim_status;
mod yaml11;

use std::sync::{Mutex, PoisonError};

use openrigcue_model::{native_value::LegacyValue, Telemetry};
use yaml_rust2::Yaml;

pub use self::data_valid_event::{BridgeEvent, DataValidEvent, DATA_VALID_TIMEOUT};
use self::memory::{Buffers, Header, Scalar, VarValue, Variables, STATUS_CONNECTED};
use self::session_info::{truthy, SessionInfo};
pub use self::sim_status::{HttpSimStatus, SimStatus, SIM_STATUS_ADDRESS, SIM_STATUS_PATH};
use super::answers::{write_all_raised, write_all_zero, MotionRates, RAW_FLAG_DEFAULT, ZERO_NAME};
use super::block_reader::{BlockOpener, BlockReader, MappingSpec};
use super::estimated_wheel_slip::{estimated_wheel_slip, SlipInputs, WheelSide};
use super::pages::LiveMirrors;
use super::python_numeric::{rad_to_deg, truncate_to_int};
use crate::udp_racing::unhandled::DispatcherEnds;
use crate::{NativeRead, Source, SourceError};

/// The memory map `Local\IRSDKMemMapFileName`: 1191936 bytes, mapped read-only.
pub const MEMORY: MappingSpec = MappingSpec { name: "Local\\IRSDKMemMapFileName", size: 1_191_936 };
/// The data-valid event, which start-up opens and waits on (32 ms); the bridge relays its state.
pub const DATA_VALID_EVENT: &str = "Local\\IRSDKDataValidEvent";
/// Every mapping this listener reads.
pub const MAPPINGS: &[MappingSpec] = &[MEMORY];

/// A failed channel expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Raised;

/// The configuration the listener reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IracingOptions {
    /// `APP/pause_telemetry_when_not_playing` (default `yes`).
    pub pause_when_not_playing: bool,
    /// `CUSTOM_FEATURES/dams_front_tl_mode` as an integer (default 0); `None` when the setting is not an integer, which makes
    /// the `steering_wheel_angle` read fail.
    pub dams_front_tl_mode: Option<i64>,
}

impl Default for IracingOptions {
    fn default() -> Self {
        IracingOptions { pause_when_not_playing: true, dams_front_tl_mode: Some(0) }
    }
}

/// `dams_front_tl_mode` selecting the steering-wheel angle instead of the lateral acceleration.
const DAMS_MODE_STEERING: i64 = 2;
/// Required DAMS setting value read by the steering getter.
pub type NativeDamsReader = std::sync::Arc<dyn Fn() -> Option<LegacyValue> + Send + Sync>;
/// Gravity removed from `VertAccel`.
const GRAVITY: f64 = 9.8;

/// What start-up consults before mapping the memory: the simulator status page and the data-valid event.
pub struct Simulator<S: SimStatus = HttpSimStatus, E: DataValidEvent = BridgeEvent> {
    pub status: S,
    pub data_valid: E,
}

impl Default for Simulator {
    fn default() -> Self {
        Simulator { status: HttpSimStatus::default(), data_valid: BridgeEvent::new() }
    }
}

/// Listener `iracing`.
pub struct Iracing<O: BlockOpener = LiveMirrors, S: SimStatus = HttpSimStatus, E: DataValidEvent = BridgeEvent> {
    opener: O,
    /// The map opened by the first successful start-up wait and kept until shutdown.
    shared_mem: Mutex<Option<Box<dyn BlockReader>>>,
    simulator: Simulator<S, E>,
    options: IracingOptions,
    pause_reader: Option<super::NativePauseReader>,
    dams_reader: Option<NativeDamsReader>,
    sdk: Mutex<Connection>,
    previous: PreviousSample,
    rates: MotionRates,
}

impl Iracing {
    /// Listener reading the bridge mirror and event relay and probing the simulator over HTTP.
    pub fn new(options: IracingOptions) -> Self {
        Iracing::with_parts(LiveMirrors, Simulator::default(), options)
    }
}

impl<O: BlockOpener, S: SimStatus, E: DataValidEvent> Iracing<O, S, E> {
    /// Listener reading the mapping `opener` opens and consulting `simulator` at startup.
    pub fn with_parts(opener: O, simulator: Simulator<S, E>, options: IracingOptions) -> Self {
        Iracing {
            opener,
            shared_mem: Mutex::new(None),
            simulator,
            options,
            pause_reader: None,
            dams_reader: None,
            sdk: Mutex::new(Connection::default()),
            previous: PreviousSample { speed: Some(0.0), rpm: Some(0.0) },
            rates: MotionRates::ZERO,
        }
    }

    /// Updates the configuration read by the channel getters.
    pub fn set_options(&mut self, options: IracingOptions) {
        self.options = options;
    }

    pub fn set_pause_reader(&mut self, reader: super::NativePauseReader) {
        self.pause_reader = Some(reader);
    }

    pub fn set_dams_reader(&mut self, reader: NativeDamsReader) {
        self.dams_reader = Some(reader);
    }

    /// The rate channels answered by the last [`Source::poll`] (`yaw_rate` in rad/s).
    pub fn motion_rates(&self) -> MotionRates {
        self.rates
    }

    /// Start-up: the status page, then `OpenEventW` and the 32 ms wait
    /// (failure returns false), then the read-only map, opened when it is not yet held
    /// (and kept), and the header of that memory. Returns true when
    /// the client became initialized. `Err` where start-up fails outright (and with it the
    /// liveness test): a status probe failure other than an unreachable server, or a failing map.
    fn startup(&mut self) -> Result<bool, DispatcherEnds> {
        let running = self.simulator.status.is_running().map_err(|Raised| {
            DispatcherEnds::new("iracing checkStatus: IRSDK._check_sim_status raised (not a URLError)")
        })?;
        if !running || !self.simulator.data_valid.open_and_wait() {
            return Ok(false);
        }
        if self.shared_mem.get_mut().unwrap_or_else(PoisonError::into_inner).is_none() {
            let opened = self.opener.create_read_only_held(MEMORY).map_err(|error| {
                DispatcherEnds::new(format!("iracing checkStatus: mmap of {} raised: {error}", MEMORY.name))
            })?;
            *self.shared_mem.get_mut().unwrap_or_else(PoisonError::into_inner) = Some(opened);
        }
        let initialized = {
            let memory = self.shared_mem.get_mut().unwrap_or_else(PoisonError::into_inner);
            let Some(memory) = memory.as_mut() else { return Ok(false) };
            self.sdk
                .get_mut()
                .unwrap_or_else(PoisonError::into_inner)
                .attach_live(memory.as_mut())
                .map_err(|_| DispatcherEnds::new("iracing startup: header read raised"))?
        };
        Ok(initialized)
    }

    /// The current bytes of the held map.
    fn memory(&self) -> Option<Vec<u8>> {
        self.shared_mem.lock().unwrap_or_else(PoisonError::into_inner).as_mut()?.read().ok().map(<[u8]>::to_vec)
    }

    fn live_value(&self, name: &str) -> Result<Value, Raised> {
        let mut sdk = self.sdk.lock().unwrap_or_else(PoisonError::into_inner);
        let mut memory = self.shared_mem.lock().unwrap_or_else(PoisonError::into_inner);
        sdk.get_live(memory.as_mut().ok_or(Raised)?.as_mut(), name)
    }

    /// The liveness test checks for a missing value and then reads a present value again before assigning it.
    fn remembered(&self, name: &str) -> Result<Option<f64>, Raised> {
        if self.live_value(name)?.is_none() {
            Ok(Some(0.0))
        } else {
            Ok(self.live_value(name)?.number().ok())
        }
    }

    fn connected_live(&self) -> Result<bool, Raised> {
        let mut sdk = self.sdk.lock().unwrap_or_else(PoisonError::into_inner);
        let mut memory = self.shared_mem.lock().unwrap_or_else(PoisonError::into_inner);
        match memory.as_mut() {
            Some(memory) => sdk.is_connected_live(memory.as_mut()),
            None => Ok(false),
        }
    }

    /// Shutdown, including closing and dropping the held map.
    fn shutdown(&mut self) {
        self.sdk.get_mut().unwrap_or_else(PoisonError::into_inner).shutdown();
        *self.shared_mem.get_mut().unwrap_or_else(PoisonError::into_inner) = None;
        self.simulator.data_valid.forget();
    }
}

impl<O: BlockOpener, S: SimStatus, E: DataValidEvent> Source for Iracing<O, S, E> {
    fn id(&self) -> &'static str {
        "iracing"
    }

    /// The liveness test: while initialized and connected, remember `Speed` and `RPM` for the
    /// slip estimate; otherwise try start-up, and shut down again on failure.
    ///
    /// A failure here is not handled (start-up's status probe or map, the `SessionNum` lookup of
    /// the connected test, a `Speed`/`RPM` read, which fails on a map with fewer than two variable
    /// buffers): it ends the dispatcher's loop, which is reported as [`DispatcherEnds`].
    fn is_running(&mut self) -> Result<bool, SourceError> {
        let raised = |what: &str| SourceError::from(DispatcherEnds::new(format!("iracing checkStatus: {what} raised")));
        if self.sdk.get_mut().unwrap_or_else(PoisonError::into_inner).initialized
            && self.connected_live().map_err(|Raised| raised("is_connected (SessionNum)"))?
        {
            self.previous.speed = self.remembered("Speed").map_err(|Raised| raised("game['Speed']"))?;
            self.previous.rpm = self.remembered("RPM").map_err(|Raised| raised("game['RPM']"))?;
            return Ok(true);
        }
        let running = self.startup()? && self.connected_live().map_err(|Raised| raised("is_connected (SessionNum)"))?;
        if !running {
            self.shutdown();
        }
        Ok(running)
    }

    fn receive_native(&mut self, _t: &mut Telemetry) -> Result<bool, SourceError> {
        let sdk = self.sdk.get_mut().unwrap_or_else(PoisonError::into_inner);
        sdk.frozen = None;
        let buffers = sdk.buffers.clone().ok_or_else(|| {
            DispatcherEnds::at_receive_data(
                "iracing receiveData: AttributeError: 'NoneType' object has no attribute 'var_buf'",
            )
        })?;
        self.simulator.data_valid.wait();
        let mut memory = self.shared_mem.lock().unwrap_or_else(PoisonError::into_inner);
        let memory = memory
            .as_mut()
            .ok_or_else(|| DispatcherEnds::at_receive_data("iracing receiveData: shared memory read raised"))?;
        let read = |memory: &mut dyn BlockReader, offset| {
            Connection::live_word(memory, offset)
                .map_err(|_| DispatcherEnds::at_receive_data("iracing receiveData: buffer property read raised"))
        };
        let mut order = Vec::with_capacity(buffers.count());
        for index in 0..buffers.count() {
            order.push((index, read(memory.as_mut(), 48 + index * 16)?));
        }
        order.sort_by_key(|&(_, tick)| std::cmp::Reverse(tick));
        let index = order
            .first()
            .ok_or_else(|| DispatcherEnds::at_receive_data("iracing receiveData: newest buffer lookup raised"))?
            .0;
        // The slice start and stop are evaluated separately.
        let start = i64::from(read(memory.as_mut(), 52 + index * 16)?);
        let stop = i64::from(read(memory.as_mut(), 52 + index * 16)?) + i64::from(buffers.length(index).unwrap());
        let bound = |value: i64| {
            (if value < 0 { MEMORY.size as i64 + value } else { value }).clamp(0, MEMORY.size as i64) as usize
        };
        let (start, stop) = (bound(start), bound(stop));
        let bytes = memory
            .read_range(start, stop.saturating_sub(start))
            .map_err(|_| DispatcherEnds::at_receive_data("iracing receiveData: buffer slice read raised"))?;
        self.sdk.get_mut().unwrap_or_else(PoisonError::into_inner).frozen = Some((index, bytes));
        Ok(true)
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        match self.read_native(name) {
            NativeRead::Value(value) => Some(value),
            _ => None,
        }
    }

    fn read_native(&self, name: &str) -> NativeRead {
        let mut sdk = self.sdk.lock().unwrap_or_else(PoisonError::into_inner);
        let mut frame = Frame {
            memory: &[],
            live_memory: Some(&self.shared_mem),
            sdk: &mut sdk,
            options: self.options,
            pause_reader: &self.pause_reader,
            dams_reader: &self.dams_reader,
            previous: self.previous,
            stored_none: false,
        };
        frame.native_answer(name).map_or(NativeRead::Raised, NativeRead::Value)
    }

    /// The receive step (freezing the latest buffer: the ≤ 32 ms wait on the data-valid
    /// event, then the newest data) and every channel read.
    fn poll(&mut self, t: &mut Telemetry) -> Result<bool, SourceError> {
        if self.sdk.get_mut().unwrap_or_else(PoisonError::into_inner).buffers.is_none() {
            // Freezing the latest buffer without a header fails the receive step
            // (unreachable through the dispatcher, which reads only after a successful liveness test).
            return Err(DispatcherEnds::at_receive_data(
                "iracing receiveData: AttributeError: 'NoneType' object has no attribute 'var_buf'",
            )
            .into());
        }
        self.simulator.data_valid.wait();
        let Some(memory) = self.memory() else { return Ok(false) };
        let mut frame = Frame {
            memory: &memory,
            live_memory: None,
            sdk: self.sdk.get_mut().unwrap_or_else(PoisonError::into_inner),
            options: self.options,
            pause_reader: &None,
            dams_reader: &None,
            previous: self.previous,
            stored_none: false,
        };
        self.rates = frame.write(t);
        if frame.stored_none {
            return Err(DispatcherEnds::new("gameListener: a channel answered None (TypeError comparing None)").into());
        }
        Ok(true)
    }
}

/// The previous speed and rpm: the values seen by the last connected liveness test, `None` when the value
/// was not a number (the slip estimate then fails).
#[derive(Debug, Clone, Copy)]
struct PreviousSample {
    speed: Option<f64>,
    rpm: Option<f64>,
}

/// The memory-map client's connection state the listener depends on.
///
/// The data-valid event needs no field of its own: start-up only sets the header after the event was opened and
/// signalled, and every failed liveness test runs shutdown, so the event term of the connected test holds whenever
/// `buffers` is set.
#[derive(Default)]
struct Connection {
    /// Whether start-up succeeded.
    initialized: bool,
    /// The variable buffers fixed by the last start-up; present while there is a header.
    buffers: Option<Buffers>,
    /// The connected-state workaround counter (not reset by shutdown).
    workaround_state: u8,
    /// The variable name table, read on first use.
    variables: Option<Variables>,
    /// The separately cached variable header positions, populated before the name table.
    variable_positions: Option<Vec<i64>>,
    /// The buffer selected when the latest buffer was frozen, and its private data copy.
    frozen: Option<(usize, Vec<u8>)>,
    session: SessionInfo,
}

impl Connection {
    fn live_word(memory: &mut dyn BlockReader, offset: usize) -> Result<i32, Raised> {
        Ok(i32::from_le_bytes(memory.read_range(offset, 4).map_err(|_| Raised)?.try_into().map_err(|_| Raised)?))
    }

    fn attach_live(&mut self, memory: &mut dyn BlockReader) -> Result<bool, Raised> {
        // The buffer count is read first, then the buffer length once for
        // every constructed buffer. The version is read afterwards.
        let count = usize::try_from(Self::live_word(memory, 32)?).unwrap_or(0);
        let mut lengths = Vec::new();
        for _ in 0..count {
            lengths.push(Self::live_word(memory, 36)?);
        }
        self.buffers = Some(Buffers::from_lengths(lengths));
        self.initialized = Self::live_word(memory, 0)? >= 1 && self.buffers.as_ref().is_some_and(|b| !b.is_empty());
        Ok(self.initialized)
    }

    fn is_connected_live(&mut self, memory: &mut dyn BlockReader) -> Result<bool, Raised> {
        if self.buffers.is_none() {
            return Ok(false);
        }
        if Self::live_word(memory, 4)? == STATUS_CONNECTED {
            self.workaround_state = 0;
        }
        if self.workaround_state == 0 && Self::live_word(memory, 4)? != STATUS_CONNECTED {
            self.workaround_state = 1;
        }
        if self.workaround_state == 1 && self.get_live(memory, "SessionNum")?.is_none() {
            self.workaround_state = 2;
        }
        if self.workaround_state == 2 && !self.get_live(memory, "SessionNum")?.is_none() {
            self.workaround_state = 3;
        }
        Ok(Self::live_word(memory, 4)? == STATUS_CONNECTED || self.workaround_state == 3)
    }

    fn get_live(&mut self, memory: &mut dyn BlockReader, name: &str) -> Result<Value, Raised> {
        let buffers = self.buffers.clone().ok_or(Raised)?;
        if self.variables.is_none() {
            // Both caches are assigned before their loops; a late failure leaves
            // the completed prefix and does not retry construction on the next lookup.
            self.variables = Some(Variables::default());
            if self.variable_positions.is_none() {
                self.variable_positions = Some(Vec::new());
                let count = usize::try_from(Self::live_word(memory, 24)?).unwrap_or(0);
                for index in 0..count {
                    let base = i64::from(Self::live_word(memory, 28)?);
                    self.variable_positions.as_mut().ok_or(Raised)?.push(base + index as i64 * 144);
                }
            }
            for &at in self.variable_positions.as_ref().ok_or(Raised)? {
                let offset =
                    Variables::absolute_offset(MEMORY.size, at.checked_add(16).ok_or(Raised)?, 32).ok_or(Raised)?;
                let bytes = memory.read_range(offset, 32).map_err(|_| Raised)?;
                self.variables.as_mut().ok_or(Raised)?.insert_name(&bytes, at);
            }
        }
        let variables = self.variables.as_ref().ok_or(Raised)?;
        if let Some(at) = variables.header_position(name) {
            let mut order = Vec::with_capacity(buffers.count());
            for index in 0..buffers.count() {
                order.push((index, Self::live_word(memory, 48 + index * 16)?));
            }
            order.sort_by_key(|&(_, tick)| std::cmp::Reverse(tick));
            let selected = order.get(1).ok_or(Raised)?.0;
            let read_word = |memory: &mut dyn BlockReader, relative: i64| -> Result<i32, Raised> {
                let offset = Variables::absolute_offset(MEMORY.size, at.checked_add(relative).ok_or(Raised)?, 4)
                    .ok_or(Raised)?;
                let bytes = memory.read_range(offset, 4).map_err(|_| Raised)?;
                Ok(i32::from_le_bytes(bytes.try_into().map_err(|_| Raised)?))
            };
            let kind = read_word(memory, 0)?;
            // An unknown type code fails before the count is read.
            Variables::value_size(kind, 0).ok_or(Raised)?;
            let count = read_word(memory, 8)?;
            let size = Variables::value_size(kind, count).ok_or(Raised)?;
            let value = if let Some((_, bytes)) = self.frozen.as_ref().filter(|(index, _)| *index == selected) {
                let offset = read_word(memory, 4)?;
                let start = Variables::absolute_offset(bytes.len(), i64::from(offset), size).ok_or(Raised)?;
                Variables::decode_value(kind, count, bytes.get(start..start + size).ok_or(Raised)?).ok_or(Raised)?
            } else {
                let buffer_offset = Self::live_word(memory, 52 + selected * 16)?;
                let offset = read_word(memory, 4)?;
                let start = i64::from(buffer_offset).checked_add(i64::from(offset)).ok_or(Raised)?;
                let bytes = memory
                    .read_range(Variables::absolute_offset(MEMORY.size, start, size).ok_or(Raised)?, size)
                    .map_err(|_| Raised)?;
                Variables::decode_value(kind, count, &bytes).ok_or(Raised)?
            };
            return Variables::reshape(value, read_word(memory, 8)?).map(Value::Var).ok_or(Raised);
        }
        let update = Self::live_word(memory, 12)?;
        if self.session.needs_update(update) {
            self.session.advance(Self::live_word(memory, 12)?);
        }
        if let Some(value) = self.session.cached(name) {
            return Ok(Value::Doc(value));
        }
        let region = self::session_info::SessionRegion {
            offset: Self::live_word(memory, 20)?,
            len: Self::live_word(memory, 16)?,
            update,
        };
        let (start, len) = region.bounds(MEMORY.size);
        let bytes = memory.read_range(start, len).map_err(|_| Raised)?;
        let region = self::session_info::SessionRegion { offset: 0, len: len as i32, update };
        Ok(match self.session.lookup_current(&bytes, region, name)? {
            None | Some(Yaml::Null) => Value::None,
            Some(value) => Value::Doc(value),
        })
    }

    /// Shutdown.
    fn shutdown(&mut self) {
        self.initialized = false;
        self.buffers = None;
        self.variables = None;
        self.variable_positions = None;
        self.frozen = None;
        self.session.clear();
    }

    /// A lookup by name: a telemetry variable from the second-newest buffer, otherwise the
    /// session-info block of that name.
    fn get(&mut self, memory: &[u8], name: &str) -> Result<Value, Raised> {
        let buffers = self.buffers.clone().ok_or(Raised)?;
        let header = Header::read(memory).ok_or(Raised)?;
        let variables = match &mut self.variables {
            Some(variables) => variables,
            empty => {
                let (variables, complete) = Variables::read(memory, &header);
                let variables = empty.insert(variables);
                if !complete {
                    return Err(Raised);
                }
                variables
            }
        };
        if variables.contains(name) {
            let selected = buffers.second_newest_index(memory).ok_or(Raised)?;
            if let Some((index, bytes)) = &self.frozen {
                if *index == selected {
                    return variables.value_from(memory, bytes, 0, name).map(Value::Var).ok_or(Raised);
                }
            }
            let offset = buffers.second_newest_offset(memory).ok_or(Raised)?;
            return variables.value(memory, offset, name).map(Value::Var).ok_or(Raised);
        }
        Ok(match self.session.lookup(memory, header.session, name)? {
            None | Some(Yaml::Null) => Value::None,
            Some(document) => Value::Doc(document),
        })
    }
}

/// A value returned by a lookup by name.
#[derive(Debug, Clone, PartialEq)]
enum Value {
    /// No value.
    None,
    Var(VarValue),
    /// A parsed session-info value.
    Doc(Yaml),
}

impl Value {
    fn is_none(&self) -> bool {
        matches!(self, Value::None)
    }

    /// Truthiness: false for no value, zero and empty text or containers.
    fn truthy(&self) -> bool {
        match self {
            Value::None => false,
            Value::Var(VarValue::One(Scalar::Char(_))) => true,
            Value::Var(VarValue::One(scalar)) => scalar.number().is_some_and(|n| n != 0.0),
            Value::Var(VarValue::Many(items)) => !items.is_empty(),
            Value::Doc(document) => truthy(document),
        }
    }

    /// The value as a number in arithmetic, comparisons and floating-point arguments; `Raised` (a type error) for
    /// anything else.
    fn number(&self) -> Result<f64, Raised> {
        match self {
            Value::Var(VarValue::One(scalar)) => scalar.number().ok_or(Raised),
            Value::Doc(document) => yaml_number(document).ok_or(Raised),
            _ => Err(Raised),
        }
    }

    /// The value as an integer (for bit tests and integer arguments): booleans and integers only.
    fn integer(&self) -> Result<i64, Raised> {
        match self {
            Value::Var(VarValue::One(Scalar::Int(i))) => Ok(*i),
            Value::Var(VarValue::One(Scalar::Bool(b))) => Ok(i64::from(*b)),
            Value::Doc(Yaml::Integer(i)) => Ok(*i),
            Value::Doc(Yaml::Boolean(b)) => Ok(i64::from(*b)),
            _ => Err(Raised),
        }
    }

    /// Equality with an integer constant (false for non-numbers).
    fn equals(&self, constant: i64) -> bool {
        self.number().is_ok_and(|n| n == constant as f64)
    }
}

/// The value of a channel expression, as the dispatcher received it.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Answer {
    Number(f64),
    /// No value, which the dispatcher stores as is; its min/max update then fails.
    Nothing,
    /// A non-numeric object (list, mapping, string, bytes), which the dispatcher stores as is.
    Object,
    Raised,
}

impl Answer {
    /// A looked-up value that is present.
    fn returned(value: Value) -> Answer {
        match value.number() {
            Ok(n) => Answer::Number(n),
            Err(Raised) => Answer::Object,
        }
    }

    /// The value, or 0 when there is none.
    fn or_zero(value: Value) -> Answer {
        if value.is_none() {
            Answer::Number(0.0)
        } else {
            Answer::returned(value)
        }
    }

    /// An arithmetic result.
    fn computed(result: Result<f64, Raised>) -> Answer {
        result.map_or(Answer::Raised, Answer::Number)
    }

    /// The answer passed on as a floating-point argument (no value is a type error).
    fn as_argument(self) -> Result<f64, Raised> {
        match self {
            Answer::Number(n) => Ok(n),
            Answer::Nothing | Answer::Object | Answer::Raised => Err(Raised),
        }
    }

    /// Stores the answer as the dispatcher does: a number is written, a failed expression writes `fallback` when
    /// there is one. No value and other objects are stored as they are, which `Telemetry` cannot hold: the slot keeps
    /// its previous value, and the result tells whether the stored value was `None` (see [`Frame::stored_none`]).
    #[must_use]
    fn store(self, slot: &mut f64, fallback: Option<f64>) -> bool {
        match (self, fallback) {
            (Answer::Number(n), _) => *slot = n,
            (Answer::Raised, Some(fallback)) => *slot = fallback,
            (Answer::Nothing, _) => return true,
            _ => {}
        }
        false
    }
}

/// The engine-off test sitting in the middle of the channel chain: `Speed` and `RPM` present,
/// `RPM < 400` and `Speed < 1` make every later channel answer 0.
type EngineOff = Result<bool, Raised>;

/// One round of channel reads over the current memory.
struct Frame<'a> {
    memory: &'a [u8],
    live_memory: Option<&'a Mutex<Option<Box<dyn BlockReader>>>>,
    sdk: &'a mut Connection,
    options: IracingOptions,
    pause_reader: &'a Option<super::NativePauseReader>,
    dams_reader: &'a Option<NativeDamsReader>,
    previous: PreviousSample,
    /// Whether a channel the dispatcher stores as it comes (`speed`, `yaw`, `lateral_velocity` in DAMS mode 0, and
    /// every registry channel without `round`) answered `None`. The dispatcher's min/max update then compares
    /// `None`, which fails
    /// and ends its main loop; the top-speed update fails for a `None` speed as well.
    stored_none: bool,
}

/// The registry default of channels whose expression may fail.
const ZERO_DEFAULT: Option<f64> = Some(0.0);

impl Frame<'_> {
    fn get(&mut self, name: &str) -> Result<Value, Raised> {
        if let Some(memory) = self.live_memory {
            let mut memory = memory.lock().unwrap_or_else(PoisonError::into_inner);
            return self.sdk.get_live(memory.as_mut().ok_or(Raised)?.as_mut(), name);
        }
        self.sdk.get(self.memory, name)
    }

    /// Writes every requested channel and returns the rate channels.
    fn write(&mut self, t: &mut Telemetry) -> MotionRates {
        match self.paused() {
            Err(Raised) => {
                write_all_raised(t);
                return MotionRates::RAISED;
            }
            Ok(true) => {
                write_all_zero(t);
                return MotionRates::ZERO;
            }
            Ok(false) => {}
        }
        let off = self.engine_off();
        self.write_before_gate(t, off);
        self.write_motion(t, off);
        self.write_corners(t, off);
        self.write_flags_and_fluids(t, off);
        self.write_location(t, off);
        let yaw_rate = self.after_gate(off, |f| Ok(Answer::or_zero(f.get("YawRate")?)));
        let others = self.after_gate(off, |_| Ok(Answer::Number(0.0)));
        MotionRates { yaw_rate: number(yaw_rate), pitch_rate: number(others), roll_rate: number(others) }
    }

    /// The preamble run for every channel: replay, or not in the car, answers 0 while pausing is enabled.
    fn paused(&mut self) -> Result<bool, Raised> {
        let pause = self.options.pause_when_not_playing;
        let replay = self.get("IsReplayPlaying")?;
        if !replay.is_none() && replay.truthy() && pause {
            return Ok(true);
        }
        let on_track = self.get("IsOnTrackCar")?;
        Ok(pause && !on_track.truthy())
    }

    /// `Speed` and `RPM` are both present, `RPM` is below 400 and `Speed` is below 1.
    fn engine_off(&mut self) -> EngineOff {
        let speed = self.get("Speed")?;
        if speed.is_none() {
            return Ok(false);
        }
        let rpm = self.get("RPM")?;
        if rpm.is_none() {
            return Ok(false);
        }
        Ok(rpm.number()? < 400.0 && speed.number()? < 1.0)
    }

    /// A channel placed after the engine-off test.
    fn after_gate(&mut self, off: EngineOff, expression: impl FnOnce(&mut Self) -> Result<Answer, Raised>) -> Answer {
        match off {
            Err(Raised) => Answer::Raised,
            Ok(true) => Answer::Number(0.0),
            Ok(false) => expression(self).unwrap_or(Answer::Raised),
        }
    }

    fn present(&mut self, name: &str) -> Result<Answer, Raised> {
        let value = self.get(name)?;
        Ok(if value.is_none() { Answer::Nothing } else { Answer::returned(value) })
    }

    /// The channels before the engine-off test.
    fn write_before_gate(&mut self, t: &mut Telemetry, off: EngineOff) {
        self.stored_none |= self.present("Speed").unwrap_or(Answer::Raised).store(&mut t.speed, None);
        self.stored_none |= self.wheel_slip(WheelSide::Left, off).store(&mut t.wheel_slip_lf, None);
        self.stored_none |= self.wheel_slip(WheelSide::Left, off).store(&mut t.wheel_slip_lr, None);
        self.stored_none |= self.wheel_slip(WheelSide::Right, off).store(&mut t.wheel_slip_rr, None);
        self.stored_none |= self.wheel_slip(WheelSide::Right, off).store(&mut t.wheel_slip_rf, None);
        self.stored_none |= self.present("Gear").unwrap_or(Answer::Raised).store(&mut t.gear, None);
        self.stored_none |= self.max_rpm().unwrap_or(Answer::Raised).store(&mut t.max_rpm, None);
        if let Ok(Some(vehicle)) = self.car_name() {
            t.vehicle = vehicle;
        }
        // The loader's rounding fails for a missing value, which therefore also writes the default 0.
        match self.steering().unwrap_or(Answer::Raised) {
            Answer::Number(n) => t.steering_wheel_angle = n,
            Answer::Nothing | Answer::Object | Answer::Raised => t.steering_wheel_angle = 0.0,
        }
    }

    /// `wheel_slip_*`: the estimated slip of one side over the nested channel reads.
    fn wheel_slip(&mut self, side: WheelSide, off: EngineOff) -> Answer {
        Answer::computed(self.slip_inputs(off).map(|inputs| estimated_wheel_slip(side, &inputs)))
    }

    /// The inputs of the slip estimate: the `lateral_velocity`, `speed`, `rpm` and `gear` channels, the
    /// remembered speed and rpm, then `throttle`, `brake` and `clutch`; a missing or non-numeric value is an
    /// error.
    fn slip_inputs(&mut self, off: EngineOff) -> Result<SlipInputs, Raised> {
        let lateral_velocity = self.lateral_velocity(off).as_argument()?;
        let speed = self.present("Speed")?.as_argument()?;
        let rpm = self.after_gate(off, |f| f.present("RPM")).as_argument()?;
        let gear = self.get("Gear")?.integer()?;
        let previous_speed = self.previous.speed.ok_or(Raised)?;
        let previous_rpm = self.previous.rpm.ok_or(Raised)?;
        let throttle = self.mapped(off, "Throttle").as_argument()?;
        let brake = self.mapped(off, "Brake").as_argument()?;
        self.mapped(off, "Clutch").as_argument()?;
        Ok(SlipInputs { lateral_velocity, speed, rpm, gear, previous_speed, previous_rpm, throttle, brake })
    }

    /// `max_rpm`: `DriverInfo.DriverCarSLBlinkRPM` when `DriverInfo` is truthy.
    fn max_rpm(&mut self) -> Result<Answer, Raised> {
        let driver_info = self.get("DriverInfo")?;
        if !driver_info.truthy() {
            return Ok(Answer::Nothing);
        }
        loader_number(index(&driver_info, "DriverCarSLBlinkRPM")?)
    }

    /// `car_name`: the `CarScreenName` of the driver whose `CarIdx` is `DriverCarIdx`, `'-1'` when none matches;
    /// `Ok(None)` when `DriverInfo` is missing.
    ///
    /// `Drivers` may be any iterable value: an empty list, mapping or string ends the search at once
    /// (`'-1'`); a non-empty mapping or string yields keys or characters, which have no `CarIdx` (an error); no
    /// value and numbers cannot be iterated (an error).
    fn car_name(&mut self) -> Result<Option<String>, Raised> {
        let driver_info = self.get("DriverInfo")?;
        if !driver_info.truthy() {
            return Ok(None);
        }
        let car_index = index(&driver_info, "DriverCarIdx")?;
        let drivers = match index(&driver_info, "Drivers")? {
            Yaml::Array(drivers) => drivers.as_slice(),
            Yaml::Hash(entries) if entries.is_empty() => &[],
            Yaml::String(text) if text.is_empty() => &[],
            _ => return Err(Raised),
        };
        for driver in drivers {
            let Yaml::Hash(fields) = driver else { return Err(Raised) };
            let idx = fields.get(&Yaml::String("CarIdx".to_owned())).ok_or(Raised)?;
            if yaml_equal(idx, car_index) {
                let name = fields.get(&Yaml::String("CarScreenName".to_owned())).ok_or(Raised)?;
                return Ok(yaml_text(name));
            }
        }
        Ok(Some("-1".to_owned()))
    }

    /// `steering_wheel_angle`: `SteeringWheelAngle` (rad, 0 when missing) in DAMS mode 2, otherwise `LatAccel`.
    fn steering(&mut self) -> Result<Answer, Raised> {
        match self.options.dams_front_tl_mode {
            None => Err(Raised),
            Some(DAMS_MODE_STEERING) => Ok(Answer::or_zero(self.get("SteeringWheelAngle")?)),
            Some(_) => self.present("LatAccel"),
        }
    }

    fn lateral_velocity(&mut self, off: EngineOff) -> Answer {
        self.after_gate(off, |f| {
            let value = f.get("VelocityY")?;
            Ok(if value.is_none() { Answer::Nothing } else { Answer::computed(value.number().map(|v| -v)) })
        })
    }

    /// A channel of the mapping table: the mapped variable, `0` when missing.
    fn mapped(&mut self, off: EngineOff, name: &str) -> Answer {
        self.after_gate(off, |f| Ok(Answer::or_zero(f.get(name)?)))
    }

    /// Velocities, accelerations and angles.
    fn write_motion(&mut self, t: &mut Telemetry, off: EngineOff) {
        self.stored_none |= self.present_gated(off, "RPM").store(&mut t.rpm, None);
        // DAMS mode 0 stores the lateral velocity as it comes; modes 1 and 2
        // compute from the value instead.
        let stored_none = self.lateral_velocity(off).store(&mut t.lateral_velocity, ZERO_DEFAULT);
        self.stored_none |= stored_none && self.options.dams_front_tl_mode == Some(0);
        // The test is on VelocityZ; the value returned is VelocityX.
        self.stored_none |= self
            .after_gate(
                off,
                |f| if f.get("VelocityZ")?.is_none() { Ok(Answer::Nothing) } else { f.present("VelocityX") },
            )
            .store(&mut t.longitudinal_velocity, None);
        self.stored_none |= self.present_gated(off, "VelocityX").store(&mut t.vertical_velocity, None);
        self.stored_none |= self
            .after_gate(off, |f| Ok(Answer::Number(f.acceleration("VertAccel") - GRAVITY)))
            .store(&mut t.vertical_acceleration, None);
        self.stored_none |= self
            .after_gate(off, |f| Ok(Answer::Number(f.acceleration("LatAccel"))))
            .store(&mut t.lateral_acceleration, None);
        self.stored_none |= self
            .after_gate(off, |f| Ok(Answer::Number(f.acceleration("LongAccel"))))
            .store(&mut t.longitudinal_acceleration, None);
        self.stored_none |= self.angle(off, "Yaw", -1.0).store(&mut t.yaw, ZERO_DEFAULT);
        self.stored_none |= self.angle(off, "Roll", 1.0).store(&mut t.roll, ZERO_DEFAULT);
        self.stored_none |= self.angle(off, "Pitch", -1.0).store(&mut t.pitch, ZERO_DEFAULT);
    }

    fn present_gated(&mut self, off: EngineOff, name: &str) -> Answer {
        self.after_gate(off, |f| f.present(name))
    }

    /// The named angle in degrees times `sign`; a missing value is an error.
    fn angle(&mut self, off: EngineOff, name: &str, sign: f64) -> Answer {
        self.after_gate(off, |f| Ok(Answer::computed(f.get(name)?.number().map(|radians| rad_to_deg(radians) * sign))))
    }

    /// An acceleration: the named value as a float, element 0 of a list (360 Hz logging),
    /// `0` when neither works.
    fn acceleration(&mut self, name: &str) -> f64 {
        match self.get(name) {
            Ok(Value::Var(VarValue::One(scalar))) => scalar_float(scalar),
            Ok(Value::Var(VarValue::Many(items))) => first_element_float(&items),
            Ok(Value::Doc(document)) => yaml_float(&document),
            Ok(Value::None) | Err(Raised) => 0.0,
        }
    }

    /// Suspension, rumble, tyre temperature and wear channels.
    fn write_corners(&mut self, t: &mut Telemetry, off: EngineOff) {
        self.stored_none |= self
            .first_present(off, ["LFshockDefl", "RFshockDefl", "LRshockDefl"])
            .store(&mut t.suspension_travel_lf, None);
        self.stored_none |= self
            .first_present(off, ["LRshockDefl", "RRshockDefl", "LFshockDefl"])
            .store(&mut t.suspension_travel_lr, None);
        self.stored_none |= self
            .first_present(off, ["RFshockDefl", "LFshockDefl", "RRshockDefl"])
            .store(&mut t.suspension_travel_rf, None);
        self.stored_none |= self
            .first_present(off, ["RRshockDefl", "LRshockDefl", "RFshockDefl"])
            .store(&mut t.suspension_travel_rr, None);
        self.stored_none |= self
            .first_present(off, ["LFshockVel", "RFshockVel", "LRshockVel"])
            .store(&mut t.suspension_velocity_lf, None);
        self.stored_none |= self
            .first_present(off, ["LRshockVel", "RRshockVel", "LFshockVel"])
            .store(&mut t.suspension_velocity_lr, None);
        self.stored_none |= self
            .first_present(off, ["RFshockVel", "LFshockVel", "RRshockVel"])
            .store(&mut t.suspension_velocity_rf, None);
        self.stored_none |= self
            .first_present(off, ["RRshockVel", "LRshockVel", "RFshockVel"])
            .store(&mut t.suspension_velocity_rr, None);
        self.stored_none |= self.present_gated(off, "TireRF_RumblePitch").store(&mut t.rumble_pitch_rf, ZERO_DEFAULT);
        self.stored_none |= self.present_gated(off, "TireLF_RumblePitch").store(&mut t.rumble_pitch_lf, ZERO_DEFAULT);
        self.stored_none |= self.present_gated(off, "TireLR_RumblePitch").store(&mut t.rumble_pitch_lr, ZERO_DEFAULT);
        self.stored_none |= self.present_gated(off, "TireRR_RumblePitch").store(&mut t.rumble_pitch_rr, ZERO_DEFAULT);
        for (corner, temperature, wear) in [
            ("LF", &mut t.tyre_temperature_lf, &mut t.tyre_wear_lf),
            ("RF", &mut t.tyre_temperature_rf, &mut t.tyre_wear_rf),
            ("LR", &mut t.tyre_temperature_lr, &mut t.tyre_wear_lr),
            ("RR", &mut t.tyre_temperature_rr, &mut t.tyre_wear_rr),
        ] {
            self.stored_none |=
                self.tyre_minimum(off, corner, ["tempCM", "tempCR", "tempCL"]).store(temperature, ZERO_DEFAULT);
            self.stored_none |= self.tyre_minimum(off, corner, ["wearM", "wearR", "wearL"]).store(wear, ZERO_DEFAULT);
        }
    }

    /// The first of the named values that is present, else 0.
    fn first_present(&mut self, off: EngineOff, names: [&str; 3]) -> Answer {
        self.after_gate(off, |f| {
            for name in names {
                let value = f.get(name)?;
                if !value.is_none() {
                    return Ok(Answer::returned(value));
                }
            }
            Ok(Answer::Number(0.0))
        })
    }

    /// The tyre channels: the minimum of the three readings `[first, second, third]` (for example `LFtempCM`,
    /// `LFtempCR`, `LFtempCL`), taken in this order: the smaller of the first and the third (the third on a
    /// tie), then the smaller of the second and that (that one on a tie).
    fn tyre_minimum(&mut self, off: EngineOff, corner: &str, suffixes: [&str; 3]) -> Answer {
        self.after_gate(off, |f| {
            let [first, second, third] = suffixes.map(|suffix| format!("{corner}{suffix}"));
            let first = f.get(&first)?.number()?;
            let second = f.get(&second)?.number()?;
            let third = f.get(&third)?.number()?;
            let low = if first < third { first } else { third };
            Ok(Answer::Number(if second < low { second } else { low }))
        })
    }

    /// Flags, spotters, ABS, pressures and the mapping-table channels.
    fn write_flags_and_fluids(&mut self, t: &mut Telemetry, off: EngineOff) {
        self.stored_none |= self.present_gated(off, "SessionFlags").store(&mut t.raw_flag, Some(RAW_FLAG_DEFAULT));
        self.stored_none |= self
            .after_gate(off, |f| Ok(Answer::Number(normalized_flag(f.get("SessionFlags")?.integer()?))))
            .store(&mut t.flag, None);
        self.stored_none |= self.spotter(off, [2, 4, 6]).store(&mut t.spotter_left, ZERO_DEFAULT);
        self.stored_none |= self.spotter(off, [3, 4, 6]).store(&mut t.spotter_right, ZERO_DEFAULT);
        self.stored_none |= self
            .after_gate(off, |f| Ok(Answer::computed(python_int(&f.get("BrakeABSactive")?))))
            .store(&mut t.abs_lock, ZERO_DEFAULT);
        self.stored_none |= self.hundredfold(off, "OilPress").store(&mut t.oil_pressure, ZERO_DEFAULT);
        self.stored_none |= self.hundredfold(off, "ManifoldPress").store(&mut t.turbo_boost_pressure, ZERO_DEFAULT);
        for (name, slot) in [
            ("Lap", &mut t.lap),
            ("LapCompleted", &mut t.lap_completed),
            ("LapDist", &mut t.lap_dist),
            ("LapDistPct", &mut t.lap_dist_percent),
            ("RaceLaps", &mut t.race_laps),
            ("OilTemp", &mut t.oil_temperature),
            ("OilLevel", &mut t.oil_level),
            ("WaterTemp", &mut t.water_temperature),
            ("WaterLevel", &mut t.water_level),
            ("FuelPress", &mut t.fuel_pressure),
            ("FuelLevelPct", &mut t.fuel_level),
        ] {
            self.stored_none |= self.mapped(off, name).store(slot, ZERO_DEFAULT);
        }
        // Channels without an expression: the channel name is no variable or session block, so `0`.
        for slot in [
            &mut t.tyre_pressure_lf,
            &mut t.tyre_pressure_rf,
            &mut t.tyre_pressure_lr,
            &mut t.tyre_pressure_rr,
            &mut t.water_pressure,
            &mut t.canopy,
            &mut t.afterburner,
            &mut t.landing_gear_left,
            &mut t.landing_gear_nose,
            &mut t.landing_gear_right,
            &mut t.turbulence,
            &mut t.touchdown_left,
            &mut t.touchdown_nose,
            &mut t.touchdown_right,
        ] {
            self.stored_none |= self.after_gate(off, |_| Ok(Answer::Number(0.0))).store(slot, ZERO_DEFAULT);
        }
    }

    /// `spotter_*`: `1` when `CarLeftRight` is one of `values`.
    fn spotter(&mut self, off: EngineOff, values: [i64; 3]) -> Answer {
        self.after_gate(off, |f| {
            let side = f.get("CarLeftRight")?;
            Ok(Answer::Number(if values.iter().any(|&v| side.equals(v)) { 1.0 } else { 0.0 }))
        })
    }

    /// The named value times 100 (bar to kPa), `0` when missing.
    fn hundredfold(&mut self, off: EngineOff, name: &str) -> Answer {
        self.after_gate(off, |f| {
            let value = f.get(name)?;
            Ok(if value.is_none() { Answer::Number(0.0) } else { Answer::computed(value.number().map(|v| v * 100.0)) })
        })
    }

    /// `track_loc`: `WeekendInfo.TrackDisplayShortName`, `0` on any error or without `WeekendInfo`.
    fn write_location(&mut self, t: &mut Telemetry, off: EngineOff) {
        let location = match off {
            Ok(false) => self.track_name().unwrap_or_else(|Raised| Some(ZERO_NAME.to_owned())),
            Ok(true) | Err(Raised) => Some(ZERO_NAME.to_owned()),
        };
        if let Some(location) = location {
            t.location = location;
        }
    }

    fn track_name(&mut self) -> Result<Option<String>, Raised> {
        let weekend = self.get("WeekendInfo")?;
        if !weekend.truthy() {
            return Ok(Some(ZERO_NAME.to_owned()));
        }
        Ok(yaml_text(index(&weekend, "TrackDisplayShortName")?))
    }
}

/// The `flag` channel: the flag of the first matching `SessionFlags` bit, in table order.
fn normalized_flag(flags: i64) -> f64 {
    const ORDER: [(i64, f64); 19] = [
        (1024, 1.0),           // green_held
        (32, 2.0),             // blue
        (2, 3.0),              // white
        (16, 5.0),             // red
        (8, 6.0),              // yellow
        (1_048_576, 9.0),      // repair
        (65_536, 10.0),        // black
        (1, 11.0),             // checkered
        (4, 1.0),              // green
        (256, 13.0),           // yellow_waving
        (1 << 30, 20.0),       // start_set
        (1 << 29, 21.0),       // start_ready
        (1 << 31, 22.0),       // start_go
        (128 | 131_072, 30.0), // crossed or disqualify
        (64, 31.0),            // debris
        (512, 32.0),           // one_lap_to_green
        (32_768, 34.0),        // caution_waving
        (524_288, 35.0),       // furled
        (16_384, 36.0),        // caution
    ];
    ORDER.iter().find(|(bits, _)| flags & bits != 0).map_or(0.0, |&(_, flag)| flag)
}

/// A variable or session value as an integer.
fn python_int(value: &Value) -> Result<f64, Raised> {
    match value {
        Value::Var(VarValue::One(Scalar::Float(f))) => truncate_to_int(*f).ok_or(Raised),
        Value::Doc(Yaml::Real(_)) => value.number().and_then(|f| truncate_to_int(f).ok_or(Raised)),
        _ => value.integer().map(|i| i as f64),
    }
}

fn number(answer: Answer) -> Option<f64> {
    match answer {
        Answer::Number(n) => Some(n),
        Answer::Nothing | Answer::Object | Answer::Raised => None,
    }
}

/// A key lookup on a session-info mapping (an error for a missing key or a non-mapping).
fn index<'v>(value: &'v Value, key: &str) -> Result<&'v Yaml, Raised> {
    match value {
        Value::Doc(Yaml::Hash(map)) => map.get(&Yaml::String(key.to_owned())).ok_or(Raised),
        _ => Err(Raised),
    }
}

/// The generic loader's treatment of a YAML value: numbers are stored, strings are converted to integers, no value stays.
fn loader_number(value: &Yaml) -> Result<Answer, Raised> {
    match value {
        Yaml::String(text) => python_int_literal(text).map(Answer::Number).ok_or(Raised),
        Yaml::Null => Ok(Answer::Nothing),
        other => Ok(yaml_number(other).map_or(Answer::Object, Answer::Number)),
    }
}

/// Integer from decimal text (surrounding whitespace, sign, `_` between digits).
fn python_int_literal(text: &str) -> Option<f64> {
    let trimmed = text.trim();
    let digits = trimmed.strip_prefix(['+', '-']).unwrap_or(trimmed);
    let valid = !digits.is_empty()
        && !digits.starts_with('_')
        && !digits.ends_with('_')
        && !digits.contains("__")
        && digits.chars().all(|c| c.is_ascii_digit() || c == '_');
    valid.then(|| trimmed.replace('_', "").parse::<f64>().ok()).flatten()
}

fn yaml_number(value: &Yaml) -> Option<f64> {
    match value {
        Yaml::Integer(i) => Some(*i as f64),
        Yaml::Real(_) => value.as_f64(),
        Yaml::Boolean(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// A session value as a float, then its first element as a float, for an acceleration (0 where both fail).
fn yaml_float(value: &Yaml) -> f64 {
    match value {
        Yaml::String(text) => text_float(text),
        Yaml::Array(items) => match items.first() {
            Some(Yaml::String(text)) => text.trim().parse().unwrap_or(0.0),
            Some(first) => yaml_number(first).unwrap_or(0.0),
            None => 0.0,
        },
        other => yaml_number(other).unwrap_or(0.0),
    }
}

/// Text as a float, falling back to its first character as a float.
fn text_float(text: &str) -> f64 {
    text.trim().parse().ok().or_else(|| text.chars().next().and_then(|c| c.to_digit(10)).map(f64::from)).unwrap_or(0.0)
}

/// A scalar variable as a float; a character that is no digit falls back to its byte value.
fn scalar_float(scalar: Scalar) -> f64 {
    match scalar {
        Scalar::Char(byte) => char::from(byte).to_digit(10).map_or(f64::from(byte), f64::from),
        other => other.number().unwrap_or(0.0),
    }
}

/// Element 0 of a list variable (a 360 Hz array) as a float.
fn first_element_float(items: &[Scalar]) -> f64 {
    match items.first() {
        Some(Scalar::Char(byte)) => char::from(*byte).to_digit(10).map_or(0.0, f64::from),
        Some(other) => other.number().unwrap_or(0.0),
        None => 0.0,
    }
}

/// Equality of two session values (numbers compare by value).
fn yaml_equal(a: &Yaml, b: &Yaml) -> bool {
    match (yaml_number(a), yaml_number(b)) {
        (Some(x), Some(y)) => x == y,
        _ => a == b,
    }
}

/// A session value used as a name: strings as they are, numbers and booleans in their printed form; `None` for
/// `None` and containers (left unchanged).
fn yaml_text(value: &Yaml) -> Option<String> {
    match value {
        Yaml::String(text) => Some(text.clone()),
        Yaml::Integer(i) => Some(i.to_string()),
        Yaml::Boolean(b) => Some(if *b { "True" } else { "False" }.to_owned()),
        Yaml::Real(_) => value.as_f64().map(python_float_text),
        _ => None,
    }
}

/// The printed form of a float, for the values that occur in names.
fn python_float_text(value: f64) -> String {
    if value.is_finite() && value == value.trunc() && value.abs() < 1e16 {
        format!("{value:.1}")
    } else if value.is_nan() {
        "nan".to_owned()
    } else if value.is_infinite() {
        if value > 0.0 { "inf" } else { "-inf" }.to_owned()
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests;
