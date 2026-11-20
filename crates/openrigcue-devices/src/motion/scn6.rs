//! PROSIMU "T1000-2M/3M SCN6 Dyadic" platforms: one Dyadic SCN6 servo controller per actuator.
//!
//! The driver has two variants: the current one, and the previous-generation one used
//! when `[PROSIMU] use_scn6_legacy` is on.
//! Both talk TMBS ([`tmbs`]). The vendor's `TMBSCOM0.dll` is not used.
//!
//! - [`tmbs`]: the TMBS frame, commands and replies.
//! - [`axis`]: scan, power-on and homing of one SCN6 (shared with the PROSIMU traction-loss axes).
//! - [`DyadicDriver`]: the driver in its two variants.

pub mod axis;
pub mod tmbs;

use std::collections::BTreeMap;
use std::time::Duration;

use tracing::debug;

use self::axis::{
    assigned_id, assigned_key, move_org, scan_records, wait_for_homing, zfill2, ActuatorRecord, ScanModule, AXIS,
};
use self::tmbs::Command;
use super::clock::Clock;
use super::driver_support::{map_clamped, py_int};
use super::native;
use super::{DriverFactory, LinkStatus, LiveSettings, MotionContext, MotionDriver, MotionError, PyFrame};
use openrigcue_model::native_value::LegacyValue;

/// Which of the two variants of the SCN6 driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DyadicVariant {
    /// The current variant (default, `use_scn6_legacy = no`).
    Current,
    /// The previous-generation variant (`use_scn6_legacy = yes`).
    Previous,
}

/// Settings the driver reads on every call.
#[derive(Debug, Clone, PartialEq)]
pub struct DyadicSettings {
    /// Where the settings are read live (the refreshed values are the fallback).
    pub runtime: crate::motion::driver_support::RuntimeReader,
    /// `[PROSIMU] invert_tl`.
    pub invert_tl: bool,
    /// `[MOTIONBOX] reaction_speed` (slider 70..100).
    pub reaction_speed: i64,
    /// The raw `MOTIONBOX.reaction_speed` that the speed and acceleration lookups convert to an integer; `None`
    /// uses [`reaction_speed`](Self::reaction_speed).
    pub native_reaction_speed: Option<native::NativeSetting>,
    /// `[PROSIMU] post_homing_speed` (default 500).
    pub post_homing_speed: i64,
    /// `[PROSIMU] post_homing_speed_timeout` in seconds (default 5).
    pub post_homing_speed_timeout: i64,
    /// `[SIMTECHREVOLUTION] scn6_speed` / `scn6_acceleration`. These keys have no default; the current variant
    /// reads and converts them on the first frame. Failures are retained until that read is reached.
    pub simtech_scn6_speed: native::NativeSetting,
    pub simtech_scn6_acceleration: native::NativeSetting,
    /// The axis assignment: assignment-option index → `"<axis>,<serial>"`.
    pub axis_assignment: BTreeMap<String, String>,
}

impl Default for DyadicSettings {
    fn default() -> Self {
        DyadicSettings {
            runtime: Default::default(),
            invert_tl: false,
            reaction_speed: 70,
            native_reaction_speed: None,
            post_homing_speed: 500,
            post_homing_speed_timeout: 5,
            simtech_scn6_speed: Err("KeyError: 'scn6_speed'".into()),
            simtech_scn6_acceleration: Err("KeyError: 'scn6_acceleration'".into()),
            axis_assignment: BTreeMap::new(),
        }
    }
}

/// The SCN6 Dyadic models of the model table, with the per-model fields the driver reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScnModel {
    /// Whether the model allows a change of the traction-loss direction.
    allow_change_tl_dir: bool,
}

impl ScnModel {
    fn of(name: &str) -> Result<Self, MotionError> {
        match name {
            "T1000-2M SCN6 Dyadic" => Ok(ScnModel { allow_change_tl_dir: false }),
            "T1000-3M SCN6 Dyadic" => Ok(ScnModel { allow_change_tl_dir: true }),
            other => Err(MotionError::UnknownModel(other.to_string())),
        }
    }
}

/// Frame lines of a variant's frame path.
struct FrameLines {
    /// The position pass inside a stream frame and a test frame.
    send_stream: u32,
    send_test: u32,
    /// The `SIMTECHREVOLUTION.scn6_speed` and `scn6_acceleration` settings as integers (current variant).
    speed_setting: u32,
    accel_setting: u32,
    /// The velocity write after the first lookup (current variant) and the one for a changed pair.
    write_first: u32,
    write_changed: u32,
    /// The id lookup of an actuator, and the integer conversion of its key inside the lookup.
    get_id: u32,
    get_id_int: u32,
    /// The protocol's velocity write inside the driver's velocity write.
    write_velocity: u32,
    /// The slot read of the position: negated for homing pattern 7, else
    /// subtracted from the maximum.
    reversed_position: u32,
    position: u32,
    /// The absolute move of an actuator's interface.
    position_call: u32,
    /// The traction-loss slot multiplied by its direction.
    tl_direction: u32,
}

/// The frame line of the initial velocity write in the previous variant's connect.
const INITIAL_VELOCITY_LINE: u32 = 225;

/// Assignment-option index of the traction-loss actuator.
const TL_ASSIGNMENT_KEY: &str = "2";
/// The traction-loss slot whose sign `invert_tl` flips.
const TL_SLOT: usize = 2;
/// Speed and acceleration of the traction-loss actuator.
const TL_SPEED: i64 = 5000;
const TL_ACCEL: i64 = 80;
/// The current variant's acceleration uses a fixed 5 s window and 100 after homing.
const POST_HOMING_ACCEL_WINDOW_S: u64 = 5;
const POST_HOMING_ACCEL: i64 = 100;
/// Homing pattern whose actuators take `-value` instead of the maximum minus the value.
const REVERSED_HOMING_PATTERN: i64 = 7;
/// The previous variant writes a velocity 11 times.
const PREVIOUS_VELOCITY_WRITES: usize = 11;
/// Seconds per day: the seconds since homing drop whole days.
const DAY_S: u64 = 86_400;

/// The Dyadic SCN6 driver.
pub struct DyadicDriver {
    variant: DyadicVariant,
    settings: LiveSettings<DyadicSettings>,
    /// The actuator records, keyed by position.
    actuators: Vec<ActuatorRecord>,
    /// When homing completed, as a clock reading.
    homing_completed: Option<Duration>,
    /// The axis assignment status.
    assignment_ok: bool,
    /// The caller's list after its traction-loss slot was multiplied by the direction (in place).
    edited: Option<Vec<i64>>,
    /// Whether the last `&[i64]` frame had its TL slot written.
    wrote: bool,
}

impl DyadicDriver {
    pub fn new(variant: DyadicVariant, settings: LiveSettings<DyadicSettings>) -> Self {
        DyadicDriver {
            variant,
            settings,
            actuators: Vec::new(),
            homing_completed: None,
            assignment_ok: false,
            edited: None,
            wrote: false,
        }
    }

    pub fn factory(variant: DyadicVariant, settings: LiveSettings<DyadicSettings>) -> DriverFactory {
        Box::new(move || Box::new(DyadicDriver::new(variant, settings.clone())))
    }

    /// The axis assignment status after the last frame.
    pub fn assignment_ok(&self) -> bool {
        self.assignment_ok
    }

