//! The Windows mapping objects the application itself owns, replicated into the game's prefix by
//! `openrigcue-bridge`.
//!
//! A listener's create-or-open of a named mapping creates a zero-filled page-file section
//! when none exists (and opens it otherwise), and a mapping object the listener keeps holds the section
//! alive, with whatever the game last wrote into it, until the listener drops it
//! or the application exits. Those objects live in this process ([`Registry`]); every connected
//! bridge mirrors them inside its prefix:
//!
//! - [`Registry::acquire`] is a new mapping object. The first one for a name (`gen` = generation) is sent to every
//!   connected bridge, which creates the section (or opens it, see [`Opening`]), holds it, mirrors it and answers;
//!   the call waits for those answers so the read that follows sees the section. Further
//!   objects for a held name only count references (a new object replacing the previous one keeps
//!   the section alive throughout).
//! - Dropping the last [`HeldSection`] of a name releases it: the bridges close their handle, so the section goes
//!   away unless the game still holds it.
//! - [`Registry::create_once`] is a per-poll mapping: create-or-open, copy, release.
//! - While no bridge holds a section, its content is the last frame read through a bridge, or zeros for a section
//!   created while no bridge ran ([`HeldSection::read`]): the object keeps exactly that after the game
//!   (and with it the prefix) went away. A bridge connecting later creates the section with that content, so a
//!   game started afterwards finds it.
//! - The bridges connect over the pipe relay's TCP port with the route name [`CONTROL_ROUTE`]; a connected bridge
//!   is also how the bridge knows the application is running (the always-served pipes and held sections exist
//!   exactly while it runs). A plain open (`OpenFileMappingW`, or a mirror read) while no bridge is connected
//!   fails ([`Registry::bridge_connected`]): without a prefix there is no game section to open.
//!
//! Control framing: a little-endian `u32` length, then the message (at most [`MAX_MESSAGE`] bytes).
//!
//! | direction | message |
//! |---|---|
//! | to bridge | `H` gen:u64 flags:u8 size:u64 name_len:u16 name [seed: `size` bytes when flags bit 1] — hold; flags bit 0: create when missing |
//! | to bridge | `h` gen:u64 size:u64 name_len:u16 name — create/open PAGE_READONLY and keep FILE_MAP_READ view |
//! | to bridge | `C` gen:u64 size:u64 name_len:u16 name — temporary read-only create/open, copy, release |
//! | to bridge | `R` gen:u64 — release |
//! | to bridge | `T` size:u64 name_len:u16 name — create-or-open, copy, close |
//! | to bridge | `S` — every object held when the bridge connected has been sent |
//! | to Linux | `A` gen:u64 ok:u8 — held (ok 0: the section could not be opened) |
//! | to Linux | `a` / `C` gen:u64 ok:u8 [bytes on success, error text on failure] — read-only operation completed |
//! | to Linux | `D` gen:u64 — released |

use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use super::block_reader::MappingSpec;
use super::mapping::{MappedView, MIRROR_DIR};

/// TCP relay route of the bridges' control connections (the first frame of the connection).
pub const CONTROL_ROUTE: &str = "OpenRigCue:sections";
/// Largest control message (the iRacing map, 1 191 936 bytes, is the largest seed).
pub const MAX_MESSAGE: usize = 4 << 20;
/// How long an acquire or release waits for the connected bridges to answer.
const ANSWER_TIMEOUT: Duration = Duration::from_millis(250);
/// How long one message to a bridge may take to be written.
const WRITE_TIMEOUT: Duration = Duration::from_secs(1);

/// How a mapping object opens the section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opening {
    /// Create-or-open: created zero-filled when missing.
    CreateOrOpen,
    /// Read-only create-or-open: PAGE_READONLY creation, FILE_MAP_READ view.
    CreateReadOnly,
    /// An explicit open of an existing section, without creating it.
    Existing,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One held name.
struct Entry {
    gen: u64,
    spec: MappingSpec,
    opening: Opening,
    refs: usize,
    /// What the object shows while no bridge holds the section.
    retained: Vec<u8>,
    /// Bridges that hold the section (answered `A` with ok 1).
    held_by: HashSet<u64>,
    /// Bridges that answered `A` (held or not).
    answered: HashSet<u64>,
    errors: HashMap<u64, String>,
}

/// A connected bridge.
struct Bridge {
    id: u64,
    writer: BridgeWriter,
}

enum BridgeWriter {
    Tcp(TcpStream),
    #[cfg(test)]
    Metadata(std::sync::mpsc::Sender<Vec<u8>>),
}

impl BridgeWriter {
    fn send(&mut self, message: &[u8]) -> io::Result<()> {
        match self {
            Self::Tcp(stream) => write_message(stream, message),
            #[cfg(test)]
            Self::Metadata(sender) => sender.send(message.to_vec()).map_err(|_| io::ErrorKind::BrokenPipe.into()),
        }
    }
    fn shutdown(&self) {
        match self {
            Self::Tcp(stream) => {
                let _ = stream.shutdown(Shutdown::Both);
            }
            #[cfg(test)]
            Self::Metadata(_) => {}
        }
    }
}

