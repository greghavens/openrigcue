//! The shaker system as the daemon drives it: settings, the effect engine and the mixer.

use std::sync::{Arc, Mutex, MutexGuard};

use openrigcue_model::Telemetry;

use super::effect_test::TestFrame;
use super::engine_sound::{EngineError, EngineVoice};
use super::layout::{Effect, PerGroup, Speaker};
use super::levels::{required_speaker_test_levels, GroupLevels};
use super::mixer::Mixer;
use super::profiles::SpeakerProfile;
use super::rig::{FrameContext, FrameStage, LiveFrame, RigStatus, ShakeInput, ShakeRig, Voice};
use super::samples::{SampleBank, SampleFolder, SampleKey};
use super::tuning::{DspError, ShakeboxConfig, ShakerTuning};

/// Settings that change the group levels.
#[derive(Debug, Clone)]
pub struct ShakerSettings {
    pub config: ShakeboxConfig,
    /// The speaker profile. `None` when `SHAKEBOX.profile` names no stored layout:
    /// the lookup fails and the layout in use stays, so [`ShakerSystem::update_settings`]
    /// keeps the current profile and [`ShakerSystem::new`] starts from [`SpeakerProfile::empty`].
    pub profile: Option<SpeakerProfile>,
    pub tuning: ShakerTuning,
    /// The traction-loss maximum of the active game profile.
    pub max_traction_loss: f64,
}

/// Game/UI state consulted every frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameState {
    /// An output device is open (a sound card is selected and initialised).
    pub output_ready: bool,
    /// A game listener is running, as the shared
    /// running-listener value holds it when the frame runs; the effect test's listener
    /// counts (the test stores it there, see [`ShakerSystem::test_frame`]).
    pub game_running: bool,
    /// The sidebar shaker switch is on.
    pub switch_on: bool,
}

#[derive(Default)]
struct SoundOperation {
    voice: Option<super::rig::Voice>,
    stop: bool,
    release_failed: bool,
    catch_errors: bool,
    release: Option<super::mixer::SoundHandle>,
    start: Option<(super::samples::SampleKey, super::rig::Plays, super::layout::Group)>,
    failure: Option<super::rig::Action>,
}

fn sound_error_message(operation: &SoundOperation, stage: &str, error: &EngineError) -> Option<String> {
    if !operation.catch_errors {
        return None;
    }
    let voice = operation.voice.expect("prepared operation");
    let labels = match voice {
        Voice::Impact => ("impact mixer ", "impact sound ", "impact sound "),
        Voice::Lateral => ("lat mixer ", "lat sound ", "lat sound "),
        Voice::TractionLoss => ("traction loss mixer ", "traction loss sound ", "traction loss sound "),
        Voice::Brake => ("brake mixer ", "brake sound ", "brake sound "),
        Voice::Accel => ("accel mixer ", "accel sound ", "accel sound "),
        Voice::WheelSlipLf => ("wheel_slip lf mixer ", "wheel_slip lf sound ", "wheel_slip lf sound "),
        Voice::WheelSlipRf => ("wheel_slip_rf_mixer ", "wheel_slip_rf_mixer sound", "wheel_slip_rf_mixer sound"),
        Voice::WheelSlipLr => ("wheel_slip_lr_mixer ", "wheel_slip_lr_sound ", "wheel_slip_lr_sound "),
        Voice::WheelSlipRr => ("wheel_slip_rr_mixer ", "wheel_slip_rr_mixer sound", "wheel_slip_rr_mixer sound"),
        Voice::SuspensionLf => ("suspension lf mixer ", "suspension lf sound ", "suspension lf sound "),
        Voice::SuspensionRf => ("suspension_rf_mixer ", "suspension_rf_mixer sound", "suspension_rf_mixer sound"),
        Voice::SuspensionLr => ("suspension_lr_mixer ", "suspension_lr_sound ", "suspension_lr_sound "),
        Voice::SuspensionRr => ("suspension_rr_mixer ", "suspension_rr_mixer sound", "suspension_rr_mixer sound"),
        Voice::RoadBump => ("road bump mixer ", "road bump sound ", "road bump sound "),
        _ => return None,
    };
    let (verb, label) = match stage {
        "release" => ("releasing", labels.0),
        "create" => ("creating", labels.1),
        _ => (if voice == Voice::RoadBump { "while playing" } else { "playing" }, labels.2),
    };
    let message = error.native_error().map_or_else(|| error.to_string(), |error| error.message);
    Some(format!("Error {verb} {label}{message}"))
}

fn log_sound_error(operation: &SoundOperation, stage: &str, error: EngineError) -> Result<(), EngineError> {
    if let Some(message) = sound_error_message(operation, stage, &error) {
        tracing::error!("{message}");
        Ok(())
    } else {
        Err(error)
    }
}

/// One in-flight call keeps input arguments and pending previous-value commits local.
pub struct FrameProgress {
    input: ShakeInput,
    native_lateral_velocity: Option<openrigcue_model::native_value::LegacyValue>,
    frame: LiveFrame,
    engine: Option<super::tuning::EngineType>,
    speakers_failed: bool,
    engine_command: Option<super::rig::EngineCommand>,
    engine_was_paused: bool,
    sound: SoundOperation,
}
impl FrameProgress {
    pub fn stages(&self) -> Vec<FrameStage> {
        FrameStage::for_status(self.frame.status)
            .into_iter()
            .flat_map(|stage| {
                if let Some(voice) = stage.sound_voice() {
                    vec![
                        FrameStage::SoundPrepare(voice),
                        FrameStage::SoundReleaseLookup,
                        FrameStage::SoundRelease,
                        FrameStage::SoundClear,
                        FrameStage::SoundCreate,
                        FrameStage::SoundLoops,
                        stage,
                    ]
                } else {
                    vec![stage]
                }
            })
            .collect()
    }
}

/// Effect engine plus mixer. The mixer is shared with the audio callback.
///
/// Settings and channel groups change separately: the groups (and so
/// the levels of every sample started afterwards) are only rebuilt by [`ShakerSystem::reload_levels`]:
///
/// | event | call |
/// |---|---|
/// | effect level slider or on/off switch | [`ShakerSystem::update_settings`], then [`ShakerSystem::reload_levels`] |
/// | channel volume slider | `update_settings`, then `reload_levels` |
/// | custom speaker matrix confirmed | [`ShakerSystem::edit_custom_matrix`] |
/// | sound card selected or output (re)opened | `reload_levels` |
/// | low-pass switch, cutoff, resonance/speed, engine fade range, engine type | `update_settings` only: the next frame's DSP block and engine block read them |
/// | speaker profile selected | `update_settings` only: the new matrix applies from the next rebuild |
/// | game/vehicle profile loaded (every effect and channel slider is set) | `update_settings`, then `reload_levels` when one of the effect levels, switches or channel volumes differs from what the slider showed, or on the first load |
/// | `max_telemetry_vars` changed | `update_settings` only |
pub struct ShakerSystem {
    /// Always holds a profile (`settings.profile` is `Some`).
    settings: ShakerSettings,
    levels: GroupLevels,
    rig: ShakeRig,
    mixer: Arc<Mutex<Mixer>>,
}

impl ShakerSystem {
    /// Constructor with an immediately computed initial build.
    pub fn new(mut settings: ShakerSettings, bank: SampleBank, sample_rate: u32) -> ShakerSystem {
        let profile = settings.profile.get_or_insert_with(SpeakerProfile::empty);
        let mut levels = GroupLevels::empty();
        let mixer = if levels.rebuild(profile, &settings.tuning, None).is_ok() {
            Mixer::new(sample_rate, bank, levels.clone())
        } else {
            Mixer::new_unbuilt(sample_rate, bank)
        };
        let mixer = Arc::new(Mutex::new(mixer));
        ShakerSystem { settings, levels, rig: ShakeRig::new(), mixer }
    }

    /// Daemon startup: there are no channel groups until a handler requests a rebuild.
    pub fn new_unbuilt(mut settings: ShakerSettings, bank: SampleBank, sample_rate: u32) -> ShakerSystem {
        settings.profile.get_or_insert_with(SpeakerProfile::empty);
        let levels = GroupLevels::empty();
        let mixer = Arc::new(Mutex::new(Mixer::new_unbuilt(sample_rate, bank)));
        ShakerSystem { settings, levels, rig: ShakeRig::new(), mixer }
    }

    /// The mixer, for the audio output to render from.
    pub fn mixer(&self) -> Arc<Mutex<Mixer>> {
        Arc::clone(&self.mixer)
    }

    /// Play `engine` as the engine vibration (default: the built-in engine sound;
    /// [`EngineVoice::from_srs`] gives the FMOD events of the user's SRS banks when available).
    pub fn set_engine(&mut self, engine: EngineVoice) {
        self.lock().set_engine(engine);
    }

    /// Release the prior core/Studio nodes before an output is disabled or replaced.
    /// Frame history and the selected sample bank belong to the app, so they survive.
    /// Release the old device's voices and graph while retaining effect trigger history.
    /// The output owner must be stopped before this call takes the mixer lock.
    pub fn close_output(&mut self, rate: u32) {
        self.lock().close_output(rate);
    }

    /// Recreate an imported Studio engine from its retained library and bank bytes.
    pub fn reopen_engine(&mut self) {
        self.lock().reopen_engine();
    }

    pub fn settings(&self) -> &ShakerSettings {
        &self.settings
    }

    /// The channel-group levels of the last rebuild.
    pub fn levels(&self) -> &GroupLevels {
        &self.levels
    }

    /// The speaker profile the system holds.
    pub fn profile(&self) -> &SpeakerProfile {
        self.settings.profile.as_ref().expect("the system always holds a profile")
    }

    /// Replace the settings without rebuilding the channel groups. Frames read the new tuning
    /// and config at once (the DSP block retunes the current groups' filters, so a held
    /// loop picks up a new cutoff); the group levels keep the profile, volumes and effect levels
    /// of the last [`ShakerSystem::reload_levels`]. A `None` profile keeps the
    /// current one (an unknown `SHAKEBOX.profile`).
    pub fn update_settings(&mut self, mut settings: ShakerSettings) {
        if settings.profile.is_none() {
            settings.profile = self.settings.profile.take();
        }
        self.settings = settings;
    }

    /// Rebuild the channel groups from the current settings;
    /// voices already playing keep the levels they started with (see [`Mixer::set_levels`]).
    pub fn reload_levels(&mut self) -> Result<(), DspError> {
        self.set_focus(None)
    }