    /// Serial numbers of the connected actuators, in actuator order (for the axis-assignment dialog).
    pub fn serial_numbers(&self) -> Vec<String> {
        self.actuators.iter().map(|a| a.sn.clone()).collect()
    }

    /// Whole seconds since homing completed, without whole days.
    fn seconds_since_homing(&self, clock: &dyn Clock) -> Result<u64, MotionError> {
        let done = self.homing_completed.ok_or_else(|| MotionError::Protocol("homing_completed is None".into()))?;
        Ok(clock.now().saturating_sub(done).as_secs() % DAY_S)
    }

    /// The speed of an actuator, `-1` when it has no assignment.
    fn speed_for(&self, s: &DyadicSettings, serial: &str, clock: &dyn Clock) -> Result<i64, MotionError> {
        let elapsed = self.seconds_since_homing(clock)?;
        if (elapsed as i64) < s.post_homing_speed_timeout {
            // The previous variant logs the post-homing speed (the message has two spaces before the
            // value). The current variant logs nothing here.
            if self.variant == DyadicVariant::Previous {
                debug!("returning post_homing speed to  {}", s.post_homing_speed);
            }
            return Ok(s.post_homing_speed);
        }
        Ok(match assigned_key(&s.axis_assignment, &AXIS.to_string(), serial) {
            Some(TL_ASSIGNMENT_KEY) => TL_SPEED,
            Some(_) => reaction_speed_map(s, 5000, 10000)?,
            None => -1,
        })
    }

    /// The acceleration of an actuator: a fixed 5 s post-homing window in the current variant, the configured
    /// window in the previous one.
    fn accel_for(&self, s: &DyadicSettings, serial: &str, clock: &dyn Clock) -> Result<i64, MotionError> {
        let elapsed = self.seconds_since_homing(clock)?;
        let window = match self.variant {
            DyadicVariant::Current => POST_HOMING_ACCEL_WINDOW_S as i64,
            DyadicVariant::Previous => s.post_homing_speed_timeout,
        };
        if (elapsed as i64) < window {
            // The previous variant logs the post-homing acceleration.
            if self.variant == DyadicVariant::Previous {
                debug!("returning post_homing accel to  {POST_HOMING_ACCEL}");
            }
            return Ok(POST_HOMING_ACCEL);
        }
        Ok(match assigned_key(&s.axis_assignment, &AXIS.to_string(), serial) {
            Some(TL_ASSIGNMENT_KEY) => TL_ACCEL,
            Some(_) => reaction_speed_map(s, 80, 250)?,
            None => -1,
        })
    }

    /// The scan variant whose text and frame lines this driver uses.
    fn module(&self) -> ScanModule {
        match self.variant {
            DyadicVariant::Current => ScanModule::Dyadic,
            DyadicVariant::Previous => ScanModule::DyadicPrevious,
        }
    }

    /// A frame of the driver's `function` at `line`.
    fn at(&self, function: &'static str, line: u32) -> PyFrame {
        PyFrame::new(self.module().file(), function, line)
    }

    /// Frame lines of the steps of the frame path whose errors leave it.
    fn lines(&self) -> FrameLines {
        match self.variant {
            DyadicVariant::Current => FrameLines {
                send_stream: 404,
                send_test: 407,
                speed_setting: 334,
                accel_setting: 347,
                write_first: 359,
                write_changed: 366,
                get_id: 368,
                get_id_int: 237,
                write_velocity: 20,
                reversed_position: 381,
                position: 383,
                position_call: 385,
                tl_direction: 326,
            },
            DyadicVariant::Previous => FrameLines {
                send_stream: 354,
                send_test: 357,
                speed_setting: 0,
                accel_setting: 0,
                write_first: 0,
                write_changed: 336,
                get_id: 323,
                get_id_int: 247,
                write_velocity: 27,
                reversed_position: 344,
                position: 346,
                position_call: 348,
                tl_direction: 319,
            },
        }
    }

    /// The assignment id of actuator `index` in the position pass; a key that is not an integer fails inside
    /// the lookup's frame.
    fn assigned_id(&self, s: &DyadicSettings, index: usize) -> Result<LegacyValue, MotionError> {
        let lines = self.lines();
        let record = &self.actuators[index];
        assigned_id(&s.axis_assignment, &record.axis, &record.sn)
            .map_err(|e| e.in_frames([self.at("send_actuator_pos", lines.get_id), self.at("get_id", lines.get_id_int)]))
    }

    /// Write the velocity of actuator `index` and remember it
    /// (one write in the current variant, eleven in the previous one). A reply that is not hexadecimal is a
    /// value error through this frame; the caller adds the frame of its own step.
    fn write_velocity(
        &mut self,
        index: usize,
        speed: i64,
        accel: i64,
        clock: &mut dyn Clock,
    ) -> Result<(), MotionError> {
        let writes = match self.variant {
            DyadicVariant::Current => 1,
            DyadicVariant::Previous => PREVIOUS_VELOCITY_WRITES,
        };
        let frame = self.at("write_velocity", self.lines().write_velocity);
        let actuator = &mut self.actuators[index];
        if self.variant == DyadicVariant::Previous {
            debug!("setting actuator new velocity {} {speed} {accel}", actuator.com_port);
        }
        for _ in 0..writes {
            let status = actuator
                .link("write_velocity")
                .map_err(|e| e.in_frame(frame))?
                .action(AXIS, &Command::Velocity { speed, accel }, clock)
                .map_err(|e| self::axis::uncaught(e).through(frame))?;
            if self.variant == DyadicVariant::Previous {
                debug!("status of the velocity sent the actuator {}", if status { "True" } else { "False" });
            }
        }
        actuator.speed = speed;
        actuator.accel = accel;
        Ok(())
    }

    /// The two sibling blocks at the head of the current variant's position loop: one for a speed still `-1`
    /// and one for an acceleration still `-1`. Each requires its `[SIMTECHREVOLUTION]` key (the values only
    /// apply to the SimTech manufacturer) and then takes the actuator's speed or acceleration; a `-1` ends the
    /// frame. The first velocity write
    /// belongs to the acceleration block, so an acceleration resolved on a later frame than the speed
    /// is written with the speed stored earlier. Returns `false` when either lookup gave `-1`.
    fn unset_velocity(&mut self, index: usize, s: &DyadicSettings, clock: &mut dyn Clock) -> Result<bool, MotionError> {
        let serial = self.actuators[index].sn.clone();
        let lines = self.lines();
        let file = self.module().file();
        if self.actuators[index].speed == -1 {
            int_setting(&s.runtime.value("SIMTECHREVOLUTION", "scn6_speed", &s.simtech_scn6_speed))
                .map_err(|e| e.in_frame(PyFrame::new(file, "send_actuator_pos", lines.speed_setting)))?;
            self.actuators[index].speed = self.speed_for(s, &serial, clock)?;
            if self.actuators[index].speed == -1 {
                return Ok(false);
            }
        }
        if self.actuators[index].accel == -1 {
            int_setting(&s.runtime.value("SIMTECHREVOLUTION", "scn6_acceleration", &s.simtech_scn6_acceleration))
                .map_err(|e| e.in_frame(PyFrame::new(file, "send_actuator_pos", lines.accel_setting)))?;
            self.actuators[index].accel = self.accel_for(s, &serial, clock)?;
            if self.actuators[index].accel == -1 {
                return Ok(false);
            }
            let (speed, accel) = (self.actuators[index].speed, self.actuators[index].accel);
            let statement = self.at("send_actuator_pos", lines.write_first);
            self.write_velocity(index, speed, accel, clock).map_err(|e| e.through(statement))?;
        }
        Ok(true)
    }

