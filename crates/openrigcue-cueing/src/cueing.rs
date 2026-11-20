//! The stream path: one motion frame from telemetry, with the state kept between frames.

use std::time::Instant;

use num_traits::ToPrimitive;
use openrigcue_model::{native_value::LegacyValue, Telemetry};

use crate::axis::{Axis, PerAxis};
use crate::effects::{apply_native_wheel_effects, EffectRng, EventInputs, SharedRandom};
use crate::error::{mh, CueError};
use crate::mixing::{
    cmp_frame, command_position, enhanced_type, normalize_native, slot_formulas_with_normalize, slot_power, slot_table,
    HeaveScale,
};
use crate::platform::{Platform, WheelSlots, SLOTS};
use crate::pymath::Num;
use crate::pyvalue::{Op, PyValue};
use crate::settings::ClipFactors;
use crate::shaping::{scale_travel, shape_actuator_native, shape_rotary_native};
use crate::smoothing::{moving_average, FormulaHistory, WindowContext, SMOOTHING_ORDER};
use crate::timers::{follow_longitudinal_native, GearSmoothing, SoftStart};
use crate::views::{config_bool, config_integer, game_item, ConfigView, GameConfigView, Sources};
use crate::worker_smoothing::{SmoothingInputs, SmoothingWorkers};

/// One position per output slot, in the platform's native units.
pub type Positions = [i64; SLOTS];
/// The motor list as computed: integers of any size, until each driver consumes a slot.
pub type NativePositions = [LegacyValue; SLOTS];
pub(crate) fn bounded_positions(values: NativePositions) -> Result<Positions, CueError> {
    let mut out = [0; SLOTS];
    for (i, value) in values.into_iter().enumerate() {
        out[i] = value
            .integer()
            .and_then(|v| v.to_i64())
            .ok_or_else(|| CueError::overflow("Python int too large to convert to C long"))?;
    }
    Ok(out)
}

/// Which telemetry listener runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Listener {
    /// `'none'`: no listener.
    None,
    /// `'emulator'`.
    Emulator,
    /// `'test'`.
    Test,
    /// A game listener.
    Game,
}

impl Listener {
    /// Listeners during which the stream path is idle and test mode may drive the rig.
    pub fn is_idle(self) -> bool {
        !matches!(self, Listener::Game)
    }
}

/// Category of the running game (its registry `type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GameKind {
    /// `'racing'`, also the fallback for a game missing from the registry.
    Racing,
    /// Any other category (flying, others, ...).
    Other,
}

/// Everything a stream frame reads besides telemetry.
#[derive(Clone, Copy)]
pub struct StreamContext<'a> {
    /// The configuration ([`crate::MotionSettings`] or [`crate::RawConfig`]).
    pub settings: &'a dyn ConfigView,
    /// The game configuration ([`crate::MotionTune`] or [`crate::RawGameConfig`]).
    pub tune: &'a dyn GameConfigView,
    pub listener: Listener,
    /// Id of the running game: a registry id, or `joy`, `test`, `none`, `emulator`.
    pub game: &'a str,
    /// Id of the last game that actually ran.
    pub previous_actual_game: &'a str,
    pub game_kind: GameKind,
}

/// The runtime state the stream path reads once the frame is computed, and the driver calls it makes.
pub trait StreamOutput {
    /// Whether the motion controller is connected.
    fn connected(&self) -> bool;
    /// Whether the motion switch is on.
    fn motion_switch_on(&self) -> bool;
    /// Publishes the computed frame and the vertical acceleration.
    fn publish(&mut self, positions: &Positions, heave: f64);
    /// Sends the frame to the model's driver, with the error the driver fails with (a missing
    /// driver is a missing-key error of the controller key). The driver edits the list in
    /// place: `positions` holds what it left.
    fn send_stream(&mut self, positions: &mut Positions) -> Result<(), CueError>;
    /// Whether debug output is enabled.
    fn debug_status(&self) -> bool;
    /// Passes the frame to the driver's debug hook.
    fn debug(&mut self, positions: &Positions) -> Result<(), CueError>;
}

/// State carried from one stream frame to the next.
#[derive(Debug, Clone)]
struct StreamState {
    /// Last delivered position per slot.
    delivered: [Option<LegacyValue>; SLOTS],
    history: FormulaHistory,
    previous_movement: Option<PyValue>,
    soft_start: SoftStart,
    gear_smoothing: GearSmoothing,
    previous_gear: PyValue,
    previous_longitudinal: PyValue,
    previous_listener: Option<Listener>,
}

impl Default for StreamState {
    fn default() -> Self {
        StreamState {
            delivered: std::array::from_fn(|_| None),
            history: FormulaHistory::default(),
            previous_movement: None,
            soft_start: SoftStart::Idle,
            gear_smoothing: GearSmoothing::default(),
            previous_gear: PyValue::Int(-100),
            previous_longitudinal: PyValue::Int(0),
            previous_listener: None,
        }
    }
}

/// The telemetry channels the motion path reads, with NaN replaced by 0.
#[derive(Debug, Clone)]
struct FrameInputs {
    axes: PerAxis<PyValue>,
    movement_axes: [PyValue; 3],
    gear: PyValue,
    speed: PyValue,
    events: EventInputs,
}

impl FrameInputs {
    fn read(t: &Telemetry) -> Self {
        let raw = |name: &str| {
            t.native_value(name)
                .map(PyValue::from)
                .unwrap_or_else(|| PyValue::Float(t.get(name).expect("registry channel")))
        };
        let axes = PerAxis::from_fn(|axis| {
            raw(match axis {
                Axis::Pitch => "pitch",
                Axis::Roll => "roll",
                Axis::Yaw => "yaw",
                Axis::Sway => "lateral_acceleration",
                Axis::Surge => "longitudinal_acceleration",
                Axis::Heave => "vertical_acceleration",
                Axis::TractionLoss => "lateral_velocity",
                Axis::TractionLossFront => "front_tl",
            })
        });
        FrameInputs {
            axes,
            movement_axes: [raw("yaw"), raw("roll"), raw("pitch")],
            gear: raw("gear"),
            speed: raw("speed"),
            events: EventInputs {
                native: Some(t.clone()),
                wheel_slip: WheelSlots {
                    lr: t.wheel_slip_lr,
                    lf: t.wheel_slip_lf,
                    rf: t.wheel_slip_rf,
                    rr: t.wheel_slip_rr,
                },
                rumble: WheelSlots {
                    lr: t.rumble_pitch_lr,
                    lf: t.rumble_pitch_lf,
                    rf: t.rumble_pitch_rf,
                    rr: t.rumble_pitch_rr,
                },
                canopy: t.canopy,
                afterburner: t.afterburner,
                landing_gear_left: t.landing_gear_left,
                landing_gear_right: t.landing_gear_right,
                cannon_firing: t.cannon_firing,
                missile_firing: t.missile_firing,
                abs_lock: t.abs_lock,
                rpm: t.native_value("rpm").map_or_else(|| PyValue::Float(t.rpm), PyValue::from),
                max_rpm: t.native_value("max_rpm").map_or_else(|| PyValue::Float(t.max_rpm), PyValue::from),
            },
        }
    }
}

/// Motion cueing: the stream and test paths.
///
/// The platform model is optional, as the `MOTIONBOX.model` setting may be `'None'`. The
/// frame-to-frame state lives as long as the cueing, across model changes and model-less periods.
pub struct MotionCueing {
    platform: Option<Platform>,
    clips: ClipFactors,
    rng: Box<dyn EffectRng>,
    state: StreamState,
    workers: Option<SmoothingWorkers>,
    motion_config: Option<std::sync::Arc<crate::MotionConfig>>,
}

impl MotionCueing {
    /// A cueing with fresh state drawing from the process-wide random source ([`crate::python_random`]).
    /// `clips` are fixed for its life; they are read once at start-up ([`ClipFactors::read`]).
    pub fn new(platform: Platform, clips: ClipFactors) -> Self {
        Self::with_rng(Some(platform), clips, Box::new(SharedRandom))
    }

    /// A cueing with no platform model selected (the `MOTIONBOX.model` setting is `'None'`).
    pub fn without_platform(clips: ClipFactors) -> Self {
        Self::with_rng(None, clips, Box::new(SharedRandom))
    }

    pub fn with_rng(platform: Option<Platform>, clips: ClipFactors, rng: Box<dyn EffectRng>) -> Self {
        MotionCueing { platform, clips, rng, state: StreamState::default(), workers: None, motion_config: None }
    }

    pub(crate) fn motion_config_handle(&self) -> std::sync::Arc<crate::MotionConfig> {
        static BUILTIN: std::sync::OnceLock<std::sync::Arc<crate::MotionConfig>> = std::sync::OnceLock::new();
        self.motion_config.clone().unwrap_or_else(|| {
            BUILTIN.get_or_init(|| std::sync::Arc::new(crate::MotionConfig::builtin().clone())).clone()
        })
    }

    pub fn set_motion_config(&mut self, config: std::sync::Arc<crate::MotionConfig>) {
        self.motion_config = Some(config);
    }

    /// Runs the production smoothing helpers independently of frame calculations. Without this,
    /// caller-supplied instants drive the deterministic fixture schedules.
    pub fn start_smoothing_workers(&mut self, inputs: std::sync::Arc<dyn SmoothingInputs>) -> std::io::Result<()> {
        self.workers = Some(SmoothingWorkers::start(inputs)?);
        Ok(())
    }

    pub fn stop_smoothing_workers(&mut self) {
        self.workers = None;
    }

    /// The selected platform model, `None` while the model setting is `'None'`.
    pub fn platform(&self) -> Option<&Platform> {
        self.platform.as_ref()
    }

    /// Select a platform model while keeping the frame-to-frame state; used when the
    /// model setting changes.
    pub fn set_platform(&mut self, platform: Platform) {
        self.platform = Some(platform);
    }

    /// Deselect the platform model (the model setting becomes `'None'`), keeping the frame-to-frame state.
    pub fn clear_platform(&mut self) {
        self.platform = None;
    }

