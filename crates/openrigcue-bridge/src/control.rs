//! The control connection to OpenRigCue: which sections the application itself holds or creates.
//!
//! OpenRigCue runs the listeners; each named mapping they open (and drop) is a Windows object owned by the
//! application. OpenRigCue keeps those objects
//! (`openrigcue_sources::shared_memory::held::Registry`) and sends them here, and the bridge makes them exist in the
//! game's prefix:
//!
//! | message | effect |
//! |---|---|
//! | `H` gen:u64 flags:u8 size:u64 name_len:u16 name [seed] | hold `name`: create it when missing (flags bit 0) or only open it; a section created here starts with the seed (flags bit 1, `size` bytes), the content the application's object kept; answered `A gen ok` once held (or not) and mirrored |
//! | `R` gen:u64 | close that hold; answered `D gen` |
//! | `T` size:u64 name_len:u16 name | create-or-open, copy into the mirror, close (a per-poll snapshot) |
//! | `S` | every object held when the connection was made has been sent (the bridge starts the game after it) |
//!
//! Each message is framed as a little-endian `u32` length followed by the message. The connection goes to the
//! pipe relay's TCP port, first frame [`CONTROL_ROUTE`] (relay framing). While it is up OpenRigCue
//! is running: the always-served pipes exist only then. When it ends, every hold is closed.

use std::collections::HashMap;
use std::io::{self, Read, Write};

/// Route name of the control connection (`openrigcue_sources::shared_memory::held::CONTROL_ROUTE`).
pub const CONTROL_ROUTE: &str = "OpenRigCue:sections";
/// Largest control message.
pub const MAX_MESSAGE: usize = 4 << 20;

/// One request from OpenRigCue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    Hold { gen: u64, create: bool, size: usize, name: String, seed: Option<Vec<u8>> },
    HoldReadOnly { gen: u64, size: usize, name: String },
    Release { gen: u64 },
    CreateOnce { size: usize, name: String },
    CopyReadOnly { gen: u64, size: usize, name: String },
    OpenEvent { gen: u64, name: String },
    WaitEvent { gen: u64, token: u64 },
    Synced,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("malformed control message")]
pub struct Malformed;

fn take<'a>(bytes: &mut &'a [u8], len: usize) -> Result<&'a [u8], Malformed> {
    if bytes.len() < len {
        return Err(Malformed);
    }
    let (head, rest) = bytes.split_at(len);
    *bytes = rest;
    Ok(head)
}

fn take_u64(bytes: &mut &[u8]) -> Result<u64, Malformed> {
    Ok(u64::from_le_bytes(take(bytes, 8)?.try_into().map_err(|_| Malformed)?))
}

fn take_name(bytes: &mut &[u8]) -> Result<String, Malformed> {
    let len = u16::from_le_bytes(take(bytes, 2)?.try_into().map_err(|_| Malformed)?);
    String::from_utf8(take(bytes, usize::from(len))?.to_vec()).map_err(|_| Malformed)
}

fn take_size(bytes: &mut &[u8]) -> Result<usize, Malformed> {
    usize::try_from(take_u64(bytes)?).map_err(|_| Malformed).and_then(|size| {
        if size == 0 || size > MAX_MESSAGE {
            Err(Malformed)
        } else {
            Ok(size)
        }
    })
}

