//! TrackMania 2 / TrackMania Turbo: the ManiaPlanet engine's `ManiaPlanet_Telemetry` map (listener key
//! `trackmania`).

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::block_reader::{BlockOpener, BridgeMirrors, Fields, KeptMapping, MappingSpec};
use super::listener_channels::begin_tick;
use super::python_numeric::rad_to_deg;
use crate::udp_flight::python::fold_roll;
use crate::udp_flight::{AngularRates, FlightSource};
use crate::{Source, SourceError};

/// Name and size (the `STelemetry` structure, 4-byte packing) of the map.
const MAP: MappingSpec = MappingSpec { name: "ManiaPlanet_Telemetry", size: 1396 };

/// Byte offsets in `STelemetry`.
const GAME_STATE: usize = 0x2c;
const VELOCITY_Y: usize = 0x478 + 4;
const ENGINE_RPM: usize = 0x4bc;
const ENGINE_GEAR: usize = 0x4c0;
const WHEELS_SLIDING: usize = 0x4dc;
const WHEELS_DAMPER_LEN: usize = 0x4ec;
const SPEED_METER: usize = 0x508;
const EULER: usize = 0x540;

/// `EGameState` value of a race in progress.
const STATE_RUNNING: i32 = 2;

/// The fields the listener reads, in the game's wheel order (the SDK's FL, FR, RR, RL).
#[derive(Debug, Clone, PartialEq)]
struct ManiaFrame {
    game_state: i32,
    vertical_velocity: f64,
    engine_rpm: f64,
    gear: i32,
    wheel_sliding: [i32; 4],
    damper_len: [f64; 4],
    speed_meter: i32,
    euler: [f64; 3],
}

impl ManiaFrame {
    fn parse(bytes: &[u8]) -> Self {
        let fields = Fields(bytes);
        ManiaFrame {
            game_state: fields.i32(GAME_STATE),
            vertical_velocity: fields.f32(VELOCITY_Y),
            engine_rpm: fields.f32(ENGINE_RPM),
            gear: fields.i32(ENGINE_GEAR),
            wheel_sliding: std::array::from_fn(|i| fields.i32(WHEELS_SLIDING + 4 * i)),
            damper_len: std::array::from_fn(|i| fields.f32(WHEELS_DAMPER_LEN + 4 * i)),
            speed_meter: fields.i32(SPEED_METER),
            euler: std::array::from_fn(|i| fields.f32(EULER + 4 * i)),
        }
    }
}

/// `deg(a)` reduced to one turn the way the listener does it: `(d/360 - round(d/360)) * 360`, then folded into
/// ±180. `None` where the rounding fails (NaN, infinity): the read fails, and the dispatcher
/// stores 0 for a failed yaw / pitch / roll read.
fn wrapped_degrees(radians: f64) -> Option<f64> {
    let turns = rad_to_deg(radians) / 360.0;
    if !turns.is_finite() {
        return None;
    }
    let rounded = turns.round_ties_even();
    // The rounded value is an integer: even a rounded -0.0 becomes positive zero.
    let rounded = if rounded == 0.0 { 0.0 } else { rounded };
    let mut angle = (turns - rounded) * 360.0;
    if angle > 180.0 {
        angle -= 360.0;
    } else if angle < -180.0 {
        angle += 360.0;
    }
    Some(angle)
}

/// The TrackMania listener.
pub struct TrackMania<O: BlockOpener = BridgeMirrors> {
    opener: O,
    /// The mapping object: re-made by every liveness test and kept until the next.
    handle: KeptMapping,
    latest: Option<ManiaFrame>,
}

impl TrackMania<BridgeMirrors> {
    pub fn new() -> Self {
        Self::with_opener(BridgeMirrors)
    }
}

impl Default for TrackMania<BridgeMirrors> {
    fn default() -> Self {
        Self::new()
    }
}

impl<O: BlockOpener> TrackMania<O> {
    pub fn with_opener(opener: O) -> Self {
        TrackMania { opener, handle: KeptMapping::new(MAP), latest: None }
    }

