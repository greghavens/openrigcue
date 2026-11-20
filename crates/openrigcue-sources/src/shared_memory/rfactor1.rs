//! rFactor 1 engine games (rFactor, Automobilista, Game Stock Car Extreme, ARCA, ...) through the
//! `SRS_Rfactor1.dll` plugin block, and rFpro through the rF1-style block of its SimHub plugin.
//!
//! Both blocks are a byte-packed `rfShared` under `$rFactorShared$`: rFactor 1 (39 676 bytes) and
//! rFpro (39 647 bytes, without the DRS / push-to-pass / gearbox / boost-map bytes and the weather
//! fields). The two listeners are otherwise identical.

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::block_reader::{BlockOpener, BridgeMirrors, Fields, KeptMapping, MappingSpec};
use super::estimated_wheel_slip::{estimated_wheel_slip, native_estimated_wheel_slip, SlipInputs, WheelSide};
use super::listener_channels::begin_tick;
use super::python_numeric::{decode_utf8, round_6, round_to_int};
use crate::{Source, SourceError};

/// The rFactor 1 plugin block.
pub const RF1_MAPPING: MappingSpec = MappingSpec { name: "$rFactorShared$", size: 39676 };
/// The rFpro block: same name, shorter structure.
pub const RFPRO_MAPPING: MappingSpec = MappingSpec { name: "$rFactorShared$", size: 39647 };

/// Degrees per radian as the listeners use it (`57.2960014343262`).
pub(crate) const RFACTOR_DEG_PER_RAD: f64 = 57.296_001_434_326_2;
/// `terrainName` of a kerb.
pub(crate) const KERB_TERRAIN: &str = "RMBL";
/// `rumble_pitch_*` value on a kerb.
pub(crate) const KERB_RUMBLE: f64 = 40.0;
/// Vehicles examined when looking for the player (the array holds 128).
pub(crate) const PLAYER_SEARCH_LIMIT: usize = 120;

/// Which of the two rF1-style listeners this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RfactorOneVariant {
    /// rFactor 1 (key `rf1`).
    Rf1,
    /// rFpro (key `rfpro`).
    Rfpro,
}

/// Offsets that differ between the two `rfShared` layouts; everything before `wheel` is shared.
struct Layout {
    mapping: MappingSpec,
    wheels: usize,
    game_phase: usize,
    sector_flag: usize,
    start_light: usize,
    num_red_lights: usize,
    vehicles: usize,
}

const WHEEL_SIZE: usize = 67;
const VEHICLE_SIZE: usize = 304;
/// `rfShared.engineMaxRPM` (same offset in both layouts).
const ENGINE_MAX_RPM: usize = 284;
/// `rfVehicleInfo.isPlayer`.
const VEHICLE_IS_PLAYER: usize = 148;

impl RfactorOneVariant {
    fn layout(self) -> Layout {
        match self {
            RfactorOneVariant::Rf1 => Layout {
                mapping: RF1_MAPPING,
                wheels: 328,
                game_phase: 620,
                sector_flag: 622,
                start_light: 625,
                num_red_lights: 626,
                vehicles: 764,
            },
            RfactorOneVariant::Rfpro => Layout {
                mapping: RFPRO_MAPPING,
                wheels: 319,
                game_phase: 611,
                sector_flag: 613,
                start_light: 616,
                num_red_lights: 617,
                vehicles: 735,
            },
        }
    }

    /// Listener key.
    pub fn listener_id(self) -> &'static str {
        match self {
            RfactorOneVariant::Rf1 => "rf1",
            RfactorOneVariant::Rfpro => "rfpro",
        }
    }

    /// The mapping the listener copies.
    pub fn mapping(self) -> MappingSpec {
        self.layout().mapping
    }
}

/// `rfVec3` / `rf2Vec3` as `[x, y, z]`.
pub(crate) type Vec3 = [f64; 3];

