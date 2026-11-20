//! Euro Truck Simulator 2 / American Truck Simulator through the ets2-telemetry plugin block
//! (byte-packed, 1018 bytes).

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::block_reader::{BlockOpener, BridgeMirrors, Fields, KeptMapping, MappingSpec};
use super::listener_channels::begin_tick;
use super::python_numeric::{decode_utf8, percent_or_zero, rad_to_deg, round_6, round_to_int};
use crate::{Source, SourceError};

/// The plugin's mapping.
pub const TRUCKSIM_MAPPING: MappingSpec = MappingSpec { name: "Local\\SimTelemetryETS2", size: 1018 };

/// Offset and length of `truckModel` (64 characters).
const TRUCK_MODEL: (usize, usize) = (800, 64);

/// The fields of the plugin block the listener reads.
#[derive(Debug, Clone, PartialEq)]
struct TruckFrame {
    paused: u32,
    speed: f64,
    acceleration_x: f64,
    acceleration_y: f64,
    acceleration_z: f64,
    rotation_x: f64,
    rotation_y: f64,
    rotation_z: f64,
    gear: i32,
    engine_rpm: f64,
    engine_rpm_max: f64,
    fuel: f64,
    fuel_capacity: f64,
    user_steer: f64,
    oil_temperature: f64,
    water_temperature: f64,
    /// `truckModel` decoded as UTF-8; `None` when the bytes are not valid UTF-8.
    truck_model: Option<String>,
}

impl TruckFrame {
    fn decode(bytes: &[u8]) -> Self {
        let f = Fields(bytes);
        TruckFrame {
            paused: f.u32(4),
            speed: f.f32(24),
            acceleration_x: f.f32(28),
            acceleration_y: f.f32(32),
            acceleration_z: f.f32(36),
            rotation_x: f.f32(52),
            rotation_y: f.f32(56),
            rotation_z: f.f32(60),
            gear: f.i32(64),
            engine_rpm: f.f32(80),
            engine_rpm_max: f.f32(84),
            fuel: f.f32(88),
            fuel_capacity: f.f32(92),
            user_steer: f.f32(104),
            oil_temperature: f.f32(624),
            water_temperature: f.f32(628),
            truck_model: decode_utf8(f.c_chars(TRUCK_MODEL.0, TRUCK_MODEL.1)),
        }
    }

    /// Paused (also set by the plugin in menus), or a block nobody has written (`speed + rpm + max rpm + gear`
    /// summed as doubles is 0).
    fn is_idle(&self) -> bool {
        self.paused == 1 || self.speed + self.engine_rpm + self.engine_rpm_max + f64::from(self.gear) == 0.0
    }

    /// Writes the channels.
    fn write(&self, t: &mut Telemetry) {
        let previous = begin_tick(t);
        t.rpm = round_to_int(self.engine_rpm).unwrap_or(previous.rpm);
        t.max_rpm = round_to_int(self.engine_rpm_max).unwrap_or(previous.max_rpm);
        t.gear = f64::from(self.gear);
        t.speed = if self.speed < 0.0 { Some(0.0) } else { round_to_int(self.speed) }.unwrap_or(previous.speed);
        t.steering_wheel_angle = -self.user_steer;
        t.longitudinal_acceleration = -round_6(self.acceleration_z);
        t.lateral_acceleration = -(round_6(self.acceleration_x) * 2.0);
        t.vertical_acceleration = round_6(self.acceleration_y) * 10.0;
        t.yaw = rad_to_deg(self.rotation_x);
        t.roll = rad_to_deg(self.rotation_z) * 10.0;
        t.pitch = rad_to_deg(self.rotation_y) * 10.0;
        t.vehicle = self.truck_model.clone().unwrap_or(previous.vehicle);
        t.oil_temperature = self.oil_temperature;
        t.water_temperature = self.water_temperature;
        t.fuel_level = percent_or_zero(self.fuel, self.fuel_capacity);
    }
}

/// The ETS2 / ATS listener (key `trucksim`).
pub struct TruckSim {
    opener: Box<dyn BlockOpener>,
    /// The kept mapping object.
    handle: KeptMapping,
    frame: Option<TruckFrame>,
}