impl Request {
    pub fn decode(message: &[u8]) -> Result<Request, Malformed> {
        let (&kind, mut rest) = message.split_first().ok_or(Malformed)?;
        let request = match kind {
            b'H' => {
                let gen = take_u64(&mut rest)?;
                let flags = take(&mut rest, 1)?[0];
                let size = take_size(&mut rest)?;
                let name = take_name(&mut rest)?;
                let seed = if flags & 2 != 0 { Some(take(&mut rest, size)?.to_vec()) } else { None };
                Request::Hold { gen, create: flags & 1 != 0, size, name, seed }
            }
            b'h' => {
                let gen = take_u64(&mut rest)?;
                let size = take_size(&mut rest)?;
                Request::HoldReadOnly { gen, size, name: take_name(&mut rest)? }
            }
            b'R' => Request::Release { gen: take_u64(&mut rest)? },
            b'T' => {
                let size = take_size(&mut rest)?;
                Request::CreateOnce { size, name: take_name(&mut rest)? }
            }
            b'C' => {
                let gen = take_u64(&mut rest)?;
                let size = take_size(&mut rest)?;
                Request::CopyReadOnly { gen, size, name: take_name(&mut rest)? }
            }
            b'E' => Request::OpenEvent { gen: take_u64(&mut rest)?, name: take_name(&mut rest)? },
            b'W' => Request::WaitEvent { gen: take_u64(&mut rest)?, token: take_u64(&mut rest)? },
            b'S' => Request::Synced,
            _ => return Err(Malformed),
        };
        if !rest.is_empty() {
            return Err(Malformed);
        }
        Ok(request)
    }
}

/// `A gen ok`.
pub fn held_answer(gen: u64, ok: bool) -> Vec<u8> {
    let mut message = vec![b'A'];
    message.extend_from_slice(&gen.to_le_bytes());
    message.push(u8::from(ok));
    message
}

/// `D gen`.
pub fn released_answer(gen: u64) -> Vec<u8> {
    let mut message = vec![b'D'];
    message.extend_from_slice(&gen.to_le_bytes());
    message
}

/// A read-only operation answers with bytes only after the requested view was
/// mapped/copied. Failures carry the Windows-side error rather than fake zeros.
fn read_only_answer(kind: u8, gen: u64, result: io::Result<Vec<u8>>) -> Vec<u8> {
    let mut message = vec![kind];
    message.extend_from_slice(&gen.to_le_bytes());
    match result {
        Ok(bytes) => {
            message.push(1);
            message.extend_from_slice(&bytes);
        }
        Err(error) => {
            message.push(0);
            message.extend_from_slice(error.to_string().as_bytes());
        }
    }
    message
}

/// The Windows side of the requests (the prefix's sections, and their mirrors).
pub trait SectionHolder {
    /// Create (when `create`) or open `name` and keep it; a section created here starts with `seed`. Mirror it
    /// before returning. `false` when it could not be opened.
    fn hold(&mut self, name: &str, size: usize, create: bool, seed: Option<&[u8]>) -> bool;
    /// Close the hold of `name`.
    fn release(&mut self, name: &str);
    /// Create-or-open `name`, copy it into its mirror, close it.
    fn create_once(&mut self, name: &str, size: usize);
    /// Create/open PAGE_READONLY, map FILE_MAP_READ at exactly `size`, and keep
    /// both handle and view. Newly created objects retain their native zeros.
    fn hold_read_only(&mut self, _name: &str, _size: usize) -> io::Result<Vec<u8>> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "read-only mappings are unsupported"))
    }
    /// Copy through a temporary read-only mapping, releasing it before reply.
    fn copy_read_only(&mut self, _name: &str, _size: usize) -> io::Result<Vec<u8>> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "read-only mappings are unsupported"))
    }
}

/// The holds of one control connection, by generation.
#[derive(Default)]
pub struct Holds {
    by_gen: HashMap<u64, String>,
}

