//! SimBin titles: GTR, GTR 2, GT Legends and the Race 07 family (Race Injection, Volvo, WTCC).
//!
//! The games' built-in exporter publishes a byte-packed 180-byte block;
//! the four listeners differ only in the mapping name.

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::block_reader::{BlockOpener, BridgeMirrors, Fields, KeptMapping, MappingSpec};
use super::listener_channels::begin_tick;
use super::python_numeric::{percent_or_zero, rad_to_deg, round_6, round_to_int};
use crate::{Source, SourceError};

/// Size of the telemetry block.
pub const SIMBIN_BLOCK_SIZE: usize = 180;
/// GTR.
pub const GTR_MAPPING: MappingSpec = MappingSpec { name: "SimBin", size: SIMBIN_BLOCK_SIZE };
/// GTR 2.
pub const GTR2_MAPPING: MappingSpec = MappingSpec { name: "$gtr2$", size: SIMBIN_BLOCK_SIZE };
/// GT Legends.
pub const GT_LEGENDS_MAPPING: MappingSpec = MappingSpec { name: "$gtlegends$", size: SIMBIN_BLOCK_SIZE };
/// Race 07, Race Injection, Volvo The Game, Race - WTCC Game.
pub const RACE07_MAPPING: MappingSpec = MappingSpec { name: "$Race$", size: SIMBIN_BLOCK_SIZE };

/// Standard gravity used to turn the block's m/s² into g.
const GRAVITY: f64 = 9.80665;

/// Which SimBin listener this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimBinTitle {
    Gtr,
    Gtr2,
    GtLegends,
    Race07,
}

impl SimBinTitle {
    /// Listener key.
    pub fn listener_id(self) -> &'static str {
        match self {
            SimBinTitle::Gtr => "gtr",
            SimBinTitle::Gtr2 => "gtr2",
            SimBinTitle::GtLegends => "gtl",
            SimBinTitle::Race07 => "race07",
        }
    }

    /// The mapping the title's exporter publishes.
    pub fn mapping(self) -> MappingSpec {
        match self {
            SimBinTitle::Gtr => GTR_MAPPING,
            SimBinTitle::Gtr2 => GTR2_MAPPING,
            SimBinTitle::GtLegends => GT_LEGENDS_MAPPING,
            SimBinTitle::Race07 => RACE07_MAPPING,
        }
    }
}

/// The fields of the telemetry block the listener reads.
#[derive(Debug, Clone, Copy, PartialEq)]
struct SimBinFrame {
    rpm: f64,
    max_engine_rps: f64,
    fuel: f64,
    fuel_capacity_liters: f64,
    engine_water_temp: f64,
    engine_oil_temp: f64,
    car_speed: f64,
    gear: i32,
    pitch: f64,
    yaw: f64,
    roll: f64,
    lateral: f64,
    vertical: f64,
    longitudinal: f64,
}

impl SimBinFrame {
    fn decode(bytes: &[u8]) -> Self {
        let f = Fields(bytes);
        SimBinFrame {
            rpm: f.f32(24),
            max_engine_rps: f.f32(28),
            fuel: f.f32(36),
            fuel_capacity_liters: f.f32(40),
            engine_water_temp: f.f32(44),
            engine_oil_temp: f.f32(48),
            car_speed: f.f32(56),
            gear: f.i32(88),
            pitch: f.f32(156),
            yaw: f.f32(160),
            roll: f.f32(164),
            lateral: f.f32(168),
            vertical: f.f32(172),
            longitudinal: f.f32(176),
        }
    }

    /// The exporter's "player not in the car" pattern, or a block nobody has written yet.
    fn is_idle(&self) -> bool {
        let not_driving = self.car_speed == 0.0 && self.rpm == -1.0 && self.max_engine_rps == -1.0 && self.gear == -2;
        let never_written = self.car_speed == 0.0 && self.rpm == 0.0 && self.max_engine_rps == 0.0 && self.gear == 0;
        not_driving || never_written
    }

