//! Live for Speed OutGauge + OutSim on one port, both without the trailing ID (listener `lfs`).

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::socket::ImportedPort;
pub use super::socket::ListenerSettings;
use super::support::channels::{publish_speed_unreadable, zero_listener_channels};
use super::support::net::{timeout_secs, Endpoint, Network, SystemNetwork};
use super::support::pymath::{int_text, rad_to_deg, trunc};
use super::support::receiver::Receiver;
use super::support::wire::{Fields, Short};
use super::unhandled::DispatcherEnds;
use crate::{Source, SourceError};

/// (The configuration default, which tests bind; the listener itself has none: it reads `NETWORK_PORT` at construction.)
#[cfg(test)]
const DEFAULT_PORT: u16 = 30000;
const RECEIVE_SIZE: usize = 256;
const OUTGAUGE_LEN: usize = 92;
const OUTSIM_LEN: usize = 64;
const MAX_RPM: f64 = 30000.0;

/// The OutGauge packet fields in use.
#[derive(Debug, Clone, PartialEq)]
struct OutGauge {
    car: [u8; 3],
    gear: u8,
    speed: f64,
    rpm: f64,
    turbo: f64,
    eng_temp: f64,
    fuel: f64,
    oil_pressure: f64,
    oil_temp: f64,
}

impl OutGauge {
    fn parse(data: &[u8]) -> Result<Self, Short> {
        let f = Fields::new(data);
        let mut car = [0u8; 3];
        car.copy_from_slice(f.bytes(4, 3)?);
        Ok(OutGauge {
            car,
            gear: f.u8(10)?,
            speed: f.f32(12)?,
            rpm: f.f32(16)?,
            turbo: f.f32(20)?,
            eng_temp: f.f32(24)?,
            fuel: f.f32(28)?,
            oil_pressure: f.f32(32)?,
            oil_temp: f.f32(36)?,
        })
    }
}

/// The OutSim packet (one unsigned int, twelve floats, three ints) fields in use.
#[derive(Debug, Clone, Copy, PartialEq)]
struct OutSim {
    heading: f64,
    pitch: f64,
    roll: f64,
    accel: [f64; 3],
    vel: [f64; 3],
}

impl OutSim {
    fn parse(data: &[u8]) -> Result<Self, Short> {
        let f = Fields::new(data);
        Ok(OutSim {
            heading: f.f32(16)?,
            pitch: f.f32(20)?,
            roll: f.f32(24)?,
            accel: [f.f32(28)?, f.f32(32)?, f.f32(36)?],
            vel: [f.f32(40)?, f.f32(44)?, f.f32(48)?],
        })
    }
}

/// World to local: the world vector rotated into the car frame by pitch, roll and
/// heading; returns `(x, y, z)` = lateral, longitudinal, vertical.
fn world_to_local(v: [f64; 3], pitch: f64, roll: f64, yaw: f64) -> [f64; 3] {
    let (sp, cp) = (pitch.sin(), pitch.cos());
    let (sr, cr) = (roll.sin(), roll.cos());
    let (sy, cy) = (yaw.sin(), yaw.cos());
    let x = cp * cy * v[0] + (cr * sy + sr * sp * cy) * v[1] + (sr * sy - cr * sp * cy) * v[2];
    let y = -cp * sy * v[0] + (cr * cy - sr * sp * sy) * v[1] + (sr * cy + cr * sp * sy) * v[2];
    let z = sp * v[0] - sr * cp * v[1] + cr * cp * v[2];
    [x, y, z]
}

/// The Live for Speed listener.
pub struct LiveForSpeedSource {
    receiver: Receiver,
    gauge: Option<OutGauge>,
    sim: Option<OutSim>,
}

impl LiveForSpeedSource {
    pub fn new(settings: ListenerSettings) -> Self {
        Self::with_network(settings, Box::new(SystemNetwork))
    }

    pub fn from_imported_port(port: ImportedPort) -> Self {
        Self::with_imported_network(port, Box::new(SystemNetwork))
    }

