//! The socket pattern of the small synchronous listeners: every liveness test binds a new socket, waits for
//! one datagram and forwards it; some listeners keep that socket to read more datagrams in the receive step.

use std::fmt;
use std::io;

use super::net::{Datagrams, Endpoint, Network};
use crate::udp_racing::forward::Forwarder;
use crate::udp_racing::socket::ImportedPort;

/// Why a liveness receive produced no datagram. Listeners treat the two differently: several record a
/// bind failure in the game errors but not a receive timeout.
#[derive(Debug)]
pub enum ReceiveFailure {
    /// The bind (or the timeout setting) failed.
    Bind(io::Error),
    /// The receive failed (timeout included).
    Receive(io::Error),
}

impl fmt::Display for ReceiveFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReceiveFailure::Bind(e) | ReceiveFailure::Receive(e) => e.fmt(f),
        }
    }
}

/// A listener's receive side.
pub struct Receiver {
    net: Box<dyn Network>,
    endpoint: Endpoint,
    imported_port: Option<ImportedPort>,
    max_len: usize,
    forwarder: Forwarder,
    /// Whether received datagrams are forwarded (false for the listeners that never forward).
    forwards: bool,
    kept: Option<Box<dyn Datagrams>>,
}

impl Receiver {
    pub fn new(net: Box<dyn Network>, endpoint: Endpoint, max_len: usize) -> Self {
        Receiver {
            net,
            endpoint,
            imported_port: None,
            max_len,
            forwarder: Forwarder::default(),
            forwards: true,
            kept: None,
        }
    }

    /// A source whose integer port is range-checked only after creating the socket.
    pub fn with_imported_port(net: Box<dyn Network>, endpoint: Endpoint, max_len: usize, port: ImportedPort) -> Self {
        Self { imported_port: Some(port), ..Self::new(net, endpoint, max_len) }
    }

    fn bind(&mut self) -> io::Result<Box<dyn Datagrams>> {
        match self.imported_port {
            Some(port) => self.net.bind_imported(&self.endpoint, port),
            None => self.net.bind(&self.endpoint),
        }
    }

    /// A receive side whose datagrams are never forwarded.
    pub fn without_forwarding(net: Box<dyn Network>, endpoint: Endpoint, max_len: usize) -> Self {
        Receiver { forwards: false, ..Receiver::new(net, endpoint, max_len) }
    }

    /// A cached integer port for a listener that never forwards.
    pub fn without_forwarding_imported(
        net: Box<dyn Network>,
        endpoint: Endpoint,
        max_len: usize,
        port: ImportedPort,
    ) -> Self {
        Self { forwards: false, ..Self::with_imported_port(net, endpoint, max_len, port) }
    }

    /// Use `forwarder` for forwarding (tests give it their own parameters).
    #[cfg(test)]
    pub fn set_forwarder(&mut self, forwarder: Forwarder) {
        self.forwarder = forwarder;
    }

    fn forward(&self, data: &[u8]) {
        if self.forwards {
            self.forwarder.forward(data);
        }
    }

    /// Bind a fresh socket, receive one datagram (forwarded) and close the socket.
    pub fn receive_once(&mut self) -> Result<Vec<u8>, ReceiveFailure> {
        self.kept = None;
        let mut socket = self.bind().map_err(ReceiveFailure::Bind)?;
        let (data, _) = socket.receive(self.max_len).map_err(ReceiveFailure::Receive)?;
        self.forward(&data);
        Ok(data)
    }

    /// Bind a fresh socket, receive one datagram (forwarded) and keep the socket for [`Receiver::receive_more`].
    /// A failed receive drops the socket.
    pub fn receive_and_keep(&mut self) -> Result<Vec<u8>, ReceiveFailure> {
        self.kept = None;
        let mut socket = self.bind().map_err(ReceiveFailure::Bind)?;
        let (data, _) = socket.receive(self.max_len).map_err(ReceiveFailure::Receive)?;
        self.forward(&data);
        self.kept = Some(socket);
        Ok(data)
    }

    /// Receive another datagram on the kept socket and forward it; a failure closes the socket.
    pub fn receive_more(&mut self) -> io::Result<Vec<u8>> {
        let data = self.receive_more_unforwarded()?;
        self.forward(&data);
        Ok(data)
    }

    /// Receive another datagram on the kept socket without forwarding it (for listeners whose receive step
    /// does not forward); a failure closes the socket.
    pub fn receive_more_unforwarded(&mut self) -> io::Result<Vec<u8>> {
        let socket = self.kept.as_mut().ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "no socket"))?;
        match socket.receive(self.max_len) {
            Ok((data, _)) => Ok(data),
            Err(e) => {
                self.kept = None;
                Err(e)
            }
        }
    }

    pub fn close(&mut self) {
        self.kept = None;
    }
}
