//! The IntelliTune voice prompts.
//!
//! The IntelliTune wizard plays pre-recorded prompts. They are imported from the
//! user's own SRS installation or installer (`sounds/voices`) and played with a local player program.
//!
//! Notifications are spoken while the Voice Notifications setting is on: their texts are queued and a monitor
//! thread turns one at a time into speech with Google's text-to-speech service, plays it and removes the file.

use std::collections::{BTreeMap, VecDeque};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::Router;
use openrigcue_effects::accessories::alerts::VoiceClip;
use openrigcue_effects::shakers::nsis::NsisArchive;
use serde::Deserialize;

use crate::api::{ApiContext, ApiError};

/// Where a prompt's recording is.
#[derive(Debug, Clone)]
enum ClipSource {
    /// A file of an SRS installation.
    File(PathBuf),
    /// Extracted from the SRS installer.
    Bytes(Arc<Vec<u8>>),
}

/// The voice prompts imported from the user's SRS installation; OpenRigCue ships none.
#[derive(Debug, Clone, Default)]
pub struct ClipLibrary {
    clips: BTreeMap<VoiceClip, ClipSource>,
}

impl ClipLibrary {
    /// No recordings.
    pub fn empty() -> ClipLibrary {
        ClipLibrary::default()
    }

    /// The prompts of `path`: an SRS install directory (holding `sounds/voices`), its `sounds` directory, the
    /// `voices` directory itself, or the SRS installer executable. Returns what was found and, per missing
    /// prompt, why.
    pub fn import(path: &Path) -> (ClipLibrary, Vec<(VoiceClip, String)>) {
        let mut library = ClipLibrary::default();
        let mut missing = Vec::new();
        if path.is_file() {
            let archive = std::fs::read(path)
                .map_err(|e| e.to_string())
                .and_then(|bytes| NsisArchive::open(bytes).map_err(|e| e.to_string()));
            match archive {
                Ok(archive) => {
                    for clip in VoiceClip::ALL {
                        match archive.read(&format!("sounds/voices/{}", clip.file_name())) {
                            Ok(bytes) => {
                                library.clips.insert(clip, ClipSource::Bytes(Arc::new(bytes)));
                            }
                            Err(error) => missing.push((clip, error.to_string())),
                        }
                    }
                }
                Err(error) => missing.extend(VoiceClip::ALL.map(|clip| (clip, error.clone()))),
            }
            return (library, missing);
        }
        let dir = [path.join("sounds").join("voices"), path.join("voices"), path.to_path_buf()]
            .into_iter()
            .find(|dir| VoiceClip::ALL.iter().any(|clip| dir.join(clip.file_name()).is_file()))
            .unwrap_or_else(|| path.join("sounds").join("voices"));
        for clip in VoiceClip::ALL {
            let file = dir.join(clip.file_name());
            if file.is_file() {
                library.clips.insert(clip, ClipSource::File(file));
            } else {
                missing.push((clip, format!("{} not found", file.display())));
            }
        }
        (library, missing)
    }

    /// Whether `clip` has a recording.
    pub fn has(&self, clip: VoiceClip) -> bool {
        self.clips.contains_key(&clip)
    }
}

/// Plays a sound file to the end on the default output.
pub trait SoundFilePlayer: Send + Sync {
    fn play_file(&self, path: &Path) -> std::io::Result<()>;
}

/// An external program and its leading arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalProgram {
    pub program: PathBuf,
    pub args: Vec<String>,
}

/// The first of `names` found as an executable file in the `PATH`-style list `search_path`.
pub fn find_program(names: &[&str], search_path: &std::ffi::OsStr) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    names.iter().find_map(|name| {
        std::env::split_paths(search_path)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
    })
}

/// Players tried for the prompts, in order: PipeWire, FFmpeg, mpg123, PulseAudio.
const PLAYERS: [(&str, &[&str]); 4] = [
    ("pw-play", &[]),
    ("ffplay", &["-nodisp", "-autoexit", "-loglevel", "quiet"]),
    ("mpg123", &["-q"]),
    ("paplay", &[]),
];

/// A prompt player running an external program that blocks until the file has played.
#[derive(Debug, Clone)]
pub struct ProcessPlayer(pub ExternalProgram);

impl ProcessPlayer {
    /// The first player program installed on `search_path`.
    pub fn find(search_path: &std::ffi::OsStr) -> Option<ProcessPlayer> {
        PLAYERS.iter().find_map(|(name, args)| {
            let program = find_program(&[name], search_path)?;
            Some(ProcessPlayer(ExternalProgram { program, args: args.iter().map(|a| (*a).to_owned()).collect() }))
        })
    }

    /// The command that plays `path`.
    pub fn command(&self, path: &Path) -> Command {
        let mut command = Command::new(&self.0.program);
        command.args(&self.0.args).arg(path).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        command
    }
}

impl SoundFilePlayer for ProcessPlayer {
    fn play_file(&self, path: &Path) -> std::io::Result<()> {
        let status = self.command(path).status()?;
        if status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other(format!("{} exited with {status}", self.0.program.display())))
        }
    }
}

/// The parts of [`Voice`].
pub struct VoiceParts {
    pub clips: ClipLibrary,
    pub player: Option<Arc<dyn SoundFilePlayer>>,
    /// Where prompts extracted from the installer are written for the player (`temp` in the documents folder).
    pub temp_dir: PathBuf,
}

/// The voice prompts, shared by the API handlers. `None` inside when voice is off: prompts
/// play only under the Windows rules, and the service without hardware loops sends nothing to a sound device.
///
/// The second field holds the notification texts waiting to be spoken, in request order.
#[derive(Clone)]
pub struct Voice(Option<Arc<VoiceParts>>, Arc<Mutex<VecDeque<String>>>);

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

impl Voice {
    /// Voice that never sounds.
    pub fn off() -> Voice {
        Voice(None, Arc::default())
    }

    pub fn new(parts: VoiceParts) -> Voice {
        Voice(Some(Arc::new(parts)), Arc::default())
    }