struct PendingCopy {
    size: usize,
    waiting: HashSet<u64>,
    bytes: Option<Vec<u8>>,
    error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RemoteEvent {
    bridge: u64,
    token: u64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct EventReply {
    pub(crate) handle: Option<RemoteEvent>,
    pub(crate) result: u32,
}

struct PendingEvent {
    waiting: HashSet<u64>,
    reply: Option<EventReply>,
}

#[derive(Default)]
struct State {
    entries: HashMap<&'static str, Entry>,
    bridges: Vec<Bridge>,
    /// Releases sent and not yet answered: gen → bridges still to answer.
    releasing: HashMap<u64, HashSet<u64>>,
    copies: HashMap<u64, PendingCopy>,
    events: HashMap<u64, PendingEvent>,
    next_gen: u64,
    next_bridge: u64,
}

/// The mapping objects of this process and the bridges that replicate them.
pub struct Registry {
    dir: PathBuf,
    state: Mutex<State>,
    answered: Condvar,
}

impl Registry {
    /// A registry whose bridges mirror into `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Arc<Registry> {
        Arc::new(Registry { dir: dir.into(), state: Mutex::default(), answered: Condvar::new() })
    }

    /// The registry of this process (mirrors in [`MIRROR_DIR`]), accepting bridges on the [`CONTROL_ROUTE`].
    pub fn global() -> Arc<Registry> {
        static GLOBAL: OnceLock<Arc<Registry>> = OnceLock::new();
        Arc::clone(GLOBAL.get_or_init(|| {
            let registry = Registry::new(MIRROR_DIR);
            let receiver = crate::process_memory::pipe_relay::tcp_route(CONTROL_ROUTE);
            let weak = Arc::downgrade(&registry);
            let spawned = thread::Builder::new().name("held_sections".into()).spawn(move || {
                while let Ok(stream) = receiver.recv() {
                    match weak.upgrade() {
                        Some(registry) => registry.attach(stream),
                        None => return,
                    }
                }
            });
            if let Err(error) = spawned {
                tracing::warn!(%error, "bridge control route not served");
            }
            registry
        }))
    }

    /// The mirror directory the bridges write.
    pub fn mirror_dir(&self) -> &Path {
        &self.dir
    }

    /// Whether a bridge is connected (the game's prefix and its sections are reachable).
    pub fn bridge_connected(&self) -> bool {
        !lock(&self.state).bridges.is_empty()
    }

    /// A new mapping object for `spec` (see the module documentation). `current` is what the listener read when
    /// it made the object from an existing section ([`Opening::Existing`]); a created section starts as zeros.
    pub fn acquire(self: &Arc<Self>, spec: MappingSpec, opening: Opening, current: Option<&[u8]>) -> HeldSection {
        let mut state = lock(&self.state);
        if let Some(entry) = state.entries.get_mut(spec.name) {
            entry.refs += 1;
            return HeldSection { registry: Arc::clone(self), name: spec.name, size: spec.size };
        }
        state.next_gen += 1;
        let gen = state.next_gen;
        let mut retained = current.map(<[u8]>::to_vec).unwrap_or_default();
        retained.resize(spec.size, 0);
        state.entries.insert(
            spec.name,
            Entry {
                gen,
                spec,
                opening,
                refs: 1,
                retained,
                held_by: HashSet::new(),
                answered: HashSet::new(),
                errors: HashMap::new(),
            },
        );
        let message = hold_message(gen, spec, opening, None);
        let waiting: HashSet<u64> = send_all(&mut state, &message);
        let deadline = Instant::now() + ANSWER_TIMEOUT;
        loop {
            let pending = match state.entries.get(spec.name) {
                Some(entry) if entry.gen == gen => {
                    let live: HashSet<u64> = state.bridges.iter().map(|b| b.id).collect();
                    waiting.iter().any(|id| live.contains(id) && !entry.answered.contains(id))
                }
                _ => false,
            };
            let now = Instant::now();
            if !pending || now >= deadline {
                break;
            }
            state = self.answered.wait_timeout(state, deadline - now).unwrap_or_else(PoisonError::into_inner).0;
        }
        drop(state);
        HeldSection { registry: Arc::clone(self), name: spec.name, size: spec.size }
    }

    /// Read-only creation has the same missing-section zero initialization,
    /// but actual CreateFileMapping/MapView failures must not become fake zeros.
    pub fn acquire_read_only(self: &Arc<Self>, spec: MappingSpec) -> io::Result<HeldSection> {
        let held = self.acquire(spec, Opening::CreateReadOnly, None);
        let failure = {
            let state = lock(&self.state);
            let entry = &state.entries[spec.name];
            if entry.held_by.is_empty() {
                entry.errors.values().next().cloned().or_else(|| {
                    state
                        .bridges
                        .iter()
                        .any(|bridge| !entry.answered.contains(&bridge.id))
                        .then(|| "read-only mapping acknowledgement timed out".into())
                })
            } else {
                None
            }
        };
        match failure {
            Some(error) => Err(io::Error::other(error)),
            None => Ok(held),
        }
    }

    /// The read-only temporary's copy is returned by the bridge before the
    /// temporary mapping is dropped; no stale mirror or invented success on error.
    pub fn copy_read_only(&self, spec: MappingSpec) -> io::Result<Vec<u8>> {
        let mut state = lock(&self.state);
        state.next_gen += 1;
        let gen = state.next_gen;
        let mut message = vec![b'C'];
        message.extend_from_slice(&gen.to_le_bytes());
        message.extend_from_slice(&(spec.size as u64).to_le_bytes());
        push_name(&mut message, spec.name);
        let waiting = send_all(&mut state, &message);
        if waiting.is_empty() {
            return Ok(vec![0; spec.size]);
        }
        state.copies.insert(gen, PendingCopy { size: spec.size, waiting, bytes: None, error: None });
        let deadline = Instant::now() + ANSWER_TIMEOUT;
        loop {
            let copy = state.copies.get_mut(&gen).expect("pending copy");
            if let Some(bytes) = copy.bytes.take() {
                state.copies.remove(&gen);
                return Ok(bytes);
            }
            if copy.waiting.is_empty() {
                let error = copy.error.take().unwrap_or_else(|| "bridge disconnected during read-only copy".into());
                state.copies.remove(&gen);
                return Err(io::Error::other(error));
            }
            let now = Instant::now();
            if now >= deadline {
                state.copies.remove(&gen);
                return Err(io::Error::new(io::ErrorKind::TimedOut, "read-only copy acknowledgement timed out"));
            }
            state = self.answered.wait_timeout(state, deadline - now).unwrap_or_else(PoisonError::into_inner).0;
        }
    }

    pub(crate) fn open_event(&self, name: &str) -> Option<EventReply> {
        self.event_request(None, |gen| {
            let mut message = vec![b'E'];
            message.extend_from_slice(&gen.to_le_bytes());
            push_name(&mut message, name);
            message
        })
    }

    pub(crate) fn wait_event(&self, handle: RemoteEvent) -> Option<EventReply> {
        self.event_request(Some(handle.bridge), |gen| {
            let mut message = vec![b'W'];
            message.extend_from_slice(&gen.to_le_bytes());
            message.extend_from_slice(&handle.token.to_le_bytes());
            message
        })
    }

    fn event_request(&self, bridge: Option<u64>, message: impl FnOnce(u64) -> Vec<u8>) -> Option<EventReply> {
        let mut state = lock(&self.state);
        state.next_gen += 1;
        let gen = state.next_gen;
        let message = message(gen);
        let waiting = match bridge {
            None => send_all(&mut state, &message),
            Some(id) => state
                .bridges
                .iter_mut()
                .filter(|candidate| candidate.id == id)
                .filter_map(|candidate| candidate.writer.send(&message).ok().map(|()| candidate.id))
                .collect(),
        };
        state.events.insert(gen, PendingEvent { waiting, reply: None });
        let deadline = Instant::now() + ANSWER_TIMEOUT;
        loop {
            let call = state.events.get(&gen).expect("pending event request");
            // Opening is attempted in each reachable prefix. A null handle in
            // one prefix must not hide the actual event in another.
            if call.waiting.is_empty() || call.reply.is_some_and(|reply| reply.handle.is_some()) {
                return state.events.remove(&gen).and_then(|call| call.reply);
            }
            let now = Instant::now();
            if now >= deadline {
                return state.events.remove(&gen).and_then(|call| call.reply);
            }
            state = self.answered.wait_timeout(state, deadline - now).unwrap_or_else(PoisonError::into_inner).0;
        }
    }

    /// A per-poll create-or-open mapping that is dropped at once: every connected
    /// bridge creates-or-opens the section and closes it again. Returns what the copy saw: the section's current
    /// mirror, or the zeros of a section created just for this copy.
    pub fn create_once(&self, spec: MappingSpec) -> Vec<u8> {
        let mut state = lock(&self.state);
        let mut message = vec![b'T'];
        message.extend_from_slice(&(spec.size as u64).to_le_bytes());
        push_name(&mut message, spec.name);
        send_all(&mut state, &message);
        let connected = !state.bridges.is_empty();
        drop(state);
        let mirrored = if connected { read_mirror(&self.dir, spec) } else { None };
        mirrored.unwrap_or_else(|| vec![0; spec.size])
    }

    /// Serve one bridge's control connection: send it every held object (with the content the object
    /// shows, so a section it has to create starts with it), then read its answers until it disconnects.
    pub fn attach(self: &Arc<Self>, stream: TcpStream) {
        // A bridge that stops reading must not stall the listeners: a write that cannot complete in time drops it.
        if stream.set_write_timeout(Some(WRITE_TIMEOUT)).is_err() {
            return;
        }
        let Ok(mut reader) = stream.try_clone() else { return };
        let Some(id) = self.register_bridge(BridgeWriter::Tcp(stream)) else { return };
        tracing::info!(bridge = id, "bridge connected");
        let registry = Arc::clone(self);
        let spawned = thread::Builder::new().name("held_sections_bridge".into()).spawn(move || {
            while let Ok(Some(message)) = read_message(&mut reader) {
                registry.answer(id, &message);
            }
            registry.detach(id);
        });
        if let Err(error) = spawned {
            tracing::warn!(%error, "bridge control connection not served");
            self.detach(id);
        }
    }

    fn register_bridge(&self, writer: BridgeWriter) -> Option<u64> {
        let mut state = lock(&self.state);
        state.next_bridge += 1;
        let id = state.next_bridge;
        let mut bridge = Bridge { id, writer };
        for entry in state.entries.values() {
            let message = if entry.opening == Opening::CreateReadOnly {
                // A newly created PAGE_READONLY section is zero-filled. It
                // cannot be populated by temporarily granting write access.
                hold_message(entry.gen, entry.spec, Opening::CreateReadOnly, None)
            } else {
                hold_message(entry.gen, entry.spec, Opening::CreateOrOpen, Some(&entry.retained))
            };
            bridge.writer.send(&message).ok()?;
        }
        bridge.writer.send(b"S").ok()?;
        state.bridges.push(bridge);
        Some(id)
    }

    fn answer(&self, bridge: u64, message: &[u8]) {
        let mut state = lock(&self.state);
        match message {
            [b'E', rest @ ..] if rest.len() == 20 => {
                let gen = u64::from_le_bytes(rest[..8].try_into().unwrap_or_default());
                let token = u64::from_le_bytes(rest[8..16].try_into().unwrap_or_default());
                let result = u32::from_le_bytes(rest[16..20].try_into().unwrap_or_default());
                if let Some(call) = state.events.get_mut(&gen) {
                    if call.waiting.remove(&bridge) {
                        let reply =
                            EventReply { handle: (token != 0).then_some(RemoteEvent { bridge, token }), result };
                        if reply.handle.is_some() || call.reply.is_none() {
                            call.reply = Some(reply);
                        }
                    }
                }
            }
            [b'a', rest @ ..] if rest.len() >= 9 => {
                let gen = u64::from_le_bytes(rest[..8].try_into().unwrap_or_default());
                if let Some(entry) = state.entries.values_mut().find(|entry| entry.gen == gen) {
                    entry.answered.insert(bridge);
                    if rest[8] != 0 && rest.len() - 9 == entry.spec.size {
                        entry.retained = rest[9..].to_vec();
                        entry.held_by.insert(bridge);
                        entry.errors.remove(&bridge);
                    } else {
                        let error = if rest[8] == 0 {
                            String::from_utf8_lossy(&rest[9..]).into_owned()
                        } else {
                            "read-only mapping reply has the wrong view size".into()
                        };
                        entry.errors.insert(bridge, error);
                    }
                }
            }
            [b'C', rest @ ..] if rest.len() >= 9 => {
                let gen = u64::from_le_bytes(rest[..8].try_into().unwrap_or_default());
                if let Some(copy) = state.copies.get_mut(&gen) {
                    copy.waiting.remove(&bridge);
                    if rest[8] != 0 && rest.len() - 9 == copy.size {
                        copy.bytes = Some(rest[9..].to_vec());
                    } else {
                        copy.error = Some(if rest[8] == 0 {
                            String::from_utf8_lossy(&rest[9..]).into_owned()
                        } else {
                            "read-only copy reply has the wrong view size".into()
                        });
                    }
                }
            }
            [b'A', rest @ ..] if rest.len() == 9 => {
                let gen = u64::from_le_bytes(rest[..8].try_into().unwrap_or_default());
                let ok = rest[8] != 0;
                if let Some(entry) = state.entries.values_mut().find(|entry| entry.gen == gen) {
                    entry.answered.insert(bridge);
                    if ok {
                        entry.held_by.insert(bridge);
                    }
                }
            }
            [b'D', rest @ ..] if rest.len() == 8 => {
                let gen = u64::from_le_bytes(rest.try_into().unwrap_or_default());
                if let Some(waiting) = state.releasing.get_mut(&gen) {
                    waiting.remove(&bridge);
                    if waiting.is_empty() {
                        state.releasing.remove(&gen);
                    }
                }
            }
            _ => tracing::debug!(bridge, "unknown bridge control message"),
        }
        drop(state);
        self.answered.notify_all();
    }

    /// A bridge went away (its prefix ended): what it last mirrored of each section it held is what the
    /// object keeps showing.
    fn detach(&self, bridge: u64) {
        let mut state = lock(&self.state);
        state.bridges.retain(|b| b.id != bridge);
        for entry in state.entries.values_mut() {
            entry.answered.remove(&bridge);
            entry.errors.remove(&bridge);
            if entry.held_by.remove(&bridge) {
                if let Some(bytes) = read_mirror(&self.dir, entry.spec) {
                    entry.retained = bytes;
                }
            }
        }
        for waiting in state.releasing.values_mut() {
            waiting.remove(&bridge);
        }
        for copy in state.copies.values_mut() {
            copy.waiting.remove(&bridge);
        }
        for call in state.events.values_mut() {
            call.waiting.remove(&bridge);
        }
        state.releasing.retain(|_, waiting| !waiting.is_empty());
        drop(state);
        tracing::info!(bridge, "bridge disconnected");
        self.answered.notify_all();
    }

    fn read_held(&self, name: &str, size: usize) -> Vec<u8> {
        let mut state = lock(&self.state);
        let Some(entry) = state.entries.get_mut(name) else { return vec![0; size] };
        if !entry.held_by.is_empty() {
            if let Some(bytes) = read_mirror(&self.dir, entry.spec) {
                entry.retained.clone_from(&bytes);
            }
        }
        let mut bytes = entry.retained.clone();
        bytes.resize(size, 0);
        bytes
    }

    fn read_held_range(&self, name: &str, size: usize, offset: usize, len: usize) -> io::Result<Vec<u8>> {
        let end = offset.checked_add(len).filter(|end| *end <= size).ok_or(io::ErrorKind::UnexpectedEof)?;
        let mut state = lock(&self.state);
        let Some(entry) = state.entries.get_mut(name) else { return Ok(vec![0; len]) };
        if !entry.held_by.is_empty() {
            if let Ok(bytes) = MappedView::read_range_in(&self.dir, entry.spec.name, entry.spec.size, offset, len) {
                entry.retained[offset..end].copy_from_slice(&bytes);
                return Ok(bytes);
            }
        }
        Ok(entry.retained[offset..end].to_vec())
    }

    fn release(&self, name: &str) {
        let mut state = lock(&self.state);
        let Some(entry) = state.entries.get_mut(name) else { return };
        entry.refs -= 1;
        if entry.refs > 0 {
            return;
        }
        let gen = entry.gen;
        state.entries.remove(name);
        let mut message = vec![b'R'];
        message.extend_from_slice(&gen.to_le_bytes());
        let waiting = send_all(&mut state, &message);
        if waiting.is_empty() {
            return;
        }
        state.releasing.insert(gen, waiting);
        let deadline = Instant::now() + ANSWER_TIMEOUT;
        while state.releasing.contains_key(&gen) {
            let now = Instant::now();
            if now >= deadline {
                state.releasing.remove(&gen);
                break;
            }
            state = self.answered.wait_timeout(state, deadline - now).unwrap_or_else(PoisonError::into_inner).0;
        }
    }

    /// Whether `name` is held (tests).
    #[cfg(test)]
    pub(crate) fn holds(&self, name: &str) -> bool {
        lock(&self.state).entries.contains_key(name)
    }
}

/// Send `message` to every connected bridge; a bridge that cannot be written to is disconnected. Returns the
/// bridges it was sent to.
fn send_all(state: &mut State, message: &[u8]) -> HashSet<u64> {
    let mut sent = HashSet::new();
    for bridge in &mut state.bridges {
        if bridge.writer.send(message).is_ok() {
            sent.insert(bridge.id);
        } else {
            bridge.writer.shutdown();
        }
    }
    sent
}

fn push_name(message: &mut Vec<u8>, name: &str) {
    let bytes = name.as_bytes();
    message.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
    message.extend_from_slice(bytes);
}

fn hold_message(gen: u64, spec: MappingSpec, opening: Opening, seed: Option<&[u8]>) -> Vec<u8> {
    if opening == Opening::CreateReadOnly {
        // A separate opcode makes an old bridge reject this mode rather than
        // silently creating a PAGE_READWRITE object from an unknown flag.
        let mut message = vec![b'h'];
        message.extend_from_slice(&gen.to_le_bytes());
        message.extend_from_slice(&(spec.size as u64).to_le_bytes());
        push_name(&mut message, spec.name);
        return message;
    }
    let mut message = vec![b'H'];
    message.extend_from_slice(&gen.to_le_bytes());
    message.push(u8::from(opening != Opening::Existing) | if seed.is_some() { 2 } else { 0 });
    message.extend_from_slice(&(spec.size as u64).to_le_bytes());
    push_name(&mut message, spec.name);
    if let Some(seed) = seed {
        let mut seed = seed.to_vec();
        seed.resize(spec.size, 0);
        message.extend_from_slice(&seed);
    }
    message
}

/// The current mirror of `spec`, or `None` while it is missing or empty.
fn read_mirror(dir: &Path, spec: MappingSpec) -> Option<Vec<u8>> {
    let mut view = MappedView::open_in(dir, spec.name, spec.size).ok()?;
    view.read().ok().map(<[u8]>::to_vec)
}

/// Writes one control message.
pub fn write_message(stream: &mut impl Write, message: &[u8]) -> io::Result<()> {
    let len = u32::try_from(message.len()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let mut frame = Vec::with_capacity(4 + message.len());
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(message);
    stream.write_all(&frame)?;
    stream.flush()
}

/// Reads one control message; `Ok(None)` at a clean end of the connection.
pub fn read_message(stream: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut header = [0u8; 4];
    let mut filled = 0;
    while filled < header.len() {
        match stream.read(&mut header[filled..])? {
            0 if filled == 0 => return Ok(None),
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            n => filled += n,
        }
    }
    let len = u32::from_le_bytes(header) as usize;
    if len > MAX_MESSAGE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "control message too long"));
    }
    let mut message = vec![0; len];
    stream.read_exact(&mut message)?;
    Ok(Some(message))
}