    pub fn clip_factors(&self) -> &ClipFactors {
        &self.clips
    }

    /// One stream frame, from telemetry to the driver.
    ///
    /// While a game listener runs and a platform model is selected, the frame is computed; then,
    /// when `out` is connected with the motion switch on, the frame and the vertical acceleration are published and the
    /// frame is sent to the driver, and only after a successful send it becomes the reference of the next
    /// frame's rate shaping; with `debug_status` the driver's debug hook
    /// gets the frame. The listener is recorded for the next frame's joy rule unless
    /// something failed. On error the state already advanced for the frame stays advanced;
    /// the error is what the motion loop logs ([`CueError::refresh_motionbox_log`]).
    pub fn send_stream(
        &mut self,
        telemetry: &Telemetry,
        ctx: &StreamContext<'_>,
        out: &mut dyn StreamOutput,
        now: Instant,
    ) -> Result<(), CueError> {
        if let Some(positions) = self.compute_native(telemetry, ctx, now)? {
            let mut positions = bounded_positions(positions)?;
            if out.connected() && out.motion_switch_on() {
                out.publish(
                    &positions,
                    telemetry
                        .native_value("vertical_acceleration")
                        .map_or(Ok(telemetry.vertical_acceleration), |v| {
                            v.py_as_double().map_err(crate::pyvalue::native_error)
                        })?,
                );
                out.send_stream(&mut positions).map_err(|e| e.at(mh("send_stream", 1044)))?;
                self.record_delivered(&positions);
            }
            if out.debug_status() {
                out.debug(&positions).map_err(|e| e.at(mh("send_stream", 1051)))?;
            }
        }
        self.record_listener(ctx.settings.current_listener().unwrap_or(ctx.listener));
        Ok(())
    }

    /// The computation part of [`MotionCueing::send_stream`] for a caller that makes the driver calls itself:
    /// `Ok(None)` while no game listener runs or no platform model is selected. Nothing is recorded: after a
    /// successful driver call the caller records the frame as the driver left it
    /// ([`MotionCueing::record_delivered`]), and at the end of the call the listener
    /// ([`MotionCueing::record_listener`]), skipped when something failed.
    pub fn compute(
        &mut self,
        telemetry: &Telemetry,
        ctx: &StreamContext<'_>,
        now: Instant,
    ) -> Result<Option<Positions>, CueError> {
        self.compute_native(telemetry, ctx, now)?.map(bounded_positions).transpose()
    }
    pub fn compute_native(
        &mut self,
        telemetry: &Telemetry,
        ctx: &StreamContext<'_>,
        now: Instant,
    ) -> Result<Option<NativePositions>, CueError> {
        let active = !ctx.settings.current_listener().unwrap_or(ctx.listener).is_idle();
        if !active {
            return Ok(None);
        }
        let mut live_model = false;
        if let Some(model) = ctx.settings.current_model() {
            live_model = true;
            if model.map_err(|e| e.at(stream_frame(762)))?.equals_str("None") {
                return Ok(None);
            }
        }
        let fallback = if self.platform.is_none() && live_model {
            Some(crate::MotionConfig::builtin().platform("None").expect("None model").expect("valid model"))
        } else {
            None
        };
        match self.platform.as_ref().or(fallback.as_ref()) {
            Some(platform) => {
                let frame = Frame {
                    platform,
                    clips: &self.clips,
                    rng: self.rng.as_mut(),
                    ctx,
                    now,
                    workers: self.workers.as_ref(),
                    motion_config: self.motion_config.as_deref(),
                };
                self.state.advance(frame, &FrameInputs::read(telemetry)).map(Some)
            }
            _ => Ok(None),
        }
    }

    /// Records `positions` as the last delivered frame, after the driver's send returned.
    pub fn record_delivered(&mut self, positions: &Positions) {
        self.state.delivered = positions.map(|v| Some(LegacyValue::Int(v)));
    }
    pub fn record_native_delivered(&mut self, positions: &NativePositions) {
        self.state.delivered = std::array::from_fn(|i| Some(positions[i].clone()));
    }

    /// Records `listener` as the previous frame's listener.
    pub fn record_listener(&mut self, listener: Listener) {
        self.state.previous_listener = Some(listener);
    }
}

/// What one stream frame reads besides telemetry and the carried state.
struct Frame<'a> {
    platform: &'a Platform,
    clips: &'a ClipFactors,
    rng: &'a mut dyn EffectRng,
    ctx: &'a StreamContext<'a>,
    now: Instant,
    workers: Option<&'a SmoothingWorkers>,
    motion_config: Option<&'a crate::MotionConfig>,
}

/// The stream path's trace frame at `line`.
fn stream_frame(line: u32) -> crate::error::Frame {
    mh("send_stream", line)
}

/// The configuration section the mixing and shaping functions read: the model's manufacturer, or
/// `'DOF REALITY'` where the model has none or is unlisted.
pub(crate) fn sources<'a>(
    ctx_settings: &'a dyn ConfigView,
    tune: &'a dyn GameConfigView,
    platform: &Platform,
    game: &'a str,
) -> Sources<'a> {
    Sources { config: ctx_settings, game: tune, manufacturer: platform.manufacturer.name(), game_running: game }
}

impl StreamState {
    /// The body of send_stream under its listener/model gate, advancing the carried state.
    fn advance(&mut self, frame: Frame<'_>, input: &FrameInputs) -> Result<NativePositions, CueError> {
        let Frame { platform, clips, rng, ctx, now, workers, motion_config } = frame;
        let sources = sources(ctx.settings, ctx.tune, platform, ctx.game);
        let mut axes = input.axes.clone();
        self.track_movement(&input.movement_axes, ctx, &sources, now, workers)?;
        axes[Axis::Surge] = self.track_gear(&axes[Axis::Surge], &input.gear, ctx, &sources, now, workers)?;
        let direction = sources.model_field("motors_direction").map_err(|e| e.at(stream_frame(827)))?;
        if direction.is_none() && !platform.listed {
            return Err(CueError::key(&platform.model).at(stream_frame(827)));
        }
        let mapping = sources.model_field("mapping_id").map_err(|e| e.at(stream_frame(828)))?;
        let maximum = sources.model_field("max_motor_abs").map_err(|e| e.at(stream_frame(829)))?;
        let motors_direction = direction.map_or(Ok(platform.motors_direction), |v| v.as_double())?;
        let mapping_id = mapping.map_or(Ok(platform.mapping_id), |v| {
            v.to_native()
                .py_integer()
                .map_err(crate::pyvalue::native_error)
                .and_then(|v| v.try_into().map_err(|_| CueError::key("mapping_id")))
        })?;
        let max_motor_abs = maximum.map_or(Ok(platform.max_motor_abs), |v| {
            v.to_native()
                .py_integer()
                .map_err(crate::pyvalue::native_error)
                .and_then(|v| v.try_into().map_err(|_| CueError::key("max_motor_abs")))
        })?;
        let mut changed;
        let platform = if motors_direction != platform.motors_direction
            || mapping_id != platform.mapping_id
            || max_motor_abs != platform.max_motor_abs
        {
            changed = platform.clone();
            changed.motors_direction = motors_direction;
            changed.max_motor_abs = max_motor_abs;
            if mapping_id != platform.mapping_id {
                changed.mapping = motion_config
                    .unwrap_or_else(|| crate::MotionConfig::builtin())
                    .tables(mapping_id)
                    .ok_or_else(|| CueError::key(&mapping_id.to_string()))?
                    .to_vec();
                changed.mapping_id = mapping_id;
            }
            &changed
        } else {
            platform
        };

        // Each slot reads its shares/weights before normalizing its forces. A live profile edit
        // between slots therefore affects later slots.
        let normalized = || normalize_native(&axes, &sources, clips, || HeaveScale::select(&sources, platform));
        let factor = || gear_smoothing_factor(&sources);
        let window_ctx = WindowContext {
            sources,
            soft_start_engaged: self.soft_start.engaged(now),
            gear_smoothing: self.gear_smoothing.native_value(now, &factor),
            speed: input.speed.clone(),
        };

        // The per-slot mix of all 12 slots, each recording its history ...
        let mut powers = [Num::Int(0); SLOTS];
        for (slot, power) in powers.iter_mut().enumerate() {
            *power = self
                .motor_power(slot, platform, normalized, &window_ctx, workers)
                .map_err(|e| e.at(stream_frame(839)))?;
        }
        // ... then the output map of all slots, the travel scaling of all slots and the
        // rate shaping of all slots.
        let mut commands = [0.0; SLOTS];
        for (slot, command) in commands.iter_mut().enumerate() {
            *command = command_position(powers[slot], slot, platform, "send_stream", 850)?;
        }
        for (slot, command) in commands.iter_mut().enumerate() {
            *command = scale_travel(*command, slot, platform, &sources).map_err(|e| e.at(stream_frame(854)))?;
        }
        let mut positions: NativePositions = std::array::from_fn(|_| LegacyValue::Int(0));
        for (slot, position) in positions.iter_mut().enumerate() {
            let previous = self.delivered[slot].as_ref();
            let manufacturer = sources.current_manufacturer();
            *position = if matches!(manufacturer, "DOF REALITY" | "Yaw VR") {
                shape_rotary_native(commands[slot], previous, platform, &sources)
                    .map_err(|e| e.at(stream_frame(865)))?
            } else {
                shape_actuator_native(commands[slot], previous, platform, &sources)
                    .map_err(|e| e.at(stream_frame(867)))?
            };
        }

        apply_native_wheel_effects(&mut positions, platform, &sources, &input.events, rng)?;
        Ok(positions)
    }

