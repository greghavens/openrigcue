//! Grand Theft Auto V with the GTAVSimFeedback mod (listener `gtav`): the mod sends Assetto Corsa's
//! `SPageFilePhysics` layout.

use std::time::Duration;

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::socket::ImportedPort;
pub use super::socket::ListenerSettings;
use super::support::channels::zero_listener_channels;
use super::support::net::{Endpoint, Network, SystemNetwork};
use super::support::pymath::round_to;
use super::support::receiver::Receiver;
use super::support::wire::{Fields, Short};
use crate::{Source, SourceError};

/// `NETWORK_PORT.gtav` as seeded by configuration migration 98.
/// (The configuration default, which tests bind; the listener itself has none: it reads `NETWORK_PORT` at construction.)
#[cfg(test)]
const DEFAULT_PORT: u16 = 20777;
const RECEIVE_SIZE: usize = 2000;
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(1);
/// The channels answered from the packet; every other name is 0 without touching it.
const MAPPED_CHANNELS: [&str; 25] = [
    "rpm",
    "gear",
    "speed",
    "steering_wheel_angle",
    "lateral_acceleration",
    "vertical_acceleration",
    "yaw",
    "roll",
    "pitch",
    "lateral_velocity",
    "longitudinal_velocity",
    "longitudinal_acceleration",
    "vertical_velocity",
    "wheel_slip_lf",
    "wheel_slip_lr",
    "wheel_slip_rf",
    "wheel_slip_rr",
    "suspension_travel_lf",
    "suspension_travel_lr",
    "suspension_travel_rf",
    "suspension_travel_rr",
    "suspension_velocity_lf",
    "suspension_velocity_rf",
    "suspension_velocity_lr",
    "suspension_velocity_rr",
];

/// Size of the packet structure (4-byte packing).
const PACKET_LEN: usize = 580;
const KMH_TO_MS: f64 = 0.277778;
/// Vector indices (`longitudinal = 0`, `lateral = 1`, `vertical = 2`).
const LONGITUDINAL: usize = 0;
const LATERAL: usize = 1;
const VERTICAL: usize = 2;
/// Wheel indices (`lf = 0`, `rf = 1`, `lr = 2`, `rr = 3`).
const RF: usize = 1;
const LR: usize = 2;
const RR: usize = 3;
const LF: usize = 0;

/// The packet fields the listener uses.
#[derive(Debug, Clone, Copy, PartialEq)]
struct GtavPacket {
    gear: i32,
    rpm: f64,
    steer_angle: f64,
    speed_kmh: f64,
    acc_g: [f64; 3],
    wheel_slip: [f64; 4],
    wheel_angular_speed: [f64; 4],
    suspension_travel: [f64; 4],
    yaw: f64,
    pitch: f64,
    roll: f64,
    local_velocity: [f64; 3],
}

impl GtavPacket {
    /// Decodes a datagram of at least the packet size.
    fn parse(data: &[u8]) -> Result<Self, Short> {
        let f = Fields::new(data);
        f.bytes(0, PACKET_LEN)?;
        let four =
            |o: usize| -> Result<[f64; 4], Short> { Ok([f.f32(o)?, f.f32(o + 4)?, f.f32(o + 8)?, f.f32(o + 12)?]) };
        Ok(GtavPacket {
            gear: f.i32(16)?,
            rpm: f.f32(20)?,
            steer_angle: f.f32(24)?,
            speed_kmh: f.f32(28)?,
            acc_g: [f.f32(44)?, f.f32(48)?, f.f32(52)?],
            wheel_slip: four(56)?,
            wheel_angular_speed: four(104)?,
            suspension_travel: four(184)?,
            yaw: f.f32(208)?,
            pitch: f.f32(212)?,
            roll: f.f32(216)?,
            local_velocity: [f.f32(568)?, f.f32(572)?, f.f32(576)?],
        })
    }
}

/// The Grand Theft Auto V listener.
pub struct GtavSource {
    receiver: Receiver,
    datagram: Vec<u8>,
    packet: Option<GtavPacket>,
}

impl GtavSource {
    pub fn new(settings: ListenerSettings) -> Self {
        Self::with_network(settings, Box::new(SystemNetwork))
    }

    pub fn from_imported_port(port: ImportedPort) -> Self {
        Self::with_imported_network(port, Box::new(SystemNetwork))
    }

    fn with_imported_network(port: ImportedPort, net: Box<dyn Network>) -> Self {
        Self {
            receiver: Receiver::with_imported_port(net, Endpoint::loopback(0, RECEIVE_TIMEOUT), RECEIVE_SIZE, port),
            datagram: Vec::new(),
            packet: None,
        }
    }