    fn with_imported_network(port: ImportedPort, net: Box<dyn Network>) -> Self {
        Self {
            receiver: Receiver::with_imported_port(
                net,
                Endpoint::loopback(0, timeout_secs(1.0 / 7.0)),
                RECEIVE_SIZE,
                port,
            ),
            gauge: None,
            sim: None,
        }
    }

    fn with_network(settings: ListenerSettings, net: Box<dyn Network>) -> Self {
        let endpoint = Endpoint::loopback(settings.port, timeout_secs(1.0 / 7.0));
        LiveForSpeedSource { receiver: Receiver::new(net, endpoint, RECEIVE_SIZE), gauge: None, sim: None }
    }

    /// The receive step: an OutGauge (92 bytes) then an OutSim (64 bytes) datagram.
    fn receive(&mut self) -> bool {
        let Ok(first) = self.receiver.receive_more() else { return false };
        if first.len() == OUTGAUGE_LEN {
            match OutGauge::parse(&first) {
                Ok(g) => self.gauge = Some(g),
                Err(_) => return false,
            }
        }
        let Ok(second) = self.receiver.receive_more() else { return false };
        if second.len() != OUTSIM_LEN {
            return false;
        }
        self.sim = OutSim::parse(&second).ok();
        true
    }

    /// Writes the channels: every channel reads 0 until both an OutGauge and an OutSim packet were
    /// decoded. Where the
    /// rpm has no integer value (NaN or infinite) the dispatcher keeps the previous rpm; where the speed has none the
    /// tick ends after `lateral_velocity` ([`publish_speed_unreadable`]).
    fn write_channels(&self, t: &mut Telemetry) -> Result<(), DispatcherEnds> {
        let (Some(g), Some(s)) = (&self.gauge, self.sim) else {
            zero_listener_channels(t);
            return Ok(());
        };
        // Both transforms precede every channel, including car_name and
        // unmapped names. The sine or cosine of an infinity fails before any of those reads finish.
        if Self::rotation_raises(s) {
            return publish_speed_unreadable(t, None, 0.0);
        }
        let local_accel = world_to_local(s.accel, s.pitch, s.roll, s.heading);
        let local_vel = world_to_local(s.vel, s.pitch, s.roll, s.heading);
        let vehicle = String::from_utf8(g.car.to_vec()).ok();
        let Some(speed) = trunc(g.speed) else {
            return publish_speed_unreadable(t, vehicle, local_vel[0]);
        };
        let previous = (t.rpm, t.vehicle.clone());
        zero_listener_channels(t);
        t.vehicle = vehicle.unwrap_or(previous.1);
        t.rpm = trunc(g.rpm).unwrap_or(previous.0);
        t.gear = f64::from(g.gear) - 1.0;
        t.speed = speed;
        t.max_rpm = MAX_RPM;
        t.lateral_acceleration = local_accel[0];
        t.steering_wheel_angle = local_accel[0];
        t.vertical_acceleration = local_accel[2];
        t.longitudinal_acceleration = local_accel[1];
        t.vertical_velocity = local_vel[2];
        t.lateral_velocity = local_vel[0];
        t.longitudinal_velocity = local_vel[1];
        t.roll = rad_to_deg(s.roll);
        t.pitch = rad_to_deg(s.pitch);
        t.yaw = rad_to_deg(s.heading);
        t.oil_temperature = g.oil_temp;
        t.oil_pressure = g.oil_pressure;
        t.water_temperature = g.eng_temp;
        t.turbo_boost_pressure = g.turbo;
        t.fuel_level = g.fuel * 100.0;
        Ok(())
    }

    fn rotation_raises(sim: OutSim) -> bool {
        [sim.pitch, sim.roll, sim.heading].iter().any(|v| v.is_infinite())
    }
}