    /// The service's voice: on only under the Windows rules with the hardware loops running;
    /// prompts from `srs_install`, played through the first player program found on `PATH`.
    pub fn for_service(windows: bool, hardware: bool, srs_install: Option<&Path>, documents: &Path) -> Voice {
        if !(windows && hardware) {
            return Voice::off();
        }
        let search_path = std::env::var_os("PATH").unwrap_or_default();
        let clips = match srs_install {
            Some(path) => {
                let (clips, missing) = ClipLibrary::import(path);
                for (clip, error) in missing {
                    tracing::warn!(clip = %clip.file_name(), %error, "SRS voice prompt not imported");
                }
                clips
            }
            None => ClipLibrary::empty(),
        };
        let voice = Voice::new(VoiceParts {
            clips,
            player: ProcessPlayer::find(&search_path).map(|p| Arc::new(p) as Arc<dyn SoundFilePlayer>),
            temp_dir: documents.join("temp"),
        });
        voice.start_speak_out_monitor(Box::new(GoogleSpeech));
        voice
    }

    /// Queues `message` to be spoken. The caller has checked the Voice Notifications setting.
    pub fn request_speak_out(&self, message: String) {
        self.1.lock().unwrap_or_else(|e| e.into_inner()).push_back(message);
    }

    /// The texts waiting to be spoken, oldest first.
    #[cfg(test)]
    pub(crate) fn queued(&self) -> Vec<String> {
        self.1.lock().unwrap_or_else(|e| e.into_inner()).iter().cloned().collect()
    }

    /// Starts the thread that speaks the queued texts: every 0.2 s it speaks those queued at that moment, one
    /// after the other. It ends when the last handle of this voice is dropped, or when the temporary
    /// directory cannot be made.
    fn start_speak_out_monitor(&self, transport: Box<dyn SpeechTransport>) {
        let Some(parts) = self.0.as_ref().map(Arc::downgrade) else { return };
        let queue = Arc::downgrade(&self.1);
        let monitor = move || loop {
            let (Some(parts), Some(queue)) = (Weak::upgrade(&parts), Weak::upgrade(&queue)) else { return };
            if let Err(error) = speak_queued(&parts, &queue, transport.as_ref()) {
                tracing::error!(%error, "voice notifications stopped");
                return;
            }
            drop((parts, queue));
            std::thread::sleep(std::time::Duration::from_millis(200));
        };
        if let Err(error) = std::thread::Builder::new().name("threadSpeakOut".into()).spawn(monitor) {
            tracing::error!(%error, "voice notifications not started");
        }
    }

    /// Plays `clip` to the end. Without a recording or a player nothing sounds and the error is
    /// logged.
    pub fn play(&self, clip: VoiceClip) {
        if let Some(parts) = &self.0 {
            parts.play(clip);
        }
    }

    /// Plays `clip` on a thread of its own.
    pub fn play_background(&self, clip: VoiceClip) {
        let Some(parts) = self.0.clone() else { return };
        if let Err(error) = std::thread::Builder::new().name("threadPlaySound".into()).spawn(move || parts.play(clip)) {
            tracing::debug!(%error, "error playing sound");
        }
    }
}

impl VoiceParts {
    fn play(&self, clip: VoiceClip) {
        let played = match (self.clips.clips.get(&clip), &self.player) {
            (Some(source), Some(player)) => self.play_source(source, clip, player.as_ref()),
            (None, _) => Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no recording imported")),
            (Some(_), None) => Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no player program installed")),
        };
        if let Err(error) = played {
            tracing::debug!(clip = %clip.file_name(), %error, "error playing sound");
        }
    }

    fn play_source(&self, source: &ClipSource, clip: VoiceClip, player: &dyn SoundFilePlayer) -> std::io::Result<()> {
        match source {
            ClipSource::File(path) => player.play_file(path),
            ClipSource::Bytes(bytes) => {
                std::fs::create_dir_all(&self.temp_dir)?;
                let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
                let path = self.temp_dir.join(format!("voice-{}-{n}-{}", std::process::id(), clip.file_name()));
                std::fs::write(&path, bytes.as_slice())?;
                let played = player.play_file(&path);
                if let Err(error) = std::fs::remove_file(&path) {
                    tracing::debug!(%error, "remove tmp file error");
                }
                played
            }
        }
    }

    /// Speaks `message`: the speech goes to a file with a random name of 20 letters in the temporary directory,
    /// is played and the file removed. Each of the three steps is tried whatever became of the one before it,
    /// and its failure is only logged. The error returned is that of making the temporary directory.
    fn speak_out(&self, message: &str, transport: &dyn SpeechTransport) -> std::io::Result<()> {
        use rand::Rng;
        const LETTERS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
        let mut rng = rand::thread_rng();
        let mut name: String = (0..20).map(|_| char::from(LETTERS[rng.gen_range(0..LETTERS.len())])).collect();
        name.push_str(".mp3");
        if !self.temp_dir.is_dir() {
            std::fs::create_dir(&self.temp_dir)?;
        }
        let path = self.temp_dir.join(name);
        if let Err(error) = save_speech(message, &path, transport) {
            tracing::debug!(%error, "error generating tts");
        }
        let played = match &self.player {
            Some(player) => player.play_file(&path),
            None => Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no player program installed")),
        };
        if let Err(error) = played {
            tracing::debug!(%error, "playsound error");
        }
        if let Err(error) = std::fs::remove_file(&path) {
            tracing::debug!(%error, "remove tmp file error");
        }
        Ok(())
    }
}

/// One pass of the monitor: speaks the texts that are queued when it starts, oldest first. A text leaves the
/// queue once it has been spoken; texts queued meanwhile wait for the next pass.
fn speak_queued(
    parts: &VoiceParts,
    queue: &Mutex<VecDeque<String>>,
    transport: &dyn SpeechTransport,
) -> std::io::Result<()> {
    let count = queue.lock().unwrap_or_else(|e| e.into_inner()).len();
    for _ in 0..count {
        let Some(message) = queue.lock().unwrap_or_else(|e| e.into_inner()).front().cloned() else { break };
        parts.speak_out(&message, transport)?;
        queue.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
    }
    Ok(())
}

/// Where Google's text-to-speech service takes its requests.
const SPEECH_URL: &str = "https://translate.google.com/_/TranslateWebserverUi/data/batchexecute";