impl Holds {
    /// Carry out `request`; returns the answer to send, if any.
    pub fn apply(&mut self, request: Request, sections: &mut impl SectionHolder) -> Option<Vec<u8>> {
        match request {
            Request::OpenEvent { .. } | Request::WaitEvent { .. } => None,
            Request::HoldReadOnly { gen, size, name } => {
                if let Some(previous) = self.by_gen.remove(&gen) {
                    sections.release(&previous);
                }
                if let Some(old) = self.by_gen.iter().find(|(_, held)| **held == name).map(|(gen, _)| *gen) {
                    self.by_gen.remove(&old);
                    sections.release(&name);
                }
                let result = sections.hold_read_only(&name, size);
                if result.is_ok() {
                    self.by_gen.insert(gen, name);
                }
                Some(read_only_answer(b'a', gen, result))
            }
            Request::Hold { gen, create, size, name, seed } => {
                if let Some(previous) = self.by_gen.remove(&gen) {
                    sections.release(&previous);
                }
                // A name is held once per connection: a new generation replaces the old one.
                if let Some(old) = self.by_gen.iter().find(|(_, held)| **held == name).map(|(gen, _)| *gen) {
                    self.by_gen.remove(&old);
                    sections.release(&name);
                }
                let ok = sections.hold(&name, size, create, seed.as_deref());
                if ok {
                    self.by_gen.insert(gen, name);
                }
                Some(held_answer(gen, ok))
            }
            Request::Release { gen } => {
                if let Some(name) = self.by_gen.remove(&gen) {
                    sections.release(&name);
                }
                Some(released_answer(gen))
            }
            Request::CreateOnce { size, name } => {
                sections.create_once(&name, size);
                None
            }
            Request::CopyReadOnly { gen, size, name } => {
                Some(read_only_answer(b'C', gen, sections.copy_read_only(&name, size)))
            }
            Request::Synced => None,
        }
    }

    /// The connection ended: OpenRigCue is gone, and with it every object it held.
    pub fn release_all(&mut self, sections: &mut impl SectionHolder) {
        for (_, name) in self.by_gen.drain() {
            sections.release(&name);
        }
    }
}