/// One of the application's mapping objects; dropping it is the listener dropping its object.
pub struct HeldSection {
    registry: Arc<Registry>,
    name: &'static str,
    size: usize,
}

impl HeldSection {
    /// What the object shows now: the bridge's mirror while a bridge holds the section, otherwise what it
    /// showed when the last bridge went away (zeros for a section created while none ran).
    pub fn read(&self) -> Vec<u8> {
        self.registry.read_held(self.name, self.size)
    }

    pub fn read_range(&self, offset: usize, len: usize) -> io::Result<Vec<u8>> {
        self.registry.read_held_range(self.name, self.size, offset, len)
    }
}

impl Drop for HeldSection {
    fn drop(&mut self) {
        self.registry.release(self.name);
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! A scripted bridge on the other end of a control connection.

    use super::*;
    use std::net::TcpListener;

    /// The bridge side of one control connection.
    pub(crate) struct FakeBridge {
        pub(crate) stream: TcpStream,
    }

    impl FakeBridge {
        /// Connects a new fake bridge to `registry`.
        pub(crate) fn connect(registry: &Arc<Registry>) -> FakeBridge {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let bridge_end = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (registry_end, _) = listener.accept().unwrap();
            registry.attach(registry_end);
            bridge_end.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            FakeBridge { stream: bridge_end }
        }

        /// The next message from the registry, past the `S` that ends the initial burst.
        pub(crate) fn next(&mut self) -> Vec<u8> {
            loop {
                let message = read_message(&mut self.stream).unwrap().unwrap();
                if message != b"S" {
                    return message;
                }
            }
        }

        pub(crate) fn send(&mut self, message: &[u8]) {
            write_message(&mut self.stream, message).unwrap();
        }

        /// Answers a hold: `A gen ok`.
        pub(crate) fn held(&mut self, gen: u64, ok: bool) {
            let mut message = vec![b'A'];
            message.extend_from_slice(&gen.to_le_bytes());
            message.push(u8::from(ok));
            self.send(&message);
        }

        /// Answers a release: `D gen`.
        pub(crate) fn released(&mut self, gen: u64) {
            let mut message = vec![b'D'];
            message.extend_from_slice(&gen.to_le_bytes());
            self.send(&message);
        }
    }

    /// A decoded `H` message.
    #[derive(Debug, PartialEq, Eq)]
    pub(crate) struct Hold {
        pub(crate) gen: u64,
        pub(crate) create: bool,
        pub(crate) size: u64,
        pub(crate) name: String,
        pub(crate) seed: Option<Vec<u8>>,
    }

    pub(crate) fn decode_hold(message: &[u8]) -> Hold {
        assert_eq!(message[0], b'H');
        let gen = u64::from_le_bytes(message[1..9].try_into().unwrap());
        let flags = message[9];
        let size = u64::from_le_bytes(message[10..18].try_into().unwrap());
        let len = u16::from_le_bytes(message[18..20].try_into().unwrap()) as usize;
        let name = String::from_utf8(message[20..20 + len].to_vec()).unwrap();
        let seed = (flags & 2 != 0).then(|| message[20 + len..].to_vec());
        Hold { gen, create: flags & 1 != 0, size, name, seed }
    }
}

#[cfg(test)]
pub(crate) mod event_metadata {
    use super::*;
    use std::sync::mpsc;

