//! UDP port forwarding: raw datagrams a listener receives are re-sent to the user's targets.
//!
//! For one datagram: when the running listener has a `PORT_FORWARDING` section, each `host:port` entry of
//! that section gets the datagram from a new UDP socket. Any failure prints
//! "something went wrong while doing the udp port forward" with the error text.
//!
//! The target loop looks the running listener and its
//! section up again, so a listener change between the membership test and the loop is a failure.
//! The target address is converted in this order: the host text, the port as a C `int` (an overflow is reworded
//! to the port-range message), the host lookup, and only then the 0-65535 test of the port.
//!
//! The forwarder takes no listener argument: whichever listener received the datagram (the dispatcher's running
//! listener, or a receive thread of another listener such as the always-on F1 2019–2025 threads), it reads
//! the running listener and `PORT_FORWARDING` on every datagram, so a Games-page edit of
//! `PORT_FORWARDING` applies to the next datagram. The running listener is
//! [`Parameters::listener_running`]; the service installs a live configuration reader in [`PortForwarding`].
//! One failure handler covers the whole target loop, so the
//! first malformed entry or failed send ends forwarding of that datagram (later targets are skipped) and the
//! message is printed again for every datagram.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use super::parameters::Parameters;
use super::support::resolver::{convert_host, resolve_converted, HostError};
use crate::udp_flight::python::{parse_int_text_checked, IntTextError, PyInt};

/// One `"host:port"` forwarding target, split at `:` with its port converted to an integer.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ForwardTarget {
    host: String,
    port: PyInt,
}

/// Why forwarding failed for one entry, with the printed error text.
#[derive(Debug)]
enum ForwardError {
    /// The listener's section has gone since the
    /// membership test.
    Key(String),
    /// An entry without `:` has no port.
    NoPort,
    /// The port text is not an integer.
    InvalidPort(String),
    DigitLimit(IntTextError),
    /// The port is outside 0–65535.
    PortRange,
    /// The host did not convert or resolve.
    Host(HostError),
    /// Creating the socket or sending failed.
    Io(io::Error),
}

impl fmt::Display for ForwardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ForwardError::Key(key) => f.write_str(&openrigcue_model::python_repr::repr_str(key)),
            ForwardError::NoPort => f.write_str("list index out of range"),
            ForwardError::InvalidPort(text) => {
                let shown: String = openrigcue_model::python_repr::repr_str(text).chars().take(200).collect();
                write!(f, "invalid literal for int() with base 10: {shown}")
            }
            ForwardError::DigitLimit(error) => error.fmt(f),
            ForwardError::PortRange => f.write_str("sendto(): port must be 0-65535."),
            ForwardError::Host(error) => error.fmt(f),
            ForwardError::Io(error) => f.write_str(&crate::printed::os_error(error)),
        }
    }
}

impl ForwardTarget {
    /// Splits an entry at `:`: the first piece is the host, the second the port as an integer.
    fn parse(entry: &str) -> Result<ForwardTarget, ForwardError> {
        let mut parts = entry.split(':');
        let host = parts.next().unwrap_or_default().to_string();
        let text = parts.next().ok_or(ForwardError::NoPort)?;
        let port = match parse_int_text_checked(text) {
            Ok(port) => port,
            Err(IntTextError::Invalid) => return Err(ForwardError::InvalidPort(text.to_string())),
            Err(error @ IntTextError::DigitLimit(_)) => return Err(ForwardError::DigitLimit(error)),
        };
        Ok(ForwardTarget { host, port })
    }

    /// The address conversion of a send, in its order: host text, port
    /// as a C `int`, host lookup, port range.
    fn address(&self) -> Result<SocketAddrV4, ForwardError> {
        let host = convert_host(&self.host).map_err(ForwardError::Host)?;
        let port = match self.port {
            PyInt::Small(port) => i32::try_from(port).map_err(|_| ForwardError::PortRange)?,
            PyInt::Large => return Err(ForwardError::PortRange),
        };
        let host = resolve_converted(&host).map_err(ForwardError::Host)?;
        let port = u16::try_from(port).map_err(|_| ForwardError::PortRange)?;
        Ok(SocketAddrV4::new(host, port))
    }
}

