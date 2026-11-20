//! The engine vibration. When the user supplies FMOD and their SRS banks, OpenRigCue plays the
//! FMOD Studio events `event:/Vehicles/Car Engine 1` ('O') and `Car Engine 2` ('E') of
//! `Vehicles.bank` ([`super::fmod::FmodEngine`]); otherwise it plays its
//! own oscillator [`EngineSound`], fed the same parameters: `rpm` (0..10000), `load`, `pitch` and
//! one level per speaker. [`EngineVoice`] is whichever of the two the mixer plays.

use std::f64::consts::TAU;
use std::path::Path;

use openrigcue_model::native_value::LegacyValue;
use openrigcue_model::python_compare::native_equal;
use thiserror::Error;

use super::layout::PerSpeaker;
use super::rig::EngineCommand;
use super::tuning::{DspError, EngineType};

/// An error of a rig frame; the rest of the frame does not run.
#[derive(Debug, Error)]
pub enum EngineError {
    /// Reached Sound release/loop/play operation failed.
    #[error("{}", error.message)]
    SoundOperation { error: DspError },
    /// A required profile lookup or a scalar conversion failed.
    #[error("{}", error.message)]
    DspParameter { group: super::layout::Group, parameter: &'static str, error: super::tuning::DspError },
    /// A DSP float parameter was NaN or infinite; the message is "INVALID FLOAT".
    #[error("INVALID FLOAT")]
    InvalidDspFloat { group: super::layout::Group, parameter: &'static str },
    /// Late engine selector failure: earlier effects remain applied.
    #[error("{}", error.message)]
    EngineSelector { error: DspError },
    /// The selector's key error whose text cannot be built: the text form of the raw key (an
    /// integer beyond 4300 digits, also inside a tuple) fails with a value error. The shaker loop
    /// builds its report from the error text, so that value error ends the loop.
    #[error("{}: {}", str_error.class, str_error.message)]
    UnprintableEngineSelector { str_error: DspError },
    /// A late required pitch lookup/conversion; preceding parameters stay applied.
    #[error("{}", error.message)]
    EngineParameter { error: DspError },
    /// The traction-loss maximum failed after earlier effects and before wheel effects.
    #[error("{}", error.message)]
    TractionParameter { error: DspError },
    /// A reached suspension/road-bump threshold failed after preceding effects.
    #[error("{}", error.message)]
    SuspensionParameter { error: DspError },
    /// Printing the speaker error failed, so pitch does not run.
    #[error("{0}")]
    DiagnosticOutput(#[from] std::io::Error),
    /// The engine type is neither 'O' nor 'E', so there is no instance for it.
    #[error("KeyError: shaker_engine_type is neither 'O' nor 'E' (no car_engine_instance)")]
    NoInstance,
    /// A failing FMOD Studio call.
    #[error(transparent)]
    Fmod(#[from] super::fmod::FmodError),
}

impl EngineError {
    /// The error of building the text of a frame error when it is reported (see
    /// [`Self::UnprintableEngineSelector`]).
    pub fn str_failure(&self) -> Option<&DspError> {
        match self {
            Self::UnprintableEngineSelector { str_error } => Some(str_error),
            _ => None,
        }
    }

    /// Error metadata available from typed operations; other cases stay text-only.
    pub fn native_error(&self) -> Option<DspError> {
        match self {
            Self::SoundOperation { error }
            | Self::DspParameter { error, .. }
            | Self::EngineSelector { error }
            | Self::EngineParameter { error }
            | Self::TractionParameter { error }
            | Self::SuspensionParameter { error } => Some(error.clone()),
            Self::InvalidDspFloat { .. } => {
                Some(DspError { class: "pyfmodex.exceptions.FmodError", message: "INVALID FLOAT".into() })
            }
            // The error is still the key error; its text cannot be rendered.
            Self::UnprintableEngineSelector { .. } => Some(DspError { class: "KeyError", message: String::new() }),
            Self::Fmod(error) => error.native_error(),
            Self::NoInstance | Self::DiagnosticOutput(_) => None,
        }
    }
}

pub(super) type EngineOutput<'a> = dyn FnMut(&str) -> std::io::Result<()> + 'a;

#[cfg(test)]
std::thread_local! {
    static TEST_OUTPUT: std::cell::RefCell<Option<(Vec<String>, bool)>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(super) fn capture_engine_output<T>(fail: bool, run: impl FnOnce() -> T) -> (T, Vec<String>) {
    TEST_OUTPUT.with(|output| *output.borrow_mut() = Some((Vec::new(), fail)));
    let result = run();
    let lines = TEST_OUTPUT.with(|output| output.borrow_mut().take().unwrap().0);
    (result, lines)
}

pub(super) fn engine_stdout(text: &str) -> std::io::Result<()> {
    #[cfg(test)]
    if let Some(result) = TEST_OUTPUT.with(|output| {
        let mut output = output.borrow_mut();
        output.as_mut().map(|(lines, fail)| {
            lines.push(text.to_owned());
            if *fail {
                Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
            } else {
                Ok(())
            }
        })
    }) {
        return result;
    }
    use std::io::Write;
    std::io::stdout().lock().write_all(text.as_bytes())
}

pub(super) fn report_speaker_error(message: &str, output: &mut EngineOutput<'_>) -> Result<(), EngineError> {
    output(&format!("error mixed level outputs not loaded yet {message}\n"))?;
    Ok(())
}

pub(super) fn engine_levels(command: &EngineCommand) -> Result<Option<PerSpeaker>, &DspError> {
    command
        .native_parameters
        .as_ref()
        .map_or(Ok(command.speaker_levels), |p| p.levels.as_ref().map(|levels| Some(*levels)))
}

pub(super) fn engine_pitch(command: &EngineCommand) -> Result<f64, EngineError> {
    command
        .native_parameters
        .as_ref()
        .map_or(Ok(command.pitch), |p| p.pitch.clone().map_err(|error| EngineError::EngineParameter { error }))
}

pub(super) enum EngineSelector<'a> {
    Compatibility(Option<EngineType>),
    Native(&'a LegacyValue),
}

/// Required profile lookup precedes either instance pause.
pub(super) fn engine_selector(command: &EngineCommand) -> Result<EngineSelector<'_>, EngineError> {
    match &command.native_engine_selector {
        Some(Ok(key)) => Ok(EngineSelector::Native(key)),
        Some(Err(error)) => Err(EngineError::EngineSelector { error: error.clone() }),
        None => Ok(EngineSelector::Compatibility(command.engine_type)),
    }
}

impl EngineSelector<'_> {
    pub(super) fn other_instance(&self) -> EngineType {
        let original = match self {
            Self::Compatibility(value) => *value == Some(EngineType::Original),
            Self::Native(key) => native_equal(key, &LegacyValue::Str("O".into())),
        };
        if original {
            EngineType::Enhanced
        } else {
            EngineType::Original
        }
    }

    /// Called after the other instance is paused, so a pause error wins over a bad key.
    pub(super) fn instance(&self) -> Result<EngineType, EngineError> {
        let key = match self {
            Self::Compatibility(value) => return value.ok_or(EngineError::NoInstance),
            Self::Native(key) => *key,
        };
        // The instance lookup hashes the key first (object handles, bytearray, set, dict,
        // list and any tuple holding one of them are a type error), then compares it with 'O'/'E'.
        if let Err(message) = openrigcue_model::native_value::Key::from_value(key) {
            return Err(EngineError::EngineSelector { error: DspError { class: "TypeError", message } });
        }
        for (name, instance) in [("O", EngineType::Original), ("E", EngineType::Enhanced)] {
            if native_equal(key, &LegacyValue::Str(name.into())) {
                return Ok(instance);
            }
        }
        // A key error: its text is the key's quoted form, checked (4300-digit limit, nested containers).
        match openrigcue_model::python_repr::repr_checked(key) {
            Ok(message) => Err(EngineError::EngineSelector { error: DspError { class: "KeyError", message } }),
            Err(error) => Err(EngineError::UnprintableEngineSelector { str_error: DspError::from_native(error) }),
        }
    }
}

/// Frequency at `rpm` parameter 0, Hz.
const BASE_HZ: f64 = 20.0;
/// Frequency rise per unit of `pitch` at the full `rpm` parameter (10000), Hz.
const HZ_PER_PITCH: f64 = 2.0;
/// Parameter smoothing time constant, seconds.
const SMOOTHING: f64 = 0.02;

/// `rpm` parameters at which the level of the FMOD engine events was measured
/// (docs/recovery/audio-real-runtime-pulse-checks-2026-11-20.json, `fmod.established`).
const LEVEL_RPM: [f64; 3] = [1000.0, 3000.0, 6000.0];
/// RMS of one speaker at level 1, `load` 1 and pitch 35 at each of [`LEVEL_RPM`], 'O' events.
const ORIGINAL_RMS: [f64; 3] = [0.189, 0.259, 0.408];
/// The same for the 'E' events.
const ENHANCED_RMS: [f64; 3] = [0.309, 0.335, 0.523];
/// Level at `load` 0 relative to `load` 1 (RMS 0.317 against 0.407), 'O' events.
const ORIGINAL_IDLE_LOAD: f64 = 0.317 / 0.407;
/// The same for the 'E' events (RMS 0.192 against 0.259).
const ENHANCED_IDLE_LOAD: f64 = 0.192 / 0.259;

/// Frequency of the engine vibration for the event parameters.
pub fn engine_hz(rpm_param: f64, pitch: f64) -> f64 {
    BASE_HZ + rpm_param.clamp(0.0, 10000.0) / 10000.0 * pitch * HZ_PER_PITCH
}

/// RMS of the engine vibration on a speaker at level 1 for the event parameters: silent
/// unless `rpm_param > 0`, straight lines between the measured points of [`LEVEL_RPM`] and
/// their end values outside them (nothing was measured there), scaled by `load` (0..1) on a
/// straight line from the measured `load` 0 ratio to 1.
pub fn engine_level(engine_type: EngineType, rpm_param: f64, load: f64) -> f64 {
    if rpm_param.is_nan() || rpm_param <= 0.0 {
        return 0.0;
    }
    let (rms, idle_load) = match engine_type {
        EngineType::Original => (ORIGINAL_RMS, ORIGINAL_IDLE_LOAD),
        EngineType::Enhanced => (ENHANCED_RMS, ENHANCED_IDLE_LOAD),
    };
    let rpm = rpm_param.clamp(LEVEL_RPM[0], LEVEL_RPM[2]);
    let segment = usize::from(rpm > LEVEL_RPM[1]);
    let along = (rpm - LEVEL_RPM[segment]) / (LEVEL_RPM[segment + 1] - LEVEL_RPM[segment]);
    let at_full_load = rms[segment] + (rms[segment + 1] - rms[segment]) * along;
    at_full_load * (idle_load + (1.0 - idle_load) * load.clamp(0.0, 1.0))
}

/// Waveform of one engine type at oscillator phase `phase` (radians).
fn waveform(engine_type: EngineType, phase: f64) -> f64 {
    match engine_type {
        EngineType::Original => phase.sin() + 0.3 * (2.0 * phase).sin(),
        EngineType::Enhanced => phase.sin() + 0.5 * (2.0 * phase).sin() + 0.25 * (0.5 * phase).sin(),
    }
}

/// RMS of [`waveform`] over its period: the root of half the sum of the squared partials.
fn waveform_rms(engine_type: EngineType) -> f64 {
    match engine_type {
        EngineType::Original => ((1.0 + 0.3 * 0.3) / 2.0_f64).sqrt(),
        EngineType::Enhanced => ((1.0 + 0.5 * 0.5 + 0.25 * 0.25) / 2.0_f64).sqrt(),
    }
}

/// The engine voice: last parameters, smoothed state and oscillator phase.
#[derive(Debug, Clone)]
pub struct EngineSound {
    rate: f64,
    playing: Option<EngineType>,
    target_hz: f64,
    target_gain: f64,
    target_levels: PerSpeaker,
    rpm_param: f64,
    load_param: f64,
    pitch_param: f64,
    hz: f64,
    gain: f64,
    levels: PerSpeaker,
    phase: f64,
}

impl EngineSound {
    pub fn new(sample_rate: u32) -> EngineSound {
        EngineSound {
            rate: f64::from(sample_rate),
            playing: None,
            target_hz: BASE_HZ,
            target_gain: 0.0,
            target_levels: [0.0; 6],
            rpm_param: 0.0,
            load_param: 1.0,
            pitch_param: 35.0,
            hz: BASE_HZ,
            gain: 0.0,
            levels: [0.0; 6],
            phase: 0.0,
        }
    }

    /// Render at `sample_rate` from now on, keeping the parameters and phase.
    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        self.rate = f64::from(sample_rate);
    }

    /// Apply one frame's engine parameters (un-pauses the selected instance; `c.engine_type`
    /// names one, see [`EngineVoice::command`]).
    pub fn command(&mut self, c: &EngineCommand) {
        self.playing = c.engine_type;
        self.set_rpm(c.rpm);
        self.set_load(c.load);
        if let Some(levels) = c.speaker_levels {
            self.target_levels = levels;
        }
        self.set_pitch(c.pitch);
    }

    fn set_rpm(&mut self, rpm: f64) {
        self.rpm_param = rpm;
        self.target_hz = engine_hz(rpm, self.pitch_param);
        self.update_gain();
    }

    /// A NaN `load` keeps the previous one.
    fn set_load(&mut self, load: f64) {
        if !load.is_nan() {
            self.load_param = load;
        }
        self.update_gain();
    }

    /// The waveform's amplitude that gives the playing instance its [`engine_level`].
    fn update_gain(&mut self) {
        self.target_gain = self.playing.map_or(0.0, |engine_type| {
            engine_level(engine_type, self.rpm_param, self.load_param) / waveform_rms(engine_type)
        });
    }

    fn set_pitch(&mut self, pitch: f64) {
        self.pitch_param = pitch;
        self.target_hz = engine_hz(self.rpm_param, pitch);
    }

    /// Pause both instances.
    pub fn pause(&mut self) {
        self.playing = None;
    }

    /// Pause this instance (the other one keeps its state).
    fn pause_instance(&mut self, instance: EngineType) {
        if self.playing == Some(instance) {
            self.playing = None;
        }
    }

    pub fn is_playing(&self) -> bool {
        self.playing.is_some()
    }

    /// Add the next frame of engine vibration to `frame` (one value per speaker).
    pub fn render_into(&mut self, frame: &mut PerSpeaker) {
        let Some(engine_type) = self.playing else {
            return;
        };
        let k = 1.0 - (-1.0 / (SMOOTHING * self.rate)).exp();
        self.hz += (self.target_hz - self.hz) * k;
        self.gain += (self.target_gain - self.gain) * k;
        for (l, t) in self.levels.iter_mut().zip(self.target_levels) {
            *l += (t - *l) * k;
        }
        // The sub-harmonic of the enhanced waveform needs a phase range of two cycles.
        self.phase = (self.phase + TAU * self.hz / self.rate) % (2.0 * TAU);
        let v = self.gain * waveform(engine_type, self.phase);
        for (out, level) in frame.iter_mut().zip(self.levels) {
            *out += v * level;
        }
    }
}

/// The engine sound the mixer plays.
#[derive(Debug)]
pub enum EngineVoice {
    /// OpenRigCue's oscillator (the default).
    Synth(EngineSound),
    /// The FMOD Studio events of the user's SRS banks.
    Fmod(Box<super::fmod::FmodEngine>),
}

impl EngineVoice {
    /// The built-in engine at `sample_rate`.
    pub fn synth(sample_rate: u32) -> EngineVoice {
        EngineVoice::Synth(EngineSound::new(sample_rate))
    }

    /// The FMOD engine events, read from the user's SRS installation or installer `srs` and played
    /// by the FMOD libraries of [`super::fmod::library_dirs`]; the built-in engine when either
    /// is missing (logged).
    pub fn from_srs(srs: &Path, sample_rate: u32) -> EngineVoice {
        match super::fmod::FmodEngine::from_srs(srs, sample_rate) {
            Ok(engine) => {
                tracing::info!("engine vibration: FMOD Studio events of the SRS banks");
                EngineVoice::Fmod(Box::new(engine))
            }
            Err(error) => {
                tracing::info!(%error, "FMOD engine not available, using the built-in engine sound");
                EngineVoice::synth(sample_rate)
            }
        }
    }

    pub(super) fn release_for_reopen(&mut self, rate: u32) -> Option<super::fmod::FmodSource> {
        let source = match self {
            Self::Fmod(engine) => Some(engine.source()),
            Self::Synth(_) => None,
        };
        *self = Self::synth(rate);
        source
    }

    pub fn is_fmod(&self) -> bool {
        !matches!(self, EngineVoice::Synth(_))
    }

    /// Render at `sample_rate` from now on. An FMOD engine that cannot be rebuilt at the new
    /// rate gives way to the built-in one.
    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        match self {
            EngineVoice::Synth(e) => e.set_sample_rate(sample_rate),
            EngineVoice::Fmod(e) => {
                if let Err(error) = e.set_sample_rate(sample_rate) {
                    tracing::warn!(%error, "FMOD engine lost, using the built-in engine sound");
                    *self = EngineVoice::synth(sample_rate);
                }
            }
        }
    }

    /// The engine block of a rig frame. A `shaker_engine_type` with no instance pauses 'O'
    /// and fails with [`EngineError::NoInstance`]. Parameter or
    /// output errors end the frame; the speaker block handles its own errors before pitch.
    pub fn command(&mut self, c: &EngineCommand) -> Result<(), EngineError> {
        self.command_with_output(c, &mut engine_stdout)
    }

    pub(super) fn command_with_output(
        &mut self,
        c: &EngineCommand,
        output: &mut EngineOutput<'_>,
    ) -> Result<(), EngineError> {
        let selected = self.begin_live(c)?;
        match engine_levels(c) {
            Ok(Some(levels)) => {
                for (slot, value) in levels.into_iter().enumerate() {
                    if !self.speaker_live_with_output(selected, slot, Ok(Some(value)), output)? {
                        break;
                    }
                }
            }
            Ok(None) => {}
            Err(error) => {
                self.speaker_live_with_output(selected, 0, Err(error.clone()), output)?;
            }
        }
        self.pitch_live(selected, engine_pitch(c)?)
    }

    pub(super) fn begin_live(&mut self, c: &EngineCommand) -> Result<EngineType, EngineError> {
        let selector = engine_selector(c)?;
        self.pause_instance_live(selector.other_instance())?;
        let selected = selector.instance()?;
        if self.paused_live(selected)? {
            self.resume_live(selected)?;
        }
        self.rpm_live(selected, c.rpm)?;
        self.load_live(selected, c.load)?;
        Ok(selected)
    }

    pub(super) fn pause_instance_live(&mut self, selected: EngineType) -> Result<(), EngineError> {
        match self {
            Self::Synth(engine) => {
                engine.pause_instance(selected);
                Ok(())
            }
            Self::Fmod(engine) => engine.pause_instance_live(selected).map_err(Into::into),
        }
    }
    pub(super) fn paused_live(&mut self, selected: EngineType) -> Result<bool, EngineError> {
        match self {
            Self::Synth(engine) => Ok(engine.playing != Some(selected)),
            Self::Fmod(engine) => engine.paused_live(selected).map_err(Into::into),
        }
    }
    pub(super) fn resume_live(&mut self, selected: EngineType) -> Result<(), EngineError> {
        match self {
            Self::Synth(engine) => {
                engine.playing = Some(selected);
                Ok(())
            }
            Self::Fmod(engine) => engine.resume_live(selected).map_err(Into::into),
        }
    }
    pub(super) fn rpm_live(&mut self, selected: EngineType, value: f64) -> Result<(), EngineError> {
        match self {
            Self::Synth(engine) => {
                engine.set_rpm(value);
                Ok(())
            }
            Self::Fmod(engine) => engine.rpm_live(selected, value).map_err(Into::into),
        }
    }
    pub(super) fn load_live(&mut self, selected: EngineType, value: f64) -> Result<(), EngineError> {
        match self {
            Self::Synth(engine) => {
                engine.set_load(value);
                Ok(())
            }
            Self::Fmod(engine) => engine.load_live(selected, value).map_err(Into::into),
        }
    }

    pub(super) fn speaker_live_with_output(
        &mut self,
        selected: EngineType,
        slot: usize,
        value: Result<Option<f64>, DspError>,
        output: &mut EngineOutput<'_>,
    ) -> Result<bool, EngineError> {
        let result = match value {
            Ok(Some(value)) => match self {
                EngineVoice::Synth(engine) => {
                    engine.target_levels[slot] = value;
                    Ok(())
                }
                EngineVoice::Fmod(engine) => engine.speaker_live(selected, slot, value),
            },
            Ok(None) => return Ok(true),
            Err(error) => {
                report_speaker_error(&error.message, output)?;
                return Ok(false);
            }
        };
        if let Err(error) = result {
            report_speaker_error(&super::fmod::caught_fmod_message(&error), output)?;
            return Ok(false);
        }
        Ok(true)
    }

    pub(super) fn speaker_live(
        &mut self,
        selected: EngineType,
        slot: usize,
        value: Result<Option<f64>, DspError>,
    ) -> Result<bool, EngineError> {
        self.speaker_live_with_output(selected, slot, value, &mut engine_stdout)
    }

    pub(super) fn pitch_live(&mut self, selected: EngineType, pitch: f64) -> Result<(), EngineError> {
        match self {
            EngineVoice::Synth(engine) => {
                engine.set_pitch(pitch);
                Ok(())
            }
            EngineVoice::Fmod(engine) => engine.pitch_live(selected, pitch).map_err(Into::into),
        }
    }

    /// The inactive branch's pause of both instances; a failing FMOD call is an error.
    pub fn pause(&mut self) -> Result<(), EngineError> {
        match self {
            EngineVoice::Synth(e) => {
                e.pause();
                Ok(())
            }
            EngineVoice::Fmod(e) => e.pause(),
        }
    }

    /// The Studio update of a rig frame: the frame's FMOD commands
    /// are submitted together; a failing update is an error. The built-in engine has no command
    /// buffer, so its frame is already in effect.
    pub fn submit(&mut self) -> Result<(), EngineError> {
        match self {
            EngineVoice::Synth(_) => Ok(()),
            EngineVoice::Fmod(e) => e.submit().map_err(Into::into),
        }
    }

    pub fn is_playing(&self) -> bool {
        match self {
            EngineVoice::Synth(e) => e.is_playing(),
            EngineVoice::Fmod(e) => e.is_playing(),
        }
    }

    pub fn render_into(&mut self, frame: &mut PerSpeaker) {
        match self {
            EngineVoice::Synth(e) => e.render_into(frame),
            EngineVoice::Fmod(e) => e.render_into(frame),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(rpm: f64, levels: Option<PerSpeaker>) -> EngineCommand {
        EngineCommand {
            engine_type: Some(EngineType::Enhanced),
            native_engine_selector: None,
            native_parameters: None,
            rpm,
            load: 1.0,
            speaker_levels: levels,
            pitch: 35.0,
        }
    }

    #[test]
    fn frequency_follows_rpm_and_pitch() {
        assert_eq!(engine_hz(0.0, 35.0), 20.0);
        assert_eq!(engine_hz(10000.0, 35.0), 90.0);
        assert_eq!(engine_hz(5000.0, 100.0), 120.0);
        assert_eq!(engine_hz(20000.0, 35.0), 90.0);
    }

    /// RMS of the sub speaker over two seconds, after the parameter smoothing has settled.
    fn steady_sub_rms(engine_type: EngineType, rpm: f64, load: f64) -> f64 {
        let mut e = EngineSound::new(48_000);
        let command = EngineCommand { engine_type: Some(engine_type), load, ..cmd(rpm, Some([1.0; 6])) };
        e.command(&command);
        for _ in 0..48_000 {
            e.render_into(&mut [0.0; 6]);
        }
        let mut sum = 0.0;
        for _ in 0..96_000 {
            let mut f = [0.0; 6];
            e.render_into(&mut f);
            sum += f[3] * f[3];
        }
        (sum / 96_000.0_f64).sqrt()
    }

    fn close(got: f64, want: f64) -> bool {
        (got - want).abs() <= 0.01 * want
    }

    #[test]
    fn level_follows_rpm_through_the_measured_points() {
        for (engine_type, measured) in
            [(EngineType::Original, [0.189, 0.259, 0.408]), (EngineType::Enhanced, [0.309, 0.335, 0.523])]
        {
            for (rpm, want) in [1000.0, 3000.0, 6000.0].into_iter().zip(measured) {
                assert_eq!(engine_level(engine_type, rpm, 1.0), want);
                let got = steady_sub_rms(engine_type, rpm, 1.0);
                assert!(close(got, want), "{engine_type:?} rpm {rpm}: rendered {got}, measured {want}");
            }
            // Straight lines between the measured points, rising all the way.
            let halfway = engine_level(engine_type, 2000.0, 1.0);
            assert!((halfway - (measured[0] + measured[1]) / 2.0).abs() < 1e-12);
            let halfway = engine_level(engine_type, 4500.0, 1.0);
            assert!((halfway - (measured[1] + measured[2]) / 2.0).abs() < 1e-12);
            let mut previous = engine_level(engine_type, 1000.0, 1.0);
            for rpm in (1100..=6000).step_by(100) {
                let now = engine_level(engine_type, f64::from(rpm), 1.0);
                assert!(now > previous, "{engine_type:?} rpm {rpm}");
                previous = now;
            }
            // Nothing was measured outside 1000..6000: the end values hold; silent at rpm 0.
            assert_eq!(engine_level(engine_type, 1.0, 1.0), measured[0]);
            assert_eq!(engine_level(engine_type, 10000.0, 1.0), measured[2]);
            assert_eq!(engine_level(engine_type, 20000.0, 1.0), measured[2]);
            assert_eq!(engine_level(engine_type, 0.0, 1.0), 0.0);
            assert_eq!(engine_level(engine_type, -5.0, 1.0), 0.0);
            assert_eq!(engine_level(engine_type, f64::NAN, 1.0), 0.0);
            assert_eq!(steady_sub_rms(engine_type, 0.0, 1.0), 0.0);
        }
    }

    #[test]
    fn level_follows_load_by_the_measured_ratio() {
        for (engine_type, idle, full) in [(EngineType::Original, 0.317, 0.407), (EngineType::Enhanced, 0.192, 0.259)] {
            let ratio = idle / full;
            for rpm in [1000.0, 3000.0, 6000.0, 9000.0] {
                let at_full = engine_level(engine_type, rpm, 1.0);
                assert!((engine_level(engine_type, rpm, 0.0) - at_full * ratio).abs() < 1e-12);
                let half = engine_level(engine_type, rpm, 0.5);
                assert!((half - at_full * (1.0 + ratio) / 2.0).abs() < 1e-12);
                // `load` is a 0..1 parameter.
                assert_eq!(engine_level(engine_type, rpm, -1.0), engine_level(engine_type, rpm, 0.0));
                assert_eq!(engine_level(engine_type, rpm, 3.0), at_full);
            }
            let rendered_full = steady_sub_rms(engine_type, 3000.0, 1.0);
            let rendered_idle = steady_sub_rms(engine_type, 3000.0, 0.0);
            assert!(close(rendered_idle / rendered_full, ratio), "{engine_type:?}: {rendered_idle} / {rendered_full}");
            assert!(rendered_idle < steady_sub_rms(engine_type, 3000.0, 0.5));
            assert!(steady_sub_rms(engine_type, 3000.0, 0.5) < rendered_full);
        }
    }

    /// The live path of a rig frame sets `load` on the built-in engine; a NaN keeps the last one.
    #[test]
    fn built_in_engine_takes_load_from_the_frame() {
        let mut voice = EngineVoice::synth(48_000);
        let idle = EngineCommand { engine_type: Some(EngineType::Original), load: 0.0, ..cmd(3000.0, Some([1.0; 6])) };
        voice.command(&idle).unwrap();
        let EngineVoice::Synth(ref sound) = voice else { unreachable!() };
        assert_eq!(sound.load_param, 0.0);
        let want = engine_level(EngineType::Original, 3000.0, 0.0) / waveform_rms(EngineType::Original);
        assert_eq!(sound.target_gain, want);
        voice.load_live(EngineType::Original, f64::NAN).unwrap();
        let EngineVoice::Synth(ref sound) = voice else { unreachable!() };
        assert_eq!(sound.load_param, 0.0);
        assert_eq!(sound.target_gain, want);
        voice.load_live(EngineType::Original, 1.0).unwrap();
        let EngineVoice::Synth(ref sound) = voice else { unreachable!() };
        assert_eq!(sound.target_gain, 0.259 / waveform_rms(EngineType::Original));
    }

    #[test]
    fn native_engine_synth_retains_each_accepted_parameter_before_output_or_pitch_errors() {
        use crate::shakers::rig::NativeEngineParameters;
        let mut voice = EngineVoice::synth(48_000);
        voice.command(&cmd(1000.0, Some([0.25; 6]))).unwrap();
        let mut command = cmd(6000.0, None);
        command.native_parameters = Some(Box::new(NativeEngineParameters {
            levels: Err(DspError { class: "KeyError", message: "'engine_fadeoff_start'".into() }),
            pitch: Ok(8.0),
        }));
        let mut lines = Vec::new();
        voice
            .command_with_output(&command, &mut |line| {
                lines.push(line.to_owned());
                Ok(())
            })
            .unwrap();
        assert_eq!(lines, ["error mixed level outputs not loaded yet 'engine_fadeoff_start'\n"]);
        let EngineVoice::Synth(ref sound) = voice else { unreachable!() };
        assert_eq!(sound.target_levels, [0.25; 6]);
        assert_eq!(sound.pitch_param, 8.0);
        command.rpm = 7000.0;
        command.native_parameters.as_mut().unwrap().pitch =
            Err(DspError { class: "ValueError", message: "pitch".into() });
        let error = voice
            .command_with_output(&command, &mut |_| Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe)))
            .unwrap_err();
        assert!(matches!(error, EngineError::DiagnosticOutput(_)));
        let EngineVoice::Synth(ref sound) = voice else { unreachable!() };
        assert_eq!(sound.rpm_param, 7000.0);
        assert_eq!(sound.pitch_param, 8.0);
        assert_eq!(sound.target_hz, engine_hz(7000.0, 8.0));
        command.native_parameters.as_mut().unwrap().levels = Ok([2.0; 6]);
        let error = voice.command_with_output(&command, &mut |_| Ok(())).unwrap_err();
        assert!(matches!(error, EngineError::EngineParameter { .. }));
        let EngineVoice::Synth(ref sound) = voice else { unreachable!() };
        assert_eq!(sound.target_levels, [2.0; 6]);
        assert_eq!(sound.pitch_param, 8.0);
    }

    #[test]
    fn synthetic_engine_overlap_retains_the_existing_single_oscillator_policy() {
        use super::super::tuning::EngineType;
        let mut voice = EngineVoice::synth(48000);
        let mut first = cmd(3000.0, Some([0.1; 6]));
        first.engine_type = Some(EngineType::Original);
        let captured = voice.begin_live(&first).unwrap();
        let mut second = cmd(4000.0, Some([0.2; 6]));
        second.engine_type = Some(EngineType::Enhanced);
        voice.command(&second).unwrap();
        assert!(voice.speaker_live(captured, 0, Ok(Some(0.75))).unwrap());
        voice.pitch_live(captured, 77.0).unwrap();
        let EngineVoice::Synth(engine) = voice else { unreachable!() };
        assert_eq!(engine.playing, Some(EngineType::Enhanced));
        assert_eq!(engine.rpm_param, 4000.0);
        assert_eq!(engine.pitch_param, 77.0);
        assert_eq!(engine.target_levels, [0.75, 0.2, 0.2, 0.2, 0.2, 0.2]);
    }

    #[test]
    fn silent_until_commanded_and_when_paused() {
        let mut e = EngineSound::new(48_000);
        let mut f = [0.0; 6];
        e.render_into(&mut f);
        assert_eq!(f, [0.0; 6]);
        e.command(&cmd(5000.0, Some([0.0, 0.0, 1.0, 0.0, 0.0, 0.0])));
        let mut peak: f64 = 0.0;
        for _ in 0..4800 {
            let mut f = [0.0; 6];
            e.render_into(&mut f);
            assert_eq!(f[0], 0.0);
            peak = peak.max(f[2].abs());
        }
        assert!(peak > 0.1);
        e.pause();
        let mut f = [0.0; 6];
        e.render_into(&mut f);
        assert_eq!(f, [0.0; 6]);
    }

    #[test]
    fn missing_levels_keep_previous() {
        let mut e = EngineSound::new(48_000);
        e.command(&cmd(5000.0, Some([1.0; 6])));
        e.command(&cmd(5000.0, None));
        assert_eq!(e.target_levels, [1.0; 6]);
    }

    #[test]
    fn built_in_engine_is_the_default_voice() {
        let mut v = EngineVoice::synth(48_000);
        assert!(!v.is_fmod());
        assert!(!v.is_playing());
        v.command(&cmd(5000.0, Some([1.0; 6]))).unwrap();
        assert!(v.is_playing());
        v.set_sample_rate(44_100);
        v.pause().unwrap();
        assert!(!v.is_playing());
    }

    /// An engine type other than 'O'/'E':
    /// 'O' is paused, then the lookup fails; the 'E' instance keeps playing with its last parameters.
    #[test]
    fn unknown_engine_type_pauses_original_then_raises() {
        let mut v = EngineVoice::synth(48_000);
        let original = EngineCommand { engine_type: Some(EngineType::Original), ..cmd(5000.0, Some([1.0; 6])) };
        v.command(&original).unwrap();
        let unknown = EngineCommand { engine_type: None, ..original };
        assert!(matches!(v.command(&unknown), Err(EngineError::NoInstance)));
        assert!(!v.is_playing(), "'O' paused");
        v.command(&cmd(5000.0, Some([1.0; 6]))).unwrap();
        assert!(matches!(v.command(&unknown), Err(EngineError::NoInstance)));
        assert!(v.is_playing(), "'E' untouched");
    }

    /// Without FMOD libraries or banks the engine falls back to
    /// the built-in sound.
    #[test]
    fn missing_banks_fall_back_to_built_in_engine() {
        let dir = std::env::temp_dir().join(format!("openrigcue-no-banks-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let v = EngineVoice::from_srs(&dir, 48_000);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(!v.is_fmod());
    }
}
