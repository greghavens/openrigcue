//! Small runtime-loaded libpulse output. The stream is pinned to an explicit sink;
//! it cannot be moved to the system default when that sink disappears.
#![allow(unsafe_code)]

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::{
    block::{BlockRenderer, FRAMES},
    linux::Plan,
    Mixer, OutputError,
};

type Handle = *mut c_void;
const OUTPUT_BUFFER_BLOCKS: usize = 4;
const MAX_READY_DISPATCHES: usize = 64;
/// `PA_STREAM_EARLY_REQUESTS`: the server asks for audio as soon as `minreq` (one processing block) is
/// free. Without it the request size is `tlength - 2 * minreq`, two blocks: PipeWire then runs the
/// stream at a 2048-frame quantum and both blocks are mixed back to back, so starts and stops are
/// admitted every 43 ms instead of at each 1024-frame block (queued commands are applied
/// at the start of every block).
const EARLY_REQUESTS: u32 = 0x4000;

/// A playing stream asks for audio every few milliseconds. PipeWire does not end a pinned
/// stream whose sink is removed: it stays ready, is never linked again (not even to a new
/// sink of the same name) and simply stops asking. A stream that took nothing for this long
/// has lost its device.
const STALL_LIMIT: Duration = Duration::from_secs(2);

/// Whether a stream that last took audio at `progress` has stalled by `now`.
fn stalled(progress: Instant, now: Instant) -> bool {
    now.saturating_duration_since(progress) >= STALL_LIMIT
}

/// A Pulse iteration may only read a request or queue a deferred write. Finish
/// ready protocol work before sleeping, while bounding a continuously busy peer.
fn dispatch_ready(mut iterate: impl FnMut() -> c_int) -> bool {
    for _ in 0..MAX_READY_DISPATCHES {
        match iterate() {
            value if value < 0 => return false,
            0 => break,
            _ => {}
        }
    }
    true
}

#[repr(C)]
struct SampleSpec {
    format: c_int,
    rate: u32,
    channels: u8,
}
#[repr(C)]
struct ChannelMap {
    channels: u8,
    map: [c_int; 32],
}
#[repr(C)]
#[derive(Debug, PartialEq, Eq)]
struct BufferAttr {
    maxlength: u32,
    tlength: u32,
    prebuf: u32,
    minreq: u32,
    fragsize: u32,
}

impl BufferAttr {
    fn playback(channels: u8) -> Self {
        let block = FRAMES as u32 * u32::from(channels) * 4;
        Self {
            maxlength: u32::MAX,
            // 1024-frame blocks in a four-block ring.
            // A time-based 20 ms target is smaller than a single 48 kHz block;
            // PipeWire can then repeatedly underrun and insert silent gaps.
            tlength: block * OUTPUT_BUFFER_BLOCKS as u32,
            prebuf: u32::MAX,
            minreq: block,
            fragsize: u32::MAX,
        }
    }
}

// PulseAudio's public ABI, pulse/{mainloop,context,stream,channelmap}.h.
struct Api {
    _library: libloading::Library,
    mainloop_new: unsafe extern "C" fn() -> Handle,
    mainloop_free: unsafe extern "C" fn(Handle),
    mainloop_get_api: unsafe extern "C" fn(Handle) -> Handle,
    mainloop_iterate: unsafe extern "C" fn(Handle, c_int, *mut c_int) -> c_int,
    context_new: unsafe extern "C" fn(Handle, *const c_char) -> Handle,
    context_connect: unsafe extern "C" fn(Handle, *const c_char, u32, Handle) -> c_int,
    context_get_state: unsafe extern "C" fn(Handle) -> c_int,
    context_errno: unsafe extern "C" fn(Handle) -> c_int,
    context_disconnect: unsafe extern "C" fn(Handle),
    context_unref: unsafe extern "C" fn(Handle),
    strerror: unsafe extern "C" fn(c_int) -> *const c_char,
    channel_map_parse: unsafe extern "C" fn(*mut ChannelMap, *const c_char) -> *mut ChannelMap,
    stream_new: unsafe extern "C" fn(Handle, *const c_char, *const SampleSpec, *const ChannelMap) -> Handle,
    stream_connect_playback:
        unsafe extern "C" fn(Handle, *const c_char, *const BufferAttr, u32, Handle, Handle) -> c_int,
    stream_get_state: unsafe extern "C" fn(Handle) -> c_int,
    stream_get_device_name: unsafe extern "C" fn(Handle) -> *const c_char,
    stream_writable_size: unsafe extern "C" fn(Handle) -> usize,
    stream_write:
        unsafe extern "C" fn(Handle, *const c_void, usize, Option<unsafe extern "C" fn(Handle)>, i64, c_int) -> c_int,
    stream_disconnect: unsafe extern "C" fn(Handle) -> c_int,
    stream_unref: unsafe extern "C" fn(Handle),
}

