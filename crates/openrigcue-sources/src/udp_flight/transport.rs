//! UDP plumbing shared by the listeners that receive one datagram per liveness check.

use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::time::Duration;

use openrigcue_model::native_value::LegacyValue;
use openrigcue_model::Telemetry;

use super::channels::{clear_listener_channels_except, AngularRates, FlightSource};
use crate::udp_racing::forward::Forwarder;
use crate::udp_racing::net::{Endpoint, Network, SystemNetwork};
use crate::udp_racing::receiver::{ReceiveFailure, Receiver};
use crate::udp_racing::socket::{socket_creation_failed, ImportedPort};
use crate::udp_racing::unhandled::DispatcherEnds;
use crate::{Source, SourceError};

/// Where a listener receives, and where it re-sends what it receives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UdpConfig {
    /// Local port (the listener's `NETWORK_PORT` setting).
    pub port: u16,
}

impl UdpConfig {
    /// The listener's port.
    pub fn port(port: u16) -> Self {
        UdpConfig { port }
    }
}

/// One blocking receive on a freshly bound socket that is closed again afterwards.
///
/// The liveness test of the small synchronous listeners: create, bind, set the timeout,
/// receive up to `max_len` bytes, close; so the port is only bound while the check runs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DatagramProbe {
    pub(crate) bind: SocketAddrV4,
    pub(crate) timeout: Duration,
    pub(crate) max_len: usize,
}

/// Room for the largest UDP payload, so a datagram longer than `max_len` can be recognised.
const LARGEST_DATAGRAM: usize = 65_536;

impl DatagramProbe {
    /// The datagram, or `None` on a bind error or when nothing arrived within the timeout (both
    /// are "not running"). A datagram longer than `max_len` is a receive error too,
    /// not a truncation.
    pub(crate) fn receive(&self) -> Option<Vec<u8>> {
        let socket = UdpSocket::bind(self.bind)
            .map_err(|error| tracing::debug!(bind = %self.bind, %error, "bind failed"))
            .ok()?;
        socket.set_read_timeout(Some(self.timeout)).ok()?;
        let mut buf = vec![0; LARGEST_DATAGRAM.max(self.max_len + 1)];
        let (len, _) = socket.recv_from(&mut buf).ok()?;
        if len > self.max_len {
            tracing::debug!(bind = %self.bind, len, max = self.max_len, "datagram larger than the receive buffer");
            return None;
        }
        buf.truncate(len);
        Some(buf)
    }
}

/// The stage of the liveness test in which a socket operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeStage {
    Creation,
    Bind,
    Receive,
}

/// Game-specific half of a [`PolledUdpSource`]: socket parameters, datagram decoding and the channel view.
pub trait Decoder: Send {
    /// Listener id.
    const ID: &'static str;
    /// Default port (the `NETWORK_PORT` setting's default).
    const DEFAULT_PORT: u16;
    /// Local address the listener binds.
    const BIND_IP: Ipv4Addr = Ipv4Addr::LOCALHOST;
    /// Receive timeout of the liveness check.
    const TIMEOUT: Duration;
    /// Receive buffer size.
    const MAX_DATAGRAM: usize;
    /// Whether the liveness check itself hands the datagram to the UDP forwarder.
    const FORWARDS: bool = true;
    /// Whether a zero-length datagram still counts as running (otherwise an empty datagram
    /// reads as "not running").
    const EMPTY_DATAGRAM_IS_RUNNING: bool = false;
    /// Channels the dispatcher keeps at their previous value while the listener cannot answer them (the
    /// read fails and the registry default is `None`). They are not cleared before [`Decoder::write`],
    /// which writes them only when it has a value.
    const HELD_CHANNELS: &'static [&'static str] = &[];

    /// Takes in one datagram; `false` when it cannot be taken in. State not replaced by a
    /// failed decode keeps its previous value.
    fn decode(&mut self, datagram: &[u8]) -> bool;

    /// Writes the provided channels; every other listener channel except [`Decoder::HELD_CHANNELS`]
    /// has already been set to 0.
    fn write(&self, telemetry: &mut Telemetry);

    /// A typed channel answer; None keeps the numeric compatibility adapter.
    fn native_channel(&self, _name: &str) -> Option<LegacyValue> {
        None
    }

    /// A failed typed channel read, independent of any older compatibility frame value.
    fn channel_read_raised(&self, _name: &str) -> bool {
        false
    }

    /// Called when a liveness check receives nothing.
    fn on_silence(&mut self) {}

    /// Apply a failed liveness test and return its plain print message, when it has one.
    fn on_probe_failure(&mut self, _stage: ProbeStage, _error: &std::io::Error) -> Option<String> {
        self.on_silence();
        None
    }

    /// Whether the `speed` read fails for the stored datagram. The dispatcher's top-speed update then repeats
    /// that read without handling the failure and its loop ends.
    fn speed_raises(&self) -> bool {
        false
    }
}