    /// A bounded metadata endpoint, with no TCP connection or service worker.
    pub(crate) struct Bridge {
        registry: Arc<Registry>,
        id: u64,
        messages: mpsc::Receiver<Vec<u8>>,
    }
    impl Bridge {
        pub(crate) fn connect(registry: &Arc<Registry>) -> Self {
            let (sender, messages) = mpsc::channel();
            let id = registry.register_bridge(BridgeWriter::Metadata(sender)).unwrap();
            Self { registry: registry.clone(), id, messages }
        }
        pub(crate) fn next(&self) -> Vec<u8> {
            loop {
                let message = self.messages.recv().unwrap();
                if message != b"S" {
                    return message;
                }
            }
        }
        pub(crate) fn reply(&self, request: &[u8], token: u64, result: u32) {
            let mut message = vec![b'E'];
            message.extend_from_slice(&request[1..9]);
            message.extend_from_slice(&token.to_le_bytes());
            message.extend_from_slice(&result.to_le_bytes());
            self.registry.answer(self.id, &message);
        }
        pub(crate) fn try_next(&self) -> Option<Vec<u8>> {
            while let Ok(message) = self.messages.try_recv() {
                if message != b"S" {
                    return Some(message);
                }
            }
            None
        }
    }
    impl Drop for Bridge {
        fn drop(&mut self) {
            self.registry.detach(self.id);
        }
    }
}

#[cfg(test)]
mod read_only_tests {
    use super::*;
    use std::sync::mpsc;

