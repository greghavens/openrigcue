//! BeamNG.drive OutGauge + MotionSim streams on one port (listener `beamng`).

use std::collections::BTreeSet;

use openrigcue_model::native_value::LegacyValue;
use openrigcue_model::Telemetry;

use super::parameters::GameErrors;
pub use super::socket::ListenerSettings;
use super::socket::{socket_creation_failed, ImportedPort};
use super::support::channels::{publish_speed_unreadable, zero_listener_channels};
use super::support::net::{timeout_secs, Endpoint, Network, SystemNetwork};
use super::support::pymath::{int_text, rad_to_deg, trunc};
use super::support::receiver::{ReceiveFailure, Receiver};
use super::support::wire::{Fields, Short};
use super::unhandled::DispatcherEnds;
use crate::{Source, SourceError};

/// (The configuration default, which tests bind; the listener itself has none: it reads `NETWORK_PORT` at construction.)
#[cfg(test)]
const DEFAULT_PORT: u16 = 4444;
const RECEIVE_SIZE: usize = 256;
const OUTGAUGE_LEN: usize = 96;
const OUTSIM_LEN: usize = 88;
const MAX_RPM: f64 = 30000.0;

/// The OutGauge packet fields in use.
#[derive(Debug, Clone, PartialEq)]
struct OutGauge {
    car: [u8; 4],
    gear: u8,
    speed: f64,
    rpm: f64,
    fuel: f64,
    oil_temp: f64,
}

impl OutGauge {
    fn parse(data: &[u8]) -> Result<Self, Short> {
        let f = Fields::new(data);
        let mut car = [0u8; 4];
        car.copy_from_slice(f.bytes(4, 4)?);
        Ok(OutGauge {
            car,
            gear: f.u8(10)?,
            speed: f.f32(12)?,
            rpm: f.f32(16)?,
            fuel: f.f32(28)?,
            oil_temp: f.f32(36)?,
        })
    }
}

/// The OutSim packet (the MotionSim stream: a 4-byte tag and 21 floats) fields in use.
#[derive(Debug, Clone, Copy, PartialEq)]
struct OutSim {
    vel: [f64; 3],
    accel: [f64; 3],
    roll: f64,
    pitch: f64,
    yaw: f64,
    yaw_rate: f64,
}

impl OutSim {
    fn parse(data: &[u8]) -> Result<Self, Short> {
        let f = Fields::new(data);
        let at = |i: usize| f.f32(4 + 4 * i);
        Ok(OutSim {
            vel: [at(3)?, at(4)?, at(5)?],
            accel: [at(6)?, at(7)?, at(8)?],
            roll: at(12)?,
            pitch: at(13)?,
            yaw: at(14)?,
            yaw_rate: at(17)?,
        })
    }
}

/// Normalises a vector: `(x − min) / (max − min)` per element; `None` for a
/// division by zero (all elements equal).
fn normalize(values: [f64; 3]) -> Option<[f64; 3]> {
    let (mut max, mut min) = (values[0], values[0]);
    for value in &values[1..] {
        if *value > max {
            max = *value;
        }
        if *value < min {
            min = *value;
        }
    }
    let span = max - min;
    (span != 0.0).then(|| values.map(|x| (x - min) / span))
}

/// The dot product.
fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a.iter().zip(b).fold(0.0, |sum, (x, y)| sum + x * y)
}

/// The slip angle. `None` where the computation fails (division by zero in
/// the normalisation, an arccosine outside its domain). The slip angle stays 0 unless the speed is above 5 km/h,
/// so a NaN speed gives 0.
fn slip_angle(speed_kph: f64, yaw: f64, pitch: f64, vel_x: f64, vel_y: f64, yaw_rate: f64) -> Option<f64> {
    // Sine and cosine of an infinity are errors, before the speed branch; NaN remains accepted.
    if yaw.is_infinite() || pitch.is_infinite() {
        return None;
    }
    let forward = normalize([yaw.sin() * pitch.cos(), yaw.cos() * pitch.cos(), 0.0])?;
    if speed_kph > 5.0 {
        let velocity = normalize([vel_x, vel_y, 0.0])?;
        let d = dot(forward, velocity);
        let arg = 1.0 - if d > 0.0 { d } else { 0.0 };
        (arg.abs() <= 1.0).then(|| arg.acos() * yaw_rate)
    } else {
        Some(0.0)
    }
}

