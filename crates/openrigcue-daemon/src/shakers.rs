//! The bass shakers: the shaker loop (one shake frame, then the
//! shift beep), the settings it reads, the sound output (`set_soundcard`, `list_soundcards`), the
//! channel-group rebuild the UI handlers call (`load_channel_groups`), and the Hardware/Shake page tests
//! (`test_sound`, `test_effect`).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use openrigcue_config::settings::Settings;
use openrigcue_config::tunes::{ChannelEffectLevels, SpeakerLayout, Tune};
use openrigcue_effects::accessories::alerts::{BeepCommand, BeepInputs, BeepPlayer};
use openrigcue_effects::accessories::dispatcher::{refresh_period, Accessories};
use openrigcue_effects::shakers::effect_test::{effect_test_frames, TEST_FRAME_GAP};
use openrigcue_effects::shakers::layout::{PerGroup, PerSpeaker};
use openrigcue_effects::shakers::mixer::EngineStatus;
use openrigcue_effects::shakers::nsis::NsisArchive;
use openrigcue_effects::shakers::samples::{SampleKey, SampleOrigin};
use openrigcue_effects::shakers::tuning::{
    native_item, native_path, DspError, EffectLevel, EffectTuning, EngineType, NativeDspSettings, NativeEngineSettings,
};
use openrigcue_effects::shakers::{
    Effect, EngineVoice, FrameState, Group, SampleBank, SampleSource, ShakeboxConfig, ShakerSettings, ShakerSystem,
    ShakerTuning, Speaker, SpeakerProfile,
};
use serde::Serialize;

use crate::runtime::Shared;
use crate::sound_devices::{AudioEvent, DeviceSources};
use crate::state::ServiceState;

/// The `[SHAKEBOX]` values a shake frame reads.
pub fn shakebox_config(settings: &Settings) -> ShakeboxConfig {
    let s = &settings.shakers;
    ShakeboxConfig {
        refresh_rate_hz: s.refresh_rate,
        side_enhancement: s.side_enhancement,
        reduction_factor: s.reduction_factor,
        min_side_enhancement: s.min_side_enhancement,
        min_suspension_velocity: s.min_suspension_velocity,
        max_suspension_velocity: s.max_suspension_velocity,
        min_bump_velocity: s.min_bump_velocity,
        max_bump_velocity: s.max_bump_velocity,
    }
}

/// The shaker keys of the active tune.
pub fn shaker_tuning(tune: &Tune) -> ShakerTuning {
    let levels = &tune.shaker_effects;
    let switches = &tune.shaker_switch;
    let filters = &tune.shaker_dsp_type;
    let cutoffs = &tune.shaker_cutoff;
    let resonances = &tune.shaker_resonance;
    let effect = |effect: Effect| {
        let (slider, enabled, filter, cutoff, resonance) = match effect {
            Effect::Engine => (levels.engine, switches.engine, filters.engine, cutoffs.engine, resonances.engine),
            Effect::Gear => (levels.gear, switches.gear, filters.gear, cutoffs.gear, resonances.gear),
            Effect::Suspension => {
                (levels.suspension, switches.suspension, filters.suspension, cutoffs.suspension, resonances.suspension)
            }
            Effect::RoadBump => {
                (levels.road_bump, switches.road_bump, filters.road_bump, cutoffs.road_bump, resonances.road_bump)
            }
            Effect::Impact => (levels.impact, switches.impact, filters.impact, cutoffs.impact, resonances.impact),
            Effect::Acceleration => (
                levels.acceleration,
                switches.acceleration,
                filters.acceleration,
                cutoffs.acceleration,
                resonances.acceleration,
            ),
            Effect::Deceleration => (
                levels.deceleration,
                switches.deceleration,
                filters.deceleration,
                cutoffs.deceleration,
                resonances.deceleration,
            ),
            Effect::LateralAcceleration => (
                levels.lateral_acceleration,
                switches.lateral_acceleration,
                filters.lateral_acceleration,
                cutoffs.lateral_acceleration,
                resonances.lateral_acceleration,
            ),
            Effect::TractionLoss => (
                levels.traction_loss,
                switches.traction_loss,
                filters.traction_loss,
                cutoffs.traction_loss,
                resonances.traction_loss,
            ),
            Effect::WheelSlip => {
                (levels.wheel_slip, switches.wheel_slip, filters.wheel_slip, cutoffs.wheel_slip, resonances.wheel_slip)
            }
            Effect::Afterburner => (
                levels.afterburner,
                switches.afterburner,
                filters.afterburner,
                cutoffs.afterburner,
                resonances.afterburner,
            ),
            Effect::CannonFiring => (
                levels.cannon_firing,
                switches.cannon_firing,
                filters.cannon_firing,
                cutoffs.cannon_firing,
                resonances.cannon_firing,
            ),
            Effect::Canopy => (levels.canopy, switches.canopy, filters.canopy, cutoffs.canopy, resonances.canopy),
            Effect::LandingGear => (
                levels.landing_gear,
                switches.landing_gear,
                filters.landing_gear,
                cutoffs.landing_gear,
                resonances.landing_gear,
            ),
            Effect::MissileFiring => (
                levels.missile_firing,
                switches.missile_firing,
                filters.missile_firing,
                cutoffs.missile_firing,
                resonances.missile_firing,
            ),
            Effect::AbsLock => {
                (levels.abs_lock, switches.abs_lock, filters.abs_lock, cutoffs.abs_lock, resonances.abs_lock)
            }
            Effect::Touchdown => {
                (levels.touchdown, switches.touchdown, filters.touchdown, cutoffs.touchdown, resonances.touchdown)
            }
            Effect::Turbulence => {
                (levels.turbulence, switches.turbulence, filters.turbulence, cutoffs.turbulence, resonances.turbulence)
            }
        };
        EffectTuning {
            level: Some(EffectLevel { slider, enabled }),
            low_pass: filter == 1,
            cutoff_hz: cutoff,
            resonance,
        }
    };
    let v = &tune.shaker_volume;
    ShakerTuning {
        effects: Effect::ALL.map(effect),
        native_effect_gains: None,
        native_group_bases: None,
        native_speaker_test: None,
        native_dsp: None,
        native_engine_selector: None,
        native_engine: None,
        native_traction_max: None,
        native_suspension: None,
        volume: speaker_values(|speaker| match speaker {
            Speaker::FrontLeft => v.fl,
            Speaker::FrontRight => v.fr,
            Speaker::Center => v.ce,
            Speaker::Sub => v.sub,
            Speaker::RearLeft => v.rl,
            Speaker::RearRight => v.rr,
        }),
        engine_fade_start: cutoffs.engine_fadeoff_start,
        engine_fade_end: cutoffs.engine_fadeoff_end,
        // The engine type: 'O' selects the standard engine, 'E' the enhanced one; any
        // other key has no instance (a missing-key error in the engine block).
        engine_type: match tune.shaker_engine_type.as_str() {
            "O" => Some(EngineType::Original),
            "E" => Some(EngineType::Enhanced),
            _ => None,
        },
    }
}

/// The six speakers in the mixer's order.
const SPEAKERS: [Speaker; 6] =
    [Speaker::FrontLeft, Speaker::FrontRight, Speaker::Center, Speaker::Sub, Speaker::RearLeft, Speaker::RearRight];

fn speaker_values(value: impl Fn(Speaker) -> f64) -> PerSpeaker {
    let mut values = [0.0; 6];
    for speaker in SPEAKERS {
        values[speaker.index()] = value(speaker);
    }
    values
}

/// One speaker's level of a channel group in a stored layout.
fn group_level(levels: &ChannelEffectLevels, group: Group) -> f64 {
    match group {
        Group::Engine => levels.engine,
        Group::Gear => levels.gear,
        Group::Afterburner => levels.afterburner,
        Group::CannonFiring => levels.cannon_firing,
        Group::MissileFiring => levels.missile_firing,
        Group::LandingGearLeft => levels.landing_gear_left,
        Group::LandingGearNose => levels.landing_gear_nose,
        Group::LandingGearRight => levels.landing_gear_right,
        Group::TouchdownLeft => levels.touchdown_left,
        Group::TouchdownNose => levels.touchdown_nose,
        Group::TouchdownRight => levels.touchdown_right,
        Group::Turbulence => levels.turbulence,
        Group::Canopy => levels.canopy,
        Group::SuspensionLf => levels.suspension_lf,
        Group::SuspensionRf => levels.suspension_rf,
        Group::SuspensionLr => levels.suspension_lr,
        Group::SuspensionRr => levels.suspension_rr,
        Group::WheelSlipLf => levels.wheel_slip_lf,
        Group::WheelSlipRf => levels.wheel_slip_rf,
        Group::WheelSlipLr => levels.wheel_slip_lr,
        Group::WheelSlipRr => levels.wheel_slip_rr,
        Group::RoadBump => levels.road_bump,
        Group::Brake => levels.brake,
        Group::AbsLock => levels.abs_lock,
        Group::Accel => levels.accel,
        Group::Impact => levels.impact,
        Group::BrakeLf => levels.brake_lf,
        Group::BrakeRf => levels.brake_rf,
        Group::AccelLr => levels.accel_lr,
        Group::AccelRr => levels.accel_rr,
        Group::LatLeft => levels.lat_l,
        Group::LatRight => levels.lat_r,
        Group::TractionLossLeft => levels.traction_loss_l,
        Group::TractionLossRight => levels.traction_loss_r,
    }
}

/// A stored speaker layout as the mixer's per-group speaker matrix.
pub fn speaker_profile(name: &str, layout: &SpeakerLayout) -> SpeakerProfile {
    let channel = |speaker: Speaker| match speaker {
        Speaker::FrontLeft => &layout.front_left,
        Speaker::FrontRight => &layout.front_right,
        Speaker::Center => &layout.center,
        Speaker::Sub => &layout.subwoofer,
        Speaker::RearLeft => &layout.rear_left,
        Speaker::RearRight => &layout.rear_right,
    };
    let mut enabled = [false; 6];
    for speaker in SPEAKERS {
        enabled[speaker.index()] = channel(speaker).enable;
    }
    let levels: PerGroup<PerSpeaker> =
        Group::ALL.map(|group| speaker_values(|speaker| group_level(&channel(speaker).sfx, group)));
    SpeakerProfile { name: name.to_owned(), enabled, levels }
}

/// Everything a shake frame reads from the configuration and the active tune.
pub fn shaker_settings(state: &ServiceState) -> ShakerSettings {
    let config = state.config.read();
    let profile = state.profile.read();
    let loaded = state.shaker_layout.read();
    let layout: Option<SpeakerLayout> = openrigcue_config::import::de::from_legacy(&loaded.value).ok();
    let mut tuning = shaker_tuning(&profile.tune);
    let native = profile.native_tune();
    tuning.native_dsp = Some(Box::new(native_dsp_settings(&native)));
    tuning.native_engine_selector = Some(native_item(&native, "shaker_engine_type"));
    tuning.native_engine = Some(Box::new(native_engine_settings(&native)));
    tuning.native_traction_max = Some(native_traction_max(&native));
    tuning.native_suspension = Some(Box::new(native_suspension_settings(config.settings())));
    let (volume, gains) = native_group_gain_inputs(&native);
    if let Some(volume) = volume {
        tuning.volume = volume;
    }
    tuning.native_effect_gains = Some(Box::new(gains));
    tuning.native_group_bases = Some(Box::new(native_group_bases(&native, &loaded.value)));
    tuning.native_speaker_test = Some(Box::new(native_speaker_test_levels(&native)));
    ShakerSettings {
        config: shakebox_config(config.settings()),
        profile: layout.as_ref().map(|layout| speaker_profile(&loaded.name, layout)),
        tuning,
        max_traction_loss: profile.tune.max_telemetry_vars.traction_loss,
    }
}

/// Each `SHAKEBOX` suspension threshold as an integer, kept exact at any size.
fn native_suspension_settings(settings: &Settings) -> openrigcue_effects::shakers::tuning::NativeSuspensionSettings {
    use openrigcue_model::native_value::LegacyValue;
    let raw = LegacyValue::Dict(settings.native_snapshot());
    let read = |key: &str| {
        let value = native_path(&raw, &["SHAKEBOX", key])?;
        value.py_integer().map(LegacyValue::from_integer).map_err(DspError::from_native)
    };
    openrigcue_effects::shakers::tuning::NativeSuspensionSettings {
        side_enhancement: read("side_enhancement"),
        min_side_enhancement: read("min_side_enhancement"),
        min_suspension_velocity: read("min_suspension_velocity"),
        max_suspension_velocity: read("max_suspension_velocity"),
        min_bump_velocity: read("min_bump_velocity"),
        max_bump_velocity: read("max_bump_velocity"),
    }
}

fn native_traction_max(
    native: &openrigcue_model::native_value::LegacyValue,
) -> Result<openrigcue_model::native_value::LegacyValue, DspError> {
    native_path(native, &["max_telemetry_vars", "traction_loss"])
}

/// The typed volume view, and each effect's gain: its level times its integer switch,
/// `/ 2.0 * 20 / 100`, evaluated on the stored values; a failed step gives no value
/// (`None`, gain 1 or 0 later).
fn native_group_gain_inputs(
    native: &openrigcue_model::native_value::LegacyValue,
) -> (Option<PerSpeaker>, [Option<f64>; 18]) {
    use openrigcue_model::native_value::LegacyValue;
    let volume = (|| {
        let volumes = native_item(native, "shaker_volume").ok()?;
        let mut volume = [0.0; 6];
        for (slot, key) in volume.iter_mut().zip(["fl", "fr", "ce", "sub", "rl", "rr"]) {
            *slot = native_item(&volumes, key).ok()?.number()?;
        }
        Some(volume)
    })();
    let gains = Effect::ALL.map(|effect| {
        let level = native_path(native, &["shaker_effects", effect.key()]).ok()?;
        let switch = native_path(native, &["shaker_switch", effect.key()]).ok()?;
        let switch = LegacyValue::from_integer(switch.py_integer().ok()?);
        level
            .py_mul(&switch)
            .and_then(|v| v.py_truediv(&LegacyValue::Float(2.0)))
            .and_then(|v| v.py_mul(&LegacyValue::Int(20)))
            .and_then(|v| v.py_truediv(&LegacyValue::Int(100)))
            .and_then(|v| v.py_as_double())
            .ok()
    });
    (volume, gains)
}

/// The speaker's level of a group `/ 10`, times the speaker's volume `/ 2.0 * 20 / 100`, per speaker,
/// evaluated on the stored values; a failure is an error at its step.
fn native_group_bases(
    native: &openrigcue_model::native_value::LegacyValue,
    layout: &openrigcue_model::native_value::LegacyValue,
) -> PerGroup<Result<PerSpeaker, DspError>> {
    use openrigcue_model::native_value::LegacyValue;
    Group::ALL.map(|group| {
        let mut levels = [0.0; 6];
        for (slot, (channel, volume)) in levels.iter_mut().zip([
            ("FL", "fl"),
            ("FR", "fr"),
            ("CE", "ce"),
            ("SUB", "sub"),
            ("RL", "rl"),
            ("RR", "rr"),
        ]) {
            let matrix = native_path(layout, &[channel, "sfx", group.key()])?;
            let matrix = matrix.py_truediv(&LegacyValue::Int(10)).map_err(DspError::from_native)?;
            let volume = native_path(native, &["shaker_volume", volume])?;
            let volume = volume
                .py_truediv(&LegacyValue::Float(2.0))
                .and_then(|v| v.py_mul(&LegacyValue::Int(20)))
                .and_then(|v| v.py_truediv(&LegacyValue::Int(100)))
                .map_err(DspError::from_native)?;
            *slot = matrix.py_mul(&volume).and_then(|v| v.py_as_double()).map_err(DspError::from_native)?;
        }
        Ok(levels)
    })
}

/// The speaker's volume `/ 20.0`, for the speaker test.
fn native_speaker_test_levels(native: &openrigcue_model::native_value::LegacyValue) -> Result<PerSpeaker, DspError> {
    use openrigcue_model::native_value::LegacyValue;
    let mut levels = [0.0; 6];
    for (slot, key) in levels.iter_mut().zip(["fl", "fr", "ce", "sub", "rl", "rr"]) {
        let value = native_path(native, &["shaker_volume", key])?;
        *slot = value
            .py_truediv(&LegacyValue::Float(20.0))
            .and_then(|v| v.py_as_double())
            .map_err(DspError::from_native)?;
    }
    Ok(levels)
}

fn native_engine_settings(native: &openrigcue_model::native_value::LegacyValue) -> NativeEngineSettings {
    let field = |section, key| native_path(native, &[section, key]);
    NativeEngineSettings {
        flag: field("shaker_dsp_type", "engine"),
        fade_start: field("shaker_cutoff", "engine_fadeoff_start"),
        fade_end: field("shaker_cutoff", "engine_fadeoff_end"),
        resonance: field("shaker_resonance", "engine")
            .and_then(|value| value.py_float_native().map_err(DspError::from_native)),
    }
}

/// An integer narrowed to the 32-bit argument of the sound engine call: a value that fits a signed or
/// an unsigned 32-bit integer is passed as its 32 bits; anything else is an overflow error.
fn dsp_filter_int(value: &openrigcue_model::native_value::LegacyValue) -> Result<i32, DspError> {
    let integer = value.py_integer().map_err(DspError::from_native)?;
    let overflow = || DspError {
        class: "ctypes.ArgumentError",
        message: "argument 3: OverflowError: int too long to convert".to_owned(),
    };
    let openrigcue_model::native_value::LegacyValue::Int(integer) =
        openrigcue_model::native_value::LegacyValue::from_integer(integer)
    else {
        return Err(overflow());
    };
    if !(i64::from(i32::MIN)..=i64::from(u32::MAX)).contains(&integer) {
        return Err(overflow());
    }
    Ok(integer as u32 as i32)
}

fn native_dsp_settings(native: &openrigcue_model::native_value::LegacyValue) -> PerGroup<NativeDspSettings> {
    Group::ALL.map(|group| {
        let item = |section| native_path(native, &[section, group.effect().key()]);
        NativeDspSettings {
            filter_type: item("shaker_dsp_type").and_then(|value| dsp_filter_int(&value)),
            cutoff_hz: item("shaker_cutoff").and_then(|value| value.py_float_native().map_err(DspError::from_native)),
            q: item("shaker_resonance").and_then(|value| value.py_float_native().map_err(DspError::from_native)),
        }
    })
}

/// Where the shaker samples come from, as the shakers state reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActiveSamples {
    /// OpenRigCue's built-in sounds only (the default, [`SampleSource::BuiltIn`]).
    BuiltIn,
    /// At least one of SRS's recordings ([`SampleSource::SrsImport`]); the rest stay built-in.
    SrsImport,
}

/// The shakers part of the status: the output and which sounds play.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ShakersStatus {
    /// A sound card is selected and initialised (`initialized`, set by `set_soundcard`).
    pub output_ready: bool,
    /// The engine vibration is SRS's FMOD Studio events ([`EngineVoice::is_fmod`]); the UI shows the FMOD
    /// credit while it is true.
    pub is_fmod: bool,
    /// Where the samples come from.
    pub sample_source: ActiveSamples,
    /// How many of the played samples are SRS's recordings.
    pub imported_samples: usize,
    /// An SRS installation or installer was given, so "Use SRS Sound Samples" can take effect.
    pub srs_sounds_available: bool,
    /// The sound card refresh request: the sound device monitor saw the card list change; the UI main loop
    /// reloads the Hardware page's card list and selects `SHAKEBOX.soundcard` again (which runs
    /// `set_soundcard`), then clears it ([`Shakers::take_soundcard_refresh`]).
    pub refresh_soundcard: bool,
}

/// The samples and engine voice for one sound choice, built on the `shaker_sounds` worker thread without holding
/// any lock (an import out of the installer takes a while).
struct ShakerSounds {
    /// The SRS installation or installer they came from; `None` for the built-in sounds.
    from: Option<PathBuf>,
    bank: SampleBank,
    engine: EngineVoice,
    /// Where the shift beep's sound (`sounds/transmission/2.wav`) was read, when there is one.
    beep_file: Option<String>,
    /// The sound chosen for the beep ([`BeepSound::location`]); `None`: none (no hardware loops).
    beep_location: Option<String>,
    beep: Box<dyn BeepPlayer + Send>,
}

impl ShakerSounds {
    /// An SRS installation holds its samples and engine banks under `sounds/`. The
    /// samples of `from` are imported with [`SampleSource::SrsImport`] (each one it lacks keeps its built-in
    /// sound) and the engine is [`EngineVoice::from_srs`] (the built-in engine when FMOD or the banks are
    /// missing); `None` gives the built-in sounds and engine.
    ///
    /// The imported shift beep is `sounds/transmission/2.wav` and
    /// follows the same choice: read from `from` (any of the three forms the samples accept, see
    /// [`beep_sound`]); with the built-in sounds, or when that file cannot be read, it is [`builtin_beep`].
    /// It sounds only when `beep_allowed` (the service's hardware loops run).
    fn load(from: Option<PathBuf>, rate: u32, beep_allowed: bool) -> ShakerSounds {
        let source = from.clone().map_or(SampleSource::BuiltIn, SampleSource::SrsImport);
        let (bank, failures) = SampleBank::load(&source);
        for (key, error) in failures {
            match key {
                Some(key) => {
                    tracing::warn!(sample = %key.relative_path(), %error, "SRS sample not imported, using the built-in sound");
                }
                None => tracing::warn!(%error, "cannot read the SRS sounds, using the built-in sounds"),
            }
        }
        let engine = match &from {
            Some(path) => EngineVoice::from_srs(path, rate),
            None => EngineVoice::synth(rate),
        };
        let srs_beep = beep_sound(from.as_deref());
        let beep_file = srs_beep.as_ref().map(|sound| sound.location.clone());
        let (beep_location, beep) = shift_beep(srs_beep.unwrap_or_else(builtin_beep), beep_allowed);
        ShakerSounds { from, bank, engine, beep_file, beep_location, beep }
    }
}

/// The shift beep player for `sound` and the sound chosen for it ([`BeepSound::location`]); with
/// `allowed` false (no hardware loops) no sound is chosen and the beep is [`SilentBeep`]. A sound that cannot be
/// played (it does not decode) stays chosen with a [`SilentBeep`].
fn shift_beep(sound: BeepSound, allowed: bool) -> (Option<String>, Box<dyn BeepPlayer + Send>) {
    if !allowed {
        return (None, Box::new(SilentBeep));
    }
    let location = sound.location.clone();
    (Some(location), beep_player(sound).unwrap_or_else(|| Box::new(SilentBeep)))
}

/// How many of the played samples in `bank` are SRS's recordings.
fn imported_samples(bank: &SampleBank) -> usize {
    SampleKey::PLAYED.into_iter().filter(|key| bank.origin(*key) == Some(SampleOrigin::Imported)).count()
}

/// The open sound card (the stream's keeper thread); dropping it closes the card.
type OpenOutput = Box<dyn Send>;

/// A lost output is opened again no sooner than this after it opened, so that an output failing right after
/// each opening is not reopened in a tight loop.
const LOST_OUTPUT_MIN_LIFE: Duration = Duration::from_secs(2);
/// How many outputs in a row may be lost younger than [`LOST_OUTPUT_MIN_LIFE`] before reopening stops.
const LOST_OUTPUT_QUICK_RETRIES: u32 = 3;

