//! Relaying a named pipe to a Linux stream socket.
//!
//! The bridge is the pipe server inside the prefix and forwards what the game writes to
//! openrigcue over a stream socket (`$XDG_RUNTIME_DIR/openrigcue/pipe-<name>`, or localhost TCP). Because a stream
//! socket has no message boundaries, every chunk is framed as a little-endian `u32` length followed by the
//! bytes: one frame per `ReadFile` result going to openrigcue, and one frame per reply coming back. Over TCP the
//! first frame the bridge sends is the pipe name, so one TCP listener can serve every pipe.

use std::io::{self, Read, Write};

use crate::catalog::{ConnectRule, Exchange, PipeSpec};

/// Localhost TCP port the relay uses when the Unix socket cannot be reached (Wine 10 has no AF_UNIX
/// support). An openrigcue-defined port, outside every telemetry port the application configures.
pub const RELAY_TCP_PORT: u16 = 33010;

/// Largest frame accepted from openrigcue (the largest pipe buffer in the catalog).
pub const MAX_FRAME_LEN: usize = 65536;

/// Write one frame.
pub fn write_frame(out: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(payload);
    out.write_all(&frame)?;
    out.flush()
}

/// Read one frame; `Ok(None)` when the peer closed the stream between frames.
pub fn read_frame(input: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match input.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME_LEN {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("relay frame of {len} bytes")));
    }
    let mut payload = vec![0; len];
    input.read_exact(&mut payload)?;
    Ok(Some(payload))
}

/// Win32 error codes the pipe servers tell apart (values from `winerror.h`; checked against `windows-sys` when
/// the Windows module is compiled).
pub mod win32 {
    pub const ERROR_BROKEN_PIPE: u32 = 109;
    pub const ERROR_PIPE_BUSY: u32 = 231;
    pub const ERROR_MORE_DATA: u32 = 234;
    pub const ERROR_PIPE_CONNECTED: u32 = 535;
}

/// The result of one synchronous `ReadFile` on a pipe instance: the chunk length, `None` once the client has
/// closed its end, or the error. Any failed read ends the client, `ERROR_MORE_DATA` (a message longer than the
/// read buffer) included, and its partial bytes are not passed on.
pub fn read_result(succeeded: bool, bytes_read: u32, error: u32) -> io::Result<Option<usize>> {
    match (succeeded, error) {
        (true, _) => Ok(Some(bytes_read as usize)),
        (false, win32::ERROR_BROKEN_PIPE) => Ok(None),
        (false, code) => Err(io::Error::from_raw_os_error(code as i32)),
    }
}

/// What a server does with a pipe instance after `ConnectNamedPipe`.
#[derive(Debug)]
pub enum Accept {
    /// Relay the connected client.
    Serve,
    /// Close the instance without reading it and create the next one at once.
    CloseUnread,
    /// Stop serving this pipe for the rest of the run.
    StopServing(io::Error),
}

/// Decide from a synchronous `ConnectNamedPipe` result (`succeeded`, and `GetLastError` when it did not).
/// - [`ConnectRule::AcceptEarlyClient`]: a client that had already connected (`ERROR_PIPE_CONNECTED`) is served;
///   any other error ends the server thread.
/// - [`ConnectRule::FreshClientOnly`]: a client is served only when `ConnectNamedPipe` returned non-zero;
///   otherwise the instance is closed and the next one created.
pub fn accept_result(rule: ConnectRule, succeeded: bool, error: u32) -> Accept {
    match (rule, succeeded, error) {
        (_, true, _) => Accept::Serve,
        (ConnectRule::AcceptEarlyClient, false, win32::ERROR_PIPE_CONNECTED) => Accept::Serve,
        (ConnectRule::AcceptEarlyClient, false, code) => Accept::StopServing(io::Error::from_raw_os_error(code as i32)),
        (ConnectRule::FreshClientOnly, false, _) => Accept::CloseUnread,
    }
}

/// The game's end of a pipe instance.
pub trait PipeEnd {
    /// One `ReadFile`, as [`read_result`] reports it: the length of the next chunk (message, for message pipes)
    /// placed in `buf`, or `None` once the client has closed its end.
    fn read_chunk(&mut self, buf: &mut [u8]) -> io::Result<Option<usize>>;
    /// One `WriteFile` of the whole chunk.
    fn write_chunk(&mut self, data: &[u8]) -> io::Result<()>;
}

