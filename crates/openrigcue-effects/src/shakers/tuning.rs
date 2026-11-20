//! User settings the shaker engine reads: the per-game effect profile (the tune's shaker keys)
//! and the `config.ini` `[SHAKEBOX]` section.

use super::layout::{Effect, PerGroup, PerSpeaker, Speaker};

/// The required raw engine-type key, kept until the late instance lookup.
pub type NativeEngineSelector = Result<openrigcue_model::native_value::LegacyValue, DspError>;

/// A raw lookup/conversion failure, deferred until the step that needs the value is reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DspError {
    pub class: &'static str,
    pub message: String,
}

/// The error classes of the shared model's raw-value operations (`"Class: message"`).
const NATIVE_CLASSES: [&str; 11] = [
    "TypeError",
    "ValueError",
    "OverflowError",
    "ZeroDivisionError",
    "MemoryError",
    "KeyError",
    "IndexError",
    "AttributeError",
    "RecursionError",
    "BufferError",
    "ArithmeticError",
];

impl DspError {
    /// The error of a failed model operation (`py_*`, `py_integer`, `py_float_native`,
    /// `native_equal_checked`). Its text is `"Class: message"`, or the bare class.
    pub fn from_native(error: String) -> DspError {
        let (head, message) = error.split_once(": ").unwrap_or((error.as_str(), ""));
        match NATIVE_CLASSES.iter().find(|class| **class == head) {
            Some(class) => DspError { class, message: message.to_owned() },
            // The model words every failure with its class; keep any other text whole.
            None => DspError { class: "TypeError", message: error },
        }
    }
}

/// The entry `key` of a raw profile or SHAKEBOX value:
/// object handles are resolved first, a dictionary without the key is a key error naming the key
/// and any other value a type error naming its type.
pub fn native_item(
    value: &openrigcue_model::native_value::LegacyValue,
    key: &str,
) -> Result<openrigcue_model::native_value::LegacyValue, DspError> {
    use openrigcue_model::native_value::LegacyValue;
    let lookup = |value: &LegacyValue| {
        value.item(key).cloned().map_err(|message| DspError {
            class: if matches!(value, LegacyValue::Dict(_)) { "KeyError" } else { "TypeError" },
            message,
        })
    };
    // Only an object handle needs its (shallow) resolution; a plain value is borrowed, so a
    // lookup copies just the child it returns.
    match value {
        LegacyValue::Object(_) => lookup(&value.resolved()),
        value => lookup(value),
    }
}

/// A nested entry, as consecutive [`native_item`] lookups, borrowing through plain
/// containers and copying only at object handles and the returned leaf.
pub fn native_path(
    value: &openrigcue_model::native_value::LegacyValue,
    path: &[&str],
) -> Result<openrigcue_model::native_value::LegacyValue, DspError> {
    use openrigcue_model::native_value::LegacyValue;
    use std::borrow::Cow;
    let class = |value: &LegacyValue| if matches!(value, LegacyValue::Dict(_)) { "KeyError" } else { "TypeError" };
    let mut current: Cow<'_, LegacyValue> = Cow::Borrowed(value);
    for key in path {
        current = match current {
            Cow::Borrowed(LegacyValue::Object(_)) | Cow::Owned(LegacyValue::Object(_)) => {
                Cow::Owned(native_item(&current, key)?)
            }
            Cow::Borrowed(plain) => {
                Cow::Borrowed(plain.item(key).map_err(|message| DspError { class: class(plain), message })?)
            }
            Cow::Owned(plain) => {
                Cow::Owned(plain.item(key).cloned().map_err(|message| DspError { class: class(&plain), message })?)
            }
        };
    }
    Ok(current.into_owned())
}

/// The ordered raw parameters of one channel group's DSP. Keeping each result separately
/// lets a failed later lookup preserve the parameters already accepted by earlier setters.
#[derive(Debug, Clone, PartialEq)]
pub struct NativeDspSettings {
    pub filter_type: Result<i32, DspError>,
    pub cutoff_hz: Result<f64, DspError>,
    pub q: Result<f64, DspError>,
}

/// One integer `SHAKEBOX` setting, kept as the exact integer (any width).
pub type NativeInt = openrigcue_model::native_value::LegacyValue;