    /// Writes the channels (identical for the four titles).
    fn write(&self, t: &mut Telemetry) {
        let previous = begin_tick(t);
        t.rpm = round_to_int(self.rpm).unwrap_or(previous.rpm);
        t.max_rpm = round_to_int(self.max_engine_rps).unwrap_or(previous.max_rpm);
        t.gear = f64::from(self.gear);
        t.speed = if self.car_speed < 0.0 { Some(0.0) } else { round_to_int(self.car_speed) }.unwrap_or(previous.speed);
        // The block has no steering field; the listener answers the lateral acceleration.
        t.steering_wheel_angle = self.lateral;
        t.longitudinal_acceleration = -(round_6(self.longitudinal) / GRAVITY);
        t.lateral_acceleration = round_6(self.lateral) / GRAVITY;
        t.vertical_acceleration = round_6(self.vertical) / GRAVITY;
        t.yaw = rad_to_deg(self.yaw);
        t.roll = rad_to_deg(self.roll);
        t.pitch = rad_to_deg(self.pitch);
        t.oil_temperature = self.engine_oil_temp;
        t.water_temperature = self.engine_water_temp;
        t.fuel_level = percent_or_zero(self.fuel, self.fuel_capacity_liters);
    }
}

/// One SimBin title's listener.
pub struct SimBin {
    title: SimBinTitle,
    opener: Box<dyn BlockOpener>,
    /// The mapping object kept between calls.
    handle: KeptMapping,
    frame: Option<SimBinFrame>,
}

impl SimBin {
    /// Listener reading the bridge mirror of the title's mapping.
    pub fn new(title: SimBinTitle) -> Self {
        Self::with_opener(title, Box::new(BridgeMirrors))
    }

    /// Listener reading mappings through `opener`.
    pub fn with_opener(title: SimBinTitle, opener: Box<dyn BlockOpener>) -> Self {
        SimBin { title, opener, handle: KeptMapping::new(title.mapping()), frame: None }
    }

    /// The liveness test: a new mapping object on every call,
    /// replacing the previous object after creation, then copying it; both the mapping object and the snapshot are
    /// dropped when the block shows the idle pattern. A failed mapping or
    /// copy answers false and changes nothing.
    fn refresh(&mut self) -> bool {
        let Ok(bytes) = self.handle.remake(self.opener.as_mut()) else { return false };
        let frame = SimBinFrame::decode(&bytes);
        if frame.is_idle() {
            self.handle.release();
            self.frame = None;
            return false;
        }
        self.frame = Some(frame);
        true
    }
}