impl Api {
    fn load() -> Result<Self, OutputError> {
        // SAFETY: library lifetime is retained in Api; each symbol uses its public C ABI.
        unsafe {
            let library = libloading::Library::new("libpulse.so.0").map_err(device_error)?;
            macro_rules! sym {
                ($name:literal) => {
                    *library.get(concat!($name, "\0").as_bytes()).map_err(device_error)?
                };
            }
            Ok(Self {
                mainloop_new: sym!("pa_mainloop_new"),
                mainloop_free: sym!("pa_mainloop_free"),
                mainloop_get_api: sym!("pa_mainloop_get_api"),
                mainloop_iterate: sym!("pa_mainloop_iterate"),
                context_new: sym!("pa_context_new"),
                context_connect: sym!("pa_context_connect"),
                context_get_state: sym!("pa_context_get_state"),
                context_errno: sym!("pa_context_errno"),
                context_disconnect: sym!("pa_context_disconnect"),
                context_unref: sym!("pa_context_unref"),
                strerror: sym!("pa_strerror"),
                channel_map_parse: sym!("pa_channel_map_parse"),
                stream_new: sym!("pa_stream_new"),
                stream_connect_playback: sym!("pa_stream_connect_playback"),
                stream_get_state: sym!("pa_stream_get_state"),
                stream_get_device_name: sym!("pa_stream_get_device_name"),
                stream_writable_size: sym!("pa_stream_writable_size"),
                stream_write: sym!("pa_stream_write"),
                stream_disconnect: sym!("pa_stream_disconnect"),
                stream_unref: sym!("pa_stream_unref"),
                _library: library,
            })
        }
    }
}

fn device_error(error: impl std::fmt::Display) -> OutputError {
    OutputError::Device(error.to_string())
}

/// All libpulse handles stay on this one thread, including teardown.
struct Connection {
    api: Api,
    mainloop: Handle,
    context: Handle,
    stream: Handle,
}

impl Drop for Connection {
    fn drop(&mut self) {
        // SAFETY: handles are uniquely owned; stream is released before its context/mainloop.
        unsafe {
            if !self.stream.is_null() {
                (self.api.stream_disconnect)(self.stream);
                (self.api.stream_unref)(self.stream);
            }
            if !self.context.is_null() {
                (self.api.context_disconnect)(self.context);
                (self.api.context_unref)(self.context);
            }
            if !self.mainloop.is_null() {
                (self.api.mainloop_free)(self.mainloop);
            }
        }
    }
}

impl Connection {
    fn error(&self) -> OutputError {
        // SAFETY: context is live and pa_strerror returns a static string.
        unsafe {
            let text = (self.api.strerror)((self.api.context_errno)(self.context));
            device_error(if text.is_null() {
                "PulseAudio error".into()
            } else {
                CStr::from_ptr(text).to_string_lossy()
            })
        }
    }

    fn iterate(&self) -> Result<(), OutputError> {
        // SAFETY: only this owning thread accesses the mainloop.
        if dispatch_ready(|| unsafe { (self.api.mainloop_iterate)(self.mainloop, 0, ptr::null_mut()) }) {
            Ok(())
        } else {
            Err(self.error())
        }
    }