    /// The stream-mode mix for one slot.
    fn motor_power(
        &mut self,
        slot: usize,
        platform: &Platform,
        normalized: impl FnOnce() -> Result<crate::mixing::Normalized, CueError>,
        window_ctx: &WindowContext<'_>,
        workers: Option<&SmoothingWorkers>,
    ) -> Result<Num, CueError> {
        let current_sources = window_ctx.sources.with_current_manufacturer();
        let sources = &current_sources;
        let window_ctx = WindowContext {
            sources: current_sources,
            soft_start_engaged: window_ctx.soft_start_engaged,
            gear_smoothing: window_ctx.gear_smoothing.clone(),
            speed: window_ctx.speed.clone(),
        };
        let enhanced = enhanced_type(sources)?;
        let mix = Platform::row(slot_table(platform, &enhanced)?, slot);
        let formulas = slot_formulas_with_normalize(normalized, mix, slot, platform, sources)?;
        // One effect at a time, its window and then its moving average.
        let mut smoothed = formulas;
        for axis in SMOOTHING_ORDER {
            let window = crate::smoothing::smoothing_window_with_workers(axis, &window_ctx, workers)?;
            smoothed[axis] = moving_average(formulas[axis], self.history.of(slot, axis), &window)
                .map_err(|e| e.at(cmp_frame(445)))?;
        }
        let power = slot_power(mix, &smoothed)?;
        self.history.push(slot, &smoothed);
        Ok(power)
    }

    /// The soft start/stop trigger of send_stream.
    fn track_movement(
        &mut self,
        axes: &[PyValue; 3],
        ctx: &StreamContext<'_>,
        sources: &Sources<'_>,
        now: Instant,
        workers: Option<&SmoothingWorkers>,
    ) -> Result<(), CueError> {
        // Two additions, yaw + roll then + pitch, before the joy override.
        let sum = axes[0]
            .binop(Op::Add, &axes[1])
            .and_then(|sum| sum.binop(Op::Add, &axes[2]))
            .map_err(|error| error.at(stream_frame(764)))?;
        let mut movement = sum;
        let game = || ctx.settings.current_game().unwrap_or_else(|| ctx.game.to_owned());
        if game() == "joy" {
            if game() == ctx.settings.current_previous_game().unwrap_or_else(|| ctx.previous_actual_game.to_owned()) {
                movement = PyValue::Int(1);
            }
            if self.previous_listener == Some(Listener::None) {
                movement = PyValue::Int(0);
            }
        }
        let engaged = workers.map_or_else(|| self.soft_start.engaged(now), SmoothingWorkers::soft_start_engaged);
        let was_still = self
            .previous_movement
            .as_ref()
            .map(|value| value.equals_checked(&PyValue::Int(0)))
            .transpose()
            .map_err(|e| e.at(stream_frame(776)))?
            .unwrap_or(false);
        let is_still = movement.equals_checked(&PyValue::Int(0)).map_err(|e| e.at(stream_frame(776)))?;
        if !engaged && was_still != is_still {
            if let Some(workers) = workers {
                workers.start_soft_start().map_err(|error| error.at(stream_frame(780)))?;
            } else {
                let timer = config_integer(sources.config, sources.manufacturer, "soft_start_stop_timer");
                self.soft_start.engage_value(now, timer);
            }
        }
        self.previous_movement = Some(movement);
        Ok(())
    }

    /// Gear smoothing of send_stream (legacy schedule or longitudinal follower); returns the
    /// surge input.
    fn track_gear(
        &mut self,
        longitudinal: &PyValue,
        gear: &PyValue,
        ctx: &StreamContext<'_>,
        sources: &Sources<'_>,
        now: Instant,
        workers: Option<&SmoothingWorkers>,
    ) -> Result<PyValue, CueError> {
        let legacy = config_bool(sources.config, "CUSTOM_FEATURES", "enable_legacy_gear_smoothing")
            .map_err(|e| e.at(stream_frame(785)))?;
        let surge = if legacy {
            if !gear.equals_checked(&self.previous_gear).map_err(|e| e.at(stream_frame(786)))? {
                if let Some(workers) = workers {
                    workers.start_gear_smoothing().map_err(|error| error.at(stream_frame(794)))?;
                } else {
                    let factor = || gear_smoothing_factor(sources);
                    self.gear_smoothing.on_gear_change_value(now, factor(), &factor);
                }
            }
            longitudinal.clone()
        } else {
            follow_longitudinal_native(
                &self.previous_longitudinal,
                longitudinal,
                || {
                    config_integer(sources.config, "MOTIONBOX", "gear_smoothing_min_delta")
                        .map_err(|e| e.at(stream_frame(808)))
                },
                || gear_smoothing_factor(sources).map_err(|e| e.at(stream_frame(810))),
                ctx.settings.current_game_kind().unwrap_or(ctx.game_kind) == GameKind::Racing,
            )?
        };
        self.previous_gear = gear.clone();
        self.previous_longitudinal = surge.clone();
        Ok(surge)
    }
}