/// Execute only the requested native event operation. Windows handles remain
/// owned by `events` for this application connection's lifetime.
pub fn apply_event<A: crate::event::consumer::EventApi>(
    request: &Request,
    events: &mut crate::event::consumer::Consumer<A>,
    snapshot: impl FnOnce(),
) -> Option<Vec<u8>> {
    let (gen, waited) = match request {
        Request::OpenEvent { gen, name } => (*gen, events.open_and_wait(name)),
        Request::WaitEvent { gen, token } => (*gen, events.wait(*token)),
        _ => return None,
    };
    // Both successful and timed-out receive waits precede the SDK's copy.
    // Finish that refresh before publishing the response to Linux.
    if waited.token != 0 {
        snapshot();
    }
    let mut reply = vec![b'E'];
    reply.extend_from_slice(&gen.to_le_bytes());
    reply.extend_from_slice(&waited.token.to_le_bytes());
    reply.extend_from_slice(&waited.result.to_le_bytes());
    Some(reply)
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
    if len > MAX_MESSAGE + 64 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "control message too long"));
    }
    let mut message = vec![0; len];
    stream.read_exact(&mut message)?;
    Ok(Some(message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct EventFixture {
        calls: Arc<Mutex<Vec<String>>>,
        results: std::collections::VecDeque<u32>,
    }
    impl crate::event::consumer::EventApi for EventFixture {
        type Handle = usize;
        fn open(&mut self, name: &str, access: u32, inherit: bool) -> Option<usize> {
            self.calls.lock().unwrap().push(format!("open {name} {access:x} {inherit}"));
            Some(1)
        }
        fn wait(&mut self, _: &usize, milliseconds: u32) -> u32 {
            self.calls.lock().unwrap().push(format!("wait {milliseconds}"));
            self.results.pop_front().unwrap()
        }
    }

    #[test]
    fn event_protocol_keeps_handle_identity_and_native_return_code() {
        let mut open = vec![b'E'];
        open.extend_from_slice(&41u64.to_le_bytes());
        open.extend_from_slice(&5u16.to_le_bytes());
        open.extend_from_slice(b"event");
        assert_eq!(Request::decode(&open), Ok(Request::OpenEvent { gen: 41, name: "event".into() }));
        let mut wait = vec![b'W'];
        wait.extend_from_slice(&42u64.to_le_bytes());
        wait.extend_from_slice(&7u64.to_le_bytes());
        assert_eq!(Request::decode(&wait), Ok(Request::WaitEvent { gen: 42, token: 7 }));
    }

    #[test]
    fn event_snapshot_is_refreshed_after_wait_even_on_timeout_before_reply() {
        for result in [0, 258, u32::MAX] {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let mut events = crate::event::consumer::Consumer::new(EventFixture {
                calls: calls.clone(),
                results: [0, result].into_iter().collect(),
            });
            events.open_and_wait("event");
            calls.lock().unwrap().clear();
            let reply = apply_event(&Request::WaitEvent { gen: 2, token: 1 }, &mut events, || {
                calls.lock().unwrap().push("snapshot".into());
            })
            .unwrap();
            assert_eq!(*calls.lock().unwrap(), ["wait 32", "snapshot"]);
            assert_eq!(&reply[1..9], &2u64.to_le_bytes());
            assert_eq!(&reply[9..17], &1u64.to_le_bytes());
            assert_eq!(&reply[17..], &result.to_le_bytes());
        }
    }

    fn hold(gen: u64, flags: u8, size: u64, name: &str, seed: &[u8]) -> Vec<u8> {
        let mut message = vec![b'H'];
        message.extend_from_slice(&gen.to_le_bytes());
        message.push(flags);
        message.extend_from_slice(&size.to_le_bytes());
        message.extend_from_slice(&(name.len() as u16).to_le_bytes());
        message.extend_from_slice(name.as_bytes());
        message.extend_from_slice(seed);
        message
    }

    #[derive(Default)]
    struct Recorder {
        calls: Vec<String>,
        fail_open: bool,
    }

    impl SectionHolder for Recorder {
        fn hold_read_only(&mut self, name: &str, size: usize) -> io::Result<Vec<u8>> {
            self.calls.push(format!("readonly hold {name} {size}"));
            if self.fail_open {
                Err(io::ErrorKind::PermissionDenied.into())
            } else {
                Ok(vec![0; size])
            }
        }
        fn copy_read_only(&mut self, name: &str, size: usize) -> io::Result<Vec<u8>> {
            self.calls.push(format!("readonly copy {name} {size}"));
            if self.fail_open {
                Err(io::ErrorKind::PermissionDenied.into())
            } else {
                Ok(vec![9; size])
            }
        }
        fn hold(&mut self, name: &str, size: usize, create: bool, seed: Option<&[u8]>) -> bool {
            self.calls.push(format!("hold {name} {size} {create} {seed:?}"));
            create || !self.fail_open
        }
        fn release(&mut self, name: &str) {
            self.calls.push(format!("release {name}"));
        }
        fn create_once(&mut self, name: &str, size: usize) {
            self.calls.push(format!("once {name} {size}"));
        }
    }

    #[test]
    fn readonly_requests_keep_protection_size_and_acknowledged_copy_results() {
        let mut holds = Holds::default();
        let mut sections = Recorder::default();
        for kind in *b"hC" {
            let mut message = vec![kind];
            message.extend_from_slice(&13u64.to_le_bytes());
            message.extend_from_slice(&2u64.to_le_bytes());
            message.extend_from_slice(&4u16.to_le_bytes());
            message.extend_from_slice(b"$R3E");
            let request = Request::decode(&message).unwrap();
            let reply = holds.apply(request, &mut sections).unwrap();
            assert_eq!(reply[0], if kind == b'h' { b'a' } else { b'C' });
            assert_eq!(&reply[1..9], &13u64.to_le_bytes());
            assert_eq!(reply[9], 1);
            assert_eq!(&reply[10..], if kind == b'h' { &[0, 0] } else { &[9, 9] });
        }
        assert_eq!(sections.calls, ["readonly hold $R3E 2", "readonly copy $R3E 2"]);
        holds.release_all(&mut sections);
        assert_eq!(sections.calls.last().unwrap(), "release $R3E");
    }

    #[test]
    fn readonly_errors_are_acknowledged_without_retaining_a_failed_hold() {
        let mut holds = Holds::default();
        let mut sections = Recorder { fail_open: true, ..Recorder::default() };
        for request in [
            Request::HoldReadOnly { gen: 1, size: 2, name: "x".into() },
            Request::CopyReadOnly { gen: 2, size: 2, name: "x".into() },
        ] {
            let reply = holds.apply(request, &mut sections).unwrap();
            assert_eq!(reply[9], 0);
            assert!(!reply[10..].is_empty());
        }
        holds.release_all(&mut sections);
        assert_eq!(sections.calls.len(), 2, "failed hold owns no handle to release");
    }

    /// The registry's message layout (openrigcue_sources::shared_memory::held).
    #[test]
    fn requests_decode() {
        assert_eq!(
            Request::decode(&hold(7, 3, 2, "acpmf_static", &[1, 2])),
            Ok(Request::Hold { gen: 7, create: true, size: 2, name: "acpmf_static".into(), seed: Some(vec![1, 2]) })
        );
        assert_eq!(
            Request::decode(&hold(8, 0, 4, "$R3E", &[])),
            Ok(Request::Hold { gen: 8, create: false, size: 4, name: "$R3E".into(), seed: None })
        );
        let mut release = vec![b'R'];
        release.extend_from_slice(&9u64.to_le_bytes());
        assert_eq!(Request::decode(&release), Ok(Request::Release { gen: 9 }));
        let mut once = vec![b'T'];
        once.extend_from_slice(&11624u64.to_le_bytes());
        once.extend_from_slice(&8u16.to_le_bytes());
        once.extend_from_slice(b"$pcars2$");
        assert_eq!(Request::decode(&once), Ok(Request::CreateOnce { size: 11624, name: "$pcars2$".into() }));
        assert_eq!(Request::decode(&hold(1, 2, 4, "x", &[1])), Err(Malformed), "short seed");
        assert_eq!(Request::decode(&hold(1, 0, 0, "x", &[])), Err(Malformed), "zero size");
        assert_eq!(Request::decode(b"S"), Ok(Request::Synced));
        assert_eq!(Request::decode(b"Z"), Err(Malformed));
        assert_eq!(held_answer(5, true), [b'A', 5, 0, 0, 0, 0, 0, 0, 0, 1]);
        assert_eq!(released_answer(5), [b'D', 5, 0, 0, 0, 0, 0, 0, 0]);
    }

    /// Holds are answered, released by generation, and all closed when the connection ends; an open that fails
    /// is answered "not held" and leaves nothing to release.
    #[test]
    fn holds_follow_the_requests() {
        let mut holds = Holds::default();
        let mut sections = Recorder { fail_open: true, ..Recorder::default() };
        let answer = holds.apply(Request::decode(&hold(1, 1, 4, "SimBin", &[])).unwrap(), &mut sections);
        assert_eq!(answer, Some(held_answer(1, true)));
        let answer = holds.apply(Request::decode(&hold(2, 0, 4, "$R3E", &[])).unwrap(), &mut sections);
        assert_eq!(answer, Some(held_answer(2, false)));
        assert_eq!(holds.apply(Request::Release { gen: 2 }, &mut sections), Some(released_answer(2)));
        assert_eq!(holds.apply(Request::CreateOnce { size: 8, name: "$pcars2$".into() }, &mut sections), None);
        holds.apply(Request::decode(&hold(3, 1, 4, "acpmf_static", &[])).unwrap(), &mut sections);
        holds.release_all(&mut sections);
        let mut calls = sections.calls.clone();
        let tail = calls.split_off(4);
        assert_eq!(
            calls,
            ["hold SimBin 4 true None", "hold $R3E 4 false None", "once $pcars2$ 8", "hold acpmf_static 4 true None"]
        );
        let mut tail = tail;
        tail.sort();
        assert_eq!(tail, ["release SimBin", "release acpmf_static"]);
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