/// The `PORT_FORWARDING` settings: listener id → its `"host:port"` entries, as the Games page edits
/// them.
#[derive(Default)]
pub struct PortForwarding {
    entries: Mutex<BTreeMap<String, Vec<String>>>,
    reader: Mutex<Option<ForwardingEntriesReader>>,
}

/// Current ordinary string-list entries for one listener, read at each datagram.
pub type ForwardingEntriesReader = Arc<dyn Fn(&str) -> Option<Vec<String>> + Send + Sync>;

impl fmt::Debug for PortForwarding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PortForwarding").field("entries", &self.lock()).finish_non_exhaustive()
    }
}

impl PortForwarding {
    /// The instance of the process, whose live reader the service installs before listeners start.
    pub fn shared() -> Arc<PortForwarding> {
        static SHARED: OnceLock<Arc<PortForwarding>> = OnceLock::new();
        SHARED.get_or_init(Arc::default).clone()
    }

    /// The instance a listener uses by default: the process-wide one, or in unit tests a fresh one so that
    /// tests running in parallel do not see each other's entries.
    pub(crate) fn for_listener() -> Arc<PortForwarding> {
        #[cfg(not(test))]
        return PortForwarding::shared();
        #[cfg(test)]
        return Arc::default();
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, Vec<String>>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Replace the whole section (as loaded or saved).
    pub fn set(&self, entries: BTreeMap<String, Vec<String>>) {
        *self.lock() = entries;
    }

    /// Sets one listener's entries (the Games page).
    pub fn set_entries(&self, key: &str, entries: Vec<String>) {
        self.lock().insert(key.to_owned(), entries);
    }

    /// Use live settings without depending on a dispatcher tick. Cached setters remain
    /// available for compatibility callers; a live reader is authoritative when installed.
    pub fn set_reader(&self, reader: ForwardingEntriesReader) {
        *self.reader.lock().unwrap_or_else(PoisonError::into_inner) = Some(reader);
    }

    /// One listener's entries, when it has a section.
    fn entries(&self, key: &str) -> Option<Vec<String>> {
        // Release our lock before invoking code that acquires the configuration lock.
        // refresh_context takes those locks in the opposite order when updating the cache.
        let reader = self.reader.lock().unwrap_or_else(PoisonError::into_inner).clone();
        match reader {
            Some(reader) => reader(key),
            None => self.lock().get(key).cloned(),
        }
    }
}

/// The UDP forwarder, with the shared state it reads.
#[derive(Debug, Clone)]
pub struct Forwarder {
    parameters: Arc<Parameters>,
    port_forwarding: Arc<PortForwarding>,
}

impl Default for Forwarder {
    /// The process-wide parameters and `PORT_FORWARDING` (in unit tests, fresh ones).
    fn default() -> Self {
        Forwarder::with(Parameters::for_listener(), PortForwarding::for_listener())
    }
}

impl Forwarder {
    /// A forwarder reading the given parameters and `PORT_FORWARDING`.
    pub fn with(parameters: Arc<Parameters>, port_forwarding: Arc<PortForwarding>) -> Self {
        Forwarder { parameters, port_forwarding }
    }

    /// Forwards one datagram.
    pub fn forward(&self, datagram: &[u8]) {
        self.forward_with(datagram, send_all, |line| crate::printed::line(line));
    }

    fn forward_with(
        &self,
        datagram: &[u8],
        send: impl FnOnce(&[String], &[u8]) -> Result<(), ForwardError>,
        print: impl FnOnce(&str),
    ) {
        // The membership test: does the running listener have a section?
        if self.port_forwarding.entries(&self.parameters.listener_running()).is_none() {
            return;
        }
        // The target loop reads the running listener and its section again.
        let running = self.parameters.listener_running();
        let sent = match self.port_forwarding.entries(&running) {
            Some(targets) => send(&targets, datagram),
            None => Err(ForwardError::Key(running)),
        };
        if let Err(error) = sent {
            print(&format!("something went wrong while doing the udp port forward {error}"));
        }
    }
}

/// The target loop: one new socket per target.
fn send_all(entries: &[String], datagram: &[u8]) -> Result<(), ForwardError> {
    for entry in entries {
        let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)).map_err(ForwardError::Io)?;
        let target = ForwardTarget::parse(entry)?;
        socket.send_to(datagram, target.address()?).map_err(ForwardError::Io)?;
    }
    Ok(())
}

