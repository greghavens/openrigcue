//! The PiBoso titles GP Bikes, MX Bikes, Kart Racing Pro and World Racing Series (listeners `gpbikes`, `mxbikes`,
//! `kart`, `wrs`): the `SProxyData_t` block the games' proxy output plugin publishes.
//!
//! The four titles share one listener; their block layouts are byte-packed.

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::answers::{write_all_raised, zero_generic_channels, MotionRates, ZERO_NAME};
use super::block_reader::{BlockOpener, Fields, KeptMapping, MappingSpec};
use super::packed::ArrayFields;
use super::pages::LiveMirrors;
use super::python_numeric::{decode_utf8, percent_or_zero, round_to_int};
use crate::{NativeRead, Source, SourceError};

/// Where one title's `SProxyData_t` keeps the fields the listener reads.
#[derive(Debug)]
pub struct PibosoTitle {
    /// Listener key.
    pub id: &'static str,
    /// The proxy block (its mapping name and size).
    pub mapping: MappingSpec,
    event: EventFields,
    data: DataFields,
}

/// Absolute offsets inside `sEvent` (which starts at byte 8).
#[derive(Debug)]
struct EventFields {
    vehicle_name: usize,
    max_rpm: usize,
    max_fuel: usize,
    track_name: usize,
}

/// Absolute offsets inside `sData`.
#[derive(Debug)]
struct DataFields {
    rpm: usize,
    engine_temperature: Option<usize>,
    water_temperature: usize,
    oil_pressure: Option<usize>,
    gear: usize,
    fuel: usize,
    speedometer: usize,
    velocity: usize,
    acceleration: usize,
    yaw_pitch_roll: usize,
}

/// Length of the character name fields.
const NAME_BYTES: usize = 100;
/// `iState` value the listener treats as running.
const STATE_RUNNING: i32 = 2;
/// `SProxyData_t.iState`.
const STATE: usize = 4;

/// GP Bikes: `Local\GPBProxyObject`, 1027 bytes (`sEvent` at 8, `sData` at 808).
pub const GP_BIKES: PibosoTitle = PibosoTitle {
    id: "gpbikes",
    mapping: MappingSpec { name: "Local\\GPBProxyObject", size: 1027 },
    event: EventFields { vehicle_name: 8 + 200, max_rpm: 8 + 304, max_fuel: 8 + 328, track_name: 8 + 544 },
    data: DataFields {
        rpm: 808,
        engine_temperature: Some(808 + 4),
        water_temperature: 808 + 8,
        oil_pressure: None,
        gear: 808 + 12,
        fuel: 808 + 16,
        speedometer: 808 + 20,
        velocity: 808 + 36,
        acceleration: 808 + 48,
        yaw_pitch_roll: 808 + 96,
    },
};

/// MX Bikes: `Local\MXBProxyObject`, 996 bytes (`sEvent` at 8, `sData` at 804).
pub const MX_BIKES: PibosoTitle = PibosoTitle {
    id: "mxbikes",
    mapping: MappingSpec { name: "Local\\MXBProxyObject", size: 996 },
    event: EventFields { vehicle_name: 8 + 200, max_rpm: 8 + 304, max_fuel: 8 + 328, track_name: 8 + 544 },
    data: DataFields {
        rpm: 804,
        engine_temperature: Some(804 + 4),
        water_temperature: 804 + 8,
        oil_pressure: None,
        gear: 804 + 12,
        fuel: 804 + 16,
        speedometer: 804 + 20,
        velocity: 804 + 36,
        acceleration: 804 + 48,
        yaw_pitch_roll: 804 + 96,
    },
};

/// Kart Racing Pro: `Local\KRPProxyObject`, 1112 bytes (`sEvent` at 8, `sData` at 908); no oil temperature.
pub const KART_RACING_PRO: PibosoTitle = PibosoTitle {
    id: "kart",
    mapping: MappingSpec { name: "Local\\KRPProxyObject", size: 1112 },
    event: EventFields { vehicle_name: 8 + 200, max_rpm: 8 + 308, max_fuel: 8 + 336, track_name: 8 + 640 },
    data: DataFields {
        rpm: 908,
        engine_temperature: None,
        water_temperature: 908 + 8,
        oil_pressure: None,
        gear: 908 + 12,
        fuel: 908 + 16,
        speedometer: 908 + 20,
        velocity: 908 + 36,
        acceleration: 908 + 48,
        yaw_pitch_roll: 908 + 96,
    },
};

