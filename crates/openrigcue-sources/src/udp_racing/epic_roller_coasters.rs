//! Epic Roller Coasters `<letter>[<number>]…` telemetry (listener `epic`).
//!
//! The datagram is rewritten into a dictionary literal with a chain of text replacements ([`rewrite`]) and
//! then parsed as that literal ([`literal_eval_dict`]).

use std::time::Duration;

use openrigcue_model::Telemetry;

pub use super::socket::ListenerSettings;
use super::socket::{socket_creation_failed, ImportedPort};
use super::support::dict_channels::{DictValue, TelemetryDict};
use super::support::net::{Endpoint, Network, SystemNetwork};
use super::support::py_literal::{literal_eval_dict, Literal, LiteralError};
use super::support::pymath::parse_float;
use super::support::receiver::{ReceiveFailure, Receiver};
use super::unhandled::DispatcherEnds;
use crate::{Source, SourceError};

/// (The configuration default, which tests bind; the listener itself has none: it reads `NETWORK_PORT` at construction.)
#[cfg(test)]
const DEFAULT_PORT: u16 = 7701;
const RECEIVE_SIZE: usize = 1024;
/// Receive timeout of 1/3 s: binary64 0x3fd5555555555555, rounded up to 333,333,334 ns.
const RECEIVE_TIMEOUT: Duration = super::support::net::socket_timeout(333_333_334);

/// The replacement pairs applied after the bracket rewrite, in order.
const RENAMES: [(&str, &str); 10] = [
    ("S", "speed"),
    ("Y", "yaw"),
    ("R", "roll"),
    ("P", "pitch"),
    ("H", "vertical_acceleration"),
    ("W", "lateral_acceleration"),
    ("U", "longitudinal_acceleration"),
    (": ", ": \""),
    (", \"", "\", \""),
    ("}", "\"}"),
];

/// Rewrites a datagram's text into a dictionary literal.
fn rewrite(text: &str) -> String {
    let braced = format!("{{\"{text}").replace(']', ", \"").replace('[', "\": ");
    let mut cut: Vec<char> = braced.chars().collect();
    cut.truncate(cut.len().saturating_sub(3));
    let mut s: String = cut.into_iter().collect();
    s.push('}');
    RENAMES.iter().fold(s, |acc, (from, to)| acc.replace(from, to))
}

/// The receive step: each value becomes a float where its text is a float
/// (the text stays otherwise), then roll and pitch are multiplied by -1 (text times -1 is the empty string). Its other
/// failures are not handled: an undecodable datagram, a rewrite that is not a dictionary literal, or a missing `R` or
/// `P` entry fail the receive step and end the dispatcher's loop ([`DispatcherEnds`]). A literal whose
/// value a channel cannot hold is reported as malformed data.
fn decode(datagram: &[u8]) -> Result<TelemetryDict, SourceError> {
    let uncaught = |what: &str| SourceError::from(DispatcherEnds::at_receive_data(format!("epic receiveData: {what}")));
    let text = std::str::from_utf8(datagram).map_err(|_| uncaught("UnicodeDecodeError"))?;
    let entries = literal_eval_dict(&rewrite(text)).map_err(|error| match error {
        LiteralError::Rejected => uncaught("ast.literal_eval raised"),
        LiteralError::Unsupported => {
            SourceError::Malformed("epic: a dict value this port cannot represent".to_string())
        }
    })?;
    let mut values = TelemetryDict::default();
    for (key, value) in entries {
        let v = match value {
            Literal::Str(text) => parse_float(&text).map_or(DictValue::Text(text), DictValue::Number),
            Literal::Number(n) => DictValue::Number(n),
            Literal::Native(value) => DictValue::Native(value),
        };
        values.set(&key, v);
    }
    for name in ["roll", "pitch"] {
        let slot = values.get_mut(name).ok_or_else(|| uncaught(&format!("KeyError: {name}")))?;
        *slot = match &*slot {
            DictValue::Integer(n) => DictValue::Integer(-n),
            DictValue::Number(n) => DictValue::Number(*n * -1.0),
            DictValue::Text(_) => DictValue::Text(String::new()),
            DictValue::Native(value) => {
                return Err(uncaught(&format!(
                    "TypeError: unsupported operand type(s) for *: '{}' and 'int'",
                    value.type_name()
                )))
            }
        };
    }
    Ok(values)
}

/// The Epic Roller Coasters listener.
pub struct EpicRollerCoastersSource {
    receiver: Receiver,
    native_setup: bool,
    datagram: Vec<u8>,
    /// The telemetry dictionary, replaced as a whole by every good datagram.
    values: TelemetryDict,
}

impl EpicRollerCoastersSource {
    pub fn new(settings: ListenerSettings) -> Self {
        Self::with_network(settings, Box::new(SystemNetwork))
    }