/// Required integer SHAKEBOX reads, deferred until their suspension branch runs. Each holds the
/// exact integer, compared exactly with the frame's floats.
#[derive(Debug, Clone, PartialEq)]
pub struct NativeSuspensionSettings {
    pub side_enhancement: Result<NativeInt, DspError>,
    pub min_side_enhancement: Result<NativeInt, DspError>,
    pub min_suspension_velocity: Result<NativeInt, DspError>,
    pub max_suspension_velocity: Result<NativeInt, DspError>,
    pub min_bump_velocity: Result<NativeInt, DspError>,
    pub max_bump_velocity: Result<NativeInt, DspError>,
}

/// Raw late engine reads. The early DSP converts the flag to an integer; this block compares it to 1.
#[derive(Debug, Clone, PartialEq)]
pub struct NativeEngineSettings {
    pub flag: Result<openrigcue_model::native_value::LegacyValue, DspError>,
    pub fade_start: Result<openrigcue_model::native_value::LegacyValue, DspError>,
    pub fade_end: Result<openrigcue_model::native_value::LegacyValue, DspError>,
    pub resonance: Result<f64, DspError>,
}

/// Flavour of the engine sound (the tune's `shaker_engine_type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineType {
    /// 'O', "Original".
    Original,
    /// 'E', "Enhanced".
    Enhanced,
}

/// The level slider and on/off switch of one effect.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EffectLevel {
    /// The effect's entry of the tune's `shaker_effects`, slider 0..20.
    pub slider: f64,
    /// The effect's entry of the tune's `shaker_switch`.
    pub enabled: bool,
}

/// Settings of one effect (one key of the `shaker_*` dictionaries).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EffectTuning {
    /// `None` when the saved profile lacks the effect's `shaker_effects` or `shaker_switch` entry, or holds
    /// a value the level formula cannot use (a non-number level, a switch that is not an integer).
    /// [`super::levels::GroupLevels::compute`] then applies the fixed fallback.
    pub level: Option<EffectLevel>,
    /// The effect's `shaker_dsp_type` entry is 1: the low-pass of the effect's filter dialog.
    pub low_pass: bool,
    /// The effect's `shaker_cutoff` entry, Hz (slider 30..500).
    pub cutoff_hz: f64,
    /// The effect's `shaker_resonance` entry (slider 1..100); for the engine it is the "Speed" (pitch) control.
    pub resonance: f64,
}

/// The shaker part of a game/vehicle profile.
#[derive(Debug, Clone, PartialEq)]
pub struct ShakerTuning {
    /// Indexed by [`Effect::index`].
    pub effects: [EffectTuning; 18],
    /// Raw level times integer switch, per effect; `None` marks an unreadable entry.
    pub native_effect_gains: Option<Box<[Option<f64>; 18]>>,
    /// Strict raw matrix/volume arithmetic before multiplying the selected effect gain.
    pub native_group_bases: Option<Box<PerGroup<Result<super::layout::PerSpeaker, DspError>>>>,
    /// Eager six-channel raw /20.0 conversion made before a speaker-test group is selected.
    pub native_speaker_test: Option<Box<Result<PerSpeaker, DspError>>>,
    /// Raw profile parameters, before the typed projection's defaults.
    pub native_dsp: Option<Box<PerGroup<NativeDspSettings>>>,
    /// Missing selectors fail before pausing; existing keys are looked up after pausing the other instance.
    pub native_engine_selector: Option<NativeEngineSelector>,
    pub native_engine: Option<Box<NativeEngineSettings>>,
    /// Required max_telemetry_vars.traction_loss, read at the later traction-loss block.
    pub native_traction_max: Option<Result<openrigcue_model::native_value::LegacyValue, DspError>>,
    pub native_suspension: Option<Box<NativeSuspensionSettings>>,
    /// The tune's `shaker_volume` entry of each speaker, slider 0..20, indexed by [`Speaker::index`].
    pub volume: PerSpeaker,
    /// The tune's `shaker_cutoff.engine_fadeoff_start`, % of max rpm.
    pub engine_fade_start: f64,
    /// The tune's `shaker_cutoff.engine_fadeoff_end`, % of max rpm.
    pub engine_fade_end: f64,
    /// The tune's `shaker_engine_type` as the key of the engine instances: `None` for a value
    /// other than 'O' and 'E', which has no engine instance (a key error in the engine block).
    pub engine_type: Option<EngineType>,
}

