//! PTA Motion System over UDP: the connect, the pose of a frame and the
//! cyclic move command with its encoding.

use std::net::{SocketAddr, ToSocketAddrs};

use rand::Rng;

use crate::motion::native::{self, NativeSetting};
use crate::motion::pt_actuator::MotionWarning;
use crate::motion::thanos::protocol::parse_python_int;
use crate::motion::{
    python_print, DatagramSocket, DriverFactory, LinkStatus, LiveSettings, MotionContext, MotionDriver, MotionError,
    PyFrame,
};
use openrigcue_model::native_value::LegacyValue;

/// The UDP sockets of the clients: one per destination,
/// replaced by every connect.
const CLIENT_SOCKETS: DatagramSocket = DatagramSocket("ptamotion-clients");
/// The file name in the frames of this driver's errors.
const PTAMOTION_FILE: &str = "backend/controllers/ptamotion.py";

/// Frame header bytes `0x55 0xAA 0xBB`.
const HEADER: [u8; 3] = [0x55, 0xAA, 0xBB];
/// The cyclic move command, the only one the driver sends.
pub const CYCLE_MOVE: u8 = 6;
/// The motor slots are rescaled from `0..65536`.
const INPUT_SPAN: f64 = 65_536.0;
/// The limits: ±11° for pitch, roll and yaw; ±0.075 for sway, surge and heave.
const ROTATION_LIMIT_DEG: f64 = 11.0;
const TRANSLATION_LIMIT: f64 = 0.075;
/// Decimal places the pose values are rounded to.
const DECIMALS: i32 = 6;
/// Range of the per-datagram XOR key (random, 1 to 100 inclusive).
const KEY_MIN: u8 = 1;
const KEY_MAX: u8 = 100;

/// One command payload: translations, rotations (radians), accelerations, velocities.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Pose {
    pub tx: f64,
    pub ty: f64,
    pub tz: f64,
    pub rx: f64,
    pub ry: f64,
    pub rz: f64,
    pub tax: f64,
    pub tay: f64,
    pub taz: f64,
    pub rvx: f64,
    pub rvy: f64,
    pub rvz: f64,
}

/// The 56-byte plain command: header, command byte, twelve
/// little-endian 32-bit floats and `time_update` as a little-endian 32-bit integer (wraps modulo 2³²).
pub fn command_payload(cmd: u8, pose: &Pose, time_update: i64) -> Vec<u8> {
    let mut out = HEADER.to_vec();
    out.push(cmd);
    let values = [
        pose.tx, pose.ty, pose.tz, pose.rx, pose.ry, pose.rz, pose.tax, pose.tay, pose.taz, pose.rvx, pose.rvy,
        pose.rvz,
    ];
    for v in values {
        out.extend_from_slice(&(v as f32).to_le_bytes());
    }
    out.extend_from_slice(&(time_update as i32).to_le_bytes());
    out
}

/// The encoding of a datagram: append a zero, shift every byte from index 1 one place right XORed
/// with `key`, store `key ^ 0x55` at index 1. Byte 0 stays as is.
pub fn encode(plain: &[u8], key: u8) -> Vec<u8> {
    let mut buf = plain.to_vec();
    buf.push(0);
    for i in (2..buf.len()).rev() {
        buf[i] = buf[i - 1] ^ key;
    }
    if buf.len() > 1 {
        buf[1] = key ^ 0x55;
    }
    buf
}

/// `x` rounded to six decimal places.
fn round6(x: f64) -> f64 {
    format!("{:.*}", DECIMALS as usize, x).parse().unwrap_or(x)
}

/// `limit` minus the slot rescaled from `0..65536` to `0..2 * limit`; the rescale takes the slot as a 64-bit float.
fn axis_offset(slot: &LegacyValue, limit: f64) -> Result<f64, MotionError> {
    let x =
        native::map([slot, &native::int(0), &native::float(INPUT_SPAN), &native::int(0), &native::float(limit * 2.0)])?;
    Ok(limit - x)
}

/// The pose of a frame: slots 0..=5 are pitch, roll, yaw, sway, surge, heave;
/// rotations become radians (`d / 180 * pi`), everything rounded to 6 decimals.
pub fn pose_from_slots(motors: &[i64]) -> Result<Pose, MotionError> {
    pose_from_native(&native::list_of(motors))
}

