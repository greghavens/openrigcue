//! UDP socket handling shared by the UDP listeners: bind, receive timeout, one-datagram receive with the
//! buffer-size rule, all behind [`DatagramSocket`].
//!
//! The listeners all use an IPv4 datagram socket, a bind to an address and port,
//! a receive timeout and a bounded receive. Listeners talk to the [`DatagramSocket`] trait so tests can script
//! datagrams and failures; [`UdpEndpoint`] is the operating-system implementation.

use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use openrigcue_model::native_value::LegacyValue;
use socket2::{Domain, Socket, Type};

/// Largest UDP payload; the receive buffer is this big so an oversized datagram can be detected.
const MAX_DATAGRAM: usize = 65_536;

/// A port integer cached when a listener is built. Socket range validation happens later, in bind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportedPort {
    Valid(u16),
    OutOfRange,
}

impl ImportedPort {
    /// The integer value of a setting, without imposing the socket's range yet.
    /// None means the value has no integer conversion.
    pub fn from_native(value: &LegacyValue) -> Option<Self> {
        let integer = |value: i128| u16::try_from(value).map_or(Self::OutOfRange, Self::Valid);
        let text = |text: &str| {
            let normalized = openrigcue_model::python_numeric::python_int_text(text)?;
            Some(normalized.parse::<i128>().map_or(Self::OutOfRange, integer))
        };
        match value {
            LegacyValue::Int(value) => Some(integer(i128::from(*value))),
            LegacyValue::Bool(value) => Some(Self::Valid(u16::from(*value))),
            LegacyValue::Float(value) if value.is_finite() => {
                let value = value.trunc();
                Some(if (0.0..65536.0).contains(&value) { Self::Valid(value as u16) } else { Self::OutOfRange })
            }
            LegacyValue::Str(value) => text(value),
            LegacyValue::Bytes(value) if value.is_ascii() => text(std::str::from_utf8(value).ok()?),
            _ => None,
        }
    }

