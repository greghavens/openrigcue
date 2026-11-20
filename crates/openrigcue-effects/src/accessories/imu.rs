//! WitMotion JY901 IMU for motion compensation: sensor configuration, the 11-byte packet stream and the
//! port scan. The compensation pass that uses its angles is [`super::compensation`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tracing::debug;

use super::compensation::NativeCompensationError;
use super::link::{Clock, PortLister, PortOpener, PortSettings, SerialLink};
use super::port_scan::{AllowedPorts, ClaimedPorts, Device, Exclusions};
use openrigcue_model::native_value::LegacyValue;

/// WitMotion register commands (`FF AA <reg> <lo> <hi>`).
pub mod command {
    /// Unlock the registers (key 0xB588).
    pub const UNLOCK: [u8; 5] = [0xFF, 0xAA, 0x69, 0x88, 0xB5];
    /// Installation orientation horizontal / vertical (register 0x23).
    pub const HORIZONTAL: [u8; 5] = [0xFF, 0xAA, 0x23, 0x00, 0x00];
    pub const VERTICAL: [u8; 5] = [0xFF, 0xAA, 0x23, 0x01, 0x00];
    /// Serial speed 115200 (register 0x04 = 6).
    pub const BAUD_115200: [u8; 5] = [0xFF, 0xAA, 0x04, 0x06, 0x00];
    /// Filter bandwidth 42 Hz (register 0x1F = 3).
    pub const BANDWIDTH_42HZ: [u8; 5] = [0xFF, 0xAA, 0x1F, 0x03, 0x00];
    /// Output rate 200 Hz (register 0x03 = 0x0B).
    pub const RATE_200HZ: [u8; 5] = [0xFF, 0xAA, 0x03, 0x0B, 0x00];
    /// Save the configuration (register 0x00 = 0).
    pub const SAVE: [u8; 5] = [0xFF, 0xAA, 0x00, 0x00, 0x00];
}

/// Pause between configuration commands.
const COMMAND_GAP: Duration = Duration::from_millis(200);
/// Length of one WitMotion packet.
pub const PACKET_LEN: usize = 11;
const ANGLE_PACKET: u8 = 0x53;
const GYRO_PACKET: u8 = 0x52;
/// Full scale of the angle packet (`/ 32768 * 180`), degrees.
const ANGLE_RANGE: f64 = 180.0;
/// Full scale of the angular velocity packet (`/ 32768 * 2000`), degrees per second.
const GYRO_RANGE: f64 = 2000.0;

/// Sensor mounting (`MOTION_COMPENSATION.installation`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Installation {
    Horizontal,
    Vertical,
}

impl Installation {
    /// The orientation a code selects: `'H'` is horizontal, every other value vertical.
    pub fn from_code(code: &str) -> Installation {
        if code == "H" {
            Installation::Horizontal
        } else {
            Installation::Vertical
        }
    }
}

/// Reads `MOTION_COMPENSATION.installation` as it is when called (the configuration sequence looks
/// the code up when it gets to the orientation command).
pub type InstallationReader = Arc<dyn Fn() -> String + Send + Sync>;

/// Required configuration read, retaining values whose string conversion would change equality with `H`.
pub type NativeInstallationReader = Arc<dyn Fn() -> Result<LegacyValue, NativeCompensationError> + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub enum ConfigurationError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Native(#[from] NativeCompensationError),
}

