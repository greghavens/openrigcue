//! Joystick motion: a joystick or wheel drives the rig directly (listener `joy`).
//!
//! A profile assigns controller inputs to seven controls (pitch front/back, roll left/right, yaw left/right,
//! speed). A mapping thread maps the controller state of the latest input pass
//! into deflections of -10..10 (speed 0..100) on its own period, and the channel getters read
//! the current deflections. Production controller state comes from [`JoystickInputs`], fed by SDL2
//! ([`sdl`]) on the joystick polling worker. The evdev reader remains a compatibility helper.

pub mod evdev;
pub mod page;
mod raw;
pub mod sdl;

use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex, PoisonError, Weak};
use std::thread::JoinHandle;
use std::time::Duration;

use openrigcue_model::native_value::LegacyValue;
use openrigcue_model::Telemetry;

use crate::udp_flight::clear_listener_channels;
use crate::udp_flight::python::{map_range, parse_int_text, PyInt};
use crate::{Source, SourceError};

/// Listener id.
pub const ID: &str = "joy";

/// Full-scale controller values (SDL's signed 16-bit axis range).
pub(crate) const AXIS_MIN: f64 = -32768.0;
pub(crate) const AXIS_MAX: f64 = 32767.0;
/// Value a button press writes, unscaled.
const BUTTON_PRESSED: f64 = 32767.0;
/// Published speed never drops below this.
const MIN_SPEED: f64 = 2.0;
/// Engine sound: speed 0..100 maps to 0..10900 rpm, at least 401; `max_rpm` is fixed.
const RPM_AT_FULL_SPEED: f64 = 10900.0;
const MIN_RPM: f64 = 401.0;
const MAX_RPM: f64 = 11000.0;
/// Speed deflection range.
const SPEED_RANGE: f64 = 100.0;

/// The profile keys the mapping tables know (`front`, `back`, `left`, `right`, `rotate_left`,
/// `rotate_right`, `speed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum JoystickControl {
    PitchFront,
    PitchBack,
    RollLeft,
    RollRight,
    YawLeft,
    YawRight,
    Speed,
}

impl JoystickControl {
    /// The control a profile key names; `None` for any other key,
    /// which the mapping tables never match.
    pub fn from_profile_key(key: &str) -> Option<Self> {
        Some(match key {
            "front" => JoystickControl::PitchFront,
            "back" => JoystickControl::PitchBack,
            "left" => JoystickControl::RollLeft,
            "right" => JoystickControl::RollRight,
            "rotate_left" => JoystickControl::YawLeft,
            "rotate_right" => JoystickControl::YawRight,
            "speed" => JoystickControl::Speed,
            _ => return None,
        })
    }
}

/// The assignment `type`, as far as the mapping pass tests it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    /// `'button'`: button events drive the control.
    Button,
    /// `'axis >'`: the positive half (dual-axis mode).
    AxisPositive,
    /// `'axis <'`: the negative half (dual-axis mode).
    AxisNegative,
    /// Any other text (the Joystick page also stores `'axis'`): no type test matches, so only single-axis mode
    /// reads the input.
    Other,
}

impl InputKind {
    /// Exact-string match of the stored `type`.
    pub fn from_stored(text: &str) -> Self {
        match text {
            "button" => InputKind::Button,
            "axis >" => InputKind::AxisPositive,
            "axis <" => InputKind::AxisNegative,
            _ => InputKind::Other,
        }
    }
}

/// The assignment `mode`, as far as the mapping pass tests it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AxisMode {
    /// `'Single Axis'`: the full axis travel spans the control's range.
    Single,
    /// `'Dual Axis'`: one half of the axis drives the control.
    Dual,
    /// Any other text: axis values are ignored (button events still apply).
    Other,
}

impl AxisMode {
    /// Exact-string match of the stored `mode`.
    pub fn from_stored(text: &str) -> Self {
        match text {
            "Single Axis" => AxisMode::Single,
            "Dual Axis" => AxisMode::Dual,
            _ => AxisMode::Other,
        }
    }
}

/// A stored assignment whose `trigger` is not an integer: reaching it ends the mapping thread.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("joystick assignment {key:?}: trigger {trigger:?} is not an integer")]
pub struct InvalidTrigger {
    pub key: String,
    pub trigger: String,
}

/// One profile entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoystickAssignment {
    pub control: JoystickControl,
    /// Controller name as the input backend reports it.
    pub device: String,
    pub kind: InputKind,
    /// Axis or button index on that controller (`trigger`).
    pub index: u32,
    pub mode: AxisMode,
    /// Negate a single-axis value before mapping (`invert_axis`).
    pub invert: bool,
}

impl JoystickAssignment {
    /// Converts a stored profile entry (`joystick`, `type`, `trigger`, `mode`,
    /// `invert_axis`) with the string semantics of the mapping pass.
    ///
    /// `Ok(None)` for an entry that can never act: a key the mapping tables do not know, or a trigger no input
    /// index equals (negative or beyond `u32`). An unknown `type` or `mode` is kept (see [`InputKind::Other`],
    /// [`AxisMode::Other`]): the other half of the entry still works. A non-integer trigger is an error.
    pub fn from_stored(
        key: &str,
        joystick: &str,
        kind: &str,
        trigger: &str,
        mode: &str,
        invert_axis: bool,
    ) -> Result<Option<Self>, InvalidTrigger> {
        let index = match parse_int_text(trigger) {
            Some(PyInt::Small(value)) => u32::try_from(value).ok(),
            Some(PyInt::Large) => None,
            None => return Err(InvalidTrigger { key: key.to_owned(), trigger: trigger.to_owned() }),
        };
        let (Some(control), Some(index)) = (JoystickControl::from_profile_key(key), index) else {
            return Ok(None);
        };
        Ok(Some(JoystickAssignment {
            control,
            device: joystick.to_owned(),
            kind: InputKind::from_stored(kind),
            index,
            mode: AxisMode::from_stored(mode),
            invert: invert_axis,
        }))
    }
}

/// A named set of assignments.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JoystickProfile {
    pub name: String,
    pub assignments: Vec<JoystickAssignment>,
}

/// Shared settings read by the independent mapping thread at the start of each pass.
#[derive(Debug, Clone, PartialEq)]
pub struct JoystickRuntime {
    pub enabled: bool,
    pub name: String,
    pub profile: Option<openrigcue_config::import::value::LegacyValue>,
}

/// Selected SQLite lookup for the enabled mapping pass. Status/poll refreshes
/// read only shared settings, so they cannot decode a profile or consume its SQL error.
type ProfileLookup =
    Arc<dyn Fn(&str) -> Result<Option<openrigcue_config::import::value::LegacyValue>, String> + Send + Sync>;
type FeatureLookup = Arc<dyn Fn(&str) -> Option<LegacyValue> + Send + Sync>;

/// Access to live shared settings, with an optional canonical mapping-time dictionary.
#[derive(Clone)]
pub struct JoystickRuntimeReader {
    globals: Arc<dyn Fn() -> JoystickRuntime + Send + Sync>,
    profile_lookup: Option<ProfileLookup>,
    feature_lookup: Option<FeatureLookup>,
}

impl JoystickRuntimeReader {
    pub fn new(read: impl Fn() -> JoystickRuntime + Send + Sync + 'static) -> Self {
        Self { globals: Arc::new(read), profile_lookup: None, feature_lookup: None }
    }

    pub fn with_profile_lookup(
        read: impl Fn() -> JoystickRuntime + Send + Sync + 'static,
        profile: impl Fn(&str) -> Result<Option<openrigcue_config::import::value::LegacyValue>, String>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        Self { globals: Arc::new(read), profile_lookup: Some(Arc::new(profile)), feature_lookup: None }
    }

    /// Required CUSTOM_FEATURES values, read only by the relevant channel getter.
    pub fn with_feature_lookup(mut self, read: impl Fn(&str) -> Option<LegacyValue> + Send + Sync + 'static) -> Self {
        self.feature_lookup = Some(Arc::new(read));
        self
    }

    /// Outer None retains numeric options; Some(None) is a failed required lookup.
    pub fn read_feature(&self, key: &str) -> Option<Option<LegacyValue>> {
        self.feature_lookup.as_ref().map(|read| read(key))
    }
}

impl std::fmt::Debug for JoystickRuntimeReader {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("JoystickRuntimeReader")
    }
}

/// Behaviour switches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JoystickOptions {
    /// Publish roll as lateral acceleration too (`CUSTOM_FEATURES.sway_when_roll_joy`).
    pub sway_when_roll: bool,
    /// Publish pitch as longitudinal acceleration too (`CUSTOM_FEATURES.surge_when_pitch_joy`).
    pub surge_when_pitch: bool,
    /// Whether the numeric compatibility replay reads pitch as an angle. Typed dispatch updates stored pitch
    /// when it actually requests `pitch`; requesting `pitch_rate` does not perform that mutation.
    pub pitch_read_as_angle: bool,
}

impl Default for JoystickOptions {
    fn default() -> Self {
        JoystickOptions { sway_when_roll: false, surge_when_pitch: false, pitch_read_as_angle: true }
    }
}

/// Current state of one controller.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ControllerState {
    /// Axis index → value in -32768..=32767.
    pub axes: BTreeMap<u32, i32>,
    /// Button index → latest press (`true`) or release since the last poll.
    pub button_events: BTreeMap<u32, bool>,
}

/// One controller's published input pass: current axes
/// and the latest value of each button that sent an event during that SDL pass.
/// The older direct injection helpers can retain button values between passes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ControllerView {
    /// Axis index → value in -32768..=32767.
    pub axes: BTreeMap<u32, i32>,
    /// Button index → pressed by its last event.
    pub buttons: BTreeMap<u32, bool>,
}

/// Button events not yet taken by one reader, by controller name.
type PendingEvents = BTreeMap<String, BTreeMap<u32, bool>>;

/// One controller event, as the SDL event queue delivers it
/// (`SDL_JOYDEVICEADDED`, `SDL_JOYDEVICEREMOVED`, `SDL_JOYAXISMOTION`, `SDL_JOYHATMOTION`, `SDL_JOYBUTTONDOWN/UP`),
/// plus other events whose instance lookup succeeds and reaches the common shortcut tail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoystickEvent {
    Added { controller: String },
    Removed { controller: String },
    Axis { controller: String, index: u32, value: i32 },
    Hat { controller: String, index: u32 },
    Button { controller: String, index: u32, pressed: bool },
    Other { controller: String },
}