/// A [`Decoder`] for a game that also provides rate channels.
pub trait RateDecoder: Decoder {
    /// The listener's `pitch_rate`, `roll_rate` and `yaw_rate` channels
    /// as of the last decoded datagram.
    fn rates(&self) -> AngularRates;
}

/// A listener that does one datagram receive per liveness check and decodes it on poll.
pub struct PolledUdpSource<D: Decoder> {
    probe: DatagramProbe,
    native_receiver: Option<Receiver>,
    /// The UDP forwarder.
    forwarder: Forwarder,
    datagram: Option<Vec<u8>>,
    decoder: D,
}

impl<D: Decoder> PolledUdpSource<D> {
    pub fn new(decoder: D, config: UdpConfig) -> Self {
        PolledUdpSource {
            probe: DatagramProbe {
                bind: SocketAddrV4::new(D::BIND_IP, config.port),
                timeout: D::TIMEOUT,
                max_len: D::MAX_DATAGRAM,
            },
            native_receiver: None,
            forwarder: Forwarder::default(),
            datagram: None,
            decoder,
        }
    }

    /// Keep the configured port as an integer until the socket is created and bind validates its range.
    pub fn from_imported_port(decoder: D, port: ImportedPort) -> Self {
        Self::with_imported_network(decoder, port, Box::new(SystemNetwork))
    }

    fn with_imported_network(decoder: D, port: ImportedPort, network: Box<dyn Network>) -> Self {
        let mut source = Self::new(decoder, UdpConfig::port(0));
        source.native_receiver = Some(Receiver::without_forwarding_imported(
            network,
            Endpoint::new(D::BIND_IP, 0, D::TIMEOUT),
            D::MAX_DATAGRAM,
            port,
        ));
        source
    }

    fn check_native_with(&mut self, print: impl FnOnce(&str)) -> Option<bool> {
        let receiver = self.native_receiver.as_mut()?;
        let datagram = match receiver.receive_and_keep() {
            Ok(datagram) => datagram,
            Err(failure) => {
                let (stage, error) = match &failure {
                    ReceiveFailure::Bind(error) if socket_creation_failed(error) => (ProbeStage::Creation, error),
                    ReceiveFailure::Bind(error) => (ProbeStage::Bind, error),
                    ReceiveFailure::Receive(error) => (ProbeStage::Receive, error),
                };
                if let Some(message) = self.decoder.on_probe_failure(stage, error) {
                    print(&message);
                }
                return Some(false);
            }
        };
        let running = D::EMPTY_DATAGRAM_IS_RUNNING || !datagram.is_empty();
        // The check stores the bytes before forwarding and explicitly closes afterwards.
        self.datagram = Some(datagram);
        if D::FORWARDS {
            self.forwarder.forward(self.datagram.as_ref().expect("stored datagram"));
        }
        receiver.close();
        Some(running)
    }

    /// The game-specific state, for values outside the [`Source`] contract.
    pub fn decoder(&self) -> &D {
        &self.decoder
    }

    /// Use `forwarder` as the UDP forwarder (tests give it their own parameters).
    #[cfg(test)]
    pub(crate) fn set_forwarder(&mut self, forwarder: Forwarder) {
        self.forwarder = forwarder;
    }
}