    pub fn from_imported_port(port: ImportedPort) -> Self {
        Self::with_imported_network(port, Box::new(SystemNetwork))
    }

    fn with_imported_network(port: ImportedPort, net: Box<dyn Network>) -> Self {
        Self {
            receiver: Receiver::with_imported_port(net, Endpoint::loopback(0, RECEIVE_TIMEOUT), RECEIVE_SIZE, port),
            native_setup: true,
            datagram: Vec::new(),
            values: TelemetryDict::default(),
        }
    }

    fn with_network(settings: ListenerSettings, net: Box<dyn Network>) -> Self {
        let endpoint = Endpoint::loopback(settings.port, RECEIVE_TIMEOUT);
        EpicRollerCoastersSource {
            receiver: Receiver::new(net, endpoint, RECEIVE_SIZE),
            native_setup: false,
            datagram: Vec::new(),
            values: TelemetryDict::default(),
        }
    }

    fn check_status_with_report(&mut self, mut report: impl FnMut(String)) -> bool {
        match self.receiver.receive_once() {
            Ok(data) => {
                self.datagram = data;
                !self.datagram.is_empty()
            }
            Err(ReceiveFailure::Bind(error)) => {
                if !socket_creation_failed(&error) {
                    report("sw fail sock".into());
                }
                false
            }
            Err(ReceiveFailure::Receive(_)) => false,
        }
    }
}

impl Source for EpicRollerCoastersSource {
    fn native_channel(&self, name: &str) -> Option<openrigcue_model::native_value::LegacyValue> {
        Some(self.values.native_channel(name))
    }

