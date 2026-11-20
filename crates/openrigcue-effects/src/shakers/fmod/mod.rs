//! The engine vibration through FMOD Studio: the events
//! `event:/Vehicles/Car Engine 1` ('O', "Original") and `Car Engine 2` ('E', "Enhanced") of the
//! user's own `Master Bank.bank`, `Master Bank.strings.bank` and `Vehicles.bank`, played by the
//! user's own FMOD Engine 2.02 libraries, loaded at run time. OpenRigCue ships neither.
//!
//! [`FmodEngine`] drives the events; the parameters themselves (`rpm`
//! 0..10000, `load`, the six per-speaker levels after the engine fade, `pitch`) are computed by
//! [`super::rig`] as [`EngineCommand`]. FMOD mixes the events' 5.1 output without a device and
//! OpenRigCue's mixer adds it to its six speaker buses, so the device output and channel
//! placement stay those of [`super::mixer`]. When the libraries or the banks are missing the
//! caller keeps the built-in engine ([`super::engine_sound::EngineSound`]).

mod ffi;

use std::ffi::CStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use thiserror::Error;

pub use ffi::FmodLibrary;
use ffi::{InstanceId, Studio};

use super::engine_sound::{
    engine_levels, engine_pitch, engine_selector, engine_stdout, report_speaker_error, EngineError, EngineOutput,
};
use super::layout::PerSpeaker;
use super::rig::EngineCommand;
use super::samples::{SampleError, Source};
use super::tuning::EngineType;

#[derive(Debug, Error)]
pub enum FmodError {
    #[error("FMOD library not loaded: {0}")]
    Library(String),
    #[error("{call} failed with FMOD error {code}")]
    Call { call: &'static str, code: i32 },
    #[error("sample rate {0} is out of range")]
    Rate(u32),
    #[error("bank of {0} bytes is too large")]
    BankTooLarge(usize),
    #[error("engine bank {name}: {source}")]
    Bank { name: &'static str, source: SampleError },
}

impl FmodError {
    pub(super) fn native_error(&self) -> Option<super::tuning::DspError> {
        match self {
            Self::Call { code, .. } if (0..82).contains(code) => Some(super::tuning::DspError {
                class: "pyfmodex.exceptions.FmodError",
                message: caught_fmod_message(self),
            }),
            _ => None,
        }
    }
}

/// The bank files, in the order they are loaded (they live in `sounds/engine`).
pub const BANK_FILES: [&str; 3] = ["Master Bank.bank", "Master Bank.strings.bank", "Vehicles.bank"];
/// The event paths of the 'O' and 'E' engine instances.
pub const ENGINE_EVENTS: [&CStr; 2] = [c"event:/Vehicles/Car Engine 1", c"event:/Vehicles/Car Engine 2"];
/// The level parameters, in the order they are set (speakers FL, FR, C, LFE, RL, RR).
const SPEAKER_PARAMETERS: [&CStr; 6] = [c"fl", c"fr", c"ce", c"sub", c"sl", c"sr"];
/// Frames FMOD mixes per update (about 5 ms at 48 kHz).
const BLOCK_FRAMES: u32 = 256;

/// The FMOD result codes 0..81 by name; an error's text is the name with `_` replaced by a space.
/// Unknown result codes keep the numeric diagnostic.
pub(super) fn caught_fmod_message(error: &FmodError) -> String {
    const NAMES: [&str; 82] = [
        "OK",
        "BADCOMMAND",
        "CHANNEL_ALLOC",
        "CHANNEL_STOLEN",
        "DMA",
        "DSP_CONNECTION",
        "DSP_DONTPROCESS",
        "DSP_FORMAT",
        "DSP_INUSE",
        "DSP_NOTFOUND",
        "DSP_RESERVED",
        "DSP_SILENCE",
        "DSP_TYPE",
        "FILE_BAD",
        "FILE_COULDNOTSEEK",
        "FILE_DISKEJECTED",
        "FILE_EOF",
        "FILE_ENDOFDATA",
        "FILE_NOTFOUND",
        "FORMAT",
        "HEADER_MISMATCH",
        "HTTP",
        "HTTP_ACCESS",
        "HTTP_PROXY_AUTH",
        "HTTP_SERVER_ERROR",
        "HTTP_TIMEOUT",
        "INITIALIZATION",
        "INITIALIZED",
        "INTERNAL",
        "INVALID_FLOAT",
        "INVALID_HANDLE",
        "INVALID_PARAM",
        "INVALID_POSITION",
        "INVALID_SPEAKER",
        "INVALID_SYNCPOINT",
        "INVALID_THREAD",
        "INVALID_VECTOR",
        "MAXAUDIBLE",
        "MEMORY",
        "MEMORY_CANTPOINT",
        "NEEDS3D",
        "NEEDSHARDWARE",
        "NET_CONNECT",
        "NET_SOCKET_ERROR",
        "NET_URL",
        "NET_WOULD_BLOCK",
        "NOTREADY",
        "OUTPUT_ALLOCATED",
        "OUTPUT_CREATEBUFFER",
        "OUTPUT_DRIVERCALL",
        "OUTPUT_FORMAT",
        "OUTPUT_INIT",
        "OUTPUT_NODRIVERS",
        "PLUGIN",
        "PLUGIN_MISSING",
        "PLUGIN_RESOURCE",
        "PLUGIN_VERSION",
        "RECORD",
        "REVERB_CHANNELGROUP",
        "REVERB_INSTANCE",
        "SUBSOUNDS",
        "SUBSOUND_ALLOCATED",
        "SUBSOUND_CANTMOVE",
        "TAGNOTFOUND",
        "TOOMANYCHANNELS",
        "TRUNCATED",
        "UNIMPLEMENTED",
        "UNINITIALIZED",
        "UNSUPPORTED",
        "VERSION",
        "EVENT_ALREADY_LOADED",
        "EVENT_LIVEUPDATE_BUSY",
        "EVENT_LIVEUPDATE_MISMATCH",
        "EVENT_LIVEUPDATE_TIMEOUT",
        "EVENT_NOTFOUND",
        "STUDIO_UNINITIALIZED",
        "STUDIO_NOT_LOADED",
        "INVALID_STRING",
        "ALREADY_LOCKED",
        "NOT_LOCKED",
        "RECORD_DISCONNECTED",
        "TOOMANYSAMPLES",
    ];
    if let FmodError::Call { code, .. } = error {
        if let Some(name) = usize::try_from(*code).ok().and_then(|code| NAMES.get(code)) {
            return name.replace('_', " ");
        }
    }
    error.to_string()
}

/// The contents of the user's three SRS engine banks.
#[derive(Debug, Clone)]
pub struct EngineBanks {
    files: [Vec<u8>; 3],
}

impl EngineBanks {
    /// Read the banks from an SRS install directory (holding `sounds/engine/`), its `sounds`
    /// directory, or the SRS installer, like [`super::samples::SampleBank::import`].
    pub fn read(srs: &Path) -> Result<EngineBanks, FmodError> {
        let source = Source::open(srs).map_err(|source| FmodError::Bank { name: BANK_FILES[0], source })?;
        let read =
            |name: &'static str| source.read_file(&["engine", name]).map_err(|source| FmodError::Bank { name, source });
        Ok(EngineBanks { files: [read(BANK_FILES[0])?, read(BANK_FILES[1])?, read(BANK_FILES[2])?] })
    }

    #[cfg(test)]
    fn from_files(files: [Vec<u8>; 3]) -> EngineBanks {
        EngineBanks { files }
    }
}

/// Where the FMOD libraries are looked for, in order; the dynamic linker's own search path
/// comes last ([`FmodEngine::from_srs`]):
/// `$OPENRIGCUE_FMOD_DIR`, `$APPDIR/usr/lib/openrigcue/fmod` (inside the AppImage),
/// `<executable dir>/../lib/openrigcue/fmod`, `$XDG_DATA_HOME/openrigcue/fmod`
/// (default `~/.local/share/openrigcue/fmod`).
pub fn library_dirs() -> Vec<PathBuf> {
    let env = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    let mut dirs = Vec::new();
    dirs.extend(env("OPENRIGCUE_FMOD_DIR"));
    dirs.extend(env("APPDIR").map(|d| d.join("usr/lib/openrigcue/fmod")));
    if let Some(exe_dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        dirs.push(exe_dir.join("../lib/openrigcue/fmod"));
    }
    let data = env("XDG_DATA_HOME").or_else(|| env("HOME").map(|h| h.join(".local/share")));
    dirs.extend(data.map(|d| d.join("openrigcue/fmod")));
    dirs
}

/// Load the FMOD libraries from the first of `dirs` that has them, else from the system path.
pub fn load_library(dirs: &[PathBuf]) -> Result<Arc<FmodLibrary>, FmodError> {
    let mut errors = Vec::new();
    for dir in dirs.iter().filter(|d| d.is_dir()) {
        match FmodLibrary::load(Some(dir)) {
            Ok(lib) => return Ok(lib),
            Err(e) => errors.push(e.to_string()),
        }
    }
    FmodLibrary::load(None).map_err(|e| {
        errors.push(e.to_string());
        FmodError::Library(errors.join("; "))
    })
}

/// The two engine instances of one FMOD Studio system.
pub struct FmodEngine {
    lib: Arc<FmodLibrary>,
    banks: Arc<EngineBanks>,
    rate: u32,
    studio: Studio,
    /// The 'O' and 'E' engine instances.
    instances: [InstanceId; 2],
    /// Last paused state set per instance.
    paused: [bool; 2],
    /// Commands buffered in Studio since the last successful Studio update. They are
    /// submitted only at a frame's update, so the
    /// mixing path must not run them earlier.
    unsubmitted: bool,
    /// A failing Studio update of the mixing path is logged as a warning once, then at
    /// debug level.
    warned: bool,
}

impl std::fmt::Debug for FmodEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FmodEngine").field("rate", &self.rate).field("paused", &self.paused).finish_non_exhaustive()
    }
}