/// Longest backlog of one event stream; later events are dropped, as SDL drops events once its queue holds
/// `SDL_MAX_QUEUED_EVENTS` (65535).
const MAX_QUEUED_EVENTS: usize = 65_535;

#[derive(Debug, Default)]
struct InputState {
    enumeration: u64,
    initial_axes: BTreeMap<String, BTreeMap<u32, i32>>,
    last_axes: BTreeMap<String, BTreeMap<u32, i32>>,
    native_joysticks: Option<serde_json::Map<String, serde_json::Value>>,
    controllers: BTreeMap<String, ControllerView>,
    /// Connected devices by name, in connection order; a name appears once per device.
    devices: Vec<String>,
    /// Every reader's pending button events; a dropped reader is forgotten at the next event.
    readers: Vec<Weak<Mutex<PendingEvents>>>,
    /// Every ordered stream's pending events; a dropped stream is forgotten at the next event.
    streams: Vec<Weak<Mutex<Vec<JoystickEvent>>>>,
}

impl InputState {
    fn publish(&mut self, event: JoystickEvent) {
        self.streams.retain(|stream| match stream.upgrade() {
            Some(pending) => {
                let mut pending = pending.lock().unwrap_or_else(PoisonError::into_inner);
                if pending.len() < MAX_QUEUED_EVENTS {
                    pending.push(event.clone());
                }
                true
            }
            None => false,
        });
    }
}

/// Controller states by controller name, shared between an input backend and the readers (the joystick listener,
/// the 4D recorder, the Joystick page).
#[derive(Debug, Clone, Default)]
pub struct JoystickInputs {
    state: Arc<Mutex<InputState>>,
}

impl JoystickInputs {
    fn lock(&self) -> std::sync::MutexGuard<'_, InputState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records a device the backend opened, before any of its events.
    pub fn connect(&self, controller: &str) {
        let mut state = self.lock();
        state.enumeration = state.enumeration.wrapping_add(1);
        state.initial_axes = state.controllers.iter().map(|(name, view)| (name.clone(), view.axes.clone())).collect();
        state.devices.push(controller.to_owned());
        state.publish(JoystickEvent::Added { controller: controller.to_owned() });
    }

    /// Records the current value of an axis.
    pub fn set_axis(&self, controller: &str, index: u32, value: i32) {
        let mut state = self.lock();
        state.initial_axes.entry(controller.to_owned()).or_default().entry(index).or_insert(value);
        state.controllers.entry(controller.to_owned()).or_default().axes.insert(index, value);
    }

    /// Records an axis movement the device reported: the value, plus an event for the ordered streams.
    pub fn axis_event(&self, controller: &str, index: u32, value: i32) {
        let mut state = self.lock();
        state.last_axes.entry(controller.to_owned()).or_default().insert(index, value);
        state.controllers.entry(controller.to_owned()).or_default().axes.insert(index, value);
        state.publish(JoystickEvent::Axis { controller: controller.to_owned(), index, value });
    }

    /// Records a hat movement (an event for the ordered streams only; hats are not axes here, as in SDL).
    pub fn hat_event(&self, controller: &str, index: u32) {
        self.lock().publish(JoystickEvent::Hat { controller: controller.to_owned(), index });
    }

    /// Records a button press or release.
    pub fn button_event(&self, controller: &str, index: u32, pressed: bool) {
        let mut state = self.lock();
        state.controllers.entry(controller.to_owned()).or_default().buttons.insert(index, pressed);
        state.readers.retain(|reader| match reader.upgrade() {
            Some(pending) => {
                lock_pending(&pending).entry(controller.to_owned()).or_default().insert(index, pressed);
                true
            }
            None => false,
        });
        state.publish(JoystickEvent::Button { controller: controller.to_owned(), index, pressed });
    }

    /// Forgets a device that was unplugged; its state goes once no device of that name is left.
    pub fn remove(&self, controller: &str) {
        let mut state = self.lock();
        state.enumeration = state.enumeration.wrapping_add(1);
        if let Some(position) = state.devices.iter().position(|name| name == controller) {
            state.devices.remove(position);
        }
        if !state.devices.iter().any(|name| name == controller) {
            state.controllers.remove(controller);
        }
        state.publish(JoystickEvent::Removed { controller: controller.to_owned() });
    }

    /// The connected devices by name, in connection order.
    pub fn devices(&self) -> Vec<String> {
        self.lock().devices.clone()
    }

    /// The current state of every controller.
    pub fn snapshot(&self) -> BTreeMap<String, ControllerView> {
        self.lock().controllers.clone()
    }

    /// The state read by the Joystick page. An enumeration changes the opaque handle identity even
    /// when the same named controller returns, as each enumeration opens a new SDL_Joystick.
    pub fn page_snapshot(&self) -> serde_json::Value {
        use serde_json::{json, Map, Value};
        let state = self.lock();
        let mut joysticks = Map::new();
        for (index, name) in state.devices.iter().enumerate() {
            let axes = state.controllers.get(name).map(|v| v.axes.len()).unwrap_or(0);
            joysticks.insert(
                name.clone(),
                json!({
                    "name": name, "joystick": state.enumeration, "num_axis": axes, "index": index,
                    "initial_axis_value": state.initial_axes.get(name),
                }),
            );
        }
        let values: Map<String, Value> = state
            .controllers
            .iter()
            .map(|(name, view)| (name.clone(), json!({"button": view.buttons, "axis": view.axes})))
            .collect();
        let last: Map<String, Value> =
            state.last_axes.iter().map(|(name, axes)| (name.clone(), json!({"axis": axes}))).collect();
        json!({"joysticks": state.native_joysticks.as_ref().unwrap_or(&joysticks), "state": values, "last_state": last})
    }

    pub fn clear_last_state(&self) {
        self.lock().last_axes.clear();
    }

    /// A reader that receives every button event from now on.
    pub fn reader(&self) -> JoystickReader {
        let pending = Arc::new(Mutex::new(PendingEvents::new()));
        self.lock().readers.push(Arc::downgrade(&pending));
        JoystickReader { inputs: self.clone(), pending }
    }

    /// A stream that receives every event from now on, in order (used by the hotkey runtime).
    pub fn events(&self) -> JoystickEvents {
        let pending = Arc::new(Mutex::new(Vec::new()));
        self.lock().streams.push(Arc::downgrade(&pending));
        JoystickEvents { pending }
    }
}

/// An ordered event stream of [`JoystickInputs`].
#[derive(Debug)]
pub struct JoystickEvents {
    pending: Arc<Mutex<Vec<JoystickEvent>>>,
}

impl JoystickEvents {
    /// The events since the last call, oldest first.
    pub fn take(&self) -> Vec<JoystickEvent> {
        std::mem::take(&mut *self.pending.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

fn lock_pending(pending: &Mutex<PendingEvents>) -> std::sync::MutexGuard<'_, PendingEvents> {
    pending.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One consumer of the inputs. SDL readers see the latest shared pass;
/// older direct injection helpers retain their pending-event compatibility queue.
#[derive(Debug)]
pub struct JoystickReader {
    inputs: JoystickInputs,
    pending: Arc<Mutex<PendingEvents>>,
}

impl JoystickReader {
    /// Current axes and the SDL pass's buttons (or queued direct-injection events).
    pub fn take(&self) -> BTreeMap<String, ControllerState> {
        let mut events = std::mem::take(&mut *lock_pending(&self.pending));
        let state = self.inputs.lock();
        let native = state.native_joysticks.is_some();
        let mut controllers: BTreeMap<String, ControllerState> = state
            .controllers
            .iter()
            .map(|(name, view)| {
                let button_events = if native { view.buttons.clone() } else { events.remove(name).unwrap_or_default() };
                (name.clone(), ControllerState { axes: view.axes.clone(), button_events })
            })
            .collect();
        if !native {
            for (name, button_events) in events {
                controllers.entry(name).or_default().button_events = button_events;
            }
        }
        controllers
    }
}

/// Range and sign of one control in a mapping table (dual-axis or single-axis).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ControlRange {
    pub(crate) min: f64,
    pub(crate) max: f64,
    pub(crate) reverse: bool,
}

const fn range(min: f64, max: f64, reverse: bool) -> ControlRange {
    ControlRange { min, max, reverse }
}

impl JoystickControl {
    /// The control's dual-axis mapping.
    pub(crate) fn dual_range(self) -> ControlRange {
        match self {
            JoystickControl::PitchFront => range(-10.0, 0.0, true),
            JoystickControl::PitchBack => range(0.0, 10.0, true),
            JoystickControl::RollLeft => range(-10.0, 0.0, false),
            JoystickControl::RollRight => range(0.0, 10.0, false),
            JoystickControl::YawLeft => range(-10.0, 0.0, false),
            JoystickControl::YawRight => range(0.0, 10.0, false),
            JoystickControl::Speed => range(0.0, SPEED_RANGE, false),
        }
    }

    /// The control's single-axis mapping.
    pub(crate) fn single_range(self) -> ControlRange {
        match self {
            JoystickControl::PitchFront => range(0.0, 10.0, false),
            JoystickControl::PitchBack => range(0.0, 10.0, true),
            JoystickControl::RollLeft => range(0.0, 10.0, true),
            JoystickControl::RollRight => range(0.0, 10.0, false),
            JoystickControl::YawLeft => range(0.0, 10.0, true),
            JoystickControl::YawRight => range(0.0, 10.0, false),
            JoystickControl::Speed => range(0.0, SPEED_RANGE, false),
        }
    }
}

/// The value of every control (0 until first set).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Deflections {
    pitch_front: f64,
    pitch_back: f64,
    roll_left: f64,
    roll_right: f64,
    yaw_left: f64,
    yaw_right: f64,
    speed: f64,
    /// Axis mapping returns floats; defaults, button writes and opposite-half resets are integers.
    float_slots: u8,
    /// A zero-valued assignment still makes the telemetry dictionary nonempty.
    written: bool,
}

impl Deflections {
    fn bit(control: JoystickControl) -> u8 {
        1 << match control {
            JoystickControl::PitchFront => 0,
            JoystickControl::PitchBack => 1,
            JoystickControl::RollLeft => 2,
            JoystickControl::RollRight => 3,
            JoystickControl::YawLeft => 4,
            JoystickControl::YawRight => 5,
            JoystickControl::Speed => 6,
        }
    }