    pub(crate) fn for_bind(self) -> io::Result<u16> {
        match self {
            Self::Valid(port) => Ok(port),
            Self::OutOfRange => Err(io::Error::new(io::ErrorKind::InvalidInput, "bind(): port must be 0-65535.")),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct SocketCreationError(#[source] io::Error);

pub(crate) fn socket_creation_error(error: io::Error) -> io::Error {
    io::Error::new(error.kind(), SocketCreationError(error))
}

pub(crate) fn socket_creation_failed(error: &io::Error) -> bool {
    error.get_ref().is_some_and(|error| error.is::<SocketCreationError>())
}

/// Keep socket allocation ahead of the bind argument check. The callbacks also admit pure fake resources.
pub(crate) fn open_imported_with<S, T>(
    port: ImportedPort,
    create: impl FnOnce() -> io::Result<S>,
    bind: impl FnOnce(S, u16) -> io::Result<T>,
) -> io::Result<T> {
    let socket = create().map_err(socket_creation_error)?;
    let port = port.for_bind()?;
    bind(socket, port)
}

/// Where a listener binds and how long one receive may block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Endpoint {
    pub address: Ipv4Addr,
    pub port: u16,
    pub timeout: Duration,
}

impl Endpoint {
    /// Bound to `127.0.0.1`, like the PC-only listeners.
    pub fn loopback(port: u16, timeout: Duration) -> Self {
        Endpoint { address: Ipv4Addr::LOCALHOST, port, timeout }
    }

    /// Bound to every interface (`0.0.0.0`), like the console-capable listeners.
    pub fn any_interface(port: u16, timeout: Duration) -> Self {
        Endpoint { address: Ipv4Addr::UNSPECIFIED, port, timeout }
    }
}

/// A listener's datagram socket. Methods take `&self` so one socket can be shared between a receiver thread and
/// the dispatcher thread, as the F1 2018 listener does.
pub trait DatagramSocket: Send + Sync {
    /// Drop the current socket (if any), then create and bind a new one with the receive timeout.
    fn reopen(&self) -> io::Result<()>;

    /// Socket creation on its own: a new, unbound socket becomes the current one, where
    /// another thread's `close` can already reach it. A socket that does not separate the steps leaves
    /// everything to [`DatagramSocket::bind_current`].
    fn create(&self) -> io::Result<()> {
        Ok(())
    }

    /// Bind and set the timeout on the current socket. Other threads can run during both
    /// calls; a socket another thread closed since [`DatagramSocket::create`] fails here as a closed
    /// descriptor does.
    fn bind_current(&self) -> io::Result<()> {
        self.reopen()
    }

    /// Close the current socket. Receiving afterwards fails until the next [`DatagramSocket::reopen`].
    fn close(&self);

    /// Wait (up to the timeout) for one datagram. Like a bounded receive on Windows, a datagram longer than
    /// `max_len` is an error rather than being truncated. A timeout is an error too.
    fn receive(&self, max_len: usize) -> io::Result<Vec<u8>>;
}

/// The operating system's UDP socket for one [`Endpoint`].
#[derive(Debug)]
pub struct UdpEndpoint {
    endpoint: Endpoint,
    imported_port: Option<ImportedPort>,
    socket: Mutex<Option<Arc<UdpSocket>>>,
}

impl UdpEndpoint {
    /// An endpoint with no socket yet; call [`DatagramSocket::reopen`] to bind it.
    pub fn new(endpoint: Endpoint) -> Self {
        UdpEndpoint { endpoint, imported_port: None, socket: Mutex::new(None) }
    }

    /// A cached port integer. No socket is allocated until the listener calls reopen.
    pub fn from_imported_port(address: Ipv4Addr, port: ImportedPort, timeout: Duration) -> Self {
        let endpoint = Endpoint { address, port: 0, timeout };
        UdpEndpoint { endpoint, imported_port: Some(port), socket: Mutex::new(None) }
    }

    /// Local port of the bound socket (tests bind port 0).
    #[cfg(test)]
    fn local_port(&self) -> Option<u16> {
        let socket = self.lock().clone()?;
        socket.local_addr().ok().map(|addr| addr.port())
    }

    fn lock(&self) -> MutexGuard<'_, Option<Arc<UdpSocket>>> {
        self.socket.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl DatagramSocket for UdpEndpoint {
    fn reopen(&self) -> io::Result<()> {
        let mut slot = self.lock();
        // The previous socket is released before binding.
        *slot = None;
        let socket = match self.imported_port {
            None => bind(self.endpoint)?,
            Some(port) => open_imported_with(
                port,
                || Socket::new(Domain::IPV4, Type::DGRAM, None),
                |socket, port| {
                    socket.bind(&SocketAddrV4::new(self.endpoint.address, port).into())?;
                    let socket: UdpSocket = socket.into();
                    socket.set_read_timeout(Some(self.endpoint.timeout))?;
                    Ok(socket)
                },
            )?,
        };
        *slot = Some(Arc::new(socket));
        Ok(())
    }

    fn create(&self) -> io::Result<()> {
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, None).map_err(socket_creation_error)?;
        // Replacing the current socket releases the previous one.
        *self.lock() = Some(Arc::new(socket.into()));
        Ok(())
    }

    fn bind_current(&self) -> io::Result<()> {
        // OS error 9 (bad file descriptor): what the calls on a socket that has been closed fail with.
        let closed = || io::Error::from_raw_os_error(9);
        let socket = self.lock().clone().ok_or_else(closed)?;
        let port = match self.imported_port {
            None => self.endpoint.port,
            Some(port) => port.for_bind()?,
        };
        socket2::SockRef::from(&*socket).bind(&SocketAddrV4::new(self.endpoint.address, port).into())?;
        // The timeout is set after the bind: closed in between, it finds no descriptor.
        if !self.lock().as_ref().is_some_and(|current| Arc::ptr_eq(current, &socket)) {
            return Err(closed());
        }
        socket.set_read_timeout(Some(self.endpoint.timeout))
    }

    fn close(&self) {
        *self.lock() = None;
    }

    fn receive(&self, max_len: usize) -> io::Result<Vec<u8>> {
        let socket = self.open_socket()?;
        receive_one(&socket, max_len)
    }
}

impl UdpEndpoint {
    fn open_socket(&self) -> io::Result<Arc<UdpSocket>> {
        self.lock().clone().ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "socket is closed"))
    }
}

/// A UDP listener's user settings: its `NETWORK_PORT` value and its `PORT_FORWARDING` entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenerSettings {
    pub port: u16,
}

impl ListenerSettings {
    /// The listener's port.
    pub fn with_port(port: u16) -> Self {
        ListenerSettings { port }
    }
}

/// Create a UDP socket bound to `endpoint` with its receive timeout set.
fn bind(endpoint: Endpoint) -> io::Result<UdpSocket> {
    let socket = UdpSocket::bind(SocketAddrV4::new(endpoint.address, endpoint.port))?;
    socket.set_read_timeout(Some(endpoint.timeout))?;
    Ok(socket)
}