fn index(engine_type: EngineType) -> usize {
    match engine_type {
        EngineType::Original => 0,
        EngineType::Enhanced => 1,
    }
}

pub(super) struct FmodSource {
    lib: Arc<FmodLibrary>,
    banks: Arc<EngineBanks>,
}
impl FmodSource {
    pub(super) fn open(&self, rate: u32) -> Result<FmodEngine, FmodError> {
        FmodEngine::open(Arc::clone(&self.lib), Arc::clone(&self.banks), rate)
    }
}

impl FmodEngine {
    pub(super) fn source(&self) -> FmodSource {
        FmodSource { lib: Arc::clone(&self.lib), banks: Arc::clone(&self.banks) }
    }

    /// Libraries from [`library_dirs`] and banks from the user's SRS installation or installer `srs`.
    pub fn from_srs(srs: &Path, rate: u32) -> Result<FmodEngine, FmodError> {
        let banks = Arc::new(EngineBanks::read(srs)?);
        let lib = load_library(&library_dirs())?;
        FmodEngine::open(lib, banks, rate)
    }

    /// Set-up: a Studio system, the three banks,
    /// an instance created and started for 'O' and 'E', then both un-paused
    /// and a Studio update.
    pub fn open(lib: Arc<FmodLibrary>, banks: Arc<EngineBanks>, rate: u32) -> Result<FmodEngine, FmodError> {
        let mut studio = Studio::create(Arc::clone(&lib), rate, BLOCK_FRAMES)?;
        for data in &banks.files {
            studio.load_bank(data)?;
        }
        let original = studio.event_instance(ENGINE_EVENTS[0])?;
        let enhanced = studio.event_instance(ENGINE_EVENTS[1])?;
        studio.start(original)?;
        studio.start(enhanced)?;
        studio.set_paused(original, false)?;
        studio.set_paused(enhanced, false)?;
        studio.update()?;
        Ok(FmodEngine {
            lib,
            banks,
            rate,
            studio,
            instances: [original, enhanced],
            paused: [false, false],
            unsubmitted: false,
            warned: false,
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.rate
    }

    /// Mix at `rate` from now on. FMOD's software format is fixed at initialisation, so the
    /// system is built again.
    pub fn set_sample_rate(&mut self, rate: u32) -> Result<(), FmodError> {
        if rate == self.rate {
            return Ok(());
        }
        *self = FmodEngine::open(Arc::clone(&self.lib), Arc::clone(&self.banks), rate)?;
        Ok(())
    }

    fn report(&mut self, result: Result<(), FmodError>) {
        if let Err(error) = result {
            if self.warned {
                tracing::debug!(%error, "FMOD engine call failed");
            } else {
                self.warned = true;
                tracing::warn!(%error, "FMOD engine call failed");
            }
        }
    }

    fn set_paused(&mut self, i: usize, paused: bool) -> Result<(), FmodError> {
        self.studio.set_paused(self.instances[i], paused)?;
        self.unsubmitted = true;
        self.paused[i] = paused;
        Ok(())
    }

    fn set_parameter(&mut self, selected: EngineType, name: &CStr, value: f64) -> Result<(), FmodError> {
        self.studio.set_parameter(self.instances[index(selected)], name, value as f32)?;
        self.unsubmitted = true;
        Ok(())
    }

    /// The engine block of a frame: pause the other type's
    /// instance, un-pause the selected one if it is paused, then set `rpm`, `load`, the six
    /// levels and `pitch`, in that order. Lookup, fade and setter errors of the speaker levels
    /// are handled: the accepted prefix is kept, the diagnostic printed, and pitch still follows.
    /// The block's Studio update is [`Self::submit`].
    /// Other errors end the block; a type other than 'O'/'E' pauses 'O'
    /// and fails with a key error.
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
                    if let Err(error) = self.speaker_live(selected, slot, value) {
                        report_speaker_error(&caught_fmod_message(&error), output)?;
                        break;
                    }
                }
            }
            Ok(None) => {}
            Err(error) => report_speaker_error(&error.message, output)?,
        }
        self.pitch_live(selected, engine_pitch(c)?)?;
        Ok(())
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

    pub(super) fn pause_instance_live(&mut self, selected: EngineType) -> Result<(), FmodError> {
        self.set_paused(index(selected), true)
    }
    pub(super) fn paused_live(&mut self, selected: EngineType) -> Result<bool, FmodError> {
        self.studio.paused(self.instances[index(selected)])
    }
    pub(super) fn resume_live(&mut self, selected: EngineType) -> Result<(), FmodError> {
        self.set_paused(index(selected), false)
    }
    pub(super) fn rpm_live(&mut self, selected: EngineType, value: f64) -> Result<(), FmodError> {
        self.set_parameter(selected, c"rpm", value)
    }
    pub(super) fn load_live(&mut self, selected: EngineType, value: f64) -> Result<(), FmodError> {
        self.set_parameter(selected, c"load", value)
    }

    pub(super) fn speaker_live(&mut self, selected: EngineType, slot: usize, value: f64) -> Result<(), FmodError> {
        self.set_parameter(selected, SPEAKER_PARAMETERS[slot], value)
    }

    pub(super) fn pitch_live(&mut self, selected: EngineType, value: f64) -> Result<(), FmodError> {
        self.set_parameter(selected, c"pitch", value)
    }

    /// The inactive branch of a frame: both instances paused; the
    /// first failing call is an error. Its Studio update is [`Self::submit`].
    pub fn pause(&mut self) -> Result<(), EngineError> {
        self.set_paused(0, true)?;
        self.set_paused(1, true)?;
        Ok(())
    }

    /// The Studio update of a frame (after the engine block, or
    /// after the inactive branch's pauses): the frame's buffered commands reach the events
    /// together. A failing update is an error here, before the frame's previous-value assignments,
    /// and leaves the commands buffered for the next update, as the Studio buffer does.
    ///
    /// With the synchronous non-real-time system this update also mixes one block; the
    /// capture keeps it in order for [`Self::render_into`], so no frame is lost or repeated.
    pub fn submit(&mut self) -> Result<(), FmodError> {
        self.studio.update()?;
        self.unsubmitted = false;
        Ok(())
    }

    /// Whether commands wait for the next [`Self::submit`].
    pub fn has_unsubmitted(&self) -> bool {
        self.unsubmitted
    }

    /// Whether an instance is un-paused.
    pub fn is_playing(&self) -> bool {
        self.paused.contains(&false)
    }

    /// Add the next frame of FMOD's 5.1 mix to `frame`. When the captured block is used up the
    /// next block is mixed: by a Studio update when every buffered command has been
    /// submitted (Studio's own processing, nothing new to run), and by the core system alone
    /// while a frame's commands wait for their submission, so that rendering
    /// goes on between two updates and never hears an unsubmitted command.
    pub fn render_into(&mut self, frame: &mut PerSpeaker) {
        let next = match self.studio.pop_frame() {
            Some(f) => Some(f),
            None => {
                let result = if self.unsubmitted { self.studio.mix_core_block() } else { self.studio.update() };
                let ok = result.is_ok();
                self.report(result);
                if ok {
                    self.studio.pop_frame()
                } else {
                    None
                }
            }
        };
        if let Some(mixed) = next {
            for (out, v) in frame.iter_mut().zip(mixed) {
                *out += f64::from(v);
            }
        }
    }

    /// Frames FMOD mixes per update.
    pub fn block_frames(&self) -> usize {
        self.studio.block()
    }
}

#[cfg(test)]
mod tests {
    use super::ffi::fake;
    use super::*;

    fn banks() -> Arc<EngineBanks> {
        Arc::new(EngineBanks::from_files([vec![1; 10], vec![2; 20], vec![3; 30]]))
    }

    fn engine() -> FmodEngine {
        let lib = fake::library();
        FmodEngine::open(lib, banks(), 48_000).unwrap()
    }