    /// Apply a custom speaker matrix edit and rebuild the groups
    /// (see [`SpeakerProfile::edit_speaker_column`]).
    pub fn edit_custom_matrix(&mut self, speaker: Speaker, column: &PerGroup<f64>) -> Result<(), DspError> {
        if let Some(profile) = self.settings.profile.as_mut() {
            profile.edit_speaker_column(speaker, column);
        }
        self.reload_levels()
    }

    /// New group levels for voices started from now on.
    fn set_focus(&mut self, focus: Option<Effect>) -> Result<(), DspError> {
        let profile = self.settings.profile.as_ref().expect("the system always holds a profile");
        self.levels.rebuild(profile, &self.settings.tuning, focus)?;
        self.lock().set_levels(self.levels.clone());
        Ok(())
    }

    /// Publish the mix levels separately from the still-current group/DSP build.
    pub fn begin_level_rebuild(&mut self) {
        self.levels = GroupLevels::empty();
    }
    pub fn publish_level_group(&mut self, group: super::layout::Group, focus: Option<Effect>) -> Result<(), DspError> {
        self.levels.publish_group(
            self.settings.profile.as_ref().expect("system holds a profile"),
            &self.settings.tuning,
            focus,
            group,
        )
    }
    pub fn finish_level_rebuild(&mut self) {
        self.lock().set_levels(self.levels.clone());
    }

    fn lock(&self) -> MutexGuard<'_, Mixer> {
        self.mixer.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// One shaker frame from the shared telemetry. During an
    /// effect test `state.game_running` is whatever the running-listener value holds then: the test
    /// listener the test stored, or what the game-listener thread wrote over it since.
    /// A DSP or engine error ends the frame there and is
    /// returned (the shaker loop logs it and goes on with the next frame).
    pub fn frame(&mut self, telemetry: &Telemetry, state: FrameState) -> Result<(), EngineError> {
        self.run(&ShakeInput::from_telemetry(telemetry), state, telemetry.native_value("lateral_velocity"))
    }

    fn run(
        &mut self,
        input: &ShakeInput,
        state: FrameState,
        native_lateral_velocity: Option<&openrigcue_model::native_value::LegacyValue>,
    ) -> Result<(), EngineError> {
        let s = &self.settings;
        let ctx = FrameContext {
            status: RigStatus::evaluate(state.output_ready, state.game_running, state.switch_on),
            tuning: &s.tuning,
            config: &s.config,
            levels: &self.levels,
            max_traction_loss: s.max_traction_loss,
            native_lateral_velocity,
        };
        let status = ctx.status;
        let before = self.rig.clone();
        let actions = self.rig.frame(input, &ctx);
        let applied = self.lock().apply(&actions);
        match &applied {
            // DSP is the first active-frame block: no voice or memory update
            // follows its failed setter. The mixer keeps its accepted prefix.
            Err(EngineError::InvalidDspFloat { .. } | EngineError::DspParameter { .. }) => self.rig = before,
            Err(_) => self.rig.abort_frame(&before, status),
            Ok(()) => {}
        }
        applied
    }

    pub fn begin_live_frame(telemetry: &Telemetry, state: FrameState) -> FrameProgress {
        Self::begin_live_input(
            ShakeInput::from_telemetry(telemetry),
            state,
            telemetry.native_value("lateral_velocity").cloned(),
        )
    }

    pub fn begin_live_test(frame: &TestFrame, state: FrameState) -> FrameProgress {
        Self::begin_live_input(frame.input, state, None)
    }

    fn begin_live_input(
        input: ShakeInput,
        state: FrameState,
        native_lateral_velocity: Option<openrigcue_model::native_value::LegacyValue>,
    ) -> FrameProgress {
        FrameProgress {
            input,
            native_lateral_velocity,
            frame: LiveFrame::new(RigStatus::evaluate(state.output_ready, state.game_running, state.switch_on)),
            engine: None,
            speakers_failed: false,
            engine_command: None,
            engine_was_paused: false,
            sound: SoundOperation::default(),
        }
    }

    /// Advance one operation after refreshing the caller's current settings.
    pub fn advance_live_frame(&mut self, progress: &mut FrameProgress, stage: FrameStage) -> Result<(), EngineError> {
        let settings = &self.settings;
        let ctx = FrameContext {
            status: progress.frame.status,
            tuning: &settings.tuning,
            config: &settings.config,
            levels: &self.levels,
            max_traction_loss: settings.max_traction_loss,
            native_lateral_velocity: progress.native_lateral_velocity.as_ref(),
        };
        match stage {
            FrameStage::SoundPrepare(voice) => {
                let final_stage = FrameStage::sound_stage(voice, progress.frame.status == RigStatus::Idle);
                let actions = self.rig.prepare_live_actions(&progress.input, &ctx, &mut progress.frame, final_stage);
                let mut operation = SoundOperation {
                    voice: Some(voice),
                    catch_errors: progress.frame.status == RigStatus::Active,
                    ..SoundOperation::default()
                };
                for action in actions {
                    match action {
                        super::rig::Action::Stop(_) => operation.stop = true,
                        super::rig::Action::Start { sample, plays, group, .. } => {
                            operation.start = Some((sample, plays, group))
                        }
                        action => operation.failure = Some(action),
                    }
                }
                progress.sound = operation;
                return Ok(());
            }
            FrameStage::SoundReleaseLookup => {
                if progress.sound.stop {
                    progress.sound.release = self.lock().native_slot(progress.sound.voice.unwrap());
                }
                return Ok(());
            }
            FrameStage::SoundRelease => {
                if progress.sound.stop {
                    let result = match &progress.sound.release {
                        Some(sound) => self.lock().native_release(sound),
                        None => Err(EngineError::SoundOperation {
                            error: DspError {
                                class: "AttributeError",
                                message: "'NoneType' object has no attribute 'release'".into(),
                            },
                        }),
                    };
                    if let Err(error) = result {
                        log_sound_error(&progress.sound, "release", error)?;
                        progress.sound.release_failed = true;
                    }
                }
                return Ok(());
            }
            FrameStage::SoundClear => {
                if progress.sound.stop && !progress.sound.release_failed {
                    let voice = progress.sound.voice.unwrap();
                    self.lock().native_clear(voice);
                    self.rig.set_held(voice, false);
                }
                return Ok(());
            }
            FrameStage::SoundCreate => {
                if let Some((key, plays, _)) = progress.sound.start {
                    let voice = progress.sound.voice.unwrap();
                    self.lock().native_create(voice, key, plays != super::rig::Plays::Once);
                    self.rig.set_held(voice, true);
                }
                return Ok(());
            }
            FrameStage::SoundLoops => {
                if let Some((_, plays, _)) = progress.sound.start {
                    if FrameStage::explicit_loop_count(progress.sound.voice.unwrap(), plays) {
                        if let Err(error) = self.lock().native_set_loops(progress.sound.voice.unwrap(), plays) {
                            log_sound_error(&progress.sound, "create", error)?;
                        }
                    }
                }
                return Ok(());
            }
            FrameStage::EngineBegin => {
                let command = super::rig::engine_command(&progress.input, &ctx);
                let selector = super::engine_sound::engine_selector(&command)?;
                self.lock().engine_mut().pause_instance_live(selector.other_instance())?;
                progress.engine_command = Some(command);
                return Ok(());
            }
            FrameStage::EngineSelect => {
                progress.engine = Some(
                    super::engine_sound::engine_selector(
                        progress.engine_command.as_ref().expect("begin precedes select"),
                    )?
                    .instance()?,
                );
                return Ok(());
            }
            FrameStage::EngineCheckPaused => {
                progress.engine_was_paused =
                    self.lock().engine_mut().paused_live(progress.engine.expect("select precedes paused"))?;
                return Ok(());
            }
            FrameStage::EngineResume => {
                if progress.engine_was_paused {
                    self.lock().engine_mut().resume_live(progress.engine.expect("select precedes resume"))?;
                }
                return Ok(());
            }
            FrameStage::EngineRpm => {
                return self.lock().engine_mut().rpm_live(
                    progress.engine.expect("select precedes rpm"),
                    progress.engine_command.as_ref().expect("begin precedes rpm").rpm,
                );
            }
            FrameStage::EngineLoad => {
                return self.lock().engine_mut().load_live(
                    progress.engine.expect("select precedes load"),
                    progress.engine_command.as_ref().expect("begin precedes load").load,
                );
            }
            FrameStage::EngineSpeaker(slot) => {
                if !progress.speakers_failed {
                    let command = super::rig::engine_command(&progress.input, &ctx);
                    let value = super::engine_sound::engine_levels(&command)
                        .map(|value| value.map(|levels| levels[slot]))
                        .map_err(Clone::clone);
                    progress.speakers_failed = !self.lock().engine_mut().speaker_live(
                        progress.engine.expect("begin stage precedes speakers"),
                        slot,
                        value,
                    )?;
                }
                return Ok(());
            }
            FrameStage::EnginePitch => {
                let pitch = super::engine_sound::engine_pitch(&super::rig::engine_command(&progress.input, &ctx))?;
                return self
                    .lock()
                    .engine_mut()
                    .pitch_live(progress.engine.expect("begin stage precedes pitch"), pitch);
            }
            _ => {}
        }
        if stage.sound_voice().is_some() {
            if let Some((_, _, group)) = progress.sound.start {
                if let Err(error) = self.lock().native_play(progress.sound.voice.unwrap(), group) {
                    log_sound_error(&progress.sound, "play", error)?;
                }
            }
            if let Some(action) = progress.sound.failure.take() {
                self.lock().apply(&[action])?;
            }
            return Ok(());
        }
        let actions = self.rig.advance_live_frame(&progress.input, &ctx, &mut progress.frame, stage);
        self.lock().apply(&actions)
    }

    /// Begin the effect test for `effect`: voices started during the test play
    /// other effects at level zero; loops already playing keep sounding.
    /// Feed [`super::effect_test::effect_test_frames`] through [`ShakerSystem::test_frame`]
    /// [`super::effect_test::TEST_FRAME_GAP`] apart, then call [`ShakerSystem::end_effect_test`].
    /// Telemetry frames may keep arriving through [`ShakerSystem::frame`] meanwhile.
    pub fn begin_effect_test(&mut self, effect: Effect) -> Result<(), DspError> {
        self.set_focus(Some(effect))
    }