    /// The position pass of the current variant. Returns the assignment status.
    fn send_current(&mut self, motors: &mut LegacyValue, ctx: &mut MotionContext<'_>) -> Result<bool, MotionError> {
        let s = self.settings.get();
        let tl_frame = self.at("send_actuator_pos", self.lines().tl_direction);
        apply_tl_direction(motors, &s, ctx.model.name, tl_frame)?;
        self.wrote = true;
        let mut ids = Vec::with_capacity(self.actuators.len());
        for index in 0..self.actuators.len() {
            let serial = self.actuators[index].sn.clone();
            if !self.unset_velocity(index, &s, ctx.host.clock())? {
                return Ok(false);
            }
            // The new speed and acceleration are read on every frame and written when either differs
            // from the stored pair (right after the block above they are the values just stored).
            let speed = self.speed_for(&s, &serial, ctx.host.clock())?;
            let accel = self.accel_for(&s, &serial, ctx.host.clock())?;
            let actuator = &self.actuators[index];
            if accel != actuator.accel || speed != actuator.speed {
                let statement = self.at("send_actuator_pos", self.lines().write_changed);
                self.write_velocity(index, speed, accel, ctx.host.clock()).map_err(|e| e.through(statement))?;
            }
            let id = self.assigned_id(&s, index)?;
            if native::eq(&id, &native::int(-1))? {
                return Ok(false);
            }
            ids.push((index, id));
        }
        self.move_all(&ids, motors, ctx)?;
        Ok(true)
    }

    /// The position pass of the previous variant (only while connected).
    fn send_previous(&mut self, motors: &mut LegacyValue, ctx: &mut MotionContext<'_>) -> Result<bool, MotionError> {
        if !ctx.link.is_connected() {
            return Ok(self.assignment_ok);
        }
        let s = self.settings.get();
        let tl_frame = self.at("send_actuator_pos", self.lines().tl_direction);
        apply_tl_direction(motors, &s, ctx.model.name, tl_frame)?;
        self.wrote = true;
        let mut ids = Vec::with_capacity(self.actuators.len());
        for index in 0..self.actuators.len() {
            let serial = self.actuators[index].sn.clone();
            let id = self.assigned_id(&s, index)?;
            if native::eq(&id, &native::int(-1))? {
                return Ok(false);
            }
            let speed = self.speed_for(&s, &serial, ctx.host.clock())?;
            let accel = self.accel_for(&s, &serial, ctx.host.clock())?;
            let actuator = &self.actuators[index];
            if speed != actuator.speed || accel != actuator.accel {
                // The changed velocity is logged with the key of the actuator record.
                debug!("setting the velocity  {index} to {speed} {accel}");
                let statement = self.at("send_actuator_pos", self.lines().write_changed);
                self.write_velocity(index, speed, accel, ctx.host.clock()).map_err(|e| e.through(statement))?;
            }
            ids.push((index, id));
        }
        self.move_all(&ids, motors, ctx)?;
        Ok(true)
    }

    /// Position commands: the slot negated for homing pattern 7, otherwise the maximum minus the slot,
    /// at any width; the absolute move packs the position as a big-endian signed 32-bit integer.
    fn move_all(
        &mut self,
        ids: &[(usize, LegacyValue)],
        motors: &LegacyValue,
        ctx: &mut MotionContext<'_>,
    ) -> Result<(), MotionError> {
        let lines = self.lines();
        for (index, id) in ids {
            let index = *index;
            let reversed = self.actuators[index].homing_pattern == Some(REVERSED_HOMING_PATTERN);
            let line = if reversed { lines.reversed_position } else { lines.position };
            let statement = self.at("send_actuator_pos", line);
            // The slot of the id: a negative id (an assignment key edited by hand) indexes from the end of the
            // list; one outside the list is an index error at the branch's frame line.
            let value = native::item_at(motors, id).map_err(|e| e.in_frame(statement))?;
            let position = if reversed {
                native::mul(&value, &native::int(-1))
            } else {
                native::sub(&native::int(ctx.model.max_motor_abs), &value)
            }
            .map_err(|e| e.in_frame(statement))?;
            let frame = self.at("send_actuator_pos", lines.position_call);
            let link = self.actuators[index].link("move_abs_integer").map_err(|e| e.in_frame(frame))?;
            let packed =
                native::pack_i32(&position, true).map_err(|e| e.in_frames([frame, tmbs::MOVE_ABS_INTEGER_PACK]))?;
            link.move_absolute(AXIS, i32::from_be_bytes(packed));
        }
        Ok(())
    }

    fn send_actuator_pos(&mut self, motors: &mut LegacyValue, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        let ok = match self.variant {
            DyadicVariant::Current => self.send_current(motors, ctx)?,
            DyadicVariant::Previous => self.send_previous(motors, ctx)?,
        };
        self.assignment_ok = ok;
        Ok(())
    }

    /// An `&[i64]` frame through the dynamic-value path; the list as the path left it is the edited
    /// frame.
    fn send_compat(&mut self, motors: &[i64], ctx: &mut MotionContext<'_>, call: u32) -> Result<(), MotionError> {
        self.wrote = false;
        let (result, list) = native::compat(motors, |list| self.send_actuator_pos(list, ctx));
        if self.wrote {
            self.edited = Some(native::edited_vec(&list, motors));
        }
        result
            .map_err(|e| e.through(self.at(if call == 0 { "send_stream" } else { "send_test" }, self.call_line(call))))
    }

    fn call_line(&self, call: u32) -> u32 {
        if call == 0 {
            self.lines().send_stream
        } else {
            self.lines().send_test
        }
    }
}

/// The reaction speed rescaled from 70..100 to `out_min..out_max` and truncated: an integer of any width, then
/// the 64-bit float the rescale takes.
fn reaction_speed_map(s: &DyadicSettings, out_min: i64, out_max: i64) -> Result<i64, MotionError> {
    let x = match &s.native_reaction_speed {
        Some(raw) => native::double(&int_setting(&s.runtime.value("MOTIONBOX", "reaction_speed", raw))?)?,
        None => s.reaction_speed as f64,
    };
    Ok(py_int(map_clamped(x, 70.0, 100.0, out_min as f64, out_max as f64)))
}

/// A raw setting as an integer, at any width.
fn int_setting(setting: &native::NativeSetting) -> Result<LegacyValue, MotionError> {
    native::to_int(&setting.clone().map_err(MotionError::Native)?)
}

/// Multiplies the traction-loss slot by its direction (current variant), unconditionally: the direction is
/// `-1` when the model allows
/// a change of direction and `invert_tl` is on, else `1`. The product is written into the caller's list.
fn apply_tl_direction(
    motors: &mut LegacyValue,
    s: &DyadicSettings,
    model: &str,
    statement: PyFrame,
) -> Result<(), MotionError> {
    // `PROSIMU.invert_tl` as a boolean, read only when the model allows a change of direction.
    let invert = || s.runtime.switch("PROSIMU", "invert_tl", &Ok(s.invert_tl)).map_err(MotionError::Native);
    let tl_dir =
        if ScnModel::of(model)?.allow_change_tl_dir && invert().map_err(|e| e.in_frame(statement))? { -1 } else { 1 };
    let value = native::item(motors, TL_SLOT).map_err(|e| e.in_frame(statement))?;
    let value = native::mul(&value, &native::int(tl_dir)).map_err(|e| e.in_frame(statement))?;
    native::assign(motors, TL_SLOT, value).map_err(|e| e.in_frame(statement))
}