impl<D: Decoder> Source for PolledUdpSource<D> {
    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        self.decoder.native_channel(name)
    }

    fn channel_read_raised(&self, name: &str) -> bool {
        self.decoder.channel_read_raised(name)
    }

    fn id(&self) -> &'static str {
        D::ID
    }

    /// The liveness test: the datagram is forwarded before the dispatcher records the result
    /// (the running listener is set after the test returns, and reset to none when it fails).
    fn is_running(&mut self) -> Result<bool, SourceError> {
        if let Some(running) = self.check_native_with(|message| println!("{message}")) {
            return Ok(running);
        }
        let Some(datagram) = self.probe.receive() else {
            self.decoder.on_silence();
            return Ok(false);
        };
        if D::FORWARDS {
            self.forwarder.forward(&datagram);
        }
        let running = D::EMPTY_DATAGRAM_IS_RUNNING || !datagram.is_empty();
        self.datagram = Some(datagram);
        Ok(running)
    }

    fn receive_native(&mut self, _telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        Ok(match &self.datagram {
            Some(datagram) => self.decoder.decode(datagram),
            None => false,
        })
    }

    fn poll(&mut self, telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        let fresh = self.receive_native(telemetry)?;
        if self.decoder.speed_raises() {
            // The dispatcher's pass up to the top-speed update: the car name, the location and lateral_velocity are
            // stored, then the failure ends the pass; every other channel keeps its value.
            let before = telemetry.clone();
            publish(&self.decoder, telemetry);
            let (vehicle, location, lateral_velocity) = (
                std::mem::take(&mut telemetry.vehicle),
                std::mem::take(&mut telemetry.location),
                telemetry.lateral_velocity,
            );
            *telemetry = Telemetry { vehicle, location, lateral_velocity, ..before };
            return Err(DispatcherEnds::speed_read_raised().into());
        }
        publish(&self.decoder, telemetry);
        Ok(fresh)
    }
}

/// One dispatcher pass over the listener's channels: everything the dispatcher copies is reset (except the held
/// channels), then the decoder writes what it can answer. NaN is passed on as the listener answered it.
pub(crate) fn publish<D: Decoder>(decoder: &D, telemetry: &mut Telemetry) {
    clear_listener_channels_except(telemetry, D::HELD_CHANNELS);
    decoder.write(telemetry);
}

impl<D: RateDecoder> FlightSource for PolledUdpSource<D> {
    fn angular_rates(&self) -> AngularRates {
        self.decoder.rates()
    }
}

#[cfg(test)]
mod native_tests {
    use super::*;
    use crate::udp_flight::{aerofly::AeroflyDecoder, condor::CondorDecoder, silent_wings::SilentWingsDecoder};
    use crate::udp_racing::forward::PortForwarding;
    use crate::udp_racing::net::Datagrams;
    use crate::udp_racing::parameters::Parameters;
    use crate::udp_racing::socket::socket_creation_error;
    use std::collections::VecDeque;
    use std::io;
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    enum Step {
        Create,
        Bind,
        Timeout,
        Receive,
        Data(Vec<u8>),
    }

    struct Script {
        steps: VecDeque<Step>,
        calls: Vec<String>,
        endpoint: Endpoint,
        max_len: usize,
    }

    struct NetworkScript(Arc<Mutex<Script>>);
    struct SocketScript {
        script: Arc<Mutex<Script>>,
        step: Step,
    }

    impl Network for NetworkScript {
        fn bind(&mut self, _: &Endpoint) -> io::Result<Box<dyn Datagrams>> {
            panic!("native port must reach bind_imported")
        }

        fn bind_imported(&mut self, endpoint: &Endpoint, port: ImportedPort) -> io::Result<Box<dyn Datagrams>> {
            let mut script = self.0.lock().unwrap();
            assert_eq!(*endpoint, script.endpoint);
            script.calls.push("create".into());
            let step = script.steps.pop_front().expect("scripted check");
            if matches!(step, Step::Create) {
                return Err(socket_creation_error(io::Error::other("creation denied")));
            }
            script.calls.push("bind".into());
            if let Err(error) = port.for_bind() {
                script.calls.push("close".into());
                return Err(error);
            }
            assert_eq!(port, ImportedPort::Valid(6001));
            if matches!(step, Step::Bind) {
                script.calls.push("close".into());
                return Err(io::Error::other("bind denied"));
            }
            script.calls.push("timeout".into());
            if matches!(step, Step::Timeout) {
                script.calls.push("close".into());
                return Err(io::Error::other("settimeout denied"));
            }
            drop(script);
            Ok(Box::new(SocketScript { script: self.0.clone(), step }))
        }
    }