/// [`pose_from_slots`] on the caller's list: each axis reads its slot in slot order (pitch, roll, yaw,
/// sway, surge, heave).
pub fn pose_from_native(motors: &LegacyValue) -> Result<Pose, MotionError> {
    // Each axis is its limit minus its rescaled slot (pitch, roll, yaw
    // are rounded in a second step; sway, surge and heave in one step each).
    const LINES: [u32; 6] = [168, 171, 174, 177, 178, 179];
    let mut axes = [0.0; 6];
    for ((index, axis), line) in axes.iter_mut().enumerate().zip(LINES) {
        let frame = PyFrame::new(PTAMOTION_FILE, "send_ptamotion", line);
        let slot = native::item(motors, index).map_err(|e| e.in_frame(frame))?;
        let limit = if index < 3 { ROTATION_LIMIT_DEG } else { TRANSLATION_LIMIT };
        let offset = axis_offset(&slot, limit).map_err(|e| e.in_frame(frame))?;
        *axis = if index < 3 { round6(offset / 180.0 * std::f64::consts::PI) } else { round6(offset) };
    }
    let [rx, ry, rz, tx, ty, tz] = axes;
    Ok(Pose { tx, ty, tz, rx, ry, rz, ..Pose::default() })
}

/// A destination of the `PT-ACTUATOR.udp_config` setting (an address and a port); the port stays text
/// until each send converts it to an integer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    pub ip: String,
    pub port: String,
}

impl Destination {
    /// The first and second `':'`-separated parts of an entry; `None` without a `':'`.
    pub fn parse(item: &str) -> Option<Self> {
        let mut parts = item.split(':');
        let ip = parts.next()?.to_string();
        let port = parts.next()?.to_string();
        Some(Destination { ip, port })
    }

    fn socket_addr(&self) -> Result<SocketAddr, String> {
        let port = parse_python_int(&self.port).ok_or_else(|| {
            format!("invalid literal for int() with base 10: {}", crate::motion::python::str_repr(&self.port))
        })?;
        let port = u16::try_from(port).map_err(|_| "getsockaddrarg: port must be 0-65535.".to_string())?;
        (self.ip.as_str(), port)
            .to_socket_addrs()
            .map_err(|e| e.to_string())?
            .next()
            .ok_or_else(|| format!("no address for {}", self.ip))
    }
}

/// What the driver reads (`PT-ACTUATOR.udp_config`, `MOTIONBOX.refresh_rate`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PtaMotionSettings {
    /// Where the settings are read live (the refreshed raw values are the fallback).
    pub runtime: crate::motion::driver_support::RuntimeReader,
    pub udp_config: Vec<String>,
    /// `MOTIONBOX.refresh_rate` as an integer, sent as `time_update`.
    pub refresh_rate: i64,
    /// The raw `MOTIONBOX.refresh_rate`, converted to an integer by each cyclic move;
    /// `None` uses [`refresh_rate`](Self::refresh_rate).
    pub native_refresh_rate: Option<NativeSetting>,
}

/// The PTA Motion System driver.
pub struct PtaMotionDriver {
    settings: LiveSettings<PtaMotionSettings>,
    warning: MotionWarning,
    /// The clients, one per destination.
    destinations: Vec<Destination>,
    key: Box<dyn FnMut() -> u8 + Send>,
}

impl PtaMotionDriver {
    pub fn new(settings: LiveSettings<PtaMotionSettings>, warning: MotionWarning) -> Self {
        PtaMotionDriver {
            settings,
            warning,
            destinations: Vec::new(),
            key: Box::new(|| rand::thread_rng().gen_range(KEY_MIN..=KEY_MAX)),
        }
    }

    pub fn factory(settings: LiveSettings<PtaMotionSettings>, warning: MotionWarning) -> DriverFactory {
        Box::new(move || Box::new(PtaMotionDriver::new(settings.clone(), warning.clone())))
    }

    /// Replace the XOR key source (tests).
    pub fn with_key(mut self, key: impl FnMut() -> u8 + Send + 'static) -> Self {
        self.key = Box::new(key);
        self
    }

