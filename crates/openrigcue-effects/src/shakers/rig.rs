//! The per-frame shaker effect engine: telemetry in, sample starts/stops and engine-sound
//! parameters out.

use std::collections::HashSet;

use openrigcue_model::native_value::LegacyValue;
use openrigcue_model::Telemetry;

use super::layout::{Effect, Group, PerGroup, PerSpeaker, GROUP_COUNT};
use super::levels::{engine_fade, native_engine_levels, GroupLevels};
use super::samples::{SampleFolder, SampleKey};
use super::suspension::{max_velocity, road_bump_sample_checked, suspension_sample_checked, AxleTravel};
use super::tables::{self, lookup, map_clamped, map_upper_clamped, py_round};
use super::tuning::{DspError, EngineType, NativeDspSettings, NativeEngineSelector, ShakeboxConfig, ShakerTuning};

/// One value per wheel.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct WheelValues {
    pub lf: f64,
    pub lr: f64,
    pub rf: f64,
    pub rr: f64,
}

/// The telemetry the shaker engine reads, in the shared telemetry units.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ShakeInput {
    pub speed_kmh: f64,
    pub rpm: f64,
    pub max_rpm: f64,
    /// Harmonized gear.
    pub gear: f64,
    pub suspension_travel: WheelValues,
    pub suspension_velocity: WheelValues,
    pub wheel_slip: WheelValues,
    pub rumble_pitch: WheelValues,
    pub lateral_acceleration: f64,
    pub vertical_acceleration: f64,
    pub longitudinal_acceleration: f64,
    pub lateral_velocity: f64,
    pub canopy: f64,
    pub afterburner: f64,
    pub landing_gear_left: f64,
    pub landing_gear_nose: f64,
    pub landing_gear_right: f64,
    pub cannon_firing: f64,
    pub missile_firing: f64,
    pub abs_lock: f64,
    pub touchdown_left: f64,
    pub touchdown_nose: f64,
    pub touchdown_right: f64,
    pub turbulence: f64,
}

impl ShakeInput {
    /// The channels the shaker loop passes to the rig, with NaN read as 0.
    ///
    /// The motion pass replaces every NaN of the shared telemetry by 0, so
    /// the rig sees 0 where a game wrote NaN.
    pub fn from_telemetry(t: &Telemetry) -> ShakeInput {
        ShakeInput {
            speed_kmh: zero_nan(t.speed_kmh),
            rpm: zero_nan(t.rpm),
            max_rpm: zero_nan(t.max_rpm),
            gear: zero_nan(t.harmonized_gear),
            suspension_travel: WheelValues {
                lf: zero_nan(t.suspension_travel_lf),
                lr: zero_nan(t.suspension_travel_lr),
                rf: zero_nan(t.suspension_travel_rf),
                rr: zero_nan(t.suspension_travel_rr),
            },
            suspension_velocity: WheelValues {
                lf: zero_nan(t.suspension_velocity_lf),
                lr: zero_nan(t.suspension_velocity_lr),
                rf: zero_nan(t.suspension_velocity_rf),
                rr: zero_nan(t.suspension_velocity_rr),
            },
            wheel_slip: WheelValues {
                lf: zero_nan(t.wheel_slip_lf),
                lr: zero_nan(t.wheel_slip_lr),
                rf: zero_nan(t.wheel_slip_rf),
                rr: zero_nan(t.wheel_slip_rr),
            },
            rumble_pitch: WheelValues {
                lf: zero_nan(t.rumble_pitch_lf),
                lr: zero_nan(t.rumble_pitch_lr),
                rf: zero_nan(t.rumble_pitch_rf),
                rr: zero_nan(t.rumble_pitch_rr),
            },
            lateral_acceleration: zero_nan(t.lateral_acceleration),
            vertical_acceleration: zero_nan(t.vertical_acceleration),
            longitudinal_acceleration: zero_nan(t.longitudinal_acceleration),
            lateral_velocity: zero_nan(t.lateral_velocity),
            canopy: zero_nan(t.canopy),
            afterburner: zero_nan(t.afterburner),
            landing_gear_left: zero_nan(t.landing_gear_left),
            landing_gear_nose: zero_nan(t.landing_gear_nose),
            landing_gear_right: zero_nan(t.landing_gear_right),
            cannon_firing: zero_nan(t.cannon_firing),
            missile_firing: zero_nan(t.missile_firing),
            abs_lock: zero_nan(t.abs_lock),
            touchdown_left: zero_nan(t.touchdown_left),
            touchdown_nose: zero_nan(t.touchdown_nose),
            touchdown_right: zero_nan(t.touchdown_right),
            turbulence: zero_nan(t.turbulence),
        }
    }
}

/// 0 for NaN, `v` otherwise.
fn zero_nan(v: f64) -> f64 {
    if v.is_nan() {
        0.0
    } else {
        v
    }
}

/// A playback slot. Starting a slot replaces what it plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Voice {
    Transmission,
    LandingGearLeft,
    LandingGearNose,
    LandingGearRight,
    Turbulence,
    TouchdownLeft,
    TouchdownNose,
    TouchdownRight,
    Missile,
    AbsLock,
    Cannon,
    Canopy,
    Afterburner,
    Impact,
    Lateral,
    TractionLoss,
    Brake,
    Accel,
    WheelSlipLf,
    WheelSlipRf,
    WheelSlipLr,
    WheelSlipRr,
    SuspensionLf,
    SuspensionRf,
    SuspensionLr,
    SuspensionRr,
    RoadBump,
    /// The speaker-test tone.
    SpeakerTest,
    /// Internal key for another playing channel of a replaced sound.
    Detached(u64),
}

impl Voice {
    /// Every voice the inactive branch stops.
    pub const RIG: [Voice; 27] = [
        Voice::RoadBump,
        Voice::Transmission,
        Voice::Canopy,
        Voice::Cannon,
        Voice::AbsLock,
        Voice::Missile,
        Voice::Afterburner,
        Voice::Turbulence,
        Voice::LandingGearLeft,
        Voice::LandingGearNose,
        Voice::LandingGearRight,
        Voice::TouchdownLeft,
        Voice::TouchdownNose,
        Voice::TouchdownRight,
        Voice::SuspensionLf,
        Voice::SuspensionLr,
        Voice::SuspensionRf,
        Voice::SuspensionRr,
        Voice::WheelSlipLf,
        Voice::WheelSlipLr,
        Voice::WheelSlipRf,
        Voice::WheelSlipRr,
        Voice::Impact,
        Voice::Accel,
        Voice::Brake,
        Voice::Lateral,
        Voice::TractionLoss,
    ];
}

/// How many times a started sample plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plays {
    /// A plain sound: one pass.
    Once,
    /// A loop count of `n`: `n + 1` passes.
    Repeat(u32),
    /// A looping sound with loop count -1 (set explicitly, or by default).
    Forever,
}

/// The effect low-pass (a multiband EQ whose filter A is a 12 dB low-pass).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LowPass {
    pub cutoff_hz: f64,
    /// `shaker_resonance`, used as the filter's Q.
    pub q: f64,
}

impl Default for LowPass {
    fn default() -> Self {
        // The multiband EQ's default frequency and Q. A failed
        // first setter leaves the later parameter at its default.
        Self { cutoff_hz: 8_000.0, q: f64::from(f32::from_bits(0x3f34_fdf4)) }
    }
}

/// Every active frame sets the filter type, frequency and Q, including when the
/// filter is disabled. Keep the float arguments until their individual calls.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct FilterSettings {
    pub low_pass: bool,
    pub parameters: LowPass,
}

/// Deferred results fail differently: a speaker-level failure is handled, a pitch failure ends the frame.
#[derive(Debug, Clone, PartialEq)]
pub struct NativeEngineParameters {
    pub levels: Result<PerSpeaker, DspError>,
    pub pitch: Result<f64, DspError>,
}

/// Engine-sound parameters (also the FMOD Studio event parameters).
#[derive(Debug, Clone, PartialEq)]
pub struct EngineCommand {
    /// The instance that plays; the other one is paused. `None`: `shaker_engine_type` names no
    /// instance; the engine block pauses 'O' and then fails on the
    /// instance lookup.
    pub engine_type: Option<EngineType>,
    pub native_engine_selector: Option<NativeEngineSelector>,
    pub native_parameters: Option<Box<NativeEngineParameters>>,
    /// `rpm` parameter, 0..10000 (rpm normalised to max rpm).
    pub rpm: f64,
    /// `load` parameter (always 1).
    pub load: f64,
    /// `fl, fr, ce, sub, sl, sr` parameters; `None` leaves the previous ones (the
    /// level computation failed).
    pub speaker_levels: Option<PerSpeaker>,
    /// `pitch` parameter.
    pub pitch: f64,
}

/// One playback instruction produced by a frame.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Release the voice (hard stop).
    Stop(Voice),
    /// Replace whatever the voice plays with `sample` through `group`.
    Start {
        voice: Voice,
        sample: SampleKey,
        plays: Plays,
        group: Group,
    },
    /// Ordered DSP settings per group, indexed by [`Group::index`].
    Filters(Box<PerGroup<FilterSettings>>),
    NativeFilters(Box<PerGroup<NativeDspSettings>>),
    /// One group in the live, ordered DSP loop.
    NativeGroupFilter {
        group: Group,
        settings: NativeDspSettings,
    },
    /// One DSP operation, with its argument read at this stage.
    NativeDspStep {
        group: Group,
        update: DspUpdate,
    },
    /// An arithmetic failure reached at this point; preceding actions have already run.
    TractionFailure(DspError),
    SuspensionFailure(DspError),
    /// Drive the engine sound.
    Engine(EngineCommand),
    /// Pause both engine instances.
    PauseEngines,
    PauseEngine(EngineType),
    /// Submit the frame's engine commands (the Studio update).
    EngineUpdate,
}

/// Whether a frame does anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RigStatus {
    /// No sound card selected or output not initialised: the frame is skipped entirely.
    Disabled,
    /// No game running or shaker switch off: everything stops.
    Idle,
    /// Effects run.
    Active,
}

impl RigStatus {
    /// The activation tests at the top of a frame.
    pub fn evaluate(output_ready: bool, game_running: bool, shakers_switch_on: bool) -> RigStatus {
        if !output_ready {
            RigStatus::Disabled
        } else if !game_running || !shakers_switch_on {
            RigStatus::Idle
        } else {
            RigStatus::Active
        }
    }
}