/// The headers of a text-to-speech request (the HTTP client adds the host and the body length).
const SPEECH_HEADERS: [(&str, &str); 4] = [
    ("Accept-Encoding", "identity"),
    ("Referer", "http://translate.google.com/"),
    (
        "User-Agent",
        "Mozilla/5.0 (Windows NT 10.0; WOW64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/47.0.2526.106 \
         Safari/537.36",
    ),
    ("Content-Type", "application/x-www-form-urlencoded;charset=utf-8"),
];

/// The name of the text-to-speech procedure, in the request and on the answer line that carries the audio.
const SPEECH_RPC: &str = "jQ1olc";

/// The most characters the service speaks in one request.
const SPEECH_PART_CHARS: usize = 100;

/// Punctuation after which the tone of speech changes.
const TONE_MARKS: [char; 4] = ['?', '!', '\u{ff1f}', '\u{ff01}'];

/// Punctuation a long text is always split at (and which is dropped): `¡()[]¿…‥،;—。，、：` and the line feed.
const BREAKS: [char; 16] = [
    '\u{a1}', '(', ')', '[', ']', '\u{bf}', '\u{2026}', '\u{2025}', '\u{60c}', ';', '\u{2014}', '\u{3002}', '\u{ff0c}',
    '\u{3001}', '\u{ff1a}', '\n',
];

/// The digit zero of every decimal digit run of Unicode 15.1; the nine code points after each are its 1 to 9.
const DECIMAL_ZEROS: [u32; 68] = [
    0x0030, 0x0660, 0x06F0, 0x07C0, 0x0966, 0x09E6, 0x0A66, 0x0AE6, 0x0B66, 0x0BE6, 0x0C66, 0x0CE6, 0x0D66, 0x0DE6,
    0x0E50, 0x0ED0, 0x0F20, 0x1040, 0x1090, 0x17E0, 0x1810, 0x1946, 0x19D0, 0x1A80, 0x1A90, 0x1B50, 0x1BB0, 0x1C40,
    0x1C50, 0xA620, 0xA8D0, 0xA900, 0xA9D0, 0xA9F0, 0xAA50, 0xABF0, 0xFF10, 0x104A0, 0x10D30, 0x11066, 0x110F0,
    0x11136, 0x111D0, 0x112F0, 0x11450, 0x114D0, 0x11650, 0x116C0, 0x11730, 0x118E0, 0x11950, 0x11C50, 0x11D50,
    0x11DA0, 0x11F50, 0x16A60, 0x16AC0, 0x16B50, 0x1D7CE, 0x1D7D8, 0x1D7E2, 0x1D7EC, 0x1D7F6, 0x1E140, 0x1E2F0,
    0x1E4F0, 0x1E950, 0x1FBF0,
];

/// One request to the text-to-speech service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeechRequest {
    pub url: &'static str,
    pub headers: &'static [(&'static str, &'static str)],
    /// The form body; ASCII only.
    pub body: String,
}

/// The answer to a [`SpeechRequest`]: the status code and the whole body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeechResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// Sends the text-to-speech requests.
pub trait SpeechTransport: Send + Sync {
    /// POSTs `request` and reads the whole answer, whatever its status; `Err` when none was received.
    fn post(&self, request: &SpeechRequest) -> Result<SpeechResponse, String>;
}

/// Google's text-to-speech service over HTTPS: a connection of its own per request, no timeout, up to 30
/// redirects.
pub struct GoogleSpeech;

impl SpeechTransport for GoogleSpeech {
    fn post(&self, request: &SpeechRequest) -> Result<SpeechResponse, String> {
        let client = reqwest::blocking::Client::builder()
            .timeout(None)
            .tcp_user_timeout(None)
            .redirect(reqwest::redirect::Policy::limited(30))
            .build()
            .map_err(|e| e.to_string())?;
        let mut call = client.post(request.url);
        for (name, value) in request.headers {
            call = call.header(*name, *value);
        }
        let response = call.body(request.body.clone()).send().map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        let body = response.bytes().map_err(|e| e.to_string())?.to_vec();
        Ok(SpeechResponse { status, body })
    }
}

/// Writes the speech of `message` to `path` as MP3. An empty message fails before the file is made. Otherwise
/// the file is created first; the message is split into parts, one request per part is sent in order and
/// the audio of each answer is appended as it arrives, so a failure leaves what was written until then.
fn save_speech(message: &str, path: &Path, transport: &dyn SpeechTransport) -> Result<(), String> {
    if message.is_empty() {
        return Err("No text to speak".into());
    }
    let mut file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let requests: Vec<SpeechRequest> = speech_parts(message).iter().map(|part| speech_request(part)).collect();
    if requests.is_empty() {
        return Err("No text to send to TTS API".into());
    }
    for request in &requests {
        let response = transport.post(request).map_err(|error| format!("Failed to connect: {error}"))?;
        if (400..600).contains(&response.status) {
            return Err(format!("{} from TTS API", response.status));
        }
        for line in response.body.split(|byte| matches!(byte, b'\n' | b'\r')) {
            let line = std::str::from_utf8(line).map_err(|e| e.to_string())?;
            if line.contains(SPEECH_RPC) {
                let audio = speech_audio(line).map_err(|error| {
                    format!("{} from TTS API: no audio stream in response ({error})", response.status)
                })?;
                file.write_all(&audio).map_err(|e| e.to_string())?;
            }
        }
    }
    file.flush().map_err(|e| e.to_string())
}

/// The request that speaks `text` (at most [`SPEECH_PART_CHARS`] characters) in English at normal speed. The
/// body is the form field `f.req`, percent-encoded, followed by `&`. Its value is the JSON
/// `[[["jQ1olc",<parameter>,null,"generic"]]]` whose `<parameter>` is, as a JSON string, the JSON
/// `[<text>,"en",null,"null"]`; both are written without spaces and with everything outside printable ASCII
/// as `\uXXXX`.
fn speech_request(text: &str) -> SpeechRequest {
    let parameter = format!("[{},\"en\",null,\"null\"]", json_string(text));
    let rpc = format!("[[[\"{SPEECH_RPC}\",{},null,\"generic\"]]]", json_string(&parameter));
    let mut body = String::from("f.req=");
    for byte in rpc.bytes() {
        if byte.is_ascii_alphanumeric() || b"_.-~/".contains(&byte) {
            body.push(char::from(byte));
        } else {
            body.push_str(&format!("%{byte:02X}"));
        }
    }
    body.push('&');
    SpeechRequest { url: SPEECH_URL, headers: &SPEECH_HEADERS, body }
}