impl MotionDriver for DyadicDriver {
    /// Find one SCN6 per controller slot, home them, then report the ports.
    fn connect(&mut self, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        ScnModel::of(ctx.model.name)?;
        let module = match self.variant {
            DyadicVariant::Current => ScanModule::Dyadic,
            DyadicVariant::Previous => ScanModule::DyadicPrevious,
        };
        // The caller connects only while the motion switch is on (1) and no motion error is set
        // (False): the previous variant, which formats its first line from
        // those two values, logs `1 False`.
        match self.variant {
            DyadicVariant::Current => debug!("trying to connect"),
            DyadicVariant::Previous => debug!("trying to connect 1 False"),
        }
        debug!("pass checks: switch is on, no motion errors");
        debug!("nbr_controllers that I need to find {}", ctx.model.controllers);
        debug!("disconnect from all controllers");
        self.actuators.clear();
        // One empty record per controller.
        debug!("create actuator_dict");
        self.actuators = (0..ctx.model.controllers).map(|_| ActuatorRecord::default()).collect();
        scan_records(ctx.host, &mut self.actuators, module)?;
        // All ports found: no record is left with an empty `com_port`.
        let all_connected = self.actuators.iter().all(|record| !record.com_port.is_empty());
        debug!("all_ports connected {}", if all_connected { "True" } else { "False" });
        if !all_connected {
            debug!("setting motion status as NOT Connected");
            ctx.link.status = LinkStatus::NotConnected;
            ctx.link.ports.clear();
            return Ok(());
        }
        let clock = ctx.host.clock();
        for actuator in &mut self.actuators {
            // The homing pattern as text of at least two digits.
            let pattern =
                actuator.homing_pattern.ok_or_else(|| MotionError::Native("KeyError: 'homing_pattern'".into()))?;
            let mode = zfill2(pattern);
            debug!("seting move org to {mode}");
            move_org(actuator, &mode, clock, module)?;
        }
        for (key, actuator) in self.actuators.iter_mut().enumerate() {
            wait_for_homing(actuator, key, clock, module);
        }
        debug!("setting motion status as Connected");
        if self.variant == DyadicVariant::Current {
            ctx.link.status = LinkStatus::Connected;
        }
        self.homing_completed = Some(clock.now());
        if self.variant == DyadicVariant::Previous {
            let s = self.settings.get();
            for index in 0..self.actuators.len() {
                let serial = self.actuators[index].sn.clone();
                // The initial velocity is logged with the actuator's speed and acceleration.
                let speed = self.speed_for(&s, &serial, ctx.host.clock())?;
                let accel = self.accel_for(&s, &serial, ctx.host.clock())?;
                debug!("setting the initial velocity of {index} to {speed} {accel}");
                // The velocity write looks both up again.
                let speed = self.speed_for(&s, &serial, ctx.host.clock())?;
                let accel = self.accel_for(&s, &serial, ctx.host.clock())?;
                let statement = module.connect_frame(INITIAL_VELOCITY_LINE);
                self.write_velocity(index, speed, accel, ctx.host.clock()).map_err(|e| e.through(statement))?;
            }
            ctx.link.status = LinkStatus::Connected;
        }
        debug!("Setting the actuator com ports");
        for (index, actuator) in self.actuators.iter().enumerate() {
            ctx.link.ports.insert(index, actuator.com_port.clone());
        }
        Ok(())
    }

    /// The actuator records are emptied: the interfaces are dropped (closing their ports).
    fn disconnect(&mut self, _ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.actuators.clear();
        Ok(())
    }

    fn send_stream(&mut self, motors: &[i64], ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.send_compat(motors, ctx, 0)
    }

    fn send_test(&mut self, motors: &[i64], ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.send_compat(motors, ctx, 1)
    }

    /// A stream frame: the position pass on the caller's list.
    fn send_stream_native(&mut self, motors: &mut LegacyValue, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        let call = self.at("send_stream", self.lines().send_stream);
        self.send_actuator_pos(motors, ctx).map_err(|e| e.through(call))
    }

    fn send_test_native(&mut self, motors: &mut LegacyValue, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        let call = self.at("send_test", self.lines().send_test);
        self.send_actuator_pos(motors, ctx).map_err(|e| e.through(call))
    }

    /// The actuator records and the axis assignment status as the Hardware page reads them.
    fn assignment(&self) -> Option<super::AxisAssignment> {
        Some(super::AxisAssignment {
            actuators: self
                .actuators
                .iter()
                .enumerate()
                .map(|(key, a)| super::AssignableActuator {
                    key,
                    axis: a.axis.clone(),
                    com_port: a.com_port.clone(),
                    sn: a.sn.clone(),
                })
                .collect(),
            status: self.assignment_ok,
            tl_port: String::new(),
        })
    }

    fn take_edited_frame(&mut self) -> Option<Vec<i64>> {
        self.edited.take()
    }