/// The BeamNG listener.
pub struct BeamNgSource {
    receiver: Receiver,
    gauge: Option<OutGauge>,
    sim: Option<OutSim>,
    game_errors: GameErrors,
}

impl BeamNgSource {
    pub fn new(settings: ListenerSettings) -> Self {
        Self::with_network(settings, Box::new(SystemNetwork))
    }

    fn with_network(settings: ListenerSettings, net: Box<dyn Network>) -> Self {
        let endpoint = Endpoint::loopback(settings.port, timeout_secs(1.0 / 7.0));
        let receiver = Receiver::new(net, endpoint, RECEIVE_SIZE);
        BeamNgSource { receiver, gauge: None, sim: None, game_errors: GameErrors::default() }
    }

    /// Keep the configured port as an integer until socket creation and bind validation.
    pub fn from_imported_port(port: ImportedPort) -> Self {
        Self::with_imported_network(port, Box::new(SystemNetwork))
    }

    fn with_imported_network(port: ImportedPort, net: Box<dyn Network>) -> Self {
        let endpoint = Endpoint::loopback(0, timeout_secs(1.0 / 7.0));
        let receiver = Receiver::with_imported_port(net, endpoint, RECEIVE_SIZE, port);
        Self { receiver, gauge: None, sim: None, game_errors: GameErrors::default() }
    }

    /// The error texts this listener contributes to the game errors.
    pub fn game_errors(&self) -> &BTreeSet<String> {
        self.game_errors.messages()
    }

    /// The receive step: one datagram per stream, matched by length. The OutGauge datagram is stored
    /// before the second receive, so it survives a failure of that receive. Neither datagram is forwarded
    /// (only the liveness test forwards). Good data clears the game errors.
    fn receive(&mut self) -> bool {
        let Ok(first) = self.receiver.receive_more_unforwarded() else { return false };
        if first.len() == OUTGAUGE_LEN {
            if let Ok(gauge) = OutGauge::parse(&first) {
                self.gauge = Some(gauge);
            }
        }
        let Ok(second) = self.receiver.receive_more_unforwarded() else { return false };
        if second.len() == OUTSIM_LEN {
            if let Ok(sim) = OutSim::parse(&second) {
                self.sim = Some(sim);
            }
        }
        self.game_errors.clear();
        true
    }

    /// Writes the channels; `steering_wheel_angle` repeats the lateral acceleration and
    /// `vertical_velocity` reads `velY`. Where the rpm has no integer value (NaN or infinite) the
    /// dispatcher keeps the previous rpm; where the slip angle fails it sets `lateral_velocity` to 0; where
    /// the speed has no integer value the tick ends after `lateral_velocity` ([`publish_speed_unreadable`]).
    fn write_channels(&self, t: &mut Telemetry) -> Result<(), DispatcherEnds> {
        let (Some(g), Some(s)) = (&self.gauge, &self.sim) else {
            zero_listener_channels(t);
            return Ok(());
        };
        let vehicle = String::from_utf8(g.car.to_vec()).ok();
        let lateral_velocity = slip_angle(g.speed * 3.6, s.yaw, s.pitch, s.vel[0], s.vel[1], s.yaw_rate).unwrap_or(0.0);
        let Some(speed) = trunc(g.speed) else {
            return publish_speed_unreadable(t, vehicle, lateral_velocity);
        };
        let previous = (t.rpm, t.vehicle.clone());
        zero_listener_channels(t);
        t.vehicle = vehicle.unwrap_or(previous.1);
        t.rpm = trunc(g.rpm).unwrap_or(previous.0);
        t.max_rpm = MAX_RPM;
        t.gear = f64::from(g.gear) - 1.0;
        t.speed = speed;
        t.lateral_acceleration = -s.accel[0];
        t.steering_wheel_angle = -s.accel[0];
        t.vertical_acceleration = s.accel[2];
        t.longitudinal_acceleration = s.accel[1];
        t.vertical_velocity = s.vel[1];
        t.lateral_velocity = lateral_velocity;
        t.roll = rad_to_deg(s.roll);
        t.pitch = rad_to_deg(s.pitch);
        t.yaw = rad_to_deg(s.yaw);
        t.oil_temperature = g.oil_temp;
        t.fuel_level = g.fuel * 100.0;
        Ok(())
    }
}