/// Receive one datagram, failing when it is longer than `max_len` (the Windows bounded-receive rule).
fn receive_one(socket: &UdpSocket, max_len: usize) -> io::Result<Vec<u8>> {
    let mut buffer = vec![0u8; MAX_DATAGRAM];
    let length = socket.recv(&mut buffer)?;
    check_length(length, max_len)?;
    buffer.truncate(length);
    Ok(buffer)
}

fn check_length(length: usize, max_len: usize) -> io::Result<()> {
    if length > max_len {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "datagram larger than the receive buffer"));
    }
    Ok(())
}

/// A scripted [`DatagramSocket`] for listener tests.
#[cfg(test)]
pub(crate) mod scripted {
    use super::*;
    use std::collections::VecDeque;

    /// What happened on a scripted socket, for assertions.
    #[derive(Debug, Default, Clone, PartialEq, Eq)]
    pub struct Activity {
        pub reopened: usize,
        pub closed: usize,
        pub receives: Vec<usize>,
    }

    #[derive(Debug, Default)]
    struct State {
        incoming: VecDeque<io::Result<Vec<u8>>>,
        refuse_bind: bool,
        open: bool,
        activity: Activity,
    }

    /// Plays back queued datagrams and failures; an empty queue behaves like a receive timeout.
    #[derive(Debug, Clone, Default)]
    pub struct ScriptedSocket(Arc<Mutex<State>>);

    impl ScriptedSocket {
        pub fn new() -> Self {
            ScriptedSocket::default()
        }

        pub fn push(&self, datagram: &[u8]) {
            self.state().incoming.push_back(Ok(datagram.to_vec()));
        }

        pub fn push_timeout(&self) {
            self.state().incoming.push_back(Err(io::Error::new(io::ErrorKind::WouldBlock, "timed out")));
        }

        pub fn refuse_bind(&self, refuse: bool) {
            self.state().refuse_bind = refuse;
        }

        pub fn is_open(&self) -> bool {
            self.state().open
        }

        pub fn activity(&self) -> Activity {
            self.state().activity.clone()
        }

        pub fn pending(&self) -> usize {
            self.state().incoming.len()
        }