/// Everything a frame reads besides the telemetry.
#[derive(Debug, Clone, Copy)]
pub struct FrameContext<'a> {
    pub status: RigStatus,
    pub tuning: &'a ShakerTuning,
    pub config: &'a ShakeboxConfig,
    pub levels: &'a GroupLevels,
    /// The tune's traction-loss maximum (`max_telemetry_vars.traction_loss`).
    pub max_traction_loss: f64,
    /// Raw numerator for the max-field arithmetic; canned frames use their float.
    pub native_lateral_velocity: Option<&'a LegacyValue>,
}

/// The individual calls, in order, inside each channel-group DSP update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DspStage {
    Remove,
    Type,
    Frequency,
    Q,
    Attach,
}
impl DspStage {
    pub const ALL: [Self; 5] = [Self::Remove, Self::Type, Self::Frequency, Self::Q, Self::Attach];
}

#[derive(Debug, Clone, PartialEq)]
pub enum DspUpdate {
    Remove,
    Type(Result<i32, DspError>),
    Frequency(Result<f64, DspError>),
    Q(Result<f64, DspError>),
    Attach,
}

/// Remembered fields used by later calls, in their late assignment order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryField {
    Gear,
    Afterburner,
    TouchdownLeft,
    TouchdownNose,
    TouchdownRight,
    Suspension(usize),
    WheelSlip(usize),
    RoadBump,
    Travel(usize),
}
impl MemoryField {
    pub const ALL: [Self; 18] = [
        Self::Gear,
        Self::Afterburner,
        Self::TouchdownLeft,
        Self::TouchdownNose,
        Self::TouchdownRight,
        Self::Suspension(0),
        Self::Suspension(2),
        Self::Suspension(1),
        Self::Suspension(3),
        Self::WheelSlip(0),
        Self::WheelSlip(2),
        Self::WheelSlip(1),
        Self::WheelSlip(3),
        Self::RoadBump,
        Self::Travel(0),
        Self::Travel(2),
        Self::Travel(1),
        Self::Travel(3),
    ];
}

/// Points between effect operations where another application thread can run.
/// Pure callers may still use [`ShakeRig::frame`]; production advances these with live settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameStage {
    /// Read the effect branch.
    SoundPrepare(Voice),
    /// Release the captured sound.
    /// Read the held sound again to capture the one to release after the branch.
    SoundReleaseLookup,
    SoundRelease,
    /// Clear the held sound after a successful release.
    SoundClear,
    /// Create a sound and hold it; no channel plays yet.
    SoundCreate,
    /// Apply an explicit loop count if this effect branch has one.
    SoundLoops,
    FilterGroup(Group, DspStage),
    Gear,
    LandingGearRight,
    Turbulence,
    LandingGearLeft,
    LandingGearNose,
    TouchdownRight,
    TouchdownLeft,
    TouchdownNose,
    Missile,
    AbsLock,
    Cannon,
    Canopy,
    Afterburner,
    Impact,
    Lateral,
    Traction,
    Brake,
    Acceleration,
    WheelSlip(usize),
    Suspension(usize),
    RoadBump,
    EngineBegin,
    EngineSelect,
    EngineCheckPaused,
    EngineResume,
    EngineRpm,
    EngineLoad,
    EngineSpeaker(usize),
    EnginePitch,
    /// The Studio update after the engine block or after the inactive branch's
    /// pauses.
    EngineUpdate,
    Commit(MemoryField),
    IdlePause(EngineType),
    IdleStop(Voice),
    IdleReset(usize),
}

impl FrameStage {
    pub(super) fn explicit_loop_count(voice: Voice, plays: Plays) -> bool {
        match plays {
            Plays::Once => false,
            Plays::Repeat(_) => true,
            Plays::Forever => !matches!(
                voice,
                Voice::WheelSlipLf
                    | Voice::WheelSlipRf
                    | Voice::WheelSlipLr
                    | Voice::WheelSlipRr
                    | Voice::SuspensionLf
                    | Voice::SuspensionRf
                    | Voice::SuspensionLr
                    | Voice::SuspensionRr
                    | Voice::RoadBump
            ),
        }
    }

    pub(super) fn sound_stage(voice: Voice, idle: bool) -> Self {
        use FrameStage::*;
        if idle {
            return IdleStop(voice);
        }
        match voice {
            Voice::Transmission => Gear,
            Voice::LandingGearRight => LandingGearRight,
            Voice::Turbulence => Turbulence,
            Voice::LandingGearLeft => LandingGearLeft,
            Voice::LandingGearNose => LandingGearNose,
            Voice::TouchdownRight => TouchdownRight,
            Voice::TouchdownLeft => TouchdownLeft,
            Voice::TouchdownNose => TouchdownNose,
            Voice::Missile => Missile,
            Voice::AbsLock => AbsLock,
            Voice::Cannon => Cannon,
            Voice::Canopy => Canopy,
            Voice::Afterburner => Afterburner,
            Voice::Impact => Impact,
            Voice::Lateral => Lateral,
            Voice::TractionLoss => Traction,
            Voice::Brake => Brake,
            Voice::Accel => Acceleration,
            Voice::WheelSlipLf => WheelSlip(0),
            Voice::WheelSlipRf => WheelSlip(1),
            Voice::WheelSlipLr => WheelSlip(2),
            Voice::WheelSlipRr => WheelSlip(3),
            Voice::SuspensionLf => Suspension(0),
            Voice::SuspensionRf => Suspension(1),
            Voice::SuspensionLr => Suspension(2),
            Voice::SuspensionRr => Suspension(3),
            Voice::RoadBump => RoadBump,
            _ => unreachable!("only rig Sound slots have stages"),
        }
    }
    pub(super) fn sound_voice(self) -> Option<Voice> {
        use FrameStage::*;
        Some(match self {
            Gear => Voice::Transmission,
            LandingGearRight => Voice::LandingGearRight,
            Turbulence => Voice::Turbulence,
            LandingGearLeft => Voice::LandingGearLeft,
            LandingGearNose => Voice::LandingGearNose,
            TouchdownRight => Voice::TouchdownRight,
            TouchdownLeft => Voice::TouchdownLeft,
            TouchdownNose => Voice::TouchdownNose,
            Missile => Voice::Missile,
            AbsLock => Voice::AbsLock,
            Cannon => Voice::Cannon,
            Canopy => Voice::Canopy,
            Afterburner => Voice::Afterburner,
            Impact => Voice::Impact,
            Lateral => Voice::Lateral,
            Traction => Voice::TractionLoss,
            Brake => Voice::Brake,
            Acceleration => Voice::Accel,
            WheelSlip(k) => [Voice::WheelSlipLf, Voice::WheelSlipRf, Voice::WheelSlipLr, Voice::WheelSlipRr][k],
            Suspension(k) => [Voice::SuspensionLf, Voice::SuspensionRf, Voice::SuspensionLr, Voice::SuspensionRr][k],
            RoadBump => Voice::RoadBump,
            IdleStop(voice) => voice,
            _ => return None,
        })
    }
}

impl FrameStage {
    pub fn for_status(status: RigStatus) -> Vec<Self> {
        use FrameStage::*;
        match status {
            RigStatus::Disabled => Vec::new(),
            RigStatus::Idle => [IdlePause(EngineType::Original), IdlePause(EngineType::Enhanced), EngineUpdate]
                .into_iter()
                .chain(Voice::RIG.into_iter().map(IdleStop))
                .chain([0, 2, 1, 3].map(IdleReset))
                .collect(),
            RigStatus::Active => Group::ALL
                .into_iter()
                .flat_map(|group| DspStage::ALL.map(|step| FilterGroup(group, step)))
                .chain([
                    Gear,
                    LandingGearRight,
                    Turbulence,
                    LandingGearLeft,
                    LandingGearNose,
                    TouchdownRight,
                    TouchdownLeft,
                    TouchdownNose,
                    Missile,
                    AbsLock,
                    Cannon,
                    Canopy,
                    Afterburner,
                    Impact,
                    Lateral,
                    Traction,
                    Brake,
                    Acceleration,
                    WheelSlip(0),
                    WheelSlip(1),
                    WheelSlip(2),
                    WheelSlip(3),
                    Suspension(0),
                    Suspension(1),
                    Suspension(2),
                    Suspension(3),
                    RoadBump,
                    EngineBegin,
                    EngineSelect,
                    EngineCheckPaused,
                    EngineResume,
                    EngineRpm,
                    EngineLoad,
                    EngineSpeaker(0),
                    EngineSpeaker(1),
                    EngineSpeaker(2),
                    EngineSpeaker(3),
                    EngineSpeaker(4),
                    EngineSpeaker(5),
                    EnginePitch,
                    EngineUpdate,
                ])
                .chain(MemoryField::ALL.map(Commit))
                .collect(),
        }
    }
}

/// Per-call results remain local until the late previous-value assignments.
#[derive(Debug)]
pub struct LiveFrame {
    pub status: RigStatus,
    wheel_slip: [Option<i64>; 4],
    suspension: [Option<i64>; 4],
    road_bump: Option<i64>,
}
impl LiveFrame {
    pub fn new(status: RigStatus) -> Self {
        Self { status, wheel_slip: [None; 4], suspension: [None; 4], road_bump: None }
    }
}

/// Values remembered from the previous frame.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Previous {
    gear: f64,
    afterburner: f64,
    touchdown_left: f64,
    touchdown_nose: f64,
    touchdown_right: f64,
    wheel_slip_file: [i64; 4],
    suspension_file: [i64; 4],
    road_bump_file: i64,
    suspension_travel: WheelValues,
}

/// The previous gear before the first frame: -10;
/// every other previous value starts at 0.
const INITIAL_PREVIOUS_GEAR: f64 = -10.0;

impl Default for Previous {
    /// The previous values before the first frame.
    fn default() -> Self {
        Previous {
            gear: INITIAL_PREVIOUS_GEAR,
            afterburner: 0.0,
            touchdown_left: 0.0,
            touchdown_nose: 0.0,
            touchdown_right: 0.0,
            wheel_slip_file: [0; 4],
            suspension_file: [0; 4],
            road_bump_file: 0,
            suspension_travel: WheelValues::default(),
        }
    }
}

fn get_wheel(value: &WheelValues, k: usize) -> f64 {
    match k {
        0 => value.lf,
        1 => value.rf,
        2 => value.lr,
        3 => value.rr,
        _ => unreachable!("known corner"),
    }
}
fn set_wheel(value: &mut WheelValues, k: usize, next: f64) {
    match k {
        0 => value.lf = next,
        1 => value.rf = next,
        2 => value.lr = next,
        3 => value.rr = next,
        _ => unreachable!("known corner"),
    }
}