    fn id(&self) -> &'static str {
        "epic"
    }

    /// The liveness test: whether a datagram arrived.
    fn is_running(&mut self) -> Result<bool, SourceError> {
        if self.native_setup {
            Ok(self.check_status_with_report(|message| println!("{message}")))
        } else {
            match self.receiver.receive_once() {
                Ok(data) => {
                    self.datagram = data;
                    Ok(!self.datagram.is_empty())
                }
                Err(error) => {
                    tracing::debug!("sw fail sock {error}");
                    Ok(false)
                }
            }
        }
    }

    /// The receive step; getter conversions remain deferred to their actual callers.
    fn receive_native(&mut self, _telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        self.values = decode(&self.datagram)?;
        Ok(true)
    }

    fn poll(&mut self, telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        let fresh = self.receive_native(telemetry)?;
        self.values.publish(telemetry)?;
        Ok(fresh)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::udp_racing::support::net::fake::FakeNetwork;
    use crate::udp_racing::unhandled::ends_dispatcher;

    #[test]
    fn native_receive_keeps_raw_text_until_dispatch_and_parse_failure_keeps_prior_dict() {
        let mut source = EpicRollerCoastersSource::with_network(
            ListenerSettings::with_port(DEFAULT_PORT),
            Box::new(FakeNetwork::default()),
        );
        source.datagram = b"S[fast]R[3]P[4]".to_vec();
        let before = Telemetry { speed: 23.0, roll: 19.0, ..Telemetry::off() };
        let mut compatibility = before.clone();
        assert!(source.receive_native(&mut compatibility).unwrap());
        assert_eq!(compatibility, before);
        assert_eq!(source.native_channel("speed"), Some("fast".into()));
        assert_eq!(source.native_channel("roll"), Some((-3.0).into()));
        source.datagram = b"S[10]R[3]".to_vec();
        let error = source.receive_native(&mut compatibility).unwrap_err();
        assert_eq!(crate::udp_racing::unhandled::failure_point(&error), Some(crate::derive::FailurePoint::ReceiveData));
        assert_eq!(source.native_channel("speed"), Some("fast".into()));
        assert_eq!(compatibility, before);
    }

    #[test]
    fn native_setup_diagnostic_is_only_the_bare_bind_failure_literal() {
        use super::super::support::net::Datagrams;
        use std::io;
        struct Failure(bool);
        impl Network for Failure {
            fn bind(&mut self, _: &Endpoint) -> io::Result<Box<dyn Datagrams>> {
                if self.0 {
                    Err(super::super::socket::socket_creation_error(io::Error::other("create denied")))
                } else {
                    Err(io::Error::other("bind denied"))
                }
            }
        }
        for (creation, expected) in [(true, Vec::new()), (false, vec!["sw fail sock"])] {
            let mut source = EpicRollerCoastersSource::with_network(
                ListenerSettings::with_port(DEFAULT_PORT),
                Box::new(Failure(creation)),
            );
            let mut reports = Vec::new();
            assert!(!source.check_status_with_report(|message| reports.push(message)));
            assert_eq!(reports, expected);
        }
        let mut source = EpicRollerCoastersSource::with_network(
            ListenerSettings::with_port(DEFAULT_PORT),
            Box::new(FakeNetwork::default()),
        );
        assert!(!source.check_status_with_report(|_| panic!("receive errors are silent")));
    }

    #[test]
    fn native_cached_port_retry_retains_epic_values_and_bind_only_message() {
        use super::super::support::net::Datagrams;
        use std::io;
        use std::sync::{Arc, Mutex};
        struct Staged(Arc<Mutex<Vec<ImportedPort>>>, FakeNetwork);
        impl Network for Staged {
            fn bind(&mut self, _: &Endpoint) -> io::Result<Box<dyn Datagrams>> {
                panic!("imported port")
            }
            fn bind_imported(&mut self, endpoint: &Endpoint, port: ImportedPort) -> io::Result<Box<dyn Datagrams>> {
                assert_eq!(*endpoint.bind.ip(), std::net::Ipv4Addr::LOCALHOST);
                assert_eq!(endpoint.timeout, Some(RECEIVE_TIMEOUT));
                let step = {
                    let mut calls = self.0.lock().unwrap();
                    calls.push(port);
                    calls.len()
                };
                let port = port.for_bind()?;
                match step {
                    1 => Err(super::super::socket::socket_creation_error(io::Error::other("creation denied"))),
                    2 => Err(io::Error::other("bind denied")),
                    3 => Err(io::Error::other("timeout denied")),
                    _ => {
                        let mut endpoint = endpoint.clone();
                        endpoint.bind.set_port(port);
                        self.1.bind(&endpoint)
                    }
                }
            }
        }
        let old_data = b"S[1]R[2]P[3]U[4]";
        let good = FakeNetwork::default();
        good.push_timeout(6001);
        good.push(6001, b"S[5]R[6]P[7]U[8]");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut source = EpicRollerCoastersSource::with_imported_network(
            ImportedPort::Valid(6001),
            Box::new(Staged(calls.clone(), good)),
        );
        source.datagram = old_data.to_vec();
        source.values = decode(old_data).unwrap();
        let mut reports = Vec::new();
        for _ in 0..4 {
            assert!(!source.check_status_with_report(|message| reports.push(message)));
            assert_eq!(source.datagram, old_data);
            assert_eq!(source.native_channel("speed"), Some(openrigcue_model::native_value::LegacyValue::Float(1.0)));
        }
        assert_eq!(reports, ["sw fail sock", "sw fail sock"]);
        assert!(source.check_status_with_report(|_| panic!("successful receive")));
        assert!(source.poll(&mut Telemetry::off()).unwrap());
        assert_eq!(source.native_channel("speed"), Some(openrigcue_model::native_value::LegacyValue::Float(5.0)));
        assert_eq!(*calls.lock().unwrap(), [ImportedPort::Valid(6001); 5]);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut source = EpicRollerCoastersSource::with_imported_network(
            ImportedPort::OutOfRange,
            Box::new(Staged(calls.clone(), FakeNetwork::default())),
        );
        let mut reports = Vec::new();
        assert!(!source.check_status_with_report(|message| reports.push(message)));
        assert_eq!(reports, ["sw fail sock"]);
        assert_eq!(*calls.lock().unwrap(), [ImportedPort::OutOfRange]);
    }

    #[test]
    fn native_none_reaches_each_original_dispatcher_failure_stage() {
        use crate::derive::*;
        struct Games;
        impl GameDirectory for Games {
            fn is_flying(&self, _: &str) -> bool {
                false
            }
            fn intellitune_reference(&self, _: &str) -> Option<IntellituneReference> {
                None
            }
        }
        for (key, history, ready) in [("yaw", 1, true), ("speed", 0, false), ("roll", 0, false), ("pitch", 0, false)] {
            let wire = format!("S[1]R[2]P[3]X[x\",\"{key}\":None,\"dummy\":\"]");
            let source = source_with(&[wire.as_bytes()]);
            let mut dispatcher = Dispatcher::new(vec![Box::new(SourceListener::new(source))]);
            dispatcher.tick(&TickInput {
                hits: &[ScanHit { game_id: "epic".into(), listener_id: "epic".into() }],
                settings: &DispatchSettings {
                    dams: DamsSettings {
                        mode: DamsMode::Off,
                        factor: Some(1.0),
                        front_lever: 0.0,
                        rear_lever: 0.0,
                        steering_ratio: Some(1.0),
                    },
                    refresh_rates: RefreshRates { shakebox: 10, intellibox: 60, motionbox: 30 },
                    native_refresh_rates: None,
                    speed_unit: SpeedUnit::Kmh,
                    native_speed_unit: None,
                    show_all_channels: false,
                },
                profile: &ProfileSettings {
                    default_max_speed_kmh: 200.0,
                    gear_shift_percent: 96.0,
                    pitch_source: AttitudeSource::Angle,
                    roll_source: AttitudeSource::Angle,
                    native: None,
                },
                runtime: &RuntimeInputs { plot_channel: Some("speed".into()), ..Default::default() },
                games: &Games,
            });
            assert!(dispatcher.has_ended(), "{key}");
            assert_eq!(dispatcher.history().len(), history, "{key}");
            assert_eq!(dispatcher.telemetry_ready(), ready, "{key}");
            assert!(dispatcher.export_rows().is_empty(), "{key}");
        }
    }

    #[test]
    fn rewrite_builds_a_dict_literal() {
        // The replacement chain
        assert_eq!(rewrite("S[12.3]Y[1.0]R[-2.0]"), "{\"speed\": \"12.3\", \"yaw\": \"1.0\", \"roll\": \"-2.0\"}");
        assert_eq!(literal_eval_dict("{\"a\": \"1\", }"), Ok(vec![("a".into(), Literal::Str("1".into()))]));
    }

    #[test]
    fn channels_and_sign_flips() {
        // Every value becomes a float; roll and pitch are multiplied by -1
        let net = FakeNetwork::default();
        let mut s =
            EpicRollerCoastersSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        net.push(DEFAULT_PORT, b"S[12.3]Y[1.0]R[-2.0]P[0.5]H[0.1]W[0.2]U[0.3]");
        assert!(s.is_running().unwrap());
        assert_eq!(net.bound()[0].bind, "127.0.0.1:7701".parse().unwrap());
        let mut t = Telemetry::off();
        assert!(s.poll(&mut t).unwrap());
        assert_eq!((t.speed, t.yaw, t.roll, t.pitch), (12.3, 1.0, 2.0, -0.5));
        assert_eq!((t.vertical_acceleration, t.lateral_acceleration, t.longitudinal_acceleration), (0.1, 0.2, 0.3));
    }

    fn source_with(datagrams: &[&[u8]]) -> EpicRollerCoastersSource {
        let net = FakeNetwork::default();
        for d in datagrams {
            net.push(DEFAULT_PORT, d);
        }
        EpicRollerCoastersSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net))
    }

    #[test]
    fn a_rejected_float_keeps_the_text_for_that_entry_only() {
        // A value that is not float text stays as it is; the loop goes on
        let mut s = source_with(&[b"S[12]Y[1.2.3]R[2]P[x]U[0.5]"]);
        assert!(s.is_running().unwrap());
        let mut t = Telemetry { yaw: 9.0, pitch: 4.0, ..Telemetry::off() };
        assert!(s.poll(&mut t).unwrap());
        assert_eq!((t.speed, t.roll, t.longitudinal_acceleration), (12.0, -2.0, 0.5));
        assert_eq!(s.values.get("yaw"), Some(&DictValue::Text("1.2.3".into())));
        // Text times -1 is the empty text
        assert_eq!(s.values.get("pitch"), Some(&DictValue::Text(String::new())));
        assert_eq!(s.native_channel("pitch"), Some(openrigcue_model::native_value::LegacyValue::Str(String::new())));
        assert_eq!((t.yaw, t.pitch), (9.0, 4.0), "a str yaw/pitch is stored as is by the original; kept here");
    }

    #[test]
    fn a_text_speed_is_unreadable_for_the_dispatcher() {
        // A text speed that is not an integer fails in the dispatcher's top-speed update
        let mut s = source_with(&[b"S[1]R[1]P[1]U[2]", b"S[fast]R[1]P[1]U[3]"]);
        let mut t = Telemetry::off();
        s.is_running().unwrap();
        s.poll(&mut t).unwrap();
        s.is_running().unwrap();
        assert!(s.poll(&mut t).is_err_and(|e| ends_dispatcher(&e)));
        assert_eq!((t.speed, t.longitudinal_acceleration), (1.0, 2.0));
    }

    #[test]
    fn missing_roll_raises_out_of_receive_data() {
        // A missing roll entry is an unhandled failure of the receive step
        let mut s = source_with(&[b"S[1]R[1]P[1]", b"S[5]P[1]"]);
        let mut t = Telemetry::off();
        s.is_running().unwrap();
        assert!(s.poll(&mut t).unwrap());
        s.is_running().unwrap();
        assert!(s.poll(&mut t).is_err_and(|e| ends_dispatcher(&e)));
        assert_eq!(t.speed, 1.0);
    }

    #[test]
    fn literal_eval_and_decode_failures_raise() {
        // Undecodable bytes and text that is not a dictionary literal are unhandled failures of the receive step
        for bad in [&b"\xff\xfe"[..], b"S[1]R[1]P[1\"]"] {
            let mut s = source_with(&[bad]);
            s.is_running().unwrap();
            let mut t = Telemetry::off();
            assert!(s.poll(&mut t).is_err_and(|e| ends_dispatcher(&e)), "{bad:?}");
        }
    }
}