        fn state(&self) -> MutexGuard<'_, State> {
            self.0.lock().unwrap()
        }
    }

    impl DatagramSocket for ScriptedSocket {
        fn reopen(&self) -> io::Result<()> {
            let mut state = self.state();
            state.activity.reopened += 1;
            if state.refuse_bind {
                state.open = false;
                return Err(io::Error::new(io::ErrorKind::AddrInUse, "address in use"));
            }
            state.open = true;
            Ok(())
        }

        fn close(&self) {
            let mut state = self.state();
            state.activity.closed += 1;
            state.open = false;
        }

        fn receive(&self, max_len: usize) -> io::Result<Vec<u8>> {
            let mut state = self.state();
            state.activity.receives.push(max_len);
            if !state.open {
                return Err(io::Error::new(io::ErrorKind::NotConnected, "socket is closed"));
            }
            let Some(next) = state.incoming.pop_front() else {
                drop(state);
                // An empty script behaves like a short receive timeout, so receiver threads do not spin.
                std::thread::sleep(Duration::from_millis(1));
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "timed out"));
            };
            let datagram = next?;
            check_length(datagram.len(), max_len)?;
            Ok(datagram)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_imported_port_keeps_integer_conversion_separate_from_bind_range() {
        let valid = [
            (LegacyValue::from("\u{3000}１２_٣\u{3000}"), ImportedPort::Valid(123)),
            (LegacyValue::from("-0000"), ImportedPort::Valid(0)),
            (LegacyValue::Float(-0.9), ImportedPort::Valid(0)),
            (LegacyValue::Bool(true), ImportedPort::Valid(1)),
            (LegacyValue::Int(65535), ImportedPort::Valid(65535)),
            (LegacyValue::from("+0070000"), ImportedPort::OutOfRange),
            (LegacyValue::Int(-1), ImportedPort::OutOfRange),
            (LegacyValue::Float(f64::MAX), ImportedPort::OutOfRange),
            (LegacyValue::from("9".repeat(4300)), ImportedPort::OutOfRange),
            (LegacyValue::Bytes(b"70000".to_vec()), ImportedPort::OutOfRange),
        ];
        for (value, expected) in valid {
            assert_eq!(ImportedPort::from_native(&value), Some(expected), "{value:?}");
        }
        for value in [
            LegacyValue::from("1.0"),
            LegacyValue::from("0".repeat(4301)),
            LegacyValue::None,
            LegacyValue::Float(f64::NAN),
            LegacyValue::Float(f64::INFINITY),
            LegacyValue::List(vec![1.into()]),
            LegacyValue::Bytes(vec![0xff]),
        ] {
            assert_eq!(ImportedPort::from_native(&value), None, "{value:?}");
        }
    }

    #[test]
    fn native_imported_socket_creation_precedes_range_validation_and_failed_resources_close() {
        use std::cell::RefCell;
        use std::rc::Rc;
        struct Lease(Rc<RefCell<Vec<&'static str>>>);
        impl Drop for Lease {
            fn drop(&mut self) {
                self.0.borrow_mut().push("close");
            }
        }
        for port in [ImportedPort::Valid(1337), ImportedPort::OutOfRange] {
            let trace = Rc::new(RefCell::new(Vec::new()));
            let result = open_imported_with(
                port,
                || {
                    trace.borrow_mut().push("create");
                    Ok(Lease(trace.clone()))
                },
                |socket, port| {
                    trace.borrow_mut().push("bind");
                    assert_eq!(port, 1337);
                    drop(socket);
                    Ok(port)
                },
            );
            if port == ImportedPort::OutOfRange {
                let error = result.unwrap_err();
                assert_eq!(error.to_string(), "bind(): port must be 0-65535.");
                assert!(!socket_creation_failed(&error));
                assert_eq!(*trace.borrow(), ["create", "close"]);
            } else {
                assert_eq!(result.unwrap(), 1337);
                assert_eq!(*trace.borrow(), ["create", "bind", "close"]);
            }
        }
        let error = open_imported_with::<(), ()>(
            ImportedPort::OutOfRange,
            || Err(io::Error::new(io::ErrorKind::PermissionDenied, "fixture allocation denied")),
            |_, _| panic!("an allocation failure precedes port validation and bind"),
        )
        .unwrap_err();
        assert!(socket_creation_failed(&error));
        assert_eq!(error.to_string(), "fixture allocation denied");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    fn loopback_endpoint() -> UdpEndpoint {
        let endpoint = UdpEndpoint::new(Endpoint::loopback(0, Duration::from_millis(200)));
        endpoint.reopen().unwrap();
        endpoint
    }

    fn send_to(endpoint: &UdpEndpoint, payload: &[u8]) {
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        sender.send_to(payload, ("127.0.0.1", endpoint.local_port().unwrap())).unwrap();
    }

    #[test]
    fn receives_one_datagram_like_recvfrom() {
        let endpoint = loopback_endpoint();
        send_to(&endpoint, b"hello");
        assert_eq!(endpoint.receive(512).unwrap(), b"hello");
    }

    #[test]
    fn oversized_datagram_is_an_error_like_windows_recvfrom() {
        let endpoint = loopback_endpoint();
        send_to(&endpoint, &[7u8; 600]);
        assert_eq!(endpoint.receive(512).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn receive_times_out_and_closed_socket_fails() {
        let endpoint = loopback_endpoint();
        let error = endpoint.receive(512).unwrap_err();
        assert!(matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut));
        endpoint.close();
        assert_eq!(endpoint.receive(512).unwrap_err().kind(), io::ErrorKind::NotConnected);
    }

    /// Creation, then bind and timeout: the published socket can be closed
    /// between the steps, and the calls then fail as on a closed descriptor.
    #[test]
    fn a_socket_closed_between_creation_and_bind_fails_with_a_bad_descriptor() {
        let endpoint = UdpEndpoint::new(Endpoint::loopback(0, Duration::from_millis(50)));
        endpoint.create().unwrap();
        assert_eq!(endpoint.local_port(), Some(0), "created, not yet bound");
        endpoint.close();
        let error = endpoint.bind_current().unwrap_err();
        assert_eq!(error.raw_os_error(), Some(9));
        assert_eq!(crate::printed::os_error(&error), "[Errno 9] Bad file descriptor");
        endpoint.create().unwrap();
        endpoint.bind_current().unwrap();
        assert_ne!(endpoint.local_port(), Some(0));
        send_to(&endpoint, b"after bind");
        assert_eq!(endpoint.receive(512).unwrap(), b"after bind");
    }

    #[test]
    fn reopen_releases_the_previous_socket_before_binding_the_same_port() {
        let endpoint = loopback_endpoint();
        let port = endpoint.local_port().unwrap();
        let fixed = UdpEndpoint::new(Endpoint::loopback(port, Duration::from_millis(50)));
        drop(endpoint);
        fixed.reopen().unwrap();
        fixed.reopen().unwrap();
        assert_eq!(fixed.local_port(), Some(port));
    }
}