/// The open card's lost-device flag ([`openrigcue_effects::shakers::output::Output::lost`]) and when it opened.
struct LostOutput {
    flag: Arc<AtomicBool>,
    opened: std::time::Instant,
    /// The loss was noticed before [`LOST_OUTPUT_MIN_LIFE`] and counted.
    quick: bool,
}

/// The shaker system, the open sound output and the shift beep, shared by the shaker loop and the API
/// (behind one lock).
pub struct Shakers {
    pub system: ShakerSystem,
    /// The open card; `Some` while `initialized`.
    output: Option<OpenOutput>,
    /// Tells when the open card's stream lost its device ([`Shakers::follow_lost_output`]).
    output_lost: Option<LostOutput>,
    /// Outputs lost in a row younger than [`LOST_OUTPUT_MIN_LIFE`].
    quick_losses: u32,
    beep: Box<dyn BeepPlayer + Send>,
    /// Configuration revision and profile generation the settings were last taken from.
    seen: (u64, u64),
    /// Whether a sound card may be opened at all (the service's hardware loops run).
    output_allowed: bool,
    /// The SRS installation or installer "Use SRS Sound Samples" reads from.
    srs_sounds: Option<PathBuf>,
    /// The SRS installation or installer the playing sounds came from; `None`: the built-in ones.
    sounds_from: Option<PathBuf>,
    /// [`imported_samples`] of the playing bank.
    imported_samples: usize,
    /// Where the SRS shift beep sound the beep plays was read ([`BeepSound::location`]), `None` while it is the
    /// one given to [`ShakersHandle::new`] or silent.
    beep_file: Option<String>,
    /// The sound chosen for the shift beep ([`BeepSound::location`]: an SRS file or `built-in`); `None` while
    /// it is the beep given to [`ShakersHandle::new`] or no sound may play (no hardware loops).
    beep_location: Option<String>,
    /// Whether [`ShakersHandle::follow_sound_choice`] has chosen the sounds once (the beep given to
    /// [`ShakersHandle::new`] plays only until then).
    sounds_chosen: bool,
    /// The `shaker_sounds` worker building the next sounds ([`ShakersHandle::follow_sound_choice`]).
    loader: Option<JoinHandle<()>>,
    /// The audio event of the sound device monitor while it is event-driven.
    device_event: Option<Arc<AudioEvent>>,
    /// The running sound device monitor's reselection flag ([`Shakers::set_device_monitor`]).
    monitor_reselect: Option<Arc<AtomicBool>>,
    /// Whether the UI page has been shown.
    app_loaded: bool,
    /// The sound card refresh request.
    refresh_soundcard: Arc<AtomicBool>,
    published: Shared<ShakersStatus>,
    engine_status: EngineStatus,
}

#[derive(Clone)]
pub struct ShakersHandle {
    processing: Arc<Mutex<Shakers>>,
    management: Arc<Mutex<()>>,
    refresh_soundcard: Arc<AtomicBool>,
    published: Shared<ShakersStatus>,
    engine_status: EngineStatus,
}

impl ShakersHandle {
    /// The shaker system with the current settings and no sound card open (not initialised until the UI's
    /// `on_soundcard_change` runs [`ShakersHandle::set_soundcard`]); `output_allowed` says whether a card may be
    /// opened at all. `bank` and `beep` are played until [`ShakersHandle::with_srs_sounds`] chooses otherwise (the
    /// service passes [`SilentBeep`] until the first sound load picks the SRS beep or [`builtin_beep`]).
    pub fn new(
        state: &ServiceState,
        bank: SampleBank,
        beep: Box<dyn BeepPlayer + Send>,
        output_allowed: bool,
    ) -> ShakersHandle {
        let imported_samples = imported_samples(&bank);
        let system = ShakerSystem::new_unbuilt(shaker_settings(state), bank, MIXER_RATE);
        let refresh_soundcard = Arc::new(AtomicBool::new(false));
        let engine_status = system.mixer().lock().unwrap_or_else(PoisonError::into_inner).engine_status();
        let published = Shared::new(ShakersStatus {
            output_ready: false,
            is_fmod: engine_status.is_fmod(),
            sample_source: if imported_samples > 0 { ActiveSamples::SrsImport } else { ActiveSamples::BuiltIn },
            imported_samples,
            srs_sounds_available: false,
            refresh_soundcard: false,
        });
        let mut shakers = Shakers {
            system,
            output: None,
            output_lost: None,
            quick_losses: 0,
            monitor_reselect: None,
            beep,
            seen: (u64::MAX, u64::MAX),
            output_allowed,
            srs_sounds: None,
            sounds_from: None,
            imported_samples,
            beep_file: None,
            beep_location: None,
            sounds_chosen: false,
            loader: None,
            device_event: None,
            app_loaded: false,
            refresh_soundcard: refresh_soundcard.clone(),
            published: published.clone(),
            engine_status: engine_status.clone(),
        };
        shakers.follow_settings(state);
        ShakersHandle {
            processing: Arc::new(Mutex::new(shakers)),
            management: Arc::new(Mutex::new(())),
            refresh_soundcard,
            published,
            engine_status,
        }
    }

    pub fn soundcard_refresh_requested(&self) -> bool {
        self.refresh_soundcard.load(Ordering::SeqCst)
    }

    pub fn request_soundcard_refresh(&self) {
        self.refresh_soundcard.store(true, Ordering::SeqCst);
    }

    pub fn clear_soundcard_refresh(&self) {
        self.refresh_soundcard.store(false, Ordering::SeqCst);
    }

    pub fn status(&self) -> ShakersStatus {
        let mut status = self.published.snapshot();
        status.is_fmod = self.engine_status.is_fmod();
        status.refresh_soundcard = self.soundcard_refresh_requested();
        status
    }

    /// Let "Use SRS Sound Samples" (`settings.shakers.use_srs_sounds`) read SRS's samples and engine banks from
    /// `srs_install` (an SRS installation, its `sounds` folder or the installer), and start applying the setting
    /// (an import runs in the background, see [`ShakersHandle::follow_sound_choice`], so the service start does
    /// not wait for it). Without a path the built-in sounds always play.
    pub fn with_srs_sounds(self, state: &ServiceState, srs_install: Option<PathBuf>) -> ShakersHandle {
        self.lock().set_srs_sounds(srs_install);
        self.follow_sound_choice(state);
        self
    }

    /// Switch samples and engine when "Use SRS Sound Samples" or the available SRS files changed. The new
    /// sounds are built on a `shaker_sounds` worker thread with the lock released and installed when ready, so
    /// neither the API, the shaker loop (which keeps playing the old sounds meanwhile) nor the service start waits
    /// for an import. One load runs at a time; a choice changed during it is followed by the next call after it
    /// finished. If the worker cannot be started, the sounds are built on the calling thread.
    pub fn follow_sound_choice(&self, state: &ServiceState) {
        let mut shakers = self.lock();
        if shakers.loader.as_ref().is_some_and(|loader| !loader.is_finished()) {
            return;
        }
        shakers.loader = None;
        let wanted = shakers.wanted_sounds(state);
        if !std::mem::replace(&mut shakers.sounds_chosen, true) && wanted.is_none() {
            // The built-in sounds: the bank given to `new` already is, the beep becomes the built-in one
            // (there is always a beep).
            let (location, beep) = shift_beep(builtin_beep(), shakers.output_allowed);
            shakers.install_beep(None, location, beep);
            return;
        }
        if wanted == shakers.sounds_from {
            return;
        }
        let rate = shakers.system.mixer().lock().unwrap_or_else(PoisonError::into_inner).sample_rate();
        let beep_allowed = shakers.output_allowed;
        let target = self.clone();
        let job = wanted.clone();
        let spawned = std::thread::Builder::new().name("shaker_sounds".into()).spawn(move || {
            let sounds = ShakerSounds::load(job, rate, beep_allowed);
            target.lock().install_sounds(sounds);
        });
        match spawned {
            Ok(loader) => shakers.loader = Some(loader),
            Err(error) => {
                tracing::warn!(%error, "cannot start the sound loader, loading on the shaker thread");
                drop(shakers);
                let sounds = ShakerSounds::load(wanted, rate, beep_allowed);
                self.lock().install_sounds(sounds);
            }
        }
    }

    /// Wait until a sound load started by [`ShakersHandle::follow_sound_choice`] is installed.
    #[cfg(test)]
    pub(crate) fn wait_for_sounds(&self) {
        let loader = self.lock().loader.take();
        if let Some(loader) = loader {
            loader.join().expect("sound loader");
        }
    }

    pub fn lock(&self) -> MutexGuard<'_, Shakers> {
        self.processing.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Called each time the UI page is shown: the page is marked as shown and the audio event is set, so the
    /// sound device monitor runs its notification check.
    pub fn app_loaded(&self) {
        let mut shakers = self.lock();
        shakers.app_loaded = true;
        tracing::debug!("check_device_changed: app_loaded received, triggering initial notification check");
        if let Some(event) = &shakers.device_event {
            event.set();
        }
    }

    /// Opens the selected sound card; called by the Hardware page's `on_soundcard_change` (after it stored the
    /// dropdown's value in `SHAKEBOX.soundcard`) on the UI thread, synchronously. With `'None'` the card is
    /// released. Otherwise the card is looked up in the driver list (`list_soundcards`)
    /// and opened with the channel count it is configured for, then the channel groups are
    /// rebuilt and the shakers are marked initialised. Both end with the device monitor's
    /// re-check event ([`Shakers::trigger_device_check`]). A card missing from the list
    /// or that cannot be opened is an error: the card stays closed,
    /// nothing is rebuilt, no event is set, and the handler shows `'None'` in the dropdown.
    pub fn set_soundcard(&self, state: &ServiceState) -> Result<(), SoundcardError> {
        let _management = self.management.lock().unwrap_or_else(PoisonError::into_inner);
        let lost = std::cell::RefCell::new(None);
        let open = |sound_card: &str, system: &ShakerSystem| {
            let configured = DeviceSources::system().configured_channels(sound_card);
            crate::audio::open_shaker_output(sound_card, system, configured).map(|(keeper, flag)| {
                *lost.borrow_mut() = Some(LostOutput { flag, opened: std::time::Instant::now(), quick: false });
                Box::new(keeper) as OpenOutput
            })
        };
        let selected = state.config.read().settings().shakers.sound_card.clone();
        let mut shakers = self.lock();
        let result = shakers.set_soundcard_with(
            state,
            &|| sound_devices(&selected).into_iter().map(|device| device.id).collect(),
            open,
        );
        shakers.output_lost = lost.take();
        result
    }

    /// Rebuilds the channel groups; called by the widget handlers that change an effect level or switch
    /// or a channel volume
    /// right after they wrote the tune: the channel groups are rebuilt from it, synchronously.
    pub fn load_channel_groups(&self, state: &ServiceState) -> Result<(), DspError> {
        self.rebuild_levels(state, None, |_| {})
    }

    fn rebuild_levels(
        &self,
        state: &ServiceState,
        focus: Option<Effect>,
        mut after: impl FnMut(Option<Group>),
    ) -> Result<(), DspError> {
        let _management = self.management.lock().unwrap_or_else(PoisonError::into_inner);
        let mut loaded_layout = state.shaker_layout.snapshot();
        {
            let mut shakers = self.lock();
            shakers.system.update_settings(shaker_settings(state));
            shakers.system.begin_level_rebuild();
        }
        after(None);
        for group in Group::ALL {
            {
                let mut shakers = self.lock();
                let current_layout = state.shaker_layout.snapshot();
                if current_layout != loaded_layout {
                    shakers.system.update_settings(shaker_settings(state));
                    loaded_layout = current_layout;
                } else {
                    shakers.follow_settings(state);
                }
                shakers.system.publish_level_group(group, focus)?;
            }
            after(Some(group));
        }
        self.lock().system.finish_level_rebuild();
        Ok(())
    }

    /// The surround recheck, called by the Hardware page's `trigger_events` after a new
    /// `SHAKEBOX.profile`: wakes the device monitor's surround check.
    pub fn trigger_surround_recheck(&self, state: &ServiceState) {
        self.lock().trigger_device_check(state, "trigger_surround_recheck: setting audio event");
    }
}

/// Why `set_soundcard` failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SoundcardError {
    /// The card is not in the driver list.
    #[error("soundcard \"{0}\" not found in driver list")]
    NotFound(String),
    /// The card could not be opened: the reason, which may be a sentence with values ([`crate::i18n::sentence`]).
    #[error("{}", crate::i18n::plain(.0))]
    Init(String),
}

impl SoundcardError {
    /// The failure as a sentence with its values, for the UI to show in the selected language.
    pub fn message(&self) -> String {
        match self {
            SoundcardError::NotFound(name) => {
                crate::i18n::sentence("soundcard \"^name^\" not found in driver list", &[("name", name)])
            }
            SoundcardError::Init(reason) => reason.clone(),
        }
    }
}

/// Mixer sample rate before an output dictates its own.
pub const MIXER_RATE: u32 = 48_000;

impl Shakers {
    /// Takes changed settings (`update_settings`) at each production effect/group boundary.
    /// Nothing is rebuilt and no card is opened here; that is done by the UI handlers' calls
    /// ([`ShakersHandle::load_channel_groups`], [`ShakersHandle::set_soundcard`]).
    fn follow_settings(&mut self, state: &ServiceState) {
        let seen = (state.config.read().revision(), state.profile.read().generation);
        if seen == self.seen {
            return;
        }
        self.seen = seen;
        self.system.update_settings(shaker_settings(state));
    }

    /// `set_soundcard` on the driver list `soundcards` and with `open` opening a card, see
    /// [`ShakersHandle::set_soundcard`].
    fn set_soundcard_with(
        &mut self,
        state: &ServiceState,
        soundcards: &dyn Fn() -> Vec<String>,
        open: impl FnOnce(&str, &ShakerSystem) -> Result<OpenOutput, String>,
    ) -> Result<(), SoundcardError> {
        let sound_card = state.config.read().settings().shakers.sound_card.clone();
        tracing::debug!("set_soundcard: starting, soundcard={sound_card}");
        // Joining the old output must precede the mixer lock: its callback can still be rendering.
        self.output = None;
        self.output_lost = None;
        self.published.write().output_ready = false;
        self.system.close_output(MIXER_RATE);
        if sound_card == "None" {
            tracing::debug!("set_soundcard: deactivating (soundcard=None)");
        } else {
            tracing::debug!("set_soundcard: activating soundcard={sound_card}");
            if !self.output_allowed {
                tracing::info!(%sound_card, "hardware loops disabled: the sound card is not opened");
                return Ok(());
            }
            self.system.reopen_engine();
            if !soundcards().contains(&sound_card) {
                tracing::error!("set_soundcard: soundcard \"{sound_card}\" not found in driver list — aborting");
                return Err(SoundcardError::NotFound(sound_card));
            }
            match open(&sound_card, &self.system) {
                Ok(output) => {
                    self.output = Some(output);
                    self.published.write().output_ready = true;
                }
                Err(error) => {
                    tracing::error!("set_soundcard: mixer1.init() failed: {}", crate::i18n::plain(&error));
                    return Err(SoundcardError::Init(error));
                }
            }
            tracing::debug!("set_soundcard: mixer1.init() OK");
            self.follow_settings(state);
            if let Err(error) = self.system.reload_levels() {
                tracing::warn!("set_soundcard: load_channel_groups failed: {}", error.message);
            }
            tracing::debug!("set_soundcard: completed successfully");
        }
        self.trigger_device_check(state, "set_soundcard: triggering audio event for surround re-check");
        Ok(())
    }

    /// Sets the device monitor's audio event, under the Windows rules and while the monitor is event-driven
    /// (end of `set_soundcard` and of the surround recheck).
    fn trigger_device_check(&self, state: &ServiceState, message: &str) {
        if !state.platform.is_windows() {
            return;
        }
        if let Some(event) = &self.device_event {
            tracing::debug!("{message}");
            event.set();
        }
    }

    /// Let the sound device monitor's audio event be woken by `set_soundcard` and the surround recheck
    /// (`Some` while the monitor is event-driven).
    pub fn set_device_event(&mut self, event: Option<Arc<AudioEvent>>) {
        self.device_event = event;
    }

    /// Whether the UI page has been shown ([`ShakersHandle::app_loaded`]).
    pub fn is_app_loaded(&self) -> bool {
        self.app_loaded
    }

    /// Requests a sound card refresh (the device monitor, after the card list changed).
    pub fn request_soundcard_refresh(&mut self) {
        self.refresh_soundcard.store(true, Ordering::SeqCst);
    }

    /// The UI main loop's take of the refresh request: whether
    /// the card list is to be reloaded, clearing the request.
    pub fn take_soundcard_refresh(&mut self) -> bool {
        self.refresh_soundcard.swap(false, Ordering::SeqCst)
    }

    /// The open card's stream cannot reach its device any more (the output was removed or reconfigured, or
    /// the sound server went away): asks for the card list to be loaded again, as a changed list does.
    /// The UI main loop then selects `SHAKEBOX.soundcard` again,
    /// which opens the card anew or shows `'None'` while it is missing.
    /// The stream is pinned to its endpoint and stays silent until it is opened again.
    /// While the sound device monitor runs the request goes through it ([`Shakers::set_device_monitor`]): the
    /// same event usually changes the card list too, and its pass then asks once for both.
    /// An output lost younger than [`LOST_OUTPUT_MIN_LIFE`] is opened again only once it would have reached
    /// that age, and not at all after [`LOST_OUTPUT_QUICK_RETRIES`] such losses in a row.
    /// Returns whether the request was made.
    pub fn follow_lost_output(&mut self) -> bool {
        let Some(lost) = self.output_lost.as_mut() else { return false };
        let young = lost.opened.elapsed() < LOST_OUTPUT_MIN_LIFE;
        if !lost.flag.load(Ordering::SeqCst) {
            if !young {
                self.quick_losses = 0;
            }
            return false;
        }
        if young && !lost.quick {
            lost.quick = true;
            self.quick_losses += 1;
            if self.quick_losses > LOST_OUTPUT_QUICK_RETRIES {
                tracing::error!("the shaker sound device is lost right after every opening; not reopening it");
                self.output_lost = None;
                return false;
            }
        }
        if young {
            return false;
        }
        if !lost.quick {
            self.quick_losses = 0;
        }
        self.output_lost = None;
        tracing::warn!("the shaker sound device was lost; asking for the sound card to be selected again");
        match &self.monitor_reselect {
            Some(reselect) => {
                reselect.store(true, Ordering::SeqCst);
                if let Some(event) = &self.device_event {
                    event.set();
                }
            }
            None => self.request_soundcard_refresh(),
        }
        true
    }

    /// The running sound device monitor's reselection flag, `None` while no monitor runs. A lost output
    /// raises it (and the audio event) instead of asking for the reload itself.
    pub fn set_device_monitor(&mut self, reselect: Option<Arc<AtomicBool>>) {
        self.monitor_reselect = reselect;
    }

    /// Whether a sound card is selected and open.
    pub fn output_ready(&self) -> bool {
        self.output.is_some()
    }

    /// The SRS files to play from: the configured installation or installer while "Use SRS Sound Samples" is
    /// on, else none (the built-in sounds).
    fn wanted_sounds(&self, state: &ServiceState) -> Option<PathBuf> {
        let use_srs = state.config.read().settings().shakers.use_srs_sounds;
        self.srs_sounds.clone().filter(|_| use_srs)
    }

    /// Play `sounds` from now on: voices already sounding keep their sample ([`Mixer::set_bank`]), the engine
    /// voice is switched to the mixer's rate. A looping shift beep stops with its old player; the new one starts
    /// at the next beep edge.
    ///
    /// [`Mixer::set_bank`]: openrigcue_effects::shakers::mixer::Mixer::set_bank
    fn install_sounds(&mut self, sounds: ShakerSounds) {
        self.imported_samples = imported_samples(&sounds.bank);
        {
            let mut status = self.published.write();
            status.imported_samples = self.imported_samples;
            status.sample_source =
                if self.imported_samples > 0 { ActiveSamples::SrsImport } else { ActiveSamples::BuiltIn };
        }
        self.sounds_from = sounds.from;
        self.install_beep(sounds.beep_file, sounds.beep_location, sounds.beep);
        self.system.mixer().lock().unwrap_or_else(PoisonError::into_inner).set_bank(sounds.bank);
        self.system.set_engine(sounds.engine);
    }

    /// Play the shift beep with `beep` from now on; a looping beep stops with its old player.
    fn install_beep(
        &mut self,
        beep_file: Option<String>,
        beep_location: Option<String>,
        beep: Box<dyn BeepPlayer + Send>,
    ) {
        self.beep.apply(BeepCommand::Stop);
        self.beep = beep;
        self.beep_file = beep_file;
        self.beep_location = beep_location;
    }

    /// The sound chosen for the shift beep ([`BeepSound::location`]: an SRS file or `built-in`); `None` while
    /// it is the beep given to [`ShakersHandle::new`] or no sound may play (no hardware loops). It sounds
    /// through the default output.
    pub fn beep_location(&self) -> Option<&str> {
        self.beep_location.as_deref()
    }

    /// Where the SRS sound the shift beep plays (`sounds/transmission/2.wav`) was read
    /// ([`BeepSound::location`]); `None`: no SRS sounds chosen or the file is missing.
    pub fn beep_file(&self) -> Option<&str> {
        self.beep_file.as_deref()
    }

    /// The shakers part of the API status.
    pub fn status(&self) -> ShakersStatus {
        let is_fmod = self.engine_status.is_fmod();
        ShakersStatus {
            output_ready: self.output_ready(),
            is_fmod,
            sample_source: if self.imported_samples > 0 { ActiveSamples::SrsImport } else { ActiveSamples::BuiltIn },
            imported_samples: self.imported_samples,
            srs_sounds_available: self.srs_sounds.is_some(),
            refresh_soundcard: self.refresh_soundcard.load(Ordering::SeqCst),
        }
    }

    fn set_srs_sounds(&mut self, path: Option<PathBuf>) {
        self.srs_sounds = path;
        self.published.write().srs_sounds_available = self.srs_sounds.is_some();
    }

    fn frame_state(&self, state: &ServiceState) -> FrameState {
        FrameState {
            output_ready: self.output_ready(),
            game_running: state.session.read().listener.is_some(),
            switch_on: state.controls.read().switches.shakers,
        }
    }
}

/// The driver list: `'None'` first, then every output device of the audio host
/// (a device whose name cannot be read is `'Unknown'`, which the Hardware page's `load_soundcard`
/// skips).
pub fn list_soundcards() -> Vec<String> {
    let mut soundcards = vec!["None".to_owned()];
    match openrigcue_effects::shakers::output::device_names() {
        Ok(names) => soundcards.extend(names),
        Err(error) => tracing::warn!("list_soundcards: num_drivers failed: {error}"),
    }
    tracing::debug!("list_soundcards: result={soundcards:?}");
    soundcards
}

/// The sound cards the Hardware page loads (`load_soundcard` reads [`list_soundcards`]).
pub fn sound_cards() -> Vec<String> {
    list_soundcards()
}

/// Hardware picker descriptors. Preserve a stored older selection explicitly rather
/// than guessing which physical endpoint an old default/front/surround alias meant.
pub fn sound_devices(selected: &str) -> Vec<openrigcue_effects::shakers::output::OutputDevice> {
    use openrigcue_effects::shakers::output::{self, OutputDevice};
    let mut devices = vec![OutputDevice { id: "None".into(), name: "None".into() }];
    match output::devices() {
        Ok(outputs) => devices.extend(outputs),
        Err(error) => tracing::warn!(%error, "cannot enumerate sound devices"),
    }
    retain_selected_output(&mut devices, selected);
    devices
}