/// The `rfVehicleInfo` fields read for the player's car.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Vehicle {
    sector: i8,
    local_vel: Vec3,
    local_accel: Vec3,
    ori_x: Vec3,
    ori_y: Vec3,
    ori_z: Vec3,
    speed: f64,
}

impl Vehicle {
    fn decode(f: &Fields<'_>, base: usize) -> Self {
        let vec3 = |at: usize| [f.f32(base + at), f.f32(base + at + 4), f.f32(base + at + 8)];
        Vehicle {
            sector: f.i8(base + 98),
            local_vel: vec3(216),
            local_accel: vec3(228),
            ori_x: vec3(240),
            ori_y: vec3(252),
            ori_z: vec3(264),
            speed: f.f32(base + 300),
        }
    }
}

/// The `rfWheel` fields read per wheel (index order front-left, front-right, rear-left, rear-right).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Wheel {
    suspension_deflection: f64,
    /// Whether the decoded terrain name is `RMBL`; a name that is not UTF-8 is a failed read, which the
    /// dispatcher turns into the rumble default 0. Keep the failed read separate from a valid road.
    on_kerb: Option<bool>,
}

/// One `rfShared` snapshot reduced to what the listener reads, with the player's vehicle resolved.
#[derive(Debug, Clone, PartialEq)]
struct RfactorOneFrame {
    vehicle_name: Option<String>,
    track_name: Option<String>,
    gear: i32,
    engine_rpm: f64,
    engine_water_temp: f64,
    engine_oil_temp: f64,
    throttle: f64,
    brake: f64,
    clutch: f64,
    steering: f64,
    engine_max_rpm: f64,
    wheels: [Wheel; 4],
    game_phase: i8,
    sector_flag: [i8; 3],
    start_light: i8,
    num_red_lights: i8,
    player: Vehicle,
}

/// Speed and rpm of the previous snapshot, the estimator's reference sample.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct PreviousSample {
    pub(crate) speed: f64,
    pub(crate) rpm: f64,
}

/// Orientation of a body from its rotation-matrix rows, in degrees: `(roll, pitch, yaw)` as the rF1 listener
/// computes them from `oriX`, `oriY`, `oriZ`.
fn rf1_orientation(ori_x: Vec3, ori_y: Vec3, ori_z: Vec3) -> (f64, f64, f64) {
    let roll = -(ori_y[0].atan2((ori_x[0] * ori_x[0] + ori_z[0] * ori_z[0]).sqrt())) * RFACTOR_DEG_PER_RAD;
    let (nxz, nzz) = (-ori_x[2], -ori_z[2]);
    let pitch = (-ori_y[2]).atan2((nxz * nxz + nzz * nzz).sqrt()) * RFACTOR_DEG_PER_RAD;
    let yaw = -((-ori_x[2]).atan2(ori_z[2])) * RFACTOR_DEG_PER_RAD;
    (roll, pitch, yaw)
}

impl RfactorOneFrame {
    fn decode(bytes: &[u8], layout: &Layout, player_index: usize) -> Self {
        let f = Fields(bytes);
        let wheel = |i: usize| {
            let base = layout.wheels + i * WHEEL_SIZE;
            Wheel {
                suspension_deflection: f.f32(base + 4),
                on_kerb: decode_utf8(f.c_chars(base + 48, 16)).map(|name| name == KERB_TERRAIN),
            }
        };
        RfactorOneFrame {
            vehicle_name: decode_utf8(f.c_chars(12, 64)),
            track_name: decode_utf8(f.c_chars(76, 64)),
            gear: f.i32(240),
            engine_rpm: f.f32(244),
            engine_water_temp: f.f32(248),
            engine_oil_temp: f.f32(252),
            throttle: f.f32(260),
            brake: f.f32(264),
            clutch: f.f32(272),
            steering: f.f32(268),
            engine_max_rpm: f.f32(ENGINE_MAX_RPM),
            wheels: [wheel(0), wheel(1), wheel(2), wheel(3)],
            game_phase: f.i8(layout.game_phase),
            sector_flag: [f.i8(layout.sector_flag), f.i8(layout.sector_flag + 1), f.i8(layout.sector_flag + 2)],
            start_light: f.i8(layout.start_light),
            num_red_lights: f.i8(layout.num_red_lights),
            player: Vehicle::decode(&f, layout.vehicles + player_index * VEHICLE_SIZE),
        }
    }