    fn cmd(engine_type: EngineType, rpm: f64, levels: Option<PerSpeaker>, pitch: f64) -> EngineCommand {
        EngineCommand {
            engine_type: Some(engine_type),
            native_engine_selector: None,
            native_parameters: None,
            rpm,
            load: 1.0,
            speaker_levels: levels,
            pitch,
        }
    }

    fn calls_after(mark: usize) -> Vec<String> {
        fake::calls()[mark..].to_vec()
    }

    /// Set-up: banks in order, both events created and started,
    /// both un-paused, one update.
    #[test]
    fn reinit_studio_and_set_soundcard_start_both_events() {
        let e = engine();
        let calls = fake::calls();
        let start = calls.iter().position(|c| c.starts_with("FMOD_Studio_System_LoadBankMemory")).unwrap();
        assert_eq!(
            calls[start..],
            [
                "FMOD_Studio_System_LoadBankMemory 10 mode 0",
                "FMOD_Studio_System_LoadBankMemory 20 mode 0",
                "FMOD_Studio_System_LoadBankMemory 30 mode 0",
                "FMOD_Studio_System_GetEvent event:/Vehicles/Car Engine 1",
                "FMOD_Studio_EventDescription_CreateInstance 0",
                "FMOD_Studio_System_GetEvent event:/Vehicles/Car Engine 2",
                "FMOD_Studio_EventDescription_CreateInstance 1",
                "FMOD_Studio_EventInstance_Start 0",
                "FMOD_Studio_EventInstance_Start 1",
                "FMOD_Studio_EventInstance_SetPaused 0 0",
                "FMOD_Studio_EventInstance_SetPaused 1 0",
                "FMOD_Studio_System_Update",
            ]
        );
        assert!(e.is_playing());
        assert_eq!(fake::started(), [0, 1]);
    }

    /// Engine block for 'E': pause 'O', the paused state read on 'E' (not paused: no
    /// set), then rpm, load, fl..sr, pitch, none of them ignoring the seek speed.
    #[test]
    fn shake_rig_engine_block_parameter_order() {
        let mut e = engine();
        let mark = fake::calls().len();
        e.command(&cmd(EngineType::Enhanced, 4321.5, Some([0.1, 0.2, 0.3, 0.4, 0.5, 0.6]), 35.0)).unwrap();
        assert_eq!(
            calls_after(mark),
            [
                "FMOD_Studio_EventInstance_SetPaused 0 1",
                "FMOD_Studio_EventInstance_GetPaused 1",
                "FMOD_Studio_EventInstance_SetParameterByName 1 rpm=4321.5 0",
                "FMOD_Studio_EventInstance_SetParameterByName 1 load=1 0",
                "FMOD_Studio_EventInstance_SetParameterByName 1 fl=0.1 0",
                "FMOD_Studio_EventInstance_SetParameterByName 1 fr=0.2 0",
                "FMOD_Studio_EventInstance_SetParameterByName 1 ce=0.3 0",
                "FMOD_Studio_EventInstance_SetParameterByName 1 sub=0.4 0",
                "FMOD_Studio_EventInstance_SetParameterByName 1 sl=0.5 0",
                "FMOD_Studio_EventInstance_SetParameterByName 1 sr=0.6 0",
                "FMOD_Studio_EventInstance_SetParameterByName 1 pitch=35 0",
            ]
        );
        // Buffered until the block's Studio update (`submit`).
        assert!(!fake::is_paused(0));
        assert_eq!(fake::param(1, "rpm"), None);
        assert_eq!(fake::queued().len(), 10);
        assert!(e.has_unsubmitted());
        e.submit().unwrap();
        assert!(!e.has_unsubmitted());
        assert!(fake::queued().is_empty());
        assert!(fake::is_paused(0));
        assert!(!fake::is_paused(1));
        assert_eq!(fake::param(1, "rpm"), Some(4321.5));
    }

    /// A paused instance is un-paused, after the inactive branch
    /// paused both.
    #[test]
    fn paused_instance_is_resumed_by_the_engine_block() {
        let mut e = engine();
        e.pause().unwrap();
        assert!(!e.is_playing());
        e.submit().unwrap();
        assert!(fake::is_paused(0) && fake::is_paused(1));
        let mark = fake::calls().len();
        e.command(&cmd(EngineType::Original, 0.0, None, 60.0)).unwrap();
        e.submit().unwrap();
        assert_eq!(
            calls_after(mark)[..3],
            [
                "FMOD_Studio_EventInstance_SetPaused 1 1",
                "FMOD_Studio_EventInstance_GetPaused 0",
                "FMOD_Studio_EventInstance_SetPaused 0 0",
            ]
        );
        assert!(e.is_playing());
        // No levels (their computation failed): only rpm, load and pitch.
        let params: Vec<String> = calls_after(mark).into_iter().filter(|c| c.contains("SetParameterByName")).collect();
        assert_eq!(params.len(), 3);
        assert_eq!(fake::param(0, "pitch"), Some(60.0));
    }

    /// The per-speaker mix reaches the six buses: the fake mixes the `fl..sr` parameters of
    /// the playing instance into its channels, which `render_into` adds to the frame.
    #[test]
    fn fmod_mix_reaches_the_speaker_buses() {
        let mut e = engine();
        e.command(&cmd(EngineType::Original, 5000.0, Some([1.0, 0.0, 0.5, 2.0, 0.0, 0.25]), 35.0)).unwrap();
        // The block mixed at start-up (both instances at their defaults: silent) comes first.
        for _ in 0..e.block_frames() {
            let mut f = [0.0; 6];
            e.render_into(&mut f);
            assert_eq!(f, [0.0; 6]);
        }
        // Before the frame's update the core mixes on without the buffered commands.
        let mark = fake::calls().len();
        let mut f = [0.0; 6];
        e.render_into(&mut f);
        assert_eq!(calls_after(mark), ["FMOD_System_Update"]);
        assert_eq!(f, [0.0; 6]);
        for _ in 1..e.block_frames() {
            e.render_into(&mut [0.0; 6]);
        }
        // The frame's Studio update: the whole frame reaches the event; its block follows in order.
        let mark = fake::calls().len();
        e.submit().unwrap();
        assert_eq!(calls_after(mark), ["FMOD_Studio_System_Update"]);
        let mark = fake::calls().len();
        let mut f = [0.1; 6];
        e.render_into(&mut f);
        assert!(calls_after(mark).is_empty(), "the submitted block was mixed at the update");
        let want = [1.1, 0.1, 0.6, 2.1, 0.1, 0.35];
        for (got, want) in f.iter().zip(want) {
            assert!((got - want).abs() < 1e-6, "{f:?}");
        }
        for _ in 1..e.block_frames() {
            e.render_into(&mut [0.0; 6]);
        }
        // With nothing waiting, the next block is a Studio update.
        let mark = fake::calls().len();
        let mut f = [0.0; 6];
        e.render_into(&mut f);
        assert_eq!(calls_after(mark), ["FMOD_Studio_System_Update"]);
        assert!((f[0] - 1.0).abs() < 1e-6);
        e.pause().unwrap();
        e.submit().unwrap();
        for _ in 0..2 * e.block_frames() {
            e.render_into(&mut [0.0; 6]);
        }
        let mut f = [0.0; 6];
        e.render_into(&mut f);
        assert_eq!(f, [0.0; 6], "paused instances are silent");
    }

    /// Studio runs only what a frame submitted at its update, while
    /// the core mixer keeps rendering. A block needed in the middle of a frame (here after the
    /// rpm, load and first three levels) therefore carries none of that frame; the update
    /// carries all of it together.
    #[test]
    fn a_block_inside_a_frame_hears_none_of_the_frame() {
        let mut e = engine();
        e.command(&cmd(EngineType::Original, 1000.0, Some([0.25; 6]), 35.0)).unwrap();
        e.submit().unwrap();
        let drain = |e: &mut FmodEngine| {
            while e.studio.pop_frame().is_some() {}
        };
        drain(&mut e);
        let selected = e.begin_live(&cmd(EngineType::Original, 6000.0, None, 0.0)).unwrap();
        for slot in 0..3 {
            e.speaker_live(selected, slot, 1.0).unwrap();
        }
        let mark = fake::calls().len();
        let mut f = [0.0; 6];
        e.render_into(&mut f);
        assert_eq!(calls_after(mark), ["FMOD_System_Update"]);
        assert_eq!(f, [0.25; 6], "the mid-frame block keeps the previous frame");
        assert_eq!(fake::param(0, "rpm"), Some(1000.0));
        for slot in 3..6 {
            e.speaker_live(selected, slot, 1.0).unwrap();
        }
        e.pitch_live(selected, 77.0).unwrap();
        e.submit().unwrap();
        assert_eq!(fake::param(0, "rpm"), Some(6000.0));
        assert_eq!(fake::param(0, "pitch"), Some(77.0));
        drain(&mut e);
        let mut f = [0.0; 6];
        e.render_into(&mut f);
        assert_eq!(f, [1.0; 6], "the update delivered all six levels at once");
    }