    const SPEC: MappingSpec = MappingSpec { name: "Local\\ReadOnlyFixture", size: 2 };

    fn metadata_bridge(registry: &Registry) -> (u64, mpsc::Receiver<Vec<u8>>) {
        let (sender, receiver) = mpsc::channel();
        let id = registry.register_bridge(BridgeWriter::Metadata(sender)).unwrap();
        (id, receiver)
    }
    fn answer(kind: u8, gen: u64, ok: bool, data: &[u8]) -> Vec<u8> {
        let mut answer = vec![kind];
        answer.extend_from_slice(&gen.to_le_bytes());
        answer.push(u8::from(ok));
        answer.extend_from_slice(data);
        answer
    }
    fn generation(message: &[u8]) -> u64 {
        u64::from_le_bytes(message[1..9].try_into().unwrap())
    }
    fn registry() -> Arc<Registry> {
        // Metadata only: this directory is not created and no view is mapped.
        Registry::new("/nonexistent/openrigcue-read-only-metadata-fixture")
    }

    #[test]
    fn readonly_reconnect_retains_creation_mode_and_never_sends_a_write_seed() {
        let registry = registry();
        let held = registry.acquire_read_only(SPEC).unwrap();
        assert_eq!(held.read(), [0, 0]);
        for _ in 0..2 {
            // Previously received data must not turn PAGE_READONLY creation
            // into a writable seeded object when a prefix is recreated.
            lock(&registry.state).entries.get_mut(SPEC.name).unwrap().retained = vec![8, 9];
            let (bridge, messages) = metadata_bridge(&registry);
            let request = messages.recv().unwrap();
            assert_eq!(request[0], b'h');
            assert_eq!(&request[9..17], &2u64.to_le_bytes());
            assert_eq!(&request[19..], SPEC.name.as_bytes(), "there is no seed after the name");
            assert_eq!(messages.recv().unwrap(), b"S");
            registry.answer(bridge, &answer(b'a', generation(&request), true, &[0, 0]));
            assert_eq!(held.read(), [0, 0]);
            registry.detach(bridge);
        }
        drop(held);
        assert!(!registry.holds(SPEC.name));
        let existing = hold_message(3, SPEC, Opening::Existing, None);
        assert_eq!((existing[0], existing[9]), (b'H', 0), "true open-only stays separate");
    }