/// A forwarder for tests: the running listener is `listener` and
/// its `PORT_FORWARDING` section is `entries`; the returned parameters change the running listener.
#[cfg(test)]
pub(crate) fn test_forwarder(listener: &str, entries: &[String]) -> (Forwarder, Arc<Parameters>) {
    let (parameters, port_forwarding) = (Arc::new(Parameters::default()), Arc::new(PortForwarding::default()));
    parameters.set_listener_running(listener);
    port_forwarding.set_entries(listener, entries.to_vec());
    (Forwarder::with(parameters.clone(), port_forwarding), parameters)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn live_forwarding_reader_is_authoritative_at_each_datagram() {
        let (forwarder, parameters, forwarding) = forwarder();
        parameters.set_listener_running("f12025");
        forwarding.set_entries("f12025", vec!["stale:1".into()]);
        let current =
            Arc::new(Mutex::new(BTreeMap::<String, Vec<String>>::from([("f12025".into(), vec!["first:2".into()])])));
        forwarding.set_reader({
            let current = current.clone();
            Arc::new(move |key| current.lock().unwrap().get(key).cloned())
        });
        let mut sent = Vec::new();
        for (key, targets) in [
            ("f12025", Some(vec!["first:2".into()])),
            ("f12025", Some(vec!["second:3".into(), "third:4".into()])),
            ("grid", Some(Vec::new())),
            ("f12025", None),
        ] {
            parameters.set_listener_running(key);
            match targets.clone() {
                Some(targets) => {
                    current.lock().unwrap().insert(key.into(), targets);
                }
                None => {
                    current.lock().unwrap().remove(key);
                }
            }
            // The old dispatcher cache can continue to refresh; it cannot override live data.
            forwarding.set(BTreeMap::from([(key.into(), vec!["stale:9".into()])]));
            let before = sent.len();
            forwarder.forward_with(
                b"frame",
                |entries, packet| {
                    sent.push((entries.to_vec(), packet.to_vec()));
                    Ok(())
                },
                |_| panic!("valid targets"),
            );
            if let Some(targets) = targets {
                assert_eq!(sent.last(), Some(&(targets, b"frame".to_vec())));
            } else {
                assert_eq!(sent.len(), before, "a removed live key must not revive the stale cache");
            }
        }
    }

    #[test]
    fn live_forwarding_reader_runs_without_forwarding_locks() {
        let (_, _, forwarding) = forwarder();
        let weak = Arc::downgrade(&forwarding);
        forwarding.set_reader(Arc::new(move |_| {
            let forwarding = weak.upgrade().unwrap();
            assert!(forwarding.reader.try_lock().is_ok(), "reader lock retained across callback");
            assert!(forwarding.entries.try_lock().is_ok(), "cache lock retained across callback");
            forwarding.set_entries("grid", vec!["compatibility:1".into()]);
            Some(vec!["live:2".into()])
        }));
        assert_eq!(forwarding.entries("grid"), Some(vec!["live:2".into()]));
    }

    fn receiver() -> (UdpSocket, u16) {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        let port = socket.local_addr().unwrap().port();
        (socket, port)
    }

    fn received(socket: &UdpSocket) -> Option<Vec<u8>> {
        let mut buffer = [0u8; 2048];
        socket.recv(&mut buffer).ok().map(|n| buffer[..n].to_vec())
    }

    fn forwarder() -> (Forwarder, Arc<Parameters>, Arc<PortForwarding>) {
        let (parameters, port_forwarding) = (Arc::new(Parameters::default()), Arc::new(PortForwarding::default()));
        (Forwarder::with(parameters.clone(), port_forwarding.clone()), parameters, port_forwarding)
    }

    #[test]
    fn native_forwarding_failure_prints_once_with_the_completed_target_prefix() {
        let (forwarder, parameters, port_forwarding) = forwarder();
        parameters.set_listener_running("grid");
        port_forwarding.set_entries("grid", vec!["host:1337".into(), "host:x'y".into(), "host:2333".into()]);
        let mut sent = Vec::new();
        let mut printed = Vec::new();
        forwarder.forward_with(
            b"frame",
            |targets, bytes| {
                assert_eq!(bytes, b"frame");
                for target in targets {
                    sent.push(ForwardTarget::parse(target)?);
                }
                Ok(())
            },
            |line| printed.push(line.to_owned()),
        );
        assert_eq!(sent, [ForwardTarget { host: "host".into(), port: PyInt::Small(1337) }]);
        assert_eq!(
            printed,
            [r#"something went wrong while doing the udp port forward invalid literal for int() with base 10: "x'y""#]
        );
    }

    #[test]
    fn native_forwarding_integer_error_uses_python_repr_and_200_character_precision() {
        for (text, expected) in
            [("a\nb".to_owned(), "'a\\nb'".to_owned()), ("x".repeat(250), format!("'{}", "x".repeat(199)))]
        {
            let error = ForwardTarget::parse(&format!("host:{text}")).unwrap_err();
            assert_eq!(error.to_string(), format!("invalid literal for int() with base 10: {expected}"));
        }
    }

    #[test]
    fn native_forwarding_catch_preserves_io_text_and_skips_absent_entries() {
        let (forwarder, parameters, port_forwarding) = forwarder();
        forwarder.forward_with(b"none", |_, _| panic!("no configured entry"), |_| panic!("nothing to print"));
        parameters.set_listener_running("grid");
        port_forwarding.set_entries("grid", vec!["host:1337".into()]);
        let mut printed = Vec::new();
        forwarder.forward_with(
            b"frame",
            |_, _| Err(ForwardError::Io(io::Error::new(io::ErrorKind::PermissionDenied, "fixture send denied"))),
            |line| printed.push(line.to_owned()),
        );
        assert_eq!(printed, ["something went wrong while doing the udp port forward fixture send denied"]);
    }

    #[test]
    fn parses_targets_like_split_and_int() {
        // An entry splits at ':' into the host and the integer port
        assert_eq!(
            ForwardTarget::parse("192.168.1.5:20777").ok(),
            Some(ForwardTarget { host: "192.168.1.5".into(), port: PyInt::Small(20777) })
        );
        assert_eq!(
            ForwardTarget::parse("192.168.1.5:20777").unwrap().address().unwrap().to_string(),
            "192.168.1.5:20777"
        );
        assert_eq!(ForwardTarget::parse("host: 1_000 :ignored").map(|t| t.port).ok(), Some(PyInt::Small(1000)));
        assert_eq!(ForwardTarget::parse("host:+42").map(|t| t.port).ok(), Some(PyInt::Small(42)));
        let error = |entry: &str| match ForwardTarget::parse(entry) {
            Ok(target) => target.address().unwrap_err().to_string(),
            Err(error) => error.to_string(),
        };
        assert_eq!(error("no-port"), "list index out of range");
        assert_eq!(error("host:"), "invalid literal for int() with base 10: ''");
        assert_eq!(error("host:0x10"), "invalid literal for int() with base 10: '0x10'");
        assert_eq!(error("host:1__0"), "invalid literal for int() with base 10: '1__0'");
        assert_eq!(error("127.0.0.1:65536"), "sendto(): port must be 0-65535.");
        assert_eq!(error("127.0.0.1:-1"), "sendto(): port must be 0-65535.");
        assert_eq!(error("host:99999999999999999999999"), "sendto(): port must be 0-65535.");
        assert_eq!(error("host:2147483648"), "sendto(): port must be 0-65535.", "no C int: refused before a lookup");
        assert_eq!(error("a\0b:1"), "host name must not contain null character");
        // The host text is converted before the port and the port's range.
        assert_eq!(error("bücher..de:70000"), "encoding of hostname failed");
        assert_eq!(error("bücher..de:4294967296"), "encoding of hostname failed");
        assert_eq!(error("\0é:1"), "host name must not contain null character");
        assert_eq!(ForwardTarget::parse(&format!("host:{}7", "0".repeat(4299))).unwrap().port, PyInt::Small(7));
        assert_eq!(error(&format!("host:+{}7", "０_".repeat(4300))), "Exceeds the limit (4300 digits) for integer string conversion: value has 4301 digits; use sys.set_int_max_str_digits() to increase the limit");
        assert_eq!(error(&format!("host:{}", "9".repeat(4300))), "sendto(): port must be 0-65535.");
    }

    #[test]
    fn forwards_to_the_running_listeners_entries() {
        // Forwarding happens only when the running listener is a PORT_FORWARDING key.
        let (socket, port) = receiver();
        let (forwarder, parameters, port_forwarding) = forwarder();
        port_forwarding.set_entries("forza", vec![format!("127.0.0.1:{port}")]);
        forwarder.forward(b"listener_running is 'none'");
        assert_eq!(received(&socket), None);
        parameters.set_listener_running("forza");
        forwarder.forward(b"datagram");
        assert_eq!(received(&socket).as_deref(), Some(&b"datagram"[..]));
        parameters.set_listener_running("ac");
        forwarder.forward(b"ac has no entry");
        assert_eq!(received(&socket), None);
    }

    #[test]
    fn entries_are_read_on_every_datagram() {
        // The Games page assigns a listener's entries; the next datagram uses the new list.
        let (first, first_port) = receiver();
        let (second, second_port) = receiver();
        let (forwarder, parameters, port_forwarding) = forwarder();
        parameters.set_listener_running("gt7");
        port_forwarding.set_entries("gt7", vec![format!("127.0.0.1:{first_port}")]);
        forwarder.forward(b"one");
        port_forwarding.set_entries("gt7", vec![format!("127.0.0.1:{second_port}")]);
        forwarder.forward(b"two");
        assert_eq!(received(&first).as_deref(), Some(&b"one"[..]));
        assert_eq!(received(&second).as_deref(), Some(&b"two"[..]));
        assert_eq!(received(&first), None);
    }

    #[test]
    fn a_malformed_entry_ends_the_target_loop() {
        // One failure handler covers the loop, so the first bad entry stops later targets.
        let (first, first_port) = receiver();
        let (later, later_port) = receiver();
        let (forwarder, parameters, port_forwarding) = forwarder();
        parameters.set_listener_running("f12024");
        port_forwarding.set_entries(
            "f12024",
            vec![format!("127.0.0.1:{first_port}"), "broken".into(), format!("127.0.0.1:{later_port}")],
        );
        forwarder.forward(b"x");
        forwarder.forward(b"y");
        assert_eq!(received(&first).as_deref(), Some(&b"x"[..]));
        assert_eq!(received(&first).as_deref(), Some(&b"y"[..]), "the entries are parsed again for each datagram");
        assert_eq!(received(&later), None);
    }

    #[test]
    fn a_failed_send_skips_the_remaining_targets() {
        // A failed send ends the loop for this datagram.
        let (later, later_port) = receiver();
        let (forwarder, parameters, port_forwarding) = forwarder();
        parameters.set_listener_running("dirt");
        // Broadcasting without SO_BROADCAST is refused by the kernel, like an unreachable target.
        port_forwarding.set_entries("dirt", vec!["<broadcast>:9".to_string(), format!("127.0.0.1:{later_port}")]);
        forwarder.forward(b"x");
        assert_eq!(received(&later), None);
    }

    /// The loop reads the running listener and the section again: a key gone since the membership test
    /// is a handled failure.
    #[test]
    fn a_listener_change_between_the_test_and_the_loop_is_a_key_error() {
        let (forwarder, parameters, forwarding) = forwarder();
        parameters.set_listener_running("grid");
        let reads = Arc::new(Mutex::new(0));
        forwarding.set_reader({
            let reads = reads.clone();
            Arc::new(move |key| {
                let mut reads = reads.lock().unwrap();
                *reads += 1;
                (*reads == 1).then(|| vec![format!("{key}:1")])
            })
        });
        let mut printed = Vec::new();
        forwarder.forward_with(b"frame", |_, _| panic!("no list to walk"), |line| printed.push(line.to_owned()));
        assert_eq!(printed, ["something went wrong while doing the udp port forward 'grid'"]);
        assert_eq!(*reads.lock().unwrap(), 2);
    }

    /// Whether the test runs in the private fixture, whose hosts file is the only source of names.
    fn fixture_names() -> bool {
        std::fs::read_to_string("/etc/hosts").is_ok_and(|hosts| hosts.contains("sink.fixture"))
    }

    /// A target may name its host; the send resolves it with IPv4 hints. A name the resolver does not know is
    /// a resolver error, reported before the port's range is looked at; a port that is no C `int` is refused
    /// before the lookup.
    #[test]
    fn a_named_target_is_resolved_by_sendto() {
        if !fixture_names() {
            return;
        }
        let (sink, port) = receiver();
        let (forwarder, parameters, port_forwarding) = forwarder();
        parameters.set_listener_running("forza");
        port_forwarding.set_entries("forza", vec![format!("sink.fixture:{port}")]);
        forwarder.forward(b"by name");
        assert_eq!(received(&sink).as_deref(), Some(&b"by name"[..]));
        if std::fs::read_to_string("/etc/hosts").is_ok_and(|hosts| hosts.contains("xn--mnchen-3ya.fixture")) {
            port_forwarding.set_entries("forza", vec![format!("münchen.fixture:{port}")]);
            forwarder.forward(b"by its ACE name");
            assert_eq!(received(&sink).as_deref(), Some(&b"by its ACE name"[..]));
        }
        let failure = |entry: &str| {
            port_forwarding.set_entries("forza", vec![entry.to_owned(), format!("127.0.0.1:{port}")]);
            let mut printed = Vec::new();
            forwarder.forward_with(b"x", send_all, |line| printed.push(line.to_owned()));
            assert_eq!(received(&sink), None, "the failed target ends the loop");
            printed.pop().unwrap()
        };
        let prefix = "something went wrong while doing the udp port forward ";
        assert_eq!(failure("absent.fixture:20777"), format!("{prefix}[Errno -2] Name or service not known"));
        assert_eq!(failure("absent.fixture:70000"), format!("{prefix}[Errno -2] Name or service not known"));
        // A non-ASCII name reaches the resolver in its ACE form.
        assert_eq!(failure("bücher.fixture:20777"), format!("{prefix}[Errno -2] Name or service not known"));
        assert_eq!(failure("sink.fixture:70000"), format!("{prefix}sendto(): port must be 0-65535."));
        assert_eq!(failure("absent.fixture:4294967296"), format!("{prefix}sendto(): port must be 0-65535."));
    }

    #[test]
    fn any_listener_forwards_to_the_running_listeners_entries() {
        // The forwarder takes no listener argument: a background thread's datagram (e.g. the F1 2019-2025 receive
        // threads while a shared-memory listener runs) goes to the running listener's targets.
        let (sink, port) = receiver();
        let parameters = Arc::new(Parameters::default());
        let port_forwarding = Arc::new(PortForwarding::default());
        let background = Forwarder::with(parameters.clone(), port_forwarding.clone());
        port_forwarding.set(BTreeMap::from([("ac".to_string(), vec![format!("127.0.0.1:{port}")])]));
        parameters.set_listener_running("ac");
        background.forward(b"to ac's targets");
        assert_eq!(received(&sink).as_deref(), Some(&b"to ac's targets"[..]));
        parameters.set_listener_running("none");
        background.forward(b"listener_running 'none'");
        assert_eq!(received(&sink), None);
    }
}