#[derive(Debug, thiserror::Error)]
pub enum ImuScanError {
    #[error(transparent)]
    Ports(#[from] super::link::PortScanError),
    #[error(transparent)]
    Native(#[from] NativeCompensationError),
}

fn send_sequence(link: &mut dyn SerialLink, commands: &[&[u8; 5]], clock: &dyn Clock) -> std::io::Result<()> {
    for c in commands {
        link.write(*c)?;
        clock.sleep(COMMAND_GAP);
    }
    Ok(())
}

/// Baud-rate change: unlock, switch to 115200, save (0.2 s apart).
pub fn set_baud_rate(link: &mut dyn SerialLink, clock: &dyn Clock) -> std::io::Result<()> {
    send_sequence(link, &[&command::UNLOCK, &command::BAUD_115200, &command::SAVE], clock)
}

/// Sensor configuration: unlock, 200 Hz, 42 Hz bandwidth, orientation, save (0.2 s apart). The
/// installation code is read after the bandwidth command's pause.
pub fn set_config(
    link: &mut dyn SerialLink,
    installation: &dyn Fn() -> String,
    clock: &dyn Clock,
) -> std::io::Result<()> {
    send_sequence(link, &[&command::UNLOCK, &command::RATE_200HZ, &command::BANDWIDTH_42HZ], clock)?;
    let orientation = match Installation::from_code(&installation()) {
        Installation::Horizontal => &command::HORIZONTAL,
        Installation::Vertical => &command::VERTICAL,
    };
    send_sequence(link, &[orientation, &command::SAVE], clock)
}

pub fn set_config_native(
    link: &mut dyn SerialLink,
    installation: &NativeInstallationReader,
    clock: &dyn Clock,
) -> Result<(), ConfigurationError> {
    send_sequence(link, &[&command::UNLOCK, &command::RATE_200HZ, &command::BANDWIDTH_42HZ], clock)?;
    let orientation = if matches!(installation()?, LegacyValue::Str(code) if code == "H") {
        &command::HORIZONTAL
    } else {
        &command::VERTICAL
    };
    send_sequence(link, &[orientation, &command::SAVE], clock)?;
    Ok(())
}

/// Position of the first packet header (`0x55`, then a type byte with bits 0x50 set), or `None`.
pub fn find_first_packet(buffer: &[u8]) -> Option<usize> {
    let mut i = 0;
    loop {
        if buffer.get(i) == Some(&0x55) && buffer.get(i + 1).is_some_and(|t| t & 0x50 == 0x50) {
            return Some(i);
        }
        if i + 2 >= buffer.len() {
            return None;
        }
        i += 1;
    }
}

/// Packet checksum: low byte of the sum of the first 10 bytes equals the 11th.
fn checksum_ok(packet: &[u8]) -> bool {
    packet[..10].iter().map(|b| u32::from(*b)).sum::<u32>() & 0xFF == u32::from(packet[10])
}

/// The X, Y, Z words of a packet of type `kind` scaled to `range` (`word / 32768 * range`).
fn decode_xyz(packet: &[u8], kind: u8, range: f64) -> Option<[f64; 3]> {
    (packet[1] == kind).then(|| {
        std::array::from_fn(|i| f64::from(i16::from_le_bytes([packet[2 + 2 * i], packet[3 + 2 * i]])) / 32768.0 * range)
    })
}

/// What one parser pass decoded: the last angle packet (`0x53`, degrees) and the last angular velocity
/// packet (`0x52`, degrees per second) of the batch.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Decoded {
    /// X, Y, Z angles in degrees.
    pub angles: Option<[f64; 3]>,
    /// X, Y, Z angular velocities (`word / 32768 * 2000`, °/s) of a `0x52` packet,
    /// used as the tilt rates for VR prediction.
    pub rates: Option<[f64; 3]>,
}

/// Consume whole packets from `buffer` and return what they decoded.
///
/// One parser pass: with at least 11 bytes, drop the bytes before the first
/// header, cut every whole packet, and decode those whose checksum matches (`0x53` angles,
/// `0x52` angular velocities; later packets of a batch overwrite earlier ones).
pub fn consume_packets(buffer: &mut Vec<u8>) -> Decoded {
    let mut decoded = Decoded::default();
    let length = buffer.len();
    if length < PACKET_LEN {
        return decoded;
    }
    let Some(header) = find_first_packet(buffer) else { return decoded };
    buffer.drain(..header);
    let packets = (length - header) / PACKET_LEN;
    let cut: Vec<u8> = buffer.drain(..packets * PACKET_LEN).collect();
    for packet in cut.chunks(PACKET_LEN).filter(|p| checksum_ok(p)) {
        if let Some(angles) = decode_xyz(packet, ANGLE_PACKET, ANGLE_RANGE) {
            decoded.angles = Some(angles);
        }
        if let Some(rates) = decode_xyz(packet, GYRO_PACKET, GYRO_RANGE) {
            decoded.rates = Some(rates);
        }
    }
    decoded
}

/// One IMU reading as the VR pose stream sees it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ImuSample {
    /// Counts the decoded angle packets batches; increases by one per new reading.
    pub sequence: u64,
    /// X, Y, Z angles in degrees.
    pub angles: [f64; 3],
    /// When the parser decoded the angles.
    pub received: Instant,
    /// The latest angular velocities (°/s) and when they were decoded, if the sensor sends `0x52` packets.
    pub rates: Option<([f64; 3], Instant)>,
}