/// The shaker effect engine with its frame-to-frame state.
#[derive(Debug, Default, Clone)]
pub struct ShakeRig {
    previous: Previous,
    /// Voices whose sample object exists.
    held: HashSet<Voice>,
}

impl ShakeRig {
    pub(super) fn set_held(&mut self, voice: Voice, held: bool) {
        if held {
            self.held.insert(voice);
        } else {
            self.held.remove(&voice);
        }
    }

    pub(super) fn prepare_live_actions(
        &mut self,
        input: &ShakeInput,
        ctx: &FrameContext,
        frame: &mut LiveFrame,
        stage: FrameStage,
    ) -> Vec<Action> {
        let held = self.held.clone();
        let actions = self.advance_live_frame(input, ctx, frame, stage);
        self.held = held;
        actions
    }

    pub fn new() -> ShakeRig {
        ShakeRig::default()
    }

    /// Run one frame.
    pub fn frame(&mut self, input: &ShakeInput, ctx: &FrameContext) -> Vec<Action> {
        let mut out = Vec::new();
        match ctx.status {
            RigStatus::Disabled => {}
            RigStatus::Idle => self.idle(&mut out),
            RigStatus::Active => self.active(input, ctx, &mut out),
        }
        out
    }

    /// Run one stage against the settings currently visible to this call.
    /// Previous-file results commit at the end; held sample objects change with each operation.
    pub fn advance_live_frame(
        &mut self,
        i: &ShakeInput,
        ctx: &FrameContext,
        frame: &mut LiveFrame,
        stage: FrameStage,
    ) -> Vec<Action> {
        use FrameStage::*;
        let mut out = Vec::new();
        let error = match stage {
            SoundPrepare(_) | SoundReleaseLookup | SoundRelease | SoundClear | SoundCreate | SoundLoops => Ok(()),
            FilterGroup(group, stage) => {
                let native = ctx.tuning.native_dsp.as_ref().map(|settings| &settings[group.index()]);
                let typed = ctx.tuning.effect(group.effect());
                let update = match stage {
                    DspStage::Remove => DspUpdate::Remove,
                    DspStage::Type => {
                        DspUpdate::Type(native.map_or_else(|| Ok(i32::from(typed.low_pass)), |s| s.filter_type.clone()))
                    }
                    DspStage::Frequency => {
                        DspUpdate::Frequency(native.map_or_else(|| Ok(typed.cutoff_hz), |s| s.cutoff_hz.clone()))
                    }
                    DspStage::Q => DspUpdate::Q(native.map_or_else(|| Ok(typed.resonance), |s| s.q.clone())),
                    DspStage::Attach => DspUpdate::Attach,
                };
                out.push(Action::NativeDspStep { group, update });
                Ok(())
            }
            Gear => {
                self.gear(i, &mut out);
                Ok(())
            }
            LandingGearRight => {
                self.travel_range(
                    i.landing_gear_right,
                    Voice::LandingGearRight,
                    Group::LandingGearRight,
                    SampleFolder::LandingGear,
                    &mut out,
                );
                Ok(())
            }
            Turbulence => {
                if i.turbulence < 1.0 {
                    self.stop(Voice::Turbulence, &mut out)
                } else {
                    self.hold(
                        Voice::Turbulence,
                        SampleKey::new(SampleFolder::Turbulence, 0),
                        Group::Turbulence,
                        &mut out,
                    )
                };
                Ok(())
            }
            LandingGearLeft => {
                self.travel_range(
                    i.landing_gear_left,
                    Voice::LandingGearLeft,
                    Group::LandingGearLeft,
                    SampleFolder::LandingGear,
                    &mut out,
                );
                Ok(())
            }
            LandingGearNose => {
                self.travel_range(
                    i.landing_gear_nose,
                    Voice::LandingGearNose,
                    Group::LandingGearNose,
                    SampleFolder::LandingGear,
                    &mut out,
                );
                Ok(())
            }
            TouchdownRight => {
                self.touchdown(
                    i.touchdown_right,
                    self.previous.touchdown_right,
                    Voice::TouchdownRight,
                    Group::TouchdownRight,
                    &mut out,
                );
                Ok(())
            }
            TouchdownLeft => {
                self.touchdown(
                    i.touchdown_left,
                    self.previous.touchdown_left,
                    Voice::TouchdownLeft,
                    Group::TouchdownLeft,
                    &mut out,
                );
                Ok(())
            }
            TouchdownNose => {
                self.touchdown(
                    i.touchdown_nose,
                    self.previous.touchdown_nose,
                    Voice::TouchdownNose,
                    Group::TouchdownNose,
                    &mut out,
                );
                Ok(())
            }
            Missile => {
                self.while_nonzero(
                    i.missile_firing,
                    Voice::Missile,
                    SampleKey::new(SampleFolder::CannonFiring, 0),
                    Group::MissileFiring,
                    &mut out,
                );
                Ok(())
            }
            AbsLock => {
                self.while_nonzero(
                    i.abs_lock,
                    Voice::AbsLock,
                    SampleKey::new(SampleFolder::Suspension, 5),
                    Group::AbsLock,
                    &mut out,
                );
                Ok(())
            }
            Cannon => {
                self.while_nonzero(
                    i.cannon_firing,
                    Voice::Cannon,
                    SampleKey::new(SampleFolder::MissileFiring, 0),
                    Group::CannonFiring,
                    &mut out,
                );
                Ok(())
            }
            Canopy => {
                self.travel_range(i.canopy, Voice::Canopy, Group::Canopy, SampleFolder::Canopy, &mut out);
                Ok(())
            }
            Afterburner => {
                self.afterburner(i.afterburner, &mut out);
                Ok(())
            }
            Impact => {
                self.pulse(
                    lookup(&tables::IMPACT, i.longitudinal_acceleration),
                    Voice::Impact,
                    Group::Impact,
                    &mut out,
                );
                Ok(())
            }
            Lateral => {
                let group = if i.lateral_acceleration < 0.0 { Group::LatLeft } else { Group::LatRight };
                self.pulse(lookup(&tables::LATERAL, i.lateral_acceleration.abs()), Voice::Lateral, group, &mut out);
                Ok(())
            }
            Traction => self.live_traction(i, ctx, &mut out),
            Brake => {
                self.pulse(
                    lookup(&tables::BRAKE, i.longitudinal_acceleration),
                    Voice::Brake,
                    Self::live_side_group(i, Group::BrakeRf, Group::BrakeLf, Group::Brake),
                    &mut out,
                );
                Ok(())
            }
            Acceleration => {
                self.pulse(
                    lookup(&tables::ACCEL, i.longitudinal_acceleration),
                    Voice::Accel,
                    Self::live_side_group(i, Group::AccelRr, Group::AccelLr, Group::Accel),
                    &mut out,
                );
                Ok(())
            }
            WheelSlip(k) => {
                let (value, voice, group) = [
                    (i.wheel_slip.lf, Voice::WheelSlipLf, Group::WheelSlipLf),
                    (i.wheel_slip.rf, Voice::WheelSlipRf, Group::WheelSlipRf),
                    (i.wheel_slip.lr, Voice::WheelSlipLr, Group::WheelSlipLr),
                    (i.wheel_slip.rr, Voice::WheelSlipRr, Group::WheelSlipRr),
                ][k];
                let file = lookup(&tables::WHEEL_SLIP, value);
                self.level_loop(file, self.previous.wheel_slip_file[k], voice, group, &mut out);
                frame.wheel_slip[k] = Some(file);
                Ok(())
            }
            Suspension(k) => self.live_suspension(i, ctx, k, &mut out).map(|file| frame.suspension[k] = Some(file)),
            RoadBump => road_bump_sample_checked(
                i.vertical_acceleration,
                max_velocity([
                    i.suspension_velocity.lf,
                    i.suspension_velocity.lr,
                    i.suspension_velocity.rf,
                    i.suspension_velocity.rr,
                ]),
                ctx.config,
                ctx.tuning.native_suspension.as_deref(),
            )
            .map(|file| {
                self.level_loop(file, self.previous.road_bump_file, Voice::RoadBump, Group::RoadBump, &mut out);
                frame.road_bump = Some(file)
            }),
            EngineBegin | EngineSelect | EngineCheckPaused | EngineResume | EngineRpm | EngineLoad
            | EngineSpeaker(_) | EnginePitch => Ok(()),
            EngineUpdate => {
                out.push(Action::EngineUpdate);
                Ok(())
            }
            Commit(field) => {
                match field {
                    MemoryField::Gear => self.previous.gear = i.gear,
                    MemoryField::Afterburner => self.previous.afterburner = i.afterburner,
                    MemoryField::TouchdownLeft => self.previous.touchdown_left = i.touchdown_left,
                    MemoryField::TouchdownNose => self.previous.touchdown_nose = i.touchdown_nose,
                    MemoryField::TouchdownRight => self.previous.touchdown_right = i.touchdown_right,
                    MemoryField::Suspension(k) => {
                        if let Some(value) = frame.suspension[k] {
                            self.previous.suspension_file[k] = value;
                        }
                    }
                    MemoryField::WheelSlip(k) => {
                        if let Some(value) = frame.wheel_slip[k] {
                            self.previous.wheel_slip_file[k] = value;
                        }
                    }
                    MemoryField::RoadBump => {
                        if let Some(value) = frame.road_bump {
                            self.previous.road_bump_file = value;
                        }
                    }
                    MemoryField::Travel(k) => {
                        set_wheel(&mut self.previous.suspension_travel, k, get_wheel(&i.suspension_travel, k))
                    }
                }
                Ok(())
            }
            IdlePause(instance) => {
                out.push(Action::PauseEngine(instance));
                Ok(())
            }
            IdleStop(voice) => {
                self.stop(voice, &mut out);
                Ok(())
            }
            IdleReset(k) => {
                set_wheel(&mut self.previous.suspension_travel, k, 0.0);
                Ok(())
            }
        };
        if let Err(error) = error {
            out.push(if stage == Traction { Action::TractionFailure(error) } else { Action::SuspensionFailure(error) });
        }
        out
    }

    fn live_side_group(i: &ShakeInput, right: Group, left: Group, both: Group) -> Group {
        if i.lateral_acceleration > 3.0 {
            right
        } else if i.lateral_acceleration < -3.0 {
            left
        } else {
            both
        }
    }