    /// A frame that fails before its update leaves its accepted commands buffered: the mixer
    /// keeps the previous frame, and the next successful update runs them, in call order,
    /// ahead of that frame's own commands. The update's own failure is an error and keeps the
    /// buffer.
    #[test]
    fn raised_frames_wait_for_the_next_update() {
        let mut e = engine();
        e.command(&cmd(EngineType::Original, 1000.0, Some([0.25; 6]), 35.0)).unwrap();
        e.submit().unwrap();
        fake::with(|s| s.fail_parameter = Some("load"));
        assert!(e.command(&cmd(EngineType::Original, 2000.0, Some([0.5; 6]), 36.0)).is_err());
        fake::with(|s| s.fail_parameter = None);
        assert_eq!(fake::queued(), [fake::Queued::Paused(1, true), fake::Queued::Parameter(0, "rpm".into(), 2000.0)]);
        while e.studio.pop_frame().is_some() {}
        let mut f = [0.0; 6];
        e.render_into(&mut f);
        assert_eq!(f, [0.25; 6]);
        assert_eq!(fake::param(0, "rpm"), Some(1000.0));
        fake::with(|s| s.fail = Some("FMOD_Studio_System_Update"));
        assert!(matches!(e.submit(), Err(FmodError::Call { call: "FMOD_Studio_System_Update", code: 30 })));
        assert!(e.has_unsubmitted());
        assert_eq!(fake::queued().len(), 2);
        fake::with(|s| s.fail = None);
        e.command(&cmd(EngineType::Original, 3000.0, None, 37.0)).unwrap();
        e.submit().unwrap();
        assert_eq!(fake::param(0, "rpm"), Some(3000.0));
        assert_eq!(fake::param(0, "load"), Some(1.0));
        assert_eq!(fake::param(0, "pitch"), Some(37.0));
    }

    /// Builds `testdata/fmod_fixture.c` into `libfmod.so.13` and `libfmodstudio.so.13` (the
    /// Studio library linked against the core one, as FMOD ships them).
    fn compiled_fixture() -> (tempfile::TempDir, Arc<FmodLibrary>) {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("fmod_fixture.c");
        std::fs::write(&source, include_str!("testdata/fmod_fixture.c")).unwrap();
        let cc = |args: &[&str]| {
            let status = std::process::Command::new("cc")
                .args(["-std=c11", "-O1", "-Wall", "-Werror", "-shared", "-fPIC"])
                .args(args)
                .current_dir(dir.path())
                .status()
                .expect("a C compiler (cc) builds the FMOD fixture");
            assert!(status.success(), "cc {args:?}");
        };
        cc(&["-DFIXTURE_CORE", "-Wl,-soname,libfmod.so.13", "-o", "libfmod.so.13", "fmod_fixture.c"]);
        cc(&[
            "-DFIXTURE_STUDIO",
            "-Wl,-soname,libfmodstudio.so.13",
            "-o",
            "libfmodstudio.so.13",
            "fmod_fixture.c",
            "-L.",
            "-l:libfmod.so.13",
            "-Wl,-rpath,$ORIGIN",
        ]);
        let lib = FmodLibrary::load(Some(dir.path())).unwrap();
        (dir, lib)
    }

    fn next_frame(e: &mut FmodEngine) -> PerSpeaker {
        let mut f = [0.0; 6];
        e.render_into(&mut f);
        f
    }

    /// The real loading path (`dlopen` of both sonames from the FMOD directory, every bound
    /// symbol) against the compiled fixture: set-up starts silent
    /// events, a block needed inside a frame is mixed by the core without the frame, the
    /// frame's update delivers all of it in one block, and a non-finite value is refused at
    /// its call with `INVALID FLOAT`.
    #[test]
    fn compiled_fixture_runs_the_source_submission_contract() {
        let (_dir, lib) = compiled_fixture();
        let banks = Arc::new(EngineBanks::from_files([vec![1; 10], vec![2; 20], vec![3; 30]]));
        let mut e = FmodEngine::open(lib, banks, 48_000).unwrap();
        let block = e.block_frames();
        for _ in 0..block {
            assert_eq!(next_frame(&mut e), [0.0; 6]);
        }
        e.command(&cmd(EngineType::Original, 3000.0, Some([0.5, 0.25, 0.125, 1.0, 0.0, 0.75]), 35.0)).unwrap();
        e.submit().unwrap();
        for _ in 0..block {
            assert_eq!(next_frame(&mut e), [0.5, 0.25, 0.125, 1.0, 0.0, 0.75]);
        }
        let selected = e.begin_live(&cmd(EngineType::Original, 4000.0, None, 0.0)).unwrap();
        e.speaker_live(selected, 0, 2.0).unwrap();
        for _ in 0..block {
            assert_eq!(next_frame(&mut e), [0.5, 0.25, 0.125, 1.0, 0.0, 0.75], "core block without the open frame");
        }
        for slot in 1..6 {
            e.speaker_live(selected, slot, 2.0).unwrap();
        }
        e.pitch_live(selected, 36.0).unwrap();
        e.submit().unwrap();
        for _ in 0..2 * block {
            assert_eq!(next_frame(&mut e), [2.0; 6]);
        }
        let error = e.command(&cmd(EngineType::Original, f64::NAN, Some([1.0; 6]), 35.0)).unwrap_err();
        assert!(matches!(error, EngineError::Fmod(FmodError::Call { code: 29, .. })));
        assert_eq!(error.native_error().map(|n| n.message), Some("INVALID FLOAT".to_string()));
        e.pause().unwrap();
        assert!(e.paused_live(EngineType::Original).unwrap(), "getPaused reads the buffered pause");
        for _ in 0..block {
            assert_eq!(next_frame(&mut e), [2.0; 6], "the pauses wait for line 1441");
        }
        e.submit().unwrap();
        for _ in 0..2 * block {
            assert_eq!(next_frame(&mut e), [0.0; 6]);
        }
    }

    /// A parameter is set as a 32-bit float; an infinite or NaN value is refused at
    /// the call with `INVALID_FLOAT`, before anything is
    /// buffered, so the frame fails there.
    #[test]
    fn non_finite_parameter_is_refused_at_the_call() {
        let mut e = engine();
        for rpm in [f64::NAN, f64::INFINITY, 1e39] {
            let mark = fake::calls().len();
            let error = e.command(&cmd(EngineType::Original, rpm, Some([1.0; 6]), 35.0)).unwrap_err();
            assert!(matches!(
                error,
                EngineError::Fmod(FmodError::Call { call: "FMOD_Studio_EventInstance_SetParameterByName", code: 29 })
            ));
            assert_eq!(caught_fmod_message(&FmodError::Call { call: "", code: 29 }), "INVALID FLOAT");
            assert_eq!(calls_after(mark).iter().filter(|c| c.contains("SetParameterByName")).count(), 1);
            assert!(!fake::queued().iter().any(|c| matches!(c, fake::Queued::Parameter(..))));
            e.submit().unwrap();
        }
    }

    /// A frame's engine action through the mixer: the FMOD mix lands on the six buses
    /// unscaled (the event applies the per-speaker levels itself).
    #[test]
    fn mixer_plays_the_fmod_engine() {
        use crate::shakers::engine_sound::EngineVoice;
        use crate::shakers::levels::GroupLevels;
        use crate::shakers::mixer::Mixer;
        use crate::shakers::rig::Action;
        use crate::shakers::{SampleBank, ShakerTuning, SpeakerProfile};

        let profile = SpeakerProfile::built_in("6 Speakers - 5.1").unwrap();
        let levels = GroupLevels::compute(&profile, &ShakerTuning::default(), None);
        let mut m = Mixer::new(48_000, SampleBank::synthesized(), levels);
        m.set_engine(EngineVoice::Fmod(Box::new(engine())));
        assert!(m.engine().is_fmod());
        m.apply(&[
            Action::Engine(cmd(EngineType::Original, 3000.0, Some([0.5, 0.25, 0.0, 1.0, 0.0, 0.0]), 35.0)),
            Action::EngineUpdate,
        ])
        .unwrap();
        let mut out = vec![0.0f32; 6 * 600];
        m.render(&mut out);
        let block = BLOCK_FRAMES as usize;
        assert!(out[..6 * block].iter().all(|&v| v == 0.0));
        for f in out[6 * block..].chunks(6) {
            assert_eq!(f, [0.5, 0.25, 0.0, 1.0, 0.0, 0.0]);
        }
        m.apply(&[Action::PauseEngines]).unwrap();
        assert!(!m.engine_playing());
    }