fn retain_selected_output(devices: &mut Vec<openrigcue_effects::shakers::output::OutputDevice>, selected: &str) {
    use openrigcue_effects::shakers::output::{self, OutputDevice};
    if selected != "None" && !devices.iter().any(|device| device.id == selected) {
        devices.push(OutputDevice { id: selected.into(), name: output::legacy_device_label(selected) });
    }
}

fn run_live_shaker_frame(
    state: &ServiceState,
    shakers: &ShakersHandle,
    telemetry: &openrigcue_model::Telemetry,
    test: Option<&openrigcue_effects::shakers::effect_test::TestFrame>,
    mut after: impl FnMut(openrigcue_effects::shakers::rig::FrameStage),
) -> Result<(), openrigcue_effects::shakers::engine_sound::EngineError> {
    let mut progress = {
        let mut guard = shakers.lock();
        guard.follow_settings(state);
        let status = guard.frame_state(state);
        match test {
            Some(frame) => ShakerSystem::begin_live_test(frame, status),
            None => ShakerSystem::begin_live_frame(telemetry, status),
        }
    };
    for stage in progress.stages() {
        {
            let mut guard = shakers.lock();
            guard.follow_settings(state);
            guard.system.advance_live_frame(&mut progress, stage)?;
        }
        after(stage);
    }
    Ok(())
}

/// The shift beep's inputs, read from the session, the configuration, then the session again.
///
/// Each value is read in a statement of its own, so each guard is released before the next lock is
/// taken. Read inside one struct expression, the guards would all live to the end of that statement:
/// the session read lock would still be held while the configuration lock and then the session lock
/// were taken again. A session writer (the telemetry main loop) queued between the two session reads
/// then blocks the second one while it waits for the first, and every later configuration writer
/// (the UI) waits behind the configuration read lock held meanwhile.
fn beep_inputs(state: &ServiceState) -> BeepInputs {
    let listener_running = state.session.read().listener.is_some();
    let shift_sound = state.config.read().settings().app.gear_shift_sound;
    let shift_light = f64::from(u8::from(state.session.read().shift_light));
    BeepInputs { listener_running, shift_sound, shift_light }
}

/// One shaker loop iteration: a shake frame on the current telemetry, then the shift beep. Returns the
/// pause before the next one (`None` ends the loop).
pub fn shaker_pass(state: &ServiceState, shakers: &ShakersHandle, accessories: &Accessories) -> Option<Duration> {
    // The sample-import option can be changed from the UI before any telemetry arrives.
    shakers.follow_sound_choice(state);
    shakers.lock().follow_lost_output();
    // Wait for the telemetry's `spotter_right` key before frame/settings/beep work.
    if !*state.telemetry_ready.read() {
        return Some(Duration::from_millis(500));
    }
    let telemetry = state.telemetry.snapshot();
    if let Err(error) = run_live_shaker_frame(state, shakers, &telemetry, None, |_| {}) {
        // A failed frame is logged with its message and call trace. When the message itself cannot
        // be rendered, the loop ends with that error: no beep step, no sleep, no next frame.
        if let Some(failure) = error.str_failure() {
            tracing::error!("shaker loop stopped: {}: {}", failure.class, failure.message);
            return None;
        }
        tracing::error!("error sending data to shakebox: {error}");
    }
    let mut guard = shakers.lock();
    let beep = beep_inputs(state);
    accessories.shift_beep_cycle(&beep, guard.beep.as_mut());
    drop(guard);
    refresh_period(i64::from(state.config.read().settings().shakers.refresh_rate))
}

/// Whether the Shake page "Test" button runs the effect test (a sound card is selected,
/// whether or not the card could be opened).
pub fn effect_test_allowed(state: &ServiceState) -> bool {
    state.config.read().settings().shakers.sound_card != "None"
}

/// Whether the Hardware page speaker "Test" button runs the speaker test (a sound card is selected,
/// whether or not the card could be opened).
pub fn speaker_test_allowed(state: &ServiceState) -> bool {
    state.config.read().settings().shakers.sound_card != "None"
}

/// The Hardware page speaker "Test" button: transmission/1 once on `speaker`.
pub fn test_sound(state: &ServiceState, shakers: &ShakersHandle, speaker: Speaker) -> Result<(), DspError> {
    let _management = shakers.management.lock().unwrap_or_else(PoisonError::into_inner);
    let mut shakers = shakers.lock();
    let settings = shaker_settings(state);
    shakers.system.update_settings(settings);
    shakers.system.test_speaker(speaker)
}

/// The Shake page "Test" button of an effect: the effect's groups alone, the 13 test frames
/// with 12 sleeps of `TEST_FRAME_GAP` between them, then the normal groups again.
/// `test_listener` is the running listener, `'test'` when none runs; it is stored in the shared
/// session listener (`state.session.listener`) right before each of frames 1-12, and the listener the test
/// started with is stored back after the 12th. Every reader of the session's listener sees it (the shaker loop's
/// telemetry frames, the shift beep, the other loops) until the telemetry thread publishes its own listener over
/// it. The shaker loop keeps running meanwhile. Runs on the
/// calling thread (the UI handler blocks for about 0.8 s). A frame whose engine block fails ends the test there
/// (no further frame, store or rebuild).
pub fn test_effect(state: &ServiceState, shakers: &ShakersHandle, effect: Effect) -> Result<(), ShakerTestError> {
    let current = state.session.read().listener.clone();
    let test = Some(current.clone().unwrap_or_else(|| "test".to_owned()));
    test_effect_with_listeners(state, shakers, effect, current, test)
}

#[derive(Debug, thiserror::Error)]
pub enum ShakerTestError {
    #[error("{}: {}", error.class, error.message)]
    Native { error: DspError },
    #[error(transparent)]
    Frame(#[from] openrigcue_effects::shakers::engine_sound::EngineError),
}

impl From<DspError> for ShakerTestError {
    fn from(error: DspError) -> Self {
        Self::Native { error }
    }
}

impl ShakerTestError {
    pub fn detail(&self) -> String {
        match self {
            Self::Native { error } => format!("{}: {}", error.class, error.message),
            Self::Frame(error) => error
                .native_error()
                .map_or_else(|| error.to_string(), |error| format!("{}: {}", error.class, error.message)),
        }
    }
}

/// The UI passes its two independently captured listener arguments; this helper never recaptures them.
pub fn test_effect_with_listeners(
    state: &ServiceState,
    shakers: &ShakersHandle,
    effect: Effect,
    current: Option<String>,
    test: Option<String>,
) -> Result<(), ShakerTestError> {
    test_effect_with_sleeper(state, shakers, effect, current, test, std::thread::sleep)
}

fn test_effect_with_sleeper(
    state: &ServiceState,
    shakers: &ShakersHandle,
    effect: Effect,
    current: Option<String>,
    test: Option<String>,
    mut sleep: impl FnMut(Duration),
) -> Result<(), ShakerTestError> {
    shakers.rebuild_levels(state, Some(effect), |_| {})?;
    let frames = effect_test_frames();
    for (n, frame) in frames.iter().enumerate() {
        if frame.forced_active {
            state.session.write().listener.clone_from(&test);
        }
        let raised = run_live_shaker_frame(state, shakers, &openrigcue_model::Telemetry::off(), Some(frame), |_| {});
        raised?;
        if frame.forced_active && !frame.forced_after {
            state.session.write().listener.clone_from(&current);
        }
        if n + 1 < frames.len() {
            sleep(TEST_FRAME_GAP);
        }
    }
    shakers.rebuild_levels(state, None, |_| {}).map_err(Into::into)
}

/// The running shaker loop.
pub struct ShakerThread {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

impl ShakerThread {
    /// Starts the shaker loop thread.
    pub fn start(
        state: ServiceState,
        shakers: ShakersHandle,
        accessories: Arc<Accessories>,
    ) -> std::io::Result<ShakerThread> {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = std::thread::Builder::new().name("thread_shakebox".into()).spawn(move || {
            while !flag.load(Ordering::SeqCst) {
                let Some(pause) = shaker_pass(&state, &shakers, &accessories) else {
                    tracing::error!("shaker loop stopped: SHAKEBOX.refresh_rate gives no valid period");
                    return;
                };
                std::thread::sleep(pause);
            }
        })?;
        Ok(ShakerThread { stop, handle })
    }

    pub fn stop(self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.handle.join();
    }
}

/// The shift beep's sound file read from an SRS source.
pub struct BeepSound {
    /// The file (`<sounds>/transmission/2.wav`) or `<installer>!sounds/transmission/2.wav`.
    pub location: String,
    pub bytes: Vec<u8>,
}

/// The imported shift beep sound, `sounds/transmission/2.wav`, read from the same
/// three forms of SRS path as the samples (`SampleSource::SrsImport`): the installer (read out of its NSIS
/// archive), an install directory holding `sounds/`, or the `sounds` directory itself. `None` when there is no
/// path or the file cannot be read.
pub fn beep_sound(srs_install: Option<&Path>) -> Option<BeepSound> {
    const IN_SOUNDS: [&str; 2] = ["transmission", "2.wav"];
    let path = srs_install?;
    if path.is_file() {
        let inner = format!("sounds/{}", IN_SOUNDS.join("/"));
        let read = std::fs::read(path)
            .map_err(|e| e.to_string())
            .and_then(|bytes| NsisArchive::open(bytes).map_err(|e| e.to_string()))
            .and_then(|archive| archive.read(&inner).map_err(|e| e.to_string()));
        return match read {
            Ok(bytes) => Some(BeepSound { location: format!("{}!{inner}", path.display()), bytes }),
            Err(error) => {
                tracing::warn!(installer = %path.display(), %error, "no shift beep sound in the SRS installer");
                None
            }
        };
    }
    let nested = path.join("sounds");
    let sounds = if nested.is_dir() { nested } else { path.to_path_buf() };
    let file = IN_SOUNDS.iter().fold(sounds, |p, part| p.join(part));
    let bytes = std::fs::read(&file).ok()?;
    Some(BeepSound { location: file.display().to_string(), bytes })
}

/// The built-in shift beep, synthesized: 44.1 kHz 16-bit stereo, 43968 frames (0.99701 s). 78 silent samples,
/// then a square wave of 463.1 Hz, plateau 0.1308 of
/// full scale, 23 cycles (46 half periods of 47.6 samples, to sample 2268, about 50 ms), then silence to the end
/// of the 43968 frames, so a looped beep sounds every 0.99701 s. Band-limited to the odd
/// harmonics below 22.05 kHz. Both channels are the same. Encoded as a WAV file
/// so it plays through the same path as an imported beep file.
pub fn builtin_beep() -> BeepSound {
    const RATE: u32 = 44_100;
    const FREQUENCY: f64 = 463.1;
    const PLATEAU: f64 = 0.1308;
    const START: usize = 78;
    const CYCLES: f64 = 23.0;
    /// Frames of the beep (a `data` chunk of 0x2af00 bytes / 4).
    const FRAMES: usize = 43_968;
    let frames = FRAMES;
    let tone = (CYCLES * f64::from(RATE) / FREQUENCY).round() as usize;
    let harmonics: Vec<f64> =
        (1..).step_by(2).map(f64::from).take_while(|k| k * FREQUENCY < f64::from(RATE) / 2.0).collect();
    let mut pcm = Vec::with_capacity(frames * 4);
    for n in 0..frames {
        let value = if (START..START + tone).contains(&n) {
            let phase = std::f64::consts::TAU * FREQUENCY * (n - START) as f64 / f64::from(RATE);
            let square: f64 = harmonics.iter().map(|k| (k * phase).sin() / k).sum();
            PLATEAU * 4.0 / std::f64::consts::PI * square
        } else {
            0.0
        };
        let bytes = ((value * f64::from(i16::MAX)).round() as i16).to_le_bytes();
        pcm.extend_from_slice(&bytes);
        pcm.extend_from_slice(&bytes);
    }
    BeepSound { location: "built-in".to_string(), bytes: wav_pcm16_stereo(RATE, &pcm) }
}

/// A canonical 16-bit PCM stereo WAV file around `pcm` (interleaved little-endian samples).
fn wav_pcm16_stereo(rate: u32, pcm: &[u8]) -> Vec<u8> {
    let data_len = u32::try_from(pcm.len()).unwrap_or(u32::MAX);
    let mut wav = Vec::with_capacity(44 + pcm.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&rate.to_le_bytes());
    wav.extend_from_slice(&(rate * 4).to_le_bytes());
    wav.extend_from_slice(&4u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend_from_slice(pcm);
    wav
}

/// A shift beep that cannot sound: no hardware loops, or a sound that does not decode.
pub struct SilentBeep;

impl BeepPlayer for SilentBeep {
    fn apply(&mut self, command: BeepCommand) {
        tracing::debug!(?command, "shift beep (no beep sound available)");
    }
}

/// The shift beep through the default sound device, looped asynchronously;
/// `None` when `sound` does not decode.
pub fn beep_player(sound: BeepSound) -> Option<Box<dyn BeepPlayer + Send>> {
    let player = crate::audio::LoopingBeep::decode(&sound.bytes, &sound.location);
    if player.is_none() {
        tracing::warn!(sound = %sound.location, "shift beep sound does not decode; the beep stays silent");
    }
    player.map(|player| Box::new(player) as Box<dyn BeepPlayer + Send>)
}

#[cfg(test)]
pub(crate) mod tests {
    #[test]
    fn sound_device_identity_survives_native_ini_and_legacy_default_is_explicit() {
        use openrigcue_effects::shakers::output::OutputDevice;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.ini");
        let mut settings = openrigcue_config::settings::Settings::default();
        let selected = "pulse:alsa_output.usb-Generic_USB_Audio-00.HiFi__SPDIF__sink";
        settings.edit_native(|raw| {
            raw.get_mut("SHAKEBOX").unwrap().as_dict_mut().unwrap().insert("soundcard", selected.into());
        });
        settings.write_ini_only(&path).unwrap();
        let loaded = openrigcue_config::settings::Settings::load_ini_only(&path).unwrap();
        assert_eq!(loaded.shakers.sound_card, selected);
        assert_eq!(
            loaded.native_snapshot().get("SHAKEBOX").unwrap().item("soundcard").unwrap().as_str(),
            Some(selected)
        );
        let mut devices = vec![OutputDevice { id: "None".into(), name: "None".into() }];
        super::retain_selected_output(&mut devices, "sysdefault");
        assert_eq!(devices[1].id, "sysdefault");
        assert_eq!(devices[1].name, "System default output (previous selection)");
        super::retain_selected_output(&mut devices, "sysdefault");
        assert_eq!(devices.len(), 2);
    }

    use super::*;

    /// The stored tune as an owned tree the tests can edit with `as_dict_mut()`: object
    /// handles are resolved recursively, so an edit
    /// reaches the profile only through its later `replace_native_tune`.
    fn detached(value: openrigcue_model::native_value::LegacyValue) -> openrigcue_model::native_value::LegacyValue {
        use openrigcue_model::native_value::{Dict, LegacyValue};
        match value {
            LegacyValue::Object(object) => detached(object.resolved()),
            LegacyValue::Dict(dict) => {
                let mut out = Dict::new();
                for (key, value) in dict.iter_native() {
                    out.insert_native(detached(key), detached(value.clone())).expect("hashable key");
                }
                LegacyValue::Dict(out)
            }
            LegacyValue::List(values) => LegacyValue::List(values.into_iter().map(detached).collect()),
            LegacyValue::Tuple(values) => LegacyValue::Tuple(values.into_iter().map(detached).collect()),
            value => value,
        }
    }

    fn detached_native_tune(profile: &crate::profile::ActiveProfile) -> openrigcue_model::native_value::LegacyValue {
        detached(profile.native_tune())
    }

    #[test]
    fn native_nonfinite_dsp_tune_parameters_reach_the_typed_frame_settings() {
        use openrigcue_config::import::value::LegacyValue;
        use openrigcue_config::tunes::FactoryTunes;

        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, f64::MAX] {
            let tune = FactoryTunes::builtin().get("emulator").unwrap();
            let mut native = LegacyValue::from(tune.to_json());
            for key in ["shaker_cutoff", "shaker_resonance"] {
                native
                    .as_dict_mut()
                    .unwrap()
                    .get_mut(key)
                    .unwrap()
                    .as_dict_mut()
                    .unwrap()
                    .insert("engine", LegacyValue::Float(value));
            }
            let tune = Tune::from_legacy(&native).unwrap();
            let tuning = shaker_tuning(&tune);
            let engine = tuning.effect(Effect::Engine);
            assert_eq!(engine.cutoff_hz.to_bits(), value.to_bits());
            assert_eq!(engine.resonance.to_bits(), value.to_bits());
        }
    }

    /// The built-in beep's shape: a 43968-frame loop period, silent for 78
    /// samples, a 463 Hz square wave (46 sign changes) at a 0.13 plateau (tone RMS 0.130) for 23
    /// cycles, silent afterwards.
    #[test]
    fn builtin_beep_matches_measured_srs_beep() {
        let beep = builtin_beep();
        let bytes = &beep.bytes;
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(bytes[24..28].try_into().unwrap()), 44_100);
        assert_eq!(u16::from_le_bytes(bytes[22..24].try_into().unwrap()), 2);
        let frames: Vec<[i16; 2]> = bytes[44..]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| [i16::from_le_bytes([c[0], c[1]]), i16::from_le_bytes([c[2], c[3]])])
            .collect();
        assert_eq!(frames.len(), 43_968, "SRS's loop period: 175872 data bytes");
        assert!(frames.iter().all(|f| f[0] == f[1]), "both channels the same");
        let samples: Vec<f64> = frames.iter().map(|f| f64::from(f[0]) / f64::from(i16::MAX)).collect();
        let tone_end = 78 + (23.0 * 44_100.0 / 463.1_f64).round() as usize;
        assert!(samples[..78].iter().all(|v| *v == 0.0), "silent before the tone");
        assert!(samples[tone_end..].iter().all(|v| *v == 0.0), "silent after 23 cycles");
        let tone = &samples[78..tone_end];
        let rms = (tone.iter().map(|v| v * v).sum::<f64>() / tone.len() as f64).sqrt();
        assert!((rms - 0.130).abs() < 0.003, "rms {rms}");
        let plateau = tone.iter().filter(|v| v.abs() > 0.05).map(|v| v.abs()).sum::<f64>()
            / tone.iter().filter(|v| v.abs() > 0.05).count() as f64;
        assert!((plateau - 0.1308).abs() < 0.003, "plateau {plateau}");
        let loud: Vec<f64> = tone.iter().copied().filter(|v| v.abs() > 0.05).collect();
        let changes = loud.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count();
        assert_eq!(changes, 45, "46 half periods");
    }
    /// The built-in beep decodes into the default-device player (nothing is opened until a loop starts).
    #[test]
    fn builtin_beep_has_a_player() {
        assert!(beep_player(builtin_beep()).is_some());
    }

    use crate::state::OriginalPlatform;
    use crate::store::ConfigStore;
    use openrigcue_config::settings::Configuration;
    use openrigcue_config::tunes::FactoryTunes;