/// The game configuration's `gear_smoothing_factor`, as an integer.
fn gear_smoothing_factor(sources: &Sources<'_>) -> Result<PyValue, CueError> {
    game_item(sources.game, "gear_smoothing_factor")?.to_integer_value()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    #[test]
    fn imported_dictionary_gear_changes_restart_legacy_smoothing() {
        use openrigcue_model::native_value::{Dict, Key, LegacyValue};
        let gear = |value| {
            let mut fields = Dict::new();
            fields.insert(Key::Int(1), LegacyValue::Int(value));
            LegacyValue::Dict(fields)
        };
        let mut run = Run::new("dof-reality-p3", Manufacturer::DofReality);
        run.settings.legacy_gear_smoothing = true;
        let mut cueing = run.cueing();
        let now = Instant::now();
        let mut frame = Telemetry::off();
        frame.set_native("gear", gear(2));
        let context = idle_ctx(&run.settings, &run.tune, Listener::Game, run.game);
        step(&mut cueing, &frame, &context, now).unwrap();
        assert_eq!(cueing.state.gear_smoothing.value(now, &|| Ok(95)), Some(95));

        run.tune.gear_smoothing_factor = 42;
        let later = now + Duration::from_millis(100);
        frame.set_native("gear", gear(3));
        let context = idle_ctx(&run.settings, &run.tune, Listener::Game, run.game);
        step(&mut cueing, &frame, &context, later).unwrap();
        assert_eq!(cueing.state.gear_smoothing.value(later, &|| Ok(42)), Some(42));
        assert!(cueing.state.previous_gear.equals(&PyValue::from(&gear(3))));

        run.tune.gear_smoothing_factor = 21;
        let context = idle_ctx(&run.settings, &run.tune, Listener::Game, run.game);
        step(&mut cueing, &frame, &context, later).unwrap();
        assert_eq!(cueing.state.gear_smoothing.value(later, &|| Ok(21)), Some(42));
    }

    #[test]
    fn production_stream_starts_owned_workers_instead_of_nominal_schedules() {
        struct Inputs;
        impl crate::SmoothingInputs for Inputs {
            fn soft_start_manufacturer(&self) -> String {
                "DOF REALITY".into()
            }
            fn soft_start_timer(&self, _: &str) -> Result<i128, CueError> {
                Ok(3600)
            }
            fn gear_smoothing_factor(&self) -> Result<i128, CueError> {
                Ok(73)
            }
        }
        let mut run = Run::new("dof-reality-p3", Manufacturer::DofReality);
        run.settings.legacy_gear_smoothing = true;
        let mut cueing = run.cueing();
        cueing.start_smoothing_workers(std::sync::Arc::new(Inputs)).unwrap();
        let context = ctx(&run.settings, &run.tune, Listener::Game, run.game, run.kind);
        cueing.compute(&telemetry(&[("gear", 1.0)]), &context, Instant::now()).unwrap();
        let workers = cueing.workers.as_ref().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !workers.soft_start_engaged() || workers.gear_smoothing() != Some(73) {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(cueing.state.soft_start, SoftStart::Idle);
        assert_eq!(cueing.state.gear_smoothing, GearSmoothing::default());
        cueing.stop_smoothing_workers();
    }

    #[test]
    fn stream_normalizes_each_slot_after_weights_using_current_profile_values() {
        use std::borrow::Cow;
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct ChangingTune {
            tune: MotionTune,
            maxima_reads: AtomicUsize,
            weight_reads: AtomicUsize,
        }
        impl GameConfigView for ChangingTune {
            fn item(&self, key: &str) -> Option<Cow<'_, PyValue>> {
                if key == "motion_switch" {
                    self.weight_reads.fetch_add(1, Ordering::SeqCst);
                }
                let value = self.tune.item(key)?;
                if key != "max_telemetry_vars" {
                    return Some(value);
                }
                // Every normalization begins only after this slot's nine weight reads.
                let weights = self.weight_reads.load(Ordering::SeqCst);
                assert!(weights >= 9);
                let mut value = value.into_owned();
                self.maxima_reads.fetch_add(1, Ordering::SeqCst);
                let max_pitch = if weights <= 9 { 5.0 } else { 10.0 };
                if let PyValue::Dict(values) = &mut value {
                    *values.iter_mut().find(|(key, _)| key == "pitch").unwrap() =
                        ("pitch".into(), PyValue::Float(max_pitch));
                }
                Some(Cow::Owned(value))
            }
        }
        let run = Run::new("dof-reality-p3", Manufacturer::DofReality);
        let tune = ChangingTune {
            tune: run.tune.clone(),
            maxima_reads: AtomicUsize::new(0),
            weight_reads: AtomicUsize::new(0),
        };
        let context = ctx(&run.settings, &tune, Listener::Game, run.game, run.kind);
        let mut cueing = run.cueing();
        cueing.compute(&telemetry(&[("pitch", 1.0), ("gear", 1.0)]), &context, Instant::now()).unwrap();
        assert_eq!(tune.maxima_reads.load(Ordering::SeqCst), 8 * SLOTS);
        let value = |slot| match cueing.state.history.of(slot, Axis::Pitch).back().unwrap() {
            Num::Real(value) => *value,
            Num::Int(value) => *value as f64,
            value => panic!("unexpected complex pitch: {value:?}"),
        };
        let slot0 = value(0);
        let slot1 = value(1);
        assert_ne!(slot0, 0.0);
        assert_eq!(slot0, slot1 * 2.0, "the second slot must read the edited pitch maximum");
        tune.weight_reads.store(0, Ordering::SeqCst);
        tune.maxima_reads.store(0, Ordering::SeqCst);
        let sliders = crate::TestSliders { pitch: -10.0, ..Default::default() };
        cueing.test(&sliders, &crate::TestContext { settings: &run.settings, tune: &tune, game: run.game }).unwrap();
        assert_eq!(
            tune.maxima_reads.load(Ordering::SeqCst),
            8 * SLOTS,
            "test mode must also normalize after each slot's weights"
        );
    }

    use super::*;
    use crate::external::ExternalMixing;
    use crate::platform::{builtin, Manufacturer, MotionConfig};
    use crate::settings::MotionSettings;
    use crate::timing::HostOs;
    use crate::tune::MotionTune;

    /// Answers every randint with its upper bound, so effect amplitudes are deterministic.
    struct MaxRng;

    impl EffectRng for MaxRng {
        fn randint(&mut self, _low: i128, high: i128) -> i128 {
            high
        }
    }

    fn telemetry(fields: &[(&str, f64)]) -> Telemetry {
        let mut t = Telemetry::off();
        for name in Telemetry::CHANNELS {
            *t.get_mut(name).unwrap() = 0.0;
        }
        for (name, value) in fields {
            *t.get_mut(name).unwrap() = *value;
        }
        t
    }

    /// Four fixed test frames.
    fn scenario() -> Vec<Telemetry> {
        vec![
            telemetry(&[
                ("roll", 1.0),
                ("pitch", 0.5),
                ("yaw", 0.2),
                ("lateral_acceleration", 1.2),
                ("longitudinal_acceleration", -0.8),
                ("vertical_acceleration", 0.3),
                ("lateral_velocity", 0.5),
                ("gear", 3.0),
                ("speed", 30.0),
                ("rpm", 3000.0),
                ("max_rpm", 8000.0),
            ]),
            telemetry(&[
                ("roll", 2.0),
                ("pitch", 1.5),
                ("yaw", 0.4),
                ("lateral_acceleration", 2.0),
                ("longitudinal_acceleration", 2.5),
                ("vertical_acceleration", -0.6),
                ("lateral_velocity", 1.0),
                ("gear", 3.0),
                ("speed", 31.0),
                ("wheel_slip_lr", 4.0),
                ("rumble_pitch_rf", 6.0),
            ]),
            telemetry(&[
                ("roll", -3.0),
                ("pitch", -2.0),
                ("yaw", 0.1),
                ("lateral_acceleration", -2.5),
                ("longitudinal_acceleration", -3.0),
                ("vertical_acceleration", 1.2),
                ("lateral_velocity", -2.0),
                ("front_tl", 1.5),
                ("gear", 4.0),
                ("speed", 32.0),
                ("cannon_firing", 1.0),
            ]),
            telemetry(&[
                ("roll", -2.9),
                ("pitch", -2.1),
                ("yaw", 0.15),
                ("lateral_acceleration", -2.4),
                ("longitudinal_acceleration", -2.9),
                ("vertical_acceleration", 1.1),
                ("lateral_velocity", -1.9),
                ("front_tl", 1.4),
                ("gear", 4.0),
                ("speed", 33.0),
            ]),
        ]
    }

    /// A fake driver side: the runtime state and the driver calls at the end of send_stream.
    #[derive(Default)]
    struct Sink {
        connected: bool,
        switch_on: bool,
        debug_status: bool,
        /// The error the driver's send fails with.
        fail_send: Option<CueError>,
        /// Calls in order: "publish", "send", "debug".
        calls: Vec<&'static str>,
        published: Vec<(Positions, f64)>,
        sent: Vec<Positions>,
        /// Every computed frame, through the debug hook.
        computed: Vec<Positions>,
    }

    impl Sink {
        fn delivering() -> Sink {
            Sink { connected: true, switch_on: true, debug_status: true, ..Sink::default() }
        }
    }

    impl StreamOutput for Sink {
        fn connected(&self) -> bool {
            self.connected
        }
        fn motion_switch_on(&self) -> bool {
            self.switch_on
        }
        fn publish(&mut self, positions: &Positions, heave: f64) {
            self.calls.push("publish");
            self.published.push((*positions, heave));
        }
        fn send_stream(&mut self, positions: &mut Positions) -> Result<(), CueError> {
            self.calls.push("send");
            if let Some(e) = self.fail_send.clone() {
                return Err(e);
            }
            self.sent.push(*positions);
            Ok(())
        }
        fn debug_status(&self) -> bool {
            self.debug_status
        }
        fn debug(&mut self, positions: &Positions) -> Result<(), CueError> {
            self.calls.push("debug");
            self.computed.push(*positions);
            Ok(())
        }
    }

    struct Run {
        platform: &'static str,
        settings: MotionSettings,
        tune: MotionTune,
        game: &'static str,
        kind: GameKind,
        active: Vec<bool>,
    }

    impl Run {
        fn new(platform: &'static str, manufacturer: Manufacturer) -> Self {
            Run {
                platform,
                settings: MotionSettings::for_manufacturer(manufacturer, HostOs::Windows),
                tune: MotionTune::default(),
                game: "dirtrally2",
                kind: GameKind::Racing,
                active: vec![true; 8],
            }
        }

        fn cueing(&self) -> MotionCueing {
            let platform = builtin(self.platform).unwrap().unwrap();
            MotionCueing::with_rng(Some(platform), ClipFactors::default(), Box::new(MaxRng))
        }

        /// send_stream over `frames`, the controller connected on the frames of `active`; every computed frame.
        fn frames(&self, frames: &[Telemetry]) -> Vec<Positions> {
            let mut cueing = self.cueing();
            let t0 = Instant::now();
            let mut sink = Sink::delivering();
            for (i, t) in frames.iter().enumerate() {
                sink.connected = self.active[i];
                let ctx = ctx(&self.settings, &self.tune, Listener::Game, self.game, self.kind);
                let now = t0 + Duration::from_millis(33 * i as u64);
                cueing.send_stream(t, &ctx, &mut sink, now).unwrap();
            }
            sink.computed
        }
    }

    fn padded(head: &[i64], rest: i64) -> Positions {
        let mut p = [rest; SLOTS];
        p[..head.len()].copy_from_slice(head);
        p
    }

    fn ctx<'a>(
        settings: &'a dyn ConfigView,
        tune: &'a dyn GameConfigView,
        listener: Listener,
        game: &'a str,
        kind: GameKind,
    ) -> StreamContext<'a> {
        StreamContext { settings, tune, listener, game, previous_actual_game: game, game_kind: kind }
    }

    fn idle_ctx<'a>(
        settings: &'a dyn ConfigView,
        tune: &'a dyn GameConfigView,
        listener: Listener,
        game: &'a str,
    ) -> StreamContext<'a> {
        ctx(settings, tune, listener, game, GameKind::Racing)
    }

    /// One send_stream frame into a delivering sink: the computed frame, or the error.
    fn step(
        c: &mut MotionCueing,
        t: &Telemetry,
        ctx: &StreamContext<'_>,
        now: Instant,
    ) -> Result<Option<Positions>, CueError> {
        let mut sink = Sink::delivering();
        c.send_stream(t, ctx, &mut sink, now)?;
        Ok(sink.computed.pop())
    }

    /// `(message, innermost frame line)`.
    fn raised<T: std::fmt::Debug>(r: Result<T, CueError>) -> (String, Option<u32>) {
        let e = r.unwrap_err();
        (e.to_string(), e.frames().last().map(|f| f.line))
    }

    #[test]
    fn send_stream_p3_reference_frames() {
        let out = Run::new("dof-reality-p3", Manufacturer::DofReality).frames(&scenario());
        let expected = [
            [2230, 2269, 2112, 2047, 2047, 2047, 2055],
            [2663, 2081, 2182, 2047, 2047, 2047, 2027],
            [1892, 1169, 1841, 2047, 2047, 2047, 2085],
            [1258, 1794, 1890, 2047, 2047, 2047, 2076],
        ];
        assert_eq!(out.len(), 4);
        for (got, want) in out.iter().zip(expected) {
            assert_eq!(*got, padded(&want, 2047));
        }
    }

    #[test]
    fn send_stream_h6_enhanced_and_normal_reference_frames() {
        let out = Run::new("dof-reality-h6", Manufacturer::DofReality).frames(&scenario());
        let expected = [
            [1845, 2212, 1900, 1806, 2251, 1972],
            [1551, 2625, 1924, 1974, 2322, 1704],
            [3032, 1603, 2139, 2639, 1973, 2683],
            [2737, 1271, 2074, 2243, 1768, 2687],
        ];
        for (got, want) in out.iter().zip(expected) {
            assert_eq!(*got, padded(&want, 2047));
        }

        let mut flying = Run::new("dof-reality-h6", Manufacturer::DofReality);
        flying.tune.enhanced_gforces = false;
        flying.game = "dcs";
        flying.kind = GameKind::Other;
        let out = flying.frames(&scenario());
        let expected = [
            [1956, 2359, 1836, 1917, 2103, 1908],
            [1425, 2421, 2147, 2484, 2526, 1292],
            [3008, 1943, 1940, 2107, 1696, 2930],
            [2703, 1619, 1937, 1846, 1414, 2920],
        ];
        for (got, want) in out.iter().zip(expected) {
            assert_eq!(*got, padded(&want, 2047));
        }
    }

    #[test]
    fn send_stream_actuator_reference_frames() {
        let mut prosimu = Run::new("prosimu-t1000-5m-prs200", Manufacturer::Prosimu);
        prosimu.tune.smoothing_factor = 50.0;
        let out = prosimu.frames(&scenario());
        let expected = [
            [35702, 29214, 31735, 36587, 30098, 33947],
            [35920, 30057, 31678, 37140, 29835, 33704],
            [36023, 31375, 31892, 37622, 31611, 33878],
            [35200, 31668, 32082, 37192, 32371, 34037],
        ];
        for (got, want) in out.iter().zip(expected) {
            assert_eq!(*got, padded(&want, 32768));
        }

        let mut belt = Run::new("pt-actuator-6-actuators-2-front-2-rear-surge-belt-canbus", Manufacturer::PtActuator);
        belt.tune.belt_tensioner = 30.0;
        belt.tune.max_horizontal = 60.0;
        let out = belt.frames(&scenario());
        let expected = [
            [36587, 35702, 29214, 30098, 33298, 30644],
            [37649, 39676, 28796, 25328, 32975, 31082],
            [30165, 22045, 38266, 46386, 33345, 30768],
            [29804, 21073, 36704, 45435, 33396, 30483],
        ];
        for (got, want) in out.iter().zip(expected) {
            assert_eq!(*got, padded(&want, 32768));
        }
    }

    #[test]
    fn send_stream_soft_start_from_standstill() {
        // A still first frame engages soft start (window 30 for DOF REALITY) for 4 s.
        let mut frames = vec![telemetry(&[("gear", 1.0)])];
        frames.extend(scenario());
        let out = Run::new("dof-reality-p3", Manufacturer::DofReality).frames(&frames);
        let expected = [
            [2047, 2047, 2047, 2047, 2047, 2047, 2047],
            [2248, 2291, 2118, 2047, 2047, 2047, 2055],
            [2662, 2079, 2181, 2047, 2047, 2047, 2027],
            [1892, 1169, 1841, 2047, 2047, 2047, 2085],
            [1258, 1794, 1890, 2047, 2047, 2047, 2076],
        ];
        for (got, want) in out.iter().zip(expected) {
            assert_eq!(*got, padded(&want, 2047));
        }
    }

    #[test]
    fn send_stream_legacy_gear_smoothing() {
        let mut run = Run::new("dof-reality-p3", Manufacturer::DofReality);
        run.settings.legacy_gear_smoothing = true;
        run.tune.effect_smoothing = PerAxis::splat(5.0);
        let out = run.frames(&scenario());
        let expected = [
            [2230, 2269, 2112, 2047, 2047, 2047, 2055],
            [2981, 1763, 2182, 2047, 2047, 2047, 2027],
            [2259, 1759, 2008, 2047, 2047, 2047, 2057],
            [1581, 1982, 1949, 2047, 2047, 2047, 2066],
        ];
        for (got, want) in out.iter().zip(expected) {
            assert_eq!(*got, padded(&want, 2047));
        }
    }

    #[test]
    fn legacy_gear_change_keeps_integer_identity_above_float_precision() {
        use openrigcue_model::native_value::LegacyValue;
        let mut run = Run::new("dof-reality-p3", Manufacturer::DofReality);
        run.settings.legacy_gear_smoothing = true;
        let mut cueing = run.cueing();
        let now = Instant::now();
        let mut frame = Telemetry::off();
        frame.set_native("gear", LegacyValue::Int(9_007_199_254_740_993));
        let context = idle_ctx(&run.settings, &run.tune, Listener::Game, run.game);
        step(&mut cueing, &frame, &context, now).unwrap();
        assert_eq!(cueing.state.gear_smoothing.value(now, &|| Ok(95)), Some(95));

        // Both integer gears have the same f64 projection, but the gear
        // comparison uses the exact integers and must start another invocation.
        frame.set_native("gear", LegacyValue::Int(9_007_199_254_740_992));
        run.tune.gear_smoothing_factor = 55;
        let context = idle_ctx(&run.settings, &run.tune, Listener::Game, run.game);
        step(&mut cueing, &frame, &context, now).unwrap();
        assert_eq!(cueing.state.gear_smoothing.value(now, &|| Ok(55)), Some(55));

        // Numeric equality still crosses types: the matching float must not
        // restart smoothing merely because its stored representation changed.
        frame.set_native("gear", LegacyValue::Float(9_007_199_254_740_992.0));
        run.tune.gear_smoothing_factor = 25;
        let context = idle_ctx(&run.settings, &run.tune, Listener::Game, run.game);
        step(&mut cueing, &frame, &context, now).unwrap();
        assert_eq!(cueing.state.gear_smoothing.value(now, &|| Ok(25)), Some(55));
    }

    #[test]
    fn send_stream_previous_output_only_from_delivered_frames() {
        let mut run = Run::new("dof-reality-p3", Manufacturer::DofReality);
        run.active = vec![true, false, true, true];
        let out = run.frames(&scenario());
        let expected = [
            [2230, 2269, 2112, 2047, 2047, 2047, 2055],
            [2663, 2081, 2182, 2047, 2047, 2047, 2027],
            [1888, 1150, 1848, 2047, 2047, 2047, 2082],
            [1258, 1794, 1889, 2047, 2047, 2047, 2076],
        ];
        for (got, want) in out.iter().zip(expected) {
            assert_eq!(*got, padded(&want, 2047));
        }
    }

    #[test]
    fn send_stream_publishes_sends_and_then_records_the_frame() {
        // The publish first, then the driver send, then the delivered-frame record.
        let run = Run::new("dof-reality-p3", Manufacturer::DofReality);
        let mut c = run.cueing();
        let frames = scenario();
        let ctx = idle_ctx(&run.settings, &run.tune, Listener::Game, run.game);
        let now = Instant::now();
        let mut sink = Sink::delivering();
        c.send_stream(&frames[0], &ctx, &mut sink, now).unwrap();
        assert_eq!(sink.calls, ["publish", "send", "debug"]);
        assert_eq!(sink.published, [(sink.sent[0], 0.3)]);
        assert_eq!(c.state.delivered, sink.sent[0].map(|v| Some(LegacyValue::Int(v))));
        assert_eq!(c.state.previous_listener, Some(Listener::Game));

        // A driver that fails: the error leaves send_stream, the delivered state keeps the
        // last delivered frame, the debug hook is not reached and the listener is not recorded.
        c.state.previous_listener = None;
        let mut failing = Sink { fail_send: Some(CueError::key("smc3")), ..Sink::delivering() };
        let err = c.send_stream(&frames[1], &ctx, &mut failing, now).unwrap_err();
        assert_eq!(failing.calls, ["publish", "send"]);
        assert_eq!(
            err.refresh_motionbox_log(),
            "error sending data to motionbox: 'smc3', traceback: Traceback (most recent call last):\n  \
             File \"backend/controllers/controllers.py\", line 418, in backend.controllers.controllers.refresh_motionbox\n  \
             File \"backend/controllers/motion_helpers.py\", line 1044, in backend.controllers.motion_helpers.send_stream\n\
             KeyError: 'smc3'\n"
        );
        assert_eq!(c.state.delivered, sink.sent[0].map(|v| Some(LegacyValue::Int(v))));
        assert_eq!(c.state.previous_listener, None);

        // Not connected, or the motion switch off: nothing is published or sent; the debug hook still runs.
        let mut off = Sink { switch_on: false, ..Sink::delivering() };
        c.send_stream(&frames[2], &ctx, &mut off, now).unwrap();
        assert_eq!(off.calls, ["debug"]);
        let mut quiet = Sink { connected: true, switch_on: true, ..Sink::default() };
        c.send_stream(&frames[3], &ctx, &mut quiet, now).unwrap();
        assert_eq!(quiet.calls, ["publish", "send"]);
    }

    #[test]
    fn compute_records_nothing() {
        // The caller records the delivered frame and the listener itself.
        let run = Run::new("dof-reality-p3", Manufacturer::DofReality);
        let mut c = run.cueing();
        let ctx = idle_ctx(&run.settings, &run.tune, Listener::Game, run.game);
        let now = Instant::now();
        let p = c.compute(&scenario()[0], &ctx, now).unwrap().unwrap();
        assert_eq!(c.state.delivered, [const { None }; SLOTS]);
        assert_eq!(c.state.previous_listener, None);
        c.record_delivered(&p);
        c.record_listener(Listener::Game);
        assert_eq!(c.state.delivered, p.map(|v| Some(LegacyValue::Int(v))));
        assert_eq!(c.state.previous_listener, Some(Listener::Game));
        assert_eq!(
            c.compute(&scenario()[0], &idle_ctx(&run.settings, &run.tune, Listener::None, "none"), now),
            Ok(None)
        );
    }

    #[test]
    fn send_stream_is_gated_on_the_listener() {
        let platform = builtin("dof-reality-p3").unwrap().unwrap();
        let mut c = MotionCueing::with_rng(Some(platform), ClipFactors::default(), Box::new(MaxRng));
        let (s, t) = (MotionSettings::default(), MotionTune::default());
        let now = Instant::now();
        for listener in [Listener::None, Listener::Emulator, Listener::Test] {
            let mut sink = Sink::delivering();
            c.send_stream(&scenario()[0], &idle_ctx(&s, &t, listener, "none"), &mut sink, now).unwrap();
            assert!(sink.calls.is_empty());
        }
        assert_eq!(c.state.previous_listener, Some(Listener::Test));
        assert!(c.state.previous_movement.is_none());
    }

    #[test]
    fn send_stream_joy_movement_rules() {
        let platform = builtin("dof-reality-p3").unwrap().unwrap();
        let mut c = MotionCueing::with_rng(Some(platform), ClipFactors::default(), Box::new(MaxRng));
        let (s, t) = (MotionSettings::default(), MotionTune::default());
        let now = Instant::now();
        let still = telemetry(&[]);
        // joy and joy == previous actual game: movement forced to 1 (no soft start on the first frame).
        let ctx = idle_ctx(&s, &t, Listener::Game, "joy");
        step(&mut c, &still, &ctx, now).unwrap();
        assert_eq!(c.state.previous_movement, Some(PyValue::Int(1)));
        assert!(!c.state.soft_start.engaged(now));
        // After a frame with no listener the joy movement is forced to 0: soft start engages.
        step(&mut c, &still, &idle_ctx(&s, &t, Listener::None, "joy"), now).unwrap();
        step(&mut c, &still, &ctx, now).unwrap();
        assert_eq!(c.state.previous_movement, Some(PyValue::Int(0)));
        assert!(c.state.soft_start.engaged(now));
    }

    #[test]
    fn send_stream_without_a_model_only_records_the_listener() {
        // send_stream: no frame while MOTIONBOX.model is 'None'; still records the listener.
        let mut c = MotionCueing::with_rng(None, ClipFactors::default(), Box::new(MaxRng));
        let (s, t) = (MotionSettings::default(), MotionTune::default());
        let now = Instant::now();
        let still = telemetry(&[]);
        let joy = idle_ctx(&s, &t, Listener::Game, "joy");
        assert_eq!(step(&mut c, &still, &joy, now), Ok(None));
        assert_eq!(c.state.previous_listener, Some(Listener::Game));
        assert!(c.state.previous_movement.is_none());

        // A model-less frame with no listener, then a joy frame once a model is selected: the joy rule
        // sees `none` as the previous listener and forces movement 0.
        c.set_platform(builtin("dof-reality-p3").unwrap().unwrap());
        step(&mut c, &still, &joy, now).unwrap();
        assert_eq!(c.state.previous_movement, Some(PyValue::Int(1)));
        c.clear_platform();
        assert_eq!(step(&mut c, &still, &idle_ctx(&s, &t, Listener::None, "joy"), now), Ok(None));
        assert_eq!(c.state.previous_listener, Some(Listener::None));
        // The rest of the state survives the model-less period.
        assert_eq!(c.state.previous_movement, Some(PyValue::Int(1)));
        assert_eq!(c.state.history.of(0, Axis::Pitch).len(), 1);
        assert!(c.state.delivered[0].is_some());
        c.set_platform(builtin("dof-reality-p3").unwrap().unwrap());
        step(&mut c, &still, &joy, now).unwrap();
        assert_eq!(c.state.previous_movement, Some(PyValue::Int(0)));
        assert!(c.state.soft_start.engaged(now));
    }

    #[test]
    fn send_stream_error_keeps_listener_but_advances_state() {
        let platform = builtin("dof-reality-p3").unwrap().unwrap();
        let mut c = MotionCueing::with_rng(Some(platform), ClipFactors::default(), Box::new(MaxRng));
        let s = MotionSettings::default();
        // event amplitude 0 -> randint(3, 0)
        let t = MotionTune { max_power: 1.0, ..MotionTune::default() };
        let mut frame = scenario()[0].clone();
        frame.cannon_firing = 1.0;
        let ctx = idle_ctx(&s, &t, Listener::Game, "dirtrally2");
        let err = step(&mut c, &frame, &ctx, Instant::now()).unwrap_err();
        assert_eq!(err.to_string(), "empty range in randrange(3, 1)");
        let frames: Vec<_> = err.frames().map(|f| (f.function, f.line)).collect();
        assert_eq!(frames, [("send_stream", 1003), ("calc_wheel_effect", 1066)]);
        assert_eq!(c.state.previous_listener, None);
        assert!(c.state.previous_gear.equals(&PyValue::Float(3.0)));
        assert_eq!(c.state.history.of(0, Axis::Pitch).len(), 1);
        assert_eq!(c.state.delivered, [const { None }; SLOTS]);
    }

    /// Runs frames, returning each frame's result and the sway history length and complex count per slot.
    fn complex_run(
        id: &str,
        tune: &MotionTune,
        settings: &mut MotionSettings,
        frames: &[Telemetry],
        switch_at: usize,
    ) -> (MotionCueing, Vec<Result<Option<Positions>, CueError>>) {
        let platform = builtin(id).unwrap().unwrap();
        let mut c = MotionCueing::with_rng(Some(platform), ClipFactors::default(), Box::new(MaxRng));
        let t0 = Instant::now();
        let mut out = Vec::new();
        for (i, f) in frames.iter().enumerate() {
            if i == switch_at {
                settings.sway_curving_factor = 1.0;
            }
            let ctx = idle_ctx(settings, tune, Listener::Game, "dirtrally2");
            out.push(step(&mut c, f, &ctx, t0 + Duration::from_millis(33 * i as u64)));
        }
        (c, out)
    }

    fn sway_history(c: &MotionCueing, slot: usize) -> (usize, usize) {
        let h = c.state.history.of(slot, Axis::Sway);
        (h.len(), h.iter().filter(|v| matches!(v, Num::Complex(_))).count())
    }

    #[test]
    fn send_stream_complex_sway_fails_at_the_map_and_stays_in_the_history() {
        // P3, sway maximum -18, sway curving
        // factor 1.5. Every frame fails with a type error in the output map after all 12 slots recorded their
        // (complex) history; after the curving factor goes back to 1 at frame 7 the averaged history is
        // still complex and frames keep failing.
        let mut tune = MotionTune::default();
        tune.maxima.sway = -18.0;
        let mut settings = MotionSettings { sway_curving_factor: 1.5, ..MotionSettings::default() };
        let frames: Vec<_> = scenario().into_iter().cycle().take(12).collect();
        let (c, out) = complex_run("dof-reality-p3", &tune, &mut settings, &frames, 7);
        for r in out {
            assert_eq!(raised(r), ("must be real number, not complex".to_owned(), Some(204)));
        }
        for slot in 0..SLOTS {
            assert_eq!(sway_history(&c, slot), (12, 12), "slot {slot}");
        }
        assert_eq!(c.state.previous_listener, None);
    }

    #[test]
    fn send_stream_complex_sway_fails_at_a_deadzone() {
        // H6 with enhanced g-forces off, sway maximum -1, curving 1.5: slot 2 has a sway
        // deadzone of 50, so the sign test of its complex share fails with a type error there after
        // slots 0 and 1 recorded history.
        let mut tune = MotionTune { enhanced_gforces: false, ..MotionTune::default() };
        tune.maxima.sway = -1.0;
        let mut settings = MotionSettings { sway_curving_factor: 1.5, ..MotionSettings::default() };
        let frames: Vec<_> = scenario().into_iter().take(2).collect();
        let (c, out) = complex_run("dof-reality-h6", &tune, &mut settings, &frames, 99);
        for r in out {
            assert_eq!(raised(r).1, Some(370));
        }
        assert_eq!(sway_history(&c, 0), (2, 2));
        assert_eq!(sway_history(&c, 1), (2, 2));
        for slot in 2..SLOTS {
            assert_eq!(sway_history(&c, slot), (0, 0), "slot {slot}");
        }
    }

    /// P3 of `MotionConfig` with these external tables.
    fn external_p3(ext: &ExternalMixing) -> Platform {
        MotionConfig::builtin().clone().with_external_mixing(ext).platform("P3").unwrap().unwrap()
    }

    #[test]
    fn send_stream_external_tables_replace_the_normal_table() {
        // Seven-column CSV rows (as the built-in defaults have): the mixing step fails at column 7
        // on slot 0, so a P3 frame with enhanced g-forces off fails; with them on the built-in table runs.
        let ext =
            ExternalMixing::from_files(None, None, Some((std::path::Path::new("p.csv"), b"h\n0,1,2,3,4,5,6,7\n")))
                .unwrap()
                .unwrap();
        let mut c = MotionCueing::with_rng(Some(external_p3(&ext)), ClipFactors::default(), Box::new(MaxRng));
        let s = MotionSettings::default();
        let off = MotionTune { enhanced_gforces: false, ..MotionTune::default() };
        let on = MotionTune::default();
        let now = Instant::now();
        assert_eq!(
            raised(step(&mut c, &scenario()[0], &idle_ctx(&s, &off, Listener::Game, "dirtrally2"), now)),
            ("list index out of range".to_owned(), Some(215))
        );
        assert!(step(&mut c, &scenario()[0], &idle_ctx(&s, &on, Listener::Game, "dirtrally2"), now).unwrap().is_some());
        // Platforms outside mapping ids 2, 3 and 6 are not affected.
        let config = MotionConfig::builtin().clone().with_external_mixing(&ext);
        assert_eq!(
            config.platform("Yaw3").unwrap().unwrap(),
            MotionConfig::builtin().platform("Yaw3").unwrap().unwrap()
        );
        assert_ne!(config.platform("P2").unwrap().unwrap(), MotionConfig::builtin().platform("P2").unwrap().unwrap());
    }

    #[test]
    fn send_stream_external_tables_reference_frames() {
        // These 12-row CSVs loaded as the external mixing tables, then stream frames for P3 with
        // enhanced g-forces off.
        let table = |header: &str, rows: &[&str], rest: &str| {
            let mut text = format!("{header}\n");
            for i in 0..SLOTS {
                text.push_str(&format!("m{i},{}\n", rows.get(i).copied().unwrap_or(rest)));
            }
            text
        };
        let dir = table(
            "motor,p,r,y,sw,su,h,tl,tlf",
            &["+,+,0,+,+,+,0,0", "+,-,0,-,+,+,0,0", "0,0,+,0,0,0,+,+", "0,0,0,0,0,0,0,0", "0,0,0,0,0,0,0,0"]
                .into_iter()
                .chain(["-,0,0,0,0,+,0,0", "0,0,0,0,0,+,0,0"])
                .collect::<Vec<_>>(),
            "0,0,0,0,0,0,0,0",
        );
        let dz = table("motor,r,p,y,sw,su,h,tl,tlf", &["30,0,0,0,50,0,0,0"], "0,0,0,0,0,0,0,0");
        let full = "100,100,100,100,100,100,100,100";
        let pc = table(
            "motor,p,r,y,sw,su,h,tl,tlf",
            &[
                "100,80,100,60,100,100,100,100",
                full,
                "100,100,50,100,100,100,100,200",
                full,
                full,
                "40,100,100,100,100,100,100,100",
            ],
            full,
        );
        let p = std::path::Path::new("x.csv");
        let ext =
            ExternalMixing::from_files(Some((p, dir.as_bytes())), Some((p, dz.as_bytes())), Some((p, pc.as_bytes())))
                .unwrap()
                .unwrap();
        let mut run = Run::new("dof-reality-p3", Manufacturer::DofReality);
        run.tune.enhanced_gforces = false;
        let mut cueing = MotionCueing::with_rng(Some(external_p3(&ext)), ClipFactors::default(), Box::new(MaxRng));
        let t0 = Instant::now();
        let expected = [
            [2168, 2269, 2102, 2047, 2047, 2037, 2055],
            [2548, 2081, 2163, 2047, 2047, 1968, 2027],
            [2037, 1169, 2142, 2047, 2047, 2172, 2085],
            [1400, 1794, 2136, 2047, 2047, 2153, 2076],
        ];
        for (i, (frame, want)) in scenario().iter().zip(expected).enumerate() {
            let ctx = idle_ctx(&run.settings, &run.tune, Listener::Game, run.game);
            let got = step(&mut cueing, frame, &ctx, t0 + Duration::from_millis(33 * i as u64)).unwrap().unwrap();
            assert_eq!(got, padded(&want, 2047), "frame {i}");
        }
        // Case A: a 6-row percentages file is padded with 7-column rows
        // and the other two tables are the built-in 7-column defaults: slot 0 reads its 8 shares, then
        // fails at deadzone column 7 (index out of range) before recording any history, every frame.
        let six = table("motor,p,r,y,sw,su,h,tl,tlf", &[full; 6], full);
        let six: String = six.lines().take(7).map(|l| format!("{l}\n")).collect();
        let ext = ExternalMixing::from_files(None, None, Some((p, six.as_bytes()))).unwrap().unwrap();
        assert_eq!(ext.slot(6).percentages.len(), 7);
        let mut cueing = MotionCueing::with_rng(Some(external_p3(&ext)), ClipFactors::default(), Box::new(MaxRng));
        for (i, frame) in scenario().iter().take(2).enumerate() {
            let ctx = idle_ctx(&run.settings, &run.tune, Listener::Game, run.game);
            let got = step(&mut cueing, frame, &ctx, t0 + Duration::from_millis(33 * i as u64));
            assert_eq!(raised(got), ("list index out of range".to_owned(), Some(367)), "frame {i}");
        }
        assert!(cueing.state.history.of(0, Axis::Roll).is_empty());
        assert_eq!(cueing.state.previous_listener, None);
    }

    /// External tables of 12 rows each, `rows` first and `rest` after.
    fn ext_tables(direction: &[&str], deadzone: &[&str], percentages: &[&str]) -> ExternalMixing {
        let table = |rows: &[&str], rest: &str| {
            let mut text = String::from("motor,p,r,y,sw,su,h,tl,tlf\n");
            for i in 0..SLOTS {
                text.push_str(&format!("m{i},{}\n", rows.get(i).copied().unwrap_or(rest)));
            }
            text
        };
        let (dir, dz, pc) = (
            table(direction, "0,0,0,0,0,0,0,0"),
            table(deadzone, "0,0,0,0,0,0,0,0"),
            table(percentages, "100,100,100,100,100,100,100,100"),
        );
        let p = std::path::Path::new("x.csv");
        ExternalMixing::from_files(Some((p, dir.as_bytes())), Some((p, dz.as_bytes())), Some((p, pc.as_bytes())))
            .unwrap()
            .unwrap()
    }

    const EXT_DIRECTION: [&str; 7] = [
        "+,+,0,+,+,+,0,0",
        "+,-,0,-,+,+,0,0",
        "0,0,+,0,0,0,+,+",
        "0,0,0,0,0,0,0,0",
        "0,0,0,0,0,0,0,0",
        "-,0,0,0,0,+,0,0",
        "0,0,0,0,0,+,0,0",
    ];

    /// Streams the scenario frames through P3 with these external tables (enhanced g-forces off).
    fn ext_run(ext: &ExternalMixing, count: usize) -> (MotionCueing, Vec<Result<Option<Positions>, CueError>>) {
        let mut run = Run::new("dof-reality-p3", Manufacturer::DofReality);
        run.tune.enhanced_gforces = false;
        let mut cueing = MotionCueing::with_rng(Some(external_p3(ext)), ClipFactors::default(), Box::new(MaxRng));
        let t0 = Instant::now();
        let out = scenario()
            .iter()
            .take(count)
            .enumerate()
            .map(|(i, frame)| {
                let ctx = idle_ctx(&run.settings, &run.tune, Listener::Game, run.game);
                step(&mut cueing, frame, &ctx, t0 + Duration::from_millis(33 * i as u64))
            })
            .collect();
        (cueing, out)
    }

    #[test]
    fn send_stream_external_tables_with_unbounded_ints() {
        // Case B: a share of 2^53 + 1, a deadzone of 2^53 + 1001 and a
        // share of 1.2e29, all kept as exact integers.
        let ext = ext_tables(
            &EXT_DIRECTION,
            &["30,0,0,0,50,0,0,0", "9007199254741101,0,0,0,0,0,0,0"],
            &[
                "9007199254740993,80,100,60,100,100,100,100",
                "100,100,100,100,100,100,100,100",
                "100,100,50,100,100,100,100,123456789012345678901234567890",
            ],
        );
        let (_, out) = ext_run(&ext, 4);
        let expected = [
            [3891, 204, 2102, 2047, 2047, 2009, 2055, 2047, 2047, 2047, 2047, 2047],
            [3891, 204, 2163, 2047, 2047, 1880, 2027, 2047, 2047, 2047, 2047, 2047],
            [3891, 204, 3873, 2047, 2047, 2303, 2085, 2047, 2047, 2047, 2047, 2047],
            [3522, 572, 3891, 2047, 2047, 2267, 2076, 2047, 2047, 2047, 2047, 2047],
        ];
        for (i, (got, want)) in out.into_iter().zip(expected).enumerate() {
            assert_eq!(got, Ok(Some(want)), "frame {i}");
        }
    }

    #[test]
    fn send_stream_external_tables_overflow_and_type_errors() {
        let huge = "1".repeat(320);
        // Case C: a share whose quotient by 100 overflows: slot 0 fails at its weights.
        let share = format!("{huge},100,100,100,100,100,100,100");
        let (c, mut out) =
            ext_run(&ext_tables(&EXT_DIRECTION, &["30,0,0,0,50,0,0,0", "0,0,0,0,0,0,0,0"], &[&share]), 1);
        assert_eq!(raised(out.remove(0)), ("integer division result too large for a float".to_owned(), Some(232)));
        assert!(c.state.history.of(0, Axis::Pitch).is_empty());
        // Case D: slot 1's roll deadzone caps roll at 100 - 11…1, which does not convert to a float.
        let dz = format!("{huge},0,0,0,0,0,0,0");
        let (c, mut out) = ext_run(&ext_tables(&EXT_DIRECTION, &["0,0,0,0,0,0,0,0", &dz], &[]), 1);
        assert_eq!(raised(out.remove(0)), ("int too large to convert to float".to_owned(), Some(377)));
        assert_eq!(c.state.history.of(0, Axis::Pitch).len(), 1);
        assert!(c.state.history.of(1, Axis::Pitch).is_empty());
        // Case E: an invalid direction on slot 2 fails with a type error in its signal sum, after slots 0 and 1
        // recorded their history and before slot 2 does.
        let (c, mut out) =
            ext_run(&ext_tables(&["+,+,0,+,+,+,0,0", "+,-,0,-,+,+,0,0", "0,0,+,x,0,0,+,+"], &[], &[]), 1);
        assert_eq!(
            raised(out.remove(0)),
            ("unsupported operand type(s) for +: 'float' and 'NoneType'".to_owned(), Some(449))
        );
        assert_eq!(c.state.history.of(1, Axis::Pitch).len(), 1);
        assert!(c.state.history.of(2, Axis::Pitch).is_empty());
    }

    #[test]
    fn send_stream_chooses_each_window_before_its_moving_average() {
        // Case F2: roll window -5 (-5 / 5 truncated to -1 steps) and, from
        // frame 1, DAMS traction-loss auto smoothing with a maximum speed of 0. Frame 0 records history;
        // frame 1 fails in roll's moving average (division by zero) before traction loss's
        // auto smoothing is reached.
        let mut run = Run::new("dof-reality-p3", Manufacturer::DofReality);
        run.tune.effect_smoothing[Axis::Roll] = -5.0;
        let mut cueing = run.cueing();
        let t0 = Instant::now();
        let frames = scenario();
        let ctx = idle_ctx(&run.settings, &run.tune, Listener::Game, run.game);
        assert_eq!(
            step(&mut cueing, &frames[0], &ctx, t0),
            Ok(Some(padded(&[2230, 2269, 2112, 2047, 2047, 2047, 2055], 2047)))
        );
        run.settings.traction_loss_auto_smoothing.mode = 1;
        run.settings.traction_loss_auto_smoothing.max_speed_kmh = 0.0;
        for (i, frame) in frames.iter().enumerate().skip(1).take(2) {
            let ctx = idle_ctx(&run.settings, &run.tune, Listener::Game, run.game);
            let err = step(&mut cueing, frame, &ctx, t0 + Duration::from_millis(33 * i as u64)).unwrap_err();
            assert_eq!(err.to_string(), "float division by zero", "frame {i}");
            let frames: Vec<_> = err.frames().map(|f| (f.function, f.line)).collect();
            assert_eq!(
                frames,
                [("send_stream", 839), ("calculate_motor_power", 445), ("calculate_moving_average", 144)]
            );
        }
    }

    #[test]
    fn send_stream_fails_for_a_model_missing_from_motion_models() {
        // Case G: MOTIONBOX.model 'STR-SCN6150' with the STR models not
        // registered. Each frame passes the gate, advances the movement and gear state, then fails
        // with a missing-key error; the listener is not recorded and no history is written.
        let mut c =
            MotionCueing::with_rng(Some(Platform::unlisted("STR-SCN6150")), ClipFactors::default(), Box::new(MaxRng));
        let (s, t) = (MotionSettings::default(), MotionTune::default());
        let ctx = idle_ctx(&s, &t, Listener::Game, "dirtrally2");
        let t0 = Instant::now();
        for (i, frame) in scenario().iter().take(2).enumerate() {
            let got = step(&mut c, frame, &ctx, t0 + Duration::from_millis(33 * i as u64));
            assert_eq!(raised(got), ("'STR-SCN6150'".to_owned(), Some(827)), "frame {i}");
        }
        assert_eq!(c.state.previous_listener, None);
        assert!(c.state.previous_gear.equals(&PyValue::Float(3.0)));
        assert!((c.state.previous_longitudinal.as_double().unwrap() - -0.635).abs() < 1e-12);
        assert_eq!(c.state.previous_movement, Some(PyValue::Float(3.9)));
        assert_eq!(c.state.soft_start, SoftStart::Idle);
        assert!(c.state.history.of(0, Axis::Roll).is_empty());
    }

    #[test]
    fn native_movement_additions_raise_before_joy_overrides_gear_and_model_lookup() {
        use openrigcue_model::native_value::LegacyValue;
        let (settings, tune) = (MotionSettings::default(), MotionTune::default());
        for game in ["dirtrally2", "joy"] {
            for (values, message) in [
                (
                    [LegacyValue::Str("1".into()), LegacyValue::Int(0), LegacyValue::Int(0)],
                    "can only concatenate str (not \"int\") to str",
                ),
                (
                    [LegacyValue::Int(1), LegacyValue::Str("2".into()), LegacyValue::Int(0)],
                    "unsupported operand type(s) for +: 'int' and 'str'",
                ),
                (
                    [LegacyValue::Int(1), LegacyValue::Int(2), LegacyValue::None],
                    "unsupported operand type(s) for +: 'int' and 'NoneType'",
                ),
            ] {
                let mut cueing = MotionCueing::with_rng(
                    Some(Platform::unlisted("unlisted")),
                    ClipFactors::default(),
                    Box::new(MaxRng),
                );
                let mut telemetry = Telemetry::off();
                for (name, value) in ["yaw", "roll", "pitch"].into_iter().zip(values) {
                    telemetry.set_native(name, value);
                }
                let mut sink = Sink::delivering();
                let error = cueing
                    .send_stream(
                        &telemetry,
                        &idle_ctx(&settings, &tune, Listener::Game, game),
                        &mut sink,
                        Instant::now(),
                    )
                    .unwrap_err();
                assert_eq!(error.to_string(), message);
                assert!(error.traceback().contains("line 764"));
                assert_eq!(cueing.state.previous_movement, None);
                assert_eq!(cueing.state.previous_gear, PyValue::Int(-100));
                assert_eq!(cueing.state.previous_listener, None);
                assert!(sink.calls.is_empty());
            }
        }
        for (values, expected) in [
            ([LegacyValue::Int(1), LegacyValue::Int(2), LegacyValue::Int(3)], PyValue::Int(6)),
            (
                [LegacyValue::Str("1".into()), LegacyValue::Str("2".into()), LegacyValue::Str("3".into())],
                PyValue::Str("123".into()),
            ),
        ] {
            let mut cueing =
                MotionCueing::with_rng(Some(Platform::unlisted("unlisted")), ClipFactors::default(), Box::new(MaxRng));
            let mut telemetry = Telemetry::off();
            for (name, value) in ["yaw", "roll", "pitch"].into_iter().zip(values) {
                telemetry.set_native(name, value);
            }
            let error = cueing
                .compute(&telemetry, &idle_ctx(&settings, &tune, Listener::Game, "dirtrally2"), Instant::now())
                .unwrap_err();
            assert_eq!(error.to_string(), "'unlisted'");
            assert_eq!(cueing.state.previous_movement, Some(expected));
        }
    }

    #[test]
    fn send_stream_longitudinal_follower_state() {
        let platform = builtin("dof-reality-p3").unwrap().unwrap();
        let mut c = MotionCueing::with_rng(Some(platform), ClipFactors::default(), Box::new(MaxRng));
        let (s, t) = (MotionSettings::default(), MotionTune::default());
        let now = Instant::now();
        let ctx = idle_ctx(&s, &t, Listener::Game, "dirtrally2");
        step(&mut c, &telemetry(&[("longitudinal_acceleration", 3.0), ("roll", 1.0)]), &ctx, now).unwrap();
        // |3 - 0| > 1: 0 + 3 · 5 % = 0.15 is what the frame (and the next comparison) uses.
        assert!((c.state.previous_longitudinal.as_double().unwrap() - 0.15).abs() < 1e-12);
        let mut other = ctx;
        other.game_kind = GameKind::Other;
        step(&mut c, &telemetry(&[("longitudinal_acceleration", 3.0), ("roll", 1.0)]), &other, now).unwrap();
        assert_eq!(c.state.previous_longitudinal, PyValue::Float(3.0));
    }

    #[test]
    fn caller_nan_preparation_clears_values_before_send_stream() {
        let platform = builtin("dof-reality-p3").unwrap().unwrap();
        let (s, t) = (MotionSettings::default(), MotionTune::default());
        let ctx = idle_ctx(&s, &t, Listener::Game, "dirtrally2");
        let now = Instant::now();
        let mut a = MotionCueing::with_rng(Some(platform.clone()), ClipFactors::default(), Box::new(MaxRng));
        let mut b = MotionCueing::with_rng(Some(platform), ClipFactors::default(), Box::new(MaxRng));
        let mut nan = scenario()[0].clone();
        nan.yaw = f64::NAN;
        crate::clear_telemetry_nan_checked(&mut nan).unwrap();
        let mut zero = scenario()[0].clone();
        zero.yaw = 0.0;
        assert_eq!(step(&mut a, &nan, &ctx, now), step(&mut b, &zero, &ctx, now));
    }

    #[test]
    fn send_stream_reads_raw_values_where_the_original_does() {
        use crate::views::testing::{raw_config, raw_game, set_game};
        use crate::views::{RawConfig, RawGameConfig};
        let run = Run::new("dof-reality-p3", Manufacturer::DofReality);
        let now = Instant::now();
        let frame = &scenario()[0];
        let raw_ctx = |config: &'_ RawConfig, game: &'_ RawGameConfig| {
            let (config, game) = (config.clone(), game.clone());
            move |c: &mut MotionCueing, t: &Telemetry, now: Instant| {
                step(c, t, &idle_ctx(&config, &game, Listener::Game, "dirtrally2"), now)
            }
        };
        let config = raw_config(&run.settings, "DOF REALITY");
        let game = raw_game(&run.tune);

        // The raw copies give the expected frame.
        let mut c = run.cueing();
        let got = raw_ctx(&config, &game)(&mut c, frame, now);
        assert_eq!(got, Ok(Some(padded(&[2230, 2269, 2112, 2047, 2047, 2047, 2055], 2047))));

        // An unknown word in a boolean setting fails in the boolean parser.
        let mut bad = config.clone();
        bad.set("CUSTOM_FEATURES", "enable_legacy_gear_smoothing", "maybe");
        let err = raw_ctx(&bad, &game)(&mut run.cueing(), frame, now).unwrap_err();
        assert_eq!(err.to_string(), "Boolean value expected. Got maybe");
        let frames: Vec<_> = err.frames().map(|f| (f.function, f.line)).collect();
        assert_eq!(frames, [("send_stream", 785), ("str2bool", 261)]);

        // A non-integer gear_smoothing_min_delta fails the integer parse, every frame.
        let mut bad = config.clone();
        bad.set("MOTIONBOX", "gear_smoothing_min_delta", "1.5");
        assert_eq!(
            raised(raw_ctx(&bad, &game)(&mut run.cueing(), frame, now)),
            ("invalid literal for int() with base 10: '1.5'".to_owned(), Some(808))
        );

        // Max-power scaling: the game setting motion_max_horizontal is not read on a platform without
        // main actuators; a missing motion_max_power fails with a missing-key error after the 12 slots' history.
        let mut g = game.clone();
        set_game(&mut g, &["motion_max_horizontal"], None);
        assert!(raw_ctx(&config, &g)(&mut run.cueing(), frame, now).is_ok());
        let mut g = game.clone();
        set_game(&mut g, &["motion_max_power"], None);
        let mut c = run.cueing();
        let err = raw_ctx(&config, &g)(&mut c, frame, now).unwrap_err();
        let frames: Vec<_> = err.frames().map(|f| (f.function, f.line)).collect();
        assert_eq!(frames, [("send_stream", 854), ("apply_max_power", 477)]);
        assert_eq!(c.state.history.of(11, Axis::Pitch).len(), 1);

        // A missing smoothing / boost factor falls back to 50 / 35 (logged on every call).
        let mut g = game.clone();
        set_game(&mut g, &["motion_smoothing_factor"], None);
        set_game(&mut g, &["motion_boost_factor"], None);
        let mut fallback = run.tune.clone();
        fallback.smoothing_factor = 50.0;
        fallback.boost_factor = 35.0;
        let mut a = run.cueing();
        let mut b = run.cueing();
        let raw = raw_ctx(&config, &g);
        for (i, t) in scenario().iter().enumerate() {
            let now = now + Duration::from_millis(33 * i as u64);
            let x = raw(&mut a, t, now);
            let y = step(&mut b, t, &idle_ctx(&run.settings, &fallback, Listener::Game, "dirtrally2"), now);
            assert_eq!(x, y, "frame {i}");
        }

        // soft_crash_enabled is compared with 'yes' as a string by the rotary shaping.
        let mut on = config.clone();
        on.set("MOTIONBOX", "soft_crash_enabled", "Yes");
        let typed_off = MotionSettings { soft_crash_enabled: false, ..run.settings.clone() };
        let mut a = run.cueing();
        let mut b = run.cueing();
        let raw = raw_ctx(&on, &game);
        for (i, t) in scenario().iter().enumerate() {
            let now = now + Duration::from_millis(33 * i as u64);
            let x = raw(&mut a, t, now);
            let y = step(&mut b, t, &idle_ctx(&typed_off, &run.tune, Listener::Game, "dirtrally2"), now);
            assert_eq!(x, y, "frame {i}");
        }

        // The nine effect sliders are computed before any effect runs: a missing switch of the last
        // one fails even when no effect fires.
        let mut g = game.clone();
        set_game(&mut g, &["motion_switch", "abs_lock"], None);
        assert_eq!(raised(raw_ctx(&config, &g)(&mut run.cueing(), frame, now)), ("'abs_lock'".to_owned(), Some(914)));
    }
}
