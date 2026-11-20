//! Warplanes: WW1 Fighters / Battles over the Pacific: the 24-byte map `Local\ww1` the game writes once its
//! `smc.cfg` names it (listener key `warplanes`).

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::block_reader::{BlockOpener, BridgeMirrors, Fields, KeptMapping, MappingSpec};
use super::listener_channels::begin_tick;
use crate::udp_flight::python::fold_roll;
use crate::udp_flight::{AngularRates, FlightSource};
use crate::{Source, SourceError};

const MAP: MappingSpec = MappingSpec { name: "Local\\ww1", size: 24 };

/// The game's structure: six byte-packed 32-bit floats in this order.
#[derive(Debug, Clone, Copy, PartialEq)]
struct PlaneFrame {
    pitch: f64,
    yaw: f64,
    vertical_acceleration: f64,
    roll: f64,
    lateral_acceleration: f64,
    longitudinal_acceleration: f64,
}

impl PlaneFrame {
    fn parse(bytes: &[u8]) -> Self {
        let fields = Fields(bytes);
        PlaneFrame {
            pitch: fields.f32(0),
            yaw: fields.f32(4),
            vertical_acceleration: fields.f32(8),
            roll: fields.f32(12),
            lateral_acceleration: fields.f32(16),
            longitudinal_acceleration: fields.f32(20),
        }
    }

    /// The liveness sum, in a fixed addition order.
    fn sum(&self) -> f64 {
        self.vertical_acceleration
            + self.lateral_acceleration
            + self.longitudinal_acceleration
            + self.pitch
            + self.roll
            + self.yaw
    }
}

/// The Warplanes listener.
pub struct Warplanes<O: BlockOpener = BridgeMirrors> {
    opener: O,
    /// The mapping object: re-made by every liveness test and kept until the next.
    handle: KeptMapping,
    latest: Option<PlaneFrame>,
}

impl Warplanes<BridgeMirrors> {
    pub fn new() -> Self {
        Self::with_opener(BridgeMirrors)
    }
}

impl Default for Warplanes<BridgeMirrors> {
    fn default() -> Self {
        Self::new()
    }
}

impl<O: BlockOpener> Warplanes<O> {
    pub fn with_opener(opener: O) -> Self {
        Warplanes { opener, handle: KeptMapping::new(MAP), latest: None }
    }
}

impl<O: BlockOpener> Source for Warplanes<O> {
    fn id(&self) -> &'static str {
        "warplanes"
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        let f = self.latest.as_ref();
        let value = match name {
            "yaw" => f?.yaw,
            "roll" => fold_roll(-f?.roll),
            "pitch" => fold_roll(-f?.pitch),
            "lateral_acceleration" => f?.lateral_acceleration,
            "vertical_acceleration" => f?.vertical_acceleration,
            "longitudinal_acceleration" => f?.longitudinal_acceleration,
            _ => return Some(LegacyValue::Int(0)),
        };
        Some(LegacyValue::Float(value))
    }

    fn channel_read_raised(&self, name: &str) -> bool {
        self.native_channel(name).is_none()
    }

    /// The liveness test: running while the six values do not sum to exactly 0.
    fn is_running(&mut self) -> Result<bool, SourceError> {
        let Ok(bytes) = self.handle.remake(&mut self.opener) else {
            return Ok(false);
        };
        let frame = PlaneFrame::parse(&bytes);
        self.latest = Some(frame);
        Ok(frame.sum() != 0.0)
    }

    fn receive_native(&mut self, _telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        Ok(true)
    }

    /// Numeric compatibility view: accelerations and yaw as sent,
    /// pitch and roll negated and folded at ±90°, every other channel 0.
    fn poll(&mut self, t: &mut Telemetry) -> Result<bool, SourceError> {
        let Some(frame) = self.latest else { return Ok(true) };
        begin_tick(t);
        t.lateral_acceleration = frame.lateral_acceleration;
        t.vertical_acceleration = frame.vertical_acceleration;
        t.longitudinal_acceleration = frame.longitudinal_acceleration;
        t.yaw = frame.yaw;
        t.roll = fold_roll(-frame.roll);
        t.pitch = fold_roll(-frame.pitch);
        Ok(true)
    }
}

impl<O: BlockOpener> FlightSource for Warplanes<O> {
    /// `yaw_rate` is not answered (0), so as a flying game its motion yaw reads 0.
    fn angular_rates(&self) -> AngularRates {
        AngularRates::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared_memory::block_reader::fake::FakeBlocks;

    fn source(values: [f32; 6]) -> Warplanes<FakeBlocks> {
        let blocks = FakeBlocks::default();
        blocks.set(MAP, values.iter().flat_map(|v| v.to_le_bytes()).collect());
        Warplanes::with_opener(blocks)
    }

    #[test]
    fn native_measurements_and_missing_frame_reads() {
        use LegacyValue::{Float, Int};
        let mut s = source([-100.0, 45.0, 1.5, 30.0, -0.0, 0.25]);
        assert!(s.poll(&mut Telemetry::off()).unwrap());
        assert!(s.channel_read_raised("yaw"));
        assert_eq!(s.native_channel("unknown"), Some(Int(0)));
        assert!(s.is_running().unwrap());
        for (name, expected) in [
            ("pitch", Float(80.0)),
            ("yaw", Float(45.0)),
            ("roll", Float(-30.0)),
            ("vertical_acceleration", Float(1.5)),
            ("longitudinal_acceleration", Float(0.25)),
            ("rpm", Int(0)),
            ("yaw_rate", Int(0)),
        ] {
            assert_eq!(s.native_channel(name), Some(expected), "{name}");
        }
        assert!(
            matches!(s.native_channel("lateral_acceleration"), Some(Float(v)) if v.to_bits() == (-0.0f64).to_bits())
        );
        s.latest.as_mut().unwrap().roll = f64::NAN;
        assert!(matches!(s.native_channel("roll"), Some(Float(v)) if v.is_nan()));
        assert!(!s.channel_read_raised("roll"));
    }

    /// Liveness: a zero sum (including values that cancel out) means not running.
    #[test]
    fn check_status_zero_sum() {
        assert!(!Warplanes::with_opener(FakeBlocks::default()).is_running().unwrap());
        assert!(!source([1.0, -1.0, 0.0, 0.0, 0.0, 0.0]).is_running().unwrap());
        assert!(source([0.0, 0.0, 0.0, 0.0, 0.0, 0.5]).is_running().unwrap());
    }

    /// Channel field order (pitch, yaw, heave, roll, sway, surge) and the ±90 folds.
    #[test]
    fn get_channel_mapping() {
        let mut s = source([-100.0, 45.0, 1.5, 30.0, -0.5, 0.25]);
        assert!(s.is_running().unwrap());
        let mut t = Telemetry::off();
        assert!(s.poll(&mut t).unwrap());
        assert_eq!(t.pitch, 80.0);
        assert_eq!(t.yaw, 45.0);
        assert_eq!(t.vertical_acceleration, 1.5);
        assert_eq!(t.roll, -30.0);
        assert_eq!(t.lateral_acceleration, -0.5);
        assert_eq!(t.longitudinal_acceleration, 0.25);
        assert_eq!(t.gear, 0.0);
    }
}