    /// One frame of the effect test (a rig frame with canned telemetry). The test
    /// stores the test listener as the shared running-listener value right before
    /// frames 1-12 ([`TestFrame::forced_active`]) and the real one back after the 12th
    /// ([`TestFrame::forced_after`]); the caller does those stores and passes the listener the
    /// shared state holds as `state.game_running`. A DSP or engine error is returned
    /// (it ends the test, see [`ShakerSystem::frame`]).
    pub fn test_frame(&mut self, frame: &TestFrame, state: FrameState) -> Result<(), EngineError> {
        self.run(&frame.input, state, None)
    }

    /// Restore the normal group levels after the effect test (a rebuild after the
    /// 13th frame). Loops started during the test keep their test levels until they restart.
    pub fn end_effect_test(&mut self) -> Result<(), DspError> {
        self.reload_levels()
    }

    /// Play transmission/1 once on one speaker at `volume / 20` (the speaker test).
    pub fn test_speaker(&mut self, speaker: Speaker) -> Result<(), DspError> {
        let levels = required_speaker_test_levels(&self.settings.tuning, speaker)?;
        self.lock().play_direct(Voice::SpeakerTest, SampleKey::new(SampleFolder::Transmission, 1), levels);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shakers::effect_test::effect_test_frames;

    #[test]
    fn clear_between_release_branch_and_receiver_lookup_raises_or_is_caught() {
        for (voice, stage, caught) in
            [(Voice::Transmission, FrameStage::Gear, false), (Voice::Impact, FrameStage::Impact, true)]
        {
            let mut system = ShakerSystem::new(settings(), SampleBank::synthesized(), 48000);
            let mut input = Telemetry::off();
            input.harmonized_gear = 1.0;
            input.longitudinal_acceleration = 30.0;
            let mut seed = ShakerSystem::begin_live_frame(&input, ON);
            for op in [FrameStage::SoundPrepare(voice), FrameStage::SoundCreate, FrameStage::SoundLoops, stage] {
                system.advance_live_frame(&mut seed, op).unwrap();
            }
            let mut progress = ShakerSystem::begin_live_frame(&input, ON);
            system.advance_live_frame(&mut progress, FrameStage::SoundPrepare(voice)).unwrap();
            system.lock().native_clear(voice);
            system.advance_live_frame(&mut progress, FrameStage::SoundReleaseLookup).unwrap();
            let result = system.advance_live_frame(&mut progress, FrameStage::SoundRelease);
            assert_eq!(result.is_ok(), caught);
            if let Err(error) = result {
                let native = error.native_error().unwrap();
                assert_eq!(native.class, "AttributeError");
                assert_eq!(native.message, "'NoneType' object has no attribute 'release'");
            }
        }
    }

    #[test]
    fn release_receiver_is_read_after_branch_and_retained_after_later_clear() {
        let mut system = ShakerSystem::new(settings(), SampleBank::synthesized(), 48000);
        let mut input = Telemetry::off();
        input.harmonized_gear = 1.0;
        let mut seed = ShakerSystem::begin_live_frame(&input, ON);
        for stage in [FrameStage::SoundPrepare(Voice::Transmission), FrameStage::SoundCreate, FrameStage::Gear] {
            system.advance_live_frame(&mut seed, stage).unwrap();
        }
        let first = system.lock().native_slot(Voice::Transmission).unwrap();
        let mut older = ShakerSystem::begin_live_frame(&input, ON);
        system.advance_live_frame(&mut older, FrameStage::SoundPrepare(Voice::Transmission)).unwrap();
        let mut newer = ShakerSystem::begin_live_frame(&input, ON);
        for stage in [FrameStage::SoundPrepare(Voice::Transmission), FrameStage::SoundCreate, FrameStage::Gear] {
            system.advance_live_frame(&mut newer, stage).unwrap();
        }
        let second = system.lock().native_slot(Voice::Transmission).unwrap();
        system.advance_live_frame(&mut older, FrameStage::SoundReleaseLookup).unwrap();
        system.lock().native_clear(Voice::Transmission);
        system.advance_live_frame(&mut older, FrameStage::SoundRelease).unwrap();
        assert!(system.lock().native_release(&second).is_err(), "receiver was second live Sound");
        assert!(system.lock().native_release(&first).is_ok(), "first sound was not released by stale branch");
    }

    #[test]
    fn native_sound_local_diagnostics_keep_exact_labels_and_early_errors_escape() {
        let error = || EngineError::SoundOperation {
            error: DspError { class: "pyfmodex.exceptions.FmodError", message: "INVALID HANDLE".into() },
        };
        let operation = |voice| SoundOperation { voice: Some(voice), catch_errors: true, ..SoundOperation::default() };
        assert_eq!(
            sound_error_message(&operation(Voice::Impact), "release", &error()).as_deref(),
            Some("Error releasing impact mixer INVALID HANDLE")
        );
        assert_eq!(
            sound_error_message(&operation(Voice::WheelSlipRf), "play", &error()).as_deref(),
            Some("Error playing wheel_slip_rf_mixer soundINVALID HANDLE")
        );
        assert_eq!(
            sound_error_message(&operation(Voice::RoadBump), "play", &error()).as_deref(),
            Some("Error while playing road bump sound INVALID HANDLE")
        );
        assert!(sound_error_message(&operation(Voice::Transmission), "release", &error()).is_none());
        let idle = SoundOperation { voice: Some(Voice::Impact), catch_errors: false, ..SoundOperation::default() };
        assert!(sound_error_message(&idle, "release", &error()).is_none());
    }

    #[test]
    fn clear_after_another_create_forgets_the_new_slot_but_keeps_its_playing_channel() {
        let mut system = ShakerSystem::new(settings(), SampleBank::synthesized(), 48000);
        let mut input = Telemetry::off();
        input.harmonized_gear = 1.0;
        let mut initial = ShakerSystem::begin_live_frame(&input, ON);
        for op in [FrameStage::SoundPrepare(Voice::Transmission), FrameStage::SoundCreate, FrameStage::Gear] {
            system.advance_live_frame(&mut initial, op).unwrap();
        }
        let mut first = ShakerSystem::begin_live_frame(&input, ON);
        system.advance_live_frame(&mut first, FrameStage::SoundPrepare(Voice::Transmission)).unwrap();
        system.advance_live_frame(&mut first, FrameStage::SoundReleaseLookup).unwrap();
        system.advance_live_frame(&mut first, FrameStage::SoundRelease).unwrap();
        let mut second = ShakerSystem::begin_live_frame(&input, ON);
        system.advance_live_frame(&mut second, FrameStage::SoundPrepare(Voice::Transmission)).unwrap();
        for op in [FrameStage::SoundCreate, FrameStage::Gear] {
            system.advance_live_frame(&mut second, op).unwrap();
        }
        system.advance_live_frame(&mut first, FrameStage::SoundClear).unwrap();
        assert!(system.lock().native_slot(Voice::Transmission).is_none());
        let mut out = vec![0.0; 64 * 6];
        system.lock().render(&mut out);
        assert!(out.iter().any(|v| *v != 0.0));
    }

    #[test]
    fn retained_sound_handle_survives_global_overwrite_and_is_invalid_after_device_close() {
        let mut system = ShakerSystem::new(settings(), SampleBank::synthesized(), 48000);
        let mut input = Telemetry::off();
        input.harmonized_gear = 1.0;
        let mut first = ShakerSystem::begin_live_frame(&input, ON);
        for op in [FrameStage::SoundPrepare(Voice::Transmission), FrameStage::SoundCreate, FrameStage::Gear] {
            system.advance_live_frame(&mut first, op).unwrap();
        }
        let mut second = ShakerSystem::begin_live_frame(&input, ON);
        system.advance_live_frame(&mut second, FrameStage::SoundPrepare(Voice::Transmission)).unwrap();
        system.advance_live_frame(&mut second, FrameStage::SoundReleaseLookup).unwrap();
        system.lock().native_clear(Voice::Transmission);
        system.close_output(48000);
        assert!(system.advance_live_frame(&mut second, FrameStage::SoundRelease).is_err());
    }

    #[test]
    fn looping_sound_error_has_local_catch_only_for_later_effect_family() {
        for (voice, stage, caught) in
            [(Voice::LandingGearRight, FrameStage::LandingGearRight, false), (Voice::Impact, FrameStage::Impact, true)]
        {
            let mut system = ShakerSystem::new(settings(), SampleBank::synthesized(), 48000);
            let mut input = Telemetry::off();
            input.landing_gear_right = 0.5;
            input.longitudinal_acceleration = 30.0;
            let mut progress = ShakerSystem::begin_live_frame(&input, ON);
            system.advance_live_frame(&mut progress, FrameStage::SoundPrepare(voice)).unwrap();
            system.advance_live_frame(&mut progress, FrameStage::SoundCreate).unwrap();
            system.lock().native_clear(voice);
            assert_eq!(system.advance_live_frame(&mut progress, FrameStage::SoundLoops).is_ok(), caught);
            if caught {
                assert!(system.advance_live_frame(&mut progress, stage).is_ok());
            }
        }
    }

    #[test]
    fn live_sound_family_matches_whole_frame_pcm_without_interleaving() {
        let mut actual = ShakerSystem::new(settings(), SampleBank::synthesized(), 48000);
        let mut expected = ShakerSystem::new(settings(), SampleBank::synthesized(), 48000);
        for n in 0..25 {
            let mut input = Telemetry::off();
            input.harmonized_gear = (n % 5) as f64;
            input.speed_kmh = 40.0;
            input.landing_gear_left = if n % 2 == 0 { 0.5 } else { 0.0 };
            input.landing_gear_nose = input.landing_gear_left;
            input.landing_gear_right = input.landing_gear_left;
            input.canopy = input.landing_gear_left;
            input.turbulence = (n % 3) as f64;
            input.afterburner = (n % 2) as f64;
            input.cannon_firing = input.afterburner;
            input.missile_firing = input.afterburner;
            input.longitudinal_acceleration = (n % 7) as f64 - 3.0;
            input.lateral_acceleration = (n % 9) as f64 - 4.0;
            input.wheel_slip_lf = (n % 6) as f64;
            input.wheel_slip_rf = input.wheel_slip_lf;
            input.suspension_velocity_lf = (n % 8) as f64;
            input.suspension_velocity_rf = input.suspension_velocity_lf;
            let state = if n % 8 == 7 { FrameState { switch_on: false, ..ON } } else { ON };
            expected.frame(&input, state).unwrap();
            let mut progress = ShakerSystem::begin_live_frame(&input, state);
            for stage in progress.stages() {
                actual.advance_live_frame(&mut progress, stage).unwrap();
            }
            let mut a = vec![0.0; 1024 * 6];
            let mut b = a.clone();
            actual.lock().render(&mut a);
            expected.lock().render(&mut b);
            assert_eq!(a, b, "frame{n}");
        }
    }

    #[test]
    fn release_failures_follow_each_source_catch_and_preserve_the_slot() {
        for (voice, stage, caught) in
            [(Voice::Transmission, FrameStage::Gear, false), (Voice::Impact, FrameStage::Impact, true)]
        {
            let mut system = ShakerSystem::new(settings(), SampleBank::synthesized(), 48000);
            let mut input = Telemetry::off();
            input.harmonized_gear = 1.0;
            input.longitudinal_acceleration = 30.0;
            let mut first = ShakerSystem::begin_live_frame(&input, ON);
            for op in [FrameStage::SoundPrepare(voice), FrameStage::SoundCreate, FrameStage::SoundLoops, stage] {
                system.advance_live_frame(&mut first, op).unwrap();
            }
            let old = system.lock().native_slot(voice).unwrap();
            let mut second = ShakerSystem::begin_live_frame(&input, ON);
            system.advance_live_frame(&mut second, FrameStage::SoundPrepare(voice)).unwrap();
            system.lock().native_release(&old).unwrap();
            system.advance_live_frame(&mut second, FrameStage::SoundReleaseLookup).unwrap();
            assert_eq!(system.advance_live_frame(&mut second, FrameStage::SoundRelease).is_ok(), caught);
            if caught {
                system.advance_live_frame(&mut second, FrameStage::SoundClear).unwrap();
            }
            assert!(Arc::ptr_eq(&old, &system.lock().native_slot(voice).unwrap()), "failed release skips clear");
        }
    }

    #[test]
    fn concurrent_sound_creation_and_play_use_the_live_slot_not_the_prepared_sample() {
        let mut system = ShakerSystem::new(settings(), SampleBank::synthesized(), 48000);
        let mut input = Telemetry::off();
        input.harmonized_gear = 1.0;
        let mut first = ShakerSystem::begin_live_frame(&input, ON);
        let mut second = ShakerSystem::begin_live_frame(&input, ON);
        system.advance_live_frame(&mut first, FrameStage::SoundPrepare(Voice::Transmission)).unwrap();
        system.advance_live_frame(&mut second, FrameStage::SoundPrepare(Voice::Transmission)).unwrap();
        system.advance_live_frame(&mut first, FrameStage::SoundCreate).unwrap();
        let id = system.lock().native_slot(Voice::Transmission).unwrap();
        system.advance_live_frame(&mut first, FrameStage::Gear).unwrap();
        system.advance_live_frame(&mut second, FrameStage::SoundCreate).unwrap();
        system.advance_live_frame(&mut second, FrameStage::Gear).unwrap();
        let current = system.lock().native_slot(Voice::Transmission).unwrap();
        assert!(!Arc::ptr_eq(&id, &current));
        // The first sound's channel is still audible after its slot was replaced.
        let mixer = system.mixer();
        let mut mixer = mixer.lock().unwrap();
        mixer.native_release(&current).unwrap();
        let mut output = vec![0.0; 64 * 6];
        mixer.render(&mut output);
        assert!(output.iter().any(|value| *value != 0.0));
        mixer.native_release(&id).unwrap();
        output.fill(1.0);
        mixer.render(&mut output);
        assert!(output.iter().all(|value| *value == 0.0));
    }

    #[test]
    fn two_play_calls_after_overwrite_both_reference_the_new_global_sound() {
        let mut system = ShakerSystem::new(settings(), SampleBank::synthesized(), 48000);
        let mut input = Telemetry::off();
        input.harmonized_gear = 1.0;
        let mut first = ShakerSystem::begin_live_frame(&input, ON);
        let mut second = ShakerSystem::begin_live_frame(&input, ON);
        for progress in [&mut first, &mut second] {
            system.advance_live_frame(progress, FrameStage::SoundPrepare(Voice::Transmission)).unwrap();
        }
        system.advance_live_frame(&mut first, FrameStage::SoundCreate).unwrap();
        system.advance_live_frame(&mut second, FrameStage::SoundCreate).unwrap();
        let id = system.lock().native_slot(Voice::Transmission).unwrap();
        system.advance_live_frame(&mut first, FrameStage::Gear).unwrap();
        system.advance_live_frame(&mut second, FrameStage::Gear).unwrap();
        system.lock().native_release(&id).unwrap();
        let mut output = vec![1.0; 64 * 6];
        system.lock().render(&mut output);
        assert!(output.iter().all(|value| *value == 0.0));
    }

    #[test]
    fn overlapping_live_frames_keep_independent_late_commits_and_failure_prefixes() {
        let mut system = ShakerSystem::new(settings(), SampleBank::synthesized(), 48000);
        let mut first_input = Telemetry::off();
        first_input.harmonized_gear = 1.0;
        let mut second_input = first_input.clone();
        second_input.harmonized_gear = 2.0;
        let mut first = ShakerSystem::begin_live_frame(&first_input, ON);
        for stage in first.stages().into_iter().take_while(|stage| *stage != FrameStage::EngineBegin) {
            system.advance_live_frame(&mut first, stage).unwrap();
        }
        let mut second = ShakerSystem::begin_live_frame(&second_input, ON);
        for stage in second.stages() {
            system.advance_live_frame(&mut second, stage).unwrap();
        }
        for stage in first.stages().into_iter().skip_while(|stage| *stage != FrameStage::EngineBegin) {
            system.advance_live_frame(&mut first, stage).unwrap();
        }
        system.lock().apply(&[super::super::rig::Action::Stop(Voice::Transmission)]).unwrap();
        let mut unchanged = ShakerSystem::begin_live_frame(&first_input, ON);
        for stage in unchanged.stages() {
            system.advance_live_frame(&mut unchanged, stage).unwrap();
        }
        assert!(!system.lock().is_playing(Voice::Transmission), "later completion of first frame stored gear1");

        let mut bad = settings();
        bad.tuning.native_traction_max = Some(Err(DspError { class: "KeyError", message: "'traction_loss'".into() }));
        system.update_settings(bad);
        let mut failed = ShakerSystem::begin_live_frame(&second_input, ON);
        let mut reached = false;
        for stage in failed.stages() {
            if system.advance_live_frame(&mut failed, stage).is_err() {
                reached = true;
                break;
            }
        }
        assert!(reached);
        assert!(system.lock().is_playing(Voice::Transmission));
        system.update_settings(settings());
        system.lock().apply(&[super::super::rig::Action::Stop(Voice::Transmission)]).unwrap();
        let mut retry = ShakerSystem::begin_live_frame(&second_input, ON);
        for stage in retry.stages() {
            system.advance_live_frame(&mut retry, stage).unwrap();
        }
        assert!(system.lock().is_playing(Voice::Transmission), "failed frame did not commit gear2");
    }

    #[test]
    fn active_engine_observes_empty_and_partial_rebuild_levels_while_voices_keep_old_graph() {
        use super::super::engine_sound::capture_engine_output;
        use super::super::layout::Group;
        let mut settings = settings();
        settings.tuning.native_engine = Some(Box::new(super::super::tuning::NativeEngineSettings {
            flag: Ok(openrigcue_model::native_value::LegacyValue::Int(0)),
            fade_start: Ok(openrigcue_model::native_value::LegacyValue::Int(0)),
            fade_end: Ok(openrigcue_model::native_value::LegacyValue::Int(100)),
            resonance: Ok(35.0),
        }));
        let mut system = ShakerSystem::new(settings, SampleBank::synthesized(), 48000);
        let mut telemetry = Telemetry::off();
        telemetry.rpm = 3000.0;
        telemetry.max_rpm = 7000.0;
        telemetry.harmonized_gear = 1.0;
        system.frame(&telemetry, ON).unwrap();
        system.begin_level_rebuild();
        assert!(system.lock().is_playing(Voice::Transmission));
        let (result, lines) = capture_engine_output(false, || {
            let mut progress = ShakerSystem::begin_live_frame(&telemetry, ON);
            for stage in progress.stages() {
                system.advance_live_frame(&mut progress, stage)?;
            }
            Ok::<_, EngineError>(())
        });
        result.unwrap();
        assert_eq!(lines, ["error mixed level outputs not loaded yet 'engine'\n"]);
        assert!(system.lock().is_playing(Voice::Transmission));
        system.publish_level_group(Group::Engine, None).unwrap();
        assert!(system.levels().required_of(Group::Gear).is_err());
        let (result, lines) = capture_engine_output(false, || {
            let mut progress = ShakerSystem::begin_live_frame(&telemetry, ON);
            for stage in progress.stages() {
                system.advance_live_frame(&mut progress, stage)?;
            }
            Ok::<_, EngineError>(())
        });
        result.unwrap();
        assert!(lines.is_empty());
        for group in Group::ALL.into_iter().skip(1) {
            system.publish_level_group(group, None).unwrap();
        }
        system.finish_level_rebuild();
        assert!(system.lock().is_playing(Voice::Transmission));
    }

    #[test]
    fn live_engine_speaker_stages_read_new_levels_and_pitch_between_native_calls() {
        for split in 0..6 {
            let mut initial = settings();
            initial.tuning.volume = [2.0, 4.0, 6.0, 8.0, 10.0, 12.0];
            initial.tuning.effect_mut(Effect::Engine).low_pass = false;
            let mut changed = initial.clone();
            changed.tuning.volume = [14.0, 12.0, 10.0, 8.0, 6.0, 4.0];
            changed.tuning.effect_mut(Effect::Engine).low_pass = true;
            changed.tuning.effect_mut(Effect::Engine).resonance = 77.0;
            changed.tuning.engine_fade_start = 200.0;
            changed.tuning.engine_fade_end = 210.0;
            let mut mixed = changed.clone();
            mixed.tuning.volume[..=split].copy_from_slice(&initial.tuning.volume[..=split]);
            let mut expected = ShakerSystem::new(mixed, SampleBank::synthesized(), 48000);
            let mut actual = ShakerSystem::new(initial, SampleBank::synthesized(), 48000);
            let mut input = Telemetry::off();
            input.rpm = 3500.0;
            input.max_rpm = 7000.0;
            expected.frame(&input, ON).unwrap();
            let mut progress = ShakerSystem::begin_live_frame(&input, ON);
            for stage in progress.stages() {
                actual.advance_live_frame(&mut progress, stage).unwrap();
                if stage == FrameStage::EngineSpeaker(split) {
                    actual.update_settings(changed.clone());
                    actual.reload_levels().unwrap();
                }
            }
            for system in [&actual, &expected] {
                system.lock().apply(&Voice::RIG.map(super::super::rig::Action::Stop)).unwrap();
            }
            let mut a = vec![0.0; 2048 * 6];
            let mut b = a.clone();
            actual.lock().render(&mut a);
            expected.lock().render(&mut b);
            assert!(a == b, "speaker split after {split}");
        }
    }

    #[test]
    fn live_engine_speaker_failure_skips_later_speakers_but_runs_live_pitch_and_late_commit() {
        use super::super::engine_sound::capture_engine_output;
        for print_fails in [false, true] {
            let mut initial = settings();
            initial.tuning.effect_mut(Effect::Engine).low_pass = false;
            let mut actual = ShakerSystem::new(initial.clone(), SampleBank::synthesized(), 48000);
            let mut input = Telemetry::off();
            input.rpm = 3500.0;
            input.max_rpm = 7000.0;
            input.harmonized_gear = 1.0;
            let mut progress = ShakerSystem::begin_live_frame(&input, ON);
            let (result, lines) = capture_engine_output(print_fails, || {
                for stage in progress.stages() {
                    actual.advance_live_frame(&mut progress, stage)?;
                    if stage == FrameStage::EngineSpeaker(1) {
                        let mut bad = initial.clone();
                        bad.tuning.native_engine = Some(Box::new(super::super::tuning::NativeEngineSettings {
                            flag: Ok(openrigcue_model::native_value::LegacyValue::Int(1)),
                            fade_start: Err(DspError { class: "KeyError", message: "'fade_start'".into() }),
                            fade_end: Ok(openrigcue_model::native_value::LegacyValue::Int(100)),
                            resonance: Ok(77.0),
                        }));
                        actual.update_settings(bad);
                    }
                }
                Ok::<_, EngineError>(())
            });
            assert_eq!(lines, ["error mixed level outputs not loaded yet 'fade_start'\n"]);
            assert_eq!(result.is_err(), print_fails);
            assert!(actual.lock().engine_playing());
            let expected = ShakerSystem::new(initial.clone(), SampleBank::synthesized(), 48000);
            let mix = actual.levels().of(super::super::layout::Group::Engine);
            expected
                .lock()
                .apply(&[super::super::rig::Action::Engine(super::super::rig::EngineCommand {
                    engine_type: initial.tuning.engine_type,
                    native_engine_selector: None,
                    native_parameters: None,
                    rpm: 5000.0,
                    load: 1.0,
                    speaker_levels: Some([mix[0], mix[1], 0.0, 0.0, 0.0, 0.0]),
                    pitch: if print_fails { 35.0 } else { 77.0 },
                })])
                .unwrap();
            actual.lock().apply(&Voice::RIG.map(super::super::rig::Action::Stop)).unwrap();
            let mut a = vec![0.0; 1024 * 6];
            let mut b = a.clone();
            actual.lock().render(&mut a);
            expected.lock().render(&mut b);
            assert!(a == b, "speaker error must preserve prefix and {} pitch", if print_fails { "old" } else { "new" });
            actual.update_settings(initial);
            let mut retry = ShakerSystem::begin_live_frame(&input, ON);
            for stage in retry.stages() {
                actual.advance_live_frame(&mut retry, stage).unwrap();
            }
            assert_eq!(
                actual.lock().is_playing(Voice::Transmission),
                print_fails,
                "failed print skips late gear commit"
            );
        }
    }

    #[test]
    fn live_dsp_parameter_changes_keep_the_values_already_read_by_the_frame() {
        use super::super::rig::DspStage;
        for after in DspStage::ALL {
            let mut initial = settings();
            let gear = initial.tuning.effect_mut(Effect::Gear);
            gear.low_pass = true;
            gear.cutoff_hz = 123.0;
            gear.resonance = 1.0;
            let mut changed = initial.clone();
            let gear = changed.tuning.effect_mut(Effect::Gear);
            gear.low_pass = false;
            gear.cutoff_hz = 444.0;
            gear.resonance = 2.5;
            let mut expected_settings = changed.clone();
            let gear = expected_settings.tuning.effect_mut(Effect::Gear);
            if after != DspStage::Remove {
                gear.low_pass = true;
            }
            if matches!(after, DspStage::Frequency | DspStage::Q | DspStage::Attach) {
                gear.cutoff_hz = 123.0;
            }
            if matches!(after, DspStage::Q | DspStage::Attach) {
                gear.resonance = 1.0;
            }
            let mut control = ShakerSystem::new(expected_settings, SampleBank::synthesized(), 48000);
            let mut live = ShakerSystem::new(initial, SampleBank::synthesized(), 48000);
            let mut input = Telemetry::off();
            input.harmonized_gear = 1.0;
            control.frame(&input, ON).unwrap();
            let mut progress = ShakerSystem::begin_live_frame(&input, ON);
            for stage in progress.stages() {
                live.advance_live_frame(&mut progress, stage).unwrap();
                if stage == FrameStage::FilterGroup(super::super::layout::Group::Gear, after) {
                    live.update_settings(changed.clone());
                }
            }
            let mut a = vec![0.0; 2048 * 6];
            let mut b = a.clone();
            live.lock().render(&mut a);
            control.lock().render(&mut b);
            assert_eq!(a, b, "mixed parameter snapshot after {after:?}");
        }
    }

    #[test]
    fn live_stage_failure_prefixes_and_recovery_match_whole_frame_fixtures() {
        use super::super::rig::Action;
        use openrigcue_model::native_value::LegacyValue;
        fn run_live(system: &mut ShakerSystem, input: &Telemetry, state: FrameState) -> Result<(), EngineError> {
            let mut progress = ShakerSystem::begin_live_frame(input, state);
            for stage in progress.stages() {
                system.advance_live_frame(&mut progress, stage)?;
            }
            Ok(())
        }
        let mut failures = Vec::new();
        let mut dsp = settings();
        dsp.tuning.effect_mut(Effect::Gear).cutoff_hz = f64::NAN;
        failures.push(dsp);
        let mut traction = settings();
        traction.tuning.native_traction_max = Some(Err(DspError { class: "KeyError", message: "'traction'".into() }));
        failures.push(traction);
        let mut suspension = settings();
        let mut native = native_suspension_settings();
        native.min_suspension_velocity = Err(DspError { class: "ValueError", message: "bad suspension".into() });
        suspension.tuning.native_suspension = Some(Box::new(native));
        failures.push(suspension);
        let mut bump = settings();
        let mut native = native_suspension_settings();
        native.min_bump_velocity = Err(DspError { class: "ValueError", message: "bad bump".into() });
        bump.tuning.native_suspension = Some(Box::new(native));
        failures.push(bump);
        let mut engine = settings();
        engine.tuning.native_engine_selector = Some(Ok(LegacyValue::from("X")));
        failures.push(engine);
        for (case, failure) in failures.into_iter().enumerate() {
            let mut pure = ShakerSystem::new(settings(), SampleBank::synthesized(), 48000);
            let mut live = ShakerSystem::new(settings(), SampleBank::synthesized(), 48000);
            let mut input = Telemetry::off();
            input.harmonized_gear = 1.0;
            input.rpm = 4000.0;
            input.max_rpm = 7000.0;
            input.speed_kmh = 50.0;
            input.wheel_slip_lf = 12.0;
            input.suspension_velocity_lf = 0.2;
            input.suspension_velocity_rf = 0.2;
            input.vertical_acceleration = 4.5;
            for phase in 0..5 {
                let state = if phase == 4 { NO_GAME } else { ON };
                if phase == 1 {
                    pure.update_settings(failure.clone());
                    live.update_settings(failure.clone());
                    input.harmonized_gear = 2.0;
                    input.wheel_slip_lf = 6.0;
                    input.vertical_acceleration = 7.0;
                } else if phase == 2 {
                    pure.update_settings(settings());
                    live.update_settings(settings());
                    for system in [&pure, &live] {
                        system
                            .lock()
                            .apply(&[Action::Stop(Voice::Transmission), Action::Stop(Voice::WheelSlipLf)])
                            .unwrap();
                    }
                }
                let expected = pure.frame(&input, state);
                let actual = run_live(&mut live, &input, state);
                assert_eq!(format!("{actual:?}"), format!("{expected:?}"), "case {case}, phase {phase}");
                if phase == 1 {
                    assert!(actual.is_err(), "case {case} must reach its source failure");
                }
                let mut a = vec![0.0; 1067 * 6];
                let mut b = a.clone();
                pure.lock().render(&mut a);
                live.lock().render(&mut b);
                assert_eq!(a, b, "PCM case {case}, phase {phase}");
            }
        }
    }

    #[test]
    fn output_recreation_preserves_frame_history_without_resurrecting_held_samples() {
        let mut system = ShakerSystem::new(settings(), SampleBank::synthesized(), 48000);
        let mut telemetry = Telemetry::off();
        telemetry.harmonized_gear = 1.;
        telemetry.wheel_slip_lf = 12.;
        system.frame(&telemetry, ON).unwrap();
        assert!(system.lock().is_playing(Voice::Transmission));
        assert!(system.lock().is_playing(Voice::WheelSlipLf));
        let before = format!("{:?}", system.rig);
        system.close_output(48000);
        system.lock().set_sample_rate(96000);
        assert_eq!(format!("{:?}", system.rig), before);
        assert!(!system.lock().is_playing(Voice::Transmission));
        assert!(!system.lock().is_playing(Voice::WheelSlipLf));
        system.reload_levels().unwrap();
        system.reopen_engine();
        system.frame(&telemetry, ON).unwrap();
        assert!(!system.lock().is_playing(Voice::Transmission), "same gear does not retrigger after core replacement");
        assert!(!system.lock().is_playing(Voice::WheelSlipLf), "same file leaves invalidated old sample silent");
        telemetry.harmonized_gear = 2.;
        telemetry.wheel_slip_lf = 6.;
        system.frame(&telemetry, ON).unwrap();
        assert!(system.lock().is_playing(Voice::Transmission));
        assert!(system.lock().is_playing(Voice::WheelSlipLf));
    }

    fn native_suspension_settings() -> super::super::tuning::NativeSuspensionSettings {
        super::super::tuning::NativeSuspensionSettings {
            side_enhancement: Ok(crate::shakers::tuning::NativeInt::Int(1)),
            min_side_enhancement: Ok(crate::shakers::tuning::NativeInt::Int(2)),
            min_suspension_velocity: Ok(crate::shakers::tuning::NativeInt::Int(90)),
            max_suspension_velocity: Ok(crate::shakers::tuning::NativeInt::Int(100_000)),
            min_bump_velocity: Ok(crate::shakers::tuning::NativeInt::Int(70)),
            max_bump_velocity: Ok(crate::shakers::tuning::NativeInt::Int(100_000)),
        }
    }

    #[test]
    fn native_suspension_error_retains_prior_voice_actions_but_no_file_commits() {
        let mut s = settings();
        s.tuning.native_suspension = Some(Box::new(native_suspension_settings()));
        let mut sys = ShakerSystem::new(s.clone(), SampleBank::synthesized(), 48_000);
        let mut frame = Telemetry::off();
        frame.rpm = 4000.0;
        frame.speed_kmh = 50.0;
        frame.harmonized_gear = 1.0;
        frame.wheel_slip_lf = 12.0;
        frame.suspension_travel_lf = 0.01;
        frame.suspension_travel_rf = 0.01;
        sys.frame(&frame, ON).unwrap();
        assert!(sys.lock().is_playing(Voice::SuspensionLf));
        assert!(sys.lock().is_playing(Voice::SuspensionRf));
        s.tuning.native_suspension.as_mut().unwrap().min_suspension_velocity =
            Err(DspError { class: "KeyError", message: "'min_suspension_velocity'".into() });
        sys.update_settings(s.clone());
        frame.harmonized_gear = 2.0;
        frame.wheel_slip_lf = 6.0;
        frame.suspension_travel_lf = 0.014;
        frame.suspension_travel_rf = 0.014;
        frame.suspension_velocity_rf = 0.2;
        assert!(matches!(sys.frame(&frame, ON), Err(EngineError::SuspensionParameter { .. })));
        assert!(sys.lock().is_playing(Voice::Transmission));
        assert!(sys.lock().engine_playing(), "the later engine block leaves its previous voice held");
        assert!(sys.lock().is_playing(Voice::SuspensionRf), "failing current corner kept its prior voice");
        sys.lock()
            .apply(&[
                super::super::rig::Action::Stop(Voice::WheelSlipLf),
                super::super::rig::Action::Stop(Voice::SuspensionLf),
                super::super::rig::Action::Stop(Voice::Transmission),
            ])
            .unwrap();
        s.tuning.native_suspension = Some(Box::new(native_suspension_settings()));
        sys.update_settings(s);
        sys.frame(&frame, ON).unwrap();
        assert!(sys.lock().is_playing(Voice::WheelSlipLf), "failed frame did not remember new slip file");
        assert!(sys.lock().is_playing(Voice::SuspensionLf), "failed frame did not remember earlier corner file");
        assert!(sys.lock().is_playing(Voice::Transmission), "failed frame did not remember gear");
    }

    #[test]
    fn native_road_bump_error_is_late_and_engine_error_restores_file_history() {
        let mut s = settings();
        s.tuning.native_suspension = Some(Box::new(native_suspension_settings()));
        let mut sys = ShakerSystem::new(s.clone(), SampleBank::synthesized(), 48_000);
        let mut frame = Telemetry::off();
        frame.rpm = 4000.0;
        frame.speed_kmh = 50.0;
        frame.wheel_slip_lf = 12.0;
        frame.suspension_velocity_lf = 0.2;
        frame.vertical_acceleration = 4.5;
        sys.frame(&frame, ON).unwrap();
        s.tuning.native_suspension.as_mut().unwrap().min_bump_velocity =
            Err(DspError { class: "ValueError", message: "bad bump".into() });
        sys.update_settings(s.clone());
        frame.wheel_slip_lf = 6.0;
        frame.vertical_acceleration = 7.0;
        assert!(matches!(sys.frame(&frame, ON), Err(EngineError::SuspensionParameter { .. })));
        assert!(sys.lock().is_playing(Voice::RoadBump), "later bump calculation leaves prior voice held");
        sys.lock().apply(&[super::super::rig::Action::Stop(Voice::WheelSlipLf)]).unwrap();
        s.tuning.native_suspension = Some(Box::new(native_suspension_settings()));
        s.tuning.native_engine_selector = Some(Err(DspError { class: "KeyError", message: "'selector'".into() }));
        sys.update_settings(s.clone());
        assert!(matches!(sys.frame(&frame, ON), Err(EngineError::EngineSelector { .. })));
        assert!(sys.lock().is_playing(Voice::WheelSlipLf), "bump error did not commit earlier slip file");
        sys.lock().apply(&[super::super::rig::Action::Stop(Voice::RoadBump)]).unwrap();
        s.tuning.native_engine_selector = None;
        sys.update_settings(s);
        sys.frame(&frame, ON).unwrap();
        assert!(sys.lock().is_playing(Voice::RoadBump), "late engine error did not commit new road-bump file");
    }

    #[test]
    fn native_suspension_failures_obey_frame_and_earlier_error_gates() {
        let mut s = settings();
        let mut native = native_suspension_settings();
        native.side_enhancement = Err(DspError { class: "KeyError", message: "'side'".into() });
        native.min_suspension_velocity = Err(DspError { class: "KeyError", message: "'velocity'".into() });
        native.min_bump_velocity = Err(DspError { class: "KeyError", message: "'bump'".into() });
        s.tuning.native_suspension = Some(Box::new(native));
        let mut sys = ShakerSystem::new(s.clone(), SampleBank::synthesized(), 48_000);
        let mut input = Telemetry::off();
        input.harmonized_gear = 1.0;
        input.wheel_slip_lf = 12.0;
        input.suspension_travel_rf = 0.02;
        for frame in [
            FrameState { output_ready: false, game_running: true, switch_on: true },
            FrameState { output_ready: true, game_running: false, switch_on: true },
            FrameState { output_ready: true, game_running: true, switch_on: false },
        ] {
            sys.frame(&input, frame).unwrap();
        }
        sys.frame(&input, ON).unwrap(); // parked suspension and zero shock speed skip the fields
        input.rpm = 4000.0;
        input.suspension_travel_rf = 0.03;
        let error = sys.frame(&input, ON).unwrap_err();
        assert_eq!(error.native_error().unwrap().message, "'side'");
        assert!(sys.lock().is_playing(Voice::WheelSlipLf), "the preceding wheel-slip section is complete");
        assert!(!sys.lock().is_playing(Voice::SuspensionRf), "later suspension corner was not computed");
        s.tuning.native_traction_max = Some(Err(DspError { class: "KeyError", message: "'traction'".into() }));
        sys.update_settings(s);
        assert_eq!(sys.frame(&input, ON).unwrap_err().native_error().unwrap().message, "'traction'");
    }

    #[test]
    fn native_traction_failure_retains_held_later_voices_and_repeats_gear_on_recovery() {
        use crate::shakers::rig::Voice;
        use openrigcue_model::native_value::LegacyValue;
        let mut settings = settings();
        settings.tuning.native_traction_max = Some(Ok(LegacyValue::Int(2)));
        let mut system = ShakerSystem::new(settings.clone(), SampleBank::synthesized(), 48_000);
        let active = FrameState { output_ready: true, game_running: true, switch_on: true };
        let mut input = Telemetry::off();
        input.harmonized_gear = 1.0;
        input.wheel_slip_lf = 12.0;
        system.frame(&input, active).unwrap();
        assert!(system.lock().is_playing(Voice::WheelSlipLf));
        assert!(system.lock().engine_playing());
        settings.tuning.native_traction_max =
            Some(Err(DspError { class: "KeyError", message: "'traction_loss'".into() }));
        system.update_settings(settings.clone());
        input.harmonized_gear = 2.0;
        input.wheel_slip_lf = 0.0;
        input.rpm = 4000.0;
        let error = system.frame(&input, active).unwrap_err();
        assert!(matches!(error, EngineError::TractionParameter { .. }));
        assert!(system.lock().is_playing(Voice::WheelSlipLf), "later stop must not be computed/applied");
        assert!(system.lock().engine_playing(), "late engine block must remain untouched");
        system.lock().apply(&[crate::shakers::rig::Action::Stop(Voice::Transmission)]).unwrap();
        settings.tuning.native_traction_max = Some(Ok(LegacyValue::Int(2)));
        system.update_settings(settings);
        system.frame(&input, active).unwrap();
        assert!(system.lock().is_playing(Voice::Transmission), "failed frame did not commit previous gear");
        assert!(!system.lock().is_playing(Voice::WheelSlipLf));
    }

    #[test]
    fn native_traction_failure_is_not_reached_when_inactive_or_early_dsp_fails() {
        use openrigcue_model::native_value::LegacyValue;
        let mut settings = settings();
        settings.tuning.native_traction_max =
            Some(Err(DspError { class: "KeyError", message: "'traction_loss'".into() }));
        let mut system = ShakerSystem::new(settings.clone(), SampleBank::synthesized(), 48_000);
        for state in [
            FrameState { output_ready: false, game_running: true, switch_on: true },
            FrameState { output_ready: true, game_running: false, switch_on: true },
            FrameState { output_ready: true, game_running: true, switch_on: false },
        ] {
            system.frame(&Telemetry::off(), state).unwrap();
        }
        let mut dsp = [const {
            crate::shakers::tuning::NativeDspSettings { filter_type: Ok(0), cutoff_hz: Ok(120.0), q: Ok(1.0) }
        }; crate::shakers::layout::GROUP_COUNT];
        dsp[0].filter_type = Err(DspError { class: "KeyError", message: "'early-type'".into() });
        settings.tuning.native_dsp = Some(Box::new(dsp));
        settings.tuning.native_traction_max = Some(Ok(LegacyValue::None));
        system.update_settings(settings);
        let error = system
            .frame(&Telemetry::off(), FrameState { output_ready: true, game_running: true, switch_on: true })
            .unwrap_err();
        assert!(matches!(error, EngineError::DspParameter { .. }));
        assert_eq!(error.native_error().unwrap().message, "'early-type'");
    }
    use crate::shakers::mixer::CHANNELS;

    fn settings() -> ShakerSettings {
        ShakerSettings {
            config: ShakeboxConfig::default(),
            profile: Some(SpeakerProfile::built_in("6 Speakers - 5.1").unwrap()),
            tuning: ShakerTuning::default(),
            max_traction_loss: 2.0,
        }
    }

    const ON: FrameState = FrameState { output_ready: true, game_running: true, switch_on: true };

    #[test]
    fn dsp_nonfinite_parameters_raise_even_when_the_filter_is_disabled() {
        for low_pass in [true, false] {
            for cutoff in [true, false] {
                for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, f64::MAX, -f64::MAX] {
                    let mut s = settings();
                    let effect = s.tuning.effect_mut(Effect::Engine);
                    effect.low_pass = low_pass;
                    if cutoff {
                        effect.cutoff_hz = value;
                    } else {
                        effect.resonance = value;
                    }
                    let mut sys = ShakerSystem::new(s, SampleBank::synthesized(), 48_000);
                    let mut telemetry = Telemetry::off();
                    telemetry.harmonized_gear = 1.0;
                    let error = sys.frame(&telemetry, ON).unwrap_err();
                    assert_eq!(error.to_string(), "INVALID FLOAT");
                    assert!(!sys.mixer().lock().unwrap().is_playing(Voice::Transmission));
                }
            }
        }
    }