    fn live_traction(&mut self, i: &ShakeInput, ctx: &FrameContext, out: &mut Vec<Action>) -> Result<(), DspError> {
        let tl = match &ctx.tuning.native_traction_max {
            Some(maximum) => native_traction_loss_percent(
                ctx.native_lateral_velocity.unwrap_or(&LegacyValue::Float(i.lateral_velocity)),
                maximum.as_ref().map_err(Clone::clone)?,
            )?,
            None => traction_loss_percent(i.lateral_velocity, ctx.max_traction_loss),
        };
        self.pulse(
            lookup(&tables::TRACTION_LOSS_REAR, tl.abs()),
            Voice::TractionLoss,
            if tl < 0.0 { Group::TractionLossLeft } else { Group::TractionLossRight },
            out,
        );
        Ok(())
    }

    fn live_suspension(
        &mut self,
        i: &ShakeInput,
        ctx: &FrameContext,
        k: usize,
        out: &mut Vec<Action>,
    ) -> Result<i64, DspError> {
        let st = i.suspension_travel;
        let prev = self.previous.suspension_travel;
        let sv = i.suspension_velocity;
        let rp = i.rumble_pitch;
        let (axle, velocity, rumble, voice, group) = [
            (
                AxleTravel { current: st.lf, previous: prev.lf, side_current: st.rf, side_previous: prev.rf },
                sv.lf,
                rp.lf,
                Voice::SuspensionLf,
                Group::SuspensionLf,
            ),
            (
                AxleTravel { current: st.rf, previous: prev.rf, side_current: st.lf, side_previous: prev.lf },
                sv.rf,
                rp.rf,
                Voice::SuspensionRf,
                Group::SuspensionRf,
            ),
            (
                AxleTravel { current: st.lr, previous: prev.lr, side_current: st.rr, side_previous: prev.rr },
                sv.lr,
                rp.lr,
                Voice::SuspensionLr,
                Group::SuspensionLr,
            ),
            (
                AxleTravel { current: st.rr, previous: prev.rr, side_current: st.lr, side_previous: prev.lr },
                sv.rr,
                rp.rr,
                Voice::SuspensionRr,
                Group::SuspensionRr,
            ),
        ][k];
        let file = suspension_sample_checked(
            axle,
            velocity,
            rumble,
            i.rpm,
            i.speed_kmh,
            ctx.config,
            ctx.tuning.native_suspension.as_deref(),
        )?;
        self.level_loop(file, self.previous.suspension_file[k], voice, group, out);
        Ok(file)
    }

    fn stop(&mut self, voice: Voice, out: &mut Vec<Action>) {
        if self.held.remove(&voice) {
            out.push(Action::Stop(voice));
        }
    }

    fn start(&mut self, voice: Voice, sample: SampleKey, plays: Plays, group: Group, out: &mut Vec<Action>) {
        self.stop(voice, out);
        self.held.insert(voice);
        out.push(Action::Start { voice, sample, plays, group });
    }

    /// Start a looping sample unless the voice already holds one.
    fn hold(&mut self, voice: Voice, sample: SampleKey, group: Group, out: &mut Vec<Action>) {
        if !self.held.contains(&voice) {
            self.start(voice, sample, Plays::Forever, group, out);
        }
    }

    /// The inactive branch: pause the engine, stop every voice, forget the travel history.
    fn idle(&mut self, out: &mut Vec<Action>) {
        out.push(Action::PauseEngines);
        out.push(Action::EngineUpdate);
        for voice in Voice::RIG {
            self.stop(voice, out);
        }
        self.previous.suspension_travel = WheelValues::default();
    }

    fn active(&mut self, i: &ShakeInput, ctx: &FrameContext, out: &mut Vec<Action>) {
        out.push(match &ctx.tuning.native_dsp {
            Some(settings) => Action::NativeFilters(settings.clone()),
            None => Action::Filters(Box::new(filters(ctx))),
        });
        self.gear(i, out);
        self.travel_range(
            i.landing_gear_right,
            Voice::LandingGearRight,
            Group::LandingGearRight,
            SampleFolder::LandingGear,
            out,
        );
        if i.turbulence < 1.0 {
            self.stop(Voice::Turbulence, out);
        } else {
            self.hold(Voice::Turbulence, SampleKey::new(SampleFolder::Turbulence, 0), Group::Turbulence, out);
        }
        self.travel_range(
            i.landing_gear_left,
            Voice::LandingGearLeft,
            Group::LandingGearLeft,
            SampleFolder::LandingGear,
            out,
        );
        self.travel_range(
            i.landing_gear_nose,
            Voice::LandingGearNose,
            Group::LandingGearNose,
            SampleFolder::LandingGear,
            out,
        );
        self.touchdown(
            i.touchdown_right,
            self.previous.touchdown_right,
            Voice::TouchdownRight,
            Group::TouchdownRight,
            out,
        );
        self.touchdown(i.touchdown_left, self.previous.touchdown_left, Voice::TouchdownLeft, Group::TouchdownLeft, out);
        self.touchdown(i.touchdown_nose, self.previous.touchdown_nose, Voice::TouchdownNose, Group::TouchdownNose, out);
        // The folders are crossed: the missile plays sounds/cannon_firing and vice versa.
        self.while_nonzero(
            i.missile_firing,
            Voice::Missile,
            SampleKey::new(SampleFolder::CannonFiring, 0),
            Group::MissileFiring,
            out,
        );
        self.while_nonzero(
            i.abs_lock,
            Voice::AbsLock,
            SampleKey::new(SampleFolder::Suspension, 5),
            Group::AbsLock,
            out,
        );
        self.while_nonzero(
            i.cannon_firing,
            Voice::Cannon,
            SampleKey::new(SampleFolder::MissileFiring, 0),
            Group::CannonFiring,
            out,
        );
        self.travel_range(i.canopy, Voice::Canopy, Group::Canopy, SampleFolder::Canopy, out);
        self.afterburner(i.afterburner, out);
        if let Err(error) = self.impacts(i, ctx, out) {
            out.push(Action::TractionFailure(error));
            return;
        }
        if let Err(error) = self.wheels(i, ctx, out).and_then(|()| self.road_bump(i, ctx, out)) {
            out.push(Action::SuspensionFailure(error));
            return;
        }
        out.push(Action::Engine(engine_command(i, ctx)));
        out.push(Action::EngineUpdate);
        self.remember(i);
    }

    /// Gear change thump: one pass of transmission/1 on a change to a gear other than 0.
    fn gear(&mut self, i: &ShakeInput, out: &mut Vec<Action>) {
        if self.previous.gear != i.gear && i.gear != 0.0 {
            self.start(
                Voice::Transmission,
                SampleKey::new(SampleFolder::Transmission, 1),
                Plays::Once,
                Group::Gear,
                out,
            );
        }
    }

    /// Landing gear and canopy: loop while the position is between 0.15 and 0.85 (moving).
    fn travel_range(&mut self, position: f64, voice: Voice, group: Group, folder: SampleFolder, out: &mut Vec<Action>) {
        if position < 0.15 || position > 0.85 {
            self.stop(voice, out);
        } else {
            self.hold(voice, SampleKey::new(folder, 0), group, out);
        }
    }

    /// Touchdown: a per-frame jump of 50 or more restarts suspension/4..6 (the jump 50..60 mapped to 4..6).
    fn touchdown(&mut self, value: f64, previous: f64, voice: Voice, group: Group, out: &mut Vec<Action>) {
        let diff = (value - previous).abs();
        if diff < 50.0 {
            self.stop(voice, out);
        } else {
            let n = map_clamped(diff, 50.0, 60.0, 4.0, 6.0).trunc() as u8;
            self.start(voice, SampleKey::new(SampleFolder::Suspension, n), Plays::Forever, group, out);
        }
    }

    /// Missile, ABS and cannon: loop while the value is non-zero.
    fn while_nonzero(&mut self, value: f64, voice: Voice, sample: SampleKey, group: Group, out: &mut Vec<Action>) {
        if value == 0.0 {
            self.stop(voice, out);
        } else {
            self.hold(voice, sample, group, out);
        }
    }

    /// Afterburner: on a change, loop when it became 1, stop otherwise.
    fn afterburner(&mut self, value: f64, out: &mut Vec<Action>) {
        if value != self.previous.afterburner {
            if value == 1.0 {
                self.start(
                    Voice::Afterburner,
                    SampleKey::new(SampleFolder::Afterburner, 0),
                    Plays::Forever,
                    Group::Afterburner,
                    out,
                );
            } else {
                self.stop(Voice::Afterburner, out);
            }
        }
    }

    /// Impact, lateral G, rear traction loss, deceleration and acceleration: a positive sample
    /// restarts every frame and plays three times.
    fn impacts(&mut self, i: &ShakeInput, ctx: &FrameContext, out: &mut Vec<Action>) -> Result<(), DspError> {
        let impact = lookup(&tables::IMPACT, i.longitudinal_acceleration);
        self.pulse(impact, Voice::Impact, Group::Impact, out);

        let lat = lookup(&tables::LATERAL, i.lateral_acceleration.abs());
        let lat_group = if i.lateral_acceleration < 0.0 { Group::LatLeft } else { Group::LatRight };
        self.pulse(lat, Voice::Lateral, lat_group, out);

        self.live_traction(i, ctx, out)?;

        let brake = lookup(&tables::BRAKE, i.longitudinal_acceleration);
        self.pulse(brake, Voice::Brake, Self::live_side_group(i, Group::BrakeRf, Group::BrakeLf, Group::Brake), out);
        let accel = lookup(&tables::ACCEL, i.longitudinal_acceleration);
        self.pulse(accel, Voice::Accel, Self::live_side_group(i, Group::AccelRr, Group::AccelLr, Group::Accel), out);
        Ok(())
    }

    fn pulse(&mut self, file: i64, voice: Voice, group: Group, out: &mut Vec<Action>) {
        match suspension_key(file) {
            None => self.stop(voice, out),
            Some(sample) => self.start(voice, sample, Plays::Repeat(2), group, out),
        }
    }

    /// Wheel slip and suspension per corner: a positive sample loops, restarting when it changes.
    fn wheels(&mut self, i: &ShakeInput, ctx: &FrameContext, out: &mut Vec<Action>) -> Result<(), DspError> {
        use super::layout::Group as G;
        let slip = [
            (i.wheel_slip.lf, Voice::WheelSlipLf, G::WheelSlipLf),
            (i.wheel_slip.rf, Voice::WheelSlipRf, G::WheelSlipRf),
            (i.wheel_slip.lr, Voice::WheelSlipLr, G::WheelSlipLr),
            (i.wheel_slip.rr, Voice::WheelSlipRr, G::WheelSlipRr),
        ];
        for (k, (value, voice, group)) in slip.into_iter().enumerate() {
            let file = lookup(&tables::WHEEL_SLIP, value);
            let previous = self.previous.wheel_slip_file[k];
            self.level_loop(file, previous, voice, group, out);
            self.previous.wheel_slip_file[k] = file;
        }

        for k in 0..4 {
            self.previous.suspension_file[k] = self.live_suspension(i, ctx, k, out)?;
        }
        Ok(())
    }

