//! The non-blocking message pump of a SimConnect server connection.

use std::collections::VecDeque;
use std::io::{self, Read, Write};

use super::wire::{decode_server, ClientMessage, ServerMessage, WireError};

/// Bytes sent and received per pump at most.
const MAX_PACKET: usize = 4096;

/// Why the pump failed.
#[derive(Debug, thiserror::Error)]
pub enum PumpError {
    #[error("socket error: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Wire(#[from] WireError),
    /// The peer reset the connection and every buffered message has been delivered.
    #[error("SimConnect connection closed")]
    Closed,
}

/// A connection to the simulator's SimConnect server, over a non-blocking stream.
pub struct Connection<S> {
    stream: S,
    alive: bool,
    outgoing: Vec<u8>,
    incoming: Vec<u8>,
    ready: VecDeque<ServerMessage>,
}

impl<S: Read + Write> Connection<S> {
    pub fn new(stream: S) -> Self {
        Connection { stream, alive: true, outgoing: Vec::new(), incoming: Vec::new(), ready: VecDeque::new() }
    }

    /// The underlying stream (tests inspect what was sent).
    #[cfg(test)]
    pub(crate) fn stream(&self) -> &S {
        &self.stream
    }

    /// Only queues the encoded message.
    pub fn queue(&mut self, message: &ClientMessage, protocol: u32, counter: u32) {
        self.outgoing.extend_from_slice(&message.encode(protocol, counter));
    }

    /// One pump: send up to 4096 queued bytes, read up to 4096 bytes, parse at most one
    /// message, return the oldest parsed one. A reset peer stops the socket traffic; once nothing is left the
    /// pump reports [`PumpError::Closed`]. A graceful close (a zero-byte read) goes unnoticed.
    pub fn pump(&mut self) -> Result<Option<ServerMessage>, PumpError> {
        if self.alive {
            if !self.outgoing.is_empty() {
                let end = self.outgoing.len().min(MAX_PACKET);
                match self.stream.write(&self.outgoing[..end]) {
                    Ok(sent) => {
                        self.outgoing.drain(..sent);
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(error) => return Err(error.into()),
                }
            }
            let mut buffer = [0u8; MAX_PACKET];
            match self.stream.read(&mut buffer) {
                Ok(read) => self.incoming.extend_from_slice(&buffer[..read]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) if error.kind() == io::ErrorKind::ConnectionReset => self.alive = false,
                Err(error) => return Err(error.into()),
            }
        }
        if let Some((message, used)) = decode_server(&self.incoming)? {
            self.ready.push_back(message);
            self.incoming.drain(..used);
        }
        match self.ready.pop_front() {
            Some(message) => Ok(Some(message)),
            None if !self.alive => Err(PumpError::Closed),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::collections::VecDeque;
    use std::io::{self, Read, Write};

    /// A scripted non-blocking stream.
    #[derive(Default)]
    pub(crate) struct FakeStream {
        pub(crate) reads: VecDeque<io::Result<Vec<u8>>>,
        pub(crate) written: Vec<u8>,
        pub(crate) accept_per_write: Option<usize>,
    }

    impl Read for FakeStream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.reads.pop_front() {
                Some(Ok(bytes)) => {
                    buf[..bytes.len()].copy_from_slice(&bytes);
                    Ok(bytes.len())
                }
                Some(Err(error)) => Err(error),
                None => Err(io::ErrorKind::WouldBlock.into()),
            }
        }
    }

    impl Write for FakeStream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let n = self.accept_per_write.map_or(buf.len(), |n| n.min(buf.len()));
            self.written.extend_from_slice(&buf[..n]);
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::wire::tests::server_frame;
    use super::testing::FakeStream;
    use super::*;

    fn quit() -> ServerMessage {
        ServerMessage::Unhandled { repr: "<SQuit: >".into() }
    }

    /// At most 4096 bytes go out per pump, one message is parsed per pump.
    #[test]
    fn pump_sends_in_packets_and_parses_one_message_per_call() {
        let mut connection = Connection::new(FakeStream::default());
        let open = ClientMessage::RequestSystemState { request_id: 0, state_name: "Sim" };
        for counter in 0..20 {
            connection.queue(&open, 4, counter);
        }
        let mut two = server_frame(3, &[]);
        two.extend_from_slice(&server_frame(3, &[]));
        connection.stream.reads.push_back(Ok(two));
        assert_eq!(connection.pump().unwrap(), Some(quit()));
        assert_eq!(connection.stream.written.len(), 4096);
        assert_eq!(connection.pump().unwrap(), Some(quit()));
        assert_eq!(connection.pump().unwrap(), None);
        assert_eq!(connection.stream.written.len(), 20 * 276);
    }

    /// A connection reset ends traffic; buffered messages are still delivered, then Closed.
    #[test]
    fn reset_then_closed() {
        let mut connection = Connection::new(FakeStream::default());
        connection.stream.reads.push_back(Ok(server_frame(3, &[])));
        connection.stream.reads.push_back(Err(io::ErrorKind::ConnectionReset.into()));
        assert_eq!(connection.pump().unwrap(), Some(quit()));
        assert!(matches!(connection.pump(), Err(PumpError::Closed)));
    }

    /// A zero-byte read (graceful close) is not an error for the pump.
    #[test]
    fn graceful_close_goes_unnoticed() {
        let mut connection = Connection::new(FakeStream::default());
        connection.stream.reads.push_back(Ok(Vec::new()));
        assert_eq!(connection.pump().unwrap(), None);
        connection.stream.reads.push_back(Err(io::ErrorKind::BrokenPipe.into()));
        assert!(matches!(connection.pump(), Err(PumpError::Io(_))));
    }
}