impl ShakerTuning {
    pub fn effect(&self, effect: Effect) -> &EffectTuning {
        &self.effects[effect.index()]
    }

    pub fn effect_mut(&mut self, effect: Effect) -> &mut EffectTuning {
        &mut self.effects[effect.index()]
    }

    /// The per-game factory tune (or its vehicle
    /// entry) where it differs from [`ShakerTuning::default`]; the common default otherwise.
    pub fn catalog_default(game: &str, vehicle: Option<&str>) -> ShakerTuning {
        let mut t = ShakerTuning::default();
        let key = match vehicle {
            Some(v) => format!("{game}/{v}"),
            None => game.to_string(),
        };
        for (entry, change) in CATALOG_OVERRIDES {
            if *entry == key {
                change.apply(&mut t);
            }
        }
        t
    }
}

impl Default for ShakerTuning {
    /// The value every game shares in the factory tunes: level 10 (road bump 6),
    /// all switches on, no DSP, cutoff 400 Hz, resonance 10 (engine 35), volumes 10 (sub 6),
    /// engine fade 95..100 %, engine type 'E'.
    fn default() -> Self {
        let mut effects = [EffectTuning {
            level: Some(EffectLevel { slider: 10.0, enabled: true }),
            low_pass: false,
            cutoff_hz: 400.0,
            resonance: 10.0,
        }; 18];
        effects[Effect::RoadBump.index()].level = Some(EffectLevel { slider: 6.0, enabled: true });
        effects[Effect::Engine.index()].resonance = 35.0;
        let mut volume = [10.0; 6];
        volume[Speaker::Sub.index()] = 6.0;
        ShakerTuning {
            effects,
            native_effect_gains: None,
            native_group_bases: None,
            native_speaker_test: None,
            native_dsp: None,
            native_engine_selector: None,
            native_engine: None,
            native_traction_max: None,
            native_suspension: None,
            volume,
            engine_fade_start: 95.0,
            engine_fade_end: 100.0,
            engine_type: Some(EngineType::Enhanced),
        }
    }
}

/// One difference of a catalogue entry from the common default.
#[derive(Clone, Copy)]
enum Change {
    Level(Effect, f64),
    Enabled(Effect, bool),
    LowPass(Effect),
    Cutoff(Effect, f64),
    Resonance(Effect, f64),
    Volume(Speaker, f64),
    AllVolumes(f64),
    Fade(f64, f64),
    FadeStart(f64),
    Engine(EngineType),
}

impl Change {
    fn apply(&self, t: &mut ShakerTuning) {
        match *self {
            Change::Level(e, v) => {
                if let Some(level) = &mut t.effect_mut(e).level {
                    level.slider = v;
                }
            }
            Change::Enabled(e, v) => {
                if let Some(level) = &mut t.effect_mut(e).level {
                    level.enabled = v;
                }
            }
            Change::LowPass(e) => t.effect_mut(e).low_pass = true,
            Change::Cutoff(e, v) => t.effect_mut(e).cutoff_hz = v,
            Change::Resonance(e, v) => t.effect_mut(e).resonance = v,
            Change::Volume(s, v) => t.volume[s.index()] = v,
            Change::AllVolumes(v) => t.volume = [v; 6],
            Change::Fade(a, b) => {
                t.engine_fade_start = a;
                t.engine_fade_end = b;
            }
            Change::FadeStart(a) => t.engine_fade_start = a,
            Change::Engine(e) => t.engine_type = Some(e),
        }
    }
}

use Change::*;
use Effect as E;

/// Cutoff 120 Hz on the ten car effects (f12011, f12012, acudp).
const CUTOFF_120: [Change; 10] = [
    Cutoff(E::Deceleration, 120.0),
    Cutoff(E::WheelSlip, 120.0),
    Cutoff(E::LateralAcceleration, 120.0),
    Cutoff(E::RoadBump, 120.0),
    Cutoff(E::Gear, 120.0),
    Cutoff(E::Acceleration, 120.0),
    Cutoff(E::TractionLoss, 120.0),
    Cutoff(E::Engine, 120.0),
    Cutoff(E::Impact, 120.0),
    Cutoff(E::Suspension, 120.0),
];