    #[test]
    fn dsp_failure_retains_held_voices_and_rig_memory_until_recovery() {
        let mut sys = ShakerSystem::new(settings(), SampleBank::synthesized(), 48_000);
        let mut telemetry = slipping();
        telemetry.harmonized_gear = 1.0;
        sys.frame(&telemetry, ON).unwrap();
        let mut samples = vec![0.0; 48_000 * CHANNELS];
        sys.mixer().lock().unwrap().render(&mut samples);
        assert!(!sys.mixer().lock().unwrap().is_playing(Voice::Transmission));
        assert!(sys.mixer().lock().unwrap().is_playing(Voice::WheelSlipLf));

        let mut bad = sys.settings().clone();
        bad.tuning.effect_mut(Effect::Engine).resonance = f64::INFINITY;
        sys.update_settings(bad);
        telemetry.harmonized_gear = 2.0;
        telemetry.wheel_slip_lf = 0.0;
        assert_eq!(sys.frame(&telemetry, ON).unwrap_err().to_string(), "INVALID FLOAT");
        assert!(!sys.mixer().lock().unwrap().is_playing(Voice::Transmission));
        assert!(sys.mixer().lock().unwrap().is_playing(Voice::WheelSlipLf));

        sys.update_settings(settings());
        sys.frame(&telemetry, ON).unwrap();
        assert!(sys.mixer().lock().unwrap().is_playing(Voice::Transmission), "failed DSP did not remember gear");
        assert!(!sys.mixer().lock().unwrap().is_playing(Voice::WheelSlipLf), "held state survived until recovery");
    }