/// How a relayed client ended without an I/O failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientEnd {
    /// The client closed its end, or the session rule ended it: tear the instance down, pause, next client.
    NextClient,
    /// Stop serving this pipe for the rest of the run.
    StopServing,
}

/// Length of the DynamicCab packet header: a 16-bit packet size and an 8-bit packet type.
const CAB_HEADER_LEN: usize = 3;
/// Packet type of the DynamicCab handshake.
const CAB_HANDSHAKE_TYPE: u8 = 1;

/// Relay one connected client until it disconnects, the session rule ends it, or either side fails (`Err`, which
/// also ends the client).
///
/// openrigcue (over `upstream`) produces the replies:
/// - [`Exchange::Inbound`]: each message is read into the pipe's read buffer and forwarded; nothing is written
///   back.
/// - [`Exchange::HandshakeSession`]: each `ReadFile` result is forwarded and exactly one reply per packet (the
///   frame openrigcue sends back) is written. The session continues only once a handshake has been answered: after
///   a reply to anything but a handshake, with no handshake answered yet, the client is disconnected. A packet
///   shorter than the header ends the pipe server for the rest of the run; that packet is still passed to
///   openrigcue, which ends its side the same way, and no reply is awaited.
pub fn relay_client(
    spec: &PipeSpec,
    pipe: &mut impl PipeEnd,
    upstream: &mut (impl Read + Write),
) -> io::Result<ClientEnd> {
    let mut buf = vec![0; spec.read_size];
    let mut handshake_answered = false;
    loop {
        let Some(n) = pipe.read_chunk(&mut buf)? else {
            return Ok(ClientEnd::NextClient);
        };
        let packet = &buf[..n];
        write_frame(upstream, packet)?;
        if spec.exchange == Exchange::Inbound {
            continue;
        }
        let Some(&package_type) = packet.get(CAB_HEADER_LEN - 1) else {
            return Ok(ClientEnd::StopServing);
        };
        let reply = read_frame(upstream)?.ok_or_else(|| io::Error::from(io::ErrorKind::ConnectionAborted))?;
        pipe.write_chunk(&reply)?;
        handshake_answered |= package_type == CAB_HANDSHAKE_TYPE;
        if !handshake_answered {
            return Ok(ClientEnd::NextClient);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{DYNAMIC_CAB_PIPE, FM_TELEMETRY_PIPE};
    use std::collections::VecDeque;
    use std::io::Cursor;

    /// One scripted `ReadFile` result.
    enum ClientRead {
        Chunk(Vec<u8>),
        Fail(u32),
    }

    /// A pipe client that writes `reads` in order (then closes its end) and records what it is sent.
    struct FakeClient {
        reads: VecDeque<ClientRead>,
        received: Vec<Vec<u8>>,
        write_fails: bool,
    }

    impl FakeClient {
        fn sending(chunks: &[Vec<u8>]) -> FakeClient {
            FakeClient {
                reads: chunks.iter().cloned().map(ClientRead::Chunk).collect(),
                received: Vec::new(),
                write_fails: false,
            }
        }
    }

    impl PipeEnd for FakeClient {
        fn read_chunk(&mut self, buf: &mut [u8]) -> io::Result<Option<usize>> {
            match self.reads.pop_front() {
                None => read_result(false, 0, win32::ERROR_BROKEN_PIPE),
                Some(ClientRead::Fail(code)) => read_result(false, 0, code),
                Some(ClientRead::Chunk(chunk)) => {
                    buf[..chunk.len()].copy_from_slice(&chunk);
                    read_result(true, chunk.len() as u32, 0)
                }
            }
        }
        fn write_chunk(&mut self, data: &[u8]) -> io::Result<()> {
            if self.write_fails {
                return Err(io::Error::from_raw_os_error(win32::ERROR_BROKEN_PIPE as i32));
            }
            self.received.push(data.to_vec());
            Ok(())
        }
    }

    /// Upstream socket: replies are pre-loaded, frames sent are captured.
    struct FakeUpstream {
        replies: Cursor<Vec<u8>>,
        sent: Vec<u8>,
    }

    impl FakeUpstream {
        fn replying(replies: &[&[u8]]) -> FakeUpstream {
            let mut wire = Vec::new();
            for reply in replies {
                write_frame(&mut wire, reply).unwrap();
            }
            FakeUpstream { replies: Cursor::new(wire), sent: Vec::new() }
        }
    }

    impl Read for FakeUpstream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.replies.read(buf)
        }
    }

    impl Write for FakeUpstream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.sent.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn frames(mut bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while let Some(frame) = read_frame(&mut bytes).unwrap() {
            out.push(frame);
        }
        out
    }

    /// Header bytes: a little-endian 16-bit packet size, then the 8-bit packet type.
    fn header(package_size: i16, package_type: i8) -> Vec<u8> {
        let mut bytes = package_size.to_le_bytes().to_vec();
        bytes.push(package_type as u8);
        bytes
    }

    /// A per-frame packet: header (size 107, type 2) then the 104-byte body (one double and 24 floats).
    fn per_frame_packet() -> Vec<u8> {
        let mut packet = header(107, 2);
        packet.extend_from_slice(&1.25f64.to_le_bytes());
        packet.extend((0..24).flat_map(|i| (i as f32).to_le_bytes()));
        packet
    }

    /// The handshake reply and a per-frame reply, in their C struct layout.
    const INITIAL_REPLY: [u8; 8] = [6, 0, 0, 0, 0, 0, 0, 0];
    const FRAME_REPLY: [u8; 4] = [4, 0, 1, 0];

    #[test]
    fn frame_round_trip() {
        let mut wire = Vec::new();
        write_frame(&mut wire, b"abc").unwrap();
        write_frame(&mut wire, b"").unwrap();
        assert_eq!(wire[..4], [3, 0, 0, 0]);
        assert_eq!(frames(&wire), [b"abc".to_vec(), Vec::new()]);
    }

    #[test]
    fn oversized_frame_is_rejected() {
        let wire = ((MAX_FRAME_LEN + 1) as u32).to_le_bytes();
        assert_eq!(read_frame(&mut &wire[..]).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    /// A successful ReadFile yields its byte count, a broken pipe
    /// means the client closed, and every other failure (ERROR_MORE_DATA included) is an error that ends the client.
    #[test]
    fn read_results() {
        assert_eq!(read_result(true, 17, 0).unwrap(), Some(17));
        assert_eq!(read_result(true, 0, 0).unwrap(), Some(0));
        assert_eq!(read_result(false, 0, win32::ERROR_BROKEN_PIPE).unwrap(), None);
        let more = read_result(false, 4096, win32::ERROR_MORE_DATA).unwrap_err();
        assert_eq!(more.raw_os_error(), Some(win32::ERROR_MORE_DATA as i32));
    }

    /// One rule serves an early client and stops on other errors;
    /// the other serves only a non-zero return and otherwise closes the instance unread.
    #[test]
    fn accept_results() {
        const ERROR_NO_DATA: u32 = 232;
        let ccd = ConnectRule::AcceptEarlyClient;
        assert!(matches!(accept_result(ccd, true, 0), Accept::Serve));
        assert!(matches!(accept_result(ccd, false, win32::ERROR_PIPE_CONNECTED), Accept::Serve));
        assert!(
            matches!(accept_result(ccd, false, ERROR_NO_DATA), Accept::StopServing(e) if e.raw_os_error() == Some(232))
        );
        let wpipe = ConnectRule::FreshClientOnly;
        assert!(matches!(accept_result(wpipe, true, 0), Accept::Serve));
        assert!(matches!(accept_result(wpipe, false, win32::ERROR_PIPE_CONNECTED), Accept::CloseUnread));
        assert!(matches!(accept_result(wpipe, false, ERROR_NO_DATA), Accept::CloseUnread));
    }

    /// Each FMTelemetry message (even an empty one) reaches openrigcue
    /// as one frame; nothing is written back; the client ends when it closes its end.
    #[test]
    fn inbound_pipe_forwards_each_message() {
        let messages = [b"{'speed': 12.5}".to_vec(), Vec::new(), b"{'speed': 13.0}".to_vec()];
        let mut client = FakeClient::sending(&messages);
        let mut upstream = FakeUpstream::replying(&[]);
        let end = relay_client(&FM_TELEMETRY_PIPE, &mut client, &mut upstream).unwrap();
        assert_eq!(end, ClientEnd::NextClient);
        assert_eq!(frames(&upstream.sent), messages);
        assert!(client.received.is_empty());
    }

    /// A message longer than the 4096-byte buffer makes ReadFile return 0
    /// (ERROR_MORE_DATA); the reader ends without forwarding it, and nothing after it is read.
    #[test]
    fn inbound_pipe_ends_client_on_more_data() {
        let first = b"{'speed': 1.0}".to_vec();
        let mut client = FakeClient::sending(std::slice::from_ref(&first));
        client.reads.push_back(ClientRead::Fail(win32::ERROR_MORE_DATA));
        client.reads.push_back(ClientRead::Chunk(b"rest of the long message".to_vec()));
        let mut upstream = FakeUpstream::replying(&[]);
        let err = relay_client(&FM_TELEMETRY_PIPE, &mut client, &mut upstream).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(win32::ERROR_MORE_DATA as i32));
        assert_eq!(frames(&upstream.sent), [first]);
        assert_eq!(client.reads.len(), 1);
    }

    /// The handshake (packet type 1) starts the session, after which each per-frame
    /// packet is answered once with openrigcue's reply and the session goes on until the client closes.
    #[test]
    fn handshake_session_answers_every_packet() {
        let handshake = header(3, 1);
        let packets = [handshake.clone(), per_frame_packet(), per_frame_packet()];
        let mut client = FakeClient::sending(&packets);
        let mut upstream = FakeUpstream::replying(&[&INITIAL_REPLY, &FRAME_REPLY, &FRAME_REPLY]);
        let end = relay_client(&DYNAMIC_CAB_PIPE, &mut client, &mut upstream).unwrap();
        assert_eq!(end, ClientEnd::NextClient);
        assert_eq!(frames(&upstream.sent), packets);
        assert_eq!(client.received, [INITIAL_REPLY.to_vec(), FRAME_REPLY.to_vec(), FRAME_REPLY.to_vec()]);
    }

    /// A client whose first packet is not a handshake gets one per-frame reply and is
    /// then disconnected; its next packet is never read.
    #[test]
    fn handshake_session_ends_after_reply_without_handshake() {
        let mut client = FakeClient::sending(&[per_frame_packet(), header(3, 1)]);
        let mut upstream = FakeUpstream::replying(&[&FRAME_REPLY, &INITIAL_REPLY]);
        let end = relay_client(&DYNAMIC_CAB_PIPE, &mut client, &mut upstream).unwrap();
        assert_eq!(end, ClientEnd::NextClient);
        assert_eq!(frames(&upstream.sent), [per_frame_packet()]);
        assert_eq!(client.received, [FRAME_REPLY.to_vec()]);
        assert_eq!(client.reads.len(), 1);
    }

    /// The packet type is a signed byte compared with 1; 0x81 (-127) is not a handshake.
    #[test]
    fn handshake_session_only_type_one_is_a_handshake() {
        let mut client = FakeClient::sending(&[header(3, -127), header(3, 1)]);
        let mut upstream = FakeUpstream::replying(&[&FRAME_REPLY]);
        assert_eq!(relay_client(&DYNAMIC_CAB_PIPE, &mut client, &mut upstream).unwrap(), ClientEnd::NextClient);
        assert_eq!(client.received, [FRAME_REPLY.to_vec()]);
    }

    /// A packet shorter than the 3-byte header ends the server thread: the packet is passed on, no reply is
    /// awaited, and the pipe is no longer served, also after a handshake.
    #[test]
    fn handshake_session_short_packet_stops_the_server() {
        for packets in [vec![vec![3, 0]], vec![header(3, 1), vec![7]], vec![header(3, 1), Vec::new()]] {
            let mut client = FakeClient::sending(&packets);
            let mut upstream = FakeUpstream::replying(&[&INITIAL_REPLY]);
            let end = relay_client(&DYNAMIC_CAB_PIPE, &mut client, &mut upstream).unwrap();
            assert_eq!(end, ClientEnd::StopServing);
            assert_eq!(frames(&upstream.sent), packets);
        }
    }

    /// A failed reply write ends the client.
    #[test]
    fn handshake_session_ends_on_write_failure() {
        let mut client = FakeClient::sending(&[header(3, 1), per_frame_packet()]);
        client.write_fails = true;
        let mut upstream = FakeUpstream::replying(&[&INITIAL_REPLY, &FRAME_REPLY]);
        assert!(relay_client(&DYNAMIC_CAB_PIPE, &mut client, &mut upstream).is_err());
        assert_eq!(frames(&upstream.sent), [header(3, 1)]);
    }

    #[test]
    fn handshake_session_fails_when_openrigcue_hangs_up() {
        let mut client = FakeClient::sending(&[header(3, 1)]);
        let mut upstream = FakeUpstream::replying(&[]);
        let err = relay_client(&DYNAMIC_CAB_PIPE, &mut client, &mut upstream).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionAborted);
    }
}