/// World Racing Series: `Local\WRSProxyObject`, 1048 bytes (`sEvent` at 8, `sData` at 804); adds oil pressure.
pub const WORLD_RACING_SERIES: PibosoTitle = PibosoTitle {
    id: "wrs",
    mapping: MappingSpec { name: "Local\\WRSProxyObject", size: 1048 },
    event: EventFields { vehicle_name: 8 + 200, max_rpm: 8 + 304, max_fuel: 8 + 328, track_name: 8 + 536 },
    data: DataFields {
        rpm: 804,
        engine_temperature: Some(804 + 8),
        water_temperature: 804 + 12,
        oil_pressure: Some(804 + 16),
        gear: 804 + 20,
        fuel: 804 + 24,
        speedometer: 804 + 28,
        velocity: 804 + 44,
        acceleration: 804 + 56,
        yaw_pitch_roll: 804 + 104,
    },
};

/// Every mapping the four PiBoso listeners read.
pub const MAPPINGS: &[MappingSpec] =
    &[GP_BIKES.mapping, MX_BIKES.mapping, KART_RACING_PRO.mapping, WORLD_RACING_SERIES.mapping];

/// One PiBoso listener.
pub struct Piboso<O: BlockOpener = LiveMirrors> {
    opener: O,
    title: &'static PibosoTitle,
    /// The kept mapping object: replaced by a new one on each status check.
    handle: KeptMapping,
    /// The last successfully copied block, kept when a later copy fails.
    snapshot: Option<Vec<u8>>,
}

impl Piboso<LiveMirrors> {
    /// Listener for `title` reading the bridge mirror.
    pub fn new(title: &'static PibosoTitle) -> Self {
        Piboso::with_opener(LiveMirrors, title)
    }
}

impl<O: BlockOpener> Piboso<O> {
    /// Listener for `title` reading the mapping `opener` opens.
    pub fn with_opener(opener: O, title: &'static PibosoTitle) -> Self {
        Piboso { opener, title, handle: KeptMapping::new(title.mapping), snapshot: None }
    }

    /// The PiBoso listeners answer no rate channel (they fall through to `0`).
    pub fn motion_rates(&self) -> MotionRates {
        MotionRates::ZERO
    }

    fn native_answer(&self, name: &str) -> Option<LegacyValue> {
        let (event, data) = (&self.title.event, &self.title.data);
        if name == "fuel_level" {
            // A failure anywhere in this entire branch is handled as one.
            return Some(self.snapshot.as_ref().map_or(LegacyValue::Int(0), |bytes| {
                let f = Fields(bytes);
                let denominator = f.f32(event.max_fuel);
                if denominator == 0.0 {
                    LegacyValue::Int(0)
                } else {
                    LegacyValue::Float(f.f32(data.fuel) / denominator * 100.0)
                }
            }));
        }
        let supported = matches!(
            name,
            "rpm"
                | "max_rpm"
                | "gear"
                | "speed"
                | "car_name"
                | "track_loc"
                | "lateral_acceleration"
                | "vertical_acceleration"
                | "longitudinal_acceleration"
                | "vertical_velocity"
                | "lateral_velocity"
                | "longitudinal_velocity"
                | "yaw"
                | "roll"
                | "pitch"
                | "water_temperature"
        ) || (name == "oil_temperature" && data.engine_temperature.is_some())
            || (name == "oil_pressure" && data.oil_pressure.is_some());
        if !supported {
            return Some(LegacyValue::Int(0));
        }
        let f = Fields(self.snapshot.as_ref()?);
        let float = match name {
            "rpm" => return Some(LegacyValue::Str(f.i32(data.rpm).to_string())),
            "max_rpm" => return Some(LegacyValue::Str(f.i32(event.max_rpm).to_string())),
            "gear" => return Some(LegacyValue::Str(f.i32(data.gear).to_string())),
            "speed" => {
                return crate::udp_racing::int_text(round_to_int(f.f32(data.speedometer))?).map(LegacyValue::Str)
            }
            "car_name" => return decode_utf8(f.c_chars(event.vehicle_name, NAME_BYTES)).map(LegacyValue::Str),
            "track_loc" => return decode_utf8(f.c_chars(event.track_name, NAME_BYTES)).map(LegacyValue::Str),
            "lateral_acceleration" => -(f.f32_at(data.acceleration, 0) * 10.0),
            "vertical_acceleration" => f.f32_at(data.acceleration, 1) * 10.0,
            "longitudinal_acceleration" => f.f32_at(data.acceleration, 2) * 10.0,
            "vertical_velocity" => f.f32_at(data.velocity, 1),
            "lateral_velocity" => f.f32_at(data.velocity, 0),
            "longitudinal_velocity" => f.f32_at(data.velocity, 2),
            "yaw" => f.f32_at(data.yaw_pitch_roll, 0),
            "roll" => -f.f32_at(data.yaw_pitch_roll, 2),
            "pitch" => -f.f32_at(data.yaw_pitch_roll, 1),
            "oil_temperature" => f.f32(data.engine_temperature?),
            "water_temperature" => f.f32(data.water_temperature),
            "oil_pressure" => f.f32(data.oil_pressure?),
            _ => unreachable!("supported only includes implemented channels"),
        };
        Some(LegacyValue::Float(float))
    }
}