#[derive(Debug, Default)]
struct Reading {
    angles: [f64; 3],
    sequence: u64,
    received: Option<Instant>,
    rates: Option<([f64; 3], Instant)>,
}

/// Readings shared between the parser thread, the compensation step and the VR pose stream.
#[derive(Debug, Default)]
struct SensorState {
    reading: Mutex<Reading>,
    fresh: Condvar,
    stop: AtomicBool,
}

impl SensorState {
    fn lock(&self) -> MutexGuard<'_, Reading> {
        self.reading.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn record(&self, decoded: Decoded, now: Instant) {
        let mut reading = self.lock();
        if let Some(rates) = decoded.rates {
            reading.rates = Some((rates, now));
        }
        if let Some(angles) = decoded.angles {
            reading.angles = angles;
            reading.sequence += 1;
            reading.received = Some(now);
            self.fresh.notify_all();
        }
    }
}

fn sample_of(reading: &Reading) -> Option<ImuSample> {
    reading.received.map(|received| ImuSample {
        sequence: reading.sequence,
        angles: reading.angles,
        received,
        rates: reading.rates,
    })
}

/// A handle on a connected IMU's readings that can wait for the next one without holding the accessory
/// lock (the VR pose stream runs at the sensor's output rate, 200 Hz once configured).
#[derive(Debug, Clone)]
pub struct ImuSamples(Arc<SensorState>);

impl ImuSamples {
    /// The latest reading, if any angle packet has been decoded.
    pub fn latest(&self) -> Option<ImuSample> {
        sample_of(&self.0.lock())
    }

    /// Waits up to `timeout` for a reading newer than `after` (a previous [`ImuSample::sequence`], or 0).
    /// `None` on timeout or once the IMU has been closed.
    pub fn wait_newer(&self, after: u64, timeout: Duration) -> Option<ImuSample> {
        let guard = self.0.lock();
        let (guard, _) = self
            .0
            .fresh
            .wait_timeout_while(guard, timeout, |r| r.sequence <= after && !self.0.stop.load(Ordering::SeqCst))
            .unwrap_or_else(PoisonError::into_inner);
        if self.0.stop.load(Ordering::SeqCst) || guard.sequence <= after {
            return None;
        }
        sample_of(&guard)
    }

    /// True once the IMU it came from was closed.
    pub fn closed(&self) -> bool {
        self.0.stop.load(Ordering::SeqCst)
    }
}

/// An IMU streaming angles: the serial link and its parser thread.
pub struct Imu {
    link: Box<dyn SerialLink>,
    state: Arc<SensorState>,
    parser: Option<JoinHandle<()>>,
}