    impl Datagrams for SocketScript {
        fn receive(&mut self, max_len: usize) -> io::Result<(Vec<u8>, SocketAddr)> {
            let mut script = self.script.lock().unwrap();
            assert_eq!(max_len, script.max_len);
            script.calls.push("receive".into());
            match std::mem::replace(&mut self.step, Step::Receive) {
                Step::Data(data) => Ok((data, SocketAddr::from(([127, 0, 0, 1], 6001)))),
                Step::Receive => Err(io::Error::new(io::ErrorKind::TimedOut, "timed out")),
                _ => panic!("setup failure cannot receive"),
            }
        }

        fn send_to(&mut self, _: &[u8], _: SocketAddr) -> io::Result<()> {
            panic!("a listener probe does not send")
        }

        fn try_clone(&self) -> io::Result<Box<dyn Datagrams>> {
            panic!("a listener probe does not clone")
        }
    }

    impl Drop for SocketScript {
        fn drop(&mut self) {
            self.script.lock().unwrap().calls.push("close".into());
        }
    }

    fn make_source<D: Decoder>(
        decoder: D,
        port: ImportedPort,
        steps: Vec<Step>,
    ) -> (PolledUdpSource<D>, Arc<Mutex<Script>>) {
        let script = Arc::new(Mutex::new(Script {
            steps: steps.into(),
            calls: Vec::new(),
            endpoint: Endpoint::new(D::BIND_IP, 0, D::TIMEOUT),
            max_len: D::MAX_DATAGRAM,
        }));
        let mut source = PolledUdpSource::with_imported_network(decoder, port, Box::new(NetworkScript(script.clone())));
        let parameters = Arc::new(Parameters::default());
        parameters.set_listener_running(D::ID);
        let forwarding = Arc::new(PortForwarding::default());
        let calls = script.clone();
        forwarding.set_reader(Arc::new(move |key| {
            assert_eq!(key, D::ID);
            // The forwarder reads the section twice: for its membership test and for its loop.
            calls.lock().unwrap().calls.push("forward".into());
            Some(Vec::new())
        }));
        source.set_forwarder(Forwarder::with(parameters, forwarding));
        (source, script)
    }

    fn check<D: Decoder + Default>(
        old: Vec<u8>,
        new: Vec<u8>,
        channel: &str,
        old_value: f64,
        new_value: f64,
        timeout: Duration,
        max_len: usize,
    ) {
        let (mut source, script) = make_source(
            D::default(),
            ImportedPort::Valid(6001),
            vec![
                Step::Data(old.clone()),
                Step::Create,
                Step::Bind,
                Step::Timeout,
                Step::Receive,
                Step::Data(Vec::new()),
                Step::Data(new.clone()),
            ],
        );
        assert_eq!(script.lock().unwrap().endpoint, Endpoint::loopback(0, timeout));
        assert_eq!(script.lock().unwrap().max_len, max_len);
        assert!(source.is_running().unwrap());
        let _ = source.poll(&mut Telemetry::off()).unwrap();
        assert_eq!(source.native_channel(channel), Some(LegacyValue::Float(old_value)));
        assert_eq!(
            script.lock().unwrap().calls,
            ["create", "bind", "timeout", "receive", "forward", "forward", "close"]
        );
        for expected in [
            vec!["create"],
            vec!["create", "bind", "close"],
            vec!["create", "bind", "timeout", "close"],
            vec!["create", "bind", "timeout", "receive", "close"],
        ] {
            script.lock().unwrap().calls.clear();
            assert!(!source.is_running().unwrap());
            assert_eq!(source.datagram.as_deref(), Some(old.as_slice()));
            assert_eq!(source.native_channel(channel), Some(LegacyValue::Float(old_value)));
            assert_eq!(script.lock().unwrap().calls, expected);
        }
        assert!(!source.is_running().unwrap(), "empty bytes are returned but are false to the dispatcher");
        assert_eq!(source.datagram.as_deref(), Some([].as_slice()));
        assert_eq!(source.native_channel(channel), Some(LegacyValue::Float(old_value)), "checkStatus does not decode");
        script.lock().unwrap().calls.clear();
        assert!(source.is_running().unwrap());
        assert_eq!(source.datagram.as_deref(), Some(new.as_slice()));
        let _ = source.poll(&mut Telemetry::off()).unwrap();
        assert_eq!(source.native_channel(channel), Some(LegacyValue::Float(new_value)));
        assert_eq!(
            script.lock().unwrap().calls,
            ["create", "bind", "timeout", "receive", "forward", "forward", "close"]
        );
        assert!(script.lock().unwrap().steps.is_empty());

        let (mut source, script) =
            make_source::<D>(D::default(), ImportedPort::OutOfRange, vec![Step::Create, Step::Bind]);
        assert!(!source.is_running().unwrap());
        assert_eq!(script.lock().unwrap().calls, ["create"]);
        script.lock().unwrap().calls.clear();
        assert!(!source.is_running().unwrap());
        assert_eq!(script.lock().unwrap().calls, ["create", "bind", "close"]);
    }