    #[test]
    fn native_dsp_failure_preserves_held_voices_and_all_early_rig_memory() {
        use super::super::tuning::{DspError, NativeDspSettings};
        for stage in 0..3 {
            let mut sys = ShakerSystem::new(settings(), SampleBank::synthesized(), 48_000);
            let mut telemetry = slipping();
            telemetry.harmonized_gear = 1.0;
            sys.frame(&telemetry, ON).unwrap();
            sys.mixer().lock().unwrap().render(&mut vec![0.0; 48_000 * CHANNELS]);
            let mut bad = sys.settings().clone();
            let mut native = super::super::layout::Group::ALL.map(|_| NativeDspSettings {
                filter_type: Ok(1),
                cutoff_hz: Ok(120.0),
                q: Ok(3.0),
            });
            let gear = &mut native[super::super::layout::Group::Gear.index()];
            let error = DspError { class: "KeyError", message: "'gear'".to_owned() };
            match stage {
                0 => gear.filter_type = Err(error),
                1 => gear.cutoff_hz = Err(error),
                _ => gear.q = Err(error),
            }
            bad.tuning.native_dsp = Some(Box::new(native));
            sys.update_settings(bad);
            telemetry.harmonized_gear = 2.0;
            telemetry.wheel_slip_lf = 0.0;
            assert_eq!(sys.frame(&telemetry, ON).unwrap_err().to_string(), "'gear'");
            assert!(!sys.mixer().lock().unwrap().is_playing(Voice::Transmission));
            assert!(sys.mixer().lock().unwrap().is_playing(Voice::WheelSlipLf));
            for state in [
                FrameState { output_ready: false, ..ON },
                FrameState { game_running: false, ..ON },
                FrameState { switch_on: false, ..ON },
            ] {
                // Disabled/idle paths do not attempt the invalid raw DSP reads.
                let mut inactive = ShakerSystem::new(sys.settings().clone(), SampleBank::synthesized(), 48_000);
                inactive.frame(&telemetry, state).unwrap();
            }
            sys.update_settings(settings());
            sys.frame(&telemetry, ON).unwrap();
            assert!(
                sys.mixer().lock().unwrap().is_playing(Voice::Transmission),
                "failed native read did not remember gear"
            );
            assert!(!sys.mixer().lock().unwrap().is_playing(Voice::WheelSlipLf));
        }
    }