/// Parser thread: read what is waiting (1 ms poll), consume packets, idle 5 ms when short of a packet.
fn parse_loop(link: &mut dyn SerialLink, mut buffer: Vec<u8>, state: &SensorState, clock: &dyn Clock) {
    while !state.stop.load(Ordering::SeqCst) {
        clock.sleep(Duration::from_millis(1));
        match link.read_available() {
            Ok(bytes) => buffer.extend(bytes),
            Err(e) => debug!("JY901 inWaiting() failed: {e}"),
        }
        if buffer.len() >= PACKET_LEN {
            state.record(consume_packets(&mut buffer), Instant::now());
        } else {
            clock.sleep(Duration::from_millis(5));
        }
    }
}

impl Imu {
    /// Start the parser on `link`, continuing with the bytes already received.
    fn start(link: Box<dyn SerialLink>, received: Vec<u8>, clock: Arc<dyn Clock>) -> std::io::Result<Self> {
        let state = Arc::new(SensorState::default());
        let mut reader = link.try_clone()?;
        let rx = Arc::clone(&state);
        let parser = std::thread::Builder::new()
            .name("jy901_handle".into())
            .spawn(move || parse_loop(reader.as_mut(), received, &rx, clock.as_ref()))?;
        Ok(Imu { link, state, parser: Some(parser) })
    }

    /// Latest X, Y, Z angles.
    pub fn angles(&self) -> [f64; 3] {
        self.state.lock().angles
    }

    /// The reading stream, for consumers that follow every sample (the VR pose stream).
    pub fn samples(&self) -> ImuSamples {
        ImuSamples(Arc::clone(&self.state))
    }

    /// Send the configuration on a background thread; `installation` is read
    /// when the sequence gets to the orientation.
    pub fn send_installation(
        &self,
        installation: InstallationReader,
        clock: Arc<dyn Clock>,
    ) -> std::io::Result<JoinHandle<()>> {
        let mut writer = self.link.try_clone()?;
        std::thread::Builder::new().name("jy901_set_config".into()).spawn(move || {
            if let Err(e) = set_config(writer.as_mut(), installation.as_ref(), clock.as_ref()) {
                debug!("JY901 set_config failed: {e}");
            }
        })
    }

    pub fn send_native_installation(
        &self,
        installation: NativeInstallationReader,
        clock: Arc<dyn Clock>,
    ) -> std::io::Result<JoinHandle<()>> {
        let mut writer = self.link.try_clone()?;
        std::thread::Builder::new().name("jy901_set_config".into()).spawn(move || {
            if let Err(error) = set_config_native(writer.as_mut(), &installation, clock.as_ref()) {
                debug!("JY901 set_config failed: {error}");
            }
        })
    }
}

impl Drop for Imu {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::SeqCst);
        self.state.fresh.notify_all();
        if let Some(handle) = self.parser.take() {
            let _ = handle.join();
        }
    }
}

/// Environment of an IMU scan.
pub struct ImuScanContext<'a> {
    pub ports: &'a dyn PortLister,
    pub opener: &'a dyn PortOpener,
    pub clock: Arc<dyn Clock>,
    pub claimed: &'a ClaimedPorts,
    /// The port remembered from the last successful connection.
    pub cached_port: Option<&'a str>,
    /// `MOTION_COMPENSATION.com_port`.
    pub com_port: &'a str,
    pub allowed: &'a AllowedPorts,
    /// `MOTION_COMPENSATION.installation`, read by the configuration sequence after the connection.
    pub installation: &'a InstallationReader,
}

/// The CH340 bridge of the WitMotion USB adapter.
const USB_ID: &str = "1A86:7523";
/// Read attempts at 115200 baud.
const READ_TRIES: usize = 15;

/// A connected IMU.
pub struct ConnectedImu {
    pub port: String,
    pub imu: Imu,
}

/// Look for the IMU (only when `MOTION_COMPENSATION.type` is 'W').
///
/// Cached port first; a 9600-baud pass reprograms sensors to 115200; the
/// 115200 pass reads up to 15 times (0.2 s apart without data, 1 s apart with less than a packet) until a
/// packet header shows up, then starts the parser and sends the configuration.
pub fn scan(ctx: &ImuScanContext<'_>) -> Result<Option<ConnectedImu>, super::link::PortScanError> {
    match scan_native(ctx, None) {
        Ok(found) => Ok(found),
        Err(ImuScanError::Ports(error)) => Err(error),
        Err(ImuScanError::Native(_)) => unreachable!("compatibility reader has no native errors"),
    }
}