    fn with_network(settings: ListenerSettings, net: Box<dyn Network>) -> Self {
        let endpoint = Endpoint::loopback(settings.port, RECEIVE_TIMEOUT);
        GtavSource { receiver: Receiver::new(net, endpoint, RECEIVE_SIZE), datagram: Vec::new(), packet: None }
    }

    /// Writes the channels (`lf` reads index `lr` for wheel
    /// slip and suspension travel; longitudinal acceleration reads the local velocity).
    fn write_channels(&self, t: &mut Telemetry) {
        zero_listener_channels(t);
        let Some(p) = self.packet else { return };
        let r6 = |x: f64| round_to(x, 6);
        t.speed = p.speed_kmh * KMH_TO_MS;
        let rpm = p.rpm * 10000.0;
        t.rpm = if rpm < 2000.0 { 0.0 } else { r6(rpm) };
        t.gear = f64::from(p.gear);
        t.steering_wheel_angle = p.steer_angle;
        t.lateral_acceleration = r6(p.acc_g[LATERAL]) * 10.0;
        t.vertical_acceleration = r6(p.acc_g[VERTICAL]) * 10.0;
        t.yaw = p.yaw;
        t.roll = p.roll;
        t.pitch = p.pitch;
        t.lateral_velocity = r6(p.local_velocity[LATERAL]);
        t.longitudinal_velocity = r6(p.local_velocity[LONGITUDINAL]);
        t.longitudinal_acceleration = r6(p.local_velocity[LONGITUDINAL]);
        t.vertical_velocity = r6(p.local_velocity[VERTICAL]);
        t.wheel_slip_lf = r6(p.wheel_slip[LR]);
        t.wheel_slip_rf = r6(p.wheel_slip[RF]);
        t.wheel_slip_lr = r6(p.wheel_slip[LR]);
        t.wheel_slip_rr = r6(p.wheel_slip[RR]);
        t.suspension_travel_lf = r6(p.suspension_travel[LR]);
        t.suspension_travel_rf = r6(p.suspension_travel[RF]);
        t.suspension_travel_lr = r6(p.suspension_travel[LR]);
        t.suspension_travel_rr = r6(p.suspension_travel[RR]);
        t.suspension_velocity_lf = r6(p.wheel_angular_speed[LF]);
        t.suspension_velocity_rf = r6(p.wheel_angular_speed[RF]);
        t.suspension_velocity_lr = r6(p.wheel_angular_speed[LR]);
        t.suspension_velocity_rr = r6(p.wheel_angular_speed[RR]);
    }
}