    /// The flag chain (first match wins).
    fn flag(&self) -> f64 {
        let phase = self.game_phase;
        let code = if phase == 8 {
            11
        } else if self.sector_flag.contains(&self.player.sector) {
            6
        } else if phase == 5 && self.start_light > self.num_red_lights {
            22
        } else if phase == 4 {
            20
        } else if phase == 3 || phase == 6 {
            21
        } else {
            0
        };
        f64::from(code)
    }

    /// Writes the channels (identical for both variants).
    fn write(&self, t: &mut Telemetry, previous_sample: PreviousSample) {
        let previous = begin_tick(t);
        let v = &self.player;
        let speed = round_to_int(v.speed);
        let rpm = round_to_int(self.engine_rpm);
        let lateral_velocity = round_6(-v.local_vel[0]);
        let (roll, pitch, yaw) = rf1_orientation(v.ori_x, v.ori_y, v.ori_z);
        let [w_fl, w_fr, w_rl, w_rr] = self.wheels;
        let suspension = |w: Wheel| round_6(w.suspension_deflection / 10.0);
        let rumble = |w: Wheel| if w.on_kerb == Some(true) { KERB_RUMBLE } else { 0.0 };
        let slip = |side: WheelSide| {
            Some(estimated_wheel_slip(
                side,
                &SlipInputs {
                    lateral_velocity,
                    speed: speed?,
                    rpm: rpm?,
                    gear: i64::from(self.gear),
                    previous_speed: previous_sample.speed,
                    previous_rpm: previous_sample.rpm,
                    throttle: self.throttle,
                    brake: self.brake,
                },
            ))
        };

        t.rpm = rpm.unwrap_or(previous.rpm);
        t.flag = self.flag();
        t.raw_flag = f64::from(self.game_phase);
        t.max_rpm = round_to_int(self.engine_max_rpm).unwrap_or(previous.max_rpm);
        t.gear = f64::from(self.gear);
        t.speed = speed.unwrap_or(previous.speed);
        t.steering_wheel_angle = -self.steering;
        t.longitudinal_acceleration = round_6(-v.local_accel[2]);
        t.vertical_acceleration = round_6(v.local_accel[1]);
        t.lateral_acceleration = round_6(v.local_accel[0]);
        t.vertical_velocity = round_6(v.local_vel[1]);
        t.longitudinal_velocity = round_6(v.local_vel[2]);
        t.lateral_velocity = lateral_velocity;
        t.roll = roll;
        t.pitch = pitch;
        t.yaw = yaw;
        let left = slip(WheelSide::Left);
        let right = slip(WheelSide::Right);
        t.wheel_slip_lf = left.unwrap_or(previous.wheel_slip_lf);
        t.wheel_slip_lr = left.unwrap_or(previous.wheel_slip_lr);
        t.wheel_slip_rf = right.unwrap_or(previous.wheel_slip_rf);
        t.wheel_slip_rr = right.unwrap_or(previous.wheel_slip_rr);
        t.suspension_travel_lf = suspension(w_fl);
        t.suspension_travel_lr = suspension(w_rl);
        t.suspension_travel_rf = suspension(w_fr);
        t.suspension_travel_rr = suspension(w_rr);
        t.rumble_pitch_lf = rumble(w_fl);
        t.rumble_pitch_lr = rumble(w_rl);
        t.rumble_pitch_rf = rumble(w_fr);
        t.rumble_pitch_rr = rumble(w_rr);
        t.vehicle = self.vehicle_name.clone().unwrap_or(previous.vehicle);
        // A track name that is not UTF-8 makes `track_loc` fail; the dispatcher then stores 0 (empty here).
        t.location = self.track_name.clone().unwrap_or_default();
        t.water_temperature = self.engine_water_temp;
        t.oil_temperature = self.engine_oil_temp;
    }
}