    #[test]
    fn condor_native_probe_preserves_failure_prefix_and_recovers() {
        check::<CondorDecoder>(
            b"time=1\r\nyaw=0\r\nbank=0\r\npitch=0.5".to_vec(),
            b"time=2\r\nyaw=0\r\nbank=0\r\npitch=1".to_vec(),
            "pitch",
            super::super::python::rad_to_deg(0.5),
            super::super::python::rad_to_deg(1.0),
            crate::udp_racing::net::timeout_secs(1.0 / 15.0),
            1024,
        );
    }

    #[test]
    fn silent_wings_native_probe_preserves_failure_prefix_and_recovers() {
        let mut old = vec![0; super::super::silent_wings::PACKET_SIZE];
        old[36..40].copy_from_slice(&4.5f32.to_le_bytes());
        let mut new = old.clone();
        new[36..40].copy_from_slice(&7.5f32.to_le_bytes());
        check::<SilentWingsDecoder>(old, new, "roll", 4.5, 7.5, Duration::from_millis(125), 1024);
    }

    #[test]
    fn aerofly_native_probe_preserves_failure_prefix_and_recovers() {
        check::<AeroflyDecoder>(
            b"15;-30;1800;5;-7;12;63;2".to_vec(),
            b"25;-30;1800;5;-7;12;63;2".to_vec(),
            "pitch",
            1.5,
            2.5,
            Duration::from_secs(1),
            2000,
        );
    }

    fn printed_failures<D: Decoder + Default>(expected: &[&str], range_expected: &[&str]) {
        let (mut source, _) = make_source(
            D::default(),
            ImportedPort::Valid(6001),
            vec![Step::Create, Step::Bind, Step::Timeout, Step::Receive, Step::Data(b"saved".to_vec())],
        );
        source.datagram = Some(b"previous".to_vec());
        let mut prints = Vec::new();
        for _ in 0..4 {
            assert_eq!(source.check_native_with(|line| prints.push(line.to_owned())), Some(false));
            assert_eq!(source.datagram.as_deref(), Some(b"previous".as_slice()));
        }
        assert_eq!(prints, expected);
        assert_eq!(source.check_native_with(|_| panic!("successful receive is silent")), Some(true));
        assert_eq!(source.datagram.as_deref(), Some(b"saved".as_slice()));

        let (mut source, _) = make_source::<D>(D::default(), ImportedPort::OutOfRange, vec![Step::Create, Step::Bind]);
        let mut prints = Vec::new();
        assert_eq!(source.check_native_with(|line| prints.push(line.to_owned())), Some(false));
        assert_eq!(source.check_native_with(|line| prints.push(line.to_owned())), Some(false));
        assert_eq!(prints, range_expected);
    }