    fn value(&self, control: JoystickControl) -> f64 {
        match control {
            JoystickControl::PitchFront => self.pitch_front,
            JoystickControl::PitchBack => self.pitch_back,
            JoystickControl::RollLeft => self.roll_left,
            JoystickControl::RollRight => self.roll_right,
            JoystickControl::YawLeft => self.yaw_left,
            JoystickControl::YawRight => self.yaw_right,
            JoystickControl::Speed => self.speed,
        }
    }

    fn native_value(&self, control: JoystickControl) -> LegacyValue {
        let value = self.value(control);
        if self.float_slots & Self::bit(control) != 0 {
            LegacyValue::Float(value)
        } else {
            LegacyValue::Int(value as i64)
        }
    }

    fn dominant_native(&self, first: JoystickControl, second: JoystickControl) -> LegacyValue {
        self.native_value(if self.value(first).abs() > self.value(second).abs() { first } else { second })
    }

    fn store(&mut self, control: JoystickControl, value: f64, float: bool) {
        self.written = true;
        *self.slot(control) = value;
        if float {
            self.float_slots |= Self::bit(control);
        } else {
            self.float_slots &= !Self::bit(control);
        }
    }

    fn slot(&mut self, control: JoystickControl) -> &mut f64 {
        match control {
            JoystickControl::PitchFront => &mut self.pitch_front,
            JoystickControl::PitchBack => &mut self.pitch_back,
            JoystickControl::RollLeft => &mut self.roll_left,
            JoystickControl::RollRight => &mut self.roll_right,
            JoystickControl::YawLeft => &mut self.yaw_left,
            JoystickControl::YawRight => &mut self.yaw_right,
            JoystickControl::Speed => &mut self.speed,
        }
    }

    /// Maps an axis value into the control's range, negated for reversed controls.
    fn set_mapped(&mut self, control: JoystickControl, range: ControlRange, value: f64, in_min: f64, in_max: f64) {
        let mapped = map_range(value, in_min, in_max, range.min, range.max);
        self.store(control, if range.reverse { -mapped } else { mapped }, true);
    }

    /// One assignment's mapping pass over a controller's state: its button event
    /// first, then its axis.
    fn apply(&mut self, assignment: &JoystickAssignment, state: &ControllerState) {
        if let (Some(&pressed), InputKind::Button) = (state.button_events.get(&assignment.index), assignment.kind) {
            self.apply_button(assignment.control, pressed);
        }
        let Some(&raw) = state.axes.get(&assignment.index) else {
            return;
        };
        self.apply_axis(assignment, raw);
    }

    fn apply_button(&mut self, control: JoystickControl, pressed: bool) {
        self.store(control, if pressed { BUTTON_PRESSED } else { 0.0 }, false);
    }

    fn apply_axis(&mut self, assignment: &JoystickAssignment, raw: i32) {
        let control = assignment.control;
        let value = f64::from(raw);
        match (assignment.mode, assignment.kind) {
            (AxisMode::Single, _) => {
                let value = if assignment.invert { -value } else { value };
                self.set_mapped(control, control.single_range(), value, AXIS_MIN, AXIS_MAX);
            }
            (AxisMode::Dual, InputKind::AxisPositive) if value > 0.0 => {
                self.set_mapped(control, control.dual_range(), value, 0.0, AXIS_MAX);
            }
            (AxisMode::Dual, InputKind::AxisNegative) if value < 0.0 => {
                self.set_mapped(control, control.dual_range(), value, AXIS_MIN, 0.0);
            }
            (AxisMode::Dual, InputKind::AxisPositive) if value < 0.0 => self.store(control, 0.0, false),
            (AxisMode::Dual, InputKind::AxisNegative) if value > 0.0 => self.store(control, 0.0, false),
            (AxisMode::Dual, _) | (AxisMode::Other, _) => {}
        }
    }

    /// The deflection with the larger magnitude; the second wins a tie.
    fn dominant(a: f64, b: f64) -> f64 {
        if a.abs() > b.abs() {
            a
        } else {
            b
        }
    }

    fn pitch(&self) -> f64 {
        -Self::dominant(self.pitch_front, self.pitch_back)
    }

    fn roll(&self) -> f64 {
        Self::dominant(self.roll_left, self.roll_right)
    }

    fn yaw(&self) -> f64 {
        Self::dominant(self.yaw_left, self.yaw_right)
    }
}

fn negate_native_number(value: LegacyValue) -> LegacyValue {
    match value {
        LegacyValue::Int(value) => LegacyValue::Int(-value),
        LegacyValue::Float(value) => LegacyValue::Float(-value),
        _ => unreachable!("joystick deflections contain only mapped floats and button integers"),
    }
}

/// Wheels a suspension channel is published for, by the signs of roll and pitch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Corner {
    LeftFront,
    RightFront,
    LeftRear,
    RightRear,
}

pub(crate) fn loaded_corners(roll: f64, pitch: f64) -> &'static [Corner] {
    use std::cmp::Ordering::{Equal, Greater, Less};
    use Corner::*;
    let sign = |x: f64| {
        if x > 0.0 {
            Greater
        } else if x < 0.0 {
            Less
        } else {
            Equal
        }
    };
    match (sign(roll), sign(pitch)) {
        (Greater, Greater) => &[RightRear],
        (Greater, Less) => &[RightFront],
        (Less, Greater) => &[LeftRear],
        (Less, Less) => &[LeftFront],
        (Equal, Equal) => &[],
        (Equal, Less) => &[LeftFront, RightFront],
        (Equal, Greater) => &[LeftRear, RightRear],
        (Greater, Equal) => &[RightFront, RightRear],
        (Less, Equal) => &[LeftFront, LeftRear],
    }
}

/// The suspension value: `(roll² + pitch²) / 10` for the loaded wheels, 0 for the others.
fn suspension(corner: Corner, roll: f64, pitch: f64) -> f64 {
    if loaded_corners(roll, pitch).contains(&corner) {
        (roll * roll + pitch * pitch) / 10.0
    } else {
        0.0
    }
}

/// One stored profile entry as the Joystick page saves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoredJoystickEntry<'a> {
    pub key: &'a str,
    pub joystick: &'a str,
    pub kind: &'a str,
    pub trigger: &'a str,
    pub mode: &'a str,
    pub invert_axis: bool,
}

/// One entry of the active profile as the mapping pass walks it.
#[derive(Debug, Clone, PartialEq)]
enum MappingStep {
    Assign(JoystickAssignment),
    /// An entry whose trigger is not an integer: once its controller is connected, reaching it
    /// ends the mapping thread.
    TriggerRaises {
        device: String,
        error: InvalidTrigger,
    },
}

fn steps_of(profile: &JoystickProfile) -> Vec<MappingStep> {
    profile.assignments.iter().cloned().map(MappingStep::Assign).collect()
}

/// Pass periods of the two joystick threads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JoystickTiming {
    /// Pause after one input pass (the button events of the pass and every axis
    /// into the shared state).
    pub input_pass: Duration,
    /// Pause after one mapping pass over the profile.
    pub mapping_pass: Duration,
    /// Pause of the mapping thread while the joystick listener is off.
    pub mapping_idle: Duration,
}

impl JoystickTiming {
    pub const WINDOWS: JoystickTiming = JoystickTiming {
        input_pass: Duration::from_millis(10),
        mapping_pass: Duration::from_millis(10),
        mapping_idle: Duration::from_secs(2),
    };
    pub const LINUX: JoystickTiming = JoystickTiming {
        input_pass: Duration::from_millis(25),
        mapping_pass: Duration::from_millis(25),
        mapping_idle: Duration::from_secs(2),
    };
}

impl Default for JoystickTiming {
    fn default() -> Self {
        JoystickTiming::WINDOWS
    }
}

/// The state the mapping thread writes and the channel getters read.
#[derive(Debug, Default)]
struct Mapper {
    /// The joystick switch.
    enabled: bool,
    /// The selected profile, re-read on every pass.
    steps: Vec<MappingStep>,
    /// Canonical profile, including missing/null/malformed values. The mapping pass performs its lookups
    /// only as it reaches them, rather than validating the whole profile in advance.
    raw_profile: Option<(String, Option<openrigcue_config::import::value::LegacyValue>)>,
    /// The control deflections.
    deflections: Deflections,
    /// The mapping thread ended on an error; nothing is mapped until the application restarts.
    ended: bool,
}