pub fn scan_native(
    ctx: &ImuScanContext<'_>,
    installation: Option<&NativeInstallationReader>,
) -> Result<Option<ConnectedImu>, ImuScanError> {
    let mut ports = ctx.ports.ports().map_err(super::link::PortScanError::Listing)?;
    ports.sort_by_key(|p| Some(p.device.as_str()) != ctx.cached_port);
    for baud_rate in [9600u32, 115_200] {
        for port in &ports {
            let device = port.device.as_str();
            let rules_ok = !Exclusions::RaspberryUart.excludes(device)
                && (ctx.com_port.is_empty() || ctx.com_port == device)
                && ctx.allowed.allows(device)
                && !ctx.claimed.in_use_for(Device::MotionCompensation, device);
            if !rules_ok || !port.hwid_contains(USB_ID)? {
                continue;
            }
            let settings = PortSettings { baud_rate, read_timeout: None };
            let Ok(mut link) = ctx.opener.open(device, settings) else { continue };
            if baud_rate == 9600 {
                debug!("JY901 {device}: sending set_baud_rate sequence at 9600");
                let _ = set_baud_rate(link.as_mut(), ctx.clock.as_ref());
                continue;
            }
            if let Some(found) = probe(ctx, device, link, installation)? {
                return Ok(Some(found));
            }
        }
    }
    Ok(None)
}

fn probe(
    ctx: &ImuScanContext<'_>,
    device: &str,
    mut link: Box<dyn SerialLink>,
    installation: Option<&NativeInstallationReader>,
) -> Result<Option<ConnectedImu>, ImuScanError> {
    let mut buffer = Vec::new();
    for attempt in 1..=READ_TRIES {
        ctx.clock.sleep(Duration::from_millis(1));
        let bytes = link.read_available().unwrap_or_default();
        if bytes.is_empty() {
            debug!("JY901 {device} @115200 try {attempt}/{READ_TRIES}: no data");
            ctx.clock.sleep(Duration::from_millis(200));
            continue;
        }
        buffer.extend(bytes);
        if buffer.len() < PACKET_LEN {
            ctx.clock.sleep(Duration::from_secs(1));
            continue;
        }
        if find_first_packet(&buffer).is_some() {
            let Ok(mut imu) = Imu::start(link, buffer, Arc::clone(&ctx.clock)) else { return Ok(None) };
            let result = match installation {
                Some(installation) => set_config_native(imu.link.as_mut(), installation, ctx.clock.as_ref()),
                None => set_config(imu.link.as_mut(), ctx.installation.as_ref(), ctx.clock.as_ref())
                    .map_err(ConfigurationError::Io),
            };
            if !configured(result)? {
                return Ok(None);
            }
            return Ok(Some(ConnectedImu { port: device.to_string(), imu }));
        }
    }
    Ok(None)
}

fn configured(result: Result<(), ConfigurationError>) -> Result<bool, ImuScanError> {
    match result {
        Ok(()) => Ok(true),
        Err(ConfigurationError::Io(_)) => Ok(false),
        Err(ConfigurationError::Native(error)) => Err(error.into()),
    }
}