impl Source for SimBin {
    fn id(&self) -> &'static str {
        self.title.listener_id()
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        use LegacyValue::{Float, Int, Str};
        let value = match name {
            "rpm" => return round_to_int(self.frame?.rpm).and_then(crate::udp_racing::int_text).map(Str),
            "max_rpm" => {
                return round_to_int(self.frame?.max_engine_rps).and_then(crate::udp_racing::int_text).map(Str)
            }
            "gear" => return Some(Str(self.frame?.gear.to_string())),
            "speed" => {
                let speed = self.frame?.car_speed;
                return if speed < 0.0 {
                    Some(Int(0))
                } else {
                    round_to_int(speed).and_then(crate::udp_racing::int_text).map(Str)
                };
            }
            "fuel_level" => {
                return Some(
                    self.frame
                        .filter(|f| f.fuel_capacity_liters != 0.0)
                        .map_or(Int(0), |f| Float(percent_or_zero(f.fuel, f.fuel_capacity_liters))),
                );
            }
            "steering_wheel_angle" => self.frame?.lateral,
            "longitudinal_acceleration" => -(round_6(self.frame?.longitudinal) / GRAVITY),
            "lateral_acceleration" => round_6(self.frame?.lateral) / GRAVITY,
            "vertical_acceleration" => round_6(self.frame?.vertical) / GRAVITY,
            "yaw" => rad_to_deg(self.frame?.yaw),
            "roll" => rad_to_deg(self.frame?.roll),
            "pitch" => rad_to_deg(self.frame?.pitch),
            "oil_temperature" => self.frame?.engine_oil_temp,
            "water_temperature" => self.frame?.engine_water_temp,
            _ => return Some(Int(0)),
        };
        Some(Float(value))
    }

    fn channel_read_raised(&self, name: &str) -> bool {
        self.native_channel(name).is_none()
    }

    fn is_running(&mut self) -> Result<bool, SourceError> {
        Ok(self.refresh())
    }

    /// The receive step is a no-op: the mapping was read by the liveness test.
    fn receive_native(&mut self, _telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        Ok(true)
    }

    /// Numeric compatibility view of the current mapping.
    fn poll(&mut self, telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        if let Some(frame) = self.frame {
            frame.write(telemetry);
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared_memory::block_reader::fake::{BlockBuilder, FakeBlocks};

    fn driving_block() -> BlockBuilder {
        let mut b = BlockBuilder::new(SIMBIN_BLOCK_SIZE);
        b.f32(24, 6500.5) // rpm
            .f32(28, 8000.0) // maxEngineRPS
            .f32(36, 30.0) // fuel
            .f32(40, 120.0) // fuelCapacityLiters
            .f32(44, 88.0) // engineWaterTemp
            .f32(48, 101.0) // engineOilTemp
            .f32(56, 42.5) // carSpeed
            .i32(88, 3) // gear
            .f32(156, 0.05) // pitch
            .f32(160, -1.0) // yaw
            .f32(164, 0.1) // roll
            .f32(168, 4.9) // lateral
            .f32(172, 9.80665) // vertical
            .f32(176, -19.6133); // longitudinal
        b
    }

    fn listener(blocks: &FakeBlocks, title: SimBinTitle) -> SimBin {
        SimBin::with_opener(title, Box::new(blocks.clone()))
    }

    #[test]
    fn mapping_names_per_title() {
        assert_eq!(SimBinTitle::Gtr.mapping().name, "SimBin");
        assert_eq!(SimBinTitle::Gtr2.mapping().name, "$gtr2$");
        assert_eq!(SimBinTitle::GtLegends.mapping().name, "$gtlegends$");
        assert_eq!(SimBinTitle::Race07.mapping().name, "$Race$");
        assert_eq!(SimBinTitle::Race07.listener_id(), "race07");
        assert_eq!(RACE07_MAPPING.size, 180);
    }

    #[test]
    fn get_channel_maps_the_block() {
        let blocks = FakeBlocks::default();
        blocks.set(GTR2_MAPPING, driving_block().build());
        let mut l = listener(&blocks, SimBinTitle::Gtr2);
        assert!(l.is_running().unwrap());
        let mut t = Telemetry::off();
        t.tyre_wear_lf = 5.0;
        assert!(l.poll(&mut t).unwrap());
        assert_eq!(t.rpm, 6500.0); // round(6500.5) half to even
        assert_eq!(t.max_rpm, 8000.0);
        assert_eq!(t.gear, 3.0);
        assert_eq!(t.speed, 42.0); // round(42.5) half to even
        assert_eq!(t.steering_wheel_angle, f64::from(4.9f32));
        assert_eq!(t.lateral_acceleration, round_6(f64::from(4.9f32)) / GRAVITY);
        assert_eq!(t.vertical_acceleration, round_6(f64::from(9.80665f32)) / GRAVITY);
        assert_eq!(t.longitudinal_acceleration, -(round_6(f64::from(-19.6133f32)) / GRAVITY));
        assert_eq!(t.yaw, rad_to_deg(-1.0));
        assert_eq!(t.roll, rad_to_deg(f64::from(0.1f32)));
        assert_eq!(t.pitch, rad_to_deg(f64::from(0.05f32)));
        assert_eq!(t.oil_temperature, 101.0);
        assert_eq!(t.water_temperature, 88.0);
        assert_eq!(t.fuel_level, 25.0);
        // Anything else is 0.
        assert_eq!(t.tyre_wear_lf, 0.0);
        assert_eq!(t.lateral_velocity, 0.0);
        for (name, expected) in [
            ("rpm", LegacyValue::Str("6500".into())),
            ("max_rpm", LegacyValue::Str("8000".into())),
            ("gear", LegacyValue::Str("3".into())),
            ("speed", LegacyValue::Str("42".into())),
            ("fuel_level", LegacyValue::Float(25.0)),
            ("oil_temperature", LegacyValue::Float(101.0)),
            ("yaw_rate", LegacyValue::Int(0)),
        ] {
            assert_eq!(l.native_channel(name), Some(expected), "{name}");
        }
    }

    #[test]
    fn get_channel_negative_speed_and_empty_tank() {
        let blocks = FakeBlocks::default();
        let mut b = driving_block();
        b.f32(56, -3.0).f32(40, 0.0);
        blocks.set(GTR_MAPPING, b.build());
        let mut l = listener(&blocks, SimBinTitle::Gtr);
        assert!(l.is_running().unwrap());
        let mut t = Telemetry::off();
        l.poll(&mut t).unwrap();
        assert_eq!(t.speed, 0.0);
        assert_eq!(t.fuel_level, 0.0);
        assert_eq!(l.native_channel("speed"), Some(LegacyValue::Int(0)));
        assert_eq!(l.native_channel("fuel_level"), Some(LegacyValue::Int(0)));
        let mut b = driving_block();
        b.f32(56, -0.0).f32(36, 0.0);
        blocks.set(GTR_MAPPING, b.build());
        assert!(l.is_running().unwrap());
        assert_eq!(l.native_channel("speed"), Some(LegacyValue::Str("0".into())));
        assert_eq!(l.native_channel("fuel_level"), Some(LegacyValue::Float(0.0)));
    }

    #[test]
    fn get_channel_nan_rpm_keeps_previous_value() {
        let blocks = FakeBlocks::default();
        let mut b = driving_block();
        b.f32(24, f32::NAN);
        blocks.set(GTR_MAPPING, b.build());
        let mut l = listener(&blocks, SimBinTitle::Gtr);
        assert!(l.is_running().unwrap());
        let mut t = Telemetry::off();
        t.rpm = 1234.0;
        l.poll(&mut t).unwrap();
        assert_eq!(t.rpm, 1234.0);
        assert_eq!(l.native_channel("rpm"), None);
        assert!(l.channel_read_raised("rpm"));
        assert_eq!(l.native_channel("max_rpm"), Some(LegacyValue::Str("8000".into())));
    }

    #[test]
    fn native_mmap_recreation_preserves_failure_and_replacement_order() {
        use crate::shared_memory::block_reader::fake::{MapAttempt, ScriptedMaps};
        for title in [SimBinTitle::Gtr, SimBinTitle::Gtr2, SimBinTitle::GtLegends, SimBinTitle::Race07] {
            let maps = ScriptedMaps::default();
            maps.push(MapAttempt::Bytes(driving_block().build()));
            let mut source = SimBin::with_opener(title, Box::new(maps.clone()));
            assert!(source.is_running().unwrap());
            assert_eq!(maps.trace(), ["create:1", "read:1"]);
            maps.push(MapAttempt::Bytes(driving_block().build()));
            assert!(source.is_running().unwrap());
            assert_eq!(maps.trace(), ["create:2", "close:1", "read:2"]);
            let previous = source.native_channel("rpm");
            maps.push(MapAttempt::CreateError);
            assert!(!source.is_running().unwrap());
            assert_eq!(maps.trace(), ["create:3"]);
            assert_eq!(source.native_channel("rpm"), previous);
            maps.push(MapAttempt::ReadError);
            assert!(!source.is_running().unwrap());
            assert_eq!(maps.trace(), ["create:4", "close:2", "read:4"]);
            assert_eq!(source.native_channel("rpm"), previous);
            maps.push(MapAttempt::Bytes(vec![0; SIMBIN_BLOCK_SIZE]));
            assert!(!source.is_running().unwrap());
            assert_eq!(maps.trace(), ["create:5", "close:4", "read:5", "close:5"]);
            assert_eq!(source.native_channel("rpm"), None);
            maps.push(MapAttempt::Bytes(driving_block().build()));
            assert!(source.is_running().unwrap());
            assert_eq!(maps.trace(), ["create:6", "read:6"]);
            drop(source);
            assert_eq!(maps.trace(), ["close:6"]);
        }
    }

    #[test]
    fn check_status_rejects_not_driving_and_blank_blocks() {
        let blocks = FakeBlocks::default();
        let mut l = listener(&blocks, SimBinTitle::GtLegends);
        // Mapping absent: it is created zero-filled (never written), so not running, and the object is
        // dropped again.
        assert!(!l.is_running().unwrap());
        assert_eq!(blocks.held_count(GT_LEGENDS_MAPPING.name), 0);
        // Player not in the car: speed 0, rpm -1, max -1, gear -2.
        let mut b = BlockBuilder::new(SIMBIN_BLOCK_SIZE);
        b.f32(24, -1.0).f32(28, -1.0).i32(88, -2);
        blocks.set(GT_LEGENDS_MAPPING, b.build());
        assert!(!l.is_running().unwrap());
        // Gear -2 alone with a moving car is live.
        b.f32(56, 1.0);
        blocks.set(GT_LEGENDS_MAPPING, b.build());
        assert!(l.is_running().unwrap());
        // A live check replaces the object while keeping the section continuously held.
        let opens = blocks.open_count();
        assert!(l.is_running().unwrap());
        assert_eq!(blocks.open_count(), opens + 1);
        assert_eq!(blocks.held_count(GT_LEGENDS_MAPPING.name), 1);
        // The idle pattern drops it; the next call makes a new one.
        blocks.set(GT_LEGENDS_MAPPING, vec![]);
        assert!(!l.is_running().unwrap());
        assert_eq!(blocks.held_count(GT_LEGENDS_MAPPING.name), 0);
        assert!(!l.is_running().unwrap());
        assert_eq!(blocks.open_count(), opens + 3);
    }

    #[test]
    fn receive_before_snapshot_is_true_and_only_caught_or_unmapped_getters_answer() {
        let blocks = FakeBlocks::default();
        for title in [SimBinTitle::Gtr, SimBinTitle::Gtr2, SimBinTitle::GtLegends, SimBinTitle::Race07] {
            let mut l = listener(&blocks, title);
            let mut t = Telemetry::off();
            assert!(l.poll(&mut t).unwrap());
            assert_eq!(t, Telemetry::off());
            assert_eq!(l.native_channel("fuel_level"), Some(LegacyValue::Int(0)));
            assert_eq!(l.native_channel("unknown"), Some(LegacyValue::Int(0)));
            for name in ["rpm", "max_rpm", "speed", "gear", "pitch"] {
                assert_eq!(l.native_channel(name), None, "{}: {name}", title.listener_id());
                assert!(l.channel_read_raised(name));
            }
        }
    }

    #[test]
    fn nonfinite_speed_keeps_its_negative_clamp_and_raising_positive_or_nan_paths() {
        let blocks = FakeBlocks::default();
        for (speed, expected) in
            [(f32::NEG_INFINITY, Some(LegacyValue::Int(0))), (f32::INFINITY, None), (f32::NAN, None)]
        {
            let mut b = driving_block();
            b.f32(56, speed);
            blocks.set(GTR_MAPPING, b.build());
            let mut source = listener(&blocks, SimBinTitle::Gtr);
            assert!(source.is_running().unwrap());
            assert_eq!(source.native_channel("speed"), expected);
            assert_eq!(source.channel_read_raised("speed"), expected.is_none());
            use crate::derive::{Listener, SourceListener};
            let mut listener = SourceListener::new(source);
            listener.receive();
            if expected.is_none() {
                assert_eq!(listener.channel("speed"), None);
            }
        }
    }
}