    #[test]
    fn live_shaker_snapshot_engine_kind_tracks_replacement_and_failed_reinitialization() {
        use crate::shakers::engine_sound::EngineVoice;
        use crate::shakers::levels::GroupLevels;
        use crate::shakers::mixer::Mixer;
        use crate::shakers::{SampleBank, ShakerTuning, SpeakerProfile};
        let profile = SpeakerProfile::built_in("6 Speakers - 5.1").unwrap();
        let levels = GroupLevels::compute(&profile, &ShakerTuning::default(), None);
        let mut mixer = Mixer::new(48_000, SampleBank::synthesized(), levels);
        let status = mixer.engine_status();
        assert!(!status.is_fmod());
        mixer.set_engine(EngineVoice::Fmod(Box::new(engine())));
        assert!(status.is_fmod());
        fake::with(|state| state.fail = Some("FMOD_Studio_System_GetEvent"));
        mixer.set_sample_rate(96_000);
        assert!(!mixer.engine().is_fmod());
        assert!(!status.is_fmod());
        fake::with(|state| state.fail = None);
        mixer.set_engine(EngineVoice::Fmod(Box::new(engine())));
        assert!(status.is_fmod());
        mixer.set_engine(EngineVoice::synth(96_000));
        assert!(!status.is_fmod());
    }

    #[test]
    fn output_recreation_releases_studio_before_reopening_even_at_the_same_rate() {
        use crate::shakers::{engine_sound::EngineVoice, mixer::Mixer, SampleBank};
        let mut mixer = Mixer::new(48000, SampleBank::synthesized(), crate::shakers::levels::GroupLevels::empty());
        mixer.set_engine(EngineVoice::Fmod(Box::new(engine())));
        mixer
            .apply(&[crate::shakers::rig::Action::Engine(cmd(EngineType::Original, 3000., Some([1.; 6]), 35.))])
            .unwrap();
        let mark = fake::calls().len();
        mixer.close_output(48000);
        assert!(!mixer.engine_playing());
        assert!(!mixer.engine_status().is_fmod());
        let after_close = fake::calls().len();
        assert!(fake::calls()[mark..].iter().any(|call| call == "FMOD_Studio_System_Release"));
        assert!(!fake::calls()[mark..].iter().any(|call| call.contains("System_Create")));
        mixer.reopen_engine();
        assert!(mixer.engine_status().is_fmod());
        assert!(mixer.engine_playing());
        assert!(fake::calls()[after_close..].iter().any(|call| call.contains("System_Create")));
        let mark = fake::calls().len();
        mixer.close_output(48000);
        mixer.close_output(48000);
        assert_eq!(
            fake::calls()[mark..].iter().filter(|call| call.as_str() == "FMOD_Studio_System_Release").count(),
            1
        );
        fake::with(|state| state.fail = Some("FMOD_Studio_System_GetEvent"));
        mixer.reopen_engine();
        assert!(!mixer.engine_status().is_fmod());
        fake::with(|state| state.fail = None);
        mixer.reopen_engine();
        assert!(mixer.engine_status().is_fmod());
    }

    #[test]
    fn missing_event_fails_open() {
        let lib = fake::library();
        fake::with(|s| s.fail = Some("FMOD_Studio_System_GetEvent"));
        assert!(matches!(
            FmodEngine::open(lib, banks(), 48_000),
            Err(FmodError::Call { call: "FMOD_Studio_System_GetEvent", .. })
        ));
    }

    /// A failing call is an error: the block stops at it, the next frame runs again.
    #[test]
    fn failing_call_ends_the_engine_block() {
        let mut e = engine();
        fake::with(|s| s.fail = Some("FMOD_Studio_EventInstance_SetParameterByName"));
        let mark = fake::calls().len();
        let result = e.command(&cmd(EngineType::Original, 100.0, Some([1.0; 6]), 35.0));
        assert!(matches!(result, Err(EngineError::Fmod(FmodError::Call { .. }))));
        let params = calls_after(mark).into_iter().filter(|c| c.contains("SetParameterByName")).count();
        assert_eq!(params, 1, "nothing after the failing rpm call");
        fake::with(|s| s.fail = None);
        e.command(&cmd(EngineType::Original, 100.0, Some([1.0; 6]), 35.0)).unwrap();
        e.submit().unwrap();
        assert_eq!(fake::param(0, "rpm"), Some(100.0));
    }

    #[test]
    fn idle_engine_pauses_allow_an_active_call_between_the_two_instances() {
        use crate::shakers::{
            rig::FrameStage,
            system::{FrameState, ShakerSettings, ShakerSystem},
            SampleBank, ShakerTuning, SpeakerProfile,
        };
        let tuning = ShakerTuning { engine_type: Some(EngineType::Original), ..ShakerTuning::default() };
        let mut system = ShakerSystem::new(
            ShakerSettings {
                config: Default::default(),
                profile: Some(SpeakerProfile::built_in("6 Speakers - 5.1").unwrap()),
                tuning,
                max_traction_loss: 2.0,
            },
            SampleBank::synthesized(),
            48000,
        );
        system.set_engine(super::super::engine_sound::EngineVoice::Fmod(Box::new(engine())));
        let telemetry = openrigcue_model::Telemetry::off();
        let mut idle = ShakerSystem::begin_live_frame(
            &telemetry,
            FrameState { output_ready: true, game_running: false, switch_on: true },
        );
        let mut intervened = false;
        for stage in idle.stages() {
            system.advance_live_frame(&mut idle, stage).unwrap();
            if stage == FrameStage::IdlePause(EngineType::Original) {
                assert!(fake::api_paused(0), "queued pause is visible to getPaused");
                let mut active = ShakerSystem::begin_live_frame(
                    &telemetry,
                    FrameState { output_ready: true, game_running: true, switch_on: true },
                );
                for stage in active.stages() {
                    system.advance_live_frame(&mut active, stage).unwrap();
                }
                intervened = true;
            }
        }
        assert!(intervened);
        assert!(!fake::is_paused(0), "active call resumed O after idle already paused it");
        assert!(fake::is_paused(1), "idle continuation still paused E");
    }

    /// Live frames: the Studio update is its own stage right after pitch and
    /// before the previous-value assignments, and right after the inactive branch's two pauses.
    /// Until it runs the events hear nothing of the frame; a failing update ends
    /// the frame at that stage.
    #[test]
    fn live_frames_submit_at_the_source_update_and_raise_there() {
        use crate::shakers::{
            rig::{FrameStage, MemoryField},
            system::{FrameState, ShakerSettings, ShakerSystem},
            SampleBank, ShakerTuning, SpeakerProfile,
        };
        let tuning = ShakerTuning { engine_type: Some(EngineType::Original), ..ShakerTuning::default() };
        let mut system = ShakerSystem::new(
            ShakerSettings {
                config: Default::default(),
                profile: Some(SpeakerProfile::built_in("6 Speakers - 5.1").unwrap()),
                tuning,
                max_traction_loss: 2.0,
            },
            SampleBank::synthesized(),
            48000,
        );
        system.set_engine(super::super::engine_sound::EngineVoice::Fmod(Box::new(engine())));
        let mut telemetry = openrigcue_model::Telemetry::off();
        telemetry.rpm = 3000.0;
        telemetry.max_rpm = 7000.0;
        let active = FrameState { output_ready: true, game_running: true, switch_on: true };
        let idle = FrameState { output_ready: true, game_running: false, switch_on: true };

        let mut progress = ShakerSystem::begin_live_frame(&telemetry, active);
        let stages = progress.stages();
        let update = stages.iter().position(|s| *s == FrameStage::EngineUpdate).unwrap();
        assert_eq!(stages[update - 1], FrameStage::EnginePitch);
        assert!(matches!(stages[update + 1], FrameStage::Commit(MemoryField::Gear)));
        assert_eq!(stages.iter().filter(|s| **s == FrameStage::EngineUpdate).count(), 1);
        for stage in &stages[..update] {
            system.advance_live_frame(&mut progress, *stage).unwrap();
        }
        assert_eq!(fake::param(0, "rpm"), None, "nothing reaches the event before line 2454");
        fake::with(|s| s.fail = Some("FMOD_Studio_System_Update"));
        let error = system.advance_live_frame(&mut progress, FrameStage::EngineUpdate).unwrap_err();
        assert!(matches!(error, EngineError::Fmod(FmodError::Call { call: "FMOD_Studio_System_Update", .. })));
        fake::with(|s| s.fail = None);
        assert_eq!(fake::param(0, "rpm"), None);

        let mut progress = ShakerSystem::begin_live_frame(&telemetry, active);
        for stage in progress.stages() {
            system.advance_live_frame(&mut progress, stage).unwrap();
        }
        assert_eq!(fake::param(0, "rpm"), Some(4285.71), "the raised frame's commands ran first, then this one's");

        let mut progress = ShakerSystem::begin_live_frame(&telemetry, idle);
        let stages = progress.stages();
        assert_eq!(
            stages[..3],
            [
                FrameStage::IdlePause(EngineType::Original),
                FrameStage::IdlePause(EngineType::Enhanced),
                FrameStage::EngineUpdate
            ]
        );
        for stage in &stages[..2] {
            system.advance_live_frame(&mut progress, *stage).unwrap();
        }
        assert!(!fake::is_paused(0));
        assert!(fake::api_paused(0) && fake::api_paused(1));
        let mark = fake::calls().len();
        system.advance_live_frame(&mut progress, FrameStage::EngineUpdate).unwrap();
        assert_eq!(calls_after(mark), ["FMOD_Studio_System_Update"]);
        assert!(fake::is_paused(0) && fake::is_paused(1));
    }