    fn wait_ready(&self, stream: bool) -> Result<(), OutputError> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            self.iterate()?;
            // SAFETY: requested handle has been created and is live.
            let state = unsafe {
                if stream {
                    (self.api.stream_get_state)(self.stream)
                } else {
                    (self.api.context_get_state)(self.context)
                }
            };
            if state == if stream { 2 } else { 4 } {
                return Ok(());
            }
            if state >= if stream { 3 } else { 5 } {
                return Err(self.error());
            }
            if Instant::now() >= deadline {
                return Err(device_error("sound server connection timed out"));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn open(plan: &Plan, corked: bool) -> Result<Self, OutputError> {
        let api = Api::load()?;
        let sink = CString::new(plan.sink.as_str()).map_err(device_error)?;
        let positions = CString::new(plan.positions.join(",")).map_err(device_error)?;
        // SAFETY: strings/spec/map/attributes remain live for calls, which copy them.
        unsafe {
            let mainloop = (api.mainloop_new)();
            if mainloop.is_null() {
                return Err(device_error("cannot allocate sound server mainloop"));
            }
            let mut this = Self { api, mainloop, context: ptr::null_mut(), stream: ptr::null_mut() };
            this.context = (this.api.context_new)((this.api.mainloop_get_api)(mainloop), c"OpenRigCue".as_ptr());
            if this.context.is_null() {
                return Err(device_error("cannot allocate sound server context"));
            }
            // NOAUTOSPAWN: enumeration/output must not launch another sound server.
            if (this.api.context_connect)(this.context, ptr::null(), 1, ptr::null_mut()) < 0 {
                return Err(this.error());
            }
            this.wait_ready(false)?;
            let mut map = ChannelMap { channels: 0, map: [0; 32] };
            if (this.api.channel_map_parse)(&mut map, positions.as_ptr()).is_null() {
                return Err(OutputError::NoFormat);
            }
            let spec = SampleSpec {
                format: if cfg!(target_endian = "little") { 5 } else { 6 },
                rate: plan.rate,
                channels: map.channels,
            };
            this.stream = (this.api.stream_new)(this.context, c"Shakers".as_ptr(), &spec, &map);
            if this.stream.is_null() {
                return Err(this.error());
            }
            let attr = BufferAttr::playback(map.channels);
            // DONT_MOVE | NO_REMIX_CHANNELS: no fallback/migration or synthesized channels.
            let flags = 0x0200 | 0x0020 | EARLY_REQUESTS | u32::from(corked);
            if (this.api.stream_connect_playback)(
                this.stream,
                sink.as_ptr(),
                &attr,
                flags,
                ptr::null_mut(),
                ptr::null_mut(),
            ) < 0
            {
                return Err(this.error());
            }
            this.wait_ready(true)?;
            let actual = (this.api.stream_get_device_name)(this.stream);
            if actual.is_null() || CStr::from_ptr(actual) != sink.as_c_str() {
                return Err(device_error("sound server did not select the requested output"));
            }
            Ok(this)
        }
    }

    /// Writes what the stream asks for; the number of frames written (0 when it asks for nothing).
    fn render(
        &self,
        plan: &Plan,
        mixer: &Mutex<Mixer>,
        renderer: &mut BlockRenderer,
        samples: &mut Vec<f32>,
    ) -> Result<usize, OutputError> {
        self.iterate()?;
        // SAFETY: stream is owned here; writable size is checked before constructing/copying samples.
        unsafe {
            if (self.api.stream_get_state)(self.stream) != 2 {
                return Err(self.error());
            }
            let bytes = (self.api.stream_writable_size)(self.stream);
            if bytes == usize::MAX {
                return Err(self.error());
            }
            let channels = plan.layout.len();
            // A high-rate sink may consume more than one processing block between
            // polls. BlockRenderer still processes each block independently.
            let frames = (bytes / (4 * channels)).min(FRAMES * OUTPUT_BUFFER_BLOCKS);
            if frames == 0 {
                return Ok(0);
            }
            samples.resize(frames * channels, 0.0);
            renderer.render(mixer, samples);
            if (self.api.stream_write)(self.stream, samples.as_ptr().cast(), samples.len() * 4, None, 0, 0) < 0 {
                return Err(self.error());
            }
            Ok(frames)
        }
    }
}

pub(super) struct Playback {
    stop: Option<mpsc::Sender<()>>,
    worker: Option<JoinHandle<()>>,
}
impl Playback {
    /// `lost` is raised when the stream ends by itself: its sink was removed (the stream is not
    /// moved to another one; PulseAudio ends it, PipeWire leaves it stalled, see [`STALL_LIMIT`])
    /// or the sound server connection failed.
    pub(super) fn open(plan: Plan, mixer: Arc<Mutex<Mixer>>, lost: Arc<AtomicBool>) -> Result<Self, OutputError> {
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (stop_tx, stop_rx) = mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("shaker_pulse".into())
            .spawn(move || {
                let connection = match Connection::open(&plan, false) {
                    Ok(connection) => connection,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                };
                if ready_tx.send(Ok(())).is_err() {
                    return;
                }
                let mut samples = Vec::new();
                let mut renderer = BlockRenderer::new(plan.matrix.clone());
                let mut progress = Instant::now();
                loop {
                    let written = connection.render(&plan, &mixer, &mut renderer, &mut samples).and_then(|frames| {
                        let now = Instant::now();
                        if frames > 0 {
                            progress = now;
                        } else if stalled(progress, now) {
                            return Err(device_error("the sound device stopped taking audio"));
                        }
                        Ok(())
                    });
                    if let Err(error) = written {
                        tracing::error!(%error, "shaker sound device disconnected");
                        lost.store(true, Ordering::SeqCst);
                        break;
                    }
                    match stop_rx.recv_timeout(Duration::from_millis(3)) {
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        _ => break,
                    }
                }
            })
            .map_err(device_error)?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self { stop: Some(stop_tx), worker: Some(worker) }),
            Ok(Err(error)) => {
                let _ = worker.join();
                Err(error)
            }
            Err(error) => {
                let _ = worker.join();
                Err(device_error(error))
            }
        }
    }
}
impl Drop for Playback {
    fn drop(&mut self) {
        self.stop.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playback_buffer_retains_four_native_blocks_in_every_device_layout() {
        for channels in [2, 4, 6, 8] {
            let attr = BufferAttr::playback(channels);
            let bytes_per_frame = u32::from(channels) * 4;
            assert_eq!(attr.tlength / bytes_per_frame, 4096);
            assert_eq!(attr.minreq / bytes_per_frame, 1024);
            assert_eq!(attr.maxlength, u32::MAX);
            assert_eq!(attr.prebuf, u32::MAX);
            assert_eq!(attr.fragsize, u32::MAX);
        }
    }

    #[test]
    fn ready_protocol_work_is_drained_without_waiting_or_starving_shutdown() {
        let mut calls = 0;
        let steps = [1, 3, 1, 0];
        assert!(dispatch_ready(|| {
            let result = steps[calls];
            calls += 1;
            result
        }));
        assert_eq!(calls, 4, "deferred work is handled before the next sleep");
        let mut calls = 0;
        assert!(dispatch_ready(|| {
            calls += 1;
            1
        }));
        assert_eq!(calls, MAX_READY_DISPATCHES, "a busy peer cannot hide stop requests forever");
        let mut calls = 0;
        assert!(!dispatch_ready(|| {
            calls += 1;
            if calls == 2 {
                -1
            } else {
                1
            }
        }));
        assert_eq!(calls, 2, "protocol errors terminate this connection");
    }

    /// A live test for the isolated null-sink harness. Its monitor checks sample
    /// continuity as well as channel routing; this never selects a host default.
    #[test]
    fn explicit_private_sink_speaker_cadence() {
        use crate::shakers::{
            levels::GroupLevels,
            rig::Voice,
            samples::{SampleBank, SampleFolder, SampleKey},
        };
        let Ok(name) = std::env::var("OPENRIGCUE_PULSE_TEST_SINK") else { return };
        assert!(name.starts_with("openrigcue_test_"), "only the private test sink is permitted");
        assert!(std::env::var("PULSE_SERVER").unwrap().starts_with("unix:/tmp/"));
        let sink = super::super::linux::sink(&name).unwrap();
        let plan = super::super::linux::plan(&sink).unwrap();
        assert_eq!(plan.layout.len(), 6, "the monitor fixture captures six channels");
        let mixer = Arc::new(Mutex::new(Mixer::new(plan.rate, SampleBank::synthesized(), GroupLevels::empty())));
        let output = Playback::open(plan, Arc::clone(&mixer), Arc::new(AtomicBool::new(false))).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        for channel in 0..6 {
            let mut levels = [0.0; 6];
            levels[channel] = 1.0;
            super::super::lock(&mixer).play_direct(
                Voice::SpeakerTest,
                SampleKey::new(SampleFolder::Transmission, 1),
                levels,
            );
            std::thread::sleep(Duration::from_millis(250));
        }
        // Let the last queued block reach the monitor before disconnecting.
        std::thread::sleep(Duration::from_secs(2));
        drop(output);
    }
    #[test]
    fn a_stream_that_stops_taking_audio_has_lost_its_device() {
        let start = Instant::now();
        assert!(!stalled(start, start));
        assert!(!stalled(start, start + STALL_LIMIT - Duration::from_millis(1)));
        assert!(stalled(start, start + STALL_LIMIT));
        assert!(!stalled(start + Duration::from_secs(1), start), "a clock reading before the progress is no stall");
        // Far above the few milliseconds between requests of a four-block (85 ms at 48 kHz) buffer.
        assert!(STALL_LIMIT >= Duration::from_secs(1));
    }

    /// Private-server lifecycle check. Removing the pinned sink raises `lost` (at once where the server
    /// ends the stream, after [`STALL_LIMIT`] where it leaves it stalled); the missing sink cannot be
    /// opened; when a sink of that name returns, a new stream plays on it; an output closed by its owner
    /// raises nothing. Creates and removes only its own null sink.
    #[test]
    fn explicit_private_sink_removal_raises_lost() {
        use crate::shakers::{levels::GroupLevels, samples::SampleBank};
        let Ok(name) = std::env::var("OPENRIGCUE_PULSE_LIFECYCLE_SINK") else { return };
        assert!(name.starts_with("openrigcue_test_"), "only a private test sink is permitted");
        assert!(std::env::var("PULSE_SERVER").unwrap().starts_with("unix:/tmp/"));
        let pactl = |args: &[&str]| {
            let output = std::process::Command::new("pactl").args(args).output().unwrap();
            assert!(output.status.success(), "pactl {args:?}: {}", String::from_utf8_lossy(&output.stderr));
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        };
        let sink_name = format!("sink_name={name}");
        let load = || {
            let module = pactl(&[
                "load-module",
                "module-null-sink",
                &sink_name,
                "channels=6",
                "channel_map=front-left,front-right,front-center,lfe,rear-left,rear-right",
                "rate=48000",
            ]);
            let deadline = Instant::now() + Duration::from_secs(5);
            while super::super::linux::sink(&name).is_err() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            module
        };
        let raised = |lost: &AtomicBool, within: Duration| {
            let deadline = Instant::now() + within;
            while !lost.load(Ordering::SeqCst) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            lost.load(Ordering::SeqCst)
        };
        let mixer = Arc::new(Mutex::new(Mixer::new(48_000, SampleBank::synthesized(), GroupLevels::empty())));

        let module = load();
        let plan = super::super::linux::plan(&super::super::linux::sink(&name).unwrap()).unwrap();
        assert_eq!(plan.layout.len(), 6);
        let lost = Arc::new(AtomicBool::new(false));
        let output = Playback::open(plan.clone(), Arc::clone(&mixer), Arc::clone(&lost)).unwrap();
        assert!(!raised(&lost, STALL_LIMIT + Duration::from_secs(1)), "a playing stream is not lost");
        pactl(&["unload-module", &module]);
        let removed = Instant::now();
        assert!(raised(&lost, Duration::from_secs(5)), "removing the sink must end the pinned stream");
        eprintln!("verified lost flag {:?} after the sink was removed", removed.elapsed());
        // Independent of the flag: the stream was not moved to another sink (it uses
        // `PA_STREAM_DONT_MOVE`; PulseAudio ends it, PipeWire leaves it on no sink).
        let sinks: Vec<String> = pactl(&["list", "short", "sinks"])
            .lines()
            .filter_map(|l| l.split('\t').next().map(str::to_owned))
            .collect();
        let inputs = pactl(&["list", "short", "sink-inputs"]);
        eprintln!("sinks {sinks:?}; sink inputs after the removal: {inputs:?}");
        assert!(
            inputs.lines().all(|l| l.split('\t').nth(1).is_none_or(|sink| !sinks.iter().any(|s| s == sink))),
            "the lost stream plays on another sink: {inputs}"
        );
        let started = Instant::now();
        drop(output);
        assert!(started.elapsed() < Duration::from_secs(1), "an ended worker joins at once");
        assert!(
            Playback::open(plan.clone(), Arc::clone(&mixer), Arc::new(AtomicBool::new(false))).is_err(),
            "the missing sink is not replaced by another output"
        );

        let module = load();
        let plan = super::super::linux::plan(&super::super::linux::sink(&name).unwrap()).unwrap();
        let lost = Arc::new(AtomicBool::new(false));
        let output = Playback::open(plan, Arc::clone(&mixer), Arc::clone(&lost)).unwrap();
        assert!(!raised(&lost, STALL_LIMIT + Duration::from_secs(1)), "the returned sink plays again");
        drop(output);
        assert!(!lost.load(Ordering::SeqCst), "closing an output is not a loss");
        pactl(&["unload-module", &module]);
        eprintln!("verified reopening on the returned sink and a silent owner close");
    }

    /// Private-server lifecycle check: the sound server exiting (a PipeWire or PulseAudio restart)
    /// raises `lost` on the stream it was playing. Only runs inside a private PID namespace, where
    /// the given process can only be the harness's own server.
    #[test]
    fn explicit_private_server_exit_raises_lost() {
        use crate::shakers::{levels::GroupLevels, samples::SampleBank};
        let Ok(pid) = std::env::var("OPENRIGCUE_PULSE_LIFECYCLE_SERVER_PID") else { return };
        let name = std::env::var("OPENRIGCUE_PULSE_TEST_SINK").unwrap();
        assert!(name.starts_with("openrigcue_test_"), "only the private test sink is permitted");
        assert!(std::env::var("PULSE_SERVER").unwrap().starts_with("unix:/tmp/"));
        assert_eq!(std::fs::read_to_string("/proc/1/comm").unwrap().trim(), "bwrap", "private PID namespace only");
        let server = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap();
        assert!(["pipewire", "pulseaudio"].contains(&server.trim()), "the harness's sound server: {server}");
        let plan = super::super::linux::plan(&super::super::linux::sink(&name).unwrap()).unwrap();
        let mixer = Arc::new(Mutex::new(Mixer::new(plan.rate, SampleBank::synthesized(), GroupLevels::empty())));
        let lost = Arc::new(AtomicBool::new(false));
        let output = Playback::open(plan.clone(), Arc::clone(&mixer), Arc::clone(&lost)).unwrap();
        std::thread::sleep(Duration::from_millis(500));
        assert!(!lost.load(Ordering::SeqCst));
        assert!(std::process::Command::new("kill").arg(&pid).status().unwrap().success());
        let ended = Instant::now();
        let deadline = ended + STALL_LIMIT + Duration::from_secs(3);
        while !lost.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(lost.load(Ordering::SeqCst), "a stream of an exited sound server is lost");
        eprintln!("verified lost flag {:?} after the sound server exited", ended.elapsed());
        drop(output);
        assert!(Playback::open(plan, mixer, Arc::new(AtomicBool::new(false))).is_err(), "no server, no output");
    }

    /// Private-server check of the admission cadence. The mixer renders one 1024-frame block at a time and
    /// applies queued starts and stops at the start of each block, so with the stream alone on its
    /// sink the server must ask for single blocks. A request of two blocks makes both render back to
    /// back: a sound started and released within 43 ms is then mixed whole or not at all.
    #[test]
    fn explicit_private_sink_requests_single_native_blocks() {
        let Ok(name) = std::env::var("OPENRIGCUE_PULSE_REQUEST_SINK") else { return };
        assert!(name.starts_with("openrigcue_test_"), "only the private test sink is permitted");
        assert!(std::env::var("PULSE_SERVER").unwrap().starts_with("unix:/tmp/"));
        let sink = super::super::linux::sink(&name).unwrap();
        let plan = super::super::linux::plan(&sink).unwrap();
        let connection = Connection::open(&plan, false).unwrap();
        let mixer = Mutex::new(Mixer::new(
            plan.rate,
            crate::shakers::samples::SampleBank::default(),
            crate::shakers::levels::GroupLevels::empty(),
        ));
        let mut samples = Vec::new();
        let mut renderer = BlockRenderer::new(plan.matrix.clone());
        let mut requests = Vec::new();
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(2) {
            let frames = connection.render(&plan, &mixer, &mut renderer, &mut samples).unwrap();
            if frames > 0 {
                requests.push((started.elapsed(), frames));
            }
            std::thread::sleep(Duration::from_millis(3));
        }
        // The first requests fill the empty four-block buffer.
        let steady: Vec<_> = requests.iter().filter(|(at, _)| *at > Duration::from_millis(500)).collect();
        let mut sizes = std::collections::BTreeMap::new();
        for (_, frames) in &steady {
            *sizes.entry(*frames).or_insert(0usize) += 1;
        }
        let total: usize = steady.iter().map(|(_, frames)| *frames).sum();
        eprintln!("steady requests by frame count: {sizes:?}; {total} frames in 1.5 s at {} Hz", plan.rate);
        assert!(total as f64 > f64::from(plan.rate) * 1.5 * 0.9, "the stream must keep its sink fed");
        let single = sizes.get(&FRAMES).copied().unwrap_or(0);
        assert!(single * 10 >= steady.len() * 9, "requests must be single {FRAMES}-frame blocks: {sizes:?}");
    }

    /// Opt-in host check: the real render/write path submits silence to a corked
    /// stream on a private null sink. Never uncorks or opens a physical output.
    #[test]
    fn explicit_null_sink_route_without_playback() {
        let Ok(name) = std::env::var("OPENRIGCUE_PULSE_TEST_SINK") else { return };
        assert!(name.starts_with("openrigcue_test_"), "only the private test sink is permitted");
        let sink = super::super::linux::sink(&name).unwrap();
        let plan = super::super::linux::plan(&sink).unwrap();
        let connection = Connection::open(&plan, true).unwrap();
        let mixer = Mutex::new(Mixer::new(
            plan.rate,
            crate::shakers::samples::SampleBank::default(),
            crate::shakers::levels::GroupLevels::empty(),
        ));
        let mut samples = Vec::new();
        let mut renderer = BlockRenderer::new(plan.matrix.clone());
        let deadline = Instant::now() + Duration::from_secs(2);
        while samples.is_empty() && Instant::now() < deadline {
            connection.render(&plan, &mixer, &mut renderer, &mut samples).unwrap();
            if samples.is_empty() {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        assert!(!samples.is_empty(), "the real stream_write path must be reached");
        assert!(samples.iter().all(|sample| *sample == 0.0), "only silence may enter the test sink");
        // SAFETY: Connection exclusively owns the live libpulse stream on this thread.
        unsafe {
            assert_eq!(
                (connection.api.stream_get_state)(connection.stream),
                2,
                "write accepted and stream remains ready"
            );
            assert_ne!((connection.api.stream_writable_size)(connection.stream), usize::MAX);
        }
        eprintln!(
            "verified silent production write to corked sink={} rate={} channels={} samples={} map={:?}",
            plan.sink,
            plan.rate,
            plan.layout.len(),
            samples.len(),
            plan.positions
        );
        let started = Instant::now();
        drop(connection); // Actual production disconnect/unref/mainloop-free path.
        assert!(started.elapsed() < Duration::from_secs(1), "teardown must not wait for playback");
        drop(Connection::open(&plan, true).unwrap());
        let mut missing = plan;
        missing.sink = "openrigcue_test_nonexistent".into();
        assert!(Connection::open(&missing, true).is_err(), "missing output must not use default");
    }
}