    fn level_loop(&mut self, file: i64, previous: i64, voice: Voice, group: Group, out: &mut Vec<Action>) {
        match suspension_key(file) {
            None => self.stop(voice, out),
            Some(sample) if file != previous => self.start(voice, sample, Plays::Forever, group, out),
            Some(_) => {}
        }
    }

    /// Road bump (vertical G), gated by the largest shock velocity of the four wheels.
    fn road_bump(&mut self, i: &ShakeInput, ctx: &FrameContext, out: &mut Vec<Action>) -> Result<(), DspError> {
        let sv = i.suspension_velocity;
        let file = road_bump_sample_checked(
            i.vertical_acceleration,
            max_velocity([sv.lf, sv.lr, sv.rf, sv.rr]),
            ctx.config,
            ctx.tuning.native_suspension.as_deref(),
        )?;
        let previous = self.previous.road_bump_file;
        self.level_loop(file, previous, Voice::RoadBump, Group::RoadBump, out);
        self.previous.road_bump_file = file;
        Ok(())
    }

    fn remember(&mut self, i: &ShakeInput) {
        let p = &mut self.previous;
        p.gear = i.gear;
        p.afterburner = i.afterburner;
        p.touchdown_left = i.touchdown_left;
        p.touchdown_nose = i.touchdown_nose;
        p.touchdown_right = i.touchdown_right;
        p.suspension_travel = i.suspension_travel;
    }

    /// The frame's engine action failed (no instance for the engine type, a failing FMOD
    /// call): the frame stops there and the caller gets the
    /// error, so what follows it in the frame does not run. `before` is the engine as it was
    /// before the frame. In the inactive branch the engine instances are paused first, so none of
    /// the branch happened (the voices stay, the suspension travel history keeps its values); in
    /// the active branch the engine block is the last block, so only the previous-value updates
    /// after it, including the sample numbers stored eagerly,
    /// are undone. A failed suspension step has the same
    /// end-of-frame commit boundary while preserving preceding voice operations.
    pub fn abort_frame(&mut self, before: &ShakeRig, status: RigStatus) {
        match status {
            RigStatus::Disabled => {}
            RigStatus::Idle => self.clone_from(before),
            RigStatus::Active => self.previous = before.previous,
        }
    }
}

/// Sample `n` of the suspension set for a positive table result.
fn suspension_key(file: i64) -> Option<SampleKey> {
    (file > 0).then(|| SampleKey::new(SampleFolder::Suspension, file.min(i64::from(u8::MAX)) as u8))
}

/// `lateral_velocity / (2 * max_traction_loss) * 100`, or 0 without a maximum.
pub fn traction_loss_percent(lateral_velocity: f64, max_traction_loss: f64) -> f64 {
    if max_traction_loss != 0.0 {
        lateral_velocity / (2.0 * max_traction_loss) * 100.0
    } else {
        0.0
    }
}

/// Raw `max != 0`, then `lateral_velocity / (2 * max) * 100`, each operation
/// on the raw operands (any-width integers, object handles, sequence repetition by `2 *`,
/// float overflow and division errors); the result is the float the table lookup receives.
fn native_traction_loss_percent(velocity: &LegacyValue, maximum: &LegacyValue) -> Result<f64, DspError> {
    use openrigcue_model::python_compare::native_equal_checked;
    let native = DspError::from_native;
    if !native_equal_checked(maximum, &LegacyValue::Int(0)).map_err(native)? {
        let denominator = LegacyValue::Int(2).py_mul(maximum).map_err(native)?;
        let percent =
            velocity.py_truediv(&denominator).and_then(|q| q.py_mul(&LegacyValue::Int(100))).map_err(native)?;
        return percent.py_as_double().map_err(native);
    }
    Ok(0.0)
}

/// The per-frame DSP block: the low-pass applies when the effect's DSP switch is on.
fn filters(ctx: &FrameContext) -> PerGroup<FilterSettings> {
    let mut f = [FilterSettings::default(); GROUP_COUNT];
    for group in Group::ALL {
        let t = ctx.tuning.effect(group.effect());
        f[group.index()] =
            FilterSettings { low_pass: t.low_pass, parameters: LowPass { cutoff_hz: t.cutoff_hz, q: t.resonance } };
    }
    f
}

