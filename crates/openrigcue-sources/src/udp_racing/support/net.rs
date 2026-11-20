//! UDP transport shared by the racing listeners.
//!
//! Every listener talks to a plain IPv4 datagram socket. The listeners use the
//! [`Network`] trait so tests can replace the operating system with scripted datagrams.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::Duration;

use crate::udp_racing::socket::{open_imported_with, ImportedPort};
use socket2::{Domain, Protocol, Socket, Type};

/// Largest datagram the system socket accepts; anything longer than a listener's receive size is rejected.
const MAX_DATAGRAM: usize = 65_536;

/// How a listener binds its socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// Local address and port (`0.0.0.0` for "all interfaces").
    pub bind: SocketAddrV4,
    /// Receive timeout; `None` blocks.
    pub timeout: Option<Duration>,
    /// `SO_BROADCAST`.
    pub broadcast: bool,
    /// `SO_REUSEADDR`.
    pub reuse_address: bool,
    /// Multicast group joined on `INADDR_ANY` (`IP_ADD_MEMBERSHIP`).
    pub multicast_group: Option<Ipv4Addr>,
}

/// The interval of a receive timeout given in seconds (such as `1/7`) as the system gets it: the
/// float becomes whole nanoseconds rounded away from zero, and the wait gets that interval
/// rounded up to the unit of its call ([`socket_timeout`]). `1/7` s is 142,857,143 ns and a 143 ms wait.
pub fn timeout_secs(seconds: f64) -> Duration {
    socket_timeout((seconds * 1e9).ceil() as u64)
}

/// A receive timeout of `nanos` (whole nanoseconds) as the socket wait gets it: rounded up
/// to `poll()`'s milliseconds, or to `select()`'s microseconds on Windows.
pub const fn socket_timeout(nanos: u64) -> Duration {
    let unit = if cfg!(windows) { 1_000 } else { 1_000_000 };
    Duration::from_nanos(nanos.div_ceil(unit) * unit)
}

impl Endpoint {
    /// A plain socket bound to `ip:port` with a receive timeout.
    pub fn new(ip: Ipv4Addr, port: u16, timeout: Duration) -> Self {
        Endpoint {
            bind: SocketAddrV4::new(ip, port),
            timeout: Some(timeout),
            broadcast: false,
            reuse_address: false,
            multicast_group: None,
        }
    }

    /// Bound to the loopback interface, like the PC-only listeners.
    pub fn loopback(port: u16, timeout: Duration) -> Self {
        Endpoint::new(Ipv4Addr::LOCALHOST, port, timeout)
    }

    /// Bound to every interface, like the console-capable listeners.
    pub fn any(port: u16, timeout: Duration) -> Self {
        Endpoint::new(Ipv4Addr::UNSPECIFIED, port, timeout)
    }
}

/// A bound datagram socket.
pub trait Datagrams: Send {
    /// Change the receive timeout after a protocol handshake. Compatibility fakes may ignore it.
    fn set_read_timeout(&self, _timeout: Option<Duration>) -> io::Result<()> {
        Ok(())
    }

    /// Receive one datagram. A datagram longer than `max_len` is an error, as a bounded receive is on
    /// Windows (WSAEMSGSIZE); a timeout is an error of kind `WouldBlock` or `TimedOut`.
    fn receive(&mut self, max_len: usize) -> io::Result<(Vec<u8>, SocketAddr)>;

    /// Send one datagram.
    fn send_to(&mut self, data: &[u8], to: SocketAddr) -> io::Result<()>;

    /// A second handle to the same socket, for a second thread using the same socket: both can
    /// wait in a receive at the same time and each datagram goes to one of them.
    fn try_clone(&self) -> io::Result<Box<dyn Datagrams>>;
}

/// Creates sockets.
pub trait Network: Send {
    /// Create and bind a socket.
    fn bind(&mut self, endpoint: &Endpoint) -> io::Result<Box<dyn Datagrams>>;