    /// Sends a pose: one cyclic move datagram to every destination. Send errors are
    /// printed and do not change the connection state.
    fn send_pose(&mut self, motors: &LegacyValue, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        let pose = pose_from_native(motors)?;
        let settings = self.settings.get();
        for (index, destination) in self.destinations.iter().enumerate() {
            // `time_update` is the refresh rate as an integer, packed as its low 32 bits.
            let time_update = match &settings.native_refresh_rate {
                Some(raw) => native::c_int(&native::to_int(
                    &settings.runtime.value("MOTIONBOX", "refresh_rate", raw).map_err(MotionError::Native)?,
                )?)?,
                None => settings.refresh_rate as i32,
            };
            let plain = command_payload(CYCLE_MOVE, &pose, i64::from(time_update));
            let wire = encode(&plain, (self.key)());
            let sent = destination.socket_addr().and_then(|addr| {
                ctx.host
                    .send_datagram_numbered(CLIENT_SOCKETS, index, addr, &wire)
                    .map_err(|e| crate::motion::python::os_error_str(&e))
            });
            if let Err(e) = sent {
                python_print(&["Error sending udp to pta motion", &e]);
            }
        }
        Ok(())
    }
}

impl MotionDriver for PtaMotionDriver {
    /// Connects: the client list is emptied, one client is appended per `udp_config` entry (an
    /// entry without `':'` stops there, keeping the clients already appended), then `Connected` with port
    /// `'0'` and the motion warning cleared.
    fn connect(&mut self, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        // Emptying the list drops the earlier clients and their sockets.
        self.destinations.clear();
        ctx.host.close_datagram_set(CLIENT_SOCKETS);
        for item in self.settings.get().udp_config {
            // An entry without ':' has no port part.
            let destination = Destination::parse(&item).ok_or_else(|| {
                MotionError::Native("IndexError: list index out of range".into()).in_frame(PyFrame::new(
                    "backend/controllers/ptamotion.py",
                    "connect",
                    155,
                ))
            })?;
            self.destinations.push(destination);
        }
        ctx.link.status = LinkStatus::Connected;
        ctx.link.ports.insert(0, "0".into());
        self.warning.set(String::new());
        Ok(())
    }

    /// Disconnecting has no effect on the clients.
    fn disconnect(&mut self, _ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        Ok(())
    }

    /// A stream frame: a pose only while `Connected`.
    fn send_stream(&mut self, motors: &[i64], ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.send_stream_native(&mut native::list_of(motors), ctx)
    }

    /// A stream frame on the caller's list (which is only read).
    fn send_stream_native(&mut self, motors: &mut LegacyValue, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        if ctx.link.is_connected() {
            // The pose is sent; its error passes this frame.
            self.send_pose(motors, ctx).map_err(|e| e.through(PyFrame::new(PTAMOTION_FILE, "send_stream", 190)))?;
        }
        Ok(())
    }

    fn send_test_native(&mut self, motors: &mut LegacyValue, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        if ctx.link.is_connected() {
            // The pose is sent; its error passes this frame.
            self.send_pose(motors, ctx).map_err(|e| e.through(PyFrame::new(PTAMOTION_FILE, "send_test", 195)))?;
        }
        Ok(())
    }

    /// A test frame: identical to a stream frame.
    fn send_test(&mut self, motors: &[i64], ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.send_test_native(&mut native::list_of(motors), ctx)
    }