/// Sensor fakes shared with the compensation tests.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Arc;

    use super::super::link::fakes::{FakeLink, ManualClock};
    use super::super::link::Clock;
    use super::Imu;

    /// One WitMotion packet of type `kind` (`0x55 <kind>`, X/Y/Z/temperature little-endian, sum byte).
    pub(crate) fn packet(kind: u8, x: i16, y: i16, z: i16) -> Vec<u8> {
        let mut p = vec![0x55, kind];
        for v in [x, y, z, 2500] {
            p.extend_from_slice(&v.to_le_bytes());
        }
        let sum: u32 = p.iter().map(|b| u32::from(*b)).sum();
        p.push((sum & 0xFF) as u8);
        p
    }

    /// One WitMotion angle packet (`0x55 0x53`).
    pub(crate) fn angle_packet(x: i16, y: i16, z: i16) -> Vec<u8> {
        packet(0x53, x, y, z)
    }

    /// One WitMotion angular velocity packet (`0x55 0x52`).
    pub(crate) fn gyro_packet(x: i16, y: i16, z: i16) -> Vec<u8> {
        packet(0x52, x, y, z)
    }

    /// An IMU whose parser has decoded one angle packet (X 90°, Y -45°, Z 22.5°).
    pub(crate) fn connected_imu() -> (Imu, FakeLink, Arc<dyn Clock>) {
        let link = FakeLink::new();
        let clock: Arc<dyn Clock> = Arc::new(ManualClock::default());
        let imu = Imu::start(Box::new(link.clone()), angle_packet(16384, -8192, 4096), Arc::clone(&clock)).unwrap();
        for _ in 0..100_000 {
            if imu.angles() != [0.0; 3] {
                break;
            }
            std::thread::yield_now();
        }
        assert_eq!(imu.angles(), [90.0, -45.0, 22.5]);
        (imu, link, clock)
    }
}

#[cfg(test)]
mod tests {
    use super::super::link::fakes::{FakeLink, FakeOpener, FakePorts, ManualClock};
    use super::super::link::PortEntry;
    use super::test_support::{angle_packet, gyro_packet, packet};
    use super::*;

    /// Publish a raw sample handle using only memory and a fake link; no parser is started.
    #[test]
    fn native_vr_published_samples_follow_connection_and_decoded_readings() {
        use super::super::dispatcher::Accessories;
        use super::super::status::{Connection, DeviceStatus};

        let now = Instant::now();
        let accessories = Accessories::new(now);
        assert!(accessories.imu_samples().is_none());
        let state = Arc::new(SensorState::default());
        state.record(Decoded { angles: Some([90.0, -45.0, 22.5]), rates: Some([1.0, 2.0, 3.0]) }, now);
        let imu = Imu { link: Box::new(FakeLink::new()), state: state.clone(), parser: None };
        let mut slot = accessories.imu.lock().unwrap();
        slot.set_connection(Connection::Connected(imu));
        let samples = accessories.imu_samples().unwrap();
        let first = samples.latest().unwrap();
        assert_eq!(first.angles, [90.0, -45.0, 22.5]);
        assert_eq!(first.rates, Some(([1.0, 2.0, 3.0], now)));
        state.record(Decoded { angles: Some([0.0, 1.0, 2.0]), rates: None }, now);
        assert_eq!(accessories.imu_samples().unwrap().latest().unwrap().angles, [0.0, 1.0, 2.0]);
        slot.set_connection(Connection::Offline(DeviceStatus::NotConnected));
        assert!(accessories.imu_samples().is_none());
        assert!(samples.closed());
    }

    /// Header search, parser pass, checksum and angle decoding (0x53).
    #[test]
    fn packet_stream() {
        let mut buffer = vec![0x00, 0x12];
        buffer.extend(angle_packet(16384, -8192, 0));
        buffer.extend(angle_packet(0, 0, 32767));
        let mut bad = angle_packet(1, 1, 1);
        bad[10] ^= 1;
        buffer.extend(bad);
        assert_eq!(find_first_packet(&buffer), Some(2));
        let angles = consume_packets(&mut buffer).angles.unwrap();
        assert!((angles[2] - 32767.0 / 32768.0 * 180.0).abs() < 1e-12);
        assert!(buffer.is_empty());
        let mut short = angle_packet(16384, -8192, 0);
        short.truncate(10);
        assert_eq!(consume_packets(&mut short), Decoded::default());
        let mut first = angle_packet(16384, -8192, 0);
        assert_eq!(consume_packets(&mut first).angles, Some([90.0, -45.0, 0.0]));
        assert_eq!(find_first_packet(&[0x55, 0x20, 0x00]), None);
    }