    #[test]
    fn live_engine_paused_query_result_survives_a_concurrent_pause_before_resume() {
        use crate::shakers::{
            rig::FrameStage,
            system::{FrameState, ShakerSettings, ShakerSystem},
            SampleBank, ShakerTuning, SpeakerProfile,
        };
        let tuning = ShakerTuning { engine_type: Some(EngineType::Original), ..ShakerTuning::default() };
        let mut system = ShakerSystem::new(
            ShakerSettings {
                config: Default::default(),
                profile: Some(SpeakerProfile::built_in("6 Speakers - 5.1").unwrap()),
                tuning,
                max_traction_loss: 2.0,
            },
            SampleBank::synthesized(),
            48000,
        );
        system.set_engine(super::super::engine_sound::EngineVoice::Fmod(Box::new(engine())));
        let mut telemetry = openrigcue_model::Telemetry::off();
        telemetry.rpm = 3000.0;
        telemetry.max_rpm = 7000.0;
        let state = FrameState { output_ready: true, game_running: true, switch_on: true };
        let mut progress = ShakerSystem::begin_live_frame(&telemetry, state);
        let mark = fake::calls().len();
        for stage in progress.stages() {
            system.advance_live_frame(&mut progress, stage).unwrap();
            if stage == FrameStage::EngineCheckPaused {
                assert!(!fake::is_paused(0));
                assert_eq!(fake::param(0, "rpm"), None, "parameter setters must not precede paused-query boundary");
                system.mixer().lock().unwrap().engine_mut().pause_instance_live(EngineType::Original).unwrap();
            }
        }
        assert!(fake::is_paused(0), "cached false paused-query skips later resume");
        assert!(fake::is_paused(1));
        assert_eq!(fake::param(0, "rpm"), Some(4285.71));
        assert!(!calls_after(mark).iter().any(|call| call == "FMOD_Studio_EventInstance_SetPaused 0 0"));
        let mut next = ShakerSystem::begin_live_frame(&telemetry, state);
        for stage in next.stages() {
            system.advance_live_frame(&mut next, stage).unwrap();
        }
        assert!(!fake::is_paused(0), "next call sees paused and resumes it");
    }

    #[test]
    fn separate_engine_parameter_calls_keep_their_captured_instance_and_error_boundary() {
        for failed in [None, Some(0), Some(2), Some(5)] {
            let mut voice = super::super::engine_sound::EngineVoice::Fmod(Box::new(engine()));
            let selected = voice.begin_live(&cmd(EngineType::Original, 3000.0, Some([0.25; 6]), 35.0)).unwrap();
            let other = voice.begin_live(&cmd(EngineType::Enhanced, 4000.0, Some([0.5; 6]), 36.0)).unwrap();
            assert_eq!(other, EngineType::Enhanced);
            let names = ["fl", "fr", "ce", "sub", "sl", "sr"];
            fake::with(|state| state.fail_parameter = failed.map(|slot| names[slot]));
            let mark = fake::calls().len();
            let mut lines = vec![];
            let mut failed_speakers = false;
            for slot in 0..6 {
                if !failed_speakers {
                    failed_speakers = !voice
                        .speaker_live_with_output(selected, slot, Ok(Some((slot + 1) as f64)), &mut |line| {
                            lines.push(line.to_owned());
                            Ok(())
                        })
                        .unwrap();
                }
            }
            voice.pitch_live(selected, 77.0).unwrap();
            voice.submit().unwrap();
            assert_eq!(fake::param(0, "rpm"), Some(3000.0));
            assert_eq!(fake::param(1, "rpm"), Some(4000.0));
            assert_eq!(fake::param(0, "pitch"), Some(77.0));
            assert_eq!(fake::param(1, "pitch"), None);
            for (slot, name) in names.iter().enumerate() {
                assert_eq!(
                    fake::param(0, name),
                    if failed.is_none_or(|failed| slot < failed) { Some((slot + 1) as f32) } else { None }
                );
                assert_eq!(fake::param(1, name), None);
            }
            assert_eq!(lines.len(), usize::from(failed.is_some()));
            let params = calls_after(mark).into_iter().filter(|call| call.contains("SetParameterByName")).count();
            assert_eq!(params, failed.map_or(7, |slot| slot + 2));
        }
    }

    #[test]
    fn native_engine_speaker_error_keeps_accepted_prefix_and_runs_pitch() {
        for failed in [0, 2, 5] {
            let mut e = engine();
            e.command(&cmd(EngineType::Original, 1000.0, Some([0.25; 6]), 35.0)).unwrap();
            let names = ["fl", "fr", "ce", "sub", "sl", "sr"];
            fake::with(|state| state.fail_parameter = Some(names[failed]));
            let mark = fake::calls().len();
            let levels = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
            let mut lines = Vec::new();
            e.command_with_output(&cmd(EngineType::Original, 6000.0, Some(levels), 77.0), &mut |line| {
                lines.push(line.to_owned());
                Ok(())
            })
            .unwrap();
            assert_eq!(lines, ["error mixed level outputs not loaded yet INVALID HANDLE\n"]);
            e.submit().unwrap();
            for (index, name) in names.into_iter().enumerate() {
                assert_eq!(fake::param(0, name), Some(if index < failed { levels[index] as f32 } else { 0.25 }));
            }
            assert_eq!(fake::param(0, "rpm"), Some(6000.0));
            assert_eq!(fake::param(0, "load"), Some(1.0));
            assert_eq!(fake::param(0, "pitch"), Some(77.0));
            let calls: Vec<_> =
                calls_after(mark).into_iter().filter(|call| call.contains("SetParameterByName")).collect();
            assert_eq!(calls.len(), 2 + failed + 1 + 1, "rpm/load, speaker prefix including failure, pitch");
            assert!(calls.last().unwrap().contains("pitch=77"));
            fake::with(|state| state.fail_parameter = None);
            lines.clear();
            e.command_with_output(&cmd(EngineType::Original, 6000.0, Some(levels), 8.0), &mut |line| {
                lines.push(line.to_owned());
                Ok(())
            })
            .unwrap();
            assert!(lines.is_empty());
            e.submit().unwrap();
            for (index, name) in names.into_iter().enumerate() {
                assert_eq!(fake::param(0, name), Some(levels[index] as f32));
            }
        }
    }

    #[test]
    fn native_engine_fade_error_is_caught_between_load_and_pitch() {
        use crate::shakers::rig::NativeEngineParameters;
        use crate::shakers::tuning::DspError;
        let mut e = engine();
        e.command(&cmd(EngineType::Original, 1000.0, Some([0.25; 6]), 35.0)).unwrap();
        let mut c = cmd(EngineType::Original, 6000.0, None, 8.0);
        c.native_parameters = Some(Box::new(NativeEngineParameters {
            levels: Err(DspError { class: "KeyError", message: "'engine_fadeoff_end'".into() }),
            pitch: Ok(8.0),
        }));
        let mut lines = Vec::new();
        let mark = fake::calls().len();
        e.command_with_output(&c, &mut |line| {
            lines.push(line.to_owned());
            Ok(())
        })
        .unwrap();
        assert_eq!(lines, ["error mixed level outputs not loaded yet 'engine_fadeoff_end'\n"]);
        e.submit().unwrap();
        let params: Vec<_> = calls_after(mark).into_iter().filter(|call| call.contains("SetParameterByName")).collect();
        assert_eq!(params.len(), 3);
        assert_eq!(fake::param(0, "fl"), Some(0.25));
        assert_eq!(fake::param(0, "pitch"), Some(8.0));
    }

    #[test]
    fn native_engine_fatal_parameters_and_output_failure_stop_the_tail() {
        use crate::shakers::rig::NativeEngineParameters;
        use crate::shakers::tuning::DspError;
        for failed in ["rpm", "load", "pitch"] {
            let mut e = engine();
            fake::with(|state| state.fail_parameter = Some(failed));
            let mut lines = Vec::new();
            let error = e
                .command_with_output(&cmd(EngineType::Original, 6000.0, Some([1.0; 6]), 8.0), &mut |line| {
                    lines.push(line.to_owned());
                    Ok(())
                })
                .unwrap_err();
            assert!(matches!(error, EngineError::Fmod(FmodError::Call { .. })));
            assert!(lines.is_empty(), "{failed} lies outside the local speaker catch");
            e.submit().unwrap();
            assert_eq!(fake::param(0, failed), None);
        }
        let mut e = engine();
        let mut c = cmd(EngineType::Original, 6000.0, None, 8.0);
        c.native_parameters = Some(Box::new(NativeEngineParameters {
            levels: Err(DspError {
                class: "TypeError",
                message: "'<' not supported between instances of 'float' and 'str'".into(),
            }),
            pitch: Err(DspError { class: "ValueError", message: "later pitch conversion".into() }),
        }));
        let mut lines = Vec::new();
        let error = e
            .command_with_output(&c, &mut |line| {
                lines.push(line.to_owned());
                Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
            })
            .unwrap_err();
        assert!(
            matches!(error, EngineError::DiagnosticOutput(error) if error.kind() == std::io::ErrorKind::BrokenPipe)
        );
        assert_eq!(
            lines,
            ["error mixed level outputs not loaded yet '<' not supported between instances of 'float' and 'str'\n"]
        );
        e.submit().unwrap();
        assert_eq!(fake::param(0, "rpm"), Some(6000.0));
        assert_eq!(fake::param(0, "load"), Some(1.0));
        assert_eq!(fake::param(0, "pitch"), None);
        c.native_parameters.as_mut().unwrap().levels = Ok([2.0; 6]);
        let error = e.command_with_output(&c, &mut |_| Ok(())).unwrap_err();
        assert!(matches!(error, EngineError::EngineParameter { error } if error.class == "ValueError"));
        e.submit().unwrap();
        assert_eq!(fake::param(0, "sr"), Some(2.0), "the late pitch error keeps accepted speaker writes");
        assert_eq!(fake::param(0, "pitch"), None);
    }