    #[test]
    fn readonly_hold_failure_releases_the_reference_and_propagates_the_error() {
        let registry = registry();
        let (bridge, messages) = metadata_bridge(&registry);
        assert_eq!(messages.recv().unwrap(), b"S");
        let callback_registry = registry.clone();
        let callback = thread::spawn(move || callback_registry.acquire_read_only(SPEC).map(|_| ()));
        let request = messages.recv().unwrap();
        assert_eq!(request[0], b'h');
        let gen = generation(&request);
        registry.answer(bridge, &answer(b'a', gen, false, b"Access is denied (os error 5)"));
        let release = messages.recv().unwrap();
        assert_eq!(release[0], b'R');
        let mut released = vec![b'D'];
        released.extend_from_slice(&gen.to_le_bytes());
        registry.answer(bridge, &released);
        assert_eq!(callback.join().unwrap().unwrap_err().to_string(), "Access is denied (os error 5)");
        assert!(!registry.holds(SPEC.name));
        registry.detach(bridge);
    }

    #[test]
    fn range_reads_refresh_only_requested_bytes_and_keep_them_after_detach() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::new(dir.path());
        let held = registry.acquire_read_only(SPEC).unwrap();
        let path = super::super::mapping::mirror_path_in(dir.path(), SPEC.name);
        std::fs::write(&path, [4, 7]).unwrap();
        lock(&registry.state).entries.get_mut(SPEC.name).unwrap().held_by.insert(99);
        assert_eq!(held.read_range(1, 1).unwrap(), [7]);
        std::fs::write(&path, [9, 2]).unwrap();
        assert_eq!(held.read_range(0, 1).unwrap(), [9]);
        lock(&registry.state).entries.get_mut(SPEC.name).unwrap().held_by.clear();
        assert_eq!(held.read_range(0, 2).unwrap(), [9, 7]);
        assert!(held.read_range(2, 1).is_err());
    }