    fn state() -> ServiceState {
        ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows)
    }

    fn dsp_native_profile(row: openrigcue_model::native_value::LegacyValue) -> ServiceState {
        let state = state();
        state.config.write().update(|config| config.profiles.set_native_game("ac", row));
        let active = crate::profile::ActiveProfile::load_native(
            &state.config.read(),
            "ac",
            &openrigcue_model::native_value::LegacyValue::Int(0),
            1,
        )
        .unwrap();
        *state.profile.write() = active;
        state
    }

    #[test]
    fn native_traction_max_profile_failure_occurs_after_earlier_effects() {
        use openrigcue_effects::shakers::rig::Voice;
        use openrigcue_model::native_value::LegacyValue;
        let mut row = LegacyValue::from(FactoryTunes::builtin().get("ac").unwrap().to_json());
        row.as_dict_mut()
            .unwrap()
            .get_mut("max_telemetry_vars")
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .insert("traction_loss", "2".into());
        let state = dsp_native_profile(row);
        let mut system = ShakerSystem::new(shaker_settings(&state), SampleBank::synthesized(), MIXER_RATE);
        let mut telemetry = openrigcue_model::Telemetry::off();
        telemetry.harmonized_gear = 2.0;
        telemetry.longitudinal_acceleration = -55.0;
        telemetry.lateral_acceleration = 12.0;
        telemetry.lateral_velocity = 1.0;
        telemetry.wheel_slip_lf = 12.0;
        let result = system.frame(&telemetry, FrameState { output_ready: true, game_running: true, switch_on: true });
        assert!(result.is_err(), "native max text must fail at division instead of using the typed fallback");
        assert_eq!(
            result.unwrap_err().native_error().unwrap().message,
            "unsupported operand type(s) for /: 'float' and 'str'"
        );
        let mixer = system.mixer();
        let mixer = mixer.lock().unwrap();
        assert!(mixer.is_playing(Voice::Transmission));
        assert!(mixer.is_playing(openrigcue_effects::shakers::rig::Voice::Impact));
        assert!(mixer.is_playing(Voice::Lateral));
        assert!(!mixer.is_playing(Voice::TractionLoss));
        assert!(!mixer.is_playing(Voice::WheelSlipLf));
        assert!(!mixer.engine_playing());
    }

    #[test]
    fn native_traction_max_required_profile_reads_and_numeric_kinds() {
        use openrigcue_model::native_value::LegacyValue;
        let frame = FrameState { output_ready: true, game_running: true, switch_on: true };
        for (kind, expected) in [
            (0, "'max_telemetry_vars'"),
            (1, "'traction_loss'"),
            (2, "'NoneType' object is not subscriptable"),
            (3, "unsupported operand type(s) for *: 'int' and 'NoneType'"),
        ] {
            let mut row = LegacyValue::from(FactoryTunes::builtin().get("ac").unwrap().to_json());
            match kind {
                0 => {
                    row.as_dict_mut().unwrap().remove("max_telemetry_vars");
                }
                1 => {
                    row.as_dict_mut()
                        .unwrap()
                        .get_mut("max_telemetry_vars")
                        .unwrap()
                        .as_dict_mut()
                        .unwrap()
                        .remove("traction_loss");
                }
                2 => {
                    row.as_dict_mut().unwrap().insert("max_telemetry_vars", LegacyValue::None);
                }
                _ => {
                    row.as_dict_mut()
                        .unwrap()
                        .get_mut("max_telemetry_vars")
                        .unwrap()
                        .as_dict_mut()
                        .unwrap()
                        .insert("traction_loss", LegacyValue::None);
                }
            }
            let state = dsp_native_profile(row);
            let mut system = ShakerSystem::new(shaker_settings(&state), SampleBank::synthesized(), MIXER_RATE);
            let error = system.frame(&openrigcue_model::Telemetry::off(), frame).unwrap_err();
            assert_eq!(error.native_error().unwrap().message, expected);
        }
        for value in [LegacyValue::Int(0), LegacyValue::Float(-0.0), LegacyValue::Bool(false), LegacyValue::Int(3)] {
            let mut row = LegacyValue::from(FactoryTunes::builtin().get("ac").unwrap().to_json());
            row.as_dict_mut()
                .unwrap()
                .get_mut("max_telemetry_vars")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .insert("traction_loss", value);
            let state = dsp_native_profile(row);
            let mut system = ShakerSystem::new(shaker_settings(&state), SampleBank::synthesized(), MIXER_RATE);
            system.frame(&openrigcue_model::Telemetry::off(), frame).unwrap();
        }
    }

    #[test]
    fn native_traction_max_keeps_live_numerator_kind_and_zero_bypass() {
        use openrigcue_model::native_value::LegacyValue;
        for (maximum, velocity, expected) in [
            (LegacyValue::from("2"), LegacyValue::Int(1), Some("unsupported operand type(s) for /: 'int' and 'str'")),
            (LegacyValue::Int(2), LegacyValue::None, Some("unsupported operand type(s) for /: 'NoneType' and 'int'")),
            (LegacyValue::Int(0), LegacyValue::None, None),
        ] {
            let mut row = LegacyValue::from(FactoryTunes::builtin().get("ac").unwrap().to_json());
            row.as_dict_mut()
                .unwrap()
                .get_mut("max_telemetry_vars")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .insert("traction_loss", maximum);
            let state = dsp_native_profile(row);
            let mut system = ShakerSystem::new(shaker_settings(&state), SampleBank::synthesized(), MIXER_RATE);
            let mut telemetry = openrigcue_model::Telemetry::off();
            telemetry.set_native("lateral_velocity", velocity);
            let result =
                system.frame(&telemetry, FrameState { output_ready: true, game_running: true, switch_on: true });
            match expected {
                Some(expected) => assert_eq!(result.unwrap_err().native_error().unwrap().message, expected),
                None => result.unwrap(),
            }
        }
    }

    #[test]
    fn native_traction_max_test_sfx_error_keeps_listener_and_preceding_sound_prefix() {
        use openrigcue_effects::shakers::rig::Voice;
        use openrigcue_model::native_value::LegacyValue;
        let mut row = LegacyValue::from(FactoryTunes::builtin().get("ac").unwrap().to_json());
        row.as_dict_mut()
            .unwrap()
            .get_mut("max_telemetry_vars")
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .insert("traction_loss", LegacyValue::None);
        let state = dsp_native_profile(row);
        let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), true);
        state.config.write().update(|configuration| configuration.settings.shakers.sound_card = "fake-card".into());
        shakers.lock().set_soundcard_with(&state, &|| vec!["fake-card".into()], opened).unwrap();
        let mut sleeps = 0;
        let error = test_effect_with_sleeper(
            &state,
            &shakers,
            Effect::TractionLoss,
            Some("old".into()),
            Some("test".into()),
            |_| sleeps += 1,
        )
        .unwrap_err();
        assert_eq!(error.detail(), "TypeError: unsupported operand type(s) for *: 'int' and 'NoneType'");
        assert_eq!(sleeps, 0);
        assert_eq!(state.session.read().listener.as_deref(), Some("test"));
        let guard = shakers.lock();
        let mixer = guard.system.mixer();
        let mixer = mixer.lock().unwrap();
        assert!(mixer.is_playing(Voice::Transmission));
        assert!(mixer.is_playing(Voice::Lateral));
        assert!(!mixer.is_playing(Voice::WheelSlipLf));
        assert!(!mixer.engine_playing());
    }

    #[test]
    fn native_traction_max_keeps_nonfactory_value_when_a_sibling_loses_projection() {
        use openrigcue_effects::shakers::levels::GroupLevels;
        use openrigcue_effects::shakers::rig::{Action, FrameContext, RigStatus, ShakeInput, ShakeRig, Voice};
        use openrigcue_model::native_value::LegacyValue;
        let mut row = LegacyValue::from(FactoryTunes::builtin().get("ac").unwrap().to_json());
        let maxima = row.as_dict_mut().unwrap().get_mut("max_telemetry_vars").unwrap().as_dict_mut().unwrap();
        maxima.insert("traction_loss", LegacyValue::Int(20));
        maxima.insert("pitch", LegacyValue::from("bad sibling"));
        let state = dsp_native_profile(row);
        let settings = shaker_settings(&state);
        assert_eq!(settings.tuning.native_traction_max, Some(Ok(LegacyValue::Int(20))));
        let profile = settings.profile.as_ref().unwrap();
        let levels = GroupLevels::compute(profile, &settings.tuning, None);
        let context = FrameContext {
            status: RigStatus::Active,
            tuning: &settings.tuning,
            config: &settings.config,
            levels: &levels,
            max_traction_loss: settings.max_traction_loss,
            native_lateral_velocity: None,
        };
        let actions = ShakeRig::new().frame(&ShakeInput { lateral_velocity: 1.0, ..ShakeInput::default() }, &context);
        let sample = actions
            .iter()
            .find_map(|action| match action {
                Action::Start { voice: Voice::TractionLoss, sample, .. } => Some(*sample),
                _ => None,
            })
            .expect("2.5% native traction loss selects sample2");
        assert_eq!(sample, SampleKey::new(openrigcue_effects::shakers::samples::SampleFolder::Suspension, 2));
    }

    #[test]
    fn native_suspension_threshold_runtime_patch_fails_at_road_bump() {
        use openrigcue_effects::shakers::rig::Voice;
        let state = gain_profile(4.into(), 2.into());
        let mut system = ShakerSystem::new(shaker_settings(&state), SampleBank::synthesized(), MIXER_RATE);
        let mut telemetry = openrigcue_model::Telemetry::off();
        telemetry.rpm = 4000.0;
        telemetry.speed_kmh = 50.0;
        telemetry.harmonized_gear = 2.0;
        telemetry.suspension_velocity_lf = 0.2;
        telemetry.suspension_travel_lf = 0.01;
        telemetry.vertical_acceleration = 4.5;
        let active = FrameState { output_ready: true, game_running: true, switch_on: true };
        system.frame(&telemetry, active).unwrap();
        state.config.write().update(|config| {
            config.settings = config
                .settings
                .apply_ui_patch(&serde_json::json!({"shakers": {
                    "min_bump_velocity": "bad", "side_enhancement": 7
                }}))
                .unwrap();
        });
        system.update_settings(shaker_settings(&state));
        telemetry.harmonized_gear = 3.0;
        telemetry.suspension_travel_lf = 0.02;
        let error = system.frame(&telemetry, active).expect_err("native int('bad') cannot be defaulted");
        assert_eq!(error.native_error().unwrap().class, "ValueError");
        assert_eq!(error.to_string(), "invalid literal for int() with base 10: 'bad'");
        let mixer = system.mixer();
        let mixer = mixer.lock().unwrap();
        assert!(mixer.is_playing(Voice::Transmission));
        assert!(mixer.is_playing(Voice::SuspensionLf));
        assert!(mixer.engine_playing(), "the later engine block was not reached");
        assert_eq!(system.settings().config.side_enhancement, 7);
    }

    #[test]
    fn native_suspension_settings_keep_required_types_and_nonfactory_siblings() {
        use openrigcue_model::native_value::LegacyValue;
        let initial = Settings::default()
            .apply_ui_patch(&serde_json::json!({"shakers": {
                "side_enhancement": 7, "min_side_enhancement": 9,
                "min_suspension_velocity": 45, "max_suspension_velocity": 12345,
                "min_bump_velocity": 50, "max_bump_velocity": 54321
            }}))
            .unwrap();
        for (value, expected, class) in [
            (LegacyValue::Int(42), Some(42), None),
            (LegacyValue::Float(42.5), Some(42), None),
            (LegacyValue::Bool(true), Some(1), None),
            (LegacyValue::from("٤_٢"), Some(42), None),
            (LegacyValue::from("42.5"), None, Some("ValueError")),
            (LegacyValue::None, None, Some("TypeError")),
            (LegacyValue::Float(f64::NAN), None, Some("ValueError")),
            (LegacyValue::Float(f64::INFINITY), None, Some("OverflowError")),
        ] {
            let mut settings = initial.clone();
            settings.edit_native(|root| {
                root.get_mut("SHAKEBOX").unwrap().as_dict_mut().unwrap().insert("min_bump_velocity", value.clone());
            });
            let native = native_suspension_settings(&settings);
            if let Some(expected) = expected {
                assert_eq!(native.min_bump_velocity, Ok(LegacyValue::Int(expected)), "{value:?}");
            } else {
                assert_eq!(native.min_bump_velocity.unwrap_err().class, class.unwrap(), "{value:?}");
            }
            assert_eq!(native.side_enhancement, Ok(LegacyValue::Int(7)));
            assert_eq!(native.min_side_enhancement, Ok(LegacyValue::Int(9)));
            assert_eq!(native.min_suspension_velocity, Ok(LegacyValue::Int(45)));
            assert_eq!(native.max_suspension_velocity, Ok(LegacyValue::Int(12345)));
            assert_eq!(native.max_bump_velocity, Ok(LegacyValue::Int(54321)));
        }
        let mut missing = initial.clone();
        missing.edit_native(|root| {
            root.get_mut("SHAKEBOX").unwrap().as_dict_mut().unwrap().remove("min_bump_velocity");
        });
        assert_eq!(
            native_suspension_settings(&missing).min_bump_velocity.unwrap_err(),
            DspError { class: "KeyError", message: "'min_bump_velocity'".into() }
        );
        // A valid wide value keeps its exact integer (no i64 projection, no typed fallback).
        for value in [LegacyValue::from("9223372036854775808"), LegacyValue::Float(1e100)] {
            let mut wide = initial.clone();
            wide.edit_native(|root| {
                root.get_mut("SHAKEBOX").unwrap().as_dict_mut().unwrap().insert("min_bump_velocity", value.clone());
            });
            let exact = LegacyValue::from_integer(value.py_integer().unwrap());
            assert!(matches!(exact, LegacyValue::BigInt(_)));
            assert_eq!(native_suspension_settings(&wide).min_bump_velocity, Ok(exact));
        }
        let mut digit_limit = initial.clone();
        digit_limit.edit_native(|root| {
            root.get_mut("SHAKEBOX")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .insert("min_bump_velocity", LegacyValue::Str("1".repeat(4301)));
        });
        let error = native_suspension_settings(&digit_limit).min_bump_velocity.unwrap_err();
        assert_eq!(error.class, "ValueError");
        assert!(error.message.starts_with("Exceeds the limit (4300 digits)"));
    }

    #[test]
    fn native_suspension_test_sfx_propagates_before_sleep_and_retains_listener() {
        use openrigcue_effects::shakers::rig::Voice;
        let state = gain_profile(4.into(), 2.into());
        state.config.write().update(|config| {
            config.settings = config
                .settings
                .apply_ui_patch(&serde_json::json!({"shakers": {
                    "max_suspension_velocity": "bad"
                }}))
                .unwrap();
            config.settings.shakers.sound_card = "fake-card".into();
        });
        let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), true);
        shakers.lock().set_soundcard_with(&state, &|| vec!["fake-card".into()], opened).unwrap();
        let mut sleeps = 0;
        let error = test_effect_with_sleeper(
            &state,
            &shakers,
            Effect::Suspension,
            Some("old".into()),
            Some("test".into()),
            |_| sleeps += 1,
        )
        .unwrap_err();
        assert_eq!(error.detail(), "ValueError: invalid literal for int() with base 10: 'bad'");
        assert_eq!(sleeps, 0);
        assert_eq!(state.session.read().listener.as_deref(), Some("test"));
        let guard = shakers.lock();
        let mixer = guard.system.mixer();
        let mixer = mixer.lock().unwrap();
        assert!(mixer.is_playing(Voice::Transmission));
        assert!(!mixer.is_playing(Voice::SuspensionLf));
        assert!(!mixer.engine_playing());
    }

    fn gain_profile(
        level: openrigcue_model::native_value::LegacyValue,
        switch: openrigcue_model::native_value::LegacyValue,
    ) -> ServiceState {
        use openrigcue_model::native_value::LegacyValue;
        let mut row = LegacyValue::from(FactoryTunes::builtin().get("ac").unwrap().to_json());
        for (section, key, value) in [
            ("shaker_effects", "gear", level),
            ("shaker_switch", "gear", switch),
            ("shaker_effects", "impact", LegacyValue::Int(3)),
            ("shaker_switch", "impact", LegacyValue::Bool(true)),
            ("shaker_volume", "ce", LegacyValue::Int(6)),
        ] {
            row.as_dict_mut().unwrap().get_mut(section).unwrap().as_dict_mut().unwrap().insert(key, value);
        }
        dsp_native_profile(row)
    }

    #[test]
    fn rebuild_reads_later_group_inputs_after_prior_rows_are_published() {
        use openrigcue_model::native_value::LegacyValue;
        let state = gain_profile(4.into(), 2.into());
        let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), false);
        let mut changed = false;
        shakers
            .rebuild_levels(&state, None, |row| {
                if row == Some(Group::Engine) {
                    let mut profile = state.profile.write();
                    let mut raw = detached_native_tune(&profile);
                    raw.as_dict_mut()
                        .unwrap()
                        .get_mut("shaker_effects")
                        .unwrap()
                        .assign_item("gear", LegacyValue::Int(0))
                        .unwrap();
                    profile.replace_native_tune(raw);
                    profile.generation += 1;
                    changed = true;
                }
            })
            .unwrap();
        assert!(changed);
        assert_eq!(shakers.lock().system.levels().of(Group::Gear), [0.0; 6]);
        assert_ne!(shakers.lock().system.levels().of(Group::Engine), [0.0; 6]);
    }

    #[test]
    fn concurrent_management_rebuild_waits_while_effect_reads_remain_available() {
        use std::sync::mpsc;
        let state = gain_profile(4.into(), 2.into());
        let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), false);
        let (entered_tx, entered_rx) = mpsc::sync_channel(0);
        let (resume_tx, resume_rx) = mpsc::sync_channel(0);
        let (second_tx, second_rx) = mpsc::sync_channel(1);
        std::thread::scope(|scope| {
            let state_ref = &state;
            let handle = &shakers;
            let first = scope.spawn(move || {
                handle.rebuild_levels(state_ref, None, |row| {
                    if row.is_none() {
                        entered_tx.send(()).unwrap();
                        resume_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                    }
                })
            });
            entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(shakers.management.try_lock().is_err());
            assert!(shakers.processing.try_lock().is_ok());
            let second = scope.spawn(|| {
                shakers.load_channel_groups(&state).unwrap();
                second_tx.send(()).unwrap();
            });
            assert!(second_rx.try_recv().is_err());
            assert!(shakers.lock().system.levels().required_of(Group::Engine).is_err());
            resume_tx.send(()).unwrap();
            first.join().unwrap().unwrap();
            second.join().unwrap();
            second_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        assert!(shakers.lock().system.levels().required_of(Group::Gear).is_ok());
    }

    #[test]
    fn rebuild_publishes_mix_rows_while_existing_voice_groups_remain_available() {
        use openrigcue_effects::shakers::rig::{FrameStage, Voice};
        let state = gain_profile(4.into(), 2.into());
        let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), true);
        state.config.write().update(|c| c.settings.shakers.sound_card = "fake-card".into());
        shakers.lock().set_soundcard_with(&state, &|| vec!["fake-card".into()], opened).unwrap();
        state.session.write().listener = Some("ac".into());
        state.controls.write().switches.shakers = true;
        let mut telemetry = openrigcue_model::Telemetry::off();
        telemetry.harmonized_gear = 1.0;
        telemetry.rpm = 3500.0;
        telemetry.max_rpm = 7000.0;
        run_live_shaker_frame(&state, &shakers, &telemetry, None, |_| {}).unwrap();
        let mut saw_empty = false;
        let mut saw_engine = false;
        shakers
            .rebuild_levels(&state, None, |row| {
                assert!(shakers.management.try_lock().is_err(), "rebuild keeps native management lock");
                assert!(shakers.processing.try_lock().is_ok(), "active effects can run between publication rows");
                if row.is_none() {
                    saw_empty = true;
                    let mut guard = shakers.lock();
                    let error = guard.system.levels().required_of(Group::Engine).unwrap_err();
                    assert_eq!(error.message, "'engine'");
                    assert!(guard.system.mixer().lock().unwrap().is_playing(Voice::Transmission));
                    // Existing groups remain readable to a fresh active DSP pass during empty mix publication.
                    let mut frame = ShakerSystem::begin_live_frame(&telemetry, guard.frame_state(&state));
                    guard
                        .system
                        .advance_live_frame(
                            &mut frame,
                            FrameStage::FilterGroup(Group::Engine, openrigcue_effects::shakers::rig::DspStage::Remove),
                        )
                        .unwrap();
                }
                if row == Some(Group::Engine) {
                    saw_engine = true;
                    assert!(shakers.lock().system.levels().required_of(Group::Engine).is_ok());
                    assert!(shakers.lock().system.levels().required_of(Group::Gear).is_err());
                }
            })
            .unwrap();
        assert!(saw_empty && saw_engine);
        assert!(shakers.management.try_lock().is_ok());
        assert!(shakers.lock().system.levels().required_of(Group::TractionLossRight).is_ok());
    }

    #[test]
    fn live_frame_observes_profile_rebuild_between_effects_and_releases_host_lock() {
        use openrigcue_effects::shakers::rig::{FrameStage, Voice};
        use openrigcue_model::native_value::LegacyValue;
        let state = gain_profile(4.into(), 2.into());
        let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), true);
        state.config.write().update(|c| c.settings.shakers.sound_card = "fake-card".into());
        shakers.lock().set_soundcard_with(&state, &|| vec!["fake-card".into()], opened).unwrap();
        state.session.write().listener = Some("ac".into());
        state.controls.write().switches.shakers = true;
        let mut telemetry = openrigcue_model::Telemetry::off();
        telemetry.rpm = 3000.0;
        telemetry.max_rpm = 7000.0;
        telemetry.harmonized_gear = 1.0;
        let mut saw_rebuild = false;
        run_live_shaker_frame(&state, &shakers, &telemetry, None, |stage| {
            assert!(shakers.processing.try_lock().is_ok(), "host mutex held at stage boundary");
            if stage == FrameStage::Gear {
                let mut raw = detached_native_tune(&state.profile.read());
                raw.as_dict_mut()
                    .unwrap()
                    .get_mut("shaker_effects")
                    .unwrap()
                    .assign_item("gear", LegacyValue::Int(0))
                    .unwrap();
                {
                    let mut profile = state.profile.write();
                    profile.replace_native_tune(raw);
                    profile.generation += 1;
                }
                shakers.load_channel_groups(&state).unwrap();
                saw_rebuild = true;
                let guard = shakers.lock();
                let current = guard.system.levels().of(Group::Gear);
                assert_eq!(current, [0.0; 6]);
                assert!(guard.system.mixer().lock().unwrap().is_playing(Voice::Transmission));
            }
        })
        .unwrap();
        assert!(saw_rebuild);
        let guard = shakers.lock();
        let mut output = vec![0.0; 256 * 6];
        guard.system.mixer().lock().unwrap().render(&mut output);
        assert!(output.iter().any(|x| *x != 0.0), "already-started gear remains on previous build");
    }

    #[test]
    fn another_thread_can_complete_a_group_rebuild_between_live_effects() {
        use openrigcue_effects::shakers::rig::{FrameStage, Voice};
        use std::sync::mpsc;
        let state = gain_profile(4.into(), 2.into());
        let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), true);
        state.config.write().update(|c| c.settings.shakers.sound_card = "fake-card".into());
        shakers.lock().set_soundcard_with(&state, &|| vec!["fake-card".into()], opened).unwrap();
        state.session.write().listener = Some("ac".into());
        state.controls.write().switches.shakers = true;
        let mut telemetry = openrigcue_model::Telemetry::off();
        telemetry.harmonized_gear = 1.0;
        let (ready_tx, ready_rx) = mpsc::sync_channel(0);
        let (done_tx, done_rx) = mpsc::sync_channel(0);
        std::thread::scope(|scope| {
            let state_ref = &state;
            let shakers_ref = &shakers;
            let task = scope.spawn(move || {
                ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                shakers_ref.load_channel_groups(state_ref).unwrap();
                done_tx.send(()).unwrap();
            });
            run_live_shaker_frame(&state, &shakers, &telemetry, None, |stage| {
                if stage == FrameStage::Gear {
                    ready_tx.send(()).unwrap();
                    done_rx.recv_timeout(Duration::from_secs(5)).expect("rebuild blocked by frame lock");
                    assert!(shakers.lock().system.mixer().lock().unwrap().is_playing(Voice::Transmission));
                }
            })
            .unwrap();
            task.join().unwrap();
        });
    }

    #[test]
    fn device_recreation_between_effects_keeps_captured_frame_and_later_operations() {
        use openrigcue_effects::shakers::rig::{FrameStage, Voice};
        let state = gain_profile(4.into(), 2.into());
        let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), true);
        state.config.write().update(|c| c.settings.shakers.sound_card = "fake-card".into());
        shakers.lock().set_soundcard_with(&state, &|| vec!["fake-card".into()], opened).unwrap();
        state.session.write().listener = Some("ac".into());
        state.controls.write().switches.shakers = true;
        let mut telemetry = openrigcue_model::Telemetry::off();
        telemetry.harmonized_gear = 1.0;
        telemetry.afterburner = 1.0;
        run_live_shaker_frame(&state, &shakers, &telemetry, None, |stage| {
            if stage == FrameStage::Gear {
                let mut guard = shakers.lock();
                assert!(guard.system.mixer().lock().unwrap().is_playing(Voice::Transmission));
                state.config.write().update(|c| c.settings.shakers.sound_card = "None".into());
                guard.set_soundcard_with(&state, &|| vec![], |_, _| panic!("None opens no output")).unwrap();
                assert!(!guard.output_ready());
                assert!(!guard.system.mixer().lock().unwrap().is_playing(Voice::Transmission));
                state.config.write().update(|c| c.settings.shakers.sound_card = "fake-card".into());
                guard.set_soundcard_with(&state, &|| vec!["fake-card".into()], opened).unwrap();
                // Active/idle is captured once at entry and not re-evaluated later.
                state.controls.write().switches.shakers = false;
            }
        })
        .unwrap();
        let guard = shakers.lock();
        let mixer = guard.system.mixer();
        let mixer = mixer.lock().unwrap();
        assert!(guard.output_ready());
        assert!(!mixer.is_playing(Voice::Transmission), "released prior-device voice stays gone");
        assert!(mixer.is_playing(Voice::Afterburner), "later effect uses recreated device graph");
        drop(mixer);
        drop(guard);
        let error = run_live_shaker_frame(&state, &shakers, &telemetry, None, |_| {}).unwrap_err();
        assert_eq!(error.native_error().unwrap().message, "INVALID HANDLE");
        // set_soundcard never clears the old transmission sound handle. Idle
        // release reaches that invalid core handle before afterburner.
        assert!(shakers.lock().system.mixer().lock().unwrap().is_playing(Voice::Afterburner));
    }

    #[test]
    fn live_engine_pitch_observes_a_profile_edit_after_the_first_speaker() {
        use openrigcue_effects::shakers::rig::{FrameStage, Voice};
        use openrigcue_model::native_value::LegacyValue;
        let state = gain_profile(4.into(), 2.into());
        {
            let mut profile = state.profile.write();
            let mut raw = detached_native_tune(&profile);
            raw.as_dict_mut()
                .unwrap()
                .get_mut("shaker_dsp_type")
                .unwrap()
                .assign_item("engine", LegacyValue::Int(1))
                .unwrap();
            profile.replace_native_tune(raw);
            profile.generation += 1;
        }
        let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), true);
        state.config.write().update(|c| c.settings.shakers.sound_card = "fake-card".into());
        shakers.lock().set_soundcard_with(&state, &|| vec!["fake-card".into()], opened).unwrap();
        state.session.write().listener = Some("ac".into());
        state.controls.write().switches.shakers = true;
        let mut telemetry = openrigcue_model::Telemetry::off();
        telemetry.rpm = 3000.0;
        telemetry.max_rpm = 7000.0;
        telemetry.harmonized_gear = 1.0;
        let error = run_live_shaker_frame(&state, &shakers, &telemetry, None, |stage| {
            if stage == FrameStage::EngineSpeaker(0) {
                assert!(shakers.processing.try_lock().is_ok());
                let mut profile = state.profile.write();
                let mut raw = detached_native_tune(&profile);
                raw.as_dict_mut()
                    .unwrap()
                    .get_mut("shaker_resonance")
                    .unwrap()
                    .assign_item("engine", LegacyValue::None)
                    .unwrap();
                profile.replace_native_tune(raw);
                profile.generation += 1;
            }
        })
        .unwrap_err();
        assert!(matches!(error, openrigcue_effects::shakers::engine_sound::EngineError::EngineParameter { .. }));
        assert!(shakers.lock().system.mixer().lock().unwrap().is_playing(Voice::Transmission));
    }

    #[test]
    fn live_dsp_reads_each_required_parameter_after_the_preceding_native_call() {
        use openrigcue_effects::shakers::rig::{DspStage, FrameStage, Voice};
        use openrigcue_model::native_value::LegacyValue;
        for (after, section, parameter) in [
            (DspStage::Remove, "shaker_dsp_type", "A_FILTER"),
            (DspStage::Type, "shaker_cutoff", "A_FREQUENCY"),
            (DspStage::Frequency, "shaker_resonance", "A_Q"),
        ] {
            let state = gain_profile(4.into(), 2.into());
            let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), true);
            state.config.write().update(|c| c.settings.shakers.sound_card = "fake-card".into());
            shakers.lock().set_soundcard_with(&state, &|| vec!["fake-card".into()], opened).unwrap();
            state.session.write().listener = Some("ac".into());
            state.controls.write().switches.shakers = true;
            let mut mutated = false;
            let result = run_live_shaker_frame(&state, &shakers, &openrigcue_model::Telemetry::off(), None, |stage| {
                if stage == FrameStage::FilterGroup(Group::Gear, after) {
                    let mut profile = state.profile.write();
                    let mut raw = detached_native_tune(&profile);
                    raw.as_dict_mut()
                        .unwrap()
                        .get_mut(section)
                        .unwrap()
                        .assign_item("gear", LegacyValue::None)
                        .unwrap();
                    profile.replace_native_tune(raw);
                    profile.generation += 1;
                    mutated = true;
                }
            });
            assert!(mutated, "parameter mutation was not reached");
            let error = result.unwrap_err();
            assert!(
                matches!(error, openrigcue_effects::shakers::engine_sound::EngineError::DspParameter {parameter: got, ..} if got==parameter)
            );
            assert!(!shakers.lock().system.mixer().lock().unwrap().is_playing(Voice::Transmission));
        }
    }

    #[test]
    fn live_frame_observes_a_group_rebuild_midway_through_the_dsp_loop() {
        use openrigcue_effects::shakers::rig::FrameStage;
        use openrigcue_model::native_value::LegacyValue;
        let state = gain_profile(4.into(), 2.into());
        let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), true);
        state.config.write().update(|c| c.settings.shakers.sound_card = "fake-card".into());
        shakers.lock().set_soundcard_with(&state, &|| vec!["fake-card".into()], opened).unwrap();
        state.session.write().listener = Some("ac".into());
        state.controls.write().switches.shakers = true;
        let mut telemetry = openrigcue_model::Telemetry::off();
        telemetry.rpm = 3000.0;
        telemetry.max_rpm = 7000.0;
        telemetry.harmonized_gear = 1.0;
        let mut reached = false;
        let error = run_live_shaker_frame(&state, &shakers, &telemetry, None, |stage| {
            if stage == FrameStage::FilterGroup(Group::Engine, openrigcue_effects::shakers::rig::DspStage::Attach) {
                let mut raw = detached_native_tune(&state.profile.read());
                raw.as_dict_mut()
                    .unwrap()
                    .get_mut("shaker_dsp_type")
                    .unwrap()
                    .assign_item("gear", LegacyValue::None)
                    .unwrap();
                {
                    let mut profile = state.profile.write();
                    profile.replace_native_tune(raw);
                    profile.generation += 1;
                }
                shakers.load_channel_groups(&state).unwrap();
                reached = true;
            }
        })
        .unwrap_err();
        assert!(reached);
        assert!(error.native_error().unwrap().message.contains("NoneType"));
        assert!(!shakers
            .lock()
            .system
            .mixer()
            .lock()
            .unwrap()
            .is_playing(openrigcue_effects::shakers::rig::Voice::Transmission));
    }

    #[test]
    fn live_frame_reads_later_required_profile_values_at_their_source_stage() {
        use openrigcue_effects::shakers::rig::{FrameStage, Voice};
        use openrigcue_model::native_value::LegacyValue;
        let state = gain_profile(4.into(), 2.into());
        let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), true);
        state.config.write().update(|c| c.settings.shakers.sound_card = "fake-card".into());
        shakers.lock().set_soundcard_with(&state, &|| vec!["fake-card".into()], opened).unwrap();
        state.session.write().listener = Some("ac".into());
        state.controls.write().switches.shakers = true;
        let mut telemetry = openrigcue_model::Telemetry::off();
        telemetry.rpm = 3000.0;
        telemetry.max_rpm = 7000.0;
        telemetry.harmonized_gear = 1.0;
        telemetry.lateral_velocity = 0.2;
        let result = run_live_shaker_frame(&state, &shakers, &telemetry, None, |stage| {
            if stage == FrameStage::Lateral {
                let mut raw = detached_native_tune(&state.profile.read());
                raw.as_dict_mut()
                    .unwrap()
                    .get_mut("max_telemetry_vars")
                    .unwrap()
                    .assign_item("traction_loss", LegacyValue::Str("bad".into()))
                    .unwrap();
                let mut profile = state.profile.write();
                profile.replace_native_tune(raw);
                profile.generation += 1;
            }
        });
        assert!(result.is_err(), "late source read must observe invalidated traction maximum");
        assert!(shakers.lock().system.mixer().lock().unwrap().is_playing(Voice::Transmission));
    }

    #[test]
    fn native_group_gain_keeps_raw_switch_multipliers_and_level_failure_fallbacks() {
        use openrigcue_effects::shakers::levels::GroupLevels;
        use openrigcue_model::native_value::LegacyValue;
        for (level, switch, normal, focused) in [
            (LegacyValue::Int(4), LegacyValue::Int(2), 0.8, 0.8),
            (LegacyValue::Int(4), LegacyValue::Float(2.9), 0.8, 0.8),
            (LegacyValue::Int(4), LegacyValue::from("2"), 0.8, 0.8),
            (LegacyValue::Int(4), LegacyValue::Bytes(b"2".to_vec()), 0.8, 0.8),
            (LegacyValue::Float(8.5), LegacyValue::Float(2.9), 1.7, 1.7),
            (LegacyValue::Bool(true), LegacyValue::Int(-2), -0.2, -0.2),
            (LegacyValue::Int(4), LegacyValue::from("True"), 1.0, 0.0),
            (LegacyValue::from("4"), LegacyValue::Bool(true), 1.0, 0.0),
            (LegacyValue::from("4"), LegacyValue::Bool(false), 1.0, 0.0),
            (LegacyValue::Bytes(b"4".to_vec()), LegacyValue::Bool(true), 1.0, 0.0),
            (LegacyValue::None, LegacyValue::Bool(false), 1.0, 0.0),
        ] {
            let state = gain_profile(level.clone(), switch.clone());
            let settings = shaker_settings(&state);
            let profile = settings.profile.as_ref().unwrap();
            let channel_scale = profile.level(Group::Gear, Speaker::Center) / 10.0 * 0.6;
            let gains = GroupLevels::compute(profile, &settings.tuning, None);
            assert!(
                (gains.of(Group::Gear)[Speaker::Center.index()] - channel_scale * normal).abs() < 1e-12,
                "level={level:?}, switch={switch:?}"
            );
            let gains = GroupLevels::compute(profile, &settings.tuning, Some(Effect::Gear));
            assert!((gains.of(Group::Gear)[Speaker::Center.index()] - channel_scale * focused).abs() < 1e-12);
            assert_eq!(gains.of(Group::Impact), [0.0; 6], "the effect test mutes other groups");
        }
    }

    #[test]
    fn native_group_gain_failure_keeps_unrelated_nonfactory_levels_and_volume() {
        use openrigcue_effects::shakers::levels::GroupLevels;
        use openrigcue_model::native_value::LegacyValue;
        for switch in [LegacyValue::None, LegacyValue::from("bad"), LegacyValue::List(vec![])] {
            let state = gain_profile(LegacyValue::Int(4), switch);
            let settings = shaker_settings(&state);
            assert_eq!(
                settings.tuning.volume[Speaker::Center.index()],
                6.0,
                "a malformed gear switch must not replace the unrelated stored volume"
            );
            let profile = settings.profile.as_ref().unwrap();
            let gains = GroupLevels::compute(profile, &settings.tuning, None);
            let base = |group| profile.level(group, Speaker::Center) / 10.0 * 0.6;
            assert!((gains.of(Group::Gear)[Speaker::Center.index()] - base(Group::Gear)).abs() < 1e-12);
            assert!(
                (gains.of(Group::Impact)[Speaker::Center.index()] - base(Group::Impact) * 0.3).abs() < 1e-12,
                "the unrelated impact level remains3"
            );
            let focused = GroupLevels::compute(profile, &settings.tuning, Some(Effect::Gear));
            assert_eq!(focused.of(Group::Gear), [0.0; 6]);
        }
    }

    #[test]
    fn native_group_gain_cache_changes_only_on_rebuild_and_restores_after_focus() {
        let first = gain_profile(4.into(), 2.into());
        let second = gain_profile(2.into(), 2.into());
        let mut system = ShakerSystem::new(shaker_settings(&first), SampleBank::synthesized(), MIXER_RATE);
        let before = system.levels().clone();
        system.update_settings(shaker_settings(&second));
        assert_eq!(system.levels(), &before, "a profile read alone does not rebuild held group levels");
        system.reload_levels().unwrap();
        let rebuilt = system.levels().clone();
        assert!(
            (rebuilt.of(Group::Gear)[Speaker::Center.index()] * 2.0 - before.of(Group::Gear)[Speaker::Center.index()])
                .abs()
                < 1e-12
        );
        system.begin_effect_test(Effect::Impact).unwrap();
        assert_eq!(system.levels().of(Group::Gear), [0.0; 6]);
        system.end_effect_test().unwrap();
        assert_eq!(system.levels(), &rebuilt);
    }

    #[test]
    fn native_group_rebuild_ignores_unused_enable_metadata() {
        use openrigcue_model::native_value::LegacyValue;
        let state = gain_profile(4.into(), 2.into());
        {
            let mut layout = state.shaker_layout.write();
            let channels = layout.value.as_dict_mut().unwrap();
            channels.get_mut("FL").unwrap().as_dict_mut().unwrap().remove("enable");
            channels
                .get_mut("CE")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .get_mut("sfx")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .insert("gear", LegacyValue::Int(9));
        }
        let system = ShakerSystem::new(shaker_settings(&state), SampleBank::synthesized(), MIXER_RATE);
        assert!(
            (system.levels().of(Group::Gear)[Speaker::Center.index()] - 0.9 * 0.6 * 0.8).abs() < 1e-12,
            "the source group formula never reads speaker enable metadata"
        );
    }

    #[test]
    fn native_group_rebuild_keeps_only_complete_output_rows_before_raw_arithmetic_failure() {
        use openrigcue_model::native_value::LegacyValue;
        let state = gain_profile(4.into(), 2.into());
        let mut system = ShakerSystem::new(shaker_settings(&state), SampleBank::synthesized(), MIXER_RATE);
        {
            let mut layout = state.shaker_layout.write();
            let center = layout
                .value
                .as_dict_mut()
                .unwrap()
                .get_mut("CE")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .get_mut("sfx")
                .unwrap()
                .as_dict_mut()
                .unwrap();
            center.insert("engine", LegacyValue::Int(4));
            center.insert("gear", LegacyValue::from("9"));
        }
        system.update_settings(shaker_settings(&state));
        let error = system.reload_levels().unwrap_err();
        assert_eq!(error.class, "TypeError");
        assert_eq!(error.message, "unsupported operand type(s) for /: 'str' and 'int'");
        assert_eq!(
            system.levels().of(Group::Gear),
            [0.0; 6],
            "raw str /10 fails before any part of the gear output row is published"
        );
        assert!((system.levels().of(Group::Engine)[Speaker::Center.index()] - 0.4 * 0.6).abs() < 1e-12);
        assert_eq!(system.levels().of(Group::Impact), [0.0; 6], "later rows were never reached");
        assert_eq!(system.levels().required_of(Group::Gear).unwrap_err().message, "'gear'");
        assert_eq!(system.settings().tuning.volume[Speaker::Center.index()], 6.0);
    }

    #[test]
    fn native_group_rebuild_keeps_required_read_order_even_for_muted_groups() {
        use openrigcue_model::native_value::LegacyValue;
        for volume in [LegacyValue::from("7"), LegacyValue::Bytes(b"7".to_vec()), LegacyValue::None] {
            let state = gain_profile(4.into(), 2.into());
            let mut system = ShakerSystem::new(shaker_settings(&state), SampleBank::synthesized(), MIXER_RATE);
            let mut raw = detached_native_tune(&state.profile.read());
            raw.as_dict_mut().unwrap().get_mut("shaker_volume").unwrap().assign_item("fl", volume.clone()).unwrap();
            state.profile.write().replace_native_tune(raw);
            system.update_settings(shaker_settings(&state));
            let error = system.begin_effect_test(Effect::Gear).unwrap_err();
            assert_eq!(error.class, "TypeError");
            assert_eq!(
                error.message,
                format!("unsupported operand type(s) for /: '{}' and 'float'", volume.type_name())
            );
            for group in Group::ALL {
                assert_eq!(system.levels().required_of(group).unwrap_err().class, "KeyError");
            }
            // The FL matrix read precedes that FL volume read, before FR or any row publication.
            state
                .shaker_layout
                .write()
                .value
                .as_dict_mut()
                .unwrap()
                .get_mut("FL")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .get_mut("sfx")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .remove("engine");
            system.update_settings(shaker_settings(&state));
            let error = system.reload_levels().unwrap_err();
            assert_eq!((error.class, error.message.as_str()), ("KeyError", "'engine'"));
        }
    }

    #[test]
    fn native_group_rebuild_failure_keeps_old_held_and_new_sample_routes_and_dsp_state() {
        use openrigcue_effects::shakers::layout::GROUP_COUNT;
        use openrigcue_effects::shakers::rig::{Action, FilterSettings, LowPass, Plays, Voice};
        use openrigcue_effects::shakers::samples::SampleFolder;
        use openrigcue_model::native_value::LegacyValue;
        let state = gain_profile(4.into(), 2.into());
        let mut system = ShakerSystem::new(shaker_settings(&state), SampleBank::synthesized(), MIXER_RATE);
        let reference = ShakerSystem::new(shaker_settings(&state), SampleBank::synthesized(), MIXER_RATE);
        let mut filters = [FilterSettings::default(); GROUP_COUNT];
        filters[Group::Gear.index()] =
            FilterSettings { low_pass: true, parameters: LowPass { cutoff_hz: 100.0, q: 3.0 } };
        let held = Action::Start {
            voice: Voice::SuspensionLf,
            sample: SampleKey::new(SampleFolder::Transmission, 1),
            plays: Plays::Forever,
            group: Group::Gear,
        };
        for sys in [&system, &reference] {
            let mixer = sys.mixer();
            let mut mixer = mixer.lock().unwrap();
            mixer.apply(&[Action::Filters(Box::new(filters)), held.clone()]).unwrap();
            mixer.render(&mut [0.0; 600]);
        }
        state
            .shaker_layout
            .write()
            .value
            .as_dict_mut()
            .unwrap()
            .get_mut("CE")
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .get_mut("sfx")
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .insert("gear", LegacyValue::from("9"));
        system.update_settings(shaker_settings(&state));
        assert!(system.reload_levels().is_err());
        let start = Action::Start {
            voice: Voice::Transmission,
            sample: SampleKey::new(SampleFolder::Transmission, 2),
            plays: Plays::Once,
            group: Group::Gear,
        };
        let mut actual = [0.0; 600];
        let mut expected = [0.0; 600];
        for (sys, output) in [(&system, &mut actual), (&reference, &mut expected)] {
            let mixer = sys.mixer();
            let mut mixer = mixer.lock().unwrap();
            assert!(mixer.is_playing(Voice::SuspensionLf));
            mixer.apply(std::slice::from_ref(&start)).unwrap();
            mixer.render(output);
        }
        assert_eq!(actual, expected, "failed rebuild preserves existing DSP state and the old route for new samples");
        assert!(actual.iter().any(|sample| *sample != 0.0));
        assert_eq!(system.levels().required_of(Group::Gear).unwrap_err().message, "'gear'");
    }

    #[test]
    fn native_speaker_test_bad_unselected_volume_preserves_the_playing_test() {
        use openrigcue_model::native_value::LegacyValue;
        let state = gain_profile(4.into(), 2.into());
        let mut raw = detached_native_tune(&state.profile.read());
        raw.as_dict_mut().unwrap().get_mut("shaker_volume").unwrap().assign_item("ce", LegacyValue::Int(14)).unwrap();
        state.profile.write().replace_native_tune(raw.clone());
        let mut actual = ShakerSystem::new(shaker_settings(&state), SampleBank::synthesized(), MIXER_RATE);
        let mut reference = ShakerSystem::new(shaker_settings(&state), SampleBank::synthesized(), MIXER_RATE);
        for system in [&mut actual, &mut reference] {
            system.test_speaker(Speaker::Center).unwrap();
            system.mixer().lock().unwrap().render(&mut [0.0; 60]);
        }
        raw.as_dict_mut().unwrap().get_mut("shaker_volume").unwrap().assign_item("fl", LegacyValue::from("7")).unwrap();
        state.profile.write().replace_native_tune(raw);
        actual.update_settings(shaker_settings(&state));
        let error = actual.test_speaker(Speaker::Center).unwrap_err();
        assert_eq!(error.class, "TypeError");
        assert_eq!(error.message, "unsupported operand type(s) for /: 'str' and 'float'");
        let mut actual_output = [0.0; 1_200];
        let mut expected = [0.0; 1_200];
        actual.mixer().lock().unwrap().render(&mut actual_output);
        reference.mixer().lock().unwrap().render(&mut expected);
        assert!(actual_output == expected, "eager raw volume failure precedes releasing/replacing the previous test");
        assert!(actual_output.iter().any(|sample| *sample != 0.0));
    }

    #[test]
    fn native_speaker_test_requires_all_six_raw_divisions_in_source_order() {
        use openrigcue_model::native_value::LegacyValue;
        let state = gain_profile(4.into(), 2.into());
        let original = detached_native_tune(&state.profile.read());
        let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), false);
        for (key, value) in [
            ("fl", LegacyValue::from("7")),
            ("fr", LegacyValue::Bytes(b"7".to_vec())),
            ("ce", LegacyValue::None),
            ("sub", LegacyValue::Tuple(vec![])),
            ("rl", LegacyValue::List(vec![])),
            ("rr", LegacyValue::from("7")),
        ] {
            let mut raw = original.clone();
            raw.as_dict_mut().unwrap().get_mut("shaker_volume").unwrap().assign_item(key, value.clone()).unwrap();
            state.profile.write().replace_native_tune(raw);
            let error = test_sound(&state, &shakers, Speaker::RearRight).unwrap_err();
            assert_eq!(error.class, "TypeError");
            assert_eq!(
                error.message,
                format!("unsupported operand type(s) for /: '{}' and 'float'", value.type_name())
            );
            assert!(!shakers
                .lock()
                .system
                .mixer()
                .lock()
                .unwrap()
                .is_playing(openrigcue_effects::shakers::rig::Voice::SpeakerTest));
        }
        let mut raw = original;
        let volumes = raw.as_dict_mut().unwrap().get_mut("shaker_volume").unwrap();
        volumes.as_dict_mut().unwrap().remove("fr");
        volumes.assign_item("ce", LegacyValue::None).unwrap();
        state.profile.write().replace_native_tune(raw.clone());
        assert_eq!(test_sound(&state, &shakers, Speaker::FrontLeft).unwrap_err().message, "'fr'");
        raw.as_dict_mut().unwrap().get_mut("shaker_volume").unwrap().assign_item("fl", LegacyValue::from("7")).unwrap();
        state.profile.write().replace_native_tune(raw);
        assert_eq!(
            test_sound(&state, &shakers, Speaker::FrontLeft).unwrap_err().message,
            "unsupported operand type(s) for /: 'str' and 'float'"
        );
    }

    #[test]
    fn native_speaker_test_reads_the_current_profile_without_waiting_for_a_frame() {
        use openrigcue_model::native_value::LegacyValue;
        let state = gain_profile(4.into(), 2.into());
        let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), false);
        let mut raw = detached_native_tune(&state.profile.read());
        for (key, value) in [
            ("fl", LegacyValue::Int(2)),
            ("fr", LegacyValue::Bool(true)),
            ("ce", LegacyValue::Int(14)),
            ("sub", LegacyValue::Float(6.5)),
            ("rl", LegacyValue::Int(4)),
            ("rr", LegacyValue::Int(8)),
        ] {
            raw.as_dict_mut().unwrap().get_mut("shaker_volume").unwrap().assign_item(key, value).unwrap();
        }
        state.profile.write().replace_native_tune(raw);
        test_sound(&state, &shakers, Speaker::Center).unwrap();
        let guard = shakers.lock();
        assert_eq!(
            guard.system.settings().tuning.native_speaker_test.as_ref().unwrap().as_ref(),
            &Ok([0.1, 0.05, 0.7, 0.325, 0.2, 0.4])
        );
        assert!(
            guard.system.levels().required_of(Group::Engine).is_err(),
            "test groups do not publish normal channel groups"
        );
        assert!(!guard.output_ready());
        let mut output = [0.0; 600];
        guard.system.mixer().lock().unwrap().render(&mut output);
        assert!(output.chunks(6).any(|frame| frame[2] != 0.0));
        assert!(output.chunks(6).all(|frame| [0, 1, 3, 4, 5].iter().all(|channel| frame[*channel] == 0.0)));
    }

    fn native_engine_command(
        row: openrigcue_model::native_value::LegacyValue,
        rpm: f64,
    ) -> (openrigcue_effects::shakers::rig::EngineCommand, PerSpeaker) {
        use openrigcue_effects::shakers::levels::GroupLevels;
        use openrigcue_effects::shakers::rig::{Action, FrameContext, RigStatus, ShakeInput, ShakeRig};
        let state = dsp_native_profile(row);
        let settings = shaker_settings(&state);
        let profile = settings.profile.clone().unwrap_or_else(|| SpeakerProfile::built_in("6 Speakers - 5.1").unwrap());
        let levels = GroupLevels::compute(&profile, &settings.tuning, None);
        let context = FrameContext {
            status: RigStatus::evaluate(true, true, true),
            tuning: &settings.tuning,
            config: &settings.config,
            levels: &levels,
            max_traction_loss: settings.max_traction_loss,
            native_lateral_velocity: None,
        };
        let actions = ShakeRig::new().frame(&ShakeInput { rpm, max_rpm: 10000.0, ..ShakeInput::default() }, &context);
        let command = actions
            .into_iter()
            .find_map(|action| match action {
                Action::Engine(command) => Some(command),
                _ => None,
            })
            .unwrap();
        (command, levels.of(Group::Engine))
    }

    #[test]
    fn native_engine_scalars_keep_raw_flag_comparison_and_native_resonance() {
        use openrigcue_model::native_value::LegacyValue;
        for (flag, resonance, pitch, fades) in [
            (LegacyValue::from("1"), LegacyValue::from("8"), 35.0, false),
            (LegacyValue::Int(1), LegacyValue::from("8"), 8.0, true),
            (LegacyValue::Float(1.0), LegacyValue::Bool(true), 1.0, true),
            (LegacyValue::Bool(true), LegacyValue::Int(8), 8.0, true),
            (LegacyValue::Float(1.9), LegacyValue::Int(8), 35.0, false),
            (LegacyValue::Int(1), LegacyValue::Bytes(b"8".to_vec()), 8.0, true),
        ] {
            let mut row = LegacyValue::from(FactoryTunes::builtin().get("ac").unwrap().to_json());
            row.as_dict_mut()
                .unwrap()
                .get_mut("shaker_dsp_type")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .insert("engine", flag.clone());
            row.as_dict_mut()
                .unwrap()
                .get_mut("shaker_resonance")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .insert("engine", resonance.clone());
            let (command, mix) = native_engine_command(row, 10001.0);
            assert_eq!(command.pitch, pitch, "flag={flag:?} resonance={resonance:?}");
            assert_eq!(command.speaker_levels, Some(if fades { [0.0; 6] } else { mix }), "flag={flag:?}");
        }
    }

    #[test]
    fn native_engine_fade_endpoints_are_required_before_raw_branch_comparisons() {
        use openrigcue_model::native_value::LegacyValue;
        for (flag, start, end, rpm, success) in [
            (0, None, Some(100.into()), 8000.0, false),
            (0, Some(95.into()), None, 8000.0, false),
            (0, Some("bad".into()), Some(LegacyValue::None), 8000.0, true),
            (1, Some("95".into()), Some(100.into()), 8000.0, false),
            (1, Some(95.into()), Some("bad".into()), 8000.0, true),
            (1, Some(95.into()), None, 8000.0, false),
            (1, Some(95.into()), Some("100".into()), 9700.0, false),
        ] {
            let mut row = LegacyValue::from(FactoryTunes::builtin().get("ac").unwrap().to_json());
            row.as_dict_mut()
                .unwrap()
                .get_mut("shaker_dsp_type")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .insert("engine", LegacyValue::Int(flag));
            let cutoffs = row.as_dict_mut().unwrap().get_mut("shaker_cutoff").unwrap().as_dict_mut().unwrap();
            for (key, value) in [("engine_fadeoff_start", start), ("engine_fadeoff_end", end)] {
                if let Some(value) = value {
                    cutoffs.insert(key, value);
                } else {
                    cutoffs.remove(key);
                }
            }
            let (command, mix) = native_engine_command(row, rpm);
            assert_eq!(command.speaker_levels, success.then_some(mix), "flag={flag} rpm={rpm}");
        }
    }

    #[test]
    fn native_engine_selector_keeps_canonical_keys_and_the_late_action_prefix() {
        use openrigcue_effects::shakers::rig::Voice;
        use openrigcue_model::native_value::LegacyValue;
        let base = LegacyValue::from(FactoryTunes::builtin().get("ac").unwrap().to_json());
        for (selector, class, message) in [
            (LegacyValue::from("X"), "KeyError", "'X'"),
            (LegacyValue::Int(17), "KeyError", "17"),
            (LegacyValue::None, "KeyError", "None"),
            (LegacyValue::Bytes(b"O".to_vec()), "KeyError", "b'O'"),
            (LegacyValue::List(vec![]), "TypeError", "unhashable type: 'list'"),
            (LegacyValue::Dict(Default::default()), "TypeError", "unhashable type: 'dict'"),
            (LegacyValue::Tuple(vec![LegacyValue::List(vec![])]), "TypeError", "unhashable type: 'list'"),
        ] {
            let mut row = base.clone();
            row.as_dict_mut().unwrap().insert("shaker_engine_type", selector);
            let state = dsp_native_profile(row);
            let mut system = ShakerSystem::new(shaker_settings(&state), SampleBank::synthesized(), MIXER_RATE);
            let mut telemetry = openrigcue_model::Telemetry::off();
            telemetry.harmonized_gear = 1.0;
            let frame = FrameState { output_ready: true, game_running: true, switch_on: true };
            let error = system.frame(&telemetry, frame).unwrap_err();
            assert_eq!(error.to_string(), message);
            assert!(
                matches!(error, openrigcue_effects::shakers::engine_sound::EngineError::EngineSelector { error } if error.class == class)
            );
            assert!(system.mixer().lock().unwrap().is_playing(Voice::Transmission), "the earlier gear block ran");
            system.mixer().lock().unwrap().render(&mut vec![0.0; 4_800 * 6]);
            assert!(!system.mixer().lock().unwrap().is_playing(Voice::Transmission));
            let valid = dsp_native_profile(base.clone());
            system.update_settings(shaker_settings(&valid));
            system.frame(&telemetry, frame).unwrap();
            assert!(system.mixer().lock().unwrap().is_playing(Voice::Transmission), "late error did not remember gear");
        }
    }

    /// The shaker loop reports a failed frame with its message. A missing selector key that
    /// is an integer of more than 4300 digits cannot be rendered as text, so building the report fails
    /// with an error of its own and the loop ends; a missing key that can be rendered is reported and the
    /// loop goes on.
    #[test]
    fn shaker_pass_stops_when_the_reported_error_cannot_be_rendered() {
        use openrigcue_model::native_value::LegacyValue;
        // 10**digits, built by multiplication: parsing 4301-digit text is itself refused.
        let int = |digits: usize| {
            (0..digits).fold(LegacyValue::Int(1), |value, _| value.py_mul(&LegacyValue::Int(10)).unwrap())
        };
        for (selector, stops) in [
            (int(4300), true),
            (LegacyValue::Tuple(vec![LegacyValue::from("O"), int(4300)]), true),
            (int(4299), false),
            (LegacyValue::from("X"), false),
        ] {
            let mut row = LegacyValue::from(FactoryTunes::builtin().get("ac").unwrap().to_json());
            row.as_dict_mut().unwrap().insert("shaker_engine_type", selector);
            let state = dsp_native_profile(row);
            let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), true);
            state.config.write().update(|c| c.settings.shakers.sound_card = "fake-card".into());
            shakers.lock().set_soundcard_with(&state, &|| vec!["fake-card".into()], opened).unwrap();
            state.session.write().listener = Some("ac".into());
            state.controls.write().switches.shakers = true;
            *state.telemetry_ready.write() = true;
            let accessories = Accessories::new(std::time::Instant::now());
            assert_eq!(shaker_pass(&state, &shakers, &accessories).is_none(), stops);
        }
    }

    #[test]
    fn native_engine_selector_required_lookup_precedes_pause_but_bad_key_follows_it() {
        use openrigcue_model::native_value::LegacyValue;
        for (initial, missing, still_playing) in [("O", true, true), ("O", false, false), ("E", false, true)] {
            let mut row = LegacyValue::from(FactoryTunes::builtin().get("ac").unwrap().to_json());
            row.as_dict_mut().unwrap().insert("shaker_engine_type", LegacyValue::from(initial));
            let state = dsp_native_profile(row.clone());
            let mut system = ShakerSystem::new(shaker_settings(&state), SampleBank::synthesized(), MIXER_RATE);
            let telemetry = openrigcue_model::Telemetry::off();
            let frame = FrameState { output_ready: true, game_running: true, switch_on: true };
            system.frame(&telemetry, frame).unwrap();
            assert!(system.mixer().lock().unwrap().engine_playing());
            if missing {
                row.as_dict_mut().unwrap().remove("shaker_engine_type");
            } else {
                row.as_dict_mut().unwrap().insert("shaker_engine_type", LegacyValue::from("X"));
            }
            let bad = dsp_native_profile(row);
            system.update_settings(shaker_settings(&bad));
            let error = system.frame(&telemetry, frame).unwrap_err();
            assert_eq!(error.to_string(), if missing { "'shaker_engine_type'" } else { "'X'" });
            assert_eq!(system.mixer().lock().unwrap().engine_playing(), still_playing);
        }
    }

    #[test]
    fn native_engine_selector_accepts_both_canonical_choices() {
        use openrigcue_model::native_value::LegacyValue;
        for selector in ["O", "E"] {
            let mut row = LegacyValue::from(FactoryTunes::builtin().get("ac").unwrap().to_json());
            row.as_dict_mut().unwrap().insert("shaker_engine_type", LegacyValue::from(selector));
            let state = dsp_native_profile(row);
            let mut system = ShakerSystem::new(shaker_settings(&state), SampleBank::synthesized(), MIXER_RATE);
            system
                .frame(
                    &openrigcue_model::Telemetry::off(),
                    FrameState { output_ready: true, game_running: true, switch_on: true },
                )
                .unwrap();
            assert!(system.mixer().lock().unwrap().engine_playing());
        }
    }

    #[test]
    fn native_dsp_profile_scalar_failures_reach_the_active_frame() {
        use openrigcue_model::native_value::LegacyValue;
        let base = LegacyValue::from(FactoryTunes::builtin().get("ac").unwrap().to_json());
        for (section, value, expected) in [
            ("shaker_cutoff", Some(LegacyValue::from("bad")), "could not convert string to float: 'bad'"),
            (
                "shaker_cutoff",
                Some(LegacyValue::None),
                "float() argument must be a string or a real number, not 'NoneType'",
            ),
            ("shaker_cutoff", None, "'gear'"),
            (
                "shaker_resonance",
                Some(LegacyValue::None),
                "float() argument must be a string or a real number, not 'NoneType'",
            ),
            ("shaker_dsp_type", Some(LegacyValue::from("bad")), "invalid literal for int() with base 10: 'bad'"),
            (
                "shaker_dsp_type",
                Some(LegacyValue::None),
                "int() argument must be a string, a bytes-like object or a real number, not 'NoneType'",
            ),
        ] {
            let mut row = base.clone();
            let fields = row.as_dict_mut().unwrap().get_mut(section).unwrap().as_dict_mut().unwrap();
            if let Some(value) = value {
                fields.insert("gear", value);
            } else {
                fields.remove("gear");
            }
            let state = dsp_native_profile(row);
            let mut system = ShakerSystem::new(shaker_settings(&state), SampleBank::synthesized(), MIXER_RATE);
            let mut telemetry = openrigcue_model::Telemetry::off();
            telemetry.harmonized_gear = 1.0;
            let frame = FrameState { output_ready: true, game_running: true, switch_on: true };
            assert_eq!(system.frame(&telemetry, frame).unwrap_err().to_string(), expected);
            assert!(!system.mixer().lock().unwrap().is_playing(openrigcue_effects::shakers::rig::Voice::Transmission));
        }
    }

    #[test]
    fn native_dsp_profile_missing_parent_stops_before_playback() {
        use openrigcue_model::native_value::LegacyValue;
        let mut row = LegacyValue::from(FactoryTunes::builtin().get("ac").unwrap().to_json());
        row.as_dict_mut().unwrap().remove("shaker_cutoff");
        let state = dsp_native_profile(row);
        let mut system = ShakerSystem::new(shaker_settings(&state), SampleBank::synthesized(), MIXER_RATE);
        let mut telemetry = openrigcue_model::Telemetry::off();
        telemetry.harmonized_gear = 1.0;
        let frame = FrameState { output_ready: true, game_running: true, switch_on: true };
        assert_eq!(system.frame(&telemetry, frame).unwrap_err().to_string(), "'shaker_cutoff'");
    }

    #[test]
    fn native_dsp_profile_numeric_strings_and_bools_keep_their_values() {
        use openrigcue_model::native_value::LegacyValue;
        let mut row = LegacyValue::from(FactoryTunes::builtin().get("ac").unwrap().to_json());
        for (section, value) in [
            ("shaker_dsp_type", LegacyValue::from(" +١_٢ ")),
            ("shaker_cutoff", LegacyValue::from(" 1_2_5.5 ")),
            ("shaker_resonance", LegacyValue::Bool(true)),
        ] {
            row.as_dict_mut().unwrap().get_mut(section).unwrap().as_dict_mut().unwrap().insert("gear", value);
        }
        row.as_dict_mut().unwrap().get_mut("shaker_volume").unwrap().as_dict_mut().unwrap().insert("fl", 7.into());
        let state = dsp_native_profile(row);
        let settings = shaker_settings(&state);
        assert_eq!(settings.tuning.volume[Speaker::FrontLeft.index()], 7.0);
        let gear = &settings.tuning.native_dsp.as_ref().unwrap()[Group::Gear.index()];
        assert_eq!(gear.filter_type, Ok(12));
        assert_eq!(gear.cutoff_hz, Ok(125.5));
        assert_eq!(gear.q, Ok(1.0));
        let mut system = ShakerSystem::new(settings, SampleBank::synthesized(), MIXER_RATE);
        let mut telemetry = openrigcue_model::Telemetry::off();
        telemetry.harmonized_gear = 1.0;
        system.frame(&telemetry, FrameState { output_ready: true, game_running: true, switch_on: true }).unwrap();
        assert!(system.mixer().lock().unwrap().is_playing(openrigcue_effects::shakers::rig::Voice::Transmission));
    }

    #[test]
    fn native_multiband_profile_kind_reaches_real_sample_rendering() {
        use openrigcue_effects::shakers::rig::{Action, Plays, Voice};
        use openrigcue_effects::shakers::samples::{SampleFolder, SampleKey};
        use openrigcue_model::native_value::LegacyValue;
        let render = |kind: i32| {
            let mut row = LegacyValue::from(FactoryTunes::builtin().get("ac").unwrap().to_json());
            let tune = row.as_dict_mut().unwrap();
            tune.get_mut("shaker_dsp_type").unwrap().assign_item("suspension", i64::from(kind).into()).unwrap();
            tune.get_mut("shaker_cutoff").unwrap().assign_item("suspension", 30.into()).unwrap();
            tune.get_mut("shaker_resonance").unwrap().assign_item("suspension", 1.into()).unwrap();
            tune.get_mut("shaker_volume").unwrap().assign_item("fl", 7.into()).unwrap();
            let state = dsp_native_profile(row);
            let settings = shaker_settings(&state);
            assert_eq!(settings.tuning.volume[Speaker::FrontLeft.index()], 7.0);
            let filters = settings.tuning.native_dsp.as_ref().unwrap().clone();
            assert_eq!(filters[Group::SuspensionLf.index()].filter_type, Ok(kind));
            let system = ShakerSystem::new(settings, SampleBank::synthesized(), MIXER_RATE);
            let mixer = system.mixer();
            let mut mixer = mixer.lock().unwrap();
            mixer
                .apply(&[
                    Action::NativeFilters(filters),
                    Action::Start {
                        voice: Voice::SuspensionLf,
                        sample: SampleKey::new(SampleFolder::Suspension, 5),
                        plays: Plays::Forever,
                        group: Group::SuspensionLf,
                    },
                ])
                .unwrap();
            let mut output = vec![0.0; 8_192 * 6];
            mixer.render(&mut output);
            output.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>()
        };
        let plain = render(0);
        assert!(plain > 1.0);
        assert!(render(2) < plain * 0.02);
        assert!(render(3) < plain * 0.02);
    }

    #[test]
    fn native_dsp_filter_int_uses_original_win64_untyped_ctypes_boundary() {
        use openrigcue_model::native_value::LegacyValue;
        for (value, expected) in [
            (LegacyValue::Bool(true), 1),
            (LegacyValue::Float(-2_147_483_648.9), i32::MIN),
            (LegacyValue::from(" +٢_١٤٧_٤٨٣_٦٤٨ "), i32::MIN),
            (LegacyValue::Int(4_294_967_295), -1),
            (LegacyValue::Float(4_294_967_295.9), -1),
        ] {
            assert_eq!(dsp_filter_int(&value), Ok(expected));
        }
        for value in [
            LegacyValue::Int(-2_147_483_649),
            LegacyValue::Int(4_294_967_296),
            LegacyValue::Float(-2_147_483_649.0),
            LegacyValue::Float(f64::MAX),
            LegacyValue::from("170141183460469231731687303715884105728"),
            LegacyValue::from(&format!("1{}", "0".repeat(4299))[..]),
        ] {
            let error = dsp_filter_int(&value).unwrap_err();
            assert_eq!(error.class, "ctypes.ArgumentError");
            assert_eq!(error.message, "argument 3: OverflowError: int too long to convert");
        }
        for (value, class, message) in [
            (LegacyValue::Float(f64::NAN), "ValueError", "cannot convert float NaN to integer"),
            (LegacyValue::Float(f64::INFINITY), "OverflowError", "cannot convert float infinity to integer"),
            (LegacyValue::from("1__2"), "ValueError", "invalid literal for int() with base 10: '1__2'"),
        ] {
            let error = dsp_filter_int(&value).unwrap_err();
            assert_eq!((error.class, error.message.as_str()), (class, message));
        }
        let error = dsp_filter_int(&LegacyValue::from(&format!("1{}", "0".repeat(4300))[..])).unwrap_err();
        assert_eq!(error.class, "ValueError");
        assert_eq!(error.message, "Exceeds the limit (4300 digits) for integer string conversion: value has 4301 digits; use sys.set_int_max_str_digits() to increase the limit");
    }

    #[test]
    fn native_dsp_profile_failure_order_is_type_then_cutoff_then_q() {
        use openrigcue_effects::shakers::engine_sound::EngineError;
        use openrigcue_model::native_value::LegacyValue;
        let mut row = LegacyValue::from(FactoryTunes::builtin().get("ac").unwrap().to_json());
        for section in ["shaker_dsp_type", "shaker_cutoff", "shaker_resonance"] {
            row.as_dict_mut().unwrap().get_mut(section).unwrap().as_dict_mut().unwrap().remove("gear");
        }
        for (section, parameter, value) in [
            ("shaker_dsp_type", "A_FILTER", LegacyValue::from("1")),
            ("shaker_cutoff", "A_FREQUENCY", LegacyValue::Bool(true)),
            ("shaker_resonance", "A_Q", LegacyValue::from("2.5")),
        ] {
            let state = dsp_native_profile(row.clone());
            let mut system = ShakerSystem::new(shaker_settings(&state), SampleBank::synthesized(), MIXER_RATE);
            let error = system
                .frame(
                    &openrigcue_model::Telemetry::off(),
                    FrameState { output_ready: true, game_running: true, switch_on: true },
                )
                .unwrap_err();
            assert!(matches!(error, EngineError::DspParameter { group: Group::Gear, parameter: got, error }
                if got == parameter && error.class == "KeyError" && error.message == "'gear'"));
            row.as_dict_mut().unwrap().get_mut(section).unwrap().as_dict_mut().unwrap().insert("gear", value);
        }
    }

    /// The master configuration's shaker defaults equal the shaker engine's common default tuning.
    #[test]
    fn factory_shaker_keys_match_the_engine_default() {
        let tune = FactoryTunes::builtin().get("dirtrally2.0").unwrap();
        let converted = shaker_tuning(tune);
        let default = ShakerTuning::catalog_default("dirtrally2.0", None);
        assert_eq!(converted.volume, default.volume);
        assert_eq!(converted.engine_type, default.engine_type);
        for effect in Effect::ALL {
            assert_eq!(converted.effect(effect), default.effect(effect), "{effect:?}");
        }
    }

    /// Speaker layouts: a stored built-in layout equals the engine's built-in profile.
    #[test]
    fn stored_layout_matches_the_built_in_profile() {
        let config = Configuration::first_run();
        let name = "6 Speakers - 5.1";
        let stored = config.shaker_layouts.get(name).expect("layout stored on first run");
        assert_eq!(speaker_profile(name, stored), SpeakerProfile::built_in(name).unwrap());
    }

    /// Shaker loop: the shake frame returns at once without a sound card; the loop period follows
    /// the `SHAKEBOX` refresh rate with the platform rules' cap (27 Hz on a fresh install).
    #[test]
    fn shaker_pass_without_sound_card() {
        let state = state();
        let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), false);
        assert!(!shakers.lock().output_ready());
        let accessories = Accessories::new(std::time::Instant::now());
        assert_eq!(shaker_pass(&state, &shakers, &accessories), Some(Duration::from_millis(500)));
        *state.telemetry_ready.write() = true;
        let pause = shaker_pass(&state, &shakers, &accessories);
        assert_eq!(pause, openrigcue_model::python_time::sleep_duration(1.0 / 27.0).ok());
        state.config.write().update(|c| c.settings.shakers.refresh_rate = 0);
        assert_eq!(shaker_pass(&state, &shakers, &accessories), None);
    }

    /// While the shift beep's inputs wait for the configuration lock (a UI write holds it), no session
    /// lock is held: the telemetry main loop's session write goes through. Holding the session read lock
    /// there deadlocks the shaker thread with the main loop, and the UI with both, once the beep reads
    /// the session again.
    #[test]
    fn beep_inputs_hold_no_session_lock_while_waiting_for_the_configuration() {
        use std::sync::mpsc;
        let state = state();
        state.session.write().shift_light = true;
        let shift_sound = state.config.read().settings().app.gear_shift_sound;
        let ui_write = state.config.write();
        let (started_tx, started_rx) = mpsc::channel();
        let (beep_tx, beep_rx) = mpsc::channel();
        {
            let state = state.clone();
            std::thread::spawn(move || {
                started_tx.send(()).unwrap();
                let _ = beep_tx.send(beep_inputs(&state));
            });
        }
        started_rx.recv_timeout(Duration::from_secs(5)).expect("beep reader started");
        // Time for the reader to reach the configuration lock, which the UI write above holds.
        std::thread::sleep(Duration::from_millis(300));
        let (written_tx, written_rx) = mpsc::channel();
        {
            let state = state.clone();
            std::thread::spawn(move || {
                state.session.write().previous_game = Some("ac".into());
                let _ = written_tx.send(());
            });
        }
        assert!(
            written_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "the session write waited for the beep reader, which holds the session lock while waiting for the configuration"
        );
        drop(ui_write);
        let beep = beep_rx.recv_timeout(Duration::from_secs(5)).expect("beep inputs read once the UI write ends");
        assert_eq!(beep, BeepInputs { listener_running: false, shift_sound, shift_light: 1.0 });
    }

    /// The engine type: 'O' the standard engine sound, 'E' the enhanced one, anything
    /// else no instance (a missing-key error in the engine block).
    #[test]
    fn engine_type_letter() {
        let mut tune = FactoryTunes::builtin().get("emulator").unwrap().clone();
        tune.shaker_engine_type = "O".into();
        assert_eq!(shaker_tuning(&tune).engine_type, Some(EngineType::Original));
        tune.shaker_engine_type = "E".into();
        assert_eq!(shaker_tuning(&tune).engine_type, Some(EngineType::Enhanced));
        tune.shaker_engine_type = "X".into();
        assert_eq!(shaker_tuning(&tune).engine_type, None);
    }

    /// A 16-bit mono RIFF WAV file of `frames` samples at 44.1 kHz (the sample reader accepts WAV content under
    /// the `.ogg` names).
    pub(crate) fn wav(frames: u16) -> Vec<u8> {
        let data_len = u32::from(frames) * 2;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
        bytes.extend_from_slice(&1u16.to_le_bytes()); // mono
        bytes.extend_from_slice(&44_100u32.to_le_bytes());
        bytes.extend_from_slice(&88_200u32.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        for i in 0..frames {
            bytes.extend_from_slice(&(if i % 2 == 0 { 8192i16 } else { -8192 }).to_le_bytes());
        }
        bytes
    }

    /// An SRS installation in a temporary directory holding only `sounds/transmission/1.ogg`.
    pub(crate) fn srs_install_with_one_sample() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("sounds").join("transmission");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("1.ogg"), wav(64)).unwrap();
        dir
    }

    fn shakers_with(state: &ServiceState, srs: Option<PathBuf>) -> ShakersHandle {
        let shakers = ShakersHandle::new(state, SampleBank::synthesized(), Box::new(SilentBeep), false)
            .with_srs_sounds(state, srs);
        shakers.wait_for_sounds();
        shakers
    }

    fn set_use_srs_sounds(state: &ServiceState, on: bool) {
        state.config.write().update(|c| c.settings.shakers.use_srs_sounds = on);
    }

    fn pass(state: &ServiceState, shakers: &ShakersHandle) {
        // These reload fixtures model a running loop after the first telemetry publication.
        state.telemetry.write().set_native("spotter_right", openrigcue_model::native_value::LegacyValue::Int(0));
        *state.telemetry_ready.write() = true;
        let accessories = Accessories::new(std::time::Instant::now());
        shaker_pass(state, shakers, &accessories);
        shakers.wait_for_sounds();
    }

    const BUILT_IN: ShakersStatus = ShakersStatus {
        output_ready: false,
        is_fmod: false,
        sample_source: ActiveSamples::BuiltIn,
        imported_samples: 0,
        srs_sounds_available: true,
        refresh_soundcard: false,
    };

    /// The built-in sounds are the default: an SRS installation alone imports nothing while "Use SRS Sound
    /// Samples" is off (a fresh configuration has it off).
    #[test]
    fn built_in_sounds_unless_the_user_opts_in() {
        let state = state();
        assert!(!state.config.read().settings().shakers.use_srs_sounds);
        let install = srs_install_with_one_sample();
        let shakers = shakers_with(&state, Some(install.path().to_path_buf()));
        assert_eq!(shakers.lock().status(), BUILT_IN);
    }

    /// With the setting on, the SRS samples found are played and the others keep their built-in sound; the
    /// engine falls back to the built-in one because the installation has no engine banks. Turning the setting
    /// off again at run time (next shaker pass) restores the built-in sounds.
    #[test]
    fn opting_in_imports_srs_samples_and_back() {
        let state = state();
        set_use_srs_sounds(&state, true);
        let install = srs_install_with_one_sample();
        let shakers = shakers_with(&state, Some(install.path().to_path_buf()));
        let imported = ShakersStatus { sample_source: ActiveSamples::SrsImport, imported_samples: 1, ..BUILT_IN };
        assert_eq!(shakers.lock().status(), imported);

        set_use_srs_sounds(&state, false);
        pass(&state, &shakers);
        assert_eq!(shakers.lock().status(), BUILT_IN);

        set_use_srs_sounds(&state, true);
        pass(&state, &shakers);
        assert_eq!(shakers.lock().status(), imported);
    }

    /// The setting without a usable SRS source keeps the built-in sounds and engine: no installation given,
    /// a path that does not exist, an installation without a `sounds` folder.
    #[test]
    fn opting_in_without_srs_files_keeps_built_in_sounds() {
        let state = state();
        set_use_srs_sounds(&state, true);
        let shakers = shakers_with(&state, None);
        assert_eq!(shakers.lock().status(), ShakersStatus { srs_sounds_available: false, ..BUILT_IN });

        let empty = tempfile::tempdir().unwrap();
        for path in [empty.path().join("missing"), empty.path().to_path_buf()] {
            let shakers = shakers_with(&state, Some(path));
            assert_eq!(shakers.lock().status(), BUILT_IN);
        }
    }

    /// An incomplete set of engine banks (`Vehicles.bank` missing) falls back to the built-in engine; the samples
    /// are still imported. `EngineBanks::read` fails before any FMOD library is looked for, so the result does not depend on FMOD
    /// being installed on the machine (the library search and FMOD rejecting a bank are environment-dependent
    /// and pinned in `openrigcue-effects` with the fake FMOD API, e.g. `fmod::tests::missing_event_fails_open`).
    #[test]
    fn unusable_engine_banks_fall_back_to_the_built_in_engine() {
        let state = state();
        set_use_srs_sounds(&state, true);
        let install = srs_install_with_one_sample();
        let engine = install.path().join("sounds").join("engine");
        std::fs::create_dir_all(&engine).unwrap();
        for name in ["Master Bank.bank", "Master Bank.strings.bank"] {
            std::fs::write(engine.join(name), b"not an FMOD bank").unwrap();
        }
        let shakers = shakers_with(&state, Some(install.path().to_path_buf()));
        let status = shakers.lock().status();
        assert!(!status.is_fmod);
        assert_eq!(status.sample_source, ActiveSamples::SrsImport);
    }

    /// The shift beep's SRS file (`sounds/transmission/2.wav`) is taken only while "Use SRS Sound Samples" is on,
    /// like the shaker samples; off, the beep plays the built-in sound (`built_in_shift_beep_by_default`).
    /// `output_allowed = false` keeps the beep player itself silent, so no device is opened.
    #[test]
    fn shift_beep_follows_the_sound_choice() {
        let state = state();
        let install = srs_install_with_one_sample();
        let beep = install.path().join("sounds").join("transmission").join("2.wav");
        std::fs::write(&beep, wav(64)).unwrap();
        let beep = beep.display().to_string();
        let shakers = shakers_with(&state, Some(install.path().to_path_buf()));
        assert_eq!(shakers.lock().beep_file(), None);

        set_use_srs_sounds(&state, true);
        pass(&state, &shakers);
        assert_eq!(shakers.lock().beep_file(), Some(beep.as_str()));

        set_use_srs_sounds(&state, false);
        pass(&state, &shakers);
        assert_eq!(shakers.lock().beep_file(), None);
    }

    /// There is always a shift beep. Out of the box (no SRS files, "Use SRS Sound
    /// Samples" off) the first sound choice gives the beep the built-in sound, with no worker thread; switching
    /// the SRS sounds on takes the imported file (`sounds/transmission/2.wav`), off again the built-in one. Without the hardware loops no sound is
    /// chosen. No sound card is selected, so no device is opened.
    #[test]
    fn built_in_shift_beep_by_default() {
        let state = state();
        assert_eq!(state.config.read().settings().shakers.sound_card, "None");
        let install = srs_install_with_one_sample();
        let file = install.path().join("sounds").join("transmission").join("2.wav");
        std::fs::write(&file, wav(64)).unwrap();
        let new = |srs: Option<PathBuf>| {
            ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), true)
                .with_srs_sounds(&state, srs)
        };
        for srs in [None, Some(install.path().to_path_buf())] {
            let shakers = new(srs);
            assert!(shakers.lock().loader.is_none(), "built-in sounds need no load");
            assert_eq!(shakers.lock().beep_location(), Some("built-in"));
            assert_eq!(shakers.lock().beep_file(), None);
        }

        let shakers = new(Some(install.path().to_path_buf()));
        set_use_srs_sounds(&state, true);
        pass(&state, &shakers);
        let srs = file.display().to_string();
        assert_eq!(shakers.lock().beep_location(), Some(srs.as_str()));
        set_use_srs_sounds(&state, false);
        pass(&state, &shakers);
        assert_eq!(shakers.lock().beep_location(), Some("built-in"));

        let no_hardware = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), false)
            .with_srs_sounds(&state, None);
        assert_eq!(no_hardware.lock().beep_location(), None);
    }

    /// An import is built on the `shaker_sounds` worker, not on the calling thread (service start, shaker loop):
    /// the call returns with the loader recorded, and the previous sounds play until it is installed.
    #[test]
    fn importing_runs_on_a_worker_thread() {
        let state = state();
        set_use_srs_sounds(&state, true);
        let install = srs_install_with_one_sample();
        let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), false)
            .with_srs_sounds(&state, Some(install.path().to_path_buf()));
        assert!(shakers.lock().loader.is_some());
        // A second call meanwhile (running or finished loader) starts no other load and installs nothing else.
        shakers.follow_sound_choice(&state);
        shakers.wait_for_sounds();
        assert_eq!(shakers.lock().status().sample_source, ActiveSamples::SrsImport);
        shakers.follow_sound_choice(&state);
        assert!(shakers.lock().loader.is_none());
    }

    fn expected_levels(state: &ServiceState) -> openrigcue_effects::shakers::levels::GroupLevels {
        let settings = shaker_settings(state);
        openrigcue_effects::shakers::levels::GroupLevels::compute(
            settings.profile.as_ref().unwrap(),
            &settings.tuning,
            None,
        )
    }

    /// `load_channel_groups()` from a widget handler rebuilds the groups from the tune at once; a tune edit
    /// alone (the shaker loop's next pass) only reaches the frames, not the groups.
    #[test]
    fn load_channel_groups_rebuilds_from_game_config() {
        let state = state();
        let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), false);
        let before = shakers.lock().system.levels().clone();
        {
            let mut profile = state.profile.write();
            profile.tune.shaker_effects.gear = 4.0;
            profile.tune.shaker_volume.sub = 13.0;
            profile.generation += 1;
        }
        pass(&state, &shakers);
        assert_eq!(shakers.lock().system.levels(), &before, "an edit alone does not rebuild");
        assert_eq!(shakers.lock().system.settings().tuning.effect(Effect::Gear).level.unwrap().slider, 4.0);
        shakers.load_channel_groups(&state).unwrap();
        assert_ne!(shakers.lock().system.levels(), &before);
        assert_eq!(shakers.lock().system.levels(), &expected_levels(&state));
    }

    /// No card is opened until the UI's `on_soundcard_change` runs `set_soundcard`, whatever the configuration
    /// holds.
    #[test]
    fn no_card_is_opened_before_set_soundcard() {
        let state = state();
        state.config.write().update(|c| c.settings.shakers.sound_card = "hw:CARD=X4,DEV=0".into());
        let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), true);
        pass(&state, &shakers);
        assert!(!shakers.lock().output_ready());
    }

    #[test]
    fn native_cold_group_construction_does_not_prebuild_before_a_handler_requests_it() {
        let state = gain_profile(4.into(), 2.into());
        let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), false);
        assert!(shakers.lock().system.levels().required_of(Group::Engine).is_err());
        shakers.load_channel_groups(&state).unwrap();
        assert!(shakers.lock().system.levels().required_of(Group::Engine).is_ok());
    }

    #[test]
    fn native_cold_group_lookup_precedes_dsp_conversion_after_first_failed_rebuild() {
        use openrigcue_effects::shakers::engine_sound::EngineError;
        use openrigcue_effects::shakers::rig::Voice;
        use openrigcue_model::native_value::LegacyValue;
        let state = gain_profile(4.into(), 2.into());
        let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), true);
        state
            .shaker_layout
            .write()
            .value
            .as_dict_mut()
            .unwrap()
            .get_mut("CE")
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .get_mut("sfx")
            .unwrap()
            .assign_item("gear", LegacyValue::from("9"))
            .unwrap();
        let mut raw = detached_native_tune(&state.profile.read());
        raw.as_dict_mut()
            .unwrap()
            .get_mut("shaker_dsp_type")
            .unwrap()
            .assign_item("engine", LegacyValue::None)
            .unwrap();
        {
            let mut profile = state.profile.write();
            profile.replace_native_tune(raw);
            profile.generation += 1;
        }
        state.config.write().update(|configuration| configuration.settings.shakers.sound_card = "fake-card".into());
        shakers.lock().set_soundcard_with(&state, &|| vec!["fake-card".into()], opened).unwrap();
        let on = FrameState { output_ready: true, game_running: true, switch_on: true };
        let mut telemetry = openrigcue_model::Telemetry::off();
        telemetry.harmonized_gear = 2.0;
        {
            let mut guard = shakers.lock();
            assert!(
                guard.system.levels().required_of(Group::Engine).is_ok(),
                "a completed mix row is not a committed group"
            );
            assert!(guard.system.levels().required_of(Group::Gear).is_err());
            for inactive in [
                FrameState { output_ready: false, ..on },
                FrameState { game_running: false, ..on },
                FrameState { switch_on: false, ..on },
            ] {
                guard.system.frame(&telemetry, inactive).unwrap();
            }
            let error = guard.system.frame(&telemetry, on).unwrap_err();
            assert!(matches!(error, EngineError::DspParameter { parameter: "channel_group", error, .. }
                if error.class == "TypeError" && error.message == "list indices must be integers or slices, not str"));
            assert!(!guard.system.mixer().lock().unwrap().is_playing(Voice::Transmission));
        }
        let set_values = |matrix: LegacyValue, dsp: LegacyValue| {
            state
                .shaker_layout
                .write()
                .value
                .as_dict_mut()
                .unwrap()
                .get_mut("CE")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .get_mut("sfx")
                .unwrap()
                .assign_item("gear", matrix)
                .unwrap();
            let mut raw = detached_native_tune(&state.profile.read());
            raw.as_dict_mut().unwrap().get_mut("shaker_dsp_type").unwrap().assign_item("engine", dsp).unwrap();
            let mut profile = state.profile.write();
            profile.replace_native_tune(raw);
            profile.generation += 1;
        };
        set_values(LegacyValue::Int(9), LegacyValue::Int(0));
        shakers.load_channel_groups(&state).unwrap();
        {
            let mut guard = shakers.lock();
            guard.system.frame(&telemetry, on).unwrap();
            let mixer = guard.system.mixer();
            let mut mixer = mixer.lock().unwrap();
            assert!(mixer.is_playing(Voice::Transmission), "cold failure did not remember gear");
            mixer.render(&mut [0.0; 28_800]);
            assert!(!mixer.is_playing(Voice::Transmission));
        }
        set_values(LegacyValue::from("9"), LegacyValue::None);
        assert!(shakers.load_channel_groups(&state).is_err());
        telemetry.harmonized_gear = 3.0;
        {
            let mut guard = shakers.lock();
            let error = guard.system.frame(&telemetry, on).unwrap_err();
            assert!(
                matches!(error, EngineError::DspParameter { parameter: "A_FILTER", error, .. }
                if error.message == "int() argument must be a string, a bytes-like object or a real number, not 'NoneType'"),
                "a later failed rebuild retains the already committed dictionary, so scalar conversion is reached"
            );
            assert!(!guard.system.mixer().lock().unwrap().is_playing(Voice::Transmission));
        }
        set_values(LegacyValue::Int(9), LegacyValue::Int(0));
        shakers.load_channel_groups(&state).unwrap();
        let mut guard = shakers.lock();
        guard.system.frame(&telemetry, on).unwrap();
        assert!(guard.system.mixer().lock().unwrap().is_playing(Voice::Transmission));
    }

    fn opened(_: &str, _: &ShakerSystem) -> Result<OpenOutput, String> {
        Ok(Box::new(()))
    }

    /// An open card whose stream loses its device asks for the sound card to be selected again
    /// (the sound card refresh request), once, from the shaker loop. Before, a removed or reconfigured
    /// output and a restarted sound server left the shakers silent with the card still shown as open.
    #[test]
    fn lost_output_asks_for_the_sound_card_to_be_selected_again() {
        let state = state();
        let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), true);
        let accessories = Accessories::new(std::time::Instant::now());
        let list = || vec!["None".to_owned(), "fake-card".to_owned()];
        state.config.write().update(|c| c.settings.shakers.sound_card = "fake-card".into());
        let long_ago = std::time::Instant::now() - 2 * LOST_OUTPUT_MIN_LIFE;
        let open = |shakers: &ShakersHandle, opened_at| {
            let flag = Arc::new(AtomicBool::new(false));
            let mut guard = shakers.lock();
            guard.set_soundcard_with(&state, &list, opened).unwrap();
            guard.output_lost = Some(LostOutput { flag: Arc::clone(&flag), opened: opened_at, quick: false });
            flag
        };

        let flag = open(&shakers, long_ago);
        shaker_pass(&state, &shakers, &accessories);
        assert!(!shakers.soundcard_refresh_requested(), "a playing output asks for nothing");
        flag.store(true, Ordering::SeqCst);
        shaker_pass(&state, &shakers, &accessories);
        assert!(shakers.soundcard_refresh_requested());
        shakers.clear_soundcard_refresh();
        shaker_pass(&state, &shakers, &accessories);
        assert!(!shakers.soundcard_refresh_requested(), "one request per lost output");

        // The UI's reselection opens the card again; that output is watched in turn.
        let flag = open(&shakers, long_ago);
        flag.store(true, Ordering::SeqCst);
        assert!(shakers.lock().follow_lost_output());
        shakers.clear_soundcard_refresh();

        // Closing the card forgets its flag: a late report from the old stream reopens nothing.
        let flag = open(&shakers, long_ago);
        state.config.write().update(|c| c.settings.shakers.sound_card = "None".into());
        shakers.lock().set_soundcard_with(&state, &list, |_, _| panic!("None must not open an output")).unwrap();
        flag.store(true, Ordering::SeqCst);
        shaker_pass(&state, &shakers, &accessories);
        assert!(!shakers.soundcard_refresh_requested());

        // An output lost right after opening is opened again only once it would have been two seconds old,
        // and after three such losses in a row not at all: no reopening loop.
        state.config.write().update(|c| c.settings.shakers.sound_card = "fake-card".into());
        let age = |shakers: &ShakersHandle, by: Duration| {
            let mut guard = shakers.lock();
            let lost = guard.output_lost.as_mut().unwrap();
            lost.opened -= by;
        };
        for _ in 0..LOST_OUTPUT_QUICK_RETRIES {
            let flag = open(&shakers, std::time::Instant::now());
            flag.store(true, Ordering::SeqCst);
            shaker_pass(&state, &shakers, &accessories);
            assert!(!shakers.soundcard_refresh_requested(), "not before the output's second second");
            age(&shakers, LOST_OUTPUT_MIN_LIFE);
            shaker_pass(&state, &shakers, &accessories);
            assert!(shakers.soundcard_refresh_requested());
            shakers.clear_soundcard_refresh();
        }
        let flag = open(&shakers, std::time::Instant::now());
        flag.store(true, Ordering::SeqCst);
        shaker_pass(&state, &shakers, &accessories);
        assert!(shakers.lock().output_lost.is_none(), "the fourth quick loss is the last one followed");
        assert!(!shakers.soundcard_refresh_requested());
        // An output that plays for a while starts the count again.
        let flag = open(&shakers, long_ago);
        shaker_pass(&state, &shakers, &accessories);
        assert_eq!(shakers.lock().quick_losses, 0);
        flag.store(true, Ordering::SeqCst);
        assert!(shakers.lock().follow_lost_output());
        shakers.clear_soundcard_refresh();
    }

    /// While the sound device monitor runs, a lost output raises the monitor's reselection flag and its audio
    /// event instead of asking for the reload itself: the monitor's pass asks once for the lost stream and
    /// for the card list the same event changed. Before, both asked, and a device that could not be reopened
    /// was reported twice.
    #[test]
    fn lost_output_goes_through_the_running_device_monitor() {
        let state = state();
        let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), true);
        let accessories = Accessories::new(std::time::Instant::now());
        let list = || vec!["None".to_owned(), "fake-card".to_owned()];
        state.config.write().update(|c| c.settings.shakers.sound_card = "fake-card".into());
        let reselect = Arc::new(AtomicBool::new(false));
        let event = Arc::new(AudioEvent::default());
        let lose = |shakers: &ShakersHandle| {
            let flag = Arc::new(AtomicBool::new(true));
            let mut guard = shakers.lock();
            guard.set_soundcard_with(&state, &list, opened).unwrap();
            let opened_at = std::time::Instant::now() - 2 * LOST_OUTPUT_MIN_LIFE;
            guard.output_lost = Some(LostOutput { flag, opened: opened_at, quick: false });
        };
        shakers.lock().set_device_event(Some(Arc::clone(&event)));
        shakers.lock().set_device_monitor(Some(Arc::clone(&reselect)));
        lose(&shakers);
        event.clear();
        shaker_pass(&state, &shakers, &accessories);
        assert!(reselect.load(Ordering::SeqCst), "the monitor is asked to reselect");
        assert!(event.wait(Duration::ZERO), "and woken");
        assert!(!shakers.soundcard_refresh_requested(), "the reload itself is the monitor's request");
        shaker_pass(&state, &shakers, &accessories);
        assert!(shakers.lock().output_lost.is_none(), "one report per lost output");

        // A monitor that polls has no event; one that ended leaves the direct request.
        reselect.store(false, Ordering::SeqCst);
        shakers.lock().set_device_event(None);
        lose(&shakers);
        shaker_pass(&state, &shakers, &accessories);
        assert!(reselect.load(Ordering::SeqCst) && !shakers.soundcard_refresh_requested());
        shakers.lock().set_device_monitor(None);
        lose(&shakers);
        shaker_pass(&state, &shakers, &accessories);
        assert!(shakers.soundcard_refresh_requested());
    }

    /// What the centre speaker carries in each of `blocks` 1024-frame mixer blocks.
    fn centre_frames(shakers: &ShakersHandle, blocks: usize) -> Vec<usize> {
        let mixer = shakers.lock().system.mixer();
        (0..blocks)
            .map(|_| {
                let mut out = vec![0.0_f32; 1024 * 6];
                mixer.lock().unwrap().render(&mut out);
                out.chunks(6).filter(|frame| frame[Speaker::Center.index()] != 0.0).count()
            })
            .collect()
    }

    /// The schedule behind an effect test that sometimes makes no sound. The test (UI thread) starts the
    /// Impact sound in one of its frames; the shaker loop goes on running a shake frame every refresh period
    /// with the live telemetry, and its next frame releases that sound: with the test listener still stored
    /// the frame is active and the idle telemetry gives impact file 0 (the impact sound is released), and once the
    /// telemetry loop has written `'none'` back the inactive branch releases every sound. The mixer admits a
    /// start or stop at the start of its next 1024-frame block, so what is heard is the whole blocks mixed in
    /// between: nothing, one block, or the sound to its end.
    #[test]
    fn refresh_frame_releases_an_effect_test_sound_at_the_next_block() {
        let impact_frame = effect_test_frames().into_iter().nth(2).unwrap();
        assert_eq!(impact_frame.input.longitudinal_acceleration, 99.0, "the first of the two impact frames");
        let prepared = || {
            let state = state();
            let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), true);
            state.config.write().update(|c| c.settings.shakers.sound_card = "fake-card".into());
            *state.telemetry_ready.write() = true;
            shakers.lock().set_soundcard_with(&state, &|| vec!["fake-card".into()], opened).unwrap();
            shakers.rebuild_levels(&state, Some(Effect::Impact), |_| {}).unwrap();
            state.session.write().listener = Some("test".into());
            run_live_shaker_frame(&state, &shakers, &openrigcue_model::Telemetry::off(), Some(&impact_frame), |_| {})
                .unwrap();
            assert!(shakers
                .lock()
                .system
                .mixer()
                .lock()
                .unwrap()
                .is_playing(openrigcue_effects::shakers::rig::Voice::Impact));
            (state, shakers)
        };
        let accessories = Accessories::new(std::time::Instant::now());

        // No refresh frame in between (a slow refresh rate): the sound plays to its end.
        let (_state, shakers) = prepared();
        let whole = centre_frames(&shakers, 6);
        assert!(whole[0] > 1000 && whole[1] > 0, "{whole:?}");
        let length: usize = whole.iter().sum();

        // The refresh frame comes before the next block: nothing of the sound is mixed.
        for listener in [Some("test".to_owned()), None] {
            let (state, shakers) = prepared();
            state.session.write().listener = listener;
            shaker_pass(&state, &shakers, &accessories);
            assert!(!shakers
                .lock()
                .system
                .mixer()
                .lock()
                .unwrap()
                .is_playing(openrigcue_effects::shakers::rig::Voice::Impact));
            assert_eq!(centre_frames(&shakers, 6).iter().sum::<usize>(), 0);
        }

        // One block is mixed first: exactly that block is heard.
        let (state, shakers) = prepared();
        let first = centre_frames(&shakers, 1);
        shaker_pass(&state, &shakers, &accessories);
        let rest = centre_frames(&shakers, 5);
        assert_eq!(first, [whole[0]]);
        assert_eq!(rest.iter().sum::<usize>(), 0, "{rest:?}");
        assert!(first[0] < length, "the released sound is shorter than the whole one ({first:?} of {length})");
    }

    #[test]
    fn live_shaker_snapshot_output_assignments_remain_visible_during_processing() {
        let state = state();
        let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), true);
        let mut processing = shakers.lock();
        let list = || vec!["None".to_owned(), "fake-card".to_owned()];
        state.config.write().update(|c| c.settings.shakers.sound_card = "fake-card".into());
        processing.set_soundcard_with(&state, &list, opened).unwrap();
        assert!(shakers.status().output_ready);
        processing
            .set_soundcard_with(&state, &list, |_, _| {
                assert!(!shakers.status().output_ready, "closing the old output precedes opening its replacement");
                Err("fake init failure".into())
            })
            .unwrap_err();
        assert!(!shakers.status().output_ready);
        processing.set_soundcard_with(&state, &list, opened).unwrap();
        state.config.write().update(|c| c.settings.shakers.sound_card = "missing-card".into());
        assert_eq!(
            processing.set_soundcard_with(&state, &list, opened),
            Err(SoundcardError::NotFound("missing-card".into()))
        );
        assert!(!shakers.status().output_ready);
        state.config.write().update(|c| c.settings.shakers.sound_card = "None".into());
        processing.set_soundcard_with(&state, &list, |_, _| panic!("None must not open an output")).unwrap();
        assert!(!shakers.status().output_ready);
    }

    #[test]
    fn live_shaker_snapshot_sample_metadata_and_refresh_follow_existing_assignments() {
        let state = state();
        let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), false);
        let fixture = srs_install_with_one_sample();
        let bank = SampleBank::import(fixture.path()).unwrap().0;
        let mut processing = shakers.lock();
        assert!(!shakers.status().srs_sounds_available);
        processing.set_srs_sounds(Some(fixture.path().to_path_buf()));
        processing.install_sounds(ShakerSounds {
            from: Some(fixture.path().to_path_buf()),
            bank,
            engine: EngineVoice::synth(MIXER_RATE),
            beep_file: None,
            beep_location: None,
            beep: Box::new(SilentBeep),
        });
        let mixer = processing.system.mixer();
        let audio = mixer.lock().unwrap();
        let status = shakers.status();
        assert!(status.srs_sounds_available);
        assert_eq!(status.imported_samples, 1);
        assert_eq!(status.sample_source, ActiveSamples::SrsImport);
        shakers.request_soundcard_refresh();
        assert!(shakers.status().refresh_soundcard);
        assert!(processing.take_soundcard_refresh());
        assert!(!shakers.status().refresh_soundcard);
        processing.set_srs_sounds(None);
        assert!(!shakers.status().srs_sounds_available);
        assert_eq!(shakers.status().imported_samples, 1, "the installed bank remains until it is replaced");
        drop(audio);
        processing.install_sounds(ShakerSounds {
            from: None,
            bank: SampleBank::synthesized(),
            engine: EngineVoice::synth(MIXER_RATE),
            beep_file: None,
            beep_location: None,
            beep: Box::new(SilentBeep),
        });
        assert_eq!(shakers.status().sample_source, ActiveSamples::BuiltIn);
        assert_eq!(shakers.status().imported_samples, 0);
    }

    /// Selecting a sound card: a listed card is opened, the groups are rebuilt and the monitor's event is set; `None`
    /// closes it and sets the event without a rebuild; a card missing from the driver list or failing
    /// to open is an error, leaves the card closed, rebuilds nothing and sets no event.
    #[test]
    fn output_recreation_closes_voices_before_every_open_and_retains_closed_state_on_failure() {
        use openrigcue_effects::shakers::rig::{Action, Plays, Voice};
        use openrigcue_effects::shakers::samples::SampleFolder;
        let state = state();
        let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), true);
        let list = || vec!["first".into(), "second".into()];
        struct OwnedOutput(
            std::sync::Arc<std::sync::atomic::AtomicUsize>,
            std::sync::Arc<std::sync::Mutex<openrigcue_effects::shakers::mixer::Mixer>>,
        );
        impl Drop for OwnedOutput {
            fn drop(&mut self) {
                assert!(self.1.try_lock().is_ok(), "output teardown must precede the mixer lock");
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut host = shakers.lock();
        let start = |system: &ShakerSystem| {
            system
                .mixer()
                .lock()
                .unwrap()
                .apply(&[Action::Start {
                    voice: Voice::Transmission,
                    sample: SampleKey::new(SampleFolder::Transmission, 1),
                    plays: Plays::Forever,
                    group: Group::Gear,
                }])
                .unwrap();
        };
        host.output = Some(Box::new(OwnedOutput(dropped.clone(), host.system.mixer())));
        let host_published = host.published.clone();
        for (index, (name, fail)) in
            [("first", false), ("first", false), ("second", false), ("first", true)].into_iter().enumerate()
        {
            start(&host.system);
            assert!(host.system.mixer().lock().unwrap().is_playing(Voice::Transmission));
            state.config.write().update(|c| c.settings.shakers.sound_card = name.into());
            let result = host.set_soundcard_with(&state, &list, |_, system| {
                assert_eq!(dropped.load(Ordering::SeqCst), index + 1);
                assert!(!host_published.read().output_ready);
                let mixer = system.mixer();
                let mut audio = mixer.lock().unwrap();
                assert!(!audio.is_playing(Voice::Transmission));
                assert!(!audio.engine_playing());
                audio.set_sample_rate(96000);
                if fail {
                    Err("fixture init".into())
                } else {
                    Ok(Box::new(OwnedOutput(dropped.clone(), mixer.clone())) as OpenOutput)
                }
            });
            assert_eq!(result.is_err(), fail);
            assert!(!host.system.mixer().lock().unwrap().is_playing(Voice::Transmission));
        }
        start(&host.system);
        state.config.write().update(|c| c.settings.shakers.sound_card = "missing".into());
        assert!(host.set_soundcard_with(&state, &list, |_, _| panic!("missing must not open")).is_err());
        assert!(!host.system.mixer().lock().unwrap().is_playing(Voice::Transmission));
        start(&host.system);
        state.config.write().update(|c| c.settings.shakers.sound_card = "None".into());
        host.set_soundcard_with(&state, &list, |_, _| panic!("None must not open")).unwrap();
        assert!(!host.system.mixer().lock().unwrap().is_playing(Voice::Transmission));
    }

    #[test]
    fn set_soundcard_opens_rebuilds_and_raises() {
        let state = state();
        let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), true);
        let event = Arc::new(AudioEvent::default());
        shakers.lock().set_device_event(Some(Arc::clone(&event)));
        let list = || vec!["None".to_owned(), "hw:CARD=X4,DEV=0".to_owned()];
        let card = |name: &str| state.config.write().update(|c| c.settings.shakers.sound_card = name.into());
        let edit_gear = |level: f64| {
            let mut profile = state.profile.write();
            profile.tune.shaker_effects.gear = level;
            profile.generation += 1;
        };

        card("hw:CARD=X4,DEV=0");
        edit_gear(3.0);
        assert_eq!(shakers.lock().set_soundcard_with(&state, &list, opened), Ok(()));
        assert!(shakers.lock().output_ready());
        assert_eq!(shakers.lock().system.levels(), &expected_levels(&state), "_load_channel_groups_unlocked");
        assert!(event.wait(Duration::ZERO));
        event.clear();

        card("None");
        edit_gear(7.0);
        let before = shakers.lock().system.levels().clone();
        assert_eq!(shakers.lock().set_soundcard_with(&state, &list, opened), Ok(()));
        assert!(!shakers.lock().output_ready());
        assert_eq!(shakers.lock().system.levels(), &before, "no rebuild when deactivating");
        assert!(event.wait(Duration::ZERO));
        event.clear();

        for name in ["Gone", ""] {
            card(name);
            assert_eq!(
                shakers.lock().set_soundcard_with(&state, &list, opened),
                Err(SoundcardError::NotFound(name.to_owned()))
            );
            assert!(!shakers.lock().output_ready());
        }
        card("hw:CARD=X4,DEV=0");
        let failing = |_: &str, _: &ShakerSystem| -> Result<OpenOutput, String> { Err("device busy".into()) };
        assert_eq!(
            shakers.lock().set_soundcard_with(&state, &list, failing),
            Err(SoundcardError::Init("device busy".into()))
        );
        assert!(!shakers.lock().output_ready());
        assert_eq!(shakers.lock().system.levels(), &before, "no rebuild after a failure");
        assert!(!event.wait(Duration::ZERO), "no event after a failure");
    }

    #[test]
    fn native_group_rebuild_soundcard_catch_keeps_open_output_and_publishes_event() {
        use openrigcue_model::native_value::LegacyValue;
        let state = gain_profile(4.into(), 2.into());
        let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), true);
        let event = Arc::new(AudioEvent::default());
        shakers.lock().set_device_event(Some(Arc::clone(&event)));
        let mut native = detached_native_tune(&state.profile.read());
        native
            .as_dict_mut()
            .unwrap()
            .get_mut("shaker_volume")
            .unwrap()
            .assign_item("fl", LegacyValue::from("7"))
            .unwrap();
        state.profile.write().replace_native_tune(native);
        state.config.write().update(|config| config.settings.shakers.sound_card = "fake-card".into());
        let list = || vec!["None".into(), "fake-card".into()];
        shakers.lock().set_soundcard_with(&state, &list, opened).unwrap();
        assert!(shakers.status().output_ready, "the caught group failure does not undo mixer initialization");
        assert!(event.wait(Duration::ZERO));
        assert_eq!(shakers.lock().system.levels().required_of(Group::Engine).unwrap_err().message, "'engine'");
    }

    #[test]
    fn native_group_rebuild_effect_test_failure_precedes_listener_stores_and_frames() {
        use openrigcue_model::native_value::LegacyValue;
        let state = gain_profile(4.into(), 2.into());
        state.shaker_layout.write().value = LegacyValue::None;
        state.session.write().listener = Some("original-listener".into());
        let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), false);
        let error = test_effect(&state, &shakers, Effect::Gear).unwrap_err();
        assert_eq!(error.detail(), "TypeError: 'NoneType' object is not subscriptable");
        assert_eq!(state.session.read().listener.as_deref(), Some("original-listener"));
        assert!(!shakers
            .lock()
            .system
            .mixer()
            .lock()
            .unwrap()
            .is_playing(openrigcue_effects::shakers::rig::Voice::Transmission));
    }

    #[test]
    fn native_test_sfx_frame_error_keeps_native_detail_and_listener_prefix() {
        use openrigcue_model::native_value::LegacyValue;
        let mut raw = LegacyValue::from(FactoryTunes::builtin().get("ac").unwrap().to_json());
        raw.as_dict_mut().unwrap().insert("shaker_engine_type", LegacyValue::from("X"));
        let state = dsp_native_profile(raw);
        let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), true);
        state.config.write().update(|configuration| configuration.settings.shakers.sound_card = "fake-card".into());
        shakers.lock().set_soundcard_with(&state, &|| vec!["fake-card".into()], opened).unwrap();
        state.session.write().listener = None;
        let mut sleeps = Vec::new();
        let result = test_effect_with_sleeper(
            &state,
            &shakers,
            Effect::Gear,
            None,
            Some("captured-test-listener".into()),
            |duration| sleeps.push(duration),
        );
        assert!(result.is_err(), "the source frame error must propagate to the synchronous callback");
        assert_eq!(result.unwrap_err().detail(), "KeyError: 'X'");
        assert!(sleeps.is_empty(), "the first test frame already stores the test listener and raises");
        assert_eq!(state.session.read().listener.as_deref(), Some("captured-test-listener"));
        assert_eq!(shakers.lock().system.levels().of(Group::Impact), [0.0; 6], "final normal rebuild was not reached");
    }

    #[test]
    fn native_test_sfx_refreshes_required_settings_before_focus_rebuild() {
        use openrigcue_model::native_value::LegacyValue;
        let state = gain_profile(4.into(), 2.into());
        let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), true);
        state.config.write().update(|configuration| configuration.settings.shakers.sound_card = "fake-card".into());
        shakers.lock().set_soundcard_with(&state, &|| vec!["fake-card".into()], opened).unwrap();
        let mut raw = detached_native_tune(&state.profile.read());
        raw.as_dict_mut().unwrap().get_mut("shaker_volume").unwrap().assign_item("fl", LegacyValue::from("7")).unwrap();
        {
            let mut profile = state.profile.write();
            profile.replace_native_tune(raw);
            profile.generation += 1;
        }
        state.session.write().listener = Some("runtime-listener".into());
        let mut sleeps = Vec::new();
        let result = test_effect_with_sleeper(
            &state,
            &shakers,
            Effect::Gear,
            Some("captured-current".into()),
            Some("captured-test".into()),
            |duration| sleeps.push(duration),
        );
        assert!(result.is_err(), "test_sfx rebuild reads the just-assigned native profile before any frame");
        assert_eq!(result.unwrap_err().detail(), "TypeError: unsupported operand type(s) for /: 'str' and 'float'");
        assert!(sleeps.is_empty());
        assert_eq!(state.session.read().listener.as_deref(), Some("runtime-listener"));
    }

    #[test]
    fn native_test_sfx_uses_captured_listeners_and_rewrites_test_listener_for_each_frame() {
        let state = gain_profile(4.into(), 2.into());
        let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), true);
        state.config.write().update(|configuration| configuration.settings.shakers.sound_card = "fake-card".into());
        shakers.lock().set_soundcard_with(&state, &|| vec!["fake-card".into()], opened).unwrap();
        state.session.write().listener = Some("runtime-at-dispatch".into());
        let mut observations = Vec::new();
        test_effect_with_sleeper(
            &state,
            &shakers,
            Effect::Gear,
            Some("captured-current".into()),
            Some("captured-test".into()),
            |duration| {
                observations.push((duration, state.session.read().listener.clone()));
                if observations.len() < 12 {
                    state.session.write().listener = Some("background-update".into());
                }
            },
        )
        .unwrap();
        assert_eq!(observations.len(), 12);
        assert!(observations[..11]
            .iter()
            .all(|(duration, listener)| *duration == TEST_FRAME_GAP && listener.as_deref() == Some("captured-test")));
        assert_eq!(observations[11], (TEST_FRAME_GAP, Some("captured-current".into())));
        assert_eq!(state.session.read().listener.as_deref(), Some("captured-current"));
        assert!(
            shakers.lock().system.levels().of(Group::Impact)[Speaker::Center.index()] > 0.0,
            "the normal rebuild follows the thirteenth successful frame"
        );
    }

    #[test]
    fn native_test_sfx_refreshes_profile_before_later_frames_without_restoring_after_error() {
        use openrigcue_model::native_value::LegacyValue;
        let state = gain_profile(4.into(), 2.into());
        let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), true);
        state.config.write().update(|configuration| configuration.settings.shakers.sound_card = "fake-card".into());
        shakers.lock().set_soundcard_with(&state, &|| vec!["fake-card".into()], opened).unwrap();
        let mut sleeps = Vec::new();
        let result = test_effect_with_sleeper(
            &state,
            &shakers,
            Effect::Gear,
            Some("captured-current".into()),
            Some("captured-test".into()),
            |duration| {
                sleeps.push(duration);
                let mut raw = detached_native_tune(&state.profile.read());
                raw.as_dict_mut().unwrap().insert("shaker_engine_type", LegacyValue::from("X"));
                let mut profile = state.profile.write();
                profile.replace_native_tune(raw);
                profile.generation += 1;
            },
        );
        assert_eq!(result.unwrap_err().detail(), "KeyError: 'X'");
        assert_eq!(sleeps, [TEST_FRAME_GAP]);
        assert_eq!(state.session.read().listener.as_deref(), Some("captured-test"));
        assert_eq!(shakers.lock().system.levels().of(Group::Impact), [0.0; 6]);
    }

    #[test]
    fn native_test_sfx_final_rebuild_failure_follows_listener_restoration() {
        use openrigcue_model::native_value::LegacyValue;
        let state = gain_profile(4.into(), 2.into());
        let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), true);
        state.config.write().update(|configuration| configuration.settings.shakers.sound_card = "fake-card".into());
        shakers.lock().set_soundcard_with(&state, &|| vec!["fake-card".into()], opened).unwrap();
        let mut sleeps = 0;
        let result = test_effect_with_sleeper(
            &state,
            &shakers,
            Effect::Gear,
            Some("captured-current".into()),
            Some("captured-test".into()),
            |_| {
                sleeps += 1;
                if sleeps == 12 {
                    let mut raw = detached_native_tune(&state.profile.read());
                    raw.as_dict_mut()
                        .unwrap()
                        .get_mut("shaker_volume")
                        .unwrap()
                        .assign_item("fl", LegacyValue::from("7"))
                        .unwrap();
                    let mut profile = state.profile.write();
                    profile.replace_native_tune(raw);
                    profile.generation += 1;
                }
            },
        );
        assert_eq!(result.unwrap_err().detail(), "TypeError: unsupported operand type(s) for /: 'str' and 'float'");
        assert_eq!(sleeps, 12);
        assert_eq!(state.session.read().listener.as_deref(), Some("captured-current"));
        assert!(shakers.lock().system.levels().required_of(Group::Engine).is_err());
    }

    #[test]
    fn native_test_sfx_error_detail_uses_only_explicit_native_metadata() {
        use openrigcue_effects::shakers::engine_sound::EngineError;
        use openrigcue_effects::shakers::fmod::FmodError;
        for (error, detail) in [
            (
                EngineError::EngineSelector { error: DspError { class: "KeyError", message: "'X'".into() } },
                "KeyError: 'X'",
            ),
            (
                EngineError::InvalidDspFloat { group: Group::Engine, parameter: "A_Q" },
                "pyfmodex.exceptions.FmodError: INVALID FLOAT",
            ),
            (
                EngineError::Fmod(FmodError::Call { call: "fake", code: 30 }),
                "pyfmodex.exceptions.FmodError: INVALID HANDLE",
            ),
            (EngineError::Fmod(FmodError::Call { call: "fake", code: 999 }), "fake failed with FMOD error 999"),
        ] {
            assert_eq!(ShakerTestError::Frame(error).detail(), detail);
        }
        let error =
            EngineError::DiagnosticOutput(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "fake output failure"));
        let fallback = error.to_string();
        assert_eq!(
            ShakerTestError::Frame(error).detail(),
            fallback,
            "host OS error text is not guessed into a native exception"
        );
    }

    /// The effect test: the test listener (`test` with no game) is in the shared session listener before frames
    /// 1-12 and the real one after the 12th; 12 sleeps of 1/15 s.
    #[test]
    fn test_effect_stores_the_test_listener() {
        let state = state();
        let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), false);
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let watcher = {
            let (state, seen, stop) = (state.clone(), seen.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    let listener = state.session.read().listener.clone();
                    let mut seen = seen.lock().unwrap();
                    if seen.last() != Some(&listener) {
                        seen.push(listener);
                    }
                    drop(seen);
                    std::thread::sleep(Duration::from_millis(5));
                }
            })
        };
        let started = std::time::Instant::now();
        test_effect(&state, &shakers, Effect::Gear).unwrap();
        let took = started.elapsed();
        stop.store(true, Ordering::SeqCst);
        watcher.join().unwrap();
        // The sleeps are real here, so the run cannot be shorter than twelve of them. How much longer it is
        // depends on how long the frames wait for locks other threads hold, so the number and length of the
        // sleeps are counted below instead of bounding the elapsed time from above.
        assert!(took >= TEST_FRAME_GAP * 12, "{took:?}");
        assert_eq!(state.session.read().listener, None, "the real listener is back");
        assert!(seen.lock().unwrap().contains(&Some("test".to_owned())));
        // The same run with the listeners `test_effect` captures (none running, so `'test'`) and a recording
        // sleeper: exactly 12 sleeps of the frame gap, the test listener stored at the first eleven and the
        // real one back at the twelfth.
        let mut sleeps = Vec::new();
        test_effect_with_sleeper(&state, &shakers, Effect::Gear, None, Some("test".into()), |duration| {
            sleeps.push((duration, state.session.read().listener.clone()));
        })
        .unwrap();
        assert_eq!(sleeps.len(), 12);
        assert!(sleeps[..11]
            .iter()
            .all(|(duration, listener)| *duration == TEST_FRAME_GAP && listener.as_deref() == Some("test")));
        assert_eq!(sleeps[11], (TEST_FRAME_GAP, None));
        assert_eq!(state.session.read().listener, None, "the real listener is back");
        assert!(!effect_test_allowed(&state), "SHAKEBOX.soundcard 'None'");
        state.config.write().update(|c| c.settings.shakers.sound_card = "hw:CARD=X,DEV=0".into());
        assert!(effect_test_allowed(&state), "a selected card, open or not");
    }

    /// Loading a layout name the layout store lacks is an error: the layout in use stays.
    #[test]
    fn unknown_speaker_layout_keeps_the_current_one() {
        let state = state();
        let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), false);
        let before = shakers.lock().system.profile().clone();
        state.config.write().update(|c| c.settings.shakers.speaker_layout = "No Such Layout".into());
        assert!(crate::shaker_layout::load(&state).is_err());
        assert_eq!(shaker_settings(&state).profile.as_ref(), Some(&before));
        shakers.lock().follow_settings(&state);
        assert_eq!(shakers.lock().system.profile(), &before);
    }

    /// The status field names the web UI reads.
    #[test]
    fn status_json_names() {
        let value = serde_json::to_value(BUILT_IN).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "output_ready": false,
                "is_fmod": false,
                "sample_source": "built_in",
                "imported_samples": 0,
                "srs_sounds_available": true,
                "refresh_soundcard": false,
            })
        );
        assert_eq!(serde_json::to_value(ActiveSamples::SrsImport).unwrap(), "srs_import");
    }
}