impl<O: BlockOpener> Source for Piboso<O> {
    fn id(&self) -> &'static str {
        self.title.id
    }

    fn receive_native(&mut self, _t: &mut Telemetry) -> Result<bool, SourceError> {
        Ok(true)
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        self.native_answer(name)
    }

    fn read_native(&self, name: &str) -> NativeRead {
        self.native_answer(name).map_or(NativeRead::Raised, NativeRead::Value)
    }

    /// The liveness test: a new mapping object,
    /// replacing the previous object after creation succeeds, then a copy of the block, running while
    /// its `iState` is 2.
    fn is_running(&mut self) -> Result<bool, SourceError> {
        let bytes = match self.handle.remake(&mut self.opener) {
            Ok(bytes) => bytes,
            Err(error) => {
                // GP Bikes and MX Bikes print the failure. The kart and WRS
                // listeners print nothing.
                if matches!(self.title.id, "gpbikes" | "mxbikes") {
                    crate::printed::line(crate::printed::os_error(&error));
                }
                return Ok(false);
            }
        };
        let running = Fields(&bytes).i32(STATE) == STATE_RUNNING;
        self.snapshot = Some(bytes);
        Ok(running)
    }

    /// Channel reads over the copied block; the receive step does nothing.
    fn poll(&mut self, t: &mut Telemetry) -> Result<bool, SourceError> {
        let Some(bytes) = &self.snapshot else {
            write_without_snapshot(t);
            return Ok(true);
        };
        let f = Fields(bytes);
        let (event, data) = (&self.title.event, &self.title.data);
        zero_generic_channels(t);
        t.rpm = f64::from(f.i32(data.rpm));
        t.max_rpm = f64::from(f.i32(event.max_rpm));
        t.gear = f64::from(f.i32(data.gear));
        if let Some(speed) = round_to_int(f.f32(data.speedometer)) {
            t.speed = speed;
        }
        // Strict decode: a failing track_loc stores 0, a failing car_name keeps the previous vehicle.
        t.location = decode_utf8(f.c_chars(event.track_name, NAME_BYTES)).unwrap_or_else(|| ZERO_NAME.to_owned());
        if let Some(vehicle) = decode_utf8(f.c_chars(event.vehicle_name, NAME_BYTES)) {
            t.vehicle = vehicle;
        }
        t.lateral_acceleration = -(f.f32_at(data.acceleration, 0) * 10.0);
        t.vertical_acceleration = f.f32_at(data.acceleration, 1) * 10.0;
        t.longitudinal_acceleration = f.f32_at(data.acceleration, 2) * 10.0;
        t.vertical_velocity = f.f32_at(data.velocity, 1);
        t.lateral_velocity = f.f32_at(data.velocity, 0);
        t.longitudinal_velocity = f.f32_at(data.velocity, 2);
        // Angles in the game's radians, without conversion.
        t.yaw = f.f32_at(data.yaw_pitch_roll, 0);
        t.roll = -f.f32_at(data.yaw_pitch_roll, 2);
        t.pitch = -f.f32_at(data.yaw_pitch_roll, 1);
        if let Some(offset) = data.engine_temperature {
            t.oil_temperature = f.f32(offset);
        }
        t.water_temperature = f.f32(data.water_temperature);
        t.fuel_level = percent_or_zero(f.f32(data.fuel), f.f32(event.max_fuel));
        if let Some(offset) = data.oil_pressure {
            t.oil_pressure = f.f32(offset);
        }
        Ok(true)
    }
}