    #[test]
    fn dsp_nonfinite_tuning_is_not_read_in_disabled_or_idle_frames() {
        let mut s = settings();
        let effect = s.tuning.effect_mut(Effect::Engine);
        effect.cutoff_hz = f64::NAN;
        effect.resonance = f64::INFINITY;
        let mut sys = ShakerSystem::new(s, SampleBank::synthesized(), 48_000);
        for state in [
            FrameState { output_ready: false, ..ON },
            FrameState { game_running: false, ..ON },
            FrameState { switch_on: false, ..ON },
        ] {
            sys.frame(&Telemetry::off(), state).unwrap();
        }
    }

    #[test]
    fn telemetry_frame_reaches_mixer() {
        let mut sys = ShakerSystem::new(settings(), SampleBank::synthesized(), 48_000);
        let mut t = Telemetry::off();
        t.harmonized_gear = 1.0;
        t.wheel_slip_lf = 12.0;
        t.rpm = 3000.0;
        t.max_rpm = 8000.0;
        sys.frame(&t, ON).unwrap();
        let mixer = sys.mixer();
        let m = mixer.lock().unwrap();
        assert!(m.is_playing(Voice::WheelSlipLf));
        assert!(m.is_playing(Voice::Transmission));
        assert!(m.engine_playing());
    }