/// Differences of the factory tunes from the common default.
static CATALOG_OVERRIDES: &[(&str, Change)] = &[
    ("joy", Level(E::RoadBump, 10.0)),
    ("joy", Volume(Speaker::Sub, 10.0)),
    ("joy/Default", Level(E::RoadBump, 10.0)),
    ("joy/Default", Volume(Speaker::Sub, 10.0)),
    ("ed/srv", Level(E::Engine, 5.0)),
    ("ed/srv", Level(E::Deceleration, 5.0)),
    ("ed/srv", Level(E::LateralAcceleration, 5.0)),
    ("ed/srv", Level(E::Impact, 7.0)),
    ("ed/srv", Level(E::RoadBump, 5.0)),
    ("ed/srv", Level(E::Gear, 5.0)),
    ("ed/srv", Level(E::Suspension, 3.0)),
    ("ed/srv", Level(E::Acceleration, 5.0)),
    ("ed/srv", Level(E::TractionLoss, 5.0)),
    ("ed/spaceship", Level(E::Engine, 6.0)),
    ("ed/spaceship", Level(E::Deceleration, 6.0)),
    ("ed/spaceship", Level(E::LateralAcceleration, 6.0)),
    ("ed/spaceship", Level(E::Impact, 6.0)),
    ("ed/spaceship", Level(E::RoadBump, 0.0)),
    ("ed/spaceship", Level(E::Gear, 6.0)),
    ("ed/spaceship", Level(E::Suspension, 6.0)),
    ("ed/spaceship", Level(E::Acceleration, 6.0)),
    ("ed/spaceship", Level(E::TractionLoss, 6.0)),
    ("rds", Level(E::RoadBump, 4.0)),
    ("rds", Level(E::Suspension, 3.0)),
    ("furidashi", Level(E::RoadBump, 4.0)),
    ("furidashi", Level(E::Suspension, 3.0)),
    ("falconbms", Fade(85.0, 95.0)),
    ("falconbms", Resonance(E::Engine, 10.0)),
    ("redrover", Fade(85.0, 95.0)),
    ("redrover", Resonance(E::Engine, 10.0)),
    ("f12012", CUTOFF_120[0]),
    ("f12012", CUTOFF_120[1]),
    ("f12012", CUTOFF_120[2]),
    ("f12012", CUTOFF_120[3]),
    ("f12012", CUTOFF_120[4]),
    ("f12012", CUTOFF_120[5]),
    ("f12012", CUTOFF_120[6]),
    ("f12012", CUTOFF_120[7]),
    ("f12012", CUTOFF_120[8]),
    ("f12012", CUTOFF_120[9]),
    ("f12012", Resonance(E::Engine, 10.0)),
    ("f12011", CUTOFF_120[0]),
    ("f12011", CUTOFF_120[1]),
    ("f12011", CUTOFF_120[2]),
    ("f12011", CUTOFF_120[3]),
    ("f12011", CUTOFF_120[4]),
    ("f12011", CUTOFF_120[5]),
    ("f12011", CUTOFF_120[6]),
    ("f12011", CUTOFF_120[7]),
    ("f12011", CUTOFF_120[8]),
    ("f12011", CUTOFF_120[9]),
    ("f12011", Resonance(E::Engine, 10.0)),
    ("acudp", Level(E::Engine, 7.0)),
    ("acudp", Level(E::Gear, 15.0)),
    ("acudp", Level(E::Suspension, 16.0)),
    ("acudp", Enabled(E::LateralAcceleration, false)),
    ("acudp", AllVolumes(16.0)),
    ("acudp", Volume(Speaker::Sub, 12.0)),
    ("acudp", LowPass(E::Engine)),
    ("acudp", CUTOFF_120[0]),
    ("acudp", CUTOFF_120[1]),
    ("acudp", CUTOFF_120[2]),
    ("acudp", CUTOFF_120[3]),
    ("acudp", CUTOFF_120[4]),
    ("acudp", CUTOFF_120[5]),
    ("acudp", CUTOFF_120[6]),
    ("acudp", CUTOFF_120[7]),
    ("acudp", CUTOFF_120[8]),
    ("acudp", CUTOFF_120[9]),
    ("acudp", FadeStart(99.0)),
    ("acudp", Resonance(E::Engine, 50.0)),
    ("ironrebellion", Level(E::Engine, 8.0)),
    ("ironrebellion", Level(E::Deceleration, 3.0)),
    ("ironrebellion", Level(E::LateralAcceleration, 0.0)),
    ("ironrebellion", Level(E::RoadBump, 5.0)),
    ("ironrebellion", Level(E::Acceleration, 3.0)),
    ("warplanes_ww1", Level(E::Deceleration, 4.0)),
    ("warplanes_ww1", Level(E::LateralAcceleration, 0.0)),
    ("warplanes_ww1", Level(E::Acceleration, 4.0)),
    ("warplanes_bop", Level(E::Deceleration, 4.0)),
    ("warplanes_bop", Level(E::LateralAcceleration, 0.0)),
    ("warplanes_bop", Level(E::Acceleration, 4.0)),
    ("api/Flashout 3", Engine(EngineType::Original)),
    ("acevo", Level(E::Engine, 7.0)),
    ("acevo", Level(E::Deceleration, 3.0)),
    ("acevo", Level(E::LateralAcceleration, 0.0)),
    ("acevo", Level(E::WheelSlip, 3.0)),
    ("acevo", Level(E::Suspension, 6.0)),
    ("acevo", Level(E::Acceleration, 3.0)),
    ("acevo", Level(E::TractionLoss, 3.0)),
    ("acevo", AllVolumes(5.0)),
];