/// The engine block of a frame.
pub(super) fn engine_command(i: &ShakeInput, ctx: &FrameContext) -> EngineCommand {
    let engine = ctx.tuning.effect(Effect::Engine);
    let rpm_param = if i.rpm.is_finite() && i.max_rpm.is_finite() {
        map_upper_clamped(i.rpm.trunc(), 0.0, i.max_rpm.trunc(), 0.0, 10000.0).map_or(0.0, |r| py_round(r, 2))
    } else {
        0.0
    };
    let mix = ctx.levels.of(Group::Engine);
    let mut levels = [0.0; 6];
    let mut complete = true;
    for (slot, volume) in levels.iter_mut().zip(mix) {
        match engine_fade(
            engine.low_pass,
            volume,
            i.rpm,
            i.max_rpm,
            ctx.tuning.engine_fade_start,
            ctx.tuning.engine_fade_end,
        ) {
            Some(v) => *slot = v,
            None => complete = false,
        }
    }
    // The pitch is the engine's `shaker_resonance` when the engine DSP switch is on, else 35.
    let pitch = if engine.low_pass { engine.resonance } else { 35.0 };
    let native_parameters = ctx.tuning.native_engine.as_ref().map(|settings| {
        use openrigcue_model::native_value::LegacyValue;
        use openrigcue_model::python_compare::native_equal_checked;
        Box::new(NativeEngineParameters {
            levels: native_engine_levels(settings, ctx.levels.required_of(Group::Engine), i.rpm, i.max_rpm),
            pitch: settings.flag.as_ref().map_err(Clone::clone).and_then(|flag| {
                if native_equal_checked(flag, &LegacyValue::Int(1)).map_err(DspError::from_native)? {
                    settings.resonance.clone()
                } else {
                    Ok(35.0)
                }
            }),
        })
    });
    EngineCommand {
        engine_type: ctx.tuning.engine_type,
        native_engine_selector: ctx.tuning.native_engine_selector.clone(),
        speaker_levels: native_parameters
            .as_ref()
            .map_or_else(|| complete.then_some(levels), |p| p.levels.as_ref().ok().copied()),
        pitch: native_parameters.as_ref().map_or(pitch, |p| p.pitch.as_ref().copied().unwrap_or(35.0)),
        native_parameters,
        rpm: rpm_param,
        load: 1.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shakers::layout::Speaker;
    use crate::shakers::profiles::SpeakerProfile;

    struct Fixture {
        tuning: ShakerTuning,
        config: ShakeboxConfig,
        levels: GroupLevels,
    }

    impl Fixture {
        fn new() -> Fixture {
            let tuning = ShakerTuning::default();
            let profile = SpeakerProfile::built_in("6 Speakers - 5.1").unwrap();
            let levels = GroupLevels::compute(&profile, &tuning, None);
            Fixture { tuning, config: ShakeboxConfig::default(), levels }
        }

        fn ctx(&self, status: RigStatus) -> FrameContext<'_> {
            FrameContext {
                status,
                tuning: &self.tuning,
                config: &self.config,
                levels: &self.levels,
                max_traction_loss: 2.0,
                native_lateral_velocity: None,
            }
        }
    }

    fn run(rig: &mut ShakeRig, f: &Fixture, i: &ShakeInput) -> Vec<Action> {
        rig.frame(i, &f.ctx(RigStatus::Active))
    }

    fn starts(actions: &[Action]) -> Vec<(Voice, SampleKey, Plays, Group)> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Start { voice, sample, plays, group } => Some((*voice, *sample, *plays, *group)),
                _ => None,
            })
            .collect()
    }

    fn start_of(actions: &[Action], voice: Voice) -> Option<(SampleKey, Plays, Group)> {
        starts(actions).into_iter().find(|s| s.0 == voice).map(|s| (s.1, s.2, s.3))
    }

    fn stopped(actions: &[Action], voice: Voice) -> bool {
        actions.contains(&Action::Stop(voice))
    }

    fn engine(actions: &[Action]) -> EngineCommand {
        actions.iter().find_map(|a| if let Action::Engine(e) = a { Some(e.clone()) } else { None }).unwrap()
    }

    const SUSP: fn(u8) -> SampleKey = |n| SampleKey::new(SampleFolder::Suspension, n);

    #[test]
    fn rig_status_shake_rig_activation() {
        assert_eq!(RigStatus::evaluate(false, true, true), RigStatus::Disabled);
        assert_eq!(RigStatus::evaluate(true, false, true), RigStatus::Idle);
        assert_eq!(RigStatus::evaluate(true, true, false), RigStatus::Idle);
        assert_eq!(RigStatus::evaluate(true, true, true), RigStatus::Active);
    }

    #[test]
    fn shake_rig_disabled_does_nothing() {
        let f = Fixture::new();
        let mut rig = ShakeRig::new();
        let i = ShakeInput { gear: 3.0, ..ShakeInput::default() };
        assert!(rig.frame(&i, &f.ctx(RigStatus::Disabled)).is_empty());
        // State untouched: the gear change still fires on the first active frame.
        assert!(start_of(&run(&mut rig, &f, &i), Voice::Transmission).is_some());
    }

    #[test]
    fn shake_rig_gear_change() {
        let f = Fixture::new();
        let mut rig = ShakeRig::new();
        let a = run(&mut rig, &f, &ShakeInput { gear: 1.0, ..ShakeInput::default() });
        assert_eq!(
            start_of(&a, Voice::Transmission),
            Some((SampleKey::new(SampleFolder::Transmission, 1), Plays::Once, Group::Gear))
        );
        assert!(start_of(&run(&mut rig, &f, &ShakeInput { gear: 1.0, ..ShakeInput::default() }), Voice::Transmission)
            .is_none());
        // A change to gear 0 plays nothing.
        assert!(start_of(&run(&mut rig, &f, &ShakeInput::default()), Voice::Transmission).is_none());
    }

    #[test]
    fn shake_rig_landing_gear_canopy_turbulence() {
        let f = Fixture::new();
        let mut rig = ShakeRig::new();
        let moving = ShakeInput { landing_gear_left: 0.5, canopy: 0.15, turbulence: 1.0, ..ShakeInput::default() };
        let a = run(&mut rig, &f, &moving);
        assert_eq!(
            start_of(&a, Voice::LandingGearLeft),
            Some((SampleKey::new(SampleFolder::LandingGear, 0), Plays::Forever, Group::LandingGearLeft))
        );
        assert_eq!(start_of(&a, Voice::Canopy).unwrap().0, SampleKey::new(SampleFolder::Canopy, 0));
        assert_eq!(start_of(&a, Voice::Turbulence).unwrap().0, SampleKey::new(SampleFolder::Turbulence, 0));
        // Held, not restarted.
        assert!(starts(&run(&mut rig, &f, &moving)).is_empty());
        let a = run(
            &mut rig,
            &f,
            &ShakeInput { landing_gear_left: 0.86, canopy: 0.14, turbulence: 0.99, ..ShakeInput::default() },
        );
        assert!(stopped(&a, Voice::LandingGearLeft) && stopped(&a, Voice::Canopy) && stopped(&a, Voice::Turbulence));
    }

    #[test]
    fn shake_rig_touchdown() {
        let f = Fixture::new();
        let mut rig = ShakeRig::new();
        let a = run(&mut rig, &f, &ShakeInput { touchdown_left: 55.0, ..ShakeInput::default() });
        assert_eq!(start_of(&a, Voice::TouchdownLeft), Some((SUSP(5), Plays::Forever, Group::TouchdownLeft)));
        // Same value next frame: diff 0 < 50 -> stop.
        let a = run(&mut rig, &f, &ShakeInput { touchdown_left: 55.0, ..ShakeInput::default() });
        assert!(stopped(&a, Voice::TouchdownLeft));
        let a = run(&mut rig, &f, &ShakeInput { touchdown_left: 55.0 + 50.0, ..ShakeInput::default() });
        assert_eq!(start_of(&a, Voice::TouchdownLeft).unwrap().0, SUSP(4));
        let a = run(&mut rig, &f, &ShakeInput { touchdown_left: 0.0, ..ShakeInput::default() });
        assert_eq!(start_of(&a, Voice::TouchdownLeft).unwrap().0, SUSP(6));
    }

    #[test]
    fn shake_rig_weapons_swap_and_abs() {
        let f = Fixture::new();
        let mut rig = ShakeRig::new();
        let a = run(
            &mut rig,
            &f,
            &ShakeInput { missile_firing: 1.0, cannon_firing: 1.0, abs_lock: 1.0, ..ShakeInput::default() },
        );
        assert_eq!(
            start_of(&a, Voice::Missile),
            Some((SampleKey::new(SampleFolder::CannonFiring, 0), Plays::Forever, Group::MissileFiring))
        );
        assert_eq!(
            start_of(&a, Voice::Cannon),
            Some((SampleKey::new(SampleFolder::MissileFiring, 0), Plays::Forever, Group::CannonFiring))
        );
        assert_eq!(start_of(&a, Voice::AbsLock), Some((SUSP(5), Plays::Forever, Group::AbsLock)));
        let a = run(&mut rig, &f, &ShakeInput::default());
        assert!(stopped(&a, Voice::Missile) && stopped(&a, Voice::Cannon) && stopped(&a, Voice::AbsLock));
    }

    #[test]
    fn shake_rig_afterburner_edges() {
        let f = Fixture::new();
        let mut rig = ShakeRig::new();
        let a = run(&mut rig, &f, &ShakeInput { afterburner: 1.0, ..ShakeInput::default() });
        assert!(start_of(&a, Voice::Afterburner).is_some());
        assert!(start_of(
            &run(&mut rig, &f, &ShakeInput { afterburner: 1.0, ..ShakeInput::default() }),
            Voice::Afterburner
        )
        .is_none());
        let a = run(&mut rig, &f, &ShakeInput { afterburner: 0.5, ..ShakeInput::default() });
        assert!(stopped(&a, Voice::Afterburner));
    }

    #[test]
    fn shake_rig_impact_restarts_every_frame() {
        let f = Fixture::new();
        let mut rig = ShakeRig::new();
        let hit = ShakeInput { longitudinal_acceleration: -55.0, ..ShakeInput::default() };
        let a = run(&mut rig, &f, &hit);
        assert_eq!(start_of(&a, Voice::Impact), Some((SUSP(6), Plays::Repeat(2), Group::Impact)));
        let a = run(&mut rig, &f, &hit);
        assert!(stopped(&a, Voice::Impact));
        assert!(start_of(&a, Voice::Impact).is_some());
        assert!(stopped(&run(&mut rig, &f, &ShakeInput::default()), Voice::Impact));
    }

    #[test]
    fn shake_rig_lateral_sides() {
        let f = Fixture::new();
        let mut rig = ShakeRig::new();
        let a = run(&mut rig, &f, &ShakeInput { lateral_acceleration: -12.0, ..ShakeInput::default() });
        assert_eq!(start_of(&a, Voice::Lateral), Some((SUSP(2), Plays::Repeat(2), Group::LatLeft)));
        let a = run(&mut rig, &f, &ShakeInput { lateral_acceleration: 5.0, ..ShakeInput::default() });
        assert_eq!(start_of(&a, Voice::Lateral), Some((SUSP(1), Plays::Repeat(2), Group::LatRight)));
    }

    #[test]
    fn shake_rig_traction_loss() {
        assert_eq!(traction_loss_percent(-1.0, 2.0), -25.0);
        assert_eq!(traction_loss_percent(3.0, 0.0), 0.0);
        let f = Fixture::new();
        let mut rig = ShakeRig::new();
        // -1 / (2 * 2) * 100 = -25 % -> row [20, 49.99] -> 5, left group.
        let a = run(&mut rig, &f, &ShakeInput { lateral_velocity: -1.0, ..ShakeInput::default() });
        assert_eq!(start_of(&a, Voice::TractionLoss), Some((SUSP(5), Plays::Repeat(2), Group::TractionLossLeft)));
        let a = run(&mut rig, &f, &ShakeInput { lateral_velocity: 0.1, ..ShakeInput::default() });
        assert_eq!(start_of(&a, Voice::TractionLoss).unwrap().2, Group::TractionLossRight);
    }

    #[test]
    fn native_traction_arithmetic_keeps_zero_shortcut_types_and_exact_integer_division() {
        for zero in [LegacyValue::Int(0), LegacyValue::Float(-0.0), LegacyValue::Bool(false)] {
            assert_eq!(native_traction_loss_percent(&LegacyValue::None, &zero), Ok(0.0));
        }
        for (velocity, maximum, expected) in [
            (LegacyValue::Int(1), LegacyValue::Int(2), 25.0),
            (LegacyValue::Float(-1.0), LegacyValue::Float(2.0), -25.0),
            (LegacyValue::Bool(true), LegacyValue::Bool(true), 50.0),
            (LegacyValue::Int(i64::MAX), LegacyValue::Int(i64::MAX), 50.0),
            (LegacyValue::Int(i64::MIN), LegacyValue::Int(i64::MIN), 50.0),
        ] {
            assert_eq!(native_traction_loss_percent(&velocity, &maximum), Ok(expected));
        }
        assert!(native_traction_loss_percent(&LegacyValue::Float(1.0), &LegacyValue::Float(f64::NAN))
            .unwrap()
            .is_nan());
        assert_eq!(native_traction_loss_percent(&LegacyValue::Float(1.0), &LegacyValue::Float(f64::INFINITY)), Ok(0.0));
        for maximum in [LegacyValue::None, LegacyValue::Dict(Default::default()), LegacyValue::Set(Vec::new())] {
            let error = native_traction_loss_percent(&LegacyValue::None, &maximum).unwrap_err();
            assert_eq!(error.class, "TypeError");
            assert_eq!(
                error.message,
                format!("unsupported operand type(s) for *: 'int' and '{}'", maximum.type_name())
            );
        }
        for maximum in [
            LegacyValue::from("2"),
            LegacyValue::Bytes(b"2".to_vec()),
            LegacyValue::List(Vec::new()),
            LegacyValue::Tuple(Vec::new()),
        ] {
            let error = native_traction_loss_percent(&LegacyValue::Int(1), &maximum).unwrap_err();
            assert_eq!(
                error.message,
                format!("unsupported operand type(s) for /: 'int' and '{}'", maximum.type_name())
            );
        }
        assert_eq!(
            native_traction_loss_percent(&LegacyValue::None, &LegacyValue::Float(2.0)).unwrap_err().message,
            "unsupported operand type(s) for /: 'NoneType' and 'float'"
        );
    }

    /// The raw operations take any-width integers and object handles: a
    /// BigInt maximum divides exactly, a velocity whose quotient leaves the float range is an
    /// overflow error at `/`, and object-wrapped operands are resolved before each operation.
    #[test]
    fn native_traction_arithmetic_takes_wide_integers_and_object_handles() {
        use openrigcue_model::native_object::NativeObject;
        let int = |text: &str| LegacyValue::from_integer(LegacyValue::from(text).py_integer().unwrap());
        let huge = int(&format!("1{}", "0".repeat(30)));
        assert_eq!(native_traction_loss_percent(&huge, &huge), Ok(50.0));
        assert_eq!(native_traction_loss_percent(&LegacyValue::Int(1), &huge), Ok(5e-31 * 100.0));
        let error =
            native_traction_loss_percent(&int(&format!("1{}", "0".repeat(400))), &LegacyValue::Int(2)).unwrap_err();
        assert_eq!(
            (error.class, error.message.as_str()),
            ("OverflowError", "integer division result too large for a float")
        );
        let error =
            native_traction_loss_percent(&LegacyValue::Float(1.0), &int(&format!("1{}", "0".repeat(400)))).unwrap_err();
        assert_eq!((error.class, error.message.as_str()), ("OverflowError", "int too large to convert to float"));
        let object = |value| LegacyValue::Object(NativeObject::new(value));
        assert_eq!(native_traction_loss_percent(&object(LegacyValue::Int(1)), &object(LegacyValue::Int(2))), Ok(25.0));
        assert_eq!(native_traction_loss_percent(&LegacyValue::None, &object(LegacyValue::Int(0))), Ok(0.0));
        let error = native_traction_loss_percent(&LegacyValue::Int(1), &object(LegacyValue::None)).unwrap_err();
        assert_eq!(error.message, "unsupported operand type(s) for *: 'int' and 'NoneType'");
    }

    #[test]
    fn late_memory_assignments_publish_only_the_source_ordered_prefix() {
        let f = Fixture::new();
        let mut rig = ShakeRig::new();
        let input = ShakeInput {
            gear: 2.0,
            afterburner: 1.0,
            touchdown_left: 55.0,
            touchdown_nose: 57.0,
            touchdown_right: 59.0,
            wheel_slip: WheelValues { lf: 7.0, rf: 9.0, lr: 11.0, rr: 13.0 },
            suspension_travel: WheelValues { lf: 0.01, rf: 0.02, lr: 0.03, rr: 0.04 },
            rpm: 3500.0,
            speed_kmh: 50.0,
            ..ShakeInput::default()
        };
        let mut frame = LiveFrame::new(RigStatus::Active);
        for stage in FrameStage::for_status(RigStatus::Active) {
            if matches!(stage, FrameStage::Commit(_)) {
                break;
            }
            rig.advance_live_frame(&input, &f.ctx(RigStatus::Active), &mut frame, stage);
        }
        let mut expected = Previous::default();
        assert_eq!(rig.previous, expected, "effect calculations must leave prior globals untouched");
        for field in MemoryField::ALL {
            rig.advance_live_frame(&input, &f.ctx(RigStatus::Active), &mut frame, FrameStage::Commit(field));
            match field {
                MemoryField::Gear => expected.gear = 2.0,
                MemoryField::Afterburner => expected.afterburner = 1.0,
                MemoryField::TouchdownLeft => expected.touchdown_left = 55.0,
                MemoryField::TouchdownNose => expected.touchdown_nose = 57.0,
                MemoryField::TouchdownRight => expected.touchdown_right = 59.0,
                MemoryField::Suspension(k) => expected.suspension_file[k] = frame.suspension[k].unwrap(),
                MemoryField::WheelSlip(k) => expected.wheel_slip_file[k] = frame.wheel_slip[k].unwrap(),
                MemoryField::RoadBump => expected.road_bump_file = frame.road_bump.unwrap(),
                MemoryField::Travel(0) => expected.suspension_travel.lf = 0.01,
                MemoryField::Travel(1) => expected.suspension_travel.rf = 0.02,
                MemoryField::Travel(2) => expected.suspension_travel.lr = 0.03,
                MemoryField::Travel(3) => expected.suspension_travel.rr = 0.04,
                _ => unreachable!(),
            }
            assert_eq!(rig.previous, expected, "after {field:?}");
        }
    }

    #[test]
    fn idle_travel_resets_publish_in_front_left_rear_left_front_right_rear_right_order() {
        let f = Fixture::new();
        let mut rig = ShakeRig::new();
        rig.previous.suspension_travel = WheelValues { lf: 1.0, rf: 2.0, lr: 3.0, rr: 4.0 };
        let input = ShakeInput::default();
        let mut frame = LiveFrame::new(RigStatus::Idle);
        let resets: Vec<_> = FrameStage::for_status(RigStatus::Idle)
            .into_iter()
            .filter(|stage| matches!(stage, FrameStage::IdleReset(_)))
            .collect();
        assert_eq!(
            resets,
            vec![
                FrameStage::IdleReset(0),
                FrameStage::IdleReset(2),
                FrameStage::IdleReset(1),
                FrameStage::IdleReset(3)
            ]
        );
        for (stage, expected) in resets.into_iter().zip([
            WheelValues { lf: 0.0, rf: 2.0, lr: 3.0, rr: 4.0 },
            WheelValues { lf: 0.0, rf: 2.0, lr: 0.0, rr: 4.0 },
            WheelValues { lf: 0.0, rf: 0.0, lr: 0.0, rr: 4.0 },
            WheelValues::default(),
        ]) {
            rig.advance_live_frame(&input, &f.ctx(RigStatus::Idle), &mut frame, stage);
            assert_eq!(rig.previous.suspension_travel, expected);
        }
    }

    #[test]
    fn another_frame_sees_new_gear_but_old_afterburner_until_its_assignment() {
        let f = Fixture::new();
        let mut rig = ShakeRig::new();
        let first = ShakeInput { gear: 2.0, afterburner: 1.0, ..ShakeInput::default() };
        let mut frame = LiveFrame::new(RigStatus::Active);
        for stage in FrameStage::for_status(RigStatus::Active) {
            rig.advance_live_frame(&first, &f.ctx(RigStatus::Active), &mut frame, stage);
            if stage == FrameStage::Commit(MemoryField::Gear) {
                break;
            }
        }
        let second = first;
        let mut concurrent = LiveFrame::new(RigStatus::Active);
        let gear = rig.advance_live_frame(&second, &f.ctx(RigStatus::Active), &mut concurrent, FrameStage::Gear);
        assert!(start_of(&gear, Voice::Transmission).is_none());
        let afterburner =
            rig.advance_live_frame(&second, &f.ctx(RigStatus::Active), &mut concurrent, FrameStage::Afterburner);
        assert!(start_of(&afterburner, Voice::Afterburner).is_some(), "afterburner previous flag is still old");
    }

    #[test]
    fn live_stages_match_pure_frame_for_normal_finite_inputs_and_state() {
        let f = Fixture::new();
        let mut pure = ShakeRig::new();
        let mut live = ShakeRig::new();
        for n in 0..48 {
            let input = ShakeInput {
                rpm: if n % 7 == 0 { 0.0 } else { 3000.0 },
                max_rpm: 7000.0,
                speed_kmh: 80.0,
                gear: (n % 6) as f64,
                longitudinal_acceleration: (n as f64 % 9.0) - 4.0,
                lateral_acceleration: (n as f64 % 8.0) - 3.0,
                vertical_acceleration: 0.3,
                lateral_velocity: 0.1,
                canopy: if n % 3 == 0 { 0.5 } else { 0.0 },
                afterburner: (n % 2) as f64,
                wheel_slip: WheelValues { lf: (n % 20) as f64, rf: 8.0, lr: 2.0, rr: 15.0 },
                suspension_travel: WheelValues { lf: n as f64 * 0.002, rf: n as f64 * 0.001, lr: 0.0, rr: 0.0 },
                ..ShakeInput::default()
            };
            let status = if n % 11 == 0 {
                RigStatus::Disabled
            } else if n % 9 == 0 {
                RigStatus::Idle
            } else {
                RigStatus::Active
            };
            let expected = pure.frame(&input, &f.ctx(status));
            let mut frame = LiveFrame::new(status);
            let mut actual = Vec::new();
            for stage in FrameStage::for_status(status) {
                if stage == FrameStage::EnginePitch {
                    actual.push(Action::Engine(engine_command(&input, &f.ctx(status))));
                }
                actual.extend(live.advance_live_frame(&input, &f.ctx(status), &mut frame, stage));
            }
            let normalize = |actions: Vec<Action>| {
                actions
                    .into_iter()
                    .flat_map(|action| match action {
                        Action::Filters(values) => Group::ALL
                            .into_iter()
                            .map(|group| {
                                let value = values[group.index()];
                                Action::NativeGroupFilter {
                                    group,
                                    settings: NativeDspSettings {
                                        filter_type: Ok(i32::from(value.low_pass)),
                                        cutoff_hz: Ok(value.parameters.cutoff_hz),
                                        q: Ok(value.parameters.q),
                                    },
                                }
                            })
                            .collect(),
                        Action::NativeFilters(values) => Group::ALL
                            .into_iter()
                            .map(|group| Action::NativeGroupFilter { group, settings: values[group.index()].clone() })
                            .collect(),
                        action => vec![action],
                    })
                    .flat_map(|action| match action {
                        Action::NativeGroupFilter { group, settings } => vec![
                            Action::NativeDspStep { group, update: DspUpdate::Remove },
                            Action::NativeDspStep { group, update: DspUpdate::Type(settings.filter_type) },
                            Action::NativeDspStep { group, update: DspUpdate::Frequency(settings.cutoff_hz) },
                            Action::NativeDspStep { group, update: DspUpdate::Q(settings.q) },
                            Action::NativeDspStep { group, update: DspUpdate::Attach },
                        ],
                        Action::PauseEngines => {
                            vec![Action::PauseEngine(EngineType::Original), Action::PauseEngine(EngineType::Enhanced)]
                        }
                        action => vec![action],
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(normalize(actual), normalize(expected), "frame{n}");
            assert_eq!(live.previous, pure.previous);
            assert_eq!(live.held, pure.held);
        }
    }

    #[test]
    fn native_traction_failure_stops_later_actions_without_phantom_held_state() {
        let mut f = Fixture::new();
        f.tuning.native_traction_max = Some(Ok(LegacyValue::from("2")));
        let input = ShakeInput {
            gear: 2.0,
            longitudinal_acceleration: -55.0,
            lateral_acceleration: 12.0,
            lateral_velocity: 1.0,
            wheel_slip: WheelValues { lf: 12.0, ..WheelValues::default() },
            ..ShakeInput::default()
        };
        let mut rig = ShakeRig::new();
        let actions = run(&mut rig, &f, &input);
        assert!(matches!(actions.last(), Some(Action::TractionFailure(_))));
        assert!(start_of(&actions, Voice::Transmission).is_some());
        assert!(start_of(&actions, Voice::Impact).is_some());
        assert!(start_of(&actions, Voice::Lateral).is_some());
        assert!(start_of(&actions, Voice::TractionLoss).is_none());
        assert!(start_of(&actions, Voice::WheelSlipLf).is_none());
        assert!(!rig.held.contains(&Voice::WheelSlipLf));
        assert_eq!(rig.previous.gear, INITIAL_PREVIOUS_GEAR);
        f.tuning.native_traction_max = Some(Ok(LegacyValue::Int(2)));
        let recovered = run(&mut rig, &f, &input);
        assert!(start_of(&recovered, Voice::Transmission).is_some());
        assert!(start_of(&recovered, Voice::WheelSlipLf).is_some());
        assert!(recovered.iter().any(|action| matches!(action, Action::Engine(_))));
    }

    #[test]
    fn shake_rig_brake_and_accel_corners() {
        let f = Fixture::new();
        let mut rig = ShakeRig::new();
        let a = run(
            &mut rig,
            &f,
            &ShakeInput { longitudinal_acceleration: -10.0, lateral_acceleration: 3.5, ..ShakeInput::default() },
        );
        assert_eq!(start_of(&a, Voice::Brake), Some((SUSP(2), Plays::Repeat(2), Group::BrakeRf)));
        let a = run(
            &mut rig,
            &f,
            &ShakeInput { longitudinal_acceleration: -10.0, lateral_acceleration: -3.5, ..ShakeInput::default() },
        );
        assert_eq!(start_of(&a, Voice::Brake).unwrap().2, Group::BrakeLf);
        let a = run(
            &mut rig,
            &f,
            &ShakeInput { longitudinal_acceleration: -10.0, lateral_acceleration: 3.0, ..ShakeInput::default() },
        );
        assert_eq!(start_of(&a, Voice::Brake).unwrap().2, Group::Brake);
        let a = run(
            &mut rig,
            &f,
            &ShakeInput { longitudinal_acceleration: 5.0, lateral_acceleration: 4.0, ..ShakeInput::default() },
        );
        assert_eq!(start_of(&a, Voice::Accel), Some((SUSP(2), Plays::Repeat(2), Group::AccelRr)));
        assert!(stopped(&a, Voice::Brake));
        let a = run(
            &mut rig,
            &f,
            &ShakeInput { longitudinal_acceleration: 3.0, lateral_acceleration: -4.0, ..ShakeInput::default() },
        );
        assert_eq!(start_of(&a, Voice::Accel), Some((SUSP(1), Plays::Repeat(2), Group::AccelLr)));
    }

    #[test]
    fn shake_rig_wheel_slip_restarts_on_level_change_only() {
        let f = Fixture::new();
        let mut rig = ShakeRig::new();
        let slip =
            |v| ShakeInput { wheel_slip: WheelValues { rr: v, ..WheelValues::default() }, ..ShakeInput::default() };
        let a = run(&mut rig, &f, &slip(4.0));
        assert_eq!(start_of(&a, Voice::WheelSlipRr), Some((SUSP(2), Plays::Forever, Group::WheelSlipRr)));
        assert!(start_of(&run(&mut rig, &f, &slip(5.0)), Voice::WheelSlipRr).is_none());
        assert_eq!(start_of(&run(&mut rig, &f, &slip(6.0)), Voice::WheelSlipRr).unwrap().0, SUSP(3));
        assert!(stopped(&run(&mut rig, &f, &slip(1.0)), Voice::WheelSlipRr));
    }

    #[test]
    fn shake_rig_idle_keeps_level_memory() {
        let f = Fixture::new();
        let mut rig = ShakeRig::new();
        let slip =
            ShakeInput { wheel_slip: WheelValues { lf: 12.0, ..WheelValues::default() }, ..ShakeInput::default() };
        run(&mut rig, &f, &slip);
        let idle = rig.frame(&slip, &f.ctx(RigStatus::Idle));
        assert_eq!(idle[0], Action::PauseEngines);
        assert!(stopped(&idle, Voice::WheelSlipLf));
        // The previous LF wheel-slip sample survives the inactive branch: same level, no restart.
        assert!(start_of(&run(&mut rig, &f, &slip), Voice::WheelSlipLf).is_none());
    }

    #[test]
    fn shake_rig_suspension_pairs_and_rumble() {
        let f = Fixture::new();
        let mut rig = ShakeRig::new();
        let base = ShakeInput { rpm: 3000.0, speed_kmh: 50.0, ..ShakeInput::default() };
        run(&mut rig, &f, &base);
        // LF moves 2.5 mm, RF stays: LF type 1 (no velocity) row [2, 3.99] -> 2.
        let mut i = base;
        i.suspension_travel.lf = 0.0025;
        let a = run(&mut rig, &f, &i);
        assert_eq!(start_of(&a, Voice::SuspensionLf), Some((SUSP(2), Plays::Forever, Group::SuspensionLf)));
        assert!(start_of(&a, Voice::SuspensionRf).is_none());
        // RF moves 10 mm while LF moves 1 mm: LF suppressed by side enhancement.
        let mut j = i;
        j.suspension_travel.lf = 0.0035;
        j.suspension_travel.rf = 0.01;
        let a = run(&mut rig, &f, &j);
        assert!(stopped(&a, Voice::SuspensionLf));
        assert_eq!(start_of(&a, Voice::SuspensionRf).unwrap().0, SUSP(5));
        // Kerb under RR: rumble 90 / 15 = 6 -> type 1 row [6, 7.99] -> 4.
        let mut k = j;
        k.rumble_pitch.rr = 90.0;
        assert_eq!(start_of(&run(&mut rig, &f, &k), Voice::SuspensionRr).unwrap().0, SUSP(4));
    }

    #[test]
    fn shake_rig_idle_resets_travel_history() {
        let f = Fixture::new();
        let mut rig = ShakeRig::new();
        let mut i = ShakeInput { rpm: 3000.0, speed_kmh: 50.0, ..ShakeInput::default() };
        i.suspension_travel.lf = 0.1;
        run(&mut rig, &f, &i);
        // Still at 0.1: no travel -> sample 0 -> stop.
        assert!(stopped(&run(&mut rig, &f, &i), Voice::SuspensionLf));
        rig.frame(&i, &f.ctx(RigStatus::Idle));
        // Travel history is back to 0: the same position reads as a 100 mm jump -> sample 6.
        assert_eq!(start_of(&run(&mut rig, &f, &i), Voice::SuspensionLf).unwrap().0, SUSP(6));
    }

    #[test]
    fn shake_rig_road_bump() {
        let f = Fixture::new();
        let mut rig = ShakeRig::new();
        let mut i = ShakeInput { vertical_acceleration: 4.5, ..ShakeInput::default() };
        i.suspension_velocity = WheelValues { lf: 0.01, lr: 0.2, rf: -0.5, rr: 0.0 };
        // max = 0.2 m/s = 200 mm/s in window -> type 0 row [4, 5.99] -> 4.
        let a = run(&mut rig, &f, &i);
        assert_eq!(start_of(&a, Voice::RoadBump), Some((SUSP(4), Plays::Forever, Group::RoadBump)));
        assert!(start_of(&run(&mut rig, &f, &i), Voice::RoadBump).is_none());
        i.vertical_acceleration = 0.0;
        assert!(stopped(&run(&mut rig, &f, &i), Voice::RoadBump));
    }

    #[test]
    fn shake_rig_engine_parameters() {
        let mut f = Fixture::new();
        let mut rig = ShakeRig::new();
        let i = ShakeInput { rpm: 4350.7, max_rpm: 8700.0, ..ShakeInput::default() };
        let e = engine(&run(&mut rig, &f, &i));
        // 4350.7 truncates to 4350; 4350 of 0..8700 in 0..10000 is 5000.0
        assert_eq!(e.rpm, 5000.0);
        assert_eq!(e.load, 1.0);
        assert_eq!(e.pitch, 35.0);
        assert_eq!(e.engine_type, Some(EngineType::Enhanced));
        // Engine levels are the engine group levels ('6 Speakers - 5.1': CE and SUB 10 -> 1.0 and 0.6).
        let l = e.speaker_levels.unwrap();
        assert_eq!(l[Speaker::Center.index()], 1.0);
        assert!((l[Speaker::Sub.index()] - 0.6).abs() < 1e-12);
        // max_rpm 0: the rpm parameter falls back to 0; levels untouched without the fade switch.
        let e = engine(&run(&mut rig, &f, &ShakeInput { rpm: 3000.0, max_rpm: 0.0, ..ShakeInput::default() }));
        assert_eq!(e.rpm, 0.0);
        assert!(e.speaker_levels.is_some());
        // With the fade switch on, max_rpm 0 aborts the level update; the pitch is the engine
        // resonance (default 35).
        f.tuning.effect_mut(Effect::Engine).low_pass = true;
        let e = engine(&run(&mut rig, &f, &ShakeInput { rpm: 3000.0, max_rpm: 0.0, ..ShakeInput::default() }));
        assert!(e.speaker_levels.is_none());
        assert_eq!(e.pitch, 35.0);
        f.tuning.effect_mut(Effect::Engine).resonance = 60.0;
        assert_eq!(engine(&run(&mut rig, &f, &i)).pitch, 60.0);
        // Above max rpm the parameter saturates at 10000.
        assert_eq!(
            engine(&run(&mut rig, &f, &ShakeInput { rpm: 9000.0, max_rpm: 8700.0, ..ShakeInput::default() })).rpm,
            10000.0
        );
    }

    #[test]
    fn shake_rig_dsp_follows_effect_switch() {
        let mut f = Fixture::new();
        f.tuning.effect_mut(Effect::Suspension).cutoff_hz = 120.0;
        let mut rig = ShakeRig::new();
        let filters =
            |a: &[Action]| a.iter().find_map(|x| if let Action::Filters(f) = x { Some(**f) } else { None }).unwrap();
        let a = run(&mut rig, &f, &ShakeInput::default());
        assert!(filters(&a).iter().all(|f| !f.low_pass));
        f.tuning.effect_mut(Effect::Suspension).low_pass = true;
        let a = run(&mut rig, &f, &ShakeInput::default());
        let fl = filters(&a);
        assert_eq!(
            fl[Group::SuspensionLf.index()],
            FilterSettings { low_pass: true, parameters: LowPass { cutoff_hz: 120.0, q: 10.0 } }
        );
        assert!(!fl[Group::WheelSlipLf.index()].low_pass);
    }

    #[test]
    fn shake_rig_module_init_previous_gear_minus_ten() {
        // Before the first frame the previous gear is -10 (the other previous values are 0).
        let f = Fixture::new();
        let in_gear = |gear| ShakeInput { gear, ..ShakeInput::default() };
        let mut rig = ShakeRig::new();
        assert!(start_of(&run(&mut rig, &f, &in_gear(-10.0)), Voice::Transmission).is_none());
        let mut rig = ShakeRig::new();
        assert!(start_of(&run(&mut rig, &f, &in_gear(-1.0)), Voice::Transmission).is_some());
    }

    #[test]
    fn refresh_motionbox_nan_scan_zeroes_shake_rig_input() {
        let f = Fixture::new();
        let mut t = Telemetry::off();
        t.harmonized_gear = f64::NAN;
        t.landing_gear_right = f64::NAN;
        t.canopy = f64::NAN;
        t.suspension_travel_lf = f64::NAN;
        let i = ShakeInput::from_telemetry(&t);
        assert_eq!((i.gear, i.landing_gear_right, i.canopy, i.suspension_travel.lf), (0.0, 0.0, 0.0, 0.0));
        // Gear 0 never thumps and positions 0 < 0.15 keep the loops stopped, frame after frame.
        let mut rig = ShakeRig::new();
        for _ in 0..3 {
            let a = run(&mut rig, &f, &i);
            assert!(start_of(&a, Voice::Transmission).is_none());
            assert!(start_of(&a, Voice::LandingGearRight).is_none());
            assert!(start_of(&a, Voice::Canopy).is_none());
        }
    }
}