    #[test]
    fn readonly_temporary_copy_waits_for_the_bytes_or_real_failure_reply() {
        let registry = registry();
        let (bridge, messages) = metadata_bridge(&registry);
        assert_eq!(messages.recv().unwrap(), b"S");
        for (ok, payload) in [(true, vec![4, 7]), (false, b"MapViewOfFile failed".to_vec()), (true, vec![1])] {
            let callback_registry = registry.clone();
            let callback = thread::spawn(move || callback_registry.copy_read_only(SPEC));
            let request = messages.recv().unwrap();
            assert_eq!(request[0], b'C');
            assert_eq!(&request[9..17], &2u64.to_le_bytes());
            registry.answer(bridge, &answer(b'C', generation(&request), ok, &payload));
            let result = callback.join().unwrap();
            if ok && payload.len() == SPEC.size {
                assert_eq!(result.unwrap(), payload);
            } else {
                assert!(result.is_err(), "failure/short reply must not become zeros");
            }
            assert!(lock(&registry.state).copies.is_empty());
        }
        registry.detach(bridge);
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{decode_hold, FakeBridge};
    use super::*;
    use crate::shared_memory::mapping::mirror_path_in;
    use std::fs;

    const SPEC: MappingSpec = MappingSpec { name: "Local\\GPBProxyObject", size: 4 };

    /// A create-or-open with no prefix around: the section is created
    /// zero-filled and keeps those zeros; mirrors are not consulted.
    #[test]
    fn created_without_a_bridge_reads_zeros() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(mirror_path_in(dir.path(), SPEC.name), [9, 9, 9, 9]).unwrap();
        let registry = Registry::new(dir.path());
        let held = registry.acquire(SPEC, Opening::CreateOrOpen, None);
        assert_eq!(held.read(), vec![0; 4]);
        assert!(!registry.bridge_connected());
    }