/// The rF1-engine listener (rFactor 1 or rFpro).
pub struct RfactorOne {
    variant: RfactorOneVariant,
    opener: Box<dyn BlockOpener>,
    /// The kept mapping object.
    handle: KeptMapping,
    frame: Option<RfactorOneFrame>,
    player_index: usize,
    previous: PreviousSample,
}

impl RfactorOne {
    /// Listener reading the bridge mirror of the variant's mapping.
    pub fn new(variant: RfactorOneVariant) -> Self {
        Self::with_opener(variant, Box::new(BridgeMirrors))
    }

    /// Listener reading mappings through `opener`.
    pub fn with_opener(variant: RfactorOneVariant, opener: Box<dyn BlockOpener>) -> Self {
        RfactorOne {
            variant,
            opener,
            handle: KeptMapping::new(variant.layout().mapping),
            frame: None,
            player_index: 0,
            previous: PreviousSample::default(),
        }
    }

    /// The liveness test: keep the previous snapshot's player speed and rpm as the
    /// slip reference, make a new mapping object on every call (create-or-open, replacing the kept object)
    /// and a copy, require `engineMaxRPM > 0` (otherwise the object and the snapshot are dropped), and select the first
    /// vehicle among the first 120 flagged `isPlayer` (the index is kept when none is).
    fn refresh(&mut self) -> bool {
        self.previous = self
            .frame
            .as_ref()
            .map(|frame| PreviousSample { speed: frame.player.speed, rpm: frame.engine_rpm })
            .unwrap_or_default();
        let layout = self.variant.layout();
        let Ok(bytes) = self.handle.remake(self.opener.as_mut()) else { return false };
        let fields = Fields(&bytes);
        if fields.f32(ENGINE_MAX_RPM) <= 0.0 {
            self.handle.release();
            self.frame = None;
            return false;
        }
        let is_player = |i: usize| fields.bool(layout.vehicles + i * VEHICLE_SIZE + VEHICLE_IS_PLAYER);
        if let Some(index) = (0..PLAYER_SEARCH_LIMIT).find(|&i| is_player(i)) {
            self.player_index = index;
        }
        self.frame = Some(RfactorOneFrame::decode(&bytes, &layout, self.player_index));
        true
    }
}