    /// A type other than 'O'/'E': 'O' is paused (as for 'E'), then the instance lookup fails.
    #[test]
    fn unknown_engine_type_pauses_original_and_raises() {
        let mut e = engine();
        let mark = fake::calls().len();
        let unknown = EngineCommand { engine_type: None, ..cmd(EngineType::Original, 100.0, None, 35.0) };
        assert!(matches!(e.command(&unknown), Err(EngineError::NoInstance)));
        assert_eq!(calls_after(mark), ["FMOD_Studio_EventInstance_SetPaused 0 1"]);
    }

    #[test]
    fn native_engine_selector_required_lookup_fails_before_any_studio_call() {
        use super::super::tuning::DspError;
        let mut e = engine();
        let mut command = cmd(EngineType::Original, 100.0, None, 35.0);
        command.native_engine_selector =
            Some(Err(DspError { class: "KeyError", message: "'shaker_engine_type'".into() }));
        fake::with(|state| state.fail = Some("FMOD_Studio_EventInstance_SetPaused"));
        let mark = fake::calls().len();
        let error = e.command(&command).unwrap_err();
        assert!(
            matches!(error, EngineError::EngineSelector { error } if error.class == "KeyError" && error.message == "'shaker_engine_type'")
        );
        assert!(calls_after(mark).is_empty());
        fake::with(|state| state.fail = None);
    }

    #[test]
    fn native_engine_selector_key_failure_follows_pause_and_yields_to_pause_errors() {
        use openrigcue_model::native_value::LegacyValue;
        for (key, class, message) in [
            (LegacyValue::from("X"), "KeyError", "'X'"),
            (LegacyValue::Int(17), "KeyError", "17"),
            (LegacyValue::None, "KeyError", "None"),
            (LegacyValue::List(vec![]), "TypeError", "unhashable type: 'list'"),
            (LegacyValue::ByteArray(b"O".to_vec()), "TypeError", "unhashable type: 'bytearray'"),
            (
                LegacyValue::Tuple(vec![LegacyValue::from("O"), LegacyValue::Set(vec![])]),
                "TypeError",
                "unhashable type: 'set'",
            ),
            (
                LegacyValue::Object(openrigcue_model::native_object::NativeObject::new(LegacyValue::List(vec![]))),
                "TypeError",
                "unhashable type: 'list'",
            ),
            (
                LegacyValue::from_integer(LegacyValue::from("1".repeat(4300).as_str()).py_integer().unwrap()),
                "KeyError",
                &"1".repeat(4300)[..],
            ),
            (LegacyValue::Bytes(b"O".to_vec()), "KeyError", "b'O'"),
        ] {
            let mut e = engine();
            let mut command = cmd(EngineType::Original, 100.0, None, 35.0);
            command.native_engine_selector = Some(Ok(key));
            fake::with(|state| state.fail = Some("FMOD_Studio_EventInstance_SetPaused"));
            let mark = fake::calls().len();
            assert!(matches!(
                e.command(&command),
                Err(EngineError::Fmod(FmodError::Call { call: "FMOD_Studio_EventInstance_SetPaused", .. }))
            ));
            assert_eq!(calls_after(mark), ["FMOD_Studio_EventInstance_SetPaused 0 1"]);
            fake::with(|state| state.fail = None);
            let mark = fake::calls().len();
            let error = e.command(&command).unwrap_err();
            assert!(
                matches!(error, EngineError::EngineSelector { error } if error.class == class && error.message == message)
            );
            assert_eq!(calls_after(mark), ["FMOD_Studio_EventInstance_SetPaused 0 1"]);
        }
    }

    /// The selector's key-error text is the checked quoted form of the raw key: an integer of more
    /// than 4300 digits (alone or inside a tuple) cannot be rendered, which the error carries as a
    /// value error; the pause of the other instance has already happened.
    #[test]
    fn native_engine_selector_keeps_an_unprintable_key_error() {
        use openrigcue_model::native_value::LegacyValue;
        // 10**4300 (4301 digits), built by multiplication: int() of such text is itself refused.
        let huge = (0..4300).fold(LegacyValue::Int(1), |value, _| value.py_mul(&LegacyValue::Int(10)).unwrap());
        for key in [huge.clone(), LegacyValue::Tuple(vec![LegacyValue::Int(1), huge])] {
            let mut e = engine();
            let mut command = cmd(EngineType::Original, 100.0, None, 35.0);
            command.native_engine_selector = Some(Ok(key));
            let mark = fake::calls().len();
            let error = e.command(&command).unwrap_err();
            let failure = error.str_failure().expect("str(KeyError(key)) raises");
            assert_eq!(failure.class, "ValueError");
            assert!(
                failure.message.starts_with("Exceeds the limit (4300 digits) for integer string conversion"),
                "{}",
                failure.message
            );
            assert_eq!(error.native_error().unwrap().class, "KeyError");
            assert_eq!(calls_after(mark), ["FMOD_Studio_EventInstance_SetPaused 0 1"]);
        }
    }

    #[test]
    fn native_engine_selector_valid_keys_choose_the_instance_before_parameters() {
        use openrigcue_model::native_value::LegacyValue;
        let wrapped =
            |key: &str| LegacyValue::Object(openrigcue_model::native_object::NativeObject::new(LegacyValue::from(key)));
        for (key, selected, other) in
            [(LegacyValue::from("O"), 0, 1), (LegacyValue::from("E"), 1, 0), (wrapped("O"), 0, 1), (wrapped("E"), 1, 0)]
        {
            let mut e = engine();
            let mut command = cmd(EngineType::Original, 100.0, Some([1.0; 6]), 35.0);
            command.engine_type = None;
            command.native_engine_selector = Some(Ok(key));
            let mark = fake::calls().len();
            e.command(&command).unwrap();
            e.submit().unwrap();
            assert_eq!(calls_after(mark)[0], format!("FMOD_Studio_EventInstance_SetPaused {other} 1"));
            assert!(fake::is_paused(other));
            assert!(!fake::is_paused(selected));
            assert_eq!(fake::param(selected, "rpm"), Some(100.0));
            assert_eq!(fake::param(selected, "pitch"), Some(35.0));
        }
    }

    #[test]
    fn new_rate_rebuilds_the_system() {
        let mut e = engine();
        e.set_sample_rate(48_000).unwrap();
        let mark = fake::calls().len();
        e.set_sample_rate(44_100).unwrap();
        assert_eq!(e.sample_rate(), 44_100);
        let calls = calls_after(mark);
        assert!(calls.contains(&"FMOD_System_SetSoftwareFormat 44100 6 0".to_string()));
        assert!(calls.contains(&"FMOD_Studio_System_Release".to_string()));
        assert!(e.is_playing());
    }