    fn describe(&self) -> String {
        format!("PTA Motion System UDP to {} destination(s)", self.destinations.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::motion::testing::FakeHost;
    use crate::motion::{motion_model, LinkState};

    const MODEL: &str = "PTA Motion System UDP";

    fn run(
        driver: &mut PtaMotionDriver,
        host: &mut FakeHost,
        link: &mut LinkState,
        f: impl FnOnce(&mut PtaMotionDriver, &mut MotionContext<'_>),
    ) {
        let mut ctx = MotionContext { model: motion_model(MODEL, true).unwrap(), link, host };
        f(driver, &mut ctx);
    }

    /// The plain command: 55 AA BB, cmd, 12 × float32 LE, int32 LE = 56 bytes.
    #[test]
    fn command_layout() {
        let pose = Pose { tx: 1.0, rz: -2.5, rvz: 0.5, ..Pose::default() };
        let p = command_payload(CYCLE_MOVE, &pose, 400);
        assert_eq!(p.len(), 56);
        assert_eq!(&p[..4], &[0x55, 0xAA, 0xBB, 6]);
        assert_eq!(&p[4..8], &1.0f32.to_le_bytes());
        assert_eq!(&p[24..28], &(-2.5f32).to_le_bytes());
        assert_eq!(&p[48..52], &0.5f32.to_le_bytes());
        assert_eq!(&p[52..56], &[0x90, 0x01, 0, 0]);
    }

    /// The encoding: the exact byte shuffle.
    #[test]
    fn encode_shifts_and_xors() {
        let wire = encode(&[0x55, 0xAA, 0xBB, 0x06], 7);
        assert_eq!(wire, vec![0x55, 7 ^ 0x55, 0xAA ^ 7, 0xBB ^ 7, 0x06 ^ 7]);
    }

    /// The pose: 0 → +limit, 65536 → -limit (rescaled over 0..65536), radians for rotations, six decimals.
    #[test]
    fn pose_scaling() {
        let p = pose_from_slots(&[0, 32768, 65536, 0, 32768, 65536]).unwrap();
        assert_eq!(p.rx, 0.191986);
        assert_eq!(p.ry, 0.0);
        assert_eq!(p.rz, -0.191986);
        assert_eq!(p.tx, 0.075);
        assert_eq!(p.ty, 0.0);
        assert_eq!(p.tz, -0.075);
        assert!(pose_from_slots(&[0; 5]).is_err());
    }

    /// Connect, then stream: one 57-byte datagram per destination, only while Connected.
    #[test]
    fn streams_to_every_destination() {
        let settings = LiveSettings::new(PtaMotionSettings {
            udp_config: vec!["192.168.15.201:7408".into(), "127.0.0.1:9000".into()],
            refresh_rate: 400,
            native_refresh_rate: None,
            runtime: Default::default(),
        });
        let mut driver = PtaMotionDriver::new(settings, MotionWarning::default()).with_key(|| 9);
        let (mut host, mut link) = (FakeHost::default(), LinkState::default());
        let motors = [32768i64; 12];
        run(&mut driver, &mut host, &mut link, |d, ctx| d.send_stream(&motors, ctx).unwrap());
        assert!(host.datagrams.is_empty(), "nothing before connect");
        run(&mut driver, &mut host, &mut link, |d, ctx| d.connect(ctx).unwrap());
        assert!(link.is_connected());
        assert_eq!(link.ports.get(&0).map(String::as_str), Some("0"));
        run(&mut driver, &mut host, &mut link, |d, ctx| d.send_stream(&motors, ctx).unwrap());
        assert_eq!(host.datagrams.len(), 2);
        assert_eq!(host.datagrams[0].0, "192.168.15.201:7408".parse().unwrap());
        let plain = command_payload(CYCLE_MOVE, &pose_from_slots(&motors).unwrap(), 400);
        assert_eq!(host.datagrams[1].1, encode(&plain, 9));
        assert_eq!(host.datagrams[1].1.len(), 57);
        // One socket per client; a new connect creates new clients (new sockets).
        assert_eq!(host.datagram_members, [(0, 1), (1, 1)]);
        run(&mut driver, &mut host, &mut link, |d, ctx| {
            d.connect(ctx).unwrap();
            d.send_stream(&motors, ctx).unwrap();
        });
        assert_eq!(&host.datagram_members[2..], [(0, 2), (1, 2)]);
    }

    /// Connect: an entry without ':' is an error and leaves the state unchanged; a bad port only
    /// fails the send to that destination; success clears the motion warning.
    #[test]
    fn connect_and_send_errors() {
        let settings = LiveSettings::new(PtaMotionSettings {
            udp_config: vec!["10.0.0.1".into()],
            refresh_rate: 30,
            native_refresh_rate: None,
            runtime: Default::default(),
        });
        let warning = MotionWarning::new("old warning".into());
        let mut driver = PtaMotionDriver::new(settings.clone(), warning.clone());
        let (mut host, mut link) = (FakeHost::default(), LinkState::default());
        run(&mut driver, &mut host, &mut link, |d, ctx| {
            let error = d.connect(ctx).unwrap_err();
            assert_eq!(
                error.python_traceback().unwrap(),
                "Traceback (most recent call last):\n  File \"backend/controllers/motion_helpers.py\", line 100, in \
                 backend.controllers.motion_helpers.connect\n  File \"backend/controllers/ptamotion.py\", line 155, in \
                 backend.controllers.ptamotion.connect\nIndexError: list index out of range\n"
            );
        });
        assert!(!link.is_connected());
        assert_eq!(warning.get(), "old warning");
        settings.set(PtaMotionSettings {
            udp_config: vec!["127.0.0.1:x".into(), "127.0.0.1:1".into()],
            refresh_rate: 30,
            native_refresh_rate: None,
            runtime: Default::default(),
        });
        run(&mut driver, &mut host, &mut link, |d, ctx| {
            d.connect(ctx).unwrap();
            d.send_test(&[0; 12], ctx).unwrap();
        });
        assert_eq!(host.datagrams.len(), 1);
        assert!(warning.get().is_empty());
    }

    /// Connect: the client list is emptied first and appended per entry, so a bad entry keeps the
    /// entries before it; a link still Connected streams to that partial list.
    #[test]
    fn connect_failure_keeps_partial_list() {
        let settings = LiveSettings::new(PtaMotionSettings {
            udp_config: vec!["127.0.0.1:1".into(), "127.0.0.1:2".into()],
            refresh_rate: 30,
            native_refresh_rate: None,
            runtime: Default::default(),
        });
        let mut driver = PtaMotionDriver::new(settings.clone(), MotionWarning::default());
        let (mut host, mut link) = (FakeHost::default(), LinkState::default());
        run(&mut driver, &mut host, &mut link, |d, ctx| d.connect(ctx).unwrap());
        settings.set(PtaMotionSettings {
            udp_config: vec!["127.0.0.1:3".into(), "bad".into(), "127.0.0.1:4".into()],
            refresh_rate: 30,
            native_refresh_rate: None,
            runtime: Default::default(),
        });
        run(&mut driver, &mut host, &mut link, |d, ctx| {
            assert!(d.connect(ctx).is_err());
            d.send_stream(&[0; 12], ctx).unwrap();
        });
        assert!(link.is_connected());
        let targets: Vec<SocketAddr> = host.datagrams.iter().map(|(a, _)| *a).collect();
        assert_eq!(targets, vec!["127.0.0.1:3".parse().unwrap()]);
    }

    /// The pose reads its six slots in order and converts each in the rescale; `time_update` is
    /// the refresh rate as an integer, read per destination, packed as its low 32 bits.
    #[test]
    fn native_pose_and_time_update_keep_their_width_until_packing() {
        let wide = LegacyValue::from_integer(num_bigint::BigInt::from(1) << 80);
        let mut values: Vec<LegacyValue> = (0..12).map(|_| native::int(32768)).collect();
        values[3] = wide;
        let pose = pose_from_native(&LegacyValue::List(values)).unwrap();
        assert_eq!(pose.tx, -TRANSLATION_LIMIT, "a wide slot is clamped by map");
        let huge = LegacyValue::from_integer(num_bigint::BigInt::from(1) << 1100);
        let error = pose_from_native(&LegacyValue::List(vec![huge])).unwrap_err();
        assert_eq!(error.exception_line().as_deref(), Some("OverflowError: int too large to convert to float"));
        let frames: Vec<_> = error.frames().iter().map(|f| (f.function, f.line)).collect();
        assert_eq!(frames, [("send_ptamotion", 168), ("map", 204)]);
        let error = pose_from_native(&LegacyValue::List(vec![native::int(0)])).unwrap_err();
        assert_eq!(error.exception_line().as_deref(), Some("IndexError: list index out of range"));
        let rate = LegacyValue::from_integer((num_bigint::BigInt::from(1) << 64) + 30);
        assert_eq!(native::c_int(&rate).unwrap(), 30);
        assert_eq!(native::c_int(&native::int(-1)).unwrap(), -1);
    }
}