impl Source for LiveForSpeedSource {
    fn id(&self) -> &'static str {
        "lfs"
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        let (Some(g), Some(s)) = (&self.gauge, self.sim) else { return Some(LegacyValue::Int(0)) };
        if Self::rotation_raises(s) {
            return None;
        }
        let acceleration = || world_to_local(s.accel, s.pitch, s.roll, s.heading);
        let velocity = || world_to_local(s.vel, s.pitch, s.roll, s.heading);
        let value = match name {
            "rpm" => return int_text(g.rpm).map(LegacyValue::Str),
            "max_rpm" => return Some(LegacyValue::Str("30000".into())),
            "gear" => return Some(LegacyValue::Str((i64::from(g.gear) - 1).to_string())),
            "speed" => return int_text(g.speed).map(LegacyValue::Str),
            "car_name" => return String::from_utf8(g.car.to_vec()).ok().map(LegacyValue::Str),
            "lateral_acceleration" | "steering_wheel_angle" => acceleration()[0],
            "vertical_acceleration" => acceleration()[2],
            "longitudinal_acceleration" => acceleration()[1],
            "vertical_velocity" => velocity()[2],
            "lateral_velocity" => velocity()[0],
            "longitudinal_velocity" => velocity()[1],
            "roll" => rad_to_deg(s.roll),
            "pitch" => rad_to_deg(s.pitch),
            "yaw" => rad_to_deg(s.heading),
            "oil_temperature" => g.oil_temp,
            "oil_pressure" => g.oil_pressure,
            "water_temperature" => g.eng_temp,
            "turbo_boost_pressure" => g.turbo,
            "fuel_level" => g.fuel * 100.0,
            _ => return Some(LegacyValue::Int(0)),
        };
        Some(LegacyValue::Float(value))
    }

    fn channel_read_raised(&self, name: &str) -> bool {
        self.native_channel(name).is_none()
    }

    /// The liveness test: one datagram from a fresh socket kept open for the receive step.
    fn is_running(&mut self) -> Result<bool, SourceError> {
        let running = self.receiver.receive_and_keep().is_ok_and(|d| !d.is_empty());
        Ok(running)
    }

    fn receive_native(&mut self, _telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        Ok(self.receive())
    }

    fn poll(&mut self, telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        let fresh = self.receive_native(telemetry)?;
        self.write_channels(telemetry)?;
        Ok(fresh)
    }

    fn stop(&mut self) {
        self.receiver.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::udp_racing::support::net::fake::FakeNetwork;
    use crate::udp_racing::unhandled::assert_ends;

    fn outgauge() -> Vec<u8> {
        let mut d = vec![0u8; OUTGAUGE_LEN];
        d[4..7].copy_from_slice(b"XRT");
        d[10] = 4;
        let mut put = |o: usize, v: f32| d[o..o + 4].copy_from_slice(&v.to_le_bytes());
        put(12, 33.3);
        put(16, 6200.5);
        put(20, 1.25);
        put(24, 90.0);
        put(28, 0.4);
        put(32, 3.5);
        put(36, 101.0);
        d
    }

    fn outsim() -> Vec<u8> {
        let mut d = vec![0u8; OUTSIM_LEN];
        let mut put = |o: usize, v: f32| d[o..o + 4].copy_from_slice(&v.to_le_bytes());
        put(28, 1.0);
        put(32, 2.0);
        put(36, 3.0);
        put(40, 4.0);
        put(44, 5.0);
        put(48, 6.0);
        d
    }

    #[test]
    fn native_receive_does_not_run_failing_world_to_local_getters() {
        let net = FakeNetwork::default();
        let mut source =
            LiveForSpeedSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        let mut sim = outsim();
        sim[16..20].copy_from_slice(&f32::INFINITY.to_le_bytes());
        net.push(DEFAULT_PORT, &[1]);
        net.push(DEFAULT_PORT, &outgauge());
        net.push(DEFAULT_PORT, &sim);
        assert!(source.is_running().unwrap());
        let before = Telemetry { rpm: 123.0, roll: 19.0, ..Telemetry::off() };
        let mut compatibility = before.clone();
        assert!(source.receive_native(&mut compatibility).unwrap());
        assert_eq!(compatibility, before);
        assert_eq!(source.read_native("rpm"), crate::NativeRead::Raised);
        assert_eq!(
            source.read_native("unknown"),
            crate::NativeRead::Raised,
            "the transform precedes even unknown getters"
        );
    }

    #[test]
    fn native_cached_port_failures_preserve_packets_and_recover_on_the_kept_socket() {
        use super::super::support::net::Datagrams;
        use std::io;
        use std::sync::{Arc, Mutex};
        struct Staged(Arc<Mutex<Vec<ImportedPort>>>, FakeNetwork);
        impl Network for Staged {
            fn bind(&mut self, _: &Endpoint) -> io::Result<Box<dyn Datagrams>> {
                panic!("native port must reach bind_imported")
            }
            fn bind_imported(&mut self, endpoint: &Endpoint, port: ImportedPort) -> io::Result<Box<dyn Datagrams>> {
                assert_eq!(*endpoint.bind.ip(), std::net::Ipv4Addr::LOCALHOST);
                assert_eq!(endpoint.timeout, Some(timeout_secs(1.0 / 7.0)));
                let step = {
                    let mut calls = self.0.lock().unwrap();
                    calls.push(port);
                    calls.len()
                };
                if step == 1 {
                    return Err(super::super::socket::socket_creation_error(io::Error::other("creation denied")));
                }
                let port = port.for_bind()?;
                match step {
                    2 => Err(io::Error::other("bind denied")),
                    3 => Err(io::Error::other("settimeout denied")),
                    _ => {
                        let mut endpoint = endpoint.clone();
                        endpoint.bind.set_port(port);
                        self.1.bind(&endpoint)
                    }
                }
            }
        }
        let net = FakeNetwork::default();
        net.push_timeout(6001);
        net.push(6001, b"probe");
        net.push(6001, &outgauge());
        net.push(6001, &outsim());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut source =
            LiveForSpeedSource::with_imported_network(ImportedPort::Valid(6001), Box::new(Staged(calls.clone(), net)));
        let previous_gauge = OutGauge::parse(&outgauge()).unwrap();
        let previous_sim = OutSim::parse(&outsim()).unwrap();
        source.gauge = Some(previous_gauge.clone());
        source.sim = Some(previous_sim);
        for _ in 0..4 {
            assert!(!source.is_running().unwrap());
            assert_eq!(source.gauge, Some(previous_gauge.clone()));
            assert_eq!(source.sim, Some(previous_sim));
        }
        assert!(source.is_running().unwrap());
        assert!(source.poll(&mut Telemetry::off()).unwrap());
        assert_eq!(source.native_channel("speed"), Some(LegacyValue::Str("33".into())));
        assert_eq!(*calls.lock().unwrap(), [ImportedPort::Valid(6001); 5], "receiveData uses the retained socket");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut source = LiveForSpeedSource::with_imported_network(
            ImportedPort::OutOfRange,
            Box::new(Staged(calls.clone(), FakeNetwork::default())),
        );
        assert!(!source.is_running().unwrap());
        assert!(!source.is_running().unwrap());
        assert_eq!(*calls.lock().unwrap(), [ImportedPort::OutOfRange; 2]);
    }

    #[test]
    fn native_second_receive_failure_keeps_the_completed_gauge_prefix() {
        let net = FakeNetwork::default();
        let mut source = LiveForSpeedSource::with_imported_network(ImportedPort::Valid(6001), Box::new(net.clone()));
        let old_sim = OutSim::parse(&outsim()).unwrap();
        source.sim = Some(old_sim);
        let mut changed_gauge = outgauge();
        changed_gauge[16..20].copy_from_slice(&7200.0f32.to_le_bytes());
        net.push(6001, b"probe");
        net.push(6001, &changed_gauge);
        net.push_failure(6001);
        assert!(source.is_running().unwrap());
        assert!(!source.poll(&mut Telemetry::off()).unwrap());
        assert_eq!(source.native_channel("rpm"), Some(LegacyValue::Str("7200".into())));
        assert_eq!(source.sim, Some(old_sim));
        let bound = net.bound().len();
        assert!(!source.poll(&mut Telemetry::off()).unwrap(), "receive error dropped the kept socket");
        assert_eq!(net.bound().len(), bound, "receiveData must not open another socket");
        net.push(6001, b"probe");
        net.push(6001, &outgauge());
        net.push(6001, &outsim());
        assert!(source.is_running().unwrap());
        assert!(source.poll(&mut Telemetry::off()).unwrap());
        assert_eq!(source.native_channel("rpm"), Some(LegacyValue::Str("6200".into())));
    }

    #[test]
    fn world_to_local_rotation() {
        // World to local: identity for zero angles, heading turns x/y
        assert_eq!(world_to_local([1.0, 2.0, 3.0], 0.0, 0.0, 0.0), [1.0, 2.0, 3.0]);
        let r = world_to_local([1.0, 0.0, 0.0], 0.0, 0.0, std::f64::consts::FRAC_PI_2);
        assert!(r[0].abs() < 1e-12 && (r[1] + 1.0).abs() < 1e-12);
    }

    #[test]
    fn gauge_and_sim_channels() {
        // Receive step and channels
        let net = FakeNetwork::default();
        let mut s = LiveForSpeedSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        net.push(DEFAULT_PORT, &[9]);
        net.push(DEFAULT_PORT, &outgauge());
        net.push(DEFAULT_PORT, &outsim());
        assert!(s.is_running().unwrap());
        let mut t = Telemetry::off();
        assert!(s.poll(&mut t).unwrap());
        assert_eq!((t.rpm, t.max_rpm, t.gear, t.speed), (6200.0, 30000.0, 3.0, 33.0));
        assert_eq!(t.vehicle, "XRT");
        assert_eq!(
            (t.lateral_acceleration, t.steering_wheel_angle, t.longitudinal_acceleration, t.vertical_acceleration),
            (1.0, 1.0, 2.0, 3.0)
        );
        assert_eq!((t.lateral_velocity, t.longitudinal_velocity, t.vertical_velocity), (4.0, 5.0, 6.0));
        assert_eq!(
            (t.oil_temperature, t.oil_pressure, t.water_temperature, t.turbo_boost_pressure),
            (101.0, 3.5, 90.0, 1.25)
        );
        assert_eq!(t.fuel_level, f64::from(0.4f32) * 100.0);
        for (name, expected) in [
            ("rpm", LegacyValue::Str("6200".into())),
            ("max_rpm", LegacyValue::Str("30000".into())),
            ("gear", LegacyValue::Str("3".into())),
            ("speed", LegacyValue::Str("33".into())),
            ("car_name", LegacyValue::Str("XRT".into())),
            ("lateral_velocity", LegacyValue::Float(4.0)),
            ("oil_temperature", LegacyValue::Float(101.0)),
            ("yaw_rate", LegacyValue::Int(0)),
        ] {
            assert_eq!(s.native_channel(name), Some(expected), "{name}");
        }
    }

    fn source_after(datagrams: &[&[u8]]) -> LiveForSpeedSource {
        let net = FakeNetwork::default();
        let mut s = LiveForSpeedSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        net.push(DEFAULT_PORT, &[9]);
        for d in datagrams {
            net.push(DEFAULT_PORT, d);
        }
        assert!(s.is_running().unwrap());
        s
    }

    #[test]
    fn with_only_an_outsim_packet_everything_reads_zero() {
        // Channels read 0 unless both an OutGauge and an OutSim packet are held
        let mut sim = outsim();
        sim[16..20].copy_from_slice(&f32::INFINITY.to_le_bytes());
        let mut s = source_after(&[&[0u8; 20], &sim]);
        let mut t = Telemetry {
            rpm: 3000.0,
            gear: 2.0,
            speed: 15.0,
            vehicle: "FBM".into(),
            oil_temperature: 90.0,
            ..Telemetry::off()
        };
        assert!(s.poll(&mut t).unwrap());
        let mut zero = Telemetry::off();
        zero_listener_channels(&mut zero);
        assert_eq!(t, zero);
        assert_eq!(s.native_channel("rpm"), Some(LegacyValue::Int(0)));
        assert_eq!(s.native_channel("pitch"), Some(LegacyValue::Int(0)));
        assert!(!s.channel_read_raised("rpm"), "both packets are required before computing the transforms");
    }

    #[test]
    fn non_finite_speed_ends_the_tick_after_lateral_velocity() {
        // A NaN speed has no integer value; the dispatcher's top-speed update does not
        // handle that, so only car_name, track_loc (0) and lateral_velocity are stored
        let mut gauge = outgauge();
        gauge[12..16].copy_from_slice(&f32::NAN.to_le_bytes());
        let mut s = source_after(&[&gauge, &outsim()]);
        let before = Telemetry { rpm: 3000.0, speed: 15.0, pitch: 2.0, location: "x".into(), ..Telemetry::off() };
        let mut t = before.clone();
        assert_ends(s.poll(&mut t));
        assert_eq!(t, Telemetry { vehicle: "XRT".into(), location: String::new(), lateral_velocity: 4.0, ..before });
        assert_eq!(s.native_channel("speed"), None);
        assert!(s.channel_read_raised("speed"));
    }

    #[test]
    fn non_finite_rpm_keeps_the_previous_rpm() {
        // An infinite rpm has no integer value; the dispatcher keeps rpm (default None)
        let mut gauge = outgauge();
        gauge[16..20].copy_from_slice(&f32::INFINITY.to_le_bytes());
        let mut s = source_after(&[&gauge, &outsim()]);
        let mut t = Telemetry { rpm: 3000.0, ..Telemetry::off() };
        assert!(s.poll(&mut t).unwrap());
        assert_eq!((t.rpm, t.speed, t.oil_temperature), (3000.0, 33.0, 101.0));
        assert_eq!(s.native_channel("rpm"), None);
        assert!(s.channel_read_raised("rpm"));
        assert_eq!(s.native_channel("max_rpm"), Some(LegacyValue::Str("30000".into())));
    }

    #[test]
    fn infinite_rotation_raises_before_every_channel_branch() {
        for at in [16, 20, 24] {
            let mut sim = outsim();
            sim[at..at + 4].copy_from_slice(&f32::INFINITY.to_le_bytes());
            let mut source = source_after(&[&outgauge(), &sim]);
            let before = Telemetry {
                rpm: 3000.0,
                speed: 15.0,
                pitch: 2.0,
                vehicle: "FBM".into(),
                location: "x".into(),
                lateral_velocity: 7.0,
                ..Telemetry::off()
            };
            let mut telemetry = before.clone();
            assert_ends(source.poll(&mut telemetry));
            assert_eq!(telemetry, Telemetry { location: String::new(), lateral_velocity: 0.0, ..before });
            for name in ["car_name", "speed", "rpm", "max_rpm", "unknown"] {
                assert_eq!(source.native_channel(name), None, "{name}");
                assert!(source.channel_read_raised(name), "{name}");
            }
        }
    }

    #[test]
    fn nan_rotation_is_allowed_and_scalar_getters_still_answer() {
        let mut sim = outsim();
        sim[20..24].copy_from_slice(&f32::NAN.to_le_bytes());
        let mut source = source_after(&[&outgauge(), &sim]);
        let mut telemetry = Telemetry::off();
        assert!(source.poll(&mut telemetry).unwrap());
        assert!(matches!(source.native_channel("pitch"), Some(LegacyValue::Float(value)) if value.is_nan()));
        assert!(matches!(source.native_channel("lateral_velocity"), Some(LegacyValue::Float(value)) if value.is_nan()));
        assert_eq!(source.native_channel("rpm"), Some(LegacyValue::Str("6200".into())));
        assert_eq!(source.native_channel("unknown"), Some(LegacyValue::Int(0)));
        assert!(!source.channel_read_raised("rpm"));
    }

    #[test]
    fn without_outsim_everything_reads_zero() {
        // No OutSim packet -> 0
        let net = FakeNetwork::default();
        let mut s = LiveForSpeedSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        net.push(DEFAULT_PORT, &[9]);
        net.push(DEFAULT_PORT, &outgauge());
        net.push(DEFAULT_PORT, &[0u8; 68]);
        assert!(s.is_running().unwrap());
        let mut t = Telemetry::off();
        assert!(!s.poll(&mut t).unwrap());
        assert_eq!(t.max_rpm, 0.0);
    }
}