/// What the two threads and the source share.
#[derive(Debug, Default)]
struct JoyShared {
    stop: Mutex<bool>,
    wake: Condvar,
    /// The controller state of the latest input pass.
    pass: Mutex<BTreeMap<String, ControllerState>>,
    mapper: Mutex<Mapper>,
    runtime: Mutex<Option<JoystickRuntimeReader>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl JoyShared {
    fn refresh_runtime(&self) -> Option<String> {
        let reader = lock(&self.runtime).clone()?;
        let runtime = (reader.globals)();
        let mut mapper = lock(&self.mapper);
        mapper.enabled = runtime.enabled;
        mapper.raw_profile = Some((runtime.name.clone(), runtime.profile));
        Some(runtime.name)
    }

    /// Sleeps for `duration`; `false` once the source is dropped.
    fn sleep(&self, duration: Duration) -> bool {
        let stop = lock(&self.stop);
        let (stop, _) =
            self.wake.wait_timeout_while(stop, duration, |stop| !*stop).unwrap_or_else(PoisonError::into_inner);
        !*stop
    }

    fn shut_down(&self) {
        *lock(&self.stop) = true;
        self.wake.notify_all();
    }

    /// One input pass: the shared state becomes every axis plus this pass's button events.
    fn input_pass(&self, reader: &JoystickReader) {
        *lock(&self.pass) = reader.take();
    }

    /// One enabled mapping pass. Dictionary/field errors end the thread before
    /// subsequent writes; a database read happens before the input-state snapshot.
    fn mapping_pass(&self) -> bool {
        let lookup = lock(&self.runtime).as_ref().and_then(|reader| reader.profile_lookup.clone());
        let stored = lookup.map(|lookup| {
            let name = lock(&self.mapper).raw_profile.as_ref().map(|(name, _)| name.clone()).unwrap_or_default();
            let value = lookup(&name);
            (name, value)
        });
        let mut mapper = lock(&self.mapper);
        if let Some((name, result)) = stored {
            match result {
                Ok(profile) => mapper.raw_profile = Some((name, profile)),
                Err(error) => {
                    tracing::warn!(%error,"joystick mapping thread ended (update_joystick_state)");
                    mapper.ended = true;
                    return false;
                }
            }
        }
        let pass = lock(&self.pass).clone();
        let Mapper { steps, raw_profile, deflections, .. } = &mut *mapper;
        if let Some((name, profile)) = raw_profile {
            return match raw::apply(name, profile.as_ref(), &pass, deflections) {
                Ok(()) => true,
                Err(error) => {
                    tracing::warn!(%error, "joystick mapping thread ended (update_joystick_state)");
                    mapper.ended = true;
                    false
                }
            };
        }
        for step in steps.iter() {
            match step {
                MappingStep::Assign(assignment) => {
                    if let Some(state) = pass.get(&assignment.device) {
                        deflections.apply(assignment, state);
                    }
                }
                MappingStep::TriggerRaises { device, error } if pass.contains_key(device) => {
                    tracing::warn!(%error, "joystick mapping thread ended (update_joystick_state)");
                    mapper.ended = true;
                    return false;
                }
                MappingStep::TriggerRaises { .. } => {}
            }
        }
        true
    }

    /// The input thread.
    fn run_input(&self, reader: JoystickReader, period: Duration) {
        loop {
            self.input_pass(&reader);
            if !self.sleep(period) {
                return;
            }
        }
    }

    /// The mapping thread.
    fn run_mapping(&self, timing: JoystickTiming) {
        loop {
            self.refresh_runtime();
            let enabled = lock(&self.mapper).enabled;
            let pause = if enabled {
                if !self.mapping_pass() {
                    return;
                }
                timing.mapping_pass
            } else {
                timing.mapping_idle
            };
            if !self.sleep(pause) {
                return;
            }
        }
    }
}

/// The joystick listener: the mapping thread and the input thread
/// run on their own periods ([`JoystickTiming`]); the channel getters read the deflections they left.
pub struct JoystickSource {
    shared: Arc<JoyShared>,
    threads: Vec<JoinHandle<()>>,
    profile_name: String,
    options: JoystickOptions,
    /// Roll and pitch as last stored by a channel read, which the suspension
    /// channels use.
    stored_roll: Mutex<f64>,
    stored_pitch: Mutex<f64>,
    /// The car name, written by an enabled poll and removed by a disabled one.
    received_profile_name: Option<String>,
    /// Tests drive the two passes themselves ([`JoystickSource::step`]).
    #[cfg(test)]
    reader: Option<JoystickReader>,
}

impl JoystickSource {
    /// Starts the input and mapping threads with the default periods.
    pub fn new(profile: JoystickProfile, options: JoystickOptions, inputs: JoystickInputs) -> Self {
        Self::with_timing(profile, options, inputs, JoystickTiming::default())
    }

    /// Like [`JoystickSource::new`] with the given thread periods.
    pub fn with_timing(
        profile: JoystickProfile,
        options: JoystickOptions,
        inputs: JoystickInputs,
        timing: JoystickTiming,
    ) -> Self {
        let mut source = Self::without_threads(profile, options);
        let reader = inputs.reader();
        let input = Arc::clone(&source.shared);
        let mapping = Arc::clone(&source.shared);
        let spawned = [
            std::thread::Builder::new()
                .name("hookAxis".into())
                .spawn(move || input.run_input(reader, timing.input_pass)),
            std::thread::Builder::new().name("threadJoy".into()).spawn(move || mapping.run_mapping(timing)),
        ];
        for thread in spawned {
            match thread {
                Ok(handle) => source.threads.push(handle),
                Err(error) => tracing::warn!(%error, "joystick: cannot start a thread"),
            }
        }
        source
    }

    fn without_threads(profile: JoystickProfile, options: JoystickOptions) -> Self {
        let shared = Arc::new(JoyShared::default());
        lock(&shared.mapper).steps = steps_of(&profile);
        JoystickSource {
            shared,
            threads: Vec::new(),
            profile_name: profile.name,
            options,
            stored_roll: Mutex::new(0.0),
            stored_pitch: Mutex::new(0.0),
            received_profile_name: None,
            #[cfg(test)]
            reader: None,
        }
    }

    /// A source without threads whose passes the test runs with [`JoystickSource::step`].
    #[cfg(test)]
    fn stepped(profile: JoystickProfile, options: JoystickOptions, inputs: &JoystickInputs) -> Self {
        let mut source = Self::without_threads(profile, options);
        source.reader = Some(inputs.reader());
        source
    }

    /// One input pass, then one mapping pass if the switch is on (tests).
    #[cfg(test)]
    fn step(&self) {
        self.shared.refresh_runtime();
        if let Some(reader) = &self.reader {
            self.shared.input_pass(reader);
        }
        if lock(&self.shared.mapper).enabled && !lock(&self.shared.mapper).ended {
            self.shared.mapping_pass();
        }
    }

    /// Switches joystick motion on or off (the "joystick switch" shortcut).
    pub fn set_enabled(&mut self, enabled: bool) {
        lock(&self.shared.mapper).enabled = enabled;
    }

    /// Connects the mapping thread to the live configuration, selected profile and joystick switch.
    pub fn set_runtime_reader(&mut self, reader: JoystickRuntimeReader) {
        *lock(&self.shared.runtime) = Some(reader);
        self.refresh_runtime();
    }

    /// Refreshes from the live shared settings when connected; false for callers using the explicit setters.
    pub fn refresh_runtime(&mut self) -> bool {
        match self.shared.refresh_runtime() {
            Some(name) => {
                self.profile_name = name;
                true
            }
            None => false,
        }
    }

    /// Replaces the active profile; it is re-read on every mapping pass, so edits apply at once.
    pub fn set_profile(&mut self, profile: JoystickProfile) {
        let mut mapper = lock(&self.shared.mapper);
        mapper.raw_profile = None;
        mapper.steps = steps_of(&profile);
        self.profile_name = profile.name;
    }

    /// Selects the actual stored value. `None` means the table has no such key; LegacyValue::None means a
    /// present profile whose stored value is none. Invalid fields fail at first use in the mapping thread.
    pub fn set_raw_profile(&mut self, name: &str, profile: Option<openrigcue_config::import::value::LegacyValue>) {
        lock(&self.shared.mapper).raw_profile = Some((name.to_owned(), profile));
        self.profile_name = name.to_owned();
    }

    /// Replaces the active profile with its stored entries in profile order, keeping an entry whose trigger is
    /// not an integer at its place: when the mapping pass reaches it while its controller is connected,
    /// the mapping thread ends.
    pub fn set_stored_profile<'a>(&mut self, name: &str, entries: impl IntoIterator<Item = StoredJoystickEntry<'a>>) {
        let steps = entries
            .into_iter()
            .filter_map(|e| {
                match JoystickAssignment::from_stored(e.key, e.joystick, e.kind, e.trigger, e.mode, e.invert_axis) {
                    Ok(assignment) => assignment.map(MappingStep::Assign),
                    Err(error) => Some(MappingStep::TriggerRaises { device: e.joystick.to_owned(), error }),
                }
            })
            .collect();
        let mut mapper = lock(&self.shared.mapper);
        mapper.raw_profile = None;
        mapper.steps = steps;
        self.profile_name = name.to_owned();
    }

    /// Replaces the options; they are read on every channel read.
    pub fn set_options(&mut self, options: JoystickOptions) {
        self.options = options;
    }

    /// Whether the mapping thread ended on an entry whose trigger is not an integer.
    pub fn mapping_ended(&self) -> bool {
        lock(&self.shared.mapper).ended
    }

    /// Numeric compatibility replay. Typed dispatch performs actual getter calls against current deflections.
    fn write(&mut self, d: Deflections, t: &mut Telemetry) {
        let (roll, pitch) = (d.roll(), d.pitch());
        t.speed = if d.speed < MIN_SPEED { MIN_SPEED } else { d.speed };
        if self.options.sway_when_roll {
            *lock(&self.stored_roll) = roll;
            t.lateral_acceleration = roll;
        }
        if self.options.surge_when_pitch {
            *lock(&self.stored_pitch) = pitch;
            t.longitudinal_acceleration = -pitch;
        }
        t.max_rpm = MAX_RPM;
        let rpm = map_range(d.speed, 0.0, SPEED_RANGE, 0.0, RPM_AT_FULL_SPEED);
        t.rpm = if rpm < MIN_RPM { MIN_RPM } else { rpm };
        let (stored_roll, stored_pitch) = (*lock(&self.stored_roll), *lock(&self.stored_pitch));
        t.suspension_travel_lf = suspension(Corner::LeftFront, stored_roll, stored_pitch);
        t.suspension_travel_lr = suspension(Corner::LeftRear, stored_roll, stored_pitch);
        t.suspension_travel_rf = suspension(Corner::RightFront, stored_roll, stored_pitch);
        t.suspension_travel_rr = suspension(Corner::RightRear, stored_roll, stored_pitch);
        *lock(&self.stored_roll) = roll;
        t.steering_wheel_angle = roll;
        t.yaw = d.yaw();
        if self.options.pitch_read_as_angle {
            *lock(&self.stored_pitch) = pitch;
        }
        t.pitch = pitch;
        t.roll = roll;
        t.vehicle.clone_from(&self.profile_name);
    }
}