    fn check_with(&mut self, print: impl FnOnce(String)) -> bool {
        let bytes = match self.handle.remake(&mut self.opener) {
            Ok(bytes) => bytes,
            Err(error) => {
                // A failure is printed after "error".
                print(format!("error {}", crate::printed::os_error(&error)));
                return false;
            }
        };
        let frame = ManiaFrame::parse(&bytes);
        let running = frame.game_state == STATE_RUNNING;
        self.latest = Some(frame);
        running
    }

    /// Writes every channel the listener answers; all others read 0.
    fn write_channels(frame: &ManiaFrame, t: &mut Telemetry) {
        begin_tick(t);
        t.rpm = frame.engine_rpm;
        t.max_rpm = 10000.0;
        t.gear = f64::from(frame.gear);
        // The game's own speedometer value, km/h, passed through unconverted.
        t.speed = f64::from(frame.speed_meter);
        t.yaw = wrapped_degrees(frame.euler[0]).unwrap_or(0.0);
        t.roll = wrapped_degrees(frame.euler[2]).map_or(0.0, |roll| fold_roll(-roll));
        t.pitch = wrapped_degrees(frame.euler[1]).map_or(0.0, fold_roll);
        t.vertical_velocity = frame.vertical_velocity;
        // Left/right swapped against the SDK order: lf <- 1, rf <- 0, lr <- 3, rr <- 2.
        let slip = |i: usize| if frame.wheel_sliding[i] == 1 { 4.0 } else { 0.0 };
        t.wheel_slip_lf = slip(1);
        t.wheel_slip_rf = slip(0);
        t.wheel_slip_lr = slip(3);
        t.wheel_slip_rr = slip(2);
        t.suspension_travel_lf = frame.damper_len[1] / 10.0;
        t.suspension_travel_rf = frame.damper_len[0] / 10.0;
        t.suspension_travel_lr = frame.damper_len[3] / 10.0;
        t.suspension_travel_rr = frame.damper_len[2] / 10.0;
    }
}

impl<O: BlockOpener> Source for TrackMania<O> {
    fn id(&self) -> &'static str {
        "trackmania"
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        use LegacyValue::{Float, Int};
        let f = self.latest.as_ref();
        let value = match name {
            "rpm" => f?.engine_rpm,
            "max_rpm" => return Some(Int(10000)),
            "gear" => return Some(Int(i64::from(f?.gear))),
            "speed" => return Some(Int(i64::from(f?.speed_meter))),
            "yaw" => wrapped_degrees(f?.euler[0])?,
            "pitch" => fold_roll(wrapped_degrees(f?.euler[1])?),
            "roll" => fold_roll(-wrapped_degrees(f?.euler[2])?),
            "vertical_velocity" => f?.vertical_velocity,
            "wheel_slip_lf" | "wheel_slip_rf" | "wheel_slip_lr" | "wheel_slip_rr" => {
                let i = match name {
                    "wheel_slip_lf" => 1,
                    "wheel_slip_rf" => 0,
                    "wheel_slip_lr" => 3,
                    _ => 2,
                };
                return Some(Int(if f?.wheel_sliding[i] == 1 { 4 } else { 0 }));
            }
            "suspension_travel_lf" | "suspension_travel_rf" | "suspension_travel_lr" | "suspension_travel_rr" => {
                let i = match name {
                    "suspension_travel_lf" => 1,
                    "suspension_travel_rf" => 0,
                    "suspension_travel_lr" => 3,
                    _ => 2,
                };
                f?.damper_len[i] / 10.0
            }
            _ => return Some(Int(0)),
        };
        Some(Float(value))
    }

    fn channel_read_raised(&self, name: &str) -> bool {
        self.native_channel(name).is_none()
    }

    /// The liveness test: a fresh snapshot of the map; running while `egame_state == 2`.
    fn is_running(&mut self) -> Result<bool, SourceError> {
        Ok(self.check_with(crate::printed::line))
    }

    fn receive_native(&mut self, _telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        Ok(true)
    }

    /// Numeric compatibility view of the last snapshot.
    fn poll(&mut self, telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        if let Some(frame) = &self.latest {
            Self::write_channels(frame, telemetry);
        }
        Ok(true)
    }
}