/// `text` as a JSON string in ASCII: short escapes for `"`, `\`, backspace, form feed, line feed, carriage
/// return and tab; `\uXXXX` (lower-case hex, per UTF-16 unit) for everything else outside space to `~`.
fn json_string(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ' '..='~' => out.push(c),
            _ => {
                for unit in c.encode_utf16(&mut [0; 2]) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    out.push('"');
    out
}

/// The audio of an answer line that names the speech procedure: the base64 text between the first
/// `jQ1olc","[\"` and the last `\"]` after it.
fn speech_audio(line: &str) -> Result<Vec<u8>, String> {
    let opening = format!("{SPEECH_RPC}\",\"[\\\"");
    let start = line.find(&opening).ok_or("no audio")? + opening.len();
    let end = line[start..].rfind("\\\"]").ok_or("no audio")?;
    let encoded = &line[start..start + end];
    if !encoded.is_ascii() {
        return Err("audio is not ASCII".into());
    }
    decode_base64(encoded.as_bytes())
}

/// Lenient base64: characters outside the alphabet are skipped. Decoding ends at the `=` that completes a
/// group of four begun with two or three characters (the `=` need not be adjacent); any other `=` is skipped.
/// Without such an end the text must not stop inside a group.
fn decode_base64(text: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let (mut position, mut left, mut pads) = (0_usize, 0_u8, 0_usize);
    for &byte in text {
        let value = match byte {
            b'=' => {
                if position >= 2 {
                    pads += 1;
                    if position + pads >= 4 {
                        return Ok(out);
                    }
                }
                continue;
            }
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => continue,
        };
        pads = 0;
        match position {
            0 => left = value,
            1 => {
                out.push(left << 2 | value >> 4);
                left = value & 0x0f;
            }
            2 => {
                out.push(left << 4 | value >> 2);
                left = value & 0x03;
            }
            _ => {
                out.push(left << 6 | value);
                left = 0;
            }
        }
        position = (position + 1) % 4;
    }
    match position {
        0 => Ok(out),
        1 => Err("invalid base64: one character more than a multiple of four".into()),
        _ => Err("incorrect base64 padding".into()),
    }
}

/// White space as trimmed from texts: Unicode white space and the four separator controls U+001C to U+001F.
fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// Whether `c` is `letter` (a lower-case ASCII letter) ignoring case; the long s (U+017F) is also an `s`.
fn is_letter(c: char, letter: char) -> bool {
    c.to_ascii_lowercase() == letter || (letter == 's' && c == '\u{17f}')
}

/// The parts `message` is spoken in, each at most [`SPEECH_PART_CHARS`] characters.
///
/// The trimmed message is first rewritten: a space is put after every tone mark, a hyphen followed by a line
/// feed is removed (a word cut at the end of a line), the period after `dr`, `jr`, `mr`, `mrs`, `ms`, `msgr`,
/// `prof`, `sr` and `st` is removed and `Esq.` becomes `Esquire` (both in any letter case). A text that then
/// fits is one part. A longer one is split
/// - at the character after a tone mark (a line feed there counts as the punctuation below),
/// - at a period or comma followed by a space, unless a period and a letter stand right before it,
/// - at a colon, unless a decimal digit stands right before it,
/// - at any of [`BREAKS`],
///
/// and pieces still too long are cut at their last space within the limit, or at the limit. Pieces of only
/// punctuation and white space are dropped, the others trimmed.
fn speech_parts(message: &str) -> Vec<String> {
    let mut text = message.trim_matches(is_space).to_owned();
    for mark in TONE_MARKS {
        text = text.replace(mark, &format!("{mark} "));
    }
    text = text.replace("-\n", "");
    for abbreviation in ["dr", "jr", "mr", "mrs", "ms", "msgr", "prof", "sr", "st"] {
        let chars: Vec<char> = text.chars().collect();
        let letters = abbreviation.chars().count();
        let follows = |at: usize| {
            at >= letters && chars[at - letters..at].iter().zip(abbreviation.chars()).all(|(c, l)| is_letter(*c, l))
        };
        text = chars.iter().enumerate().filter(|(at, c)| !(**c == '.' && follows(*at))).map(|(_, c)| *c).collect();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut text = String::new();
    let mut at = 0;
    while at < chars.len() {
        let esq = chars.len() - at >= 4
            && is_letter(chars[at], 'e')
            && is_letter(chars[at + 1], 's')
            && is_letter(chars[at + 2], 'q')
            && chars[at + 3] == '.';
        if esq {
            text.push_str("Esquire");
            at += 4;
        } else {
            text.push(chars[at]);
            at += 1;
        }
    }

    // Pieces without anything to speak: only punctuation and ASCII white space.
    let clean = |pieces: Vec<String>| -> Vec<String> {
        let silent = |c: char| {
            TONE_MARKS.contains(&c)
                || BREAKS.contains(&c)
                || matches!(c, '.' | ',' | ':' | ' ' | '\t' | '\r' | '\u{b}' | '\u{c}')
        };
        pieces
            .iter()
            .filter(|piece| !piece.chars().all(silent))
            .map(|piece| piece.trim_matches(is_space).to_owned())
            .collect()
    };
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= SPEECH_PART_CHARS {
        return clean(vec![text]);
    }

    let mut pieces = Vec::new();
    let mut piece = String::new();
    let mut at = 0;
    while at < chars.len() {
        let c = chars[at];
        let before = at.checked_sub(1).map(|i| chars[i]);
        let after_tone_mark = before.is_some_and(|b| TONE_MARKS.contains(&b));
        // Letters here include the dotted capital I, the dotless i, the long s and the Kelvin sign.
        let after_dotted_letter = at >= 2
            && chars[at - 2] == '.'
            && (chars[at - 1].is_ascii_alphabetic()
                || matches!(chars[at - 1], '\u{130}' | '\u{131}' | '\u{17f}' | '\u{212a}'));
        let after_digit =
            before.is_some_and(|b| DECIMAL_ZEROS.iter().any(|zero| (*zero..*zero + 10).contains(&u32::from(b))));
        let separator = if after_tone_mark && c != '\n' {
            1
        } else if matches!(c, '.' | ',') && chars.get(at + 1) == Some(&' ') && !after_dotted_letter {
            2
        } else if c == ':' {
            usize::from(!after_digit)
        } else {
            usize::from(BREAKS.contains(&c))
        };
        if separator == 0 {
            piece.push(c);
            at += 1;
        } else {
            pieces.push(std::mem::take(&mut piece));
            at += separator;
        }
    }
    pieces.push(piece);

    let mut parts = Vec::new();
    for piece in clean(pieces) {
        let piece: Vec<char> = piece.chars().collect();
        let mut rest = piece.as_slice();
        loop {
            if rest.first() == Some(&' ') {
                rest = &rest[1..];
            }
            if rest.len() <= SPEECH_PART_CHARS {
                parts.push(rest.iter().collect::<String>());
                break;
            }
            let cut = rest[..SPEECH_PART_CHARS].iter().rposition(|c| *c == ' ').unwrap_or(SPEECH_PART_CHARS);
            parts.push(rest[..cut].iter().collect());
            rest = &rest[cut..];
        }
    }
    parts.retain(|part| !part.is_empty());
    parts
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct PlayOptions {
    /// Play to the end before answering; otherwise in the background.
    wait: bool,
}

/// The voice routes: `POST /api/v1/voice/clips/{name}` (`?wait=true` to answer once played) and
/// `POST /api/v1/voice` with `{"message": <notification text>}`.
pub fn api_routes() -> Router<ApiContext> {
    Router::new().route("/api/v1/voice/clips/{name}", post(play_clip)).route("/api/v1/voice", post(request_speak_out))
}

#[derive(Deserialize)]
struct SpeakOutRequest {
    message: String,
}

/// A notification text to speak. The Voice Notifications setting is read now: the text is queued when it is
/// on and dropped when it is off.
async fn request_speak_out(
    State(ctx): State<ApiContext>,
    axum::Json(request): axum::Json<SpeakOutRequest>,
) -> Result<StatusCode, ApiError> {
    let setting = ctx
        .state
        .config
        .read()
        .native_settings_tree()
        .item_owned("APP")
        .and_then(|app| app.item_owned("voice_notifications"))
        .map_err(|detail| ApiError::internal(format!("KeyError: {detail}")))?;
    let enabled = openrigcue_cueing::str2bool(&openrigcue_cueing::PyValue::from(&setting))
        .map_err(|error| ApiError::internal(format!("{}: {}", error.exception().name(), error.message())))?;
    if enabled {
        ctx.voice.request_speak_out(request.message);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// The IntelliTune wizard's prompts.
async fn play_clip(
    State(ctx): State<ApiContext>,
    UrlPath(name): UrlPath<String>,
    Query(options): Query<PlayOptions>,
) -> Result<StatusCode, ApiError> {
    let clip = VoiceClip::from_name(&name).ok_or_else(|| ApiError::not_found(format!("no voice prompt {name:?}")))?;
    if options.wait {
        let voice = ctx.voice.clone();
        tokio::task::spawn_blocking(move || voice.play(clip)).await.map_err(|e| ApiError::internal(e.to_string()))?;
        Ok(StatusCode::NO_CONTENT)
    } else {
        ctx.voice.play_background(clip);
        Ok(StatusCode::ACCEPTED)
    }
}

/// Direct UI operations over the same service functions as the test route adapters.
pub(crate) async fn direct_call(
    ctx: crate::api::ApiContext,
    call: &crate::api::direct::Call<'_>,
) -> Result<Option<axum::response::Response>, crate::api::ApiError> {
    use openrigcue_ui::backend::Method;
    let response = match (call.method, call.segments.as_slice()) {
        (Method::Post, ["voice", "clips", parameter0]) => axum::response::IntoResponse::into_response(
            play_clip(State(ctx), UrlPath(call.parameter(parameter0)?), call.query()?).await,
        ),
        (Method::Post, ["voice"]) => {
            axum::response::IntoResponse::into_response(request_speak_out(State(ctx), call.json()?).await)
        }
        _ => return Ok(None),
    };
    Ok(Some(response))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    struct Recorder(mpsc::Sender<String>);

    impl SoundFilePlayer for Recorder {
        fn play_file(&self, path: &Path) -> std::io::Result<()> {
            let bytes = std::fs::read(path)?;
            let _ = self.0.send(format!(
                "play:{}:{}",
                path.file_name().unwrap().to_string_lossy(),
                String::from_utf8_lossy(&bytes)
            ));
            Ok(())
        }
    }

    fn voice(clips: ClipLibrary, temp: &Path) -> (Voice, mpsc::Receiver<String>) {
        let (tx, rx) = mpsc::channel();
        let parts = VoiceParts { clips, player: Some(Arc::new(Recorder(tx))), temp_dir: temp.to_path_buf() };
        (Voice::new(parts), rx)
    }

    fn install(dir: &Path, clips: &[VoiceClip]) {
        let voices = dir.join("sounds").join("voices");
        std::fs::create_dir_all(&voices).unwrap();
        for clip in clips {
            std::fs::write(voices.join(clip.file_name()), clip.name()).unwrap();
        }
    }

    /// A prompt `sounds/voices/<name>.mp3` is played from the imported installation.
    #[test]
    fn plays_installed_prompts() {
        let tmp = tempfile::tempdir().unwrap();
        install(tmp.path(), &[VoiceClip::Number(9), VoiceClip::IntellituneStarting]);
        let (clips, missing) = ClipLibrary::import(tmp.path());
        assert!(clips.has(VoiceClip::Number(9)));
        assert_eq!(missing.len(), 12);
        let (voice, rx) = voice(clips, &tmp.path().join("temp"));
        voice.play(VoiceClip::IntellituneStarting);
        assert_eq!(rx.try_recv().unwrap(), "play:intellitune_starting.mp3:intellitune_starting");
        voice.play_background(VoiceClip::Number(9));
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), "play:9.mp3:9");
    }

    /// Without a recording nothing sounds: there is no text-to-speech, the countdown numbers included.
    #[test]
    fn prompts_without_recordings_are_silent() {
        let tmp = tempfile::tempdir().unwrap();
        let (voice, rx) = voice(ClipLibrary::empty(), tmp.path());
        voice.play(VoiceClip::Number(10));
        voice.play(VoiceClip::IntellituneCaptured);
        assert!(rx.try_recv().is_err());
    }

    /// Prompts extracted from the installer go through a temporary file in `<documents>/temp`, removed after
    /// playing.
    #[test]
    fn extracted_prompts_use_a_temporary_file() {
        let tmp = tempfile::tempdir().unwrap();
        let mut clips = ClipLibrary::empty();
        clips.clips.insert(VoiceClip::Number(1), ClipSource::Bytes(Arc::new(b"one".to_vec())));
        let temp = tmp.path().join("temp");
        let (voice, rx) = voice(clips, &temp);
        voice.play(VoiceClip::Number(1));
        let played = rx.try_recv().unwrap();
        assert!(played.starts_with("play:voice-") && played.ends_with("-1.mp3:one"), "{played}");
        assert_eq!(std::fs::read_dir(&temp).unwrap().count(), 0);
    }

    /// `Voice::off` (non-Windows platform or no hardware loops) never plays.
    #[test]
    fn voice_off() {
        let tmp = tempfile::tempdir().unwrap();
        Voice::off().play(VoiceClip::Number(3));
        assert!(Voice::for_service(false, true, None, tmp.path()).0.is_none());
        assert!(Voice::for_service(true, false, None, tmp.path()).0.is_none());
    }

    /// The installer's prompts are read under `sounds/voices/`; an unreadable installer reports every prompt.
    #[test]
    fn unreadable_installer() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("SRS-Setup.exe");
        std::fs::write(&file, b"MZ not an installer").unwrap();
        let (clips, missing) = ClipLibrary::import(&file);
        assert!(!clips.has(VoiceClip::Number(0)));
        assert_eq!(missing.len(), VoiceClip::ALL.len());
    }

    /// Player discovery and command lines; nothing is run.
    #[test]
    fn external_programs() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let make = |name: &str, mode: u32| {
            use std::os::unix::fs::PermissionsExt;
            let path = bin.join(name);
            std::fs::write(&path, "").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        make("pw-play", 0o644);
        make("mpg123", 0o755);
        let search = std::env::join_paths([tmp.path().join("empty"), bin.clone()]).unwrap();
        let player = ProcessPlayer::find(&search).unwrap();
        let command = player.command(Path::new("/srs/sounds/voices/5.mp3"));
        assert_eq!(command.get_program(), bin.join("mpg123"), "a non-executable pw-play is skipped");
        assert_eq!(command.get_args().collect::<Vec<_>>(), ["-q", "/srs/sounds/voices/5.mp3"]);
        assert!(ProcessPlayer::find(&std::ffi::OsString::new()).is_none());
    }

    /// Answers the text-to-speech requests from a list and records them; nothing reaches the network.
    struct FakeSpeech {
        sent: Mutex<Vec<SpeechRequest>>,
        answers: Mutex<VecDeque<Result<SpeechResponse, String>>>,
    }

    impl FakeSpeech {
        fn new(answers: Vec<Result<SpeechResponse, String>>) -> FakeSpeech {
            FakeSpeech { sent: Mutex::default(), answers: Mutex::new(answers.into()) }
        }

        fn bodies(&self) -> Vec<String> {
            self.sent.lock().unwrap().iter().map(|request| request.body.clone()).collect()
        }
    }

    impl SpeechTransport for FakeSpeech {
        fn post(&self, request: &SpeechRequest) -> Result<SpeechResponse, String> {
            self.sent.lock().unwrap().push(request.clone());
            self.answers.lock().unwrap().pop_front().unwrap_or_else(|| Err("no answer prepared".into()))
        }
    }

    /// A 200 answer shaped like the service's, whose audio line carries `base64`.
    fn answer(base64: &str) -> Result<SpeechResponse, String> {
        let body = format!(
            ")]}}'\n\n184\n[[\"wrb.fr\",\"jQ1olc\",\"[\\\"{base64}\\\"]\",null,null,null,\"generic\"]]\n57\n\
             [[\"di\",31],[\"af.httprm\",30,\"-1\",29]]\n27\n[[\"e\",4,null,null,220]]\n"
        );
        Ok(SpeechResponse { status: 200, body: body.into_bytes() })
    }

    /// The body that asks for `text` (plain letters and spaces only).
    fn body_of(text: &str) -> String {
        format!(
            "f.req=%5B%5B%5B%22jQ1olc%22%2C%22%5B%5C%22{}%5C%22%2C%5C%22en%5C%22%2Cnull%2C%5C%22null%5C%22%5D%22%2C\
             null%2C%22generic%22%5D%5D%5D&",
            text.replace(' ', "%20")
        )
    }

    /// `play:<file name>:<content>` of a played temporary file, split; the name is 20 letters and `.mp3`.
    fn played(rx: &mpsc::Receiver<String>) -> String {
        let played = rx.try_recv().unwrap();
        let (name, content) = played.strip_prefix("play:").unwrap().split_once(':').unwrap();
        let letters = name.strip_suffix(".mp3").unwrap();
        assert!(letters.len() == 20 && letters.chars().all(|c| c.is_ascii_alphabetic()), "{name}");
        content.to_owned()
    }

    /// A queued notification: one POST to Google's text-to-speech service with the text in the form body, the
    /// audio of the answer written under the temporary directory (made when missing), played, then removed.
    #[test]
    fn spoken_notification_is_generated_played_and_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let temp = tmp.path().join("temp");
        let (voice, rx) = voice(ClipLibrary::empty(), &temp);
        let speech = FakeSpeech::new(vec![answer("aGVsbG8=")]);
        voice.request_speak_out("Calibration has finished".into());
        assert_eq!(voice.queued(), ["Calibration has finished"]);
        speak_queued(voice.0.as_ref().unwrap(), &voice.1, &speech).unwrap();
        assert_eq!(
            *speech.sent.lock().unwrap(),
            [SpeechRequest {
                url: "https://translate.google.com/_/TranslateWebserverUi/data/batchexecute",
                headers: &[
                    ("Accept-Encoding", "identity"),
                    ("Referer", "http://translate.google.com/"),
                    (
                        "User-Agent",
                        "Mozilla/5.0 (Windows NT 10.0; WOW64) AppleWebKit/537.36 (KHTML, like Gecko) \
                         Chrome/47.0.2526.106 Safari/537.36"
                    ),
                    ("Content-Type", "application/x-www-form-urlencoded;charset=utf-8"),
                ],
                body:
                    "f.req=%5B%5B%5B%22jQ1olc%22%2C%22%5B%5C%22Calibration%20has%20finished%5C%22%2C%5C%22en%5C%22%2C\
                       null%2C%5C%22null%5C%22%5D%22%2Cnull%2C%22generic%22%5D%5D%5D&"
                        .into(),
            }]
        );
        assert_eq!(played(&rx), "hello");
        assert!(rx.try_recv().is_err());
        assert_eq!(std::fs::read_dir(&temp).unwrap().count(), 0);
        assert!(voice.queued().is_empty());
    }

    /// Texts are spoken oldest first, one after the other; one queued during a pass waits for the next pass.
    #[test]
    fn queued_notifications_are_spoken_in_order() {
        struct Late(Voice, FakeSpeech);
        impl SpeechTransport for Late {
            fn post(&self, request: &SpeechRequest) -> Result<SpeechResponse, String> {
                if self.1.sent.lock().unwrap().is_empty() {
                    self.0.request_speak_out("late".into());
                }
                self.1.post(request)
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let (voice, rx) = voice(ClipLibrary::empty(), tmp.path());
        let speech = Late(voice.clone(), FakeSpeech::new(vec![answer("b25l"), answer("dHdv"), answer("bGF0ZQ==")]));
        voice.request_speak_out("first".into());
        voice.request_speak_out("second".into());
        speak_queued(voice.0.as_ref().unwrap(), &voice.1, &speech).unwrap();
        assert_eq!(speech.1.bodies(), [body_of("first"), body_of("second")]);
        assert_eq!([played(&rx), played(&rx)], ["one", "two"]);
        assert_eq!(voice.queued(), ["late"]);
        speak_queued(voice.0.as_ref().unwrap(), &voice.1, &speech).unwrap();
        assert_eq!(speech.1.bodies()[2], body_of("late"));
        assert_eq!(played(&rx), "late");
        assert!(voice.queued().is_empty());
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    /// A failed generation still plays and removes the file, which holds what was written before the failure:
    /// nothing when the first request fails, the earlier parts otherwise. The text leaves the queue all the same.
    #[test]
    fn failed_generation_still_plays_and_removes() {
        let tmp = tempfile::tempdir().unwrap();
        let (voice, rx) = voice(ClipLibrary::empty(), tmp.path());
        let long = format!("{}. {}", "a".repeat(60), "b".repeat(60));
        let failures = [
            Err("connection refused".to_owned()),
            Ok(SpeechResponse { status: 500, body: b"jQ1olc\",\"[\\\"aGVsbG8=\\\"]".to_vec() }),
            Ok(SpeechResponse { status: 200, body: b"[[\"wrb.fr\",\"jQ1olc\",null,null,null,[3]]]\n".to_vec() }),
            answer("a"),
        ];
        for failure in failures {
            let speech = FakeSpeech::new(vec![failure.clone()]);
            voice.request_speak_out("Calibration has finished".into());
            speak_queued(voice.0.as_ref().unwrap(), &voice.1, &speech).unwrap();
            assert_eq!(played(&rx), "", "{failure:?}");
            assert!(voice.queued().is_empty());
            assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);

            let speech = FakeSpeech::new(vec![answer("aGVsbG8="), failure.clone()]);
            voice.request_speak_out(long.clone());
            speak_queued(voice.0.as_ref().unwrap(), &voice.1, &speech).unwrap();
            assert_eq!(speech.bodies(), [body_of(&"a".repeat(60)), body_of(&"b".repeat(60))]);
            assert_eq!(played(&rx), "hello", "{failure:?}");
            assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
        }

        // What each failure reports.
        let path = tmp.path().join("speech.mp3");
        let report = |message: &str, answers: Vec<Result<SpeechResponse, String>>| {
            save_speech(message, &path, &FakeSpeech::new(answers)).unwrap_err()
        };
        assert_eq!(report("x", vec![Err("connection refused".into())]), "Failed to connect: connection refused");
        assert_eq!(report("x", vec![Ok(SpeechResponse { status: 403, body: Vec::new() })]), "403 from TTS API");
        assert!(report("x", vec![Ok(SpeechResponse { status: 200, body: b"jQ1olc".to_vec() })])
            .starts_with("200 from TTS API: no audio stream in response"));
        // An answer without an audio line is no failure and writes nothing; so is a status below 400.
        assert_eq!(
            save_speech("x", &path, &FakeSpeech::new(vec![Ok(SpeechResponse { status: 204, body: Vec::new() })])),
            Ok(())
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"");
        std::fs::remove_file(&path).unwrap();

        // Nothing to speak: an empty text makes no file and sends nothing; one of only punctuation makes the file.
        let speech = FakeSpeech::new(Vec::new());
        assert_eq!(save_speech("", &path, &speech).unwrap_err(), "No text to speak");
        assert!(!path.exists());
        assert_eq!(save_speech(" ?!., ", &path, &speech).unwrap_err(), "No text to send to TTS API");
        assert!(path.exists() && speech.sent.lock().unwrap().is_empty());
        voice.request_speak_out(String::new());
        speak_queued(voice.0.as_ref().unwrap(), &voice.1, &speech).unwrap();
        assert!(rx.try_recv().is_err(), "there is no file to play");
        assert!(voice.queued().is_empty());

        // Without a player program the file is still generated and removed.
        let silent =
            Voice::new(VoiceParts { clips: ClipLibrary::empty(), player: None, temp_dir: tmp.path().join("t") });
        let speech = FakeSpeech::new(vec![answer("aGVsbG8=")]);
        silent.request_speak_out("Calibration has finished".into());
        speak_queued(silent.0.as_ref().unwrap(), &silent.1, &speech).unwrap();
        assert_eq!(speech.bodies(), [body_of("Calibration has finished")]);
        assert_eq!(std::fs::read_dir(tmp.path().join("t")).unwrap().count(), 0);
    }

    /// The temporary directory is made without its parents; when that fails the text stays queued and the
    /// monitor is told to stop.
    #[test]
    fn missing_documents_folder_stops_the_monitor() {
        let tmp = tempfile::tempdir().unwrap();
        let (voice, rx) = voice(ClipLibrary::empty(), &tmp.path().join("missing").join("temp"));
        let speech = FakeSpeech::new(vec![answer("aGVsbG8=")]);
        voice.request_speak_out("Calibration has finished".into());
        assert!(speak_queued(voice.0.as_ref().unwrap(), &voice.1, &speech).is_err());
        assert_eq!(voice.queued(), ["Calibration has finished"]);
        assert!(rx.try_recv().is_err() && speech.sent.lock().unwrap().is_empty());
    }

    /// A text of more than 100 characters is sent in parts, one request each, and the audio of the answers is
    /// appended to the one file in order.
    #[test]
    fn long_text_is_split_into_parts() {
        let text = "Hello, it's 24.5 degrees in the U.K. today. $20,000,000. It's now 6:30 which means: morning \
                    missing:space and Lorem? Ipsum! Baconipsumdolorsitametflankcornedbee (done); Dr. Smith, Esq. \
                    agrees.";
        let parts = [
            "Hello",
            "it's 24.5 degrees in the U.K. today",
            "$20,000,000",
            "It's now 6:30 which means",
            "morning missing",
            "space and Lorem?",
            "Ipsum!",
            "Baconipsumdolorsitametflankcornedbee",
            "done",
            "Dr Smith",
            "Esquire agrees.",
        ];
        assert_eq!(speech_parts(text), parts);

        let tmp = tempfile::tempdir().unwrap();
        let (voice, rx) = voice(ClipLibrary::empty(), tmp.path());
        let audio = [
            "b25l", "dHdv", "IHdvcmxk", "aGVsbG8=", "b25l", "dHdv", "IHdvcmxk", "aGVsbG8=", "b25l", "dHdv", "bGF0ZQ==",
        ];
        let speech = FakeSpeech::new(audio.iter().map(|a| answer(a)).collect());
        voice.request_speak_out(text.into());
        speak_queued(voice.0.as_ref().unwrap(), &voice.1, &speech).unwrap();
        let bodies = speech.bodies();
        assert_eq!(bodies.len(), parts.len());
        assert_eq!(bodies[0], body_of("Hello"));
        assert_eq!(
            bodies[1],
            "f.req=%5B%5B%5B%22jQ1olc%22%2C%22%5B%5C%22it%27s%2024.5%20degrees%20in%20the%20U.K.%20today%5C%22%2C\
             %5C%22en%5C%22%2Cnull%2C%5C%22null%5C%22%5D%22%2Cnull%2C%22generic%22%5D%5D%5D&"
        );
        assert_eq!(bodies[10], body_of("Esquire agrees."));
        assert_eq!(played(&rx), "onetwo worldhelloonetwo worldhelloonetwolate");

        // Pieces without punctuation to split at are cut at the last space within 100 characters, else at 100.
        let words = ["word"; 30].join(" ");
        assert_eq!(speech_parts(&words), [["word"; 20].join(" "), ["word"; 10].join(" ")]);
        assert_eq!(speech_parts(&"x".repeat(250)), ["x".repeat(100), "x".repeat(100), "x".repeat(50)]);
        assert_eq!(speech_parts(&"x".repeat(100)), ["x".repeat(100)]);
    }

    /// The rewriting before a text is sent, and the escaping of the request body.
    #[test]
    fn text_is_rewritten_and_escaped() {
        assert_eq!(speech_parts("  lorem!ipsum?\n"), ["lorem! ipsum?"]);
        assert_eq!(speech_parts("jr. sr. dr."), ["jr sr dr"]);
        assert_eq!(speech_parts("MRS. Smith, Esq. and eSQ. Jones st.."), ["MRS Smith, Esquire and Esquire Jones st."]);
        assert_eq!(speech_parts("test-\ning"), ["testing"]);
        assert!(speech_parts("?!., \u{ff1f}").is_empty());
        assert_eq!(
            speech_request("a\"b\\c\n\u{e9}\u{1f600}~/\u{7f}").body,
            "f.req=%5B%5B%5B%22jQ1olc%22%2C%22%5B%5C%22a%5C%5C%5C%22b%5C%5C%5C%5Cc%5C%5Cn%5C%5Cu00e9%5C%5Cud83d\
             %5C%5Cude00~/%5C%5Cu007f%5C%22%2C%5C%22en%5C%22%2Cnull%2C%5C%22null%5C%22%5D%22%2Cnull%2C%22generic%22\
             %5D%5D%5D&"
        );
    }

    /// The audio of an answer line: lenient base64 between the first opening and the last closing.
    #[test]
    fn audio_is_extracted_from_the_answer_line() {
        assert_eq!(
            speech_audio(r#"[["wrb.fr","jQ1olc","[\"aGVsbG8=\"]",null,null,null,"generic"]]"#).unwrap(),
            b"hello"
        );
        assert_eq!(speech_audio(r#"jQ1olc","[\"aGVs\"] bG8=\"]"#).unwrap(), b"hello");
        assert_eq!(speech_audio(r#"jQ1olc","[\"aGVsbG8\"]"#).unwrap_err(), "incorrect base64 padding");
        assert!(speech_audio(r#"jQ1olc","[\"aGVsb\"]"#).is_err());
        assert_eq!(speech_audio(r#"jQ1olc","[\"=aG=Vs*bG8\"]"#).unwrap_err(), "incorrect base64 padding");
        assert_eq!(speech_audio(r#"jQ1olc","[\"aGVsbA=\"]"#).unwrap_err(), "incorrect base64 padding");
        assert_eq!(speech_audio(r#"jQ1olc","[\"aGVsbA==\"]"#).unwrap(), b"hell");
        assert_eq!(speech_audio(r#"jQ1olc","[\"aGVsbA= =bG8=\"]"#).unwrap(), b"hell");
        assert_eq!(speech_audio(r#"jQ1olc","[\"\"]"#).unwrap(), b"");
        assert!(speech_audio(r#"jQ1olc","[\"aGVsbG8="#).is_err());
        assert!(speech_audio("jQ1olc\",\"[\\\"\u{e9}\\\"]").is_err());
    }
}