    /// Compatibility for injected networks; the system transport defers this range check until after creation.
    fn bind_imported(&mut self, endpoint: &Endpoint, port: ImportedPort) -> io::Result<Box<dyn Datagrams>> {
        let mut endpoint = endpoint.clone();
        endpoint.bind.set_port(port.for_bind()?);
        self.bind(&endpoint)
    }

    /// A session-local setting. The system transport reads it after allocating and configuring the socket.
    fn bind_live(
        &mut self,
        endpoint: &Endpoint,
        read_port: &mut dyn FnMut() -> io::Result<ImportedPort>,
    ) -> io::Result<Box<dyn Datagrams>> {
        self.bind_imported(endpoint, read_port()?)
    }
}

/// The operating system's UDP stack.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemNetwork;

impl Network for SystemNetwork {
    fn bind(&mut self, endpoint: &Endpoint) -> io::Result<Box<dyn Datagrams>> {
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        configure(socket, endpoint)
    }

    fn bind_imported(&mut self, endpoint: &Endpoint, port: ImportedPort) -> io::Result<Box<dyn Datagrams>> {
        bind_imported_with(
            port,
            || Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)),
            |socket| configure_options(socket, endpoint),
            |socket, port| {
                let mut endpoint = endpoint.clone();
                endpoint.bind.set_port(port);
                bind_configured(socket, &endpoint)
            },
        )
    }

    fn bind_live(
        &mut self,
        endpoint: &Endpoint,
        read_port: &mut dyn FnMut() -> io::Result<ImportedPort>,
    ) -> io::Result<Box<dyn Datagrams>> {
        bind_live_with(
            read_port,
            || Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)),
            |socket| configure_options(socket, endpoint),
            |socket, port| {
                let mut endpoint = endpoint.clone();
                endpoint.bind.set_port(port);
                let socket = bind_socket(socket, &endpoint)?;
                Ok(Box::new(SystemSocket { socket }) as Box<dyn Datagrams>)
            },
        )
    }
}

fn bind_live_with<S, T>(
    read_port: &mut dyn FnMut() -> io::Result<ImportedPort>,
    create: impl FnOnce() -> io::Result<S>,
    prepare: impl FnOnce(&S) -> io::Result<()>,
    bind: impl FnOnce(S, u16) -> io::Result<T>,
) -> io::Result<T> {
    let socket = create()?;
    prepare(&socket)?;
    let port = read_port()?.for_bind()?;
    bind(socket, port)
}

fn bind_imported_with<S, T>(
    port: ImportedPort,
    create: impl FnOnce() -> io::Result<S>,
    prepare: impl FnOnce(&S) -> io::Result<()>,
    bind: impl FnOnce(S, u16) -> io::Result<T>,
) -> io::Result<T> {
    // Gran Turismo's socket creation and SO_BROADCAST share one failure path before bind checks the raw port.
    open_imported_with(
        port,
        || {
            let socket = create()?;
            prepare(&socket)?;
            Ok(socket)
        },
        bind,
    )
}

fn configure_options(socket: &Socket, endpoint: &Endpoint) -> io::Result<()> {
    if endpoint.reuse_address {
        socket.set_reuse_address(true)?;
    }
    if endpoint.broadcast {
        socket.set_broadcast(true)?;
    }
    Ok(())
}

fn configure(socket: Socket, endpoint: &Endpoint) -> io::Result<Box<dyn Datagrams>> {
    configure_options(&socket, endpoint)?;
    bind_configured(socket, endpoint)
}

fn bind_configured(socket: Socket, endpoint: &Endpoint) -> io::Result<Box<dyn Datagrams>> {
    let socket = bind_socket(socket, endpoint)?;
    socket.set_read_timeout(endpoint.timeout)?;
    Ok(Box::new(SystemSocket { socket }))
}