impl Source for RfactorOne {
    fn id(&self) -> &'static str {
        self.variant.listener_id()
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        use LegacyValue::{Float, Int, Str};
        let f = self.frame.as_ref();
        let value = match name {
            "rpm" => return round_to_int(f?.engine_rpm).and_then(crate::udp_racing::native_int),
            "max_rpm" => return round_to_int(f?.engine_max_rpm).and_then(crate::udp_racing::native_int),
            "gear" => return Some(Int(i64::from(f?.gear))),
            "speed" => return round_to_int(f?.player.speed).and_then(crate::udp_racing::native_int),
            "car_name" => return f?.vehicle_name.clone().map(Str),
            "track_loc" => return f?.track_name.clone().map(Str),
            "flag" => return Some(Int(f?.flag() as i64)),
            "raw_flag" => return Some(Int(i64::from(f?.game_phase))),
            "throttle" => f?.throttle,
            "brake" => f?.brake,
            "clutch" => f?.clutch,
            "steering_wheel_angle" => -f?.steering,
            "lateral_acceleration" => round_6(f?.player.local_accel[0]),
            "longitudinal_acceleration" => round_6(-f?.player.local_accel[2]),
            "vertical_acceleration" => round_6(f?.player.local_accel[1]),
            "lateral_velocity" => round_6(-f?.player.local_vel[0]),
            "longitudinal_velocity" => round_6(f?.player.local_vel[2]),
            "vertical_velocity" => round_6(f?.player.local_vel[1]),
            "roll" | "pitch" | "yaw" => {
                let p = &f?.player;
                let (roll, pitch, yaw) = rf1_orientation(p.ori_x, p.ori_y, p.ori_z);
                match name {
                    "roll" => roll,
                    "pitch" => pitch,
                    _ => yaw,
                }
            }
            "wheel_slip_lf" | "wheel_slip_lr" | "wheel_slip_rf" | "wheel_slip_rr" => {
                let f = f?;
                let side =
                    if matches!(name, "wheel_slip_lf" | "wheel_slip_lr") { WheelSide::Left } else { WheelSide::Right };
                return Some(native_estimated_wheel_slip(
                    side,
                    &SlipInputs {
                        lateral_velocity: round_6(-f.player.local_vel[0]),
                        speed: round_to_int(f.player.speed)?,
                        rpm: round_to_int(f.engine_rpm)?,
                        gear: i64::from(f.gear),
                        previous_speed: self.previous.speed,
                        previous_rpm: self.previous.rpm,
                        throttle: f.throttle,
                        brake: f.brake,
                    },
                ));
            }
            "suspension_travel_lf" | "suspension_travel_lr" | "suspension_travel_rf" | "suspension_travel_rr" => {
                let i = match name {
                    "suspension_travel_lf" => 0,
                    "suspension_travel_rf" => 1,
                    "suspension_travel_lr" => 2,
                    _ => 3,
                };
                round_6(f?.wheels[i].suspension_deflection / 10.0)
            }
            "rumble_pitch_lf" | "rumble_pitch_lr" | "rumble_pitch_rf" | "rumble_pitch_rr" => {
                let i = match name {
                    "rumble_pitch_lf" => 0,
                    "rumble_pitch_rf" => 1,
                    "rumble_pitch_lr" => 2,
                    _ => 3,
                };
                return Some(Int(if f?.wheels[i].on_kerb? { 40 } else { 0 }));
            }
            "oil_temperature" => f?.engine_oil_temp,
            "water_temperature" => f?.engine_water_temp,
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
            frame.write(telemetry, self.previous);
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared_memory::block_reader::fake::{BlockBuilder, FakeBlocks};

    /// Player at index `player` of a block in the variant's layout.
    fn racing_block(variant: RfactorOneVariant, player: usize) -> BlockBuilder {
        let l = variant.layout();
        let mut b = BlockBuilder::new(l.mapping.size);
        b.bytes(12, b"Formula ISI #12\0")
            .bytes(76, b"Mills Short\0")
            .i32(240, 3) // gear
            .f32(244, 7000.5) // engineRPM
            .f32(248, 85.0) // engineWaterTemp
            .f32(252, 110.0) // engineOilTemp
            .f32(260, 1.0) // unfilteredThrottle
            .f32(264, 0.0) // unfilteredBrake
            .f32(268, 0.25) // unfilteredSteering
            .f32(284, 18000.0); // engineMaxRPM
        for (i, (defl, terrain)) in
            [(0.1f32, &b"RMBL\0"[..]), (0.2, b"ROAD\0"), (0.3, b"RMBL\0"), (0.4, b"\xff\0")].into_iter().enumerate()
        {
            let w = l.wheels + i * WHEEL_SIZE;
            b.f32(w + 4, defl).bytes(w + 48, terrain);
        }
        b.i8(l.game_phase, 5).i8(l.sector_flag, 0).i8(l.sector_flag + 1, 0).i8(l.sector_flag + 2, 0);
        b.i8(l.start_light, 0).i8(l.num_red_lights, 0);
        let v = l.vehicles + player * VEHICLE_SIZE;
        b.i8(v + 98, 1) // sector
            .u8(v + 148, 1) // isPlayer
            .f32(v + 216, 0.75) // localVel.x
            .f32(v + 220, -0.5) // localVel.y
            .f32(v + 224, -40.0) // localVel.z
            .f32(v + 228, 1.0) // localAccel.x
            .f32(v + 232, 2.0) // localAccel.y
            .f32(v + 236, 3.0) // localAccel.z
            .f32(v + 240, 1.0) // oriX = (1, 0, 0.1)
            .f32(v + 248, 0.1)
            .f32(v + 252, 0.05) // oriY = (0.05, 1, 0.02)
            .f32(v + 256, 1.0)
            .f32(v + 260, 0.02)
            .f32(v + 264, -0.1) // oriZ = (-0.1, 0, 1)
            .f32(v + 272, 1.0)
            .f32(v + 300, 40.5); // speed
        b
    }

    fn listener(blocks: &FakeBlocks, variant: RfactorOneVariant) -> RfactorOne {
        RfactorOne::with_opener(variant, Box::new(blocks.clone()))
    }

    #[test]
    fn native_getters_preserve_both_variants_and_failed_reads() {
        use LegacyValue::{Float, Int, Str};
        for variant in [RfactorOneVariant::Rf1, RfactorOneVariant::Rfpro] {
            let blocks = FakeBlocks::default();
            let mut source = listener(&blocks, variant);
            assert!(source.poll(&mut Telemetry::off()).unwrap());
            assert_eq!(source.native_channel("unknown"), Some(Int(0)));
            assert!(source.channel_read_raised("rpm"));
            blocks.set(variant.mapping(), racing_block(variant, 0).build());
            assert!(source.is_running().unwrap());
            for (name, expected) in [
                ("rpm", Int(7000)),
                ("max_rpm", Int(18000)),
                ("gear", Int(3)),
                ("speed", Int(40)),
                ("car_name", Str("Formula ISI #12".into())),
                ("track_loc", Str("Mills Short".into())),
                ("raw_flag", Int(5)),
                ("flag", Int(0)),
                ("throttle", Float(1.0)),
                ("brake", Float(0.0)),
                ("clutch", Float(0.0)),
                ("oil_temperature", Float(110.0)),
                ("rumble_pitch_lf", Int(40)),
                ("rumble_pitch_rf", Int(0)),
                ("wheel_slip_lf", Int(0)),
                ("unknown", Int(0)),
            ] {
                assert_eq!(source.native_channel(name), Some(expected), "{variant:?} {name}");
            }
            assert!(source.channel_read_raised("rumble_pitch_rr"), "invalid UTF-8 remains raised");
            assert!(source.is_running().unwrap());
            assert!(matches!(source.native_channel("wheel_slip_lf"), Some(Float(_))));
            let f = source.frame.as_mut().unwrap();
            f.engine_rpm = f64::NAN;
            f.engine_max_rpm = f64::INFINITY;
            f.vehicle_name = None;
            f.track_name = None;
            for name in ["rpm", "max_rpm", "wheel_slip_lf", "car_name", "track_loc"] {
                assert!(source.channel_read_raised(name), "{name}");
            }
            source.frame.as_mut().unwrap().player.speed = -40.5;
            assert_eq!(source.native_channel("speed"), Some(Int(-40)), "no negative-speed clamp");
            source.frame.as_mut().unwrap().player.speed = f64::NAN;
            assert!(source.channel_read_raised("speed"));
        }
    }

    #[test]
    fn mapping_constants_and_degree_literal() {
        assert_eq!(RF1_MAPPING.name, "$rFactorShared$");
        assert_eq!((RF1_MAPPING.size, RFPRO_MAPPING.size), (39676, 39647));
        assert_eq!(RFACTOR_DEG_PER_RAD.to_bits(), 0x404c_a5e3_6000_0004);
        assert_eq!(RfactorOneVariant::Rfpro.listener_id(), "rfpro");
    }

    fn check_channels(variant: RfactorOneVariant) {
        let blocks = FakeBlocks::default();
        blocks.set(variant.mapping(), racing_block(variant, 7).build());
        let mut l = listener(&blocks, variant);
        assert!(l.is_running().unwrap());
        let mut t = Telemetry::off();
        assert!(l.poll(&mut t).unwrap());
        assert_eq!(t.rpm, 7000.0);
        assert_eq!(t.max_rpm, 18000.0);
        assert_eq!(t.gear, 3.0);
        assert_eq!(t.speed, 40.0);
        assert_eq!(t.raw_flag, 5.0);
        assert_eq!(t.flag, 0.0);
        assert_eq!(t.steering_wheel_angle, -0.25);
        assert_eq!(t.longitudinal_acceleration, -3.0);
        assert_eq!(t.vertical_acceleration, 2.0);
        assert_eq!(t.lateral_acceleration, 1.0);
        assert_eq!(t.vertical_velocity, -0.5);
        assert_eq!(t.longitudinal_velocity, -40.0);
        assert_eq!(t.lateral_velocity, -0.75);
        let (ox, oy, oz) = (
            [1.0, 0.0, f64::from(0.1f32)],
            [f64::from(0.05f32), 1.0, f64::from(0.02f32)],
            [f64::from(-0.1f32), 0.0, 1.0],
        );
        let k = RFACTOR_DEG_PER_RAD;
        assert_eq!(t.roll, -(oy[0].atan2((ox[0] * ox[0] + oz[0] * oz[0]).sqrt())) * k);
        assert_eq!(t.pitch, (-oy[2]).atan2(((-ox[2]) * (-ox[2]) + (-oz[2]) * (-oz[2])).sqrt()) * k);
        assert_eq!(t.yaw, -((-ox[2]).atan2(oz[2])) * k);
        // lf->wheel 0, rf->1, lr->2, rr->3; deflection / 10.
        assert_eq!(t.suspension_travel_lf, round_6(f64::from(0.1f32) / 10.0));
        assert_eq!(t.suspension_travel_rf, round_6(f64::from(0.2f32) / 10.0));
        assert_eq!(t.suspension_travel_lr, round_6(f64::from(0.3f32) / 10.0));
        assert_eq!(t.suspension_travel_rr, round_6(f64::from(0.4f32) / 10.0));
        assert_eq!(
            [t.rumble_pitch_lf, t.rumble_pitch_rf, t.rumble_pitch_lr, t.rumble_pitch_rr],
            [40.0, 0.0, 40.0, 0.0]
        );
        assert_eq!(t.vehicle, "Formula ISI #12");
        assert_eq!(t.location, "Mills Short");
        assert_eq!(t.water_temperature, 85.0);
        assert_eq!(t.oil_temperature, 110.0);
        // First snapshot: previous sample is 0, so the estimator returns 0.
        assert_eq!(t.wheel_slip_lf, 0.0);
    }

    #[test]
    fn get_channel_maps_the_rf1_block() {
        check_channels(RfactorOneVariant::Rf1);
    }

    #[test]
    fn get_channel_maps_the_rfpro_block() {
        check_channels(RfactorOneVariant::Rfpro);
    }

    #[test]
    fn get_channel_flag_chain() {
        let v = RfactorOneVariant::Rf1;
        let l = v.layout();
        let flag_of = |edit: &dyn Fn(&mut BlockBuilder)| {
            let blocks = FakeBlocks::default();
            let mut b = racing_block(v, 0);
            edit(&mut b);
            blocks.set(RF1_MAPPING, b.build());
            let mut s = listener(&blocks, v);
            assert!(s.is_running().unwrap());
            let mut t = Telemetry::off();
            s.poll(&mut t).unwrap();
            t.flag
        };
        assert_eq!(
            flag_of(&|b| {
                b.i8(l.game_phase, 8).i8(l.sector_flag, 1);
            }),
            11.0
        );
        assert_eq!(
            flag_of(&|b| {
                b.i8(l.sector_flag + 2, 1);
            }),
            6.0,
            "sector flag equal to the player's sector"
        );
        assert_eq!(
            flag_of(&|b| {
                b.i8(l.start_light, 3).i8(l.num_red_lights, 2);
            }),
            22.0
        );
        assert_eq!(
            flag_of(&|b| {
                b.i8(l.start_light, 2).i8(l.num_red_lights, 2);
            }),
            0.0
        );
        assert_eq!(
            flag_of(&|b| {
                b.i8(l.game_phase, 4);
            }),
            20.0
        );
        assert_eq!(
            flag_of(&|b| {
                b.i8(l.game_phase, 3);
            }),
            21.0
        );
        assert_eq!(
            flag_of(&|b| {
                b.i8(l.game_phase, 6);
            }),
            21.0
        );
        assert_eq!(
            flag_of(&|b| {
                b.i8(l.game_phase, 7);
            }),
            0.0
        );
    }

    #[test]
    fn check_status_player_search_and_liveness() {
        let v = RfactorOneVariant::Rf1;
        let l = v.layout();
        let blocks = FakeBlocks::default();
        let mut s = listener(&blocks, v);
        assert!(!s.is_running().unwrap(), "no mapping");
        // Two players flagged: the first one (index 3) wins.
        let mut b = racing_block(v, 9);
        b.u8(l.vehicles + 3 * VEHICLE_SIZE + 148, 1).f32(l.vehicles + 3 * VEHICLE_SIZE + 300, 12.0);
        blocks.set(RF1_MAPPING, b.build());
        assert!(s.is_running().unwrap());
        assert_eq!(s.player_index, 3);
        // A player beyond index 119 is never found: the index is kept.
        let mut b = racing_block(v, 125);
        b.f32(l.vehicles + 3 * VEHICLE_SIZE + 300, 12.0);
        blocks.set(RF1_MAPPING, b.build());
        assert!(s.is_running().unwrap());
        assert_eq!(s.player_index, 3);
        // engineMaxRPM <= 0: not running, snapshot dropped.
        let mut b = racing_block(v, 0);
        b.f32(284, 0.0);
        blocks.set(RF1_MAPPING, b.build());
        assert!(!s.is_running().unwrap());
        assert!(s.frame.is_none());
    }

    #[test]
    fn check_status_previous_sample_feeds_wheel_slip() {
        let v = RfactorOneVariant::Rf1;
        let blocks = FakeBlocks::default();
        let mut s = listener(&blocks, v);
        let mut first = racing_block(v, 0);
        first.f32(244, 5000.0).f32(v.layout().vehicles + 300, 30.0);
        blocks.set(RF1_MAPPING, first.build());
        assert!(s.is_running().unwrap());
        // Second snapshot: speed 40.5 -> round 40, rpm 7000.5 -> 7000; throttle 1.0.
        blocks.set(RF1_MAPPING, racing_block(v, 0).build());
        assert!(s.is_running().unwrap());
        assert_eq!(s.previous, PreviousSample { speed: 30.0, rpm: 5000.0 });
        let mut t = Telemetry::off();
        s.poll(&mut t).unwrap();
        let inputs = SlipInputs {
            lateral_velocity: -0.75,
            speed: 40.0,
            rpm: 7000.0,
            gear: 3,
            previous_speed: 30.0,
            previous_rpm: 5000.0,
            throttle: 1.0,
            brake: 0.0,
        };
        assert_eq!(t.wheel_slip_lf, estimated_wheel_slip(WheelSide::Left, &inputs));
        assert_eq!(t.wheel_slip_lr, t.wheel_slip_lf);
        assert_eq!(t.wheel_slip_rr, estimated_wheel_slip(WheelSide::Right, &inputs));
        assert!(t.wheel_slip_rf > 0.0);
    }

    #[test]
    fn get_channel_invalid_names() {
        let v = RfactorOneVariant::Rf1;
        let blocks = FakeBlocks::default();
        let mut b = racing_block(v, 0);
        b.bytes(12, b"\xff\0").bytes(76, b"\xfe\0");
        blocks.set(RF1_MAPPING, b.build());
        let mut s = listener(&blocks, v);
        assert!(s.is_running().unwrap());
        let mut t = Telemetry::off();
        t.vehicle = "old car".into();
        t.location = "old track".into();
        s.poll(&mut t).unwrap();
        assert_eq!(t.vehicle, "old car", "car_name raising keeps the dispatcher's value");
        assert_eq!(t.location, "", "track_loc raising stores 0");
    }
}