impl JoystickSource {
    fn native_answer(&self, name: &str) -> Option<LegacyValue> {
        use JoystickControl::{PitchBack, PitchFront, RollLeft, RollRight, Speed, YawLeft, YawRight};
        use LegacyValue::{Float, Int, Str};
        if self.received_profile_name.is_none() && !lock(&self.shared.mapper).deflections.written {
            return Some(Int(0));
        }
        let acceleration_enabled = match name {
            "lateral_acceleration" => self.native_feature("sway_when_roll_joy", self.options.sway_when_roll)?,
            "longitudinal_acceleration" => {
                self.native_feature("surge_when_pitch_joy", self.options.surge_when_pitch)?
            }
            _ => false,
        };
        // Settings reads precede the current deflection reads.
        let d = lock(&self.shared.mapper).deflections;
        let read_roll = || {
            let value = d.dominant_native(RollLeft, RollRight);
            *lock(&self.stored_roll) = value.number().expect("numeric deflection");
            value
        };
        let read_pitch = || {
            let value = negate_native_number(d.dominant_native(PitchFront, PitchBack));
            *lock(&self.stored_pitch) = value.number().expect("numeric deflection");
            value
        };
        Some(match name {
            "roll" | "steering_wheel_angle" => read_roll(),
            "pitch" => read_pitch(),
            "yaw" => d.dominant_native(YawLeft, YawRight),
            "lateral_acceleration" if acceleration_enabled => read_roll(),
            "longitudinal_acceleration" if acceleration_enabled => negate_native_number(read_pitch()),
            "speed" => {
                if d.speed < MIN_SPEED {
                    Int(MIN_SPEED as i64)
                } else {
                    d.native_value(Speed)
                }
            }
            "rpm" => {
                let rpm = map_range(d.speed, 0.0, SPEED_RANGE, 0.0, RPM_AT_FULL_SPEED);
                if rpm < MIN_RPM {
                    Int(MIN_RPM as i64)
                } else {
                    Float(rpm)
                }
            }
            "max_rpm" => Int(MAX_RPM as i64),
            "car_name" => self.received_profile_name.as_ref().map_or(Int(0), |name| Str(name.clone())),
            "pitch_front" => d.native_value(PitchFront),
            "pitch_back" => d.native_value(PitchBack),
            "roll_left" => d.native_value(RollLeft),
            "roll_right" => d.native_value(RollRight),
            "yaw_left" => d.native_value(YawLeft),
            "yaw_right" => d.native_value(YawRight),
            "suspension_travel_lf" | "suspension_travel_rf" | "suspension_travel_lr" | "suspension_travel_rr" => {
                let corner = match name {
                    "suspension_travel_lf" => Corner::LeftFront,
                    "suspension_travel_rf" => Corner::RightFront,
                    "suspension_travel_lr" => Corner::LeftRear,
                    _ => Corner::RightRear,
                };
                let (roll, pitch) = (*lock(&self.stored_roll), *lock(&self.stored_pitch));
                if loaded_corners(roll, pitch).contains(&corner) {
                    Float(suspension(corner, roll, pitch))
                } else {
                    Int(0)
                }
            }
            _ => Int(0),
        })
    }

    fn native_feature(&self, key: &str, fallback: bool) -> Option<bool> {
        let reader = lock(&self.shared.runtime).clone();
        match reader.and_then(|reader| reader.read_feature(key)) {
            None => Some(fallback),
            Some(value) => match value? {
                LegacyValue::Bool(value) => Some(value),
                value => openrigcue_model::native_value::str2bool(
                    &openrigcue_model::python_repr::str_native(&value).ok()?.scalar_string()?,
                ),
            },
        }
    }
}

impl Drop for JoystickSource {
    fn drop(&mut self) {
        self.shared.shut_down();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

impl Source for JoystickSource {
    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        self.native_answer(name)
    }

    fn read_native(&self, name: &str) -> crate::NativeRead {
        self.native_answer(name).map_or(crate::NativeRead::Raised, crate::NativeRead::Value)
    }

    fn id(&self) -> &'static str {
        ID
    }

    /// The listener's status is the joystick switch.
    fn is_running(&mut self) -> Result<bool, SourceError> {
        self.refresh_runtime();
        Ok(lock(&self.shared.mapper).enabled)
    }

    /// A poll stores the current profile name, or clears the listener's dictionary when disabled.
    fn receive_native(&mut self, _telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        self.refresh_runtime();
        let mut mapper = lock(&self.shared.mapper);
        if !mapper.enabled {
            mapper.deflections = Deflections::default();
            *lock(&self.stored_roll) = 0.0;
            *lock(&self.stored_pitch) = 0.0;
            self.received_profile_name = None;
            return Ok(false);
        }
        self.received_profile_name = Some(self.profile_name.clone());
        Ok(true)
    }