fn bind_socket(socket: Socket, endpoint: &Endpoint) -> io::Result<UdpSocket> {
    socket.bind(&SocketAddr::V4(endpoint.bind).into())?;
    if let Some(group) = endpoint.multicast_group {
        socket.join_multicast_v4(&group, &Ipv4Addr::UNSPECIFIED)?;
    }
    Ok(socket.into())
}

#[cfg(test)]
mod imported_tests {
    use super::*;
    use crate::udp_racing::socket::socket_creation_failed;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[test]
    fn live_port_read_follows_socket_options_and_is_skipped_on_setup_failure() {
        for failure in ["", "create", "options", "read", "range"] {
            let calls = RefCell::new(Vec::new());
            let mut read = || {
                calls.borrow_mut().push("read");
                match failure {
                    "read" => Err(io::Error::other("bad port")),
                    "range" => Ok(ImportedPort::OutOfRange),
                    _ => Ok(ImportedPort::Valid(6001)),
                }
            };
            let result = bind_live_with(
                &mut read,
                || {
                    calls.borrow_mut().push("create");
                    if failure == "create" {
                        Err(io::Error::other("create"))
                    } else {
                        Ok(())
                    }
                },
                |_| {
                    calls.borrow_mut().extend(["reuse", "broadcast"]);
                    if failure == "options" {
                        Err(io::Error::other("options"))
                    } else {
                        Ok(())
                    }
                },
                |_, port| {
                    assert_eq!(port, 6001);
                    calls.borrow_mut().push("bind");
                    Ok(())
                },
            );
            let expected = match failure {
                "create" => vec!["create"],
                "options" => vec!["create", "reuse", "broadcast"],
                "read" | "range" => vec!["create", "reuse", "broadcast", "read"],
                _ => vec!["create", "reuse", "broadcast", "read", "bind"],
            };
            assert_eq!(*calls.borrow(), expected, "{failure}");
            assert_eq!(result.is_err(), !failure.is_empty());
        }
    }

    #[test]
    fn broadcast_setup_precedes_native_range_validation_and_has_its_own_failure_boundary() {
        struct Lease(Rc<RefCell<Vec<&'static str>>>);
        impl Drop for Lease {
            fn drop(&mut self) {
                self.0.borrow_mut().push("drop");
            }
        }
        for (port, failure, expected, setup_failure) in [
            (ImportedPort::OutOfRange, "", vec!["create", "broadcast", "drop"], false),
            (ImportedPort::OutOfRange, "create", vec!["create"], true),
            (ImportedPort::OutOfRange, "broadcast", vec!["create", "broadcast", "drop"], true),
            (ImportedPort::Valid(6001), "", vec!["create", "broadcast", "bind", "timeout", "drop"], false),
        ] {
            let calls = Rc::new(RefCell::new(Vec::new()));
            let result = bind_imported_with(
                port,
                || {
                    calls.borrow_mut().push("create");
                    if failure == "create" {
                        Err(io::Error::other("create failed"))
                    } else {
                        Ok(Lease(calls.clone()))
                    }
                },
                |_| {
                    calls.borrow_mut().push("broadcast");
                    if failure == "broadcast" {
                        Err(io::Error::other("option failed"))
                    } else {
                        Ok(())
                    }
                },
                |_, actual_port| {
                    assert_eq!(actual_port, 6001);
                    calls.borrow_mut().extend(["bind", "timeout"]);
                    Ok(())
                },
            );
            assert_eq!(*calls.borrow(), expected, "{port:?} {failure}");
            if let Err(error) = result {
                assert_eq!(socket_creation_failed(&error), setup_failure, "{port:?} {failure}");
                if !setup_failure {
                    assert_eq!(error.to_string(), "bind(): port must be 0-65535.");
                }
            } else {
                assert_eq!(port, ImportedPort::Valid(6001));
            }
        }
    }

    #[test]
    fn numeric_network_fallback_retains_endpoint_options_and_refuses_out_of_range() {
        struct Capture(Vec<Endpoint>);
        impl Network for Capture {
            fn bind(&mut self, endpoint: &Endpoint) -> io::Result<Box<dyn Datagrams>> {
                self.0.push(endpoint.clone());
                Err(io::Error::other("fake bind"))
            }
        }
        let mut network = Capture(Vec::new());
        let mut endpoint = Endpoint::loopback(1, Duration::from_millis(17));
        endpoint.broadcast = true;
        endpoint.reuse_address = true;
        endpoint.multicast_group = Some(Ipv4Addr::new(239, 1, 2, 3));
        assert!(network.bind_imported(&endpoint, ImportedPort::Valid(6001)).is_err());
        let mut expected = endpoint.clone();
        expected.bind.set_port(6001);
        assert_eq!(network.0, [expected]);
        let error = network.bind_imported(&endpoint, ImportedPort::OutOfRange).err().unwrap();
        assert_eq!(error.to_string(), "bind(): port must be 0-65535.");
        assert_eq!(network.0.len(), 1, "compatibility fallback does not bind an invented port");
    }
}

struct SystemSocket {
    socket: UdpSocket,
}

impl Datagrams for SystemSocket {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.socket.set_read_timeout(timeout)
    }