    #[test]
    fn banks_come_from_the_srs_sounds_folder() {
        let dir = std::env::temp_dir().join(format!("openrigcue-fmod-banks-{}", std::process::id()));
        let engine_dir = dir.join("sounds").join("engine");
        std::fs::create_dir_all(&engine_dir).unwrap();
        for (i, name) in BANK_FILES.iter().enumerate() {
            std::fs::write(engine_dir.join(name), vec![i as u8; i + 1]).unwrap();
        }
        let banks = EngineBanks::read(&dir).unwrap();
        assert_eq!(banks.files, [vec![0], vec![1, 1], vec![2, 2, 2]]);
        std::fs::remove_file(engine_dir.join("Vehicles.bank")).unwrap();
        assert!(matches!(EngineBanks::read(&dir), Err(FmodError::Bank { name: "Vehicles.bank", .. })));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// RMS per speaker and zero crossings of the sub channel over `frames` rendered frames.
    fn real_window(e: &mut FmodEngine, frames: usize) -> ([f64; 6], usize) {
        let mut sums = [0.0; 6];
        let mut crossings = 0;
        let mut last = 0.0f64;
        for _ in 0..frames {
            let f = next_frame(e);
            for (s, v) in sums.iter_mut().zip(f) {
                assert!(v.is_finite(), "non-finite FMOD sample");
                *s += v * v;
            }
            if (f[3] > 0.0) != (last > 0.0) {
                crossings += 1;
            }
            last = f[3];
        }
        (sums.map(|s| (s / frames as f64).sqrt()), crossings)
    }

    /// Frames of the shaker frame period (1/27 s) at the engine rate.
    fn real_frame(e: &FmodEngine) -> usize {
        e.sample_rate() as usize / 27
    }

    /// Commands and submits `c` once per frame period for `seconds`, then
    /// measures one more second at that cadence.
    fn real_steady(e: &mut FmodEngine, c: &EngineCommand, seconds: f64) -> ([f64; 6], usize) {
        let frame = real_frame(e);
        for _ in 0..(seconds * 27.0) as usize {
            e.command(c).unwrap();
            e.submit().unwrap();
            real_window(e, frame);
        }
        let mut sums = [0.0; 6];
        let mut crossings = 0;
        for _ in 0..27 {
            e.command(c).unwrap();
            e.submit().unwrap();
            let (rms, z) = real_window(e, frame);
            for (s, r) in sums.iter_mut().zip(rms) {
                *s += r * r / 27.0;
            }
            crossings += z;
        }
        (sums.map(f64::sqrt), crossings)
    }

    fn real_drain(e: &mut FmodEngine) {
        while e.studio.pop_frame().is_some() {}
    }

    /// The user's Linux FMOD 2.02 libraries (`OPENRIGCUE_FMOD_DIR` or the default directories)
    /// with the SRS banks (`OPENRIGCUE_SRS_SOUNDS`, read as data), non-real-time output:
    /// every update mixes one block; Studio's buffered setters (levels, pause) are refused or
    /// accepted at the call, are visible to `getPaused` at once, are not heard by core-only
    /// blocks however many follow, and are heard after a Studio update; the events
    /// keep sounding through core-only blocks and respond to rpm, load and pitch.
    #[test]
    #[ignore = "needs the user's Linux FMOD 2.02 libraries and a local SRS installation or installer"]
    fn real_runtime_buffers_studio_commands_against_core_mixing() {
        let srs = std::env::var("OPENRIGCUE_SRS_SOUNDS").expect("OPENRIGCUE_SRS_SOUNDS");
        let mut e = FmodEngine::from_srs(Path::new(&srs), 48_000).unwrap();
        let version = e.lib.version().unwrap();
        assert_eq!(version & 0xffff_ff00, 0x0002_0200, "FMOD 2.02 runtime, found {version:08x}");
        eprintln!("native version {version:08x}");

        // One block per update, from Studio's update and from the core update alike.
        for rate in [44_100, 48_000] {
            e.set_sample_rate(rate).unwrap();
            let block = e.block_frames();
            real_drain(&mut e);
            for _ in 0..8 {
                e.studio.update().unwrap();
                assert_eq!(e.studio.captured(), block, "Studio update at {rate}");
                real_drain(&mut e);
                e.studio.mix_core_block().unwrap();
                assert_eq!(e.studio.captured(), block, "core update at {rate}");
                real_drain(&mut e);
            }
        }
        let block = e.block_frames();
        let all = cmd(EngineType::Original, 3000.0, Some([1.0; 6]), 35.0);
        let (loud, _) = real_steady(&mut e, &all, 0.5);
        eprintln!("all levels {loud:?}");
        assert!(loud.iter().all(|&r| r > 1e-3), "{loud:?}");

        // A frame's levels buffered, then core-only blocks for two seconds (frames raising
        // before their update): every block keeps all six speakers and the event keeps sounding.
        e.command(&cmd(EngineType::Original, 3000.0, Some([1.0, 0.0, 0.0, 0.0, 0.0, 0.0]), 35.0)).unwrap();
        assert!(e.has_unsubmitted());
        real_drain(&mut e);
        let mut quietest = [f64::INFINITY; 6];
        for _ in 0..2 * e.sample_rate() as usize / block {
            let (rms, _) = real_window(&mut e, block);
            for (q, r) in quietest.iter_mut().zip(rms) {
                *q = q.min(r);
            }
        }
        eprintln!("core-only quietest block {quietest:?}");
        assert!(quietest.iter().all(|&r| r > 1e-4), "core-only blocks dropped a speaker: {quietest:?}");
        assert!(e.has_unsubmitted(), "core-only blocks submit nothing");
        e.submit().unwrap();
        let frame = real_frame(&e);
        let settle = e.sample_rate() as usize / 4;
        real_window(&mut e, settle);
        let (routed, _) = real_window(&mut e, frame * 4);
        eprintln!("after the update {routed:?}");
        assert!(routed[0] > 1e-3 && routed[1..].iter().all(|&r| r < 1e-8), "{routed:?}");

        // Call-time refusals: nothing is buffered for a refused value or name.
        let selected = e.begin_live(&all).unwrap();
        for bad in [f32::NAN, f32::INFINITY] {
            let error = e.studio.set_parameter(e.instances[index(selected)], c"rpm", bad).unwrap_err();
            assert!(matches!(error, FmodError::Call { code: 29, .. }), "{error:?}");
        }
        let error = e.studio.set_parameter(e.instances[index(selected)], c"no such parameter", 1.0).unwrap_err();
        eprintln!("unknown parameter {error:?}");
        assert!(matches!(error, FmodError::Call { code: 74, .. }), "{error:?}");
        let error = e.command(&cmd(EngineType::Original, f64::NAN, Some([1.0; 6]), 35.0)).unwrap_err();
        assert_eq!(error.native_error().map(|n| n.message), Some("INVALID FLOAT".to_string()));
        let (back, _) = real_steady(&mut e, &all, 0.5);
        assert!(back.iter().all(|&r| r > 1e-3), "{back:?}");

        // The inactive branch's pauses: visible to getPaused at once, unheard until the Studio update.
        e.pause().unwrap();
        assert!(e.paused_live(EngineType::Original).unwrap() && e.paused_live(EngineType::Enhanced).unwrap());
        real_drain(&mut e);
        let (held, _) = real_window(&mut e, frame * 4);
        eprintln!("pause buffered {held:?}");
        assert!(held.iter().all(|&r| r > 1e-3), "{held:?}");
        e.submit().unwrap();
        let settle = e.sample_rate() as usize / 4;
        real_window(&mut e, settle);
        let (paused, _) = real_window(&mut e, frame * 4);
        assert!(paused.iter().all(|&r| r < 1e-8), "{paused:?}");
        let (resumed, _) = real_steady(&mut e, &all, 0.5);
        eprintln!("resumed {resumed:?}");
        assert!(resumed.iter().all(|&r| r > 1e-3), "{resumed:?}");

        // Parameter response of Vehicles.bank's engine events.
        for engine_type in [EngineType::Original, EngineType::Enhanced] {
            let at = |rpm: f64, load: f64, pitch: f64| EngineCommand {
                load,
                ..cmd(engine_type, rpm, Some([1.0; 6]), pitch)
            };
            let differs = |(a, za): ([f64; 6], usize), (b, zb): ([f64; 6], usize)| {
                (a[3] - b[3]).abs() > 0.01 * b[3] || za.abs_diff(zb) > zb / 100
            };
            let mut previous = None;
            for rpm in [1000.0, 3000.0, 6000.0] {
                let now = real_steady(&mut e, &at(rpm, 1.0, 35.0), 0.5);
                eprintln!("{engine_type:?} rpm {rpm}: sub rms {:.5} crossings {}", now.0[3], now.1);
                if let Some(before) = previous {
                    assert!(differs(before, now), "{engine_type:?}: rpm {rpm}");
                }
                previous = Some(now);
            }
            let idle = real_steady(&mut e, &at(3000.0, 0.0, 35.0), 0.5);
            let full = real_steady(&mut e, &at(3000.0, 1.0, 35.0), 0.5);
            eprintln!("{engine_type:?} load 0: {:.5}/{}, load 1: {:.5}/{}", idle.0[3], idle.1, full.0[3], full.1);
            assert!(differs(idle, full), "{engine_type:?}: load");
            let low = real_steady(&mut e, &at(3000.0, 1.0, 0.0), 0.5);
            let high = real_steady(&mut e, &at(3000.0, 1.0, 70.0), 0.5);
            eprintln!("{engine_type:?} pitch 0: {:.5}/{}, pitch 70: {:.5}/{}", low.0[3], low.1, high.0[3], high.1);
            assert!(differs(low, high), "{engine_type:?}: pitch");
        }
    }

    /// Reads the banks of the user's own SRS installation or installer.
    #[test]
    #[ignore = "needs a local SRS installation or installer"]
    fn real_banks() {
        let path = std::env::var("OPENRIGCUE_SRS_SOUNDS").expect("OPENRIGCUE_SRS_SOUNDS");
        let banks = EngineBanks::read(Path::new(&path)).unwrap();
        for data in &banks.files {
            assert!(data.starts_with(b"RIFF"), "FMOD banks are RIFF files");
        }
    }
}