impl TruckSim {
    /// Listener reading the bridge mirror of [`TRUCKSIM_MAPPING`].
    pub fn new() -> Self {
        Self::with_opener(Box::new(BridgeMirrors))
    }

    /// Listener reading mappings through `opener`.
    pub fn with_opener(opener: Box<dyn BlockOpener>) -> Self {
        TruckSim { opener, handle: KeptMapping::new(TRUCKSIM_MAPPING), frame: None }
    }

    /// The liveness test: a new mapping object on every call,
    /// replacing the previous object after successful creation, then copying it; both are dropped while the block
    /// is idle.
    fn refresh(&mut self) -> bool {
        let Ok(bytes) = self.handle.remake(self.opener.as_mut()) else { return false };
        let frame = TruckFrame::decode(&bytes);
        if frame.is_idle() {
            self.handle.release();
            self.frame = None;
            return false;
        }
        self.frame = Some(frame);
        true
    }
}

impl Default for TruckSim {
    fn default() -> Self {
        Self::new()
    }
}

impl Source for TruckSim {
    fn id(&self) -> &'static str {
        "trucksim"
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        use LegacyValue::{Float, Int, Str};
        let frame = self.frame.as_ref();
        let value = match name {
            "rpm" => return round_to_int(frame?.engine_rpm).and_then(crate::udp_racing::int_text).map(Str),
            "max_rpm" => return round_to_int(frame?.engine_rpm_max).and_then(crate::udp_racing::int_text).map(Str),
            "gear" => return Some(Str(frame?.gear.to_string())),
            "speed" => {
                let speed = frame?.speed;
                return if speed < 0.0 {
                    Some(Int(0))
                } else {
                    round_to_int(speed).and_then(crate::udp_racing::int_text).map(Str)
                };
            }
            "car_name" => return frame?.truck_model.clone().map(Str),
            "game" => return None, // The window list this read needs is not available: the read fails.
            "fuel_level" => {
                return Some(
                    frame
                        .filter(|f| f.fuel_capacity != 0.0)
                        .map_or(Int(0), |f| Float(percent_or_zero(f.fuel, f.fuel_capacity))),
                );
            }
            "steering_wheel_angle" => -frame?.user_steer,
            "longitudinal_acceleration" => -round_6(frame?.acceleration_z),
            "lateral_acceleration" => -(round_6(frame?.acceleration_x) * 2.0),
            "vertical_acceleration" => round_6(frame?.acceleration_y) * 10.0,
            "yaw" => rad_to_deg(frame?.rotation_x),
            "roll" => rad_to_deg(frame?.rotation_z) * 10.0,
            "pitch" => rad_to_deg(frame?.rotation_y) * 10.0,
            "oil_temperature" => frame?.oil_temperature,
            "water_temperature" => frame?.water_temperature,
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
        if let Some(frame) = &self.frame {
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
        let mut b = BlockBuilder::new(TRUCKSIM_MAPPING.size);
        b.u32(4, 0) // paused
            .f32(24, 22.5) // speed m/s
            .f32(28, 0.75) // accelerationX
            .f32(32, -0.125) // accelerationY
            .f32(36, 1.5) // accelerationZ
            .f32(52, 0.25) // rotationX
            .f32(56, 0.01) // rotationY
            .f32(60, -0.02) // rotationZ
            .i32(64, -1) // gear (reverse)
            .f32(80, 1499.5) // engineRpm
            .f32(84, 2500.0) // engineRpmMax
            .f32(88, 150.0) // fuel
            .f32(92, 600.0) // fuelCapacity
            .f32(104, 0.5) // userSteer
            .f32(624, 90.0) // oilTemperature
            .f32(628, 85.0) // waterTemperature
            .bytes(800, b"Scania R\0junk");
        b
    }

    fn listener(blocks: &FakeBlocks) -> TruckSim {
        TruckSim::with_opener(Box::new(blocks.clone()))
    }

    #[test]
    fn native_receive_retains_raw_truck_identity_without_projecting_a_frame() {
        use crate::derive::{Listener, SourceListener};
        use crate::NativeRead;

        let blocks = FakeBlocks::default();
        blocks.set(TRUCKSIM_MAPPING, driving_block().build());
        let mut source = listener(&blocks);
        assert!(source.is_running().unwrap());
        let before = Telemetry { vehicle: "previous".into(), speed: 1.0, ..Telemetry::off() };
        let mut compatibility = before.clone();
        assert!(source.receive_native(&mut compatibility).unwrap());
        assert_eq!(compatibility, before);
        let mut adapter = SourceListener::new(source);
        adapter.receive();
        assert_eq!(adapter.read_native("car_name"), NativeRead::Value("Scania R".into()));
        assert_eq!(adapter.read_native("track_loc"), NativeRead::Value(LegacyValue::Int(0)));
        assert_eq!(adapter.failure_point(), None);
    }

    #[test]
    fn native_mmap_recreation_preserves_failure_and_replacement_order() {
        use crate::shared_memory::block_reader::fake::{MapAttempt, ScriptedMaps};
        let maps = ScriptedMaps::default();
        maps.push(MapAttempt::Bytes(driving_block().build()));
        let mut source = TruckSim::with_opener(Box::new(maps.clone()));
        assert!(source.is_running().unwrap());
        assert_eq!(maps.trace(), ["create:1", "read:1"]);
        maps.push(MapAttempt::Bytes(driving_block().build()));
        assert!(source.is_running().unwrap());
        assert_eq!(maps.trace(), ["create:2", "close:1", "read:2"]);
        maps.push(MapAttempt::CreateError);
        assert!(!source.is_running().unwrap());
        assert_eq!(maps.trace(), ["create:3"]);
        assert_eq!(source.native_channel("rpm"), Some(LegacyValue::from("1500")));
        maps.push(MapAttempt::ReadError);
        assert!(!source.is_running().unwrap());
        assert_eq!(maps.trace(), ["create:4", "close:2", "read:4"]);
        assert_eq!(source.native_channel("rpm"), Some(LegacyValue::from("1500")));
        let mut idle = driving_block();
        idle.u32(4, 1);
        maps.push(MapAttempt::Bytes(idle.build()));
        assert!(!source.is_running().unwrap());
        assert_eq!(maps.trace(), ["create:5", "close:4", "read:5", "close:5"]);
        assert_eq!(source.native_channel("rpm"), None);
        maps.push(MapAttempt::Bytes(driving_block().build()));
        assert!(source.is_running().unwrap());
        assert_eq!(maps.trace(), ["create:6", "read:6"]);
        drop(source);
        assert_eq!(maps.trace(), ["close:6"]);
    }

    #[test]
    fn mapping_constant() {
        assert_eq!(TRUCKSIM_MAPPING.name, r"Local\SimTelemetryETS2");
        assert_eq!(TRUCKSIM_MAPPING.size, 1018);
    }

    #[test]
    fn get_channel_maps_the_block() {
        let blocks = FakeBlocks::default();
        blocks.set(TRUCKSIM_MAPPING, driving_block().build());
        let mut l = listener(&blocks);
        assert!(l.is_running().unwrap());
        let mut t = Telemetry::off();
        assert!(l.poll(&mut t).unwrap());
        assert_eq!(t.rpm, 1500.0); // round(1499.5) -> 1500 (even)
        assert_eq!(t.max_rpm, 2500.0);
        assert_eq!(t.gear, -1.0);
        assert_eq!(t.speed, 22.0); // round(22.5) -> 22
        assert_eq!(t.steering_wheel_angle, -0.5);
        assert_eq!(t.longitudinal_acceleration, -1.5);
        assert_eq!(t.lateral_acceleration, -1.5);
        assert_eq!(t.vertical_acceleration, -1.25);
        assert_eq!(t.yaw, rad_to_deg(0.25));
        assert_eq!(t.roll, rad_to_deg(f64::from(-0.02f32)) * 10.0);
        assert_eq!(t.pitch, rad_to_deg(f64::from(0.01f32)) * 10.0);
        assert_eq!(t.vehicle, "Scania R");
        assert_eq!(t.oil_temperature, 90.0);
        assert_eq!(t.water_temperature, 85.0);
        assert_eq!(t.fuel_level, 25.0);
        assert_eq!(t.lateral_velocity, 0.0);
        for (name, expected) in [
            ("rpm", LegacyValue::Str("1500".into())),
            ("max_rpm", LegacyValue::Str("2500".into())),
            ("gear", LegacyValue::Str("-1".into())),
            ("speed", LegacyValue::Str("22".into())),
            ("car_name", LegacyValue::Str("Scania R".into())),
            ("steering_wheel_angle", LegacyValue::Float(-0.5)),
            ("fuel_level", LegacyValue::Float(25.0)),
            ("unknown", LegacyValue::Int(0)),
        ] {
            assert_eq!(l.native_channel(name), Some(expected), "{name}");
        }
        assert!(l.channel_read_raised("game"), "the original game getter reads an absent module attribute");
    }

    #[test]
    fn get_channel_invalid_model_name_keeps_vehicle() {
        let blocks = FakeBlocks::default();
        let mut b = driving_block();
        b.bytes(800, &[0xc3, 0x28, 0]);
        blocks.set(TRUCKSIM_MAPPING, b.build());
        let mut l = listener(&blocks);
        assert!(l.is_running().unwrap());
        let mut t = Telemetry::off();
        t.vehicle = "before".into();
        l.poll(&mut t).unwrap();
        assert_eq!(t.vehicle, "before");
        assert_eq!(l.native_channel("car_name"), None);
        assert!(l.channel_read_raised("car_name"));
        use crate::derive::{Listener, SourceListener};
        let mut listener = SourceListener::new(l);
        listener.receive();
        assert_eq!(listener.vehicle(), None, "a failed getter must not expose the compatibility frame's identity");
    }

    #[test]
    fn pre_snapshot_receive_and_caught_fuel_defaults_retain_native_kinds() {
        let blocks = FakeBlocks::default();
        let mut source = listener(&blocks);
        let mut telemetry = Telemetry::off();
        assert!(source.poll(&mut telemetry).unwrap());
        assert_eq!(source.native_channel("fuel_level"), Some(LegacyValue::Int(0)));
        assert_eq!(source.native_channel("unknown"), Some(LegacyValue::Int(0)));
        assert!(source.channel_read_raised("rpm"));
        assert!(source.channel_read_raised("car_name"));
        assert!(source.channel_read_raised("game"));
        let mut block = driving_block();
        block.f32(24, -3.0).f32(92, 0.0);
        blocks.set(TRUCKSIM_MAPPING, block.build());
        assert!(source.is_running().unwrap());
        assert_eq!(source.native_channel("speed"), Some(LegacyValue::Int(0)));
        assert_eq!(source.native_channel("fuel_level"), Some(LegacyValue::Int(0)));
        let mut block = driving_block();
        block.f32(24, -0.0).f32(88, 0.0).f32(80, f32::NAN);
        blocks.set(TRUCKSIM_MAPPING, block.build());
        assert!(source.is_running().unwrap());
        assert_eq!(source.native_channel("speed"), Some(LegacyValue::Str("0".into())));
        assert_eq!(source.native_channel("fuel_level"), Some(LegacyValue::Float(0.0)));
        assert!(source.channel_read_raised("rpm"));
        assert_eq!(source.native_channel("rpm"), None);
    }

    #[test]
    fn check_status_paused_and_blank() {
        let blocks = FakeBlocks::default();
        let mut l = listener(&blocks);
        assert!(!l.is_running().unwrap(), "no mapping");
        let mut b = driving_block();
        b.u32(4, 1);
        blocks.set(TRUCKSIM_MAPPING, b.build());
        assert!(!l.is_running().unwrap(), "paused == 1");
        b.u32(4, 2);
        blocks.set(TRUCKSIM_MAPPING, b.build());
        assert!(l.is_running().unwrap(), "only paused == 1 counts");
        // Sum speed + rpm + max rpm + gear == 0 even with non-zero terms.
        let mut c = BlockBuilder::new(TRUCKSIM_MAPPING.size);
        c.f32(24, 0.0).f32(80, 1.0).f32(84, 1.0).i32(64, -2);
        blocks.set(TRUCKSIM_MAPPING, c.build());
        assert!(!l.is_running().unwrap());
        blocks.set(TRUCKSIM_MAPPING, vec![]);
        assert!(!l.is_running().unwrap());
        // Each call creates a new object, including the live-to-idle transition.
        assert_eq!(blocks.open_count(), 5);
        assert_eq!(blocks.held_count(TRUCKSIM_MAPPING.name), 0);
    }
}