    fn poll(&mut self, telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        if !self.receive_native(telemetry)? {
            return Ok(false);
        }
        let deflections = lock(&self.shared.mapper).deflections;
        clear_listener_channels(telemetry);
        self.write(deflections, telemetry);
        telemetry.clear_nan();
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAD: &str = "Wheel";

    /// The ordered stream sees every event in order, including a press and release of the same button
    /// between two takes, which the per-button readers collapse; the initial axis state is not an event.
    #[test]
    fn event_stream_keeps_order() {
        let inputs = JoystickInputs::default();
        let events = inputs.events();
        inputs.connect(PAD);
        inputs.set_axis(PAD, 0, 5);
        inputs.button_event(PAD, 3, true);
        inputs.axis_event(PAD, 1, -7);
        inputs.hat_event(PAD, 0);
        inputs.button_event(PAD, 3, false);
        inputs.remove(PAD);
        let controller = PAD.to_owned();
        assert_eq!(
            events.take(),
            vec![
                JoystickEvent::Added { controller: controller.clone() },
                JoystickEvent::Button { controller: controller.clone(), index: 3, pressed: true },
                JoystickEvent::Axis { controller: controller.clone(), index: 1, value: -7 },
                JoystickEvent::Hat { controller: controller.clone(), index: 0 },
                JoystickEvent::Button { controller: controller.clone(), index: 3, pressed: false },
                JoystickEvent::Removed { controller },
            ]
        );
        assert!(events.take().is_empty());
        drop(events);
        inputs.button_event(PAD, 1, true);
        assert!(inputs.lock().streams.is_empty(), "a dropped stream is forgotten");
    }

    fn assignment(control: JoystickControl, kind: InputKind, index: u32, mode: AxisMode) -> JoystickAssignment {
        JoystickAssignment { control, device: PAD.into(), kind, index, mode, invert: false }
    }

    fn source(assignments: Vec<JoystickAssignment>, options: JoystickOptions) -> (JoystickSource, JoystickInputs) {
        let inputs = JoystickInputs::default();
        let mut source =
            JoystickSource::stepped(JoystickProfile { name: "Default".into(), assignments }, options, &inputs);
        source.set_enabled(true);
        (source, inputs)
    }

    #[test]
    fn native_getters_observe_mapping_changes_without_a_poll() {
        let (s, inputs) = source(
            vec![assignment(JoystickControl::RollRight, InputKind::AxisPositive, 0, AxisMode::Dual)],
            JoystickOptions::default(),
        );
        inputs.set_axis(PAD, 0, 32767);
        s.step();
        assert_eq!(s.native_channel("suspension_travel_rf"), Some(LegacyValue::Int(0)));
        assert_eq!(s.native_channel("roll"), Some(LegacyValue::Float(10.0)));
        assert_eq!(s.native_channel("suspension_travel_rf"), Some(LegacyValue::Float(10.0)));
    }

    #[test]
    fn native_receive_captures_the_name_and_disabled_receive_clears_getter_state() {
        let (mut s, inputs) = source(
            vec![assignment(JoystickControl::RollRight, InputKind::AxisPositive, 0, AxisMode::Dual)],
            JoystickOptions::default(),
        );
        inputs.set_axis(PAD, 0, 32767);
        s.step();
        assert_eq!(s.native_channel("car_name"), Some(LegacyValue::Int(0)));
        s.receive_native(&mut Telemetry::off()).unwrap();
        assert_eq!(s.native_channel("car_name"), Some(LegacyValue::Str("Default".into())));
        assert_eq!(s.native_channel("roll_right"), Some(LegacyValue::Float(10.0)));
        assert_eq!(s.native_channel("suspension_travel_rf"), Some(LegacyValue::Int(0)));
        s.native_channel("roll");
        s.set_profile(JoystickProfile { name: "New".into(), assignments: vec![] });
        assert_eq!(s.native_channel("car_name"), Some(LegacyValue::Str("Default".into())));
        s.receive_native(&mut Telemetry::off()).unwrap();
        assert_eq!(s.native_channel("car_name"), Some(LegacyValue::Str("New".into())));
        s.set_enabled(false);
        assert_eq!(s.native_channel("suspension_travel_rf"), Some(LegacyValue::Float(10.0)));
        assert!(!s.receive_native(&mut Telemetry::off()).unwrap());
        for name in ["car_name", "roll_right", "suspension_travel_rf", "missing"] {
            assert_eq!(s.native_channel(name), Some(LegacyValue::Int(0)), "{name}");
        }
    }

    #[test]
    fn native_acceleration_and_angle_requests_update_suspension_at_their_actual_reads() {
        let (mut s, _) = source(vec![], JoystickOptions { pitch_read_as_angle: false, ..JoystickOptions::default() });
        {
            let mut mapper = lock(&s.shared.mapper);
            mapper.deflections.store(JoystickControl::RollRight, 10.0, true);
            mapper.deflections.store(JoystickControl::PitchBack, -4.0, true);
        }
        s.receive_native(&mut Telemetry::off()).unwrap();
        assert_eq!(s.native_channel("lateral_acceleration"), Some(LegacyValue::Int(0)));
        assert_eq!(s.native_channel("pitch_rate"), Some(LegacyValue::Int(0)));
        assert_eq!(s.native_channel("suspension_travel_rr"), Some(LegacyValue::Int(0)));
        assert_eq!(s.native_channel("pitch"), Some(LegacyValue::Float(4.0)));
        assert_eq!(s.native_channel("suspension_travel_rr"), Some(LegacyValue::Float(1.6)));
        s.set_options(JoystickOptions { sway_when_roll: true, surge_when_pitch: true, pitch_read_as_angle: false });
        assert_eq!(s.native_channel("lateral_acceleration"), Some(LegacyValue::Float(10.0)));
        assert_eq!(s.native_channel("longitudinal_acceleration"), Some(LegacyValue::Float(-4.0)));
        assert_eq!(s.native_channel("suspension_travel_rr"), Some(LegacyValue::Float(11.6)));
    }

    #[test]
    fn native_feature_reads_follow_each_getter_and_fail_before_stored_attitude_changes() {
        use crate::NativeRead::{Raised, Value};
        let (mut source, _) = source(vec![], JoystickOptions::default());
        let values = Arc::new(Mutex::new(BTreeMap::from([
            ("sway_when_roll_joy", LegacyValue::Str("yes".into())),
            ("surge_when_pitch_joy", LegacyValue::Str("no".into())),
        ])));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let read_values = values.clone();
        let read_calls = calls.clone();
        source.set_runtime_reader(
            JoystickRuntimeReader::new(|| JoystickRuntime { enabled: true, name: "Default".into(), profile: None })
                .with_feature_lookup(move |key| {
                    read_calls.lock().unwrap().push(key.to_owned());
                    read_values.lock().unwrap().get(key).cloned()
                }),
        );
        source.receive_native(&mut Telemetry::off()).unwrap();
        lock(&source.shared.mapper).deflections.store(JoystickControl::RollRight, 10.0, true);
        lock(&source.shared.mapper).deflections.store(JoystickControl::PitchBack, -4.0, true);
        assert_eq!(source.read_native("lateral_acceleration"), Value(LegacyValue::Float(10.0)));
        assert_eq!(source.read_native("suspension_travel_rf"), Value(LegacyValue::Float(10.0)));
        lock(&values).insert("sway_when_roll_joy", LegacyValue::Str("maybe".into()));
        lock(&source.shared.mapper).deflections.store(JoystickControl::RollRight, 5.0, true);
        assert_eq!(source.read_native("lateral_acceleration"), Raised);
        assert_eq!(source.read_native("suspension_travel_rf"), Value(LegacyValue::Float(10.0)));
        lock(&values).insert("sway_when_roll_joy", LegacyValue::Bool(true));
        assert_eq!(source.read_native("lateral_acceleration"), Value(LegacyValue::Float(5.0)));
        assert_eq!(source.read_native("longitudinal_acceleration"), Value(LegacyValue::Int(0)));
        lock(&values).remove("surge_when_pitch_joy");
        assert_eq!(source.read_native("longitudinal_acceleration"), Raised);
        assert_eq!(source.read_native("suspension_travel_rf"), Value(LegacyValue::Float(2.5)));
        lock(&values).insert("surge_when_pitch_joy", LegacyValue::Str("Y".into()));
        assert_eq!(source.read_native("longitudinal_acceleration"), Value(LegacyValue::Float(-4.0)));
        assert_eq!(
            *calls.lock().unwrap(),
            [
                "sway_when_roll_joy",
                "sway_when_roll_joy",
                "sway_when_roll_joy",
                "surge_when_pitch_joy",
                "surge_when_pitch_joy",
                "surge_when_pitch_joy",
            ]
        );
    }

    #[test]
    fn native_empty_telemetry_bypasses_features_until_receive_or_mapping_writes() {
        use crate::NativeRead::{Raised, Value};
        let (mut source, _) = source(vec![], JoystickOptions::default());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let reads = calls.clone();
        source.set_runtime_reader(
            JoystickRuntimeReader::new(|| JoystickRuntime { enabled: true, name: "Default".into(), profile: None })
                .with_feature_lookup(move |key| {
                    reads.lock().unwrap().push(key.to_owned());
                    None
                }),
        );
        assert_eq!(source.read_native("speed"), Value(LegacyValue::Int(0)));
        assert_eq!(source.read_native("lateral_acceleration"), Value(LegacyValue::Int(0)));
        assert!(calls.lock().unwrap().is_empty());
        // A mapped zero still creates a dictionary entry.
        lock(&source.shared.mapper).deflections.store(JoystickControl::RollRight, 0.0, false);
        assert_eq!(source.read_native("lateral_acceleration"), Raised);
        source.set_enabled(false);
        // The runtime reader is the authoritative enabled flag.
        *lock(&source.shared.runtime) = None;
        source.receive_native(&mut Telemetry::off()).unwrap();
        assert_eq!(source.read_native("speed"), Value(LegacyValue::Int(0)));
    }

    #[test]
    fn source_adapter_observes_mapping_between_reads_and_stores_attitude_once_requested() {
        use crate::derive::{Listener, SourceListener};
        use crate::NativeRead;
        let (mut s, _) = source(vec![], JoystickOptions::default());
        lock(&s.shared.mapper).deflections.store(JoystickControl::RollRight, 10.0, true);
        let shared = Arc::clone(&s.shared);
        s.receive_native(&mut Telemetry::off()).unwrap();
        let mut listener = SourceListener::new(s);
        listener.receive();
        assert_eq!(listener.read_native("suspension_travel_rf"), NativeRead::Value(LegacyValue::Int(0)));
        lock(&shared.mapper).deflections.store(JoystickControl::RollRight, 5.0, true);
        assert_eq!(listener.read_native("roll"), NativeRead::Value(LegacyValue::Float(5.0)));
        assert_eq!(listener.read_native("suspension_travel_rf"), NativeRead::Value(LegacyValue::Float(2.5)));
        lock(&shared.mapper).deflections.store(JoystickControl::RollRight, 0.0, false);
        assert_eq!(listener.read_native("speed"), NativeRead::Value(LegacyValue::Int(2)));
        assert_eq!(listener.read_native("suspension_travel_rf"), NativeRead::Value(LegacyValue::Float(2.5)));
        assert_eq!(listener.read_native("roll"), NativeRead::Value(LegacyValue::Int(0)));
        assert_eq!(listener.read_native("suspension_travel_rf"), NativeRead::Value(LegacyValue::Int(0)));
    }

    /// One input pass, one mapping pass, then the dispatcher's tick.
    fn poll(source: &mut JoystickSource) -> Telemetry {
        source.step();
        let mut t = Telemetry::off();
        assert!(source.poll(&mut t).unwrap());
        t
    }

    fn stored<'a>(key: &'a str, kind: &'a str, trigger: &'a str, mode: &'a str) -> StoredJoystickEntry<'a> {
        StoredJoystickEntry { key, joystick: PAD, kind, trigger, mode, invert_axis: false }
    }

    #[test]
    fn a_non_integer_trigger_ends_the_mapping_thread() {
        // A non-integer trigger is not caught: once that entry's
        // controller is connected the error ends the mapping thread and nothing is mapped again. Entries
        // before it in the profile were applied in that pass.
        let (mut s, inputs) = source(vec![], JoystickOptions::default());
        s.set_stored_profile(
            "Default",
            [stored("right", "axis", "0", "Single Axis"), stored("left", "axis", "x", "Single Axis")],
        );
        assert_eq!(poll(&mut s).roll, 0.0, "the controller is not connected yet: no exception");
        assert!(!s.mapping_ended());
        inputs.set_axis(PAD, 0, 32767);
        assert_eq!(poll(&mut s).roll, 10.0, "the first entry is applied before the second raises");
        assert!(s.mapping_ended());
        inputs.set_axis(PAD, 0, 0);
        assert_eq!(poll(&mut s).roll, 10.0, "mapping is frozen");
    }

    #[test]
    fn raw_profile_exception_ends_mapping_until_restart_even_after_profile_replacement() {
        let (mut s, inputs) = source(vec![], JoystickOptions::default());
        s.set_raw_profile(
            "Default",
            Some(
                serde_json::json!({
                    "right":{"joystick":"Wheel", "trigger":0, "mode":"Single Axis"},
                    "front":{"joystick":"Wheel", "trigger":null}
                })
                .into(),
            ),
        );
        inputs.set_axis("Wheel", 0, 32767);
        assert_eq!(poll(&mut s).roll, 10.0);
        assert!(s.mapping_ended());
        s.set_raw_profile(
            "Default",
            Some(
                serde_json::json!({
                    "right":{"joystick":"Wheel", "trigger":0, "mode":"Single Axis"}
                })
                .into(),
            ),
        );
        inputs.set_axis("Wheel", 0, 0);
        assert_eq!(poll(&mut s).roll, 10.0, "replacing the bad profile does not restart threadJoy");
    }

    #[test]
    fn the_mapping_thread_runs_on_its_own_period() {
        // The mapping and input threads each sleep 0.01 s per pass. The
        // dispatcher's reads see whatever the last mapping pass left.
        let inputs = JoystickInputs::default();
        let assignments = vec![assignment(JoystickControl::RollRight, InputKind::Other, 0, AxisMode::Single)];
        let timing = JoystickTiming {
            input_pass: Duration::from_millis(1),
            mapping_pass: Duration::from_millis(1),
            mapping_idle: Duration::from_millis(1),
        };
        let mut s = JoystickSource::with_timing(
            JoystickProfile { name: "Default".into(), assignments },
            JoystickOptions::default(),
            inputs.clone(),
            timing,
        );
        s.set_enabled(true);
        inputs.set_axis(PAD, 0, 32767);
        let started = std::time::Instant::now();
        loop {
            let mut t = Telemetry::off();
            s.poll(&mut t).unwrap();
            if t.roll == 10.0 {
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(2), "the threads mapped the axis without a poll");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(JoystickTiming::default(), JoystickTiming::WINDOWS);
        assert_eq!(JoystickTiming::WINDOWS.mapping_pass, Duration::from_millis(10));
        assert_eq!(JoystickTiming::LINUX.mapping_pass, Duration::from_millis(25));
    }

    #[test]
    fn mapping_rereads_runtime_without_a_dispatcher_check_or_poll() {
        let inputs = JoystickInputs::default();
        inputs.set_axis(PAD, 0, 32767);
        let runtime = Arc::new(Mutex::new(JoystickRuntime {
            enabled: false,
            name: "Default".into(),
            profile: Some(
                serde_json::json!({
                    "right":{"joystick":PAD, "trigger":0, "mode":"Single Axis"}
                })
                .into(),
            ),
        }));
        let timing = JoystickTiming {
            input_pass: Duration::from_millis(1),
            mapping_pass: Duration::from_millis(1),
            mapping_idle: Duration::from_millis(1),
        };
        let mut s = JoystickSource::with_timing(JoystickProfile::default(), JoystickOptions::default(), inputs, timing);
        let current = Arc::clone(&runtime);
        s.set_runtime_reader(JoystickRuntimeReader::new(move || lock(&current).clone()));
        lock(&runtime).enabled = true;
        let wait_for_roll = |expected| {
            let started = std::time::Instant::now();
            loop {
                if lock(&s.shared.mapper).deflections.roll() == expected {
                    return;
                }
                assert!(started.elapsed() < Duration::from_secs(2), "mapping thread did not reread runtime");
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        wait_for_roll(10.0);
        lock(&runtime).profile = Some(
            serde_json::json!({
                "right":{"joystick":PAD, "trigger":0, "mode":"Dual Axis", "type":"axis <"}
            })
            .into(),
        );
        wait_for_roll(0.0);
    }

    #[test]
    fn receive_data_with_the_switch_off_drops_the_deflections() {
        // A poll clears the dictionary when the listener is off
        let (mut s, inputs) = source(
            vec![assignment(JoystickControl::RollRight, InputKind::Other, 0, AxisMode::Single)],
            JoystickOptions::default(),
        );
        inputs.set_axis(PAD, 0, 32767);
        assert_eq!(poll(&mut s).roll, 10.0);
        s.set_enabled(false);
        assert!(!s.poll(&mut Telemetry::off()).unwrap());
        s.set_enabled(true);
        let mut t = Telemetry::off();
        s.poll(&mut t).unwrap();
        assert_eq!(t.roll, 0.0, "no mapping pass ran since");
    }

    #[test]
    fn check_status_follows_the_switch() {
        let (mut s, _) = source(vec![], JoystickOptions::default());
        assert!(s.is_running().unwrap());
        s.set_enabled(false);
        assert!(!s.is_running().unwrap());
        assert!(!s.poll(&mut Telemetry::off()).unwrap());
    }

    #[test]
    fn disabling_the_listener_clears_stored_attitude_before_the_next_suspension_read() {
        for (control, channel) in [(JoystickControl::RollRight, "roll"), (JoystickControl::PitchBack, "pitch")] {
            let (mut source, inputs) = source(
                vec![assignment(control, InputKind::AxisPositive, 0, AxisMode::Dual)],
                JoystickOptions::default(),
            );
            inputs.set_axis(PAD, 0, 32767);
            assert_eq!(poll(&mut source).get(channel).unwrap().abs(), 10.0);
            let loaded = poll(&mut source);
            assert!([
                loaded.suspension_travel_lf,
                loaded.suspension_travel_rf,
                loaded.suspension_travel_lr,
                loaded.suspension_travel_rr
            ]
            .iter()
            .any(|value| *value != 0.0));
            source.set_enabled(false);
            assert!(!source.poll(&mut Telemetry::off()).unwrap());
            inputs.set_axis(PAD, 0, 0);
            source.set_enabled(true);
            let resumed = poll(&mut source);
            assert_eq!((resumed.roll, resumed.pitch), (0.0, 0.0));
            assert_eq!(
                [
                    resumed.suspension_travel_lf,
                    resumed.suspension_travel_rf,
                    resumed.suspension_travel_lr,
                    resumed.suspension_travel_rr
                ],
                [0.0; 4],
                "receiveData cleared telemetry['roll']/['pitch'] as well as the mapped halves: {channel}"
            );
        }
    }

    #[test]
    fn native_channel_types_follow_button_axis_clamps_and_suspension_stages() {
        use openrigcue_model::native_value::LegacyValue::{Float, Int};
        let (mut source, inputs) = source(
            vec![
                assignment(JoystickControl::RollRight, InputKind::AxisPositive, 0, AxisMode::Dual),
                assignment(JoystickControl::Speed, InputKind::Button, 1, AxisMode::Other),
            ],
            JoystickOptions::default(),
        );
        poll(&mut source);
        for (name, value) in [("speed", 2), ("rpm", 401), ("max_rpm", 11000), ("roll", 0), ("pitch", 0), ("gear", 0)] {
            assert_eq!(source.native_channel(name), Some(Int(value)), "{name}");
        }
        inputs.set_axis(PAD, 0, 32767);
        inputs.button_event(PAD, 1, true);
        poll(&mut source);
        assert_eq!(source.native_channel("roll"), Some(Float(10.0)));
        assert_eq!(source.native_channel("speed"), Some(Int(32767)));
        assert_eq!(
            source.native_channel("suspension_travel_rf"),
            Some(Float(10.0)),
            "the explicit native read observes the completed compatibility poll's attitude writes"
        );
        poll(&mut source);
        assert_eq!(source.native_channel("suspension_travel_rf"), Some(Float(10.0)));
        assert_eq!(source.native_channel("suspension_travel_lf"), Some(Int(0)));
        inputs.set_axis(PAD, 0, -1);
        inputs.button_event(PAD, 1, false);
        poll(&mut source);
        assert_eq!(source.native_channel("roll"), Some(Int(0)), "opposite-half reset assigns integer zero");
        assert_eq!(source.native_channel("speed"), Some(Int(2)));
        assert_eq!(source.native_channel("rpm"), Some(Int(401)));
        assert_eq!(
            source.native_channel("suspension_travel_rf"),
            Some(Int(0)),
            "the actual roll read already replaced the stored attitude"
        );
        poll(&mut source);
        assert_eq!(source.native_channel("suspension_travel_rf"), Some(Int(0)));
        source.set_enabled(false);
        assert!(!source.poll(&mut Telemetry::off()).unwrap());
        assert_eq!(source.native_channel("rpm"), Some(Int(0)), "disabled receive emptied the native dictionary");
        assert_eq!(source.native_channel("roll"), Some(Int(0)));
        assert_eq!(source.native_channel("suspension_travel_rf"), Some(Int(0)));
    }

    #[test]
    fn native_mixed_button_and_axis_angles_keep_their_numeric_kinds_through_negation() {
        use openrigcue_model::native_value::LegacyValue::{Float, Int};
        let (mut source, inputs) = source(
            vec![
                assignment(JoystickControl::RollRight, InputKind::AxisPositive, 0, AxisMode::Dual),
                assignment(JoystickControl::PitchBack, InputKind::Button, 1, AxisMode::Other),
                assignment(JoystickControl::YawRight, InputKind::Button, 2, AxisMode::Other),
            ],
            JoystickOptions { sway_when_roll: true, surge_when_pitch: true, pitch_read_as_angle: true },
        );
        inputs.set_axis(PAD, 0, 32767);
        inputs.button_event(PAD, 1, true);
        inputs.button_event(PAD, 2, true);
        poll(&mut source);
        assert_eq!(source.native_channel("roll"), Some(Float(10.0)));
        assert_eq!(source.native_channel("pitch"), Some(Int(-32767)));
        assert_eq!(source.native_channel("yaw"), Some(Int(32767)));
        assert_eq!(source.native_channel("lateral_acceleration"), Some(Float(10.0)));
        assert_eq!(source.native_channel("longitudinal_acceleration"), Some(Int(32767)));
        inputs.button_event(PAD, 1, false);
        inputs.button_event(PAD, 2, false);
        poll(&mut source);
        assert_eq!(source.native_channel("roll"), Some(Float(10.0)));
        assert_eq!(source.native_channel("pitch"), Some(Int(0)));
        assert_eq!(source.native_channel("yaw"), Some(Int(0)));
        assert_eq!(source.native_channel("longitudinal_acceleration"), Some(Int(0)));
    }

    #[test]
    fn native_axis_zero_keeps_float_type_until_the_explicit_integer_reset() {
        use openrigcue_model::native_value::LegacyValue::{Float, Int};
        let (mut source, inputs) = source(
            vec![assignment(JoystickControl::RollRight, InputKind::Other, 0, AxisMode::Single)],
            JoystickOptions::default(),
        );
        inputs.set_axis(PAD, 0, -32768);
        poll(&mut source);
        assert_eq!(source.native_channel("roll"), Some(Float(0.0)));
        source.set_profile(JoystickProfile {
            name: "Dual".into(),
            assignments: vec![assignment(JoystickControl::RollRight, InputKind::AxisPositive, 0, AxisMode::Dual)],
        });
        inputs.set_axis(PAD, 0, -1);
        poll(&mut source);
        assert_eq!(source.native_channel("roll"), Some(Int(0)));
    }

    #[test]
    fn get_channel_defaults_without_inputs() {
        let (mut s, _) = source(vec![], JoystickOptions::default());
        let t = poll(&mut s);
        assert_eq!((t.speed, t.rpm, t.max_rpm), (2.0, 401.0, 11000.0));
        assert_eq!((t.pitch, t.roll, t.yaw, t.lateral_acceleration), (0.0, 0.0, 0.0, 0.0));
        assert_eq!(t.gear, 0.0);
        assert_eq!(t.vehicle, "Default");
    }

    #[test]
    fn update_joystick_state_dual_axis_halves() {
        use JoystickControl::*;
        let (mut s, inputs) = source(
            vec![
                assignment(RollLeft, InputKind::AxisNegative, 0, AxisMode::Dual),
                assignment(RollRight, InputKind::AxisPositive, 0, AxisMode::Dual),
            ],
            JoystickOptions::default(),
        );
        inputs.set_axis(PAD, 0, -32768);
        let t = poll(&mut s);
        assert_eq!((t.roll, t.steering_wheel_angle), (-10.0, -10.0));
        inputs.set_axis(PAD, 0, 32767);
        assert_eq!(poll(&mut s).roll, 10.0, "left half reset to 0, right half full");
        inputs.set_axis(PAD, 0, 0);
        assert_eq!(poll(&mut s).roll, 10.0, "a centred axis updates neither half");
    }

    #[test]
    fn update_joystick_state_single_axis_with_invert_and_reverse() {
        use JoystickControl::*;
        let mut front = assignment(PitchFront, InputKind::Other, 1, AxisMode::Single);
        front.invert = true;
        let (mut s, inputs) =
            source(vec![front, assignment(Speed, InputKind::Other, 2, AxisMode::Single)], JoystickOptions::default());
        inputs.set_axis(PAD, 1, -32768);
        inputs.set_axis(PAD, 2, 32767);
        let t = poll(&mut s);
        assert_eq!(t.pitch, -10.0, "inverted full axis maps to 10, published as -pitch_front");
        assert_eq!((t.speed, t.rpm), (100.0, 10900.0));
        let mut back = assignment(PitchBack, InputKind::Other, 1, AxisMode::Single);
        back.invert = false;
        let (mut s, inputs) = source(vec![back], JoystickOptions::default());
        inputs.set_axis(PAD, 1, 32767);
        assert_eq!(poll(&mut s).pitch, 10.0, "pitch_back is reversed (-10) and published negated");
    }

    #[test]
    fn a_button_event_is_seen_only_by_the_pass_it_falls_in() {
        // An input pass rebuilds the shared state every pass; a mapping pass reads the state of the
        // latest input pass, so a press and release inside one input pass leave only the release.
        let (mut s, inputs) = source(
            vec![assignment(JoystickControl::YawRight, InputKind::Button, 3, AxisMode::Dual)],
            JoystickOptions::default(),
        );
        inputs.button_event(PAD, 3, true);
        inputs.button_event(PAD, 3, false);
        assert_eq!(poll(&mut s).yaw, 0.0);
        inputs.button_event(PAD, 3, true);
        s.shared.input_pass(s.reader.as_ref().unwrap());
        s.shared.input_pass(s.reader.as_ref().unwrap());
        s.shared.mapping_pass();
        let mut t = Telemetry::off();
        s.poll(&mut t).unwrap();
        assert_eq!(t.yaw, 0.0, "the press fell in an input pass no mapping pass read");
    }

    #[test]
    fn set_telemetry_var1_button_writes_the_raw_value() {
        use JoystickControl::*;
        let (mut s, inputs) =
            source(vec![assignment(YawRight, InputKind::Button, 3, AxisMode::Dual)], JoystickOptions::default());
        inputs.button_event(PAD, 3, true);
        assert_eq!(poll(&mut s).yaw, 32767.0);
        assert_eq!(poll(&mut s).yaw, 32767.0, "held without new events");
        inputs.button_event(PAD, 3, false);
        assert_eq!(poll(&mut s).yaw, 0.0);
    }

    #[test]
    fn single_axis_mode_also_reads_an_axis_with_the_button_index() {
        use JoystickControl::*;
        let (mut s, inputs) =
            source(vec![assignment(RollRight, InputKind::Button, 0, AxisMode::Single)], JoystickOptions::default());
        inputs.set_axis(PAD, 0, 32767);
        assert_eq!(poll(&mut s).roll, 10.0);
    }

    #[test]
    fn other_controllers_are_ignored() {
        use JoystickControl::*;
        let (mut s, inputs) =
            source(vec![assignment(RollRight, InputKind::Other, 0, AxisMode::Single)], JoystickOptions::default());
        inputs.set_axis("Other", 0, 32767);
        assert_eq!(poll(&mut s).roll, 0.0);
    }

    #[test]
    fn sway_and_surge_options() {
        use JoystickControl::*;
        let options = JoystickOptions { sway_when_roll: true, surge_when_pitch: true, pitch_read_as_angle: true };
        let (mut s, inputs) = source(
            vec![
                assignment(RollRight, InputKind::Other, 0, AxisMode::Single),
                assignment(PitchFront, InputKind::Other, 1, AxisMode::Single),
            ],
            options,
        );
        inputs.set_axis(PAD, 0, 32767);
        inputs.set_axis(PAD, 1, 32767);
        let t = poll(&mut s);
        assert_eq!(t.lateral_acceleration, 10.0);
        assert_eq!(t.longitudinal_acceleration, 10.0, "-pitch, i.e. pitch_front");
        assert_eq!(t.pitch, -10.0);
    }

    #[test]
    fn get_suspension_value_uses_the_previously_stored_attitude() {
        use JoystickControl::*;
        let (mut s, inputs) = source(
            vec![
                assignment(RollRight, InputKind::Other, 0, AxisMode::Single),
                assignment(PitchBack, InputKind::Other, 1, AxisMode::Single),
            ],
            JoystickOptions::default(),
        );
        inputs.set_axis(PAD, 0, 32767);
        inputs.set_axis(PAD, 1, 32767);
        let first = poll(&mut s);
        assert_eq!((first.roll, first.pitch), (10.0, 10.0));
        assert_eq!(first.suspension_travel_rr, 0.0, "roll and pitch not stored yet on the first read");
        let second = poll(&mut s);
        assert_eq!(second.suspension_travel_rr, 20.0, "(10² + 10²) / 10 on the ++ wheel");
        assert_eq!(
            (second.suspension_travel_lf, second.suspension_travel_rf, second.suspension_travel_lr),
            (0.0, 0.0, 0.0)
        );
    }

    #[test]
    fn get_suspension_pos_table() {
        use Corner::*;
        assert_eq!(loaded_corners(1.0, -1.0), &[RightFront]);
        assert_eq!(loaded_corners(-1.0, 1.0), &[LeftRear]);
        assert_eq!(loaded_corners(0.0, -1.0), &[LeftFront, RightFront]);
        assert_eq!(loaded_corners(-1.0, 0.0), &[LeftFront, LeftRear]);
        assert_eq!(loaded_corners(f64::NAN, 0.0), &[] as &[Corner]);
        assert_eq!(suspension(LeftFront, -3.0, -4.0), 2.5);
    }

    #[test]
    fn mapping_dual_keys_name_the_controls() {
        use JoystickControl::*;
        let keys = ["front", "back", "left", "right", "rotate_left", "rotate_right", "speed"];
        let controls = keys.map(|k| JoystickControl::from_profile_key(k).unwrap());
        assert_eq!(controls, [PitchFront, PitchBack, RollLeft, RollRight, YawLeft, YawRight, Speed]);
        for unknown in ["Front", "pitch_front", "rotate left", ""] {
            assert_eq!(JoystickControl::from_profile_key(unknown), None, "{unknown:?}");
        }
    }

    #[test]
    fn update_joystick_state_compares_type_and_mode_exactly() {
        assert_eq!(InputKind::from_stored("button"), InputKind::Button);
        assert_eq!(InputKind::from_stored("axis >"), InputKind::AxisPositive);
        assert_eq!(InputKind::from_stored("axis <"), InputKind::AxisNegative);
        for other in ["axis", "Button", "axis>", " axis <"] {
            assert_eq!(InputKind::from_stored(other), InputKind::Other, "{other:?}");
        }
        assert_eq!(AxisMode::from_stored("Single Axis"), AxisMode::Single);
        assert_eq!(AxisMode::from_stored("Dual Axis"), AxisMode::Dual);
        assert_eq!(AxisMode::from_stored("single axis"), AxisMode::Other);
    }

    #[test]
    fn update_joystick_state_stored_entry_conversion() {
        let entry =
            |key: &str, trigger: &str| JoystickAssignment::from_stored(key, PAD, "axis >", trigger, "Dual Axis", true);
        assert_eq!(
            entry("front", " 2 ").unwrap(),
            Some(JoystickAssignment {
                control: JoystickControl::PitchFront,
                device: PAD.into(),
                kind: InputKind::AxisPositive,
                index: 2,
                mode: AxisMode::Dual,
                invert: true,
            }),
            "int(' 2 ') == 2"
        );
        assert_eq!(entry("unknown", "2").unwrap(), None, "no mapping table entry");
        assert_eq!(entry("front", "-1").unwrap(), None, "no input index is negative");
        assert_eq!(entry("front", "99999999999999999999").unwrap(), None);
        let error = entry("unknown", "2.0").unwrap_err();
        assert_eq!(error.trigger, "2.0", "int() runs before the key is looked up");
        assert!(entry("front", "").is_err());
        assert!(entry("front", &"0".repeat(4300)).unwrap().is_some());
        assert!(entry("front", &"0".repeat(4301)).is_err(), "int(trigger) fails before input matching");
    }

    #[test]
    fn update_joystick_state_unknown_mode_still_maps_buttons() {
        let button = JoystickAssignment::from_stored("rotate_right", PAD, "button", "3", "", false).unwrap().unwrap();
        let axis_of_unknown_mode =
            JoystickAssignment::from_stored("left", PAD, "axis <", "0", "?", false).unwrap().unwrap();
        let (mut s, inputs) = source(vec![button, axis_of_unknown_mode], JoystickOptions::default());
        inputs.button_event(PAD, 3, true);
        inputs.set_axis(PAD, 0, -32768);
        let t = poll(&mut s);
        assert_eq!(t.yaw, 32767.0);
        assert_eq!(t.roll, 0.0, "no mode test matches");
    }

    #[test]
    fn pitch_tie_prefers_back() {
        let d = Deflections { pitch_front: -4.0, pitch_back: 4.0, ..Deflections::default() };
        assert_eq!(d.pitch(), -4.0);
    }

    /// Button state per reader: every reader (listener, 4D recorder) sees each button event once, and the
    /// page's view keeps the last value of each button.
    #[test]
    fn every_reader_sees_each_button_event() {
        let inputs = JoystickInputs::default();
        let (first, second) = (inputs.reader(), inputs.reader());
        inputs.set_axis(PAD, 0, 100);
        inputs.button_event(PAD, 2, true);
        assert_eq!(first.take()[PAD].button_events, BTreeMap::from([(2, true)]));
        assert!(first.take()[PAD].button_events.is_empty(), "taken once");
        let seen = second.take();
        assert_eq!(seen[PAD].button_events, BTreeMap::from([(2, true)]));
        assert_eq!(seen[PAD].axes, BTreeMap::from([(0, 100)]));
        assert_eq!(inputs.snapshot()[PAD].buttons, BTreeMap::from([(2, true)]));
        drop(first);
        inputs.button_event(PAD, 2, false);
        assert_eq!(inputs.lock().readers.len(), 1, "dropped readers are forgotten");
    }

    /// The device list has one entry per connected device; the state goes with the last device of that name.
    #[test]
    fn devices_follow_connect_and_remove() {
        let inputs = JoystickInputs::default();
        inputs.connect(PAD);
        inputs.connect(PAD);
        inputs.connect("Pedals");
        inputs.set_axis(PAD, 0, 1);
        assert_eq!(inputs.devices(), [PAD, PAD, "Pedals"]);
        inputs.remove(PAD);
        assert!(inputs.snapshot().contains_key(PAD), "one Wheel is still connected");
        inputs.remove(PAD);
        assert!(!inputs.snapshot().contains_key(PAD));
        assert_eq!(inputs.devices(), ["Pedals"]);
    }
}

#[cfg(test)]
#[path = "joystick/storage_tests.rs"]
mod storage_tests;