    /// `0x52` packets decode to `word / 32768.0 * 2000` (°/s); the acceleration
    /// (`0x51`) and other packets leave both readings alone; a batch keeps its last packet of each kind.
    #[test]
    fn gyro_packets_give_angular_velocity() {
        let mut buffer = gyro_packet(16384, -8192, 0);
        buffer.extend(packet(0x51, 100, 100, 100));
        buffer.extend(angle_packet(4096, 0, 0));
        buffer.extend(gyro_packet(-16384, 1638, 32767));
        let decoded = consume_packets(&mut buffer);
        assert_eq!(decoded.angles, Some([22.5, 0.0, 0.0]));
        let rates = decoded.rates.unwrap();
        assert_eq!(rates[0], -1000.0);
        assert!((rates[1] - 1638.0 / 32768.0 * 2000.0).abs() < 1e-12);
        assert!((rates[2] - 32767.0 / 32768.0 * 2000.0).abs() < 1e-12);
        let mut accel_only = packet(0x51, 1, 2, 3);
        assert_eq!(consume_packets(&mut accel_only), Decoded::default());
    }

    /// The reading stream follows the parser: each decoded angle batch raises the sequence and wakes a
    /// waiter; rates ride along; closing the IMU ends the wait.
    #[test]
    fn sample_stream_follows_the_parser() {
        let link = FakeLink::new();
        let clock: Arc<dyn Clock> = Arc::new(ManualClock::default());
        let mut first = gyro_packet(1638, 0, 0);
        first.extend(angle_packet(16384, 0, 0));
        let imu = Imu::start(Box::new(link.clone()), first, Arc::clone(&clock)).unwrap();
        let samples = imu.samples();
        let sample = samples.wait_newer(0, Duration::from_secs(10)).expect("first reading");
        assert_eq!(sample.angles, [90.0, 0.0, 0.0]);
        let (rates, _) = sample.rates.expect("gyro rates");
        assert!((rates[0] - 1638.0 / 32768.0 * 2000.0).abs() < 1e-12);
        link.push_incoming(&angle_packet(0, 8192, 0));
        let next = samples.wait_newer(sample.sequence, Duration::from_secs(10)).expect("second reading");
        assert!(next.sequence > sample.sequence);
        assert_eq!(next.angles, [0.0, 45.0, 0.0]);
        assert_eq!(samples.wait_newer(next.sequence, Duration::from_millis(20)), None);
        drop(imu);
        assert!(samples.closed());
        assert_eq!(samples.wait_newer(next.sequence, Duration::from_secs(10)), None);
    }

    /// Connection scan: 9600 pass reprograms, 115200 pass finds a header and configures the sensor.
    #[test]
    fn scan_reprograms_then_connects() {
        let link = FakeLink::new();
        let opener = FakeOpener::with("/dev/ttyUSB9", &link);
        let ports = FakePorts(vec![PortEntry::usb("/dev/ttyUSB9", 0x1a86, 0x7523, None, None)]);
        let clock: Arc<dyn Clock> = Arc::new(ManualClock::default());
        let claimed = ClaimedPorts::default();
        let allowed = AllowedPorts::default();
        // The 9600 pass writes three commands; the third write releases the sensor data for the 115200 pass.
        link.queue_reply(&[]);
        link.queue_reply(&[]);
        link.queue_reply(&angle_packet(100, 200, 300));
        let installation: InstallationReader = Arc::new(|| "V".to_owned());
        let ctx = ImuScanContext {
            ports: &ports,
            opener: &opener,
            clock,
            claimed: &claimed,
            cached_port: None,
            com_port: "",
            allowed: &allowed,
            installation: &installation,
        };
        let found = scan(&ctx).unwrap().expect("imu");
        assert_eq!(found.port, "/dev/ttyUSB9");
        let bauds: Vec<u32> = opener.opened().iter().map(|(_, s)| s.baud_rate).collect();
        assert_eq!(bauds, vec![9600, 115_200]);
        let written = link.written();
        let mut expected = [command::UNLOCK, command::BAUD_115200, command::SAVE].concat();
        expected.extend(
            [command::UNLOCK, command::RATE_200HZ, command::BANDWIDTH_42HZ, command::VERTICAL, command::SAVE].concat(),
        );
        assert_eq!(written, expected);
    }