impl<O: BlockOpener> FlightSource for TrackMania<O> {
    /// The rate names read 0.
    fn angular_rates(&self) -> AngularRates {
        AngularRates::default()
    }
}

#[cfg(test)]
mod tests {
    use super::super::block_reader::BlockReader;
    use super::*;
    use crate::shared_memory::block_reader::fake::FakeBlocks;
    use std::collections::VecDeque;
    use std::io;
    use std::sync::{Arc, Mutex};

    fn map_bytes(build: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
        let mut bytes = vec![0u8; MAP.size];
        build(&mut bytes);
        bytes
    }

    fn put_i32(bytes: &mut [u8], offset: usize, value: i32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn put_f32(bytes: &mut [u8], offset: usize, value: f32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn source_with(bytes: Vec<u8>) -> TrackMania<FakeBlocks> {
        let blocks = FakeBlocks::default();
        blocks.set(MAP, bytes);
        TrackMania::with_opener(blocks)
    }

    enum Step {
        OpenError,
        ReadError,
        Data(Vec<u8>),
    }
    struct Script {
        steps: VecDeque<Step>,
        calls: Arc<Mutex<Vec<&'static str>>>,
    }
    struct Reader {
        step: Step,
        calls: Arc<Mutex<Vec<&'static str>>>,
    }
    impl BlockOpener for Script {
        fn open(&mut self, _: MappingSpec) -> io::Result<Box<dyn BlockReader>> {
            panic!("requires create-or-open")
        }
        fn create_held(&mut self, spec: MappingSpec) -> io::Result<Box<dyn BlockReader>> {
            assert_eq!(spec, MAP);
            self.calls.lock().unwrap().push("open");
            let step = self.steps.pop_front().unwrap();
            if matches!(step, Step::OpenError) {
                return Err(io::Error::other("open denied"));
            }
            Ok(Box::new(Reader { step, calls: self.calls.clone() }))
        }
    }
    impl BlockReader for Reader {
        fn read(&mut self) -> io::Result<&[u8]> {
            self.calls.lock().unwrap().push("read");
            match &self.step {
                Step::Data(bytes) => Ok(bytes),
                Step::ReadError => Err(io::Error::other("read denied")),
                Step::OpenError => unreachable!(),
            }
        }
    }
    impl Drop for Reader {
        fn drop(&mut self) {
            self.calls.lock().unwrap().push("close");
        }
    }

    #[test]
    fn native_failed_mapping_preserves_the_previous_telemetry() {
        let first = map_bytes(|b| {
            put_i32(b, GAME_STATE, 2);
            put_f32(b, ENGINE_RPM, 6500.5);
        });
        let next = map_bytes(|b| {
            put_i32(b, GAME_STATE, 1);
            put_f32(b, ENGINE_RPM, 3000.0);
        });
        let calls = Arc::new(Mutex::new(Vec::new()));
        let opener = Script {
            steps: vec![Step::Data(first), Step::OpenError, Step::ReadError, Step::Data(next)].into(),
            calls: calls.clone(),
        };
        let mut source = TrackMania::with_opener(opener);
        let mut printed = Vec::new();
        assert!(source.check_with(|line| printed.push(line)));
        for _ in 0..2 {
            assert!(!source.check_with(|line| printed.push(line)));
            assert_eq!(source.native_channel("rpm"), Some(LegacyValue::Float(6500.5)));
        }
        assert!(!source.check_with(|line| printed.push(line)), "menu-state successful copy still replaces smm");
        assert_eq!(source.native_channel("rpm"), Some(LegacyValue::Float(3000.0)));
        assert_eq!(printed, ["error open denied", "error read denied"]);
        assert_eq!(*calls.lock().unwrap(), ["open", "read", "open", "open", "close", "read", "open", "close", "read"]);
    }

    #[test]
    fn native_snapshot_failure_reaches_adapter_without_erasing_prior_getters() {
        use crate::derive::{Listener, SourceListener};
        let bytes = map_bytes(|b| {
            put_i32(b, GAME_STATE, 2);
            put_i32(b, SPEED_METER, 187);
        });
        let calls = Arc::new(Mutex::new(Vec::new()));
        let source = TrackMania::with_opener(Script {
            steps: vec![Step::OpenError, Step::Data(bytes), Step::ReadError].into(),
            calls,
        });
        let mut listener = SourceListener::new(source);
        assert!(!listener.is_running());
        assert!(listener.native_channel("speed").is_none());
        assert_eq!(listener.native_channel("max_rpm"), Some(LegacyValue::Int(10000)));
        assert!(listener.is_running());
        listener.receive();
        assert_eq!(listener.native_channel("speed"), Some(LegacyValue::Int(187)));
        assert!(!listener.is_running());
        listener.receive();
        assert_eq!(listener.native_channel("speed"), Some(LegacyValue::Int(187)));
        assert!(!listener.check_status_raised());
    }

    #[test]
    fn receive_before_snapshot_is_unconditionally_true() {
        let mut source = TrackMania::with_opener(FakeBlocks::default());
        assert!(source.poll(&mut Telemetry::off()).unwrap());
    }

    #[test]
    fn native_angles_keep_negative_zero_through_integer_rounding() {
        let mut source = TrackMania::with_opener(FakeBlocks::default());
        source.opener.set(
            MAP,
            map_bytes(|bytes| {
                put_i32(bytes, GAME_STATE, STATE_RUNNING);
                for i in 0..3 {
                    put_f32(bytes, EULER + 4 * i, -0.0);
                }
            }),
        );
        assert!(source.is_running().unwrap());
        for (name, expected) in [("yaw", -0.0_f64), ("pitch", -0.0), ("roll", 0.0)] {
            let Some(LegacyValue::Float(value)) = source.native_channel(name) else { panic!("float angle expected") };
            assert_eq!(value.to_bits(), expected.to_bits(), "{name}");
        }
    }

    #[test]
    fn native_types_and_raised_angle_reads() {
        use LegacyValue::{Float, Int};
        let mut source = TrackMania::with_opener(FakeBlocks::default());
        assert_eq!(source.native_channel("max_rpm"), Some(Int(10000)));
        assert_eq!(source.native_channel("unknown"), Some(Int(0)));
        assert!(source.channel_read_raised("rpm"));
        assert!(source.channel_read_raised("yaw"));
        source.opener.set(
            MAP,
            map_bytes(|bytes| {
                put_i32(bytes, GAME_STATE, STATE_RUNNING);
                put_f32(bytes, ENGINE_RPM, 6500.5);
                put_i32(bytes, ENGINE_GEAR, -1);
                put_i32(bytes, SPEED_METER, 187);
                put_i32(bytes, WHEELS_SLIDING + 4, 1);
                put_f32(bytes, VELOCITY_Y, -0.0);
            }),
        );
        assert!(source.is_running().unwrap());
        for (name, expected) in [
            ("rpm", Float(6500.5)),
            ("gear", Int(-1)),
            ("speed", Int(187)),
            ("wheel_slip_lf", Int(4)),
            ("wheel_slip_rf", Int(0)),
            ("suspension_travel_lf", Float(0.0)),
            ("yaw", Float(0.0)),
            ("car_name", Int(0)),
        ] {
            assert_eq!(source.native_channel(name), Some(expected), "{name}");
        }
        assert!(
            matches!(source.native_channel("vertical_velocity"), Some(Float(v)) if v.to_bits() == (-0.0f64).to_bits())
        );
        for (name, i) in [("yaw", 0), ("pitch", 1), ("roll", 2)] {
            source.latest.as_mut().unwrap().euler[i] = f64::INFINITY;
            assert!(source.channel_read_raised(name));
        }
    }

    /// Liveness: only EGameState 2 (Running) counts; a missing map is a zero map.
    #[test]
    fn check_status_requires_running_state() {
        let mut absent = TrackMania::with_opener(FakeBlocks::default());
        assert!(!absent.is_running().unwrap());
        let mut menus = source_with(map_bytes(|b| put_i32(b, GAME_STATE, 1)));
        assert!(!menus.is_running().unwrap());
        let mut running = source_with(map_bytes(|b| put_i32(b, GAME_STATE, 2)));
        assert!(running.is_running().unwrap());
    }

    /// Channels: field mapping, wheel swap, /10 dampers, 10000 max rpm, other channels 0.
    #[test]
    fn get_channel_mapping() {
        let bytes = map_bytes(|b| {
            put_i32(b, GAME_STATE, 2);
            put_f32(b, VELOCITY_Y, -1.5);
            put_f32(b, ENGINE_RPM, 6500.0);
            put_i32(b, ENGINE_GEAR, 3);
            put_i32(b, WHEELS_SLIDING, 1);
            put_i32(b, WHEELS_SLIDING + 12, 1);
            for (i, len) in [0.1f32, 0.2, 0.3, 0.4].into_iter().enumerate() {
                put_f32(b, WHEELS_DAMPER_LEN + 4 * i, len);
            }
            put_i32(b, SPEED_METER, 187);
        });
        let mut source = source_with(bytes);
        assert!(source.is_running().unwrap());
        let mut t = Telemetry::off();
        t.lap = 9.0;
        assert!(source.poll(&mut t).unwrap());
        assert_eq!((t.rpm, t.max_rpm, t.gear, t.speed), (6500.0, 10000.0, 3.0, 187.0));
        assert_eq!(t.vertical_velocity, -1.5);
        assert_eq!((t.wheel_slip_lf, t.wheel_slip_rf, t.wheel_slip_lr, t.wheel_slip_rr), (0.0, 4.0, 4.0, 0.0));
        assert_eq!(t.suspension_travel_lf, f64::from(0.2f32) / 10.0);
        assert_eq!(t.suspension_travel_rf, f64::from(0.1f32) / 10.0);
        assert_eq!(t.suspension_travel_lr, f64::from(0.4f32) / 10.0);
        assert_eq!(t.suspension_travel_rr, f64::from(0.3f32) / 10.0);
        assert_eq!(t.lap, 0.0);
    }

    /// yaw/roll/pitch: one-turn wrap, ±180 fold, roll negated, ±90 fold; a failed
    /// rounding (NaN, infinity) reads 0 through the dispatcher's handling of failed yaw/pitch/roll reads.
    #[test]
    fn attitude_wrap_and_folds() {
        let deg = |d: f64| d.to_radians();
        assert!((wrapped_degrees(deg(370.0)).unwrap() - 10.0).abs() < 1e-9);
        assert!((wrapped_degrees(deg(190.0)).unwrap() + 170.0).abs() < 1e-9);
        // 180° is exactly half a turn: round(0.5) is 0 (ties to even), so it stays 180.
        assert_eq!(wrapped_degrees(std::f64::consts::PI), Some(180.0));
        assert_eq!(wrapped_degrees(f64::NAN), None);
        let mut frame = ManiaFrame::parse(&vec![0; MAP.size]);
        frame.euler = [deg(30.0), deg(120.0), deg(-100.0)];
        let mut t = Telemetry::off();
        TrackMania::<FakeBlocks>::write_channels(&frame, &mut t);
        assert!((t.yaw - 30.0).abs() < 1e-4);
        // roll: -(-100) = 100 > 90 -> 180 - 100 = 80; pitch: 120 > 90 -> 60.
        assert!((t.roll - 80.0).abs() < 1e-4);
        assert!((t.pitch - 60.0).abs() < 1e-4);
        frame.euler = [f64::NAN, f64::INFINITY, f64::NAN];
        (t.yaw, t.roll, t.pitch) = (5.0, 6.0, 7.0);
        TrackMania::<FakeBlocks>::write_channels(&frame, &mut t);
        assert_eq!((t.yaw, t.roll, t.pitch), (0.0, 0.0, 0.0));
    }
}