    /// A `shaker_engine_type` other than 'O'/'E' fails in the engine block: the
    /// frame's effects before it played, the previous-value updates after it did not, so the same
    /// gear thumps again on the next frame.
    #[test]
    fn engine_key_error_ends_the_frame_before_the_previous_updates() {
        let mut bad = settings();
        bad.tuning.engine_type = None;
        let mut sys = ShakerSystem::new(bad, SampleBank::synthesized(), 48_000);
        let mut t = Telemetry::off();
        t.harmonized_gear = 1.0;
        assert!(matches!(sys.frame(&t, ON), Err(EngineError::NoInstance)));
        let mixer = sys.mixer();
        assert!(mixer.lock().unwrap().is_playing(Voice::Transmission), "the gear block ran");
        let mut buf = vec![0.0; 4_800 * CHANNELS];
        mixer.lock().unwrap().render(&mut buf);
        assert!(!mixer.lock().unwrap().is_playing(Voice::Transmission));
        sys.update_settings(settings());
        sys.frame(&t, ON).unwrap();
        assert!(mixer.lock().unwrap().is_playing(Voice::Transmission), "previous_gear was not updated");
        mixer.lock().unwrap().render(&mut buf);
        sys.frame(&t, ON).unwrap();
        assert!(!mixer.lock().unwrap().is_playing(Voice::Transmission), "same gear: no thump");
    }

    #[test]
    fn native_engine_selector_late_error_keeps_earlier_held_changes_and_retries_previous_gear() {
        use super::super::tuning::DspError;
        use openrigcue_model::native_value::LegacyValue;
        for missing in [true, false] {
            let mut sys = ShakerSystem::new(settings(), SampleBank::synthesized(), 48_000);
            let mut telemetry = slipping();
            telemetry.harmonized_gear = 1.0;
            sys.frame(&telemetry, ON).unwrap();
            sys.mixer().lock().unwrap().render(&mut vec![0.0; 48_000 * CHANNELS]);
            assert!(sys.mixer().lock().unwrap().is_playing(Voice::WheelSlipLf));
            let mut bad = sys.settings().clone();
            bad.tuning.native_engine_selector = Some(if missing {
                Err(DspError { class: "KeyError", message: "'shaker_engine_type'".into() })
            } else {
                Ok(LegacyValue::from("X"))
            });
            sys.update_settings(bad);
            telemetry.harmonized_gear = 2.0;
            telemetry.wheel_slip_lf = 0.0;
            assert!(matches!(sys.frame(&telemetry, ON), Err(EngineError::EngineSelector { .. })));
            assert!(
                sys.mixer().lock().unwrap().is_playing(Voice::Transmission),
                "the earlier gear action stayed applied"
            );
            assert!(
                !sys.mixer().lock().unwrap().is_playing(Voice::WheelSlipLf),
                "the earlier held-loop stop stayed applied"
            );
            sys.mixer().lock().unwrap().render(&mut vec![0.0; 4_800 * CHANNELS]);
            assert!(!sys.mixer().lock().unwrap().is_playing(Voice::Transmission));
            sys.update_settings(settings());
            sys.frame(&telemetry, ON).unwrap();
            assert!(
                sys.mixer().lock().unwrap().is_playing(Voice::Transmission),
                "the failed late block did not remember gear"
            );
            assert!(!sys.mixer().lock().unwrap().is_playing(Voice::WheelSlipLf));
        }
    }