    fn receive(&mut self, max_len: usize) -> io::Result<(Vec<u8>, SocketAddr)> {
        let mut buf = vec![0u8; MAX_DATAGRAM];
        let (len, from) = self.socket.recv_from(&mut buf)?;
        if len > max_len {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "datagram larger than the receive buffer"));
        }
        buf.truncate(len);
        Ok((buf, from))
    }

    fn send_to(&mut self, data: &[u8], to: SocketAddr) -> io::Result<()> {
        self.socket.send_to(data, to).map(|_| ())
    }

    fn try_clone(&self) -> io::Result<Box<dyn Datagrams>> {
        Ok(Box::new(SystemSocket { socket: self.socket.try_clone()? }))
    }
}

#[cfg(test)]
pub mod fake {
    //! A scripted network for tests: datagrams are queued per bound port.

    use super::*;
    use std::collections::{HashMap, VecDeque};
    use std::sync::{Arc, Mutex};

    /// What a scripted receive returns.
    #[derive(Debug, Clone)]
    pub enum Incoming {
        Datagram(Vec<u8>),
        Timeout,
        /// A receive error other than a timeout (connection reset).
        Failure,
    }

    #[derive(Debug, Default)]
    pub struct State {
        pub queues: HashMap<u16, VecDeque<Incoming>>,
        pub bound: Vec<Endpoint>,
        pub sent: Vec<(Vec<u8>, SocketAddr)>,
        pub refuse_bind: bool,
        pub refuse_send: bool,
    }

    /// Cloneable handle; every clone shares the same script.
    #[derive(Debug, Clone, Default)]
    pub struct FakeNetwork(pub Arc<Mutex<State>>);

    impl FakeNetwork {
        pub fn push(&self, port: u16, datagram: &[u8]) {
            self.0.lock().unwrap().queues.entry(port).or_default().push_back(Incoming::Datagram(datagram.to_vec()));
        }
        pub fn push_timeout(&self, port: u16) {
            self.0.lock().unwrap().queues.entry(port).or_default().push_back(Incoming::Timeout);
        }
        pub fn push_failure(&self, port: u16) {
            self.0.lock().unwrap().queues.entry(port).or_default().push_back(Incoming::Failure);
        }
        pub fn bound(&self) -> Vec<Endpoint> {
            self.0.lock().unwrap().bound.clone()
        }
        pub fn sent(&self) -> Vec<(Vec<u8>, SocketAddr)> {
            self.0.lock().unwrap().sent.clone()
        }
        pub fn refuse_bind(&self, refuse: bool) {
            self.0.lock().unwrap().refuse_bind = refuse;
        }
        pub fn refuse_send(&self, refuse: bool) {
            self.0.lock().unwrap().refuse_send = refuse;
        }
    }