/// The `[SHAKEBOX]` values of `config.ini` the engine reads directly. The other two keys have
/// one home each elsewhere: `soundcard` (whether shakers can play at all) is
/// [`super::system::FrameState::output_ready`], and `profile` is the selected
/// [`super::profiles::SpeakerProfile`] (its `name`) in [`super::system::ShakerSettings::profile`].
#[derive(Debug, Clone, PartialEq)]
pub struct ShakeboxConfig {
    /// `refresh_rate`, frames per second of the shaker thread.
    pub refresh_rate_hz: u32,
    pub side_enhancement: i64,
    pub reduction_factor: i64,
    pub min_side_enhancement: i64,
    pub min_suspension_velocity: i64,
    pub max_suspension_velocity: i64,
    pub min_bump_velocity: i64,
    pub max_bump_velocity: i64,
}

impl Default for ShakeboxConfig {
    /// `template.ini` values after the fresh-install migrations (v106 sets `refresh_rate` to 27).
    fn default() -> Self {
        ShakeboxConfig {
            refresh_rate_hz: 27,
            side_enhancement: 1,
            reduction_factor: 1,
            min_side_enhancement: 2,
            min_suspension_velocity: 90,
            max_suspension_velocity: 100_000,
            min_bump_velocity: 70,
            max_bump_velocity: 100_000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn master_config_common_defaults() {
        let t = ShakerTuning::default();
        let slider = |e| t.effect(e).level.map(|l| l.slider);
        assert_eq!(slider(Effect::RoadBump), Some(6.0));
        assert_eq!(slider(Effect::Gear), Some(10.0));
        assert_eq!(t.effect(Effect::Engine).resonance, 35.0);
        assert_eq!(t.effect(Effect::Impact).resonance, 10.0);
        assert_eq!(t.volume, [10.0, 10.0, 10.0, 6.0, 10.0, 10.0]);
        assert_eq!((t.engine_fade_start, t.engine_fade_end), (95.0, 100.0));
        assert_eq!(t.engine_type, Some(EngineType::Enhanced));
    }

    #[test]
    fn catalog_entries() {
        let a = ShakerTuning::catalog_default("acevo", None);
        assert_eq!(a.volume, [5.0; 6]);
        assert_eq!(a.effect(Effect::LateralAcceleration).level.map(|l| l.slider), Some(0.0));
        let u = ShakerTuning::catalog_default("acudp", None);
        assert_eq!(u.volume, [16.0, 16.0, 16.0, 12.0, 16.0, 16.0]);
        assert!(!u.effect(Effect::LateralAcceleration).level.unwrap().enabled);
        assert!(u.effect(Effect::Engine).low_pass);
        assert_eq!(u.engine_fade_start, 99.0);
        let f = ShakerTuning::catalog_default("api", Some("Flashout 3"));
        assert_eq!(f.engine_type, Some(EngineType::Original));
        assert_eq!(ShakerTuning::catalog_default("ac", None), ShakerTuning::default());
    }
}