    /// Sensor configuration: `'H'` is horizontal, any other code vertical; the code is read after the
    /// bandwidth command, so a change made before then is what the sensor gets.
    #[test]
    fn native_configuration_preserves_command_prefix_and_required_error() {
        let clock = ManualClock::default();
        let start = clock.now();
        let link = FakeLink::new();
        let error = NativeCompensationError { class: "KeyError", message: "'installation'".into() };
        let failure = error.clone();
        let reader: NativeInstallationReader = Arc::new(move || Err(failure.clone()));
        let result = set_config_native(&mut link.clone(), &reader, &clock);
        assert!(matches!(result, Err(ConfigurationError::Native(ref got)) if got == &error));
        assert_eq!(link.written(), [command::UNLOCK, command::RATE_200HZ, command::BANDWIDTH_42HZ].concat());
        assert_eq!(clock.now().duration_since(start), Duration::from_millis(600));
        assert!(matches!(configured(Err(ConfigurationError::Native(error))), Err(ImuScanError::Native(_))));
        assert!(!configured(Err(ConfigurationError::Io(std::io::Error::other("serial failure")))).unwrap());
    }

    #[test]
    fn native_configuration_compares_raw_kind_and_io_precedes_lookup() {
        let clock = ManualClock::default();
        for (value, expected) in [
            (LegacyValue::from("H"), command::HORIZONTAL),
            (LegacyValue::from("V"), command::VERTICAL),
            (LegacyValue::Bytes(b"H".to_vec()), command::VERTICAL),
            (LegacyValue::None, command::VERTICAL),
            (LegacyValue::Int(0), command::VERTICAL),
        ] {
            let link = FakeLink::new();
            let reader: NativeInstallationReader = Arc::new(move || Ok(value.clone()));
            set_config_native(&mut link.clone(), &reader, &clock).unwrap();
            assert_eq!(&link.written()[15..20], &expected);
            assert_eq!(&link.written()[20..], &command::SAVE);
        }
        let link = FakeLink::new();
        link.0.lock().unwrap().writes_before_failure = Some(2);
        let reader: NativeInstallationReader = Arc::new(|| panic!("earlier serial failure skips installation lookup"));
        assert!(matches!(set_config_native(&mut link.clone(), &reader, &clock), Err(ConfigurationError::Io(_))));
        assert_eq!(link.written(), [command::UNLOCK, command::RATE_200HZ].concat());
    }

    #[test]
    fn set_config_reads_the_installation_itself() {
        let clock = ManualClock::default();
        for (code, orientation) in [("H", command::HORIZONTAL), ("V", command::VERTICAL), ("h", command::VERTICAL)] {
            let link = FakeLink::new();
            let mut writer: Box<dyn SerialLink> = Box::new(link.clone());
            let read = || code.to_owned();
            set_config(writer.as_mut(), &read, &clock).unwrap();
            assert_eq!(&link.written()[15..20], &orientation[..]);
        }
        let link = FakeLink::new();
        let mut writer: Box<dyn SerialLink> = Box::new(link.clone());
        let written_before = std::cell::Cell::new(0);
        let probe = link.clone();
        let read = || {
            written_before.set(probe.written().len());
            "H".to_owned()
        };
        set_config(writer.as_mut(), &read, &clock).unwrap();
        assert_eq!(written_before.get(), 15, "read after unlock, rate and bandwidth");
    }
}