    /// A held object is sent to a bridge, which answers before the read; the read then shows the prefix's section.
    /// When the bridge goes away the object keeps the last frame; a bridge connecting later gets it as its seed.
    #[test]
    fn held_section_follows_the_bridge_and_keeps_the_last_frame() {
        let dir = tempfile::tempdir().unwrap();
        let path = mirror_path_in(dir.path(), SPEC.name);
        let registry = Registry::new(dir.path());
        let mut bridge = FakeBridge::connect(&registry);
        assert!(registry.bridge_connected());

        let answering = thread::spawn(move || {
            let hold = decode_hold(&bridge.next());
            assert_eq!((hold.create, hold.size, hold.name.as_str(), hold.seed), (true, 4, SPEC.name, None));
            fs::write(&path, [1, 2, 3, 4]).unwrap();
            bridge.held(hold.gen, true);
            bridge
        });
        let held = registry.acquire(SPEC, Opening::CreateOrOpen, None);
        let bridge = answering.join().unwrap();
        assert_eq!(held.read(), vec![1, 2, 3, 4]);

        fs::write(mirror_path_in(dir.path(), SPEC.name), [5, 6, 7, 8]).unwrap();
        drop(bridge);
        let deadline = Instant::now() + Duration::from_secs(5);
        while registry.bridge_connected() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        fs::write(mirror_path_in(dir.path(), SPEC.name), [0, 0, 0, 0]).unwrap();
        assert_eq!(held.read(), vec![5, 6, 7, 8]);

        let mut later = FakeBridge::connect(&registry);
        let hold = decode_hold(&later.next());
        assert_eq!((hold.create, hold.seed), (true, Some(vec![5, 6, 7, 8])));
    }

    /// A new object replacing the previous one: one hold for both, released after the last.
    #[test]
    fn objects_of_one_name_share_one_hold() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::new(dir.path());
        let mut bridge = FakeBridge::connect(&registry);
        let first = {
            let registry = Arc::clone(&registry);
            thread::spawn(move || registry.acquire(SPEC, Opening::CreateOrOpen, None))
        };
        let hold = decode_hold(&bridge.next());
        bridge.held(hold.gen, true);
        let first = first.join().unwrap();
        let second = registry.acquire(SPEC, Opening::CreateOrOpen, None);
        drop(first);
        assert!(registry.holds(SPEC.name));
        let releasing = thread::spawn(move || drop(second));
        let release = bridge.next();
        assert_eq!(release[0], b'R');
        assert_eq!(u64::from_le_bytes(release[1..9].try_into().unwrap()), hold.gen);
        bridge.released(hold.gen);
        releasing.join().unwrap();
        assert!(!registry.holds(SPEC.name));
    }

    /// The iRacing map: made from a section that was read, so the bridge only opens it, and without a bridge
    /// the object shows what was read.
    #[test]
    fn existing_section_object_opens_only() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::new(dir.path());
        let held = registry.acquire(SPEC, Opening::Existing, Some(&[7, 7]));
        assert_eq!(held.read(), vec![7, 7, 0, 0]);
        let mut bridge = FakeBridge::connect(&registry);
        let hold = decode_hold(&bridge.next());
        assert_eq!((hold.create, hold.seed), (true, Some(vec![7, 7, 0, 0])));
        drop(held);

        let mut answering = FakeBridge::connect(&registry);
        let acquiring = {
            let registry = Arc::clone(&registry);
            thread::spawn(move || registry.acquire(SPEC, Opening::Existing, Some(&[1])))
        };
        assert_eq!(bridge.next()[0], b'R');
        let hold = decode_hold(&bridge.next());
        assert!(!hold.create);
        bridge.held(hold.gen, false);
        let hold_b = decode_hold(&answering.next());
        answering.held(hold_b.gen, false);
        let held = acquiring.join().unwrap();
        assert_eq!(held.read(), vec![1, 0, 0, 0]);
    }

    /// Per-poll snapshots: each copy is a create-or-open the bridges perform and close; the copy shows the
    /// current mirror, or zeros.
    #[test]
    fn create_once_reaches_the_bridges() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::new(dir.path());
        assert_eq!(registry.create_once(SPEC), vec![0; 4]);
        let mut bridge = FakeBridge::connect(&registry);
        fs::write(mirror_path_in(dir.path(), SPEC.name), [4, 3, 2, 1]).unwrap();
        assert_eq!(registry.create_once(SPEC), vec![4, 3, 2, 1]);
        let message = bridge.next();
        assert_eq!(message[0], b'T');
        assert_eq!(u64::from_le_bytes(message[1..9].try_into().unwrap()), 4);
    }

    #[test]
    fn messages_round_trip() {
        let mut wire = Vec::new();
        write_message(&mut wire, b"abc").unwrap();
        let mut reader = io::Cursor::new(wire);
        assert_eq!(read_message(&mut reader).unwrap(), Some(b"abc".to_vec()));
        assert_eq!(read_message(&mut reader).unwrap(), None);
    }
}