    fn describe(&self) -> String {
        let ports: Vec<&str> = self.actuators.iter().map(|a| a.com_port.as_str()).collect();
        format!("Dyadic SCN6 ({:?}) on {}", self.variant, ports.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::axis::testing::{scn6_device, with_velocity};
    use super::tmbs::{pack, Command};
    use super::*;
    use crate::motion::driver_support::testing::ftdi_port;
    use crate::motion::testing::FakeHost;
    use crate::motion::{motion_model, LinkState};

    const A: &str = "/dev/ttyUSB3";
    const B: &str = "/dev/ttyUSB4";

    struct Rig {
        driver: DyadicDriver,
        host: FakeHost,
        link: LinkState,
        settings: LiveSettings<DyadicSettings>,
        model: &'static str,
    }

    impl Rig {
        fn new(variant: DyadicVariant, model: &'static str, pattern: u32) -> Self {
            let mut host = FakeHost::default();
            for (path, sn) in [(A, 0x11u32), (B, 0x22)] {
                host.ports.push(ftdi_port(path));
                let device = with_velocity(with_velocity(scn6_device(sn, pattern), 500, 100), 5000, 80);
                host = host.device(path, device);
            }
            let mut s = DyadicSettings::default();
            s.axis_assignment.insert("0".into(), "0,00000022".into());
            s.axis_assignment.insert("2".into(), "0,00000011".into());
            let settings = LiveSettings::new(s);
            Rig {
                driver: DyadicDriver::new(variant, settings.clone()),
                host,
                link: LinkState::default(),
                settings,
                model,
            }
        }
        fn call<R>(&mut self, f: impl FnOnce(&mut DyadicDriver, &mut MotionContext<'_>) -> R) -> R {
            let spec = motion_model(self.model, false).unwrap();
            let mut ctx = MotionContext { model: spec, link: &mut self.link, host: &mut self.host };
            f(&mut self.driver, &mut ctx)
        }
        fn connect(&mut self) {
            self.call(|d, ctx| d.connect(ctx)).unwrap();
        }
        fn stream(&mut self, motors: &[i64]) -> Result<(), MotionError> {
            self.call(|d, ctx| d.send_stream(motors, ctx))
        }
        fn writes(&mut self, path: &str) -> Vec<Vec<u8>> {
            self.host.log(path).written()
        }
    }

    #[test]
    fn connect_homes_every_actuator_and_reports_ports() {
        // Connect: all ports found → homing with the two-digit homing pattern, poll, Connected,
        // the motion port of each index set.
        let mut rig = Rig::new(DyadicVariant::Current, "T1000-2M SCN6 Dyadic", 7);
        rig.connect();
        assert!(rig.link.is_connected());
        // Records outer, ports inner, last match wins: actuator 0 ends on B, actuator 1 re-opens A.
        assert_eq!(rig.link.ports.get(&0).map(String::as_str), Some(B));
        assert_eq!(rig.link.ports.get(&1).map(String::as_str), Some(A));
        assert!(rig.writes(A).contains(&pack(0, "o0700000000")));
        assert!(rig.writes(B).contains(&pack(0, "o0700000000")));
        assert_eq!(rig.driver.serial_numbers(), ["00000022", "00000011"]);
        assert_eq!(rig.host.log(A).opens.len(), 2);
        assert_eq!(rig.host.log(B).opens.len(), 1);
    }

    #[test]
    fn connect_without_enough_controllers_is_not_connected() {
        let mut rig = Rig::new(DyadicVariant::Current, "T1000-3M SCN6 Dyadic", 7);
        rig.connect();
        assert!(!rig.link.is_connected());
        assert!(rig.link.ports.is_empty());
    }

    fn dyadic_connect_stages(variant: DyadicVariant) {
        let mut rig = Rig::new(variant, "T1000-2M SCN6 Dyadic", 7);
        let messages = crate::motion::driver_support::testing::capture_debug_messages(|| rig.connect());
        let stages = messages
            .iter()
            .filter(|message| {
                message.starts_with("all_ports connected ")
                    || message.starts_with("setting motion status as ")
                    || message.as_str() == "Setting the actuator com ports"
            })
            .map(String::as_str)
            .collect::<Vec<_>>();
        assert_eq!(
            stages,
            ["all_ports connected True", "setting motion status as Connected", "Setting the actuator com ports"]
        );
        if variant == DyadicVariant::Previous {
            let connected =
                messages.iter().position(|message| message == "setting motion status as Connected").unwrap();
            let ports = messages.iter().position(|message| message == "Setting the actuator com ports").unwrap();
            let velocities = messages
                .iter()
                .enumerate()
                .filter(|(_, message)| message.starts_with("setting actuator new velocity "))
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            assert_eq!(velocities.len(), 2);
            for index in velocities {
                assert!(connected < index && index < ports);
            }
        }
        assert!(rig.link.is_connected());
        assert_eq!(rig.link.ports.len(), 2);
    }

    #[test]
    fn dyadic_connect_stages_current_reports_connection_and_ports() {
        dyadic_connect_stages(DyadicVariant::Current);
    }

    #[test]
    fn dyadic_connect_stages_previous_reports_connection_before_initial_velocity() {
        dyadic_connect_stages(DyadicVariant::Previous);
    }

    #[test]
    fn first_frame_needs_the_simtech_keys() {
        // The position pass reads `SIMTECHREVOLUTION.scn6_speed` (a key error when absent).
        let mut rig = Rig::new(DyadicVariant::Current, "T1000-2M SCN6 Dyadic", 7);
        rig.connect();
        assert!(rig.stream(&[1000, 2000, 3000]).is_err());
    }

    #[test]
    fn stream_writes_post_homing_velocity_then_positions() {
        // The speed within post_homing_speed_timeout → post_homing_speed (500); the acceleration within 5 s → 100.
        // homing pattern 7 → the position is the negated slot.
        let mut rig = Rig::new(DyadicVariant::Current, "T1000-2M SCN6 Dyadic", 7);
        rig.settings.update(|s| {
            s.simtech_scn6_speed = Ok(LegacyValue::Int(5000));
            s.simtech_scn6_acceleration = Ok(LegacyValue::Int(80));
        });
        rig.connect();
        rig.stream(&[1000, 2000, 3000]).unwrap();
        assert!(rig.driver.assignment_ok());
        let b = rig.writes(B);
        assert!(b.contains(&pack(0, &Command::Velocity { speed: 500, accel: 100 }.text())));
        // B has serial 22 → key '0' → slot 0 = 1000 → -1000.
        assert_eq!(b.last().unwrap(), &pack(0, &Command::MoveAbsolute(-1000).text()));
        // A has serial 11 → key '2' → slot 2 = 3000 → -3000.
        assert_eq!(rig.writes(A).last().unwrap(), &pack(0, &Command::MoveAbsolute(-3000).text()));
    }

    #[test]
    fn velocity_switches_after_the_post_homing_window() {
        // After the window: TL key '2' → 5000/80; other keys → the reaction speed rescaled from 70..100 to 5000..10000.
        let mut rig = Rig::new(DyadicVariant::Current, "T1000-2M SCN6 Dyadic", 3);
        rig.settings.update(|s| {
            s.simtech_scn6_speed = Ok(LegacyValue::Int(5000));
            s.simtech_scn6_acceleration = Ok(LegacyValue::Int(80));
            s.reaction_speed = 70;
        });
        rig.connect();
        rig.stream(&[1000, 2000, 3000]).unwrap();
        rig.host.clock.sleep(Duration::from_secs(6));
        rig.stream(&[1000, 2000, 3000]).unwrap();
        assert!(rig.writes(A).contains(&pack(0, &Command::Velocity { speed: 5000, accel: 80 }.text())));
        // pattern 3 → position = the maximum (10000) - value
        assert_eq!(rig.writes(A).last().unwrap(), &pack(0, &Command::MoveAbsolute(7000).text()));
        assert!(rig.writes(B).contains(&pack(0, &Command::Velocity { speed: 5000, accel: 80 }.text())));
    }

    #[test]
    fn missing_assignment_stops_the_frame() {
        // No assignment id (-1) → the assignment status is false and the pass returns false.
        let mut rig = Rig::new(DyadicVariant::Current, "T1000-2M SCN6 Dyadic", 7);
        rig.settings.update(|s| {
            s.simtech_scn6_speed = Ok(LegacyValue::Int(5000));
            s.simtech_scn6_acceleration = Ok(LegacyValue::Int(80));
            s.axis_assignment.clear();
        });
        rig.connect();
        rig.stream(&[1000, 2000, 3000]).unwrap();
        assert!(!rig.driver.assignment_ok());
    }

    #[test]
    fn invert_tl_flips_the_sign_on_three_motor_models() {
        // Slot 2 is multiplied by the direction, -1 when the model allows the change and invert_tl is on.
        let s = DyadicSettings { invert_tl: true, ..DyadicSettings::default() };
        let frame = PyFrame::new("backend/controllers/dyadic.py", "send_actuator_pos", 326);
        let run = |values: &[i64], model: &str| {
            let mut list = native::list_of(values);
            apply_tl_direction(&mut list, &s, model, frame).map(|()| list)
        };
        assert_eq!(run(&[1, 2, 3], "T1000-3M SCN6 Dyadic").unwrap(), native::list_of(&[1, 2, -3]));
        assert_eq!(run(&[1, 2, 3], "T1000-2M SCN6 Dyadic").unwrap(), native::list_of(&[1, 2, 3]));
        let error = run(&[1, 2], "T1000-2M SCN6 Dyadic").unwrap_err();
        assert_eq!(error.frames(), &[frame]);
        // The product is a new integer of any width, written back into the list; a boolean becomes an integer.
        let mut list = LegacyValue::List(vec![native::int(0), native::int(0), LegacyValue::Bool(true)]);
        apply_tl_direction(&mut list, &DyadicSettings::default(), "T1000-2M SCN6 Dyadic", frame).unwrap();
        assert_eq!(native::item(&list, 2).unwrap(), native::int(1));
    }

    #[test]
    fn previous_variant_writes_velocity_eleven_times_at_connect() {
        // Previous variant: the connect writes each actuator's velocity after homing; each
        // velocity write is sent eleven times.
        let mut rig = Rig::new(DyadicVariant::Previous, "T1000-2M SCN6 Dyadic", 7);
        rig.connect();
        assert!(rig.link.is_connected());
        let velocity = pack(0, &Command::Velocity { speed: 500, accel: 100 }.text());
        assert_eq!(rig.writes(A).iter().filter(|w| **w == velocity).count(), 11);
        // The previous variant does not read the SIMTECHREVOLUTION keys.
        rig.stream(&[1000, 2000, 3000]).unwrap();
        assert!(rig.driver.assignment_ok());
    }

    #[test]
    fn native_dyadic_inputs_delay_a_speed_error_until_the_first_frame() {
        let mut rig = Rig::new(DyadicVariant::Current, "T1000-2M SCN6 Dyadic", 7);
        rig.settings.update(|settings| {
            settings.simtech_scn6_speed = Err("ValueError: fixture speed conversion".into());
            settings.simtech_scn6_acceleration = Ok(LegacyValue::Int(80));
        });
        rig.connect();
        assert!(rig.link.is_connected());
        assert_eq!(rig.stream(&[1000, 2000, 3000]).unwrap_err().to_string(), "ValueError: fixture speed conversion");
        assert_eq!(rig.driver.actuators[0].speed, -1);
        assert_eq!(rig.driver.actuators[0].accel, -1);
    }

    #[test]
    fn native_driver_errors_keep_key_error_text_at_the_actual_frame_read() {
        let mut rig = Rig::new(DyadicVariant::Current, "T1000-2M SCN6 Dyadic", 7);
        rig.settings.update(|settings| settings.simtech_scn6_speed = Err("KeyError: 'scn6_speed'".into()));
        rig.connect();
        let error = rig.stream(&[1000, 2000, 3000]).unwrap_err();
        assert_eq!(error.to_string(), "KeyError: 'scn6_speed'");
        assert_eq!(error.python_str(), "'scn6_speed'");
        assert_eq!(rig.driver.actuators[0].speed, -1);
    }

    #[test]
    fn native_dyadic_inputs_acceleration_error_retains_the_speed_prefix() {
        let mut rig = Rig::new(DyadicVariant::Current, "T1000-2M SCN6 Dyadic", 7);
        rig.settings.update(|settings| {
            settings.simtech_scn6_speed = Ok(LegacyValue::Int(5000));
            settings.simtech_scn6_acceleration = Err("TypeError: fixture acceleration conversion".into());
        });
        rig.connect();
        assert_eq!(
            rig.stream(&[1000, 2000, 3000]).unwrap_err().to_string(),
            "TypeError: fixture acceleration conversion"
        );
        assert_eq!(rig.driver.actuators[0].speed, 500);
        assert_eq!(rig.driver.actuators[0].accel, -1);
    }

    #[test]
    fn dyadic_velocity_log_current_variant_keeps_the_source_silent() {
        let mut rig = Rig::new(DyadicVariant::Current, "T1000-2M SCN6 Dyadic", 7);
        rig.connect();
        let messages = crate::motion::driver_support::testing::capture_debug_messages(|| {
            rig.call(|driver, context| driver.write_velocity(0, 500, 100, context.host.clock())).unwrap();
        });
        assert!(messages.is_empty(), "{messages:?}");
    }

    #[test]
    fn dyadic_velocity_log_previous_variant_keeps_native_text_and_repeat_count() {
        let mut rig = Rig::new(DyadicVariant::Previous, "T1000-2M SCN6 Dyadic", 7);
        rig.connect();
        let messages = crate::motion::driver_support::testing::capture_debug_messages(|| {
            rig.call(|driver, context| driver.write_velocity(0, 500, 100, context.host.clock())).unwrap();
        });
        assert_eq!(messages.first().map(String::as_str), Some("setting actuator new velocity /dev/ttyUSB4 500 100"));
        assert_eq!(messages.len(), 12);
        assert!(
            messages[1..].iter().all(|message| message == "status of the velocity sent the actuator True"),
            "{messages:?}"
        );
    }

    #[test]
    fn frame_path_exceptions_carry_their_compiled_frames() {
        // The frames of a stream frame's errors: the stream frame, then the position pass. A
        // velocity reply that is not hexadecimal fails in the protocol's reply parsing, through the driver's
        // velocity write; a missing scn6_speed fails in the position pass itself; an assignment key that is
        // not an integer fails in the id lookup. The previous variant has its own lines.
        let frames = |error: &MotionError| -> Vec<String> {
            error.frames().iter().map(|frame| format!("{}:{}:{}", frame.file, frame.function, frame.line)).collect()
        };
        let bad_velocity = |variant| {
            let mut rig = Rig::new(variant, "T1000-2M SCN6 Dyadic", 7);
            rig.settings.update(|s| {
                s.simtech_scn6_speed = Ok(LegacyValue::Int(5000));
                s.simtech_scn6_acceleration = Ok(LegacyValue::Int(80));
            });
            rig.connect();
            let velocity = pack(0, &Command::Velocity { speed: 5000, accel: 80 }.text());
            for path in [A, B] {
                let device = rig.host.devices.get_mut(path).unwrap();
                for (request, answer) in &mut device.replies {
                    if *request == velocity {
                        *answer = super::tmbs::testing::reply('0', "V1ZZ000000");
                    }
                }
            }
            // The fixture's links were opened at connect: reconnect so they answer with the changed reply.
            rig.connect();
            rig.host.clock.sleep(Duration::from_secs(6));
            rig.stream(&[1000, 2000, 3000]).unwrap_err()
        };
        let error = bad_velocity(DyadicVariant::Current);
        assert_eq!(error.to_string(), "ValueError: invalid literal for int() with base 16: b'ZZ'");
        assert_eq!(
            frames(&error),
            [
                "backend/controllers/dyadic.py:send_stream:404",
                // The records were reset by the reconnect: this is the first write of the frame.
                "backend/controllers/dyadic.py:send_actuator_pos:359",
                "backend/controllers/dyadic.py:write_velocity:20",
                "backend/controllers/pytbmscom.py:TMBSCOMProtocol.write_velocity:673",
                "backend/controllers/pytbmscom.py:TMBSCOMProtocol.parseADirectResponseFormat:217",
            ]
        );
        let error = bad_velocity(DyadicVariant::Previous);
        assert_eq!(
            frames(&error),
            [
                "backend/controllers/dyadic_previous.py:send_stream:354",
                "backend/controllers/dyadic_previous.py:send_actuator_pos:336",
                "backend/controllers/dyadic_previous.py:write_velocity:27",
                "backend/controllers/pytbmscom.py:TMBSCOMProtocol.write_velocity:673",
                "backend/controllers/pytbmscom.py:TMBSCOMProtocol.parseADirectResponseFormat:217",
            ]
        );

        let mut rig = Rig::new(DyadicVariant::Current, "T1000-2M SCN6 Dyadic", 7);
        rig.connect();
        let error = rig.call(|d, ctx| d.send_test(&[1000, 2000, 3000], ctx)).unwrap_err();
        assert_eq!(error.to_string(), "KeyError: 'scn6_speed'");
        assert_eq!(
            frames(&error),
            ["backend/controllers/dyadic.py:send_test:407", "backend/controllers/dyadic.py:send_actuator_pos:334"]
        );
        rig.settings.update(|s| {
            s.simtech_scn6_speed = Ok(LegacyValue::Int(5000));
            s.simtech_scn6_acceleration = Ok(LegacyValue::Int(80));
            s.axis_assignment.clear();
            s.axis_assignment.insert("first".into(), "0,00000022".into());
        });
        let error = rig.stream(&[1000, 2000, 3000]).unwrap_err();
        assert_eq!(error.to_string(), "ValueError: invalid literal for int() with base 10: 'first'");
        assert_eq!(
            frames(&error),
            [
                "backend/controllers/dyadic.py:send_stream:404",
                "backend/controllers/dyadic.py:send_actuator_pos:368",
                "backend/controllers/dyadic.py:get_id:237",
            ]
        );
    }

    #[test]
    fn assignment_id_past_the_list_raises_in_the_position_statement() {
        // A hand-edited assignment id of 12 or more: the slot read is an index error at the frame line of the
        // homing-pattern branch (negated, or subtracted from the maximum; each variant has its own lines),
        // on its way out through the stream frame.
        for (variant, pattern, expected) in [
            (DyadicVariant::Current, 7, ["dyadic.py:send_stream:404", "dyadic.py:send_actuator_pos:381"]),
            (DyadicVariant::Current, 1, ["dyadic.py:send_stream:404", "dyadic.py:send_actuator_pos:383"]),
            (
                DyadicVariant::Previous,
                7,
                ["dyadic_previous.py:send_stream:354", "dyadic_previous.py:send_actuator_pos:344"],
            ),
            (
                DyadicVariant::Previous,
                1,
                ["dyadic_previous.py:send_stream:354", "dyadic_previous.py:send_actuator_pos:346"],
            ),
        ] {
            let mut rig = Rig::new(variant, "T1000-2M SCN6 Dyadic", pattern);
            rig.settings.update(|s| {
                s.simtech_scn6_speed = Ok(LegacyValue::Int(5000));
                s.simtech_scn6_acceleration = Ok(LegacyValue::Int(80));
            });
            rig.connect();
            rig.settings.update(|s| {
                s.axis_assignment.clear();
                s.axis_assignment.insert("12".into(), "0,00000022".into());
                s.axis_assignment.insert("2".into(), "0,00000011".into());
            });
            let error = rig.stream(&[0; 12]).unwrap_err();
            let frames: Vec<String> = error
                .frames()
                .iter()
                .map(|frame| {
                    format!(
                        "{}:{}:{}",
                        frame.file.trim_start_matches("backend/controllers/"),
                        frame.function,
                        frame.line
                    )
                })
                .collect();
            assert_eq!(frames, expected, "{variant:?} pattern {pattern}");
            assert_eq!(error.exception_line().as_deref(), Some("IndexError: list index out of range"));
        }
    }

    #[test]
    fn previous_variant_log_lines_carry_the_source_arguments() {
        // Previous variant: the connect logs 'trying to connect' with the motion switch and the motion error
        // (1 and False whenever a connect runs). It logs the initial velocity of each actuator with its speed
        // and acceleration, and looks both up again for the velocity write; inside the
        // post-homing window each lookup logs its own line. The parts of a line are joined
        // with one space, so a part that ends with a space gives two.
        use crate::motion::driver_support::testing::capture_debug_messages;

        let mut rig = Rig::new(DyadicVariant::Previous, "T1000-2M SCN6 Dyadic", 7);
        let messages = capture_debug_messages(|| rig.connect());
        assert_eq!(messages[0], "trying to connect 1 False");
        let from = messages.iter().position(|message| message == "setting motion status as Connected").unwrap();
        let until = messages.iter().position(|message| message == "Setting the actuator com ports").unwrap();
        let window = ["returning post_homing speed to  500", "returning post_homing accel to  100"];
        let expected: Vec<String> = [(0, B), (1, A)]
            .into_iter()
            .flat_map(|(index, port)| {
                let mut lines: Vec<String> = window.iter().map(|line| line.to_string()).collect();
                lines.push(format!("setting the initial velocity of {index} to 500 100"));
                lines.extend(window.iter().map(|line| line.to_string()));
                lines.push(format!("setting actuator new velocity {port} 500 100"));
                lines.extend(std::iter::repeat_n("status of the velocity sent the actuator True".to_string(), 11));
                lines
            })
            .collect();
        assert_eq!(messages[from + 1..until], expected);

        // The position pass after the window: the lookups log nothing, and the changed
        // pair is announced as 'setting the velocity' with the key, 'to', the speed and the acceleration.
        rig.host.clock.sleep(Duration::from_secs(6));
        let messages = capture_debug_messages(|| {
            let _ = rig.stream(&[1000, 2000, 3000]);
        });
        let announced: Vec<&str> =
            messages.iter().map(String::as_str).filter(|message| message.starts_with("setting the velocity")).collect();
        assert_eq!(announced, ["setting the velocity  0 to 5000 80", "setting the velocity  1 to 5000 80"]);
        assert!(!messages.iter().any(|message| message.starts_with("returning post_homing") || message == "sending"));
    }

    #[test]
    fn current_variant_lookups_and_moves_log_nothing() {
        // The current variant's lookups and position pass log nothing:
        // the post-homing return and the position commands are silent.
        use crate::motion::driver_support::testing::capture_debug_messages;

        let mut rig = Rig::new(DyadicVariant::Current, "T1000-2M SCN6 Dyadic", 7);
        rig.settings.update(|s| {
            s.simtech_scn6_speed = Ok(LegacyValue::Int(5000));
            s.simtech_scn6_acceleration = Ok(LegacyValue::Int(80));
        });
        let connect = capture_debug_messages(|| rig.connect());
        assert_eq!(connect[0], "trying to connect");
        let messages = capture_debug_messages(|| rig.stream(&[1000, 2000, 3000]).unwrap());
        assert!(messages.is_empty(), "{messages:?}");
        assert!(rig.writes(B).contains(&pack(0, &Command::MoveAbsolute(-1000).text())));
    }

    #[test]
    fn an_acceleration_resolved_after_the_speed_writes_the_stored_speed_first() {
        // Current variant's position pass: the block for a speed of -1 and the block for an acceleration
        // of -1 are siblings. With a 15 s speed window and the fixed 5 s acceleration window, a frame at 6 s on an
        // unassigned rig stores the post-homing speed and stops at the acceleration (-1). Once assigned, a
        // frame after the speed window runs the acceleration block alone: a velocity write with the stored
        // speed and the acceleration, then the comparison writes the new speed.
        let mut rig = Rig::new(DyadicVariant::Current, "T1000-2M SCN6 Dyadic", 7);
        let assignment = rig.settings.get().axis_assignment;
        rig.settings.update(|s| {
            s.simtech_scn6_speed = Ok(LegacyValue::Int(5000));
            s.simtech_scn6_acceleration = Ok(LegacyValue::Int(80));
            s.post_homing_speed_timeout = 15;
            s.reaction_speed = 70;
            s.axis_assignment.clear();
        });
        rig.connect();
        rig.host.clock.sleep(Duration::from_secs(6));
        rig.stream(&[100, 200, 300]).unwrap();
        assert!(!rig.driver.assignment_ok());
        let velocity = |rig: &mut Rig, path: &str| -> Vec<Vec<u8>> {
            rig.writes(path).into_iter().filter(|frame| frame.get(2) == Some(&b'v')).collect()
        };
        assert!(velocity(&mut rig, B).is_empty(), "nothing is written while the acceleration is unresolved");
        rig.settings.update(|s| s.axis_assignment = assignment);
        rig.host.clock.sleep(Duration::from_secs(10));
        // The fixture answers the velocity commands it knows; the reply is not needed here.
        let _ = rig.stream(&[100, 200, 300]);
        // Actuator 0 is serial 22 on B, assigned to option '0': a reaction speed of 70 gives 5000 and
        // 80 after both windows.
        assert_eq!(
            velocity(&mut rig, B),
            [
                pack(0, &Command::Velocity { speed: 500, accel: 80 }.text()),
                pack(0, &Command::Velocity { speed: 5000, accel: 80 }.text()),
            ]
        );
    }

    #[test]
    fn a_partly_assigned_rig_sends_no_velocity_to_the_assigned_actuator() {
        // Current variant's position pass: actuator 0 (serial 22) is assigned by entry '1', but the dialog's
        // 'None' of option '0' comes first in the axis assignment: the speed lookup gives -1 there, the status is
        // False and nothing is written. The entry order decides, not the match.
        let mut rig = Rig::new(DyadicVariant::Current, "T1000-2M SCN6 Dyadic", 7);
        rig.connect();
        rig.settings.update(|s| {
            s.simtech_scn6_speed = Ok(LegacyValue::Int(5000));
            s.simtech_scn6_acceleration = Ok(LegacyValue::Int(80));
            s.axis_assignment.clear();
            s.axis_assignment.insert("0".into(), "None".into());
            s.axis_assignment.insert("1".into(), "0,00000022".into());
        });
        rig.host.clock.sleep(Duration::from_secs(60));
        let before = (rig.writes(A).len(), rig.writes(B).len());
        rig.stream(&[100, 200, 300]).unwrap();
        assert!(!rig.driver.assignment_ok());
        assert_eq!((rig.writes(A).len(), rig.writes(B).len()), before, "no velocity or position command");
    }

    #[test]
    fn dyadic_velocity_log_previous_variant_keeps_false_responses() {
        let mut rig = Rig::new(DyadicVariant::Previous, "T1000-2M SCN6 Dyadic", 7);
        rig.host
            .devices
            .get_mut(B)
            .unwrap()
            .fail_on
            .push(pack(AXIS, &Command::Velocity { speed: 500, accel: 100 }.text()));
        rig.connect();
        let messages = crate::motion::driver_support::testing::capture_debug_messages(|| {
            rig.call(|driver, context| driver.write_velocity(0, 500, 100, context.host.clock())).unwrap();
        });
        assert_eq!(messages.len(), 12);
        assert!(
            messages[1..].iter().all(|message| message == "status of the velocity sent the actuator False"),
            "{messages:?}"
        );
    }

    fn native_list(values: Vec<LegacyValue>) -> LegacyValue {
        LegacyValue::Object(openrigcue_model::native_object::NativeObject::new(LegacyValue::List(values)))
    }

    /// On the caller's dynamic list: the traction-loss slot times its direction is written into the shared
    /// object first; a position outside a signed 32-bit integer then fails in the absolute move after the
    /// positions before it were sent. Homing pattern 7 sends the negated slot at any width.
    #[test]
    fn native_tl_product_is_published_and_wide_positions_raise_in_struct_pack() {
        // The T1000-2M does not allow a change of direction: the direction is 1, and the product (a new
        // integer: a true boolean times 1 is 1) is still assigned into the list.
        let mut rig = Rig::new(DyadicVariant::Current, "T1000-2M SCN6 Dyadic", 7);
        rig.settings.update(|s| {
            s.simtech_scn6_speed = Ok(LegacyValue::Int(5000));
            s.simtech_scn6_acceleration = Ok(LegacyValue::Int(80));
            s.invert_tl = true;
        });
        rig.connect();
        let mut list = native_list(vec![native::int(1000), native::int(2000), LegacyValue::Bool(true)]);
        let alias = list.clone();
        rig.call(|d, ctx| d.send_stream_native(&mut list, ctx)).unwrap();
        assert_eq!(native::item(&alias, 2).unwrap(), native::int(1), "the TL product is in the shared list");
        assert_eq!(rig.writes(A).last().unwrap(), &pack(0, &Command::MoveAbsolute(-1).text()));
        let b_before = rig.writes(B).len();
        let wide = LegacyValue::from_integer(num_bigint::BigInt::from(1) << 40);
        let mut list = native_list(vec![native::int(1000), native::int(2000), wide.clone()]);
        let error = rig.call(|d, ctx| d.send_stream_native(&mut list, ctx)).unwrap_err();
        assert_eq!(
            error.exception_line().unwrap(),
            "struct.error: 'l' format requires -2147483648 <= number <= 2147483647"
        );
        let frames: Vec<_> = error.frames().iter().map(|f| (f.function, f.line)).collect();
        assert_eq!(
            frames,
            [("send_stream", 404), ("send_actuator_pos", 385), ("TMBSCOMProtocol.move_abs_integer", 625)]
        );
        // Actuator B (key '0') moved before A's wide TL position failed.
        assert_eq!(rig.writes(B).len(), b_before + 1);
        assert_eq!(rig.writes(B).last().unwrap(), &pack(0, &Command::MoveAbsolute(-1000).text()));
        assert_eq!(native::item(&list, 2).unwrap(), wide);
    }

    /// The id lookup gives the key as an integer of any width; a wide id is an index error at the slot read.
    #[test]
    fn native_wide_assignment_key_raises_index_error_at_the_subscript() {
        let mut rig = Rig::new(DyadicVariant::Current, "T1000-2M SCN6 Dyadic", 7);
        rig.settings.update(|s| {
            s.simtech_scn6_speed = Ok(LegacyValue::Int(5000));
            s.simtech_scn6_acceleration = Ok(LegacyValue::Int(80));
            s.axis_assignment.remove("0");
            s.axis_assignment.insert("99999999999999999999999".into(), "0,00000022".into());
        });
        rig.connect();
        let mut list = native_list(vec![native::int(1000), native::int(2000), native::int(3000)]);
        let error = rig.call(|d, ctx| d.send_stream_native(&mut list, ctx)).unwrap_err();
        // An index beyond the signed pointer width cannot index a list at all.
        assert_eq!(error.exception_line().unwrap(), "IndexError: cannot fit 'int' into an index-sized integer");
        let frames: Vec<_> = error.frames().iter().map(|f| (f.function, f.line)).collect();
        assert_eq!(frames, [("send_stream", 404), ("send_actuator_pos", 381)]);
    }
}