    #[test]
    fn aces_native_print_is_only_in_the_creation_catch() {
        printed_failures::<super::super::aces_high::AcesHighDecoder>(
            &["Exception1 creation denied"],
            &["Exception1 creation denied"],
        );
        check::<super::super::aces_high::AcesHighDecoder>(
            b"0,0,0,0,0,0,10,20,30,1,2,3,4,5,6,7,8,9".to_vec(),
            b"0,0,0,0,0,0,10,20,30,1,2,3,14,5,6,7,8,9".to_vec(),
            "roll",
            4.0,
            14.0,
            Duration::from_secs(1),
            320,
        );
    }

    #[test]
    fn flyinside_native_print_is_only_in_the_bind_timeout_catch() {
        printed_failures::<super::super::fly_inside::FlyInsideDecoder>(
            &["sw fail sock", "sw fail sock"],
            &["sw fail sock"],
        );
        check::<super::super::fly_inside::FlyInsideDecoder>(
            b"20,1,2,3,4,5,6".to_vec(),
            b"20,2,2,3,4,5,6".to_vec(),
            "roll",
            -1.0,
            -2.0,
            crate::udp_racing::net::timeout_secs(1.0 / 3.0),
            1024,
        );
    }

    fn capture_logs(operation: impl FnOnce()) -> Vec<String> {
        use tracing::{field::Visit, span, Event, Metadata, Subscriber};
        struct Capture(Arc<Mutex<Vec<String>>>);
        impl Subscriber for Capture {
            fn enabled(&self, _: &Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
                span::Id::from_u64(1)
            }
            fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
            fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
            fn enter(&self, _: &span::Id) {}
            fn exit(&self, _: &span::Id) {}
            fn event(&self, event: &Event<'_>) {
                struct Message(String);
                impl Visit for Message {
                    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                        if field.name() == "message" {
                            self.0 = format!("{value:?}");
                        }
                    }
                }
                let mut message = Message(String::new());
                event.record(&mut message);
                self.0.lock().unwrap().push(format!(
                    "{} {} {}",
                    event.metadata().target(),
                    event.metadata().level(),
                    message.0
                ));
            }
        }
        let logs = Arc::new(Mutex::new(Vec::new()));
        let registration = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
        tracing::subscriber::with_default(Capture(logs.clone()), operation);
        drop(registration);
        let values = std::mem::take(&mut *logs.lock().unwrap());
        values
    }

    #[test]
    fn dcs_native_failure_logs_keep_creation_and_bind_separate() {
        use super::super::dcs::DcsDecoder;
        let (mut source, _) = make_source(
            DcsDecoder::default(),
            ImportedPort::Valid(6001),
            vec![Step::Create, Step::Bind, Step::Timeout, Step::Receive, Step::Data(Vec::new())],
        );
        source.datagram = Some(b"old".to_vec());
        let logs = capture_logs(|| {
            for _ in 0..4 {
                assert!(!source.is_running().unwrap());
                assert_eq!(source.datagram.as_deref(), Some(b"old".as_slice()));
            }
            assert!(source.is_running().unwrap(), "DCS returns True for an empty successful datagram");
        });
        assert_eq!(
            logs,
            [
                "App.backend.listeners.dcsListener.checkStatus ERROR socket error creation denied",
                "App.backend.listeners.dcsListener.checkStatus ERROR socket bind bind denied",
                "App.backend.listeners.dcsListener.checkStatus ERROR socket bind settimeout denied",
            ]
        );
        assert_eq!(source.datagram.as_deref(), Some([].as_slice()));
        let (mut ranged, _) =
            make_source(DcsDecoder::default(), ImportedPort::OutOfRange, vec![Step::Create, Step::Bind]);
        let logs = capture_logs(|| {
            assert!(!ranged.is_running().unwrap());
            assert!(!ranged.is_running().unwrap());
        });
        assert_eq!(
            logs,
            [
                "App.backend.listeners.dcsListener.checkStatus ERROR socket error creation denied",
                "App.backend.listeners.dcsListener.checkStatus ERROR socket bind bind(): port must be 0-65535.",
            ]
        );
    }

    #[test]
    fn api_native_failure_logs_and_fan_override_recovery() {
        use crate::api::ApiDecoder;
        let mut lla = vec![0; 28];
        lla[..3].copy_from_slice(b"lla");
        lla[4..8].copy_from_slice(&101u32.to_le_bytes());
        lla[8..12].copy_from_slice(&60f32.to_le_bytes());
        let (mut source, script) = make_source(
            ApiDecoder::default(),
            ImportedPort::Valid(6001),
            vec![Step::Create, Step::Bind, Step::Timeout, Step::Receive, Step::Data(lla.clone())],
        );
        source.datagram = Some(lla.clone());
        assert!(source.decoder.decode(&lla));
        let logs = capture_logs(|| {
            for _ in 0..4 {
                assert!(source.decoder.decode(&lla));
                assert!(source.decoder.fan_override());
                assert!(!source.is_running().unwrap());
                assert!(!source.decoder.fan_override());
                assert_eq!(source.datagram.as_deref(), Some(lla.as_slice()));
                assert_eq!(source.native_channel("speed"), Some(LegacyValue::Str("60".into())));
            }
            assert!(source.is_running().unwrap());
            assert!(!source.decoder.fan_override(), "checkStatus does not parse the next lla packet");
            assert!(source.poll(&mut Telemetry::off()).unwrap());
            assert!(source.decoder.fan_override());
        });
        assert_eq!(
            logs,
            [
                "App.apiListener.checkStatus DEBUG Socket Error: creation denied",
                "App.apiListener.checkStatus DEBUG Bind Error: bind denied",
                "App.apiListener.checkStatus DEBUG Bind Error: settimeout denied",
            ]
        );
        assert!(!script.lock().unwrap().calls.iter().any(|call| call == "forward"));
        let (mut ranged, _) =
            make_source(ApiDecoder::default(), ImportedPort::OutOfRange, vec![Step::Create, Step::Bind]);
        let logs = capture_logs(|| {
            for _ in 0..2 {
                assert!(ranged.decoder.decode(&lla));
                assert!(!ranged.is_running().unwrap());
                assert!(!ranged.decoder.fan_override());
            }
        });
        assert_eq!(
            logs,
            [
                "App.apiListener.checkStatus DEBUG Socket Error: creation denied",
                "App.apiListener.checkStatus DEBUG Bind Error: bind(): port must be 0-65535.",
            ]
        );
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::thread;

    /// A port that was free a moment ago on 127.0.0.1.
    pub(crate) fn free_port() -> u16 {
        UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap().port()
    }

    /// Sends `datagram` to `port` every few milliseconds until the returned guard is dropped.
    pub(crate) struct Sender {
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl Sender {
        pub(crate) fn start(port: u16, datagram: Vec<u8>) -> Self {
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let flag = stop.clone();
            let handle = thread::spawn(move || {
                let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
                while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = socket.send_to(&datagram, (Ipv4Addr::LOCALHOST, port));
                    thread::sleep(Duration::from_millis(2));
                }
            });
            Sender { stop, handle: Some(handle) }
        }
    }

    impl Drop for Sender {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{free_port, Sender};
    use super::*;

    struct Echo {
        last: Vec<u8>,
    }

    impl Decoder for Echo {
        const ID: &'static str = "echo";
        const DEFAULT_PORT: u16 = 0;
        const TIMEOUT: Duration = Duration::from_millis(500);
        const MAX_DATAGRAM: usize = 4;

        fn decode(&mut self, datagram: &[u8]) -> bool {
            self.last = datagram.to_vec();
            true
        }

        fn write(&self, telemetry: &mut Telemetry) {
            telemetry.rpm = self.last.len() as f64;
        }
    }

    #[test]
    fn native_receive_defers_api_getters_while_retaining_the_real_decoder_state() {
        use crate::derive::{Listener, SourceListener};
        use crate::NativeRead;

        let mut source = PolledUdpSource::new(crate::api::ApiDecoder::default(), UdpConfig { port: 0 });
        source.datagram = Some(crate::api::packet::build::basic(b"api", 101, [f32::NAN, 1.0, 2.0, 3.0], 4));
        let before = Telemetry { speed: 23.0, rpm: 900.0, ..Telemetry::off() };
        let mut compatibility = before.clone();
        assert!(source.receive_native(&mut compatibility).unwrap());
        assert_eq!(compatibility, before);
        assert_eq!(source.read_native("speed"), NativeRead::Raised);
        let mut adapter = SourceListener::new(source);
        adapter.receive();
        assert_eq!(adapter.failure_point(), None);
        assert_eq!(adapter.read_native("speed"), NativeRead::Raised);
    }

    #[test]
    fn check_status_receives_and_rebinds_each_time() {
        let port = free_port();
        let mut source = PolledUdpSource::new(Echo { last: Vec::new() }, UdpConfig::port(port));
        let sender = Sender::start(port, b"abcd".to_vec());
        assert!(source.is_running().unwrap());
        drop(sender);
        let mut t = Telemetry::off();
        assert!(source.poll(&mut t).unwrap());
        assert_eq!(t.rpm, 4.0, "recvfrom(4) takes a four-byte datagram");
        assert_eq!(t.gear, 0.0, "unprovided listener channels read 0");
        // The port is free between checks: another socket can bind it.
        UdpSocket::bind((Ipv4Addr::LOCALHOST, port)).unwrap();
    }

    #[test]
    fn check_status_rejects_a_datagram_longer_than_the_receive_size() {
        // A 6-byte datagram in a 4-byte receive is an error: the liveness test answers false.
        let port = free_port();
        let mut source = PolledUdpSource::new(Echo { last: Vec::new() }, UdpConfig::port(port));
        let _sender = Sender::start(port, b"abcdef".to_vec());
        assert!(!source.is_running().unwrap());
    }

    #[test]
    fn forwarding_uses_the_running_listener() {
        // The liveness test forwards the datagram before the dispatcher makes the listener the running one.
        let sink = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        sink.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        let target = format!("127.0.0.1:{}", sink.local_addr().unwrap().port());
        let port = free_port();
        let mut source = PolledUdpSource::new(Echo { last: Vec::new() }, UdpConfig::port(port));
        let (forwarder, parameters) = crate::udp_racing::forward::test_forwarder("echo", &[target]);
        parameters.set_listener_running("none");
        source.set_forwarder(forwarder);
        let sender = Sender::start(port, b"ab".to_vec());
        assert!(source.is_running().unwrap());
        let mut buf = [0; 8];
        assert!(sink.recv(&mut buf).is_err(), "not forwarded while no listener runs");
        parameters.set_listener_running("echo");
        assert!(source.is_running().unwrap());
        drop(sender);
        assert_eq!(sink.recv(&mut buf).unwrap(), 2, "forwarded while this listener runs");
    }

    #[test]
    fn a_raising_speed_read_stores_names_and_lateral_velocity_and_ends_the_dispatcher() {
        // The dispatcher's top-speed update does not handle a failed speed read.
        let port = free_port();
        let mut source = PolledUdpSource::new(crate::api::ApiDecoder::default(), UdpConfig { port });
        let datagram = crate::api::packet::build::basic(b"api", 101, [f32::NAN, 1.0, 2.0, 3.0], 4);
        let sender = Sender::start(port, datagram);
        assert!(source.is_running().unwrap());
        drop(sender);
        let mut t =
            Telemetry { rpm: 3000.0, speed: 9.0, lateral_velocity: 4.0, location: "x".into(), ..Telemetry::off() };
        let error = source.poll(&mut t).unwrap_err();
        assert!(crate::udp_racing::unhandled::ends_dispatcher(&error));
        assert_eq!(
            t,
            Telemetry { rpm: 3000.0, speed: 9.0, lateral_velocity: 0.0, ..Telemetry::off() },
            "101 has no lateral_velocity (its handler stores 0), car_name and track_loc answer 0"
        );
    }

    #[test]
    fn check_status_is_false_on_timeout_and_on_bind_error() {
        let busy = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = busy.local_addr().unwrap().port();
        let mut source = PolledUdpSource::new(Echo { last: Vec::new() }, UdpConfig::port(port));
        assert!(!source.is_running().unwrap(), "bind error");
        drop(busy);
        assert!(!source.is_running().unwrap(), "timeout");
        let mut t = Telemetry::off();
        assert!(!source.poll(&mut t).unwrap(), "nothing received yet");
    }
}