/// Channel reads while there is no copied block yet: the implemented channels fail (the guarded `fuel_level` answers 0),
/// the others fall through to 0.
fn write_without_snapshot(t: &mut Telemetry) {
    write_all_raised(t);
    for value in [
        &mut t.wheel_slip_lf,
        &mut t.wheel_slip_lr,
        &mut t.wheel_slip_rf,
        &mut t.wheel_slip_rr,
        &mut t.suspension_travel_lf,
        &mut t.suspension_travel_lr,
        &mut t.suspension_travel_rf,
        &mut t.suspension_travel_rr,
        &mut t.suspension_velocity_lf,
        &mut t.suspension_velocity_lr,
        &mut t.suspension_velocity_rf,
        &mut t.suspension_velocity_rr,
        &mut t.flag,
        &mut t.raw_flag,
    ] {
        *value = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared_memory::block_reader::fake::BlockBuilder as Image;
    use crate::shared_memory::block_reader::fake::FakeBlocks;

    fn image(title: &PibosoTitle, state: i32) -> Image {
        let (e, d) = (&title.event, &title.data);
        let mut i = Image::new(title.mapping.size);
        i.i32(0, 1)
            .i32(STATE, state)
            .bytes(e.vehicle_name, b"KTM 450")
            .i32(e.max_rpm, 13000)
            .f32(e.max_fuel, 8.0)
            .bytes(e.track_name, b"Ottobiano")
            .i32(d.rpm, 9000)
            .f32(d.water_temperature, 70.0)
            .i32(d.gear, 2)
            .f32(d.fuel, 2.0)
            .f32(d.speedometer, 12.5)
            .f32(d.velocity, 1.0)
            .f32(d.velocity + 4, 0.5)
            .f32(d.velocity + 8, 12.0)
            .f32(d.acceleration, 0.25)
            .f32(d.acceleration + 4, -0.5)
            .f32(d.acceleration + 8, 1.5)
            .f32(d.yaw_pitch_roll, 1.0)
            .f32(d.yaw_pitch_roll + 4, 0.25)
            .f32(d.yaw_pitch_roll + 8, -0.5);
        if let Some(offset) = d.engine_temperature {
            i.f32(offset, 95.0);
        }
        if let Some(offset) = d.oil_pressure {
            i.f32(offset, 3.5);
        }
        i
    }

    fn listener(title: &'static PibosoTitle, state: i32) -> Piboso<FakeBlocks> {
        let pages = FakeBlocks::default();
        pages.set(title.mapping, image(title, state).build());
        Piboso::with_opener(pages, title)
    }

    #[test]
    fn native_mmap_recreation_preserves_failure_and_replacement_order() {
        use crate::shared_memory::block_reader::fake::{MapAttempt, ScriptedMaps};
        for title in [&GP_BIKES, &MX_BIKES, &KART_RACING_PRO, &WORLD_RACING_SERIES] {
            let maps = ScriptedMaps::default();
            maps.push(MapAttempt::Bytes(image(title, 2).build()));
            let mut source = Piboso::with_opener(maps.clone(), title);
            assert!(source.is_running().unwrap());
            assert_eq!(maps.trace(), ["create:1", "read:1"], "{}", title.id);
            let mut next = image(title, 2);
            next.i32(title.data.rpm, 7777);
            maps.push(MapAttempt::Bytes(next.build()));
            assert!(source.is_running().unwrap());
            assert_eq!(maps.trace(), ["create:2", "close:1", "read:2"], "{}", title.id);
            assert_eq!(source.read_native("rpm"), NativeRead::Value(LegacyValue::from("7777")));
            maps.push(MapAttempt::CreateError);
            assert!(!source.is_running().unwrap());
            assert_eq!(maps.trace(), ["create:3"]);
            assert_eq!(source.read_native("rpm"), NativeRead::Value(LegacyValue::from("7777")));
            maps.push(MapAttempt::ReadError);
            assert!(!source.is_running().unwrap());
            assert_eq!(maps.trace(), ["create:4", "close:2", "read:4"]);
            assert_eq!(source.read_native("rpm"), NativeRead::Value(LegacyValue::from("7777")));
            maps.push(MapAttempt::Bytes(image(title, 1).build()));
            assert!(!source.is_running().unwrap());
            assert_eq!(maps.trace(), ["create:5", "close:4", "read:5"]);
            assert_eq!(source.read_native("rpm"), NativeRead::Value(LegacyValue::from("9000")));
            drop(source);
            assert_eq!(maps.trace(), ["close:5"]);
        }
    }

    #[test]
    fn native_receive_does_not_project() {
        let mut source = listener(&GP_BIKES, 2);
        source.is_running().unwrap();
        let mut t = Telemetry { speed: 999.0, ..Telemetry::off() };
        source.receive_native(&mut t).unwrap();
        assert_eq!(t.speed, 999.0);
    }

    #[test]
    fn native_title_kinds_and_initial_failure_boundaries() {
        for title in [&GP_BIKES, &MX_BIKES, &KART_RACING_PRO, &WORLD_RACING_SERIES] {
            let mut source = listener(title, 2);
            for name in ["rpm", "speed", "max_rpm", "gear", "car_name", "roll"] {
                assert_eq!(source.read_native(name), NativeRead::Raised, "{} {name}", title.id);
            }
            for name in ["fuel_level", "unknown", "yaw_rate", "flag"] {
                assert_eq!(source.native_channel(name), Some(LegacyValue::Int(0)), "{} {name}", title.id);
            }
            source.is_running().unwrap();
            for (name, expected) in [
                ("rpm", LegacyValue::Str("9000".into())),
                ("max_rpm", LegacyValue::Str("13000".into())),
                ("gear", LegacyValue::Str("2".into())),
                ("speed", LegacyValue::Str("12".into())),
                ("car_name", LegacyValue::Str("KTM 450".into())),
                ("track_loc", LegacyValue::Str("Ottobiano".into())),
                ("lateral_acceleration", LegacyValue::Float(-2.5)),
                ("roll", LegacyValue::Float(0.5)),
                ("fuel_level", LegacyValue::Float(25.0)),
            ] {
                assert_eq!(source.native_channel(name), Some(expected), "{} {name}", title.id);
            }
            let oil = title.data.engine_temperature.map_or(LegacyValue::Int(0), |_| LegacyValue::Float(95.0));
            let pressure = title.data.oil_pressure.map_or(LegacyValue::Int(0), |_| LegacyValue::Float(3.5));
            assert_eq!(source.native_channel("oil_temperature"), Some(oil));
            assert_eq!(source.native_channel("oil_pressure"), Some(pressure));
        }
        let kart = listener(&KART_RACING_PRO, 2);
        assert_eq!(kart.native_channel("oil_temperature"), Some(LegacyValue::Int(0)));
        assert_eq!(kart.native_channel("oil_pressure"), Some(LegacyValue::Int(0)));
    }

    #[test]
    fn native_adapter_reads_only_the_check_status_copy_without_state_gate() {
        use crate::derive::{Listener, SourceListener};
        let title = &WORLD_RACING_SERIES;
        let blocks = FakeBlocks::default();
        blocks.set(title.mapping, image(title, 2).build());
        let mut listener = SourceListener::new(Piboso::with_opener(blocks.clone(), title));
        assert!(listener.is_running());
        let mut changed = image(title, 1);
        changed
            .i32(title.data.rpm, 7777)
            .f32(title.data.speedometer, f32::INFINITY)
            .f32(title.event.max_fuel, 0.0)
            .bytes(title.event.vehicle_name, b"\xff");
        blocks.set(title.mapping, changed.build());
        listener.receive();
        assert_eq!(listener.native_channel("rpm"), Some(LegacyValue::Str("9000".into())));
        assert!(!listener.is_running());
        assert_eq!(listener.native_channel("rpm"), Some(LegacyValue::Str("7777".into())));
        assert_eq!(listener.native_channel("fuel_level"), Some(LegacyValue::Int(0)));
        assert_eq!(listener.read_native("speed"), NativeRead::Raised);
        assert_eq!(listener.read_native("car_name"), NativeRead::Raised);
        blocks.fail_read(title.mapping.name, true);
        assert!(!listener.is_running());
        assert_eq!(listener.native_channel("rpm"), Some(LegacyValue::Str("7777".into())));
        let mut zero = image(title, 2);
        zero.f32(title.data.fuel, 0.0).f32(title.data.speedometer, -0.0);
        blocks.fail_read(title.mapping.name, false);
        blocks.set(title.mapping, zero.build());
        assert!(listener.is_running());
        assert_eq!(listener.native_channel("fuel_level"), Some(LegacyValue::Float(0.0)));
        assert_eq!(listener.native_channel("speed"), Some(LegacyValue::Str("0".into())));
    }

    /// The size of each byte-packed `SProxyData_t` layout.
    #[test]
    fn proxy_sizes() {
        assert_eq!(GP_BIKES.mapping.size, 1027);
        assert_eq!(MX_BIKES.mapping.size, 996);
        assert_eq!(KART_RACING_PRO.mapping.size, 1112);
        assert_eq!(WORLD_RACING_SERIES.mapping.size, 1048);
    }

    /// The offsets of one title, in field order: iState, then sEvent's name / iMaxRPM / fMaxFuel / szTrackName,
    /// then sData's iRPM, Gear, fFuel, fSpeedometer, fVelocity, fAcceleration, fYPR.
    fn offsets(t: &PibosoTitle) -> [usize; 12] {
        let (e, d) = (&t.event, &t.data);
        [
            STATE,
            e.vehicle_name,
            e.max_rpm,
            e.max_fuel,
            e.track_name,
            d.rpm,
            d.gear,
            d.fuel,
            d.speedometer,
            d.velocity,
            d.acceleration,
            d.yaw_pitch_roll,
        ]
    }

    /// The byte-packed `SProxyData_t` of each title: offsets of every field read.
    #[test]
    fn proxy_layouts() {
        assert_eq!(offsets(&GP_BIKES), [4, 208, 312, 336, 552, 808, 820, 824, 828, 844, 856, 904]);
        assert_eq!(offsets(&MX_BIKES), [4, 208, 312, 336, 552, 804, 816, 820, 824, 840, 852, 900]);
        assert_eq!(offsets(&KART_RACING_PRO), [4, 208, 316, 344, 648, 908, 920, 924, 928, 944, 956, 1004]);
        assert_eq!(offsets(&WORLD_RACING_SERIES), [4, 208, 312, 336, 544, 804, 824, 828, 832, 848, 860, 908]);
        let temperatures = |t: &PibosoTitle| (t.data.engine_temperature, t.data.water_temperature, t.data.oil_pressure);
        assert_eq!(temperatures(&GP_BIKES), (Some(812), 816, None));
        assert_eq!(temperatures(&MX_BIKES), (Some(808), 812, None));
        assert_eq!(temperatures(&KART_RACING_PRO), (None, 916, None));
        assert_eq!(temperatures(&WORLD_RACING_SERIES), (Some(812), 816, Some(820)));
    }

    /// Liveness: running only while iState == 2; an absent block is not running.
    #[test]
    fn check_status() {
        for title in [&GP_BIKES, &MX_BIKES, &KART_RACING_PRO, &WORLD_RACING_SERIES] {
            assert!(listener(title, 2).is_running().unwrap(), "{}", title.id);
            assert!(!listener(title, 1).is_running().unwrap(), "{}", title.id);
            assert!(!listener(title, 3).is_running().unwrap(), "{}", title.id);
            assert!(!Piboso::with_opener(FakeBlocks::default(), title).is_running().unwrap());
        }
    }

    /// Channels: the shared mapping of the four listeners (×10 accelerations, raw radians, round speed).
    #[test]
    fn get_channel_mapping() {
        for title in [&GP_BIKES, &MX_BIKES, &KART_RACING_PRO, &WORLD_RACING_SERIES] {
            let mut piboso = listener(title, 2);
            piboso.is_running().unwrap();
            let mut t = Telemetry::off();
            piboso.poll(&mut t).unwrap();
            assert_eq!(t.rpm, 9000.0, "{}", title.id);
            assert_eq!(t.max_rpm, 13000.0);
            assert_eq!(t.gear, 2.0);
            assert_eq!(t.speed, 12.0); // round(12.5) ties to even
            assert_eq!(t.location, "Ottobiano");
            assert_eq!(t.vehicle, "KTM 450");
            assert_eq!(t.lateral_acceleration, -2.5);
            assert_eq!(t.vertical_acceleration, -5.0);
            assert_eq!(t.longitudinal_acceleration, 15.0);
            assert_eq!(t.vertical_velocity, 0.5);
            assert_eq!(t.lateral_velocity, 1.0);
            assert_eq!(t.longitudinal_velocity, 12.0);
            assert_eq!(t.yaw, 1.0);
            assert_eq!(t.roll, 0.5);
            assert_eq!(t.pitch, -0.25);
            assert_eq!(t.water_temperature, 70.0);
            assert_eq!(t.fuel_level, 25.0);
        }
    }

    /// Channels: oil temperature for all but Kart Racing Pro, oil pressure only for World Racing Series.
    #[test]
    fn per_title_channels() {
        let expectations = [
            (&GP_BIKES, 95.0, 0.0),
            (&MX_BIKES, 95.0, 0.0),
            (&KART_RACING_PRO, 0.0, 0.0),
            (&WORLD_RACING_SERIES, 95.0, 3.5),
        ];
        for (title, oil_temperature, oil_pressure) in expectations {
            let mut piboso = listener(title, 2);
            piboso.is_running().unwrap();
            let mut t = Telemetry::off();
            piboso.poll(&mut t).unwrap();
            assert_eq!(t.oil_temperature, oil_temperature, "{}", title.id);
            assert_eq!(t.oil_pressure, oil_pressure, "{}", title.id);
        }
    }

    /// Channels: fuel_level is 0 when fMaxFuel is 0; undecodable names.
    #[test]
    fn fuel_and_names_errors() {
        let mut i = image(&GP_BIKES, 2);
        i.f32(GP_BIKES.event.max_fuel, 0.0)
            .bytes(GP_BIKES.event.track_name, b"\xff")
            .bytes(GP_BIKES.event.vehicle_name, b"\xff");
        let pages = FakeBlocks::default();
        pages.set(GP_BIKES.mapping, i.build());
        let mut piboso = Piboso::with_opener(pages, &GP_BIKES);
        piboso.is_running().unwrap();
        let mut t = Telemetry::off();
        t.vehicle = "before".into();
        piboso.poll(&mut t).unwrap();
        assert_eq!(t.fuel_level, 0.0);
        assert_eq!(t.location, ZERO_NAME);
        assert_eq!(t.vehicle, "before");
    }

    /// Channel reads before any successful liveness test (no copied block): implemented channels fail, others answer 0.
    #[test]
    fn no_snapshot() {
        let mut piboso = Piboso::with_opener(FakeBlocks::default(), &MX_BIKES);
        let mut t = Telemetry::off();
        t.rpm = 100.0;
        t.suspension_travel_lf = 0.5;
        piboso.poll(&mut t).unwrap();
        assert_eq!(t.rpm, 100.0);
        assert_eq!(t.suspension_travel_lf, 0.0);
        assert_eq!(t.location, ZERO_NAME);
    }

    /// A liveness test failing after a success keeps the previous block for channel reads.
    #[test]
    fn failed_copy_keeps_previous_snapshot() {
        let mut piboso = listener(&WORLD_RACING_SERIES, 2);
        piboso.is_running().unwrap();
        piboso.opener.fail_read(WORLD_RACING_SERIES.mapping.name, true);
        assert!(!piboso.is_running().unwrap());
        let mut t = Telemetry::off();
        piboso.poll(&mut t).unwrap();
        assert_eq!(t.rpm, 9000.0);
    }
}