impl Source for BeamNgSource {
    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        if name == "car_name" {
            let (Some(gauge), Some(_)) = (&self.gauge, &self.sim) else { return Some(LegacyValue::Int(0)) };
            return String::from_utf8(gauge.car.to_vec()).ok().map(LegacyValue::Str);
        }
        if name == "track_loc" {
            return Some(LegacyValue::Int(0));
        }
        if !Telemetry::CHANNELS.contains(&name) && !matches!(name, "yaw_rate" | "pitch_rate" | "roll_rate") {
            return None;
        }
        let (Some(gauge), Some(sim)) = (&self.gauge, &self.sim) else { return Some(LegacyValue::Int(0)) };
        Some(match name {
            "rpm" => return int_text(gauge.rpm).map(LegacyValue::Str),
            "max_rpm" => LegacyValue::Str("30000".into()),
            "gear" => LegacyValue::Str((i64::from(gauge.gear) - 1).to_string()),
            "speed" => return int_text(gauge.speed).map(LegacyValue::Str),
            "lateral_acceleration" | "steering_wheel_angle" => LegacyValue::Float(-sim.accel[0]),
            "vertical_acceleration" => LegacyValue::Float(sim.accel[2]),
            "longitudinal_acceleration" => LegacyValue::Float(sim.accel[1]),
            "vertical_velocity" => LegacyValue::Float(sim.vel[1]),
            "lateral_velocity" => {
                let value = slip_angle(gauge.speed * 3.6, sim.yaw, sim.pitch, sim.vel[0], sim.vel[1], sim.yaw_rate)?;
                if gauge.speed * 3.6 > 5.0 {
                    LegacyValue::Float(value)
                } else {
                    LegacyValue::Int(0)
                }
            }
            "roll" => LegacyValue::Float(rad_to_deg(sim.roll)),
            "pitch" => LegacyValue::Float(rad_to_deg(sim.pitch)),
            "yaw" => LegacyValue::Float(rad_to_deg(sim.yaw)),
            "oil_temperature" => LegacyValue::Float(gauge.oil_temp),
            "fuel_level" => LegacyValue::Float(gauge.fuel * 100.0),
            _ => LegacyValue::Int(0),
        })
    }

    fn channel_read_raised(&self, name: &str) -> bool {
        let (Some(gauge), Some(sim)) = (&self.gauge, &self.sim) else { return false };
        match name {
            "rpm" => !gauge.rpm.is_finite(),
            "speed" => !gauge.speed.is_finite(),
            "car_name" => std::str::from_utf8(&gauge.car).is_err(),
            "lateral_velocity" => {
                slip_angle(gauge.speed * 3.6, sim.yaw, sim.pitch, sim.vel[0], sim.vel[1], sim.yaw_rate).is_none()
            }
            _ => false,
        }
    }

    fn id(&self) -> &'static str {
        "beamng"
    }

    /// The liveness test: one datagram from a fresh socket that stays open for the receive step.
    /// Creation failure answers false; a bind or timeout-setting failure also records game errors.
    fn is_running(&mut self) -> Result<bool, SourceError> {
        let running = match self.receiver.receive_and_keep() {
            Ok(data) => !data.is_empty(),
            Err(ReceiveFailure::Bind(e)) => {
                if !socket_creation_failed(&e) {
                    self.game_errors.record(&e);
                }
                false
            }
            Err(ReceiveFailure::Receive(_)) => false,
        };
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
    use crate::udp_racing::forward::test_forwarder;
    use crate::udp_racing::support::net::fake::{FakeNetwork, ForwardSink};
    use crate::udp_racing::unhandled::assert_ends;

    #[test]
    fn native_beamng_imported_port_retries_without_losing_the_prior_streams() {
        use crate::udp_racing::support::net::Datagrams;
        use std::sync::{Arc, Mutex};
        struct Staged {
            calls: Arc<Mutex<usize>>,
            good: FakeNetwork,
        }
        impl Network for Staged {
            fn bind(&mut self, _: &Endpoint) -> std::io::Result<Box<dyn Datagrams>> {
                panic!("native path")
            }
            fn bind_imported(
                &mut self,
                endpoint: &Endpoint,
                port: ImportedPort,
            ) -> std::io::Result<Box<dyn Datagrams>> {
                assert_eq!(port, ImportedPort::Valid(6001));
                assert_eq!(endpoint.bind.ip(), &std::net::Ipv4Addr::LOCALHOST);
                assert_eq!(endpoint.timeout, Some(timeout_secs(1.0 / 7.0)));
                let mut calls = self.calls.lock().unwrap();
                *calls += 1;
                match *calls {
                    1 => Err(crate::udp_racing::socket::socket_creation_error(std::io::Error::other(
                        "allocation denied",
                    ))),
                    2 => Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "bind(): port must be 0-65535.")),
                    3 => Err(std::io::Error::other("timeout setting denied")),
                    _ => {
                        let mut endpoint = endpoint.clone();
                        endpoint.bind.set_port(6001);
                        self.good.bind(&endpoint)
                    }
                }
            }
        }
        let calls = Arc::new(Mutex::new(0));
        let good = FakeNetwork::default();
        good.push(6001, &[1]);
        good.push(6001, &outgauge());
        good.push(6001, &outsim());
        let mut source = BeamNgSource::with_imported_network(
            ImportedPort::Valid(6001),
            Box::new(Staged { calls: calls.clone(), good }),
        );
        source.gauge = Some(OutGauge::parse(&outgauge()).unwrap());
        source.sim = Some(OutSim::parse(&outsim()).unwrap());
        let gauge = source.gauge.clone();
        let sim = source.sim;
        for expected_count in [0, 1, 2] {
            assert!(!source.is_running().unwrap());
            assert_eq!(source.game_errors().len(), expected_count);
            assert_eq!(source.gauge, gauge);
            assert_eq!(source.sim, sim);
        }
        assert!(source.is_running().unwrap());
        source.receive_native(&mut Telemetry::off()).unwrap();
        assert!(source.game_errors().is_empty());
        assert_eq!(*calls.lock().unwrap(), 4);
    }

    #[test]
    fn native_beamng_creation_failure_keeps_prior_streams_without_game_error() {
        struct Failure;
        impl Network for Failure {
            fn bind(&mut self, _: &Endpoint) -> std::io::Result<Box<dyn super::super::support::net::Datagrams>> {
                Err(crate::udp_racing::socket::socket_creation_error(std::io::Error::other("create failed")))
            }
        }
        let mut source = BeamNgSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(Failure));
        source.gauge = Some(OutGauge::parse(&outgauge()).unwrap());
        source.sim = Some(OutSim::parse(&outsim()).unwrap());
        let gauge = source.gauge.clone();
        let sim = source.sim;
        assert!(!source.is_running().unwrap());
        assert!(source.game_errors().is_empty(), "creation is caught separately from bind/settimeout");
        assert_eq!(source.gauge, gauge);
        assert_eq!(source.sim, sim);
    }

    fn outgauge() -> Vec<u8> {
        let mut d = vec![0u8; OUTGAUGE_LEN];
        d[4..8].copy_from_slice(b"beam");
        d[10] = 3;
        d[12..16].copy_from_slice(&20.5f32.to_le_bytes());
        d[16..20].copy_from_slice(&3500.9f32.to_le_bytes());
        d[28..32].copy_from_slice(&0.5f32.to_le_bytes());
        d[36..40].copy_from_slice(&95.0f32.to_le_bytes());
        d
    }

    fn outsim() -> Vec<u8> {
        let mut d = b"BNG1".to_vec();
        let mut f = [0f32; 21];
        f[3] = 2.0;
        f[4] = 20.0;
        f[6] = 1.5;
        f[7] = -3.0;
        f[8] = 9.0;
        f[12] = 0.1;
        f[13] = 0.0;
        f[14] = 0.0;
        f[17] = 0.5;
        d.extend(f.iter().flat_map(|v| v.to_le_bytes()));
        d
    }

    #[test]
    fn native_getters_keep_text_numbers_float_measurements_and_integer_defaults() {
        use crate::derive::{Listener, SourceListener};
        use openrigcue_model::native_value::LegacyValue::{Float, Int, Str};
        let net = FakeNetwork::default();
        let source = BeamNgSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        let mut listener = SourceListener::new(source);
        net.push(DEFAULT_PORT, &[1]);
        net.push(DEFAULT_PORT, &outgauge());
        net.push(DEFAULT_PORT, &outsim());
        assert!(listener.is_running());
        listener.receive();
        for (name, value) in [
            ("rpm", Str("3500".into())),
            ("max_rpm", Str("30000".into())),
            ("gear", Str("2".into())),
            ("speed", Str("20".into())),
            ("lateral_acceleration", Float(-1.5)),
            ("steering_wheel_angle", Float(-1.5)),
            ("vertical_velocity", Float(20.0)),
            ("oil_temperature", Float(95.0)),
            ("fuel_level", Float(50.0)),
            ("raw_flag", Int(0)),
            ("yaw_rate", Int(0)),
        ] {
            assert_eq!(listener.native_channel(name), Some(value), "{name}");
        }
        net.push(DEFAULT_PORT, &[1]);
        let mut gauge = outgauge();
        gauge[16..20].copy_from_slice(&f32::NAN.to_le_bytes());
        net.push(DEFAULT_PORT, &gauge);
        net.push(DEFAULT_PORT, &outsim());
        assert!(listener.is_running());
        listener.receive();
        assert_eq!(listener.native_channel("rpm"), None);
        assert_eq!(listener.channel("rpm"), None, "a raised getter cannot expose the earlier RPM");
    }

    #[test]
    fn slip_trigonometry_infinity_raises_before_stationary_speed_gate() {
        let mut source =
            BeamNgSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(FakeNetwork::default()));
        let mut gauge = OutGauge::parse(&outgauge()).unwrap();
        gauge.speed = 0.0;
        source.gauge = Some(gauge);
        let base = OutSim::parse(&outsim()).unwrap();
        for (yaw, pitch) in [(f64::INFINITY, 0.0), (0.0, f64::NEG_INFINITY)] {
            let mut sim = base;
            sim.yaw = yaw;
            sim.pitch = pitch;
            source.sim = Some(sim);
            assert!(source.channel_read_raised("lateral_velocity"));
            assert_eq!(source.read_native("lateral_velocity"), crate::NativeRead::Raised);
            assert_eq!(source.native_channel("speed"), Some(LegacyValue::Str("0".into())));
        }
        assert_eq!(slip_angle(0.0, f64::NAN, 0.0, 1.0, 2.0, 1.0), Some(0.0));
    }

    #[test]
    fn native_missing_streams_return_integer_zero_and_slip_failures_remain_failed_reads() {
        use openrigcue_model::native_value::LegacyValue;
        let mut source =
            BeamNgSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(FakeNetwork::default()));
        for name in ["rpm", "speed", "gear", "max_rpm", "lateral_velocity", "raw_flag"] {
            assert_eq!(source.native_channel(name), Some(LegacyValue::Int(0)), "{name}");
            assert!(!source.channel_read_raised(name));
        }
        source.gauge = Some(OutGauge::parse(&outgauge()).unwrap());
        source.sim = Some(OutSim::parse(&outsim()).unwrap());
        source.gauge.as_mut().unwrap().speed = 0.0;
        assert_eq!(source.native_channel("lateral_velocity"), Some(LegacyValue::Int(0)));
        source.gauge.as_mut().unwrap().speed = 20.0;
        source.sim.as_mut().unwrap().vel = [0.0; 3];
        assert!(source.channel_read_raised("lateral_velocity"));
        assert_eq!(source.native_channel("lateral_velocity"), None);
        source.sim.as_mut().unwrap().vel = [2.0, 20.0, 0.0];
        source.sim.as_mut().unwrap().yaw = f64::NAN;
        assert!(!source.channel_read_raised("lateral_velocity"), "NaN extrema are values, not zero spans");
        assert_eq!(source.native_channel("lateral_velocity"), Some(LegacyValue::Float(0.0)));
    }

    #[test]
    fn normalize_retains_the_first_builtin_extreme_when_it_is_nan() {
        let values = normalize([f64::NAN, 1.0, 2.0]).unwrap();
        assert!(values.iter().all(|value| value.is_nan()));
        let values = normalize([1.0, f64::NAN, 2.0]).unwrap();
        assert_eq!(values[0], 0.0);
        assert!(values[1].is_nan());
        assert_eq!(values[2], 1.0);
    }

    #[test]
    fn normalize_and_slip_angle() {
        // Normalisation, dot product and slip angle
        assert_eq!(normalize([1.0, 3.0, 2.0]), Some([0.0, 1.0, 0.5]));
        assert_eq!(normalize([0.0, 0.0, 0.0]), None);
        assert_eq!(dot([1.0, 2.0, 3.0], [4.0, 5.0, 6.0]), 32.0);
        assert_eq!(slip_angle(5.0, 0.3, 0.0, 1.0, 2.0, 1.0), Some(0.0), "speed not above 5 km/h");
        // The "speed above 5 km/h" test is false for NaN: the slip angle stays 0
        assert_eq!(slip_angle(f64::NAN, 0.0, 0.0, 2.0, 20.0, 0.5), Some(0.0));
        // yaw 0: forward normalises to [0, 1, 0]; velocity [2, 20, 0] -> [0.1, 1, 0]; d = 1 -> acos(0)
        let s = slip_angle(73.8, 0.0, 0.0, 2.0, 20.0, 0.5).unwrap();
        assert!((s - std::f64::consts::FRAC_PI_2 * 0.5).abs() < 1e-12);
    }

    #[test]
    fn two_datagrams_per_poll() {
        // Receive step: 96-byte OutGauge then 88-byte OutSim; channel mapping
        let net = FakeNetwork::default();
        let mut s = BeamNgSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        net.push(DEFAULT_PORT, &[1]);
        net.push(DEFAULT_PORT, &outgauge());
        net.push(DEFAULT_PORT, &outsim());
        assert!(s.is_running().unwrap());
        let mut t = Telemetry::off();
        assert!(s.poll(&mut t).unwrap());
        assert_eq!((t.rpm, t.max_rpm, t.gear, t.speed), (3500.0, 30000.0, 2.0, 20.0));
        assert_eq!(t.vehicle, "beam");
        assert_eq!((t.lateral_acceleration, t.steering_wheel_angle), (-1.5, -1.5));
        assert_eq!((t.vertical_acceleration, t.longitudinal_acceleration, t.vertical_velocity), (9.0, -3.0, 20.0));
        assert!((t.lateral_velocity - std::f64::consts::FRAC_PI_2 * 0.5).abs() < 1e-12);
        assert_eq!((t.oil_temperature, t.fuel_level), (95.0, 50.0));
        assert!((t.roll - rad_to_deg(f64::from(0.1f32))).abs() < 1e-12);
    }

    #[test]
    fn gauge_survives_a_failed_second_receive() {
        // Receive step: the OutGauge packet is stored before the second receive
        let net = FakeNetwork::default();
        let mut s = BeamNgSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        net.push(DEFAULT_PORT, &[1]);
        net.push(DEFAULT_PORT, &outgauge());
        net.push_timeout(DEFAULT_PORT);
        assert!(s.is_running().unwrap());
        let mut t = Telemetry::off();
        assert!(!s.poll(&mut t).unwrap(), "second receive timed out");
        net.push(DEFAULT_PORT, &[1]);
        net.push(DEFAULT_PORT, &[0u8; 10]);
        net.push(DEFAULT_PORT, &outsim());
        assert!(s.is_running().unwrap());
        assert!(s.poll(&mut t).unwrap());
        assert_eq!((t.rpm, t.gear, t.speed), (3500.0, 2.0, 20.0), "gauge from the earlier poll");
    }

    #[test]
    fn only_check_status_forwards() {
        // Only the liveness test forwards, not the two datagrams of the receive step
        let sink = ForwardSink::new();
        let net = FakeNetwork::default();
        let mut s = BeamNgSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        s.receiver.set_forwarder(test_forwarder("beamng", &[sink.target()]).0);
        for _ in 0..2 {
            net.push(DEFAULT_PORT, &[7]);
            net.push(DEFAULT_PORT, &outgauge());
            net.push(DEFAULT_PORT, &outsim());
            assert!(s.is_running().unwrap());
            assert!(s.poll(&mut Telemetry::off()).unwrap());
        }
        assert_eq!(sink.received(), vec![vec![7], vec![7]], "only the liveness datagrams");
    }

    #[test]
    fn bind_errors_are_recorded_until_good_data() {
        // A bind failure adds its text to the game errors; a good receive step clears them
        let net = FakeNetwork::default();
        let mut s = BeamNgSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        assert!(!s.is_running().unwrap());
        assert!(s.game_errors().is_empty(), "a receive timeout is not recorded");
        net.refuse_bind(true);
        assert!(!s.is_running().unwrap());
        assert_eq!(s.game_errors().iter().collect::<Vec<_>>(), [crate::udp_racing::parameters::PORT_IN_USE]);
        net.refuse_bind(false);
        net.push(DEFAULT_PORT, &[1]);
        net.push(DEFAULT_PORT, &outgauge());
        assert!(s.is_running().unwrap());
        assert!(!s.poll(&mut Telemetry::off()).unwrap());
        assert_eq!(s.game_errors().len(), 1, "kept when the second receive fails");
        net.push(DEFAULT_PORT, &[1]);
        net.push(DEFAULT_PORT, &outgauge());
        net.push(DEFAULT_PORT, &outsim());
        assert!(s.is_running().unwrap());
        assert!(s.poll(&mut Telemetry::off()).unwrap());
        assert!(s.game_errors().is_empty());
    }

    fn poll_with_gauge(gauge: &[u8], before: Telemetry) -> (Result<bool, SourceError>, Telemetry) {
        let net = FakeNetwork::default();
        let mut s = BeamNgSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        net.push(DEFAULT_PORT, &[1]);
        net.push(DEFAULT_PORT, gauge);
        net.push(DEFAULT_PORT, &outsim());
        assert!(s.is_running().unwrap());
        let mut t = before;
        (s.poll(&mut t), t)
    }

    #[test]
    fn native_receive_retains_first_stream_on_timeout_without_eager_getters() {
        let net = FakeNetwork::default();
        let mut source = BeamNgSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        let mut gauge = outgauge();
        gauge[12..16].copy_from_slice(&f32::INFINITY.to_le_bytes());
        net.push(DEFAULT_PORT, &[1]);
        net.push(DEFAULT_PORT, &gauge);
        net.push_timeout(DEFAULT_PORT);
        assert!(source.is_running().unwrap());
        let before = Telemetry { speed: 23.0, rpm: 1300.0, ..Telemetry::off() };
        let mut compatibility = before.clone();
        assert!(!source.receive_native(&mut compatibility).unwrap());
        assert_eq!(compatibility, before);
        assert!(source.gauge.is_some());
        assert!(source.sim.is_none());
        net.push(DEFAULT_PORT, &[1]);
        net.push(DEFAULT_PORT, &[0]);
        net.push(DEFAULT_PORT, &outsim());
        assert!(source.is_running().unwrap());
        assert!(source.receive_native(&mut compatibility).unwrap());
        assert_eq!(compatibility, before);
        assert_eq!(source.read_native("speed"), crate::NativeRead::Raised);
        assert_eq!(source.native_channel("rpm"), Some(LegacyValue::Str("3500".into())));
        assert_eq!(source.native_channel("car_name"), Some(LegacyValue::Str("beam".into())));
        source.gauge.as_mut().unwrap().car = [0xff, 0, 0, 0];
        assert_eq!(source.read_native("car_name"), crate::NativeRead::Raised);
    }

    #[test]
    fn non_finite_rpm_keeps_the_previous_rpm() {
        // A NaN rpm has no integer value; the dispatcher keeps rpm (default None)
        let mut gauge = outgauge();
        gauge[16..20].copy_from_slice(&f32::NAN.to_le_bytes());
        let (result, t) = poll_with_gauge(&gauge, Telemetry { rpm: 4100.0, ..Telemetry::off() });
        assert!(result.unwrap());
        assert_eq!((t.rpm, t.speed, t.max_rpm), (4100.0, 20.0, 30000.0));
    }

    #[test]
    fn non_finite_speed_ends_the_tick_after_lateral_velocity() {
        // An infinite speed has no integer value; the dispatcher's top-speed update does not handle that,
        // so only car_name, track_loc (0) and lateral_velocity are stored
        let mut gauge = outgauge();
        gauge[12..16].copy_from_slice(&f32::INFINITY.to_le_bytes());
        let before = Telemetry { rpm: 4100.0, speed: 12.0, pitch: 3.0, location: "x".into(), ..Telemetry::off() };
        let (result, t) = poll_with_gauge(&gauge, before.clone());
        assert_ends(result);
        let lateral = slip_angle(f64::INFINITY, 0.0, 0.0, 2.0, 20.0, 0.5).unwrap();
        let expected =
            Telemetry { vehicle: "beam".into(), location: String::new(), lateral_velocity: lateral, ..before };
        assert_eq!(t, expected);
    }

    #[test]
    fn missing_stream_reads_zero() {
        // Without a gauge or sim packet every channel reads 0; a receive timeout answers false
        let net = FakeNetwork::default();
        let mut s = BeamNgSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        net.push(DEFAULT_PORT, &[1]);
        net.push(DEFAULT_PORT, &outgauge());
        assert!(s.is_running().unwrap());
        let mut t = Telemetry::off();
        assert!(!s.poll(&mut t).unwrap());
        assert_eq!(t.max_rpm, 0.0);
    }
}