    impl Network for FakeNetwork {
        fn bind(&mut self, endpoint: &Endpoint) -> io::Result<Box<dyn Datagrams>> {
            let mut state = self.0.lock().unwrap();
            if state.refuse_bind {
                return Err(io::Error::new(io::ErrorKind::AddrInUse, "address in use"));
            }
            state.bound.push(endpoint.clone());
            Ok(Box::new(FakeSocket { net: self.0.clone(), port: endpoint.bind.port() }))
        }
    }

    struct FakeSocket {
        net: Arc<Mutex<State>>,
        port: u16,
    }

    impl Datagrams for FakeSocket {
        fn receive(&mut self, max_len: usize) -> io::Result<(Vec<u8>, SocketAddr)> {
            let next = self.net.lock().unwrap().queues.get_mut(&self.port).and_then(VecDeque::pop_front);
            match next {
                Some(Incoming::Datagram(d)) if d.len() > max_len => {
                    Err(io::Error::new(io::ErrorKind::InvalidData, "datagram larger than the receive buffer"))
                }
                Some(Incoming::Datagram(d)) => Ok((d, SocketAddr::from(([192, 168, 1, 50], 40_000)))),
                Some(Incoming::Timeout) | None => Err(io::Error::new(io::ErrorKind::TimedOut, "timed out")),
                Some(Incoming::Failure) => Err(io::Error::new(io::ErrorKind::ConnectionReset, "connection reset")),
            }
        }

        fn send_to(&mut self, data: &[u8], to: SocketAddr) -> io::Result<()> {
            let mut state = self.net.lock().unwrap();
            if state.refuse_send {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "send refused"));
            }
            state.sent.push((data.to_vec(), to));
            Ok(())
        }

        fn try_clone(&self) -> io::Result<Box<dyn Datagrams>> {
            Ok(Box::new(FakeSocket { net: self.net.clone(), port: self.port }))
        }
    }

    /// A loopback socket that stands in for a port-forwarding target (the forwarder sends real datagrams).
    pub struct ForwardSink {
        socket: UdpSocket,
    }

    impl ForwardSink {
        pub fn new() -> Self {
            let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
            socket.set_read_timeout(Some(std::time::Duration::from_millis(200))).unwrap();
            ForwardSink { socket }
        }

        /// The `PORT_FORWARDING` entry that reaches this sink.
        pub fn target(&self) -> String {
            format!("127.0.0.1:{}", self.socket.local_addr().unwrap().port())
        }

        /// Every datagram forwarded so far (waits up to 200 ms for the next one).
        pub fn received(&self) -> Vec<Vec<u8>> {
            let mut buffer = [0u8; MAX_DATAGRAM];
            std::iter::from_fn(|| self.socket.recv(&mut buffer).ok().map(|n| buffer[..n].to_vec())).collect()
        }
    }
}

#[cfg(test)]
mod timeout_tests {
    use super::{socket_timeout, timeout_secs};
    use std::time::Duration;

    /// Every fractional receive timeout of the listeners: whole nanoseconds, then the wait's unit. The exact
    /// decimal timeouts (0.05, 0.1, 0.125, 0.2 s) convert without rounding.
    #[test]
    fn fractional_timeouts_follow_the_original_conversion() {
        for (seconds, nanos, poll_ms, select_us) in [
            (1.0 / 7.0, 142_857_143, 143, 142_858),
            (1.0 / 15.0, 66_666_667, 67, 66_667),
            (1.0 / 3.0, 333_333_334, 334, 333_334),
            (0.05, 50_000_000, 50, 50_000),
            (0.1, 100_000_000, 100, 100_000),
            (0.125, 125_000_000, 125, 125_000),
            (0.2, 200_000_000, 200, 200_000),
        ] {
            let expected =
                if cfg!(windows) { Duration::from_micros(select_us) } else { Duration::from_millis(poll_ms) };
            assert_eq!(timeout_secs(seconds), expected, "{seconds}");
            assert_eq!(socket_timeout(nanos), expected, "{nanos} ns");
        }
    }
}