    #[test]
    fn native_engine_caught_fade_advances_gear_but_failed_print_keeps_the_late_prefix() {
        use crate::shakers::engine_sound::capture_engine_output;
        use crate::shakers::tuning::{DspError, NativeEngineSettings};
        use openrigcue_model::native_value::LegacyValue;
        for print_fails in [false, true] {
            let mut sys = ShakerSystem::new(settings(), SampleBank::synthesized(), 48_000);
            let mut telemetry = slipping();
            telemetry.harmonized_gear = 1.0;
            sys.frame(&telemetry, ON).unwrap();
            sys.mixer().lock().unwrap().render(&mut vec![0.0; 48_000 * CHANNELS]);
            let mut bad = sys.settings().clone();
            bad.tuning.native_engine = Some(Box::new(NativeEngineSettings {
                flag: Ok(LegacyValue::Int(1)),
                fade_start: Err(DspError { class: "KeyError", message: "'engine_fadeoff_start'".into() }),
                fade_end: Ok(LegacyValue::Int(100)),
                resonance: Ok(8.0),
            }));
            sys.update_settings(bad);
            telemetry.harmonized_gear = 2.0;
            telemetry.wheel_slip_lf = 0.0;
            let (result, lines) = capture_engine_output(print_fails, || sys.frame(&telemetry, ON));
            if print_fails {
                assert!(matches!(result, Err(EngineError::DiagnosticOutput(_))));
            } else {
                result.unwrap();
            }
            assert_eq!(lines, ["error mixed level outputs not loaded yet 'engine_fadeoff_start'\n"]);
            assert!(sys.mixer().lock().unwrap().is_playing(Voice::Transmission));
            assert!(!sys.mixer().lock().unwrap().is_playing(Voice::WheelSlipLf), "earlier held-loop stop persists");
            sys.mixer().lock().unwrap().render(&mut vec![0.0; 4_800 * CHANNELS]);
            sys.update_settings(settings());
            sys.frame(&telemetry, ON).unwrap();
            assert_eq!(
                sys.mixer().lock().unwrap().is_playing(Voice::Transmission),
                print_fails,
                "caught fade remembers gear; a failed print retains previous-only rollback"
            );
        }
    }

    #[test]
    fn native_rebuild_missing_engine_mix_is_caught_before_the_reached_pitch_error() {
        use crate::shakers::engine_sound::capture_engine_output;
        use crate::shakers::tuning::NativeEngineSettings;
        use openrigcue_model::native_value::LegacyValue;
        let mut sys = ShakerSystem::new(settings(), SampleBank::synthesized(), 48_000);
        let mut telemetry = slipping();
        telemetry.harmonized_gear = 1.0;
        sys.frame(&telemetry, ON).unwrap();
        sys.mixer().lock().unwrap().render(&mut vec![0.0; 48_000 * CHANNELS]);
        let mut bad = sys.settings().clone();
        let missing = DspError { class: "KeyError", message: "'engine'".into() };
        bad.tuning.native_group_bases = Some(Box::new(super::super::layout::Group::ALL.map(|_| Err(missing.clone()))));
        let pitch = DspError {
            class: "TypeError",
            message: "float() argument must be a string or a real number, not 'NoneType'".into(),
        };
        bad.tuning.native_engine = Some(Box::new(NativeEngineSettings {
            flag: Ok(LegacyValue::Int(1)),
            fade_start: Err(DspError { class: "KeyError", message: "'engine_fadeoff_start'".into() }),
            fade_end: Ok(LegacyValue::Int(100)),
            resonance: Err(pitch.clone()),
        }));
        sys.update_settings(bad);
        assert_eq!(sys.reload_levels().unwrap_err(), missing);
        telemetry.harmonized_gear = 2.0;
        telemetry.wheel_slip_lf = 0.0;
        let (result, lines) = capture_engine_output(false, || sys.frame(&telemetry, ON));
        assert_eq!(lines, ["error mixed level outputs not loaded yet 'engine'\n"]);
        assert!(matches!(result, Err(EngineError::EngineParameter { error }) if error == pitch));
        assert!(sys.mixer().lock().unwrap().is_playing(Voice::Transmission));
        assert!(!sys.mixer().lock().unwrap().is_playing(Voice::WheelSlipLf));
        sys.mixer().lock().unwrap().render(&mut vec![0.0; 4_800 * CHANNELS]);
        sys.update_settings(settings());
        sys.reload_levels().unwrap();
        sys.frame(&telemetry, ON).unwrap();
        assert!(
            sys.mixer().lock().unwrap().is_playing(Voice::Transmission),
            "late pitch failure did not remember gear"
        );
    }

    #[test]
    fn idle_stops_everything() {
        let mut sys = ShakerSystem::new(settings(), SampleBank::synthesized(), 48_000);
        let mut t = Telemetry::off();
        t.wheel_slip_lf = 12.0;
        sys.frame(&t, ON).unwrap();
        sys.frame(&t, FrameState { game_running: false, ..ON }).unwrap();
        let mixer = sys.mixer();
        let m = mixer.lock().unwrap();
        assert!(!m.is_playing(Voice::WheelSlipLf));
        assert!(!m.engine_playing());
    }

    #[test]
    fn effect_test_focuses_levels_and_restores() {
        let mut sys = ShakerSystem::new(settings(), SampleBank::synthesized(), 48_000);
        sys.begin_effect_test(Effect::WheelSlip).unwrap();
        for f in effect_test_frames() {
            sys.test_frame(&f, FrameState { game_running: false, ..ON }).unwrap();
        }
        // Last frame runs with no game: everything stopped again.
        assert!(!sys.mixer().lock().unwrap().is_playing(Voice::WheelSlipLf));
        sys.end_effect_test().unwrap();
        sys.test_speaker(Speaker::Center).unwrap();
        let mixer = sys.mixer();
        let mut m = mixer.lock().unwrap();
        let mut buf = vec![0.0; 200 * CHANNELS];
        m.render(&mut buf);
        assert!(buf.chunks(6).any(|f| f[2] != 0.0));
        assert!(buf.chunks(6).all(|f| f[0] == 0.0));
    }

    const NO_GAME: FrameState = FrameState { game_running: false, ..ON };

    fn slipping() -> Telemetry {
        let mut t = Telemetry::off();
        t.wheel_slip_lf = 12.0;
        t
    }

    fn front_left_energy(sys: &ShakerSystem, frames: usize) -> f64 {
        let mixer = sys.mixer();
        let mut m = mixer.lock().unwrap();
        let mut buf = vec![0.0; frames * CHANNELS];
        m.render(&mut buf);
        buf.chunks(CHANNELS).map(|f| f64::from(f[Speaker::FrontLeft.index()]).powi(2)).sum()
    }

    #[test]
    fn shake_rig_dsp_block_retunes_held_loop_without_rebuild() {
        // A cutoff change only updates the settings; the next frame's DSP block sets the
        // new cutoff on the current groups, which the held wheel-slip loop plays through.
        let mut s = settings();
        let slip = s.tuning.effect_mut(Effect::WheelSlip);
        slip.low_pass = true;
        slip.cutoff_hz = 500.0;
        slip.resonance = 0.707;
        let mut sys = ShakerSystem::new(s, SampleBank::synthesized(), 48_000);
        sys.frame(&slipping(), ON).unwrap();
        front_left_energy(&sys, 5_000);
        let open = front_left_energy(&sys, 5_000);
        let before = sys.levels.clone();

        let mut s = sys.settings().clone();
        s.tuning.effect_mut(Effect::WheelSlip).cutoff_hz = 20.0;
        sys.update_settings(s);
        sys.frame(&slipping(), ON).unwrap();
        assert!(sys.mixer().lock().unwrap().is_playing(Voice::WheelSlipLf));
        front_left_energy(&sys, 5_000);
        let closed = front_left_energy(&sys, 5_000);
        assert!(open > 1.0);
        assert!(closed < open * 0.05, "{closed} vs {open}");
        assert_eq!(sys.levels, before);
    }

    #[test]
    fn trigger_events_profile_waits_for_load_channel_groups() {
        // Selecting a speaker profile swaps the profile without a rebuild.
        let mut sys = ShakerSystem::new(settings(), SampleBank::synthesized(), 48_000);
        let five_one = sys.levels.clone();
        let mut s = sys.settings().clone();
        let center = SpeakerProfile::built_in("1 Speaker - Center").unwrap();
        s.profile = Some(center.clone());
        sys.update_settings(s.clone());
        assert_eq!(sys.levels, five_one);
        sys.reload_levels().unwrap();
        assert_eq!(sys.levels, GroupLevels::compute(&center, &s.tuning, None));
    }

    #[test]
    fn get_shaker_effects_key_error_keeps_the_layout() {
        // A profile name that is not stored: the lookup fails and
        // the previous layout stays.
        let mut sys = ShakerSystem::new(settings(), SampleBank::synthesized(), 48_000);
        let before = sys.profile().clone();
        let mut s = sys.settings().clone();
        s.profile = None;
        sys.update_settings(s);
        assert_eq!(sys.profile(), &before);
        sys.reload_levels().unwrap();
        assert_eq!(sys.levels, GroupLevels::compute(&before, &sys.settings().tuning, None));
        // Before any layout was found: the empty profile gives no level anywhere.
        let empty =
            ShakerSystem::new(ShakerSettings { profile: None, ..settings() }, SampleBank::synthesized(), 48_000);
        assert_eq!(empty.profile(), &SpeakerProfile::empty());
        assert_eq!(empty.levels, GroupLevels::compute(&SpeakerProfile::empty(), &ShakerTuning::default(), None));
    }

    #[test]
    fn on_confirm_adv_shakers_rebuilds_groups() {
        let mut sys = ShakerSystem::new(settings(), SampleBank::synthesized(), 48_000);
        let rr = Speaker::RearRight;
        let mut column: PerGroup<f64> = [0.0; crate::shakers::layout::GROUP_COUNT];
        column[crate::shakers::Group::BrakeLf.index()] = 4.0;
        column[crate::shakers::Group::BrakeRf.index()] = 15.0;
        sys.edit_custom_matrix(rr, &column).unwrap();
        // brake = max(4, 15) = 15 -> 15/10 * 1.0 * 1.0.
        assert_eq!(sys.levels.of(crate::shakers::Group::Brake)[rr.index()], 1.5);
        assert_eq!(sys.profile().level(crate::shakers::Group::Brake, rr), 15.0);
    }

    #[test]
    fn test_sfx_frames_follow_the_shared_listener() {
        // A frame reads the running-listener value when it runs: a test frame or a
        // telemetry frame is active while it holds a listener (the test one included) and idle
        // once the game-listener thread wrote 'none' over it. The system keeps no listener of
        // its own between frames.
        let mut sys = ShakerSystem::new(settings(), SampleBank::synthesized(), 48_000);
        sys.begin_effect_test(Effect::WheelSlip).unwrap();
        let frames = effect_test_frames();
        sys.test_frame(&frames[0], ON).unwrap();
        sys.frame(&slipping(), ON).unwrap();
        assert!(sys.mixer().lock().unwrap().is_playing(Voice::WheelSlipLf));
        sys.frame(&slipping(), NO_GAME).unwrap();
        assert!(!sys.mixer().lock().unwrap().is_playing(Voice::WheelSlipLf), "listener reset to 'none'");
        sys.test_frame(&frames[1], NO_GAME).unwrap();
        assert!(!sys.mixer().lock().unwrap().is_playing(Voice::WheelSlipLf));
        sys.end_effect_test().unwrap();
    }
}