impl Source for GtavSource {
    fn id(&self) -> &'static str {
        "gtav"
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        use LegacyValue::{Bool, Float, Int};
        // A failed mapped lookup answers false; an unmapped name answers integer zero.
        // Before the first decoded datagram there is no packet: the lookup fails and
        // the failure is printed after "Issue".
        if self.packet.is_none() && MAPPED_CHANNELS.contains(&name) {
            crate::printed::line("Issue name 'packet' is not defined");
        }
        let float = |value: fn(&GtavPacket) -> f64| Some(self.packet.as_ref().map_or(Bool(false), |p| Float(value(p))));
        match name {
            "rpm" => Some(self.packet.as_ref().map_or(Bool(false), |p| {
                let rpm = p.rpm * 10000.0;
                if rpm < 2000.0 {
                    Int(0)
                } else {
                    Float(round_to(rpm, 6))
                }
            })),
            "gear" => Some(self.packet.as_ref().map_or(Bool(false), |p| Int(i64::from(p.gear)))),
            "speed" => float(|p| p.speed_kmh * KMH_TO_MS),
            "steering_wheel_angle" => float(|p| p.steer_angle),
            "lateral_acceleration" => float(|p| round_to(p.acc_g[LATERAL], 6) * 10.0),
            "vertical_acceleration" => float(|p| round_to(p.acc_g[VERTICAL], 6) * 10.0),
            "yaw" => float(|p| p.yaw),
            "roll" => float(|p| p.roll),
            "pitch" => float(|p| p.pitch),
            "lateral_velocity" => float(|p| round_to(p.local_velocity[LATERAL], 6)),
            "longitudinal_velocity" | "longitudinal_acceleration" => {
                float(|p| round_to(p.local_velocity[LONGITUDINAL], 6))
            }
            "vertical_velocity" => float(|p| round_to(p.local_velocity[VERTICAL], 6)),
            "wheel_slip_lf" | "wheel_slip_lr" => float(|p| round_to(p.wheel_slip[LR], 6)),
            "wheel_slip_rf" => float(|p| round_to(p.wheel_slip[RF], 6)),
            "wheel_slip_rr" => float(|p| round_to(p.wheel_slip[RR], 6)),
            "suspension_travel_lf" | "suspension_travel_lr" => float(|p| round_to(p.suspension_travel[LR], 6)),
            "suspension_travel_rf" => float(|p| round_to(p.suspension_travel[RF], 6)),
            "suspension_travel_rr" => float(|p| round_to(p.suspension_travel[RR], 6)),
            "suspension_velocity_lf" => float(|p| round_to(p.wheel_angular_speed[LF], 6)),
            "suspension_velocity_rf" => float(|p| round_to(p.wheel_angular_speed[RF], 6)),
            "suspension_velocity_lr" => float(|p| round_to(p.wheel_angular_speed[LR], 6)),
            "suspension_velocity_rr" => float(|p| round_to(p.wheel_angular_speed[RR], 6)),
            _ => Some(Int(0)),
        }
    }

    /// The liveness test: any datagram.
    fn is_running(&mut self) -> Result<bool, SourceError> {
        let running = match self.receiver.receive_once() {
            Ok(data) => {
                self.datagram = data;
                true
            }
            Err(_) => false,
        };
        Ok(running)
    }

    /// The receive step: decode the datagram; a failure is printed after "error".
    fn receive_native(&mut self, _telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        let parsed = GtavPacket::parse(&self.datagram).ok();
        if parsed.is_some() {
            self.packet = parsed;
        } else {
            // The error text for a buffer shorter than the structure.
            crate::printed::line(format!(
                "error Buffer size too small ({} instead of at least {PACKET_LEN} bytes)",
                self.datagram.len()
            ));
        }
        Ok(parsed.is_some())
    }

    fn poll(&mut self, telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        let fresh = self.receive_native(telemetry)?;
        self.write_channels(telemetry);
        Ok(fresh)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::udp_racing::support::net::fake::FakeNetwork;

    fn datagram(rpm: f32) -> Vec<u8> {
        let mut d = vec![0u8; PACKET_LEN];
        let mut put = |o: usize, v: f32| d[o..o + 4].copy_from_slice(&v.to_le_bytes());
        put(20, rpm);
        put(24, -12.5);
        put(28, 90.0);
        put(44, 0.1);
        put(48, 0.2);
        put(52, 0.3);
        for (i, v) in [0.01f32, 0.02, 0.03, 0.04].iter().enumerate() {
            put(56 + 4 * i, *v);
            put(104 + 4 * i, *v * 100.0);
            put(184 + 4 * i, *v * 10.0);
        }
        put(208, 1.0);
        put(212, 2.0);
        put(216, 3.0);
        put(568, 25.0);
        put(572, -1.5);
        put(576, 0.5);
        d[16..20].copy_from_slice(&3i32.to_le_bytes());
        d
    }

    #[test]
    fn native_port_setup_failures_retain_packet_until_successful_retry() {
        use super::super::support::net::Datagrams;
        use std::io;
        use std::sync::{Arc, Mutex};
        struct Staged(Arc<Mutex<Vec<ImportedPort>>>, FakeNetwork);
        impl Network for Staged {
            fn bind(&mut self, _: &Endpoint) -> io::Result<Box<dyn Datagrams>> {
                panic!("native cached port must reach bind_imported")
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
                    1 => Err(super::super::socket::socket_creation_error(io::Error::other("create failed"))),
                    2 => Err(io::Error::other("bind failed")),
                    3 => Err(io::Error::other("timeout failed")),
                    _ => {
                        let mut endpoint = endpoint.clone();
                        endpoint.bind.set_port(port);
                        self.1.bind(&endpoint)
                    }
                }
            }
        }
        let old_data = datagram(0.3);
        let old_packet = GtavPacket::parse(&old_data).unwrap();
        let good = FakeNetwork::default();
        good.push_timeout(6001);
        good.push(6001, &datagram(0.4));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut source =
            GtavSource::with_imported_network(ImportedPort::Valid(6001), Box::new(Staged(calls.clone(), good)));
        source.datagram = old_data.clone();
        source.packet = Some(old_packet);
        for _ in 0..4 {
            assert!(!source.is_running().unwrap());
            assert_eq!(source.datagram, old_data);
            assert_eq!(source.packet, Some(old_packet));
        }
        assert!(source.is_running().unwrap());
        assert!(source.poll(&mut Telemetry::off()).unwrap());
        assert_eq!(source.packet, Some(GtavPacket::parse(&datagram(0.4)).unwrap()));
        assert_eq!(*calls.lock().unwrap(), [ImportedPort::Valid(6001); 5]);

        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut source = GtavSource::with_imported_network(
            ImportedPort::OutOfRange,
            Box::new(Staged(calls.clone(), FakeNetwork::default())),
        );
        source.packet = Some(old_packet);
        assert!(!source.is_running().unwrap());
        assert_eq!(*calls.lock().unwrap(), [ImportedPort::OutOfRange]);
        assert_eq!(source.packet, Some(old_packet));
    }

    fn polled(rpm: f32) -> (Telemetry, GtavSource) {
        let net = FakeNetwork::default();
        let mut s = GtavSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        net.push(DEFAULT_PORT, &datagram(rpm));
        assert!(s.is_running().unwrap());
        let mut t = Telemetry::off();
        assert!(s.poll(&mut t).unwrap());
        (t, s)
    }

    #[test]
    fn channels_as_compiled() {
        // Channel values
        let (mut t, mut source) = polled(0.5);
        assert_eq!((t.rpm, t.gear, t.steering_wheel_angle), (5000.0, 3.0, -12.5));
        assert_eq!(t.speed, 90.0 * 0.277778);
        assert_eq!((t.lateral_acceleration, t.vertical_acceleration), (2.0, 3.0));
        assert_eq!((t.yaw, t.pitch, t.roll), (1.0, 2.0, 3.0));
        assert_eq!(
            (t.longitudinal_velocity, t.longitudinal_acceleration, t.lateral_velocity, t.vertical_velocity),
            (25.0, 25.0, -1.5, 0.5)
        );
        assert_eq!((t.wheel_slip_lf, t.wheel_slip_rf, t.wheel_slip_lr, t.wheel_slip_rr), (0.03, 0.02, 0.03, 0.04));
        assert_eq!((t.suspension_travel_lf, t.suspension_travel_rr), (0.3, 0.4));
        assert_eq!(t.suspension_velocity_lf, 1.0);
        assert_eq!(source.native_channel("rpm"), Some(LegacyValue::Float(5000.0)));
        assert_eq!(source.native_channel("gear"), Some(LegacyValue::Int(3)));
        assert_eq!(source.native_channel("wheel_slip_lf"), Some(LegacyValue::Float(0.03)));
        assert_eq!(source.native_channel("max_rpm"), Some(LegacyValue::Int(0)));
        source.datagram.truncate(PACKET_LEN - 1);
        assert!(!source.poll(&mut t).unwrap());
        assert_eq!(source.native_channel("rpm"), Some(LegacyValue::Float(5000.0)));
    }

    #[test]
    fn idle_rpm_reads_zero() {
        // rpm: Rpm * 10000 < 2000 -> 0
        let (idle, idle_source) = polled(0.1999);
        assert_eq!(idle.rpm, 0.0);
        assert_eq!(idle_source.native_channel("rpm"), Some(LegacyValue::Int(0)));
        let (running, running_source) = polled(0.2);
        assert_eq!(running.rpm, round_to(f64::from(0.2f32) * 10000.0, 6));
        assert_eq!(running_source.native_channel("rpm"), Some(LegacyValue::Float(running.rpm)));
        let (_, nan_source) = polled(f32::NAN);
        assert!(matches!(nan_source.native_channel("rpm"), Some(LegacyValue::Float(value)) if value.is_nan()));
    }

    #[test]
    fn short_datagram_is_rejected() {
        // Receive step: decoding needs 580 bytes
        let net = FakeNetwork::default();
        let mut s = GtavSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        net.push(DEFAULT_PORT, &[0u8; 579]);
        assert!(s.is_running().unwrap());
        let mut t = Telemetry::off();
        crate::printed::take();
        assert!(!s.poll(&mut t).unwrap());
        // The failure is printed after "error" with the short-buffer text.
        assert_eq!(crate::printed::take(), ["error Buffer size too small (579 instead of at least 580 bytes)"]);
        assert_eq!(s.native_channel("gear"), Some(LegacyValue::Bool(false)));
        assert_eq!(s.native_channel("speed"), Some(LegacyValue::Bool(false)));
        assert_eq!(s.native_channel("max_rpm"), Some(LegacyValue::Int(0)));
        // No packet yet, so each mapped read prints its failure; other names read 0 silently.
        let issue = "Issue name 'packet' is not defined";
        assert_eq!(crate::printed::take(), [issue, issue]);
        for name in MAPPED_CHANNELS {
            assert_eq!(s.native_channel(name), Some(LegacyValue::Bool(false)), "{name}");
        }
        assert_eq!(crate::printed::take().len(), MAPPED_CHANNELS.len());
    }
}
