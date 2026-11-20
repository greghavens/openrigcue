//! The receive pattern of the F1 2013–2017 and GRID listeners: the liveness test opens a fresh socket, waits for one
//! datagram, stores it, forwards it and closes the socket; the receive step parses that datagram.

use std::sync::Arc;

use super::packet::{Layout, MotionPacket};
use crate::udp_racing::forward::Forwarder;
use crate::udp_racing::parameters::GameErrors;
use crate::udp_racing::socket::{socket_creation_failed, DatagramSocket};

/// Receive buffer size.
const RECEIVE_SIZE: usize = 2048;

/// Fresh-socket probe plus the datagram it last received.
pub(super) struct ProbeReceiver {
    socket: Box<dyn DatagramSocket>,
    forwarder: Arc<Forwarder>,
    /// The stored datagram: `None` until the first datagram, then the last one received.
    data: Option<Vec<u8>>,
    /// Where the texts of a failed bind or timeout setting go (the game errors): the F1 listener's
    /// handle; `None` for GRID, which records none.
    pub(super) game_errors: Option<GameErrors>,
}

impl ProbeReceiver {
    pub(super) fn new(
        socket: Box<dyn DatagramSocket>,
        forwarder: Arc<Forwarder>,
        game_errors: Option<GameErrors>,
    ) -> Self {
        ProbeReceiver { socket, forwarder, data: None, game_errors }
    }

    /// The liveness test: the game is running when a non-empty
    /// datagram arrives within the timeout.
    pub(super) fn probe(&mut self) -> bool {
        let forwarder = self.forwarder.clone();
        self.probe_with_forward(|datagram| forwarder.forward(datagram))
    }

    fn probe_with_forward(&mut self, forward: impl FnOnce(&[u8])) -> bool {
        if let Err(error) = self.socket.reopen() {
            if !socket_creation_failed(&error) {
                if let Some(game_errors) = self.game_errors.as_mut() {
                    game_errors.record(&error);
                }
            }
            self.socket.close();
            return false;
        }
        let datagram = match self.socket.receive(RECEIVE_SIZE) {
            Ok(datagram) => datagram,
            Err(_) => {
                self.socket.close();
                return false;
            }
        };
        let running = !datagram.is_empty();
        self.data = Some(datagram);
        // The datagram is stored before forwarding,
        // then the receiving socket is explicitly closed after the forwarder returns.
        forward(self.data.as_deref().expect("the received datagram was just stored"));
        self.socket.close();
        running
    }

    /// The stored datagram the receive step parses (the dispatcher runs the liveness test before every
    /// receive step, so it is the datagram of the probe just made).
    pub(super) fn data(&self) -> Option<&[u8]> {
        self.data.as_deref()
    }
}

/// Parses a datagram: the 304-byte F1 layout from its first 304 bytes,
/// else the extradata=3 layout from its first 256 bytes; `None` when neither fits (the previous packet stays).
pub(super) fn parse_extradata(data: &[u8]) -> Option<MotionPacket> {
    MotionPacket::decode_prefix(data, Layout::Extended)
        .or_else(|| MotionPacket::decode_prefix(data, Layout::Extradata3))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::udp_racing::socket::scripted::ScriptedSocket;

    #[test]
    fn native_probe_creation_and_bind_failures_keep_separate_error_prefixes() {
        use std::io;
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct StagedSocket {
            script: ScriptedSocket,
            calls: AtomicUsize,
        }
        impl DatagramSocket for StagedSocket {
            fn reopen(&self) -> io::Result<()> {
                match self.calls.fetch_add(1, Ordering::Relaxed) {
                    0 => Err(crate::udp_racing::socket::socket_creation_error(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "fixture allocation denied",
                    ))),
                    1 => Err(io::Error::new(io::ErrorKind::InvalidInput, "bind(): port must be 0-65535.")),
                    2 => Err(io::Error::new(io::ErrorKind::PermissionDenied, "fixture settimeout denied")),
                    _ => self.script.reopen(),
                }
            }
            fn close(&self) {
                self.script.close();
            }
            fn receive(&self, max_len: usize) -> io::Result<Vec<u8>> {
                self.script.receive(max_len)
            }
        }
        for report_errors in [false, true] {
            let script = ScriptedSocket::new();
            script.push(b"recovery");
            let socket = StagedSocket { script: script.clone(), calls: AtomicUsize::new(0) };
            let errors = report_errors.then(GameErrors::default);
            let mut receiver = ProbeReceiver::new(Box::new(socket), Arc::default(), errors);
            for expected_errors in [0, 1, 2] {
                assert!(!receiver.probe_with_forward(|_| panic!("failed setup cannot forward")));
                assert!(receiver.data().is_none());
                if let Some(errors) = &receiver.game_errors {
                    assert_eq!(errors.messages().len(), expected_errors);
                    assert!(!errors.messages().contains("fixture allocation denied"));
                }
            }
            assert!(receiver.probe_with_forward(|data| {
                assert_eq!(data, b"recovery");
                assert!(script.is_open());
            }));
            assert_eq!(receiver.data(), Some(b"recovery".as_slice()));
            if let Some(errors) = &receiver.game_errors {
                assert_eq!(errors.messages().len(), 2, "F1 clears errors later, when receiveData parses");
            }
        }
    }

    #[test]
    fn native_probe_forwards_before_socket_close_including_empty_datagrams() {
        for bytes in [b"frame".as_slice(), b"".as_slice()] {
            let socket = ScriptedSocket::new();
            socket.push(bytes);
            let mut receiver = ProbeReceiver::new(Box::new(socket.clone()), Arc::default(), None);
            let running = receiver.probe_with_forward(|data| {
                assert_eq!(data, bytes);
                assert!(socket.is_open(), "the original forwards before sock.close()");
                assert_eq!(socket.activity().closed, 0);
            });
            assert_eq!(running, !bytes.is_empty());
            assert_eq!(receiver.data(), Some(bytes));
            assert!(!socket.is_open());
            assert_eq!(socket.activity().closed, 1);
        }
    }

    #[test]
    fn native_probe_failed_bind_and_receive_preserve_data_without_forwarding() {
        for errors in [None, Some(GameErrors::default())] {
            let socket = ScriptedSocket::new();
            socket.push(b"prior");
            let mut receiver = ProbeReceiver::new(Box::new(socket.clone()), Arc::default(), errors);
            assert!(receiver.probe_with_forward(|data| assert_eq!(data, b"prior")));
            socket.refuse_bind(true);
            socket.push(b"pending");
            let before = socket.activity().receives.len();
            assert!(!receiver.probe_with_forward(|_| panic!("failed bind cannot forward")));
            assert_eq!(socket.activity().receives.len(), before, "the bind handler returns immediately");
            assert_eq!(socket.pending(), 1);
            assert_eq!(receiver.data(), Some(b"prior".as_slice()));
            socket.refuse_bind(false);
            assert!(receiver.probe_with_forward(|data| assert_eq!(data, b"pending")));
            socket.push_timeout();
            assert!(!receiver.probe_with_forward(|_| panic!("failed receive cannot forward")));
            assert_eq!(receiver.data(), Some(b"pending".as_slice()));
            assert!(!socket.is_open());
        }
    }
}
