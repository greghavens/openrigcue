//! rFactor 2 and Le Mans Ultimate through The Iron Wolf's rF2 Shared Memory Map Plugin
//! (4-byte packing): the telemetry block (241 680 bytes) and the scoring block (75 312 bytes).

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::block_reader::{BlockOpener, BridgeMirrors, Fields, KeptMapping, MappingSpec};
use super::estimated_wheel_slip::{estimated_wheel_slip, native_estimated_wheel_slip, SlipInputs, WheelSide};
use super::listener_channels::begin_tick;
use super::python_numeric::{decode_utf8, percent_or_zero, round_6, round_to_int};
use super::rfactor1::{PreviousSample, Vec3, KERB_RUMBLE, KERB_TERRAIN, PLAYER_SEARCH_LIMIT, RFACTOR_DEG_PER_RAD};
use crate::{Source, SourceError};

/// The plugin's telemetry block.
pub const RF2_TELEMETRY_MAPPING: MappingSpec = MappingSpec { name: "$rFactor2SMMP_Telemetry$", size: 241680 };
/// The plugin's scoring block.
pub const RF2_SCORING_MAPPING: MappingSpec = MappingSpec { name: "$rFactor2SMMP_Scoring$", size: 75312 };

/// Vehicle slots in both blocks; index -1 ("player not found") is the last one.
const VEHICLE_SLOTS: usize = 128;
/// Byte offset of `rf2Telemetry.mVehicles` (the first `rf2VehicleTelemetry`).
const TELEMETRY_VEHICLES_OFFSET: usize = 16;
/// Byte size of one `rf2VehicleTelemetry`.
const TELEMETRY_VEHICLE_SIZE: usize = 1888;
/// Byte offset of the first wheel within `rf2VehicleTelemetry`.
const WHEEL_BASE: usize = 848;
/// Byte size of one wheel record.
const WHEEL_SIZE: usize = 260;
/// Byte offset of `rF2Scoring.scoringInfo`.
const SCORING_INFO_OFFSET: usize = 12;
/// Byte offset of `rF2Scoring.vehicles` (the first `rF2VehicleScoring`).
const SCORING_VEHICLES_OFFSET: usize = 560;
/// Byte size of one `rF2VehicleScoring`.
const SCORING_VEHICLE_SIZE: usize = 584;
/// `rF2VehicleScoring.flag` value of a blue flag.
const BLUE_FLAG: u8 = 6;
/// `rf2VehicleTelemetry.engineMaxRPM`.
const ENGINE_MAX_RPM: usize = 532;
/// `rF2VehicleScoring.isPlayer`.
const IS_PLAYER: usize = 196;

/// The index for a search result: the found slot, or `-1`, which addresses the last slot.
fn python_index(found: Option<usize>) -> usize {
    found.unwrap_or(VEHICLE_SLOTS - 1)
}

/// The `rf2VehicleTelemetry` fields the listener reads. Wheels in rF2 order FL, FR, RL, RR.
#[derive(Debug, Clone, PartialEq)]
struct VehicleTelemetry {
    vehicle_name: Option<String>,
    local_vel: Vec3,
    local_accel: Vec3,
    ori: [Vec3; 3],
    gear: i32,
    engine_rpm: f64,
    engine_water_temp: f64,
    engine_oil_temp: f64,
    throttle: f64,
    brake: f64,
    clutch: f64,
    steering: f64,
    fuel: f64,
    engine_max_rpm: f64,
    fuel_capacity: f64,
    suspension_deflection: [f64; 4],
    /// Whether the decoded terrain name is `RMBL`, per wheel; None keeps a failed decode separate from valid road.
    on_kerb: [Option<bool>; 4],
}

fn telemetry_vehicle_base(index: usize) -> usize {
    TELEMETRY_VEHICLES_OFFSET + index * TELEMETRY_VEHICLE_SIZE
}

fn speed_of(local_vel: Vec3) -> f64 {
    let [x, y, z] = local_vel;
    (x * x + y * y + z * z).sqrt()
}

impl VehicleTelemetry {
    fn decode(block: &[u8], index: usize) -> Self {
        let f = Fields(block);
        let base = telemetry_vehicle_base(index);
        let vec3 = |at: usize| [f.f64(base + at), f.f64(base + at + 8), f.f64(base + at + 16)];
        let wheel = |i: usize| base + WHEEL_BASE + i * WHEEL_SIZE;
        let on_kerb = |i: usize| decode_utf8(f.c_chars(wheel(i) + 160, 16)).map(|name| name == KERB_TERRAIN);
        VehicleTelemetry {
            vehicle_name: decode_utf8(f.c_chars(base + 32, 64)),
            local_vel: vec3(184),
            local_accel: vec3(208),
            ori: [vec3(232), vec3(256), vec3(280)],
            gear: f.i32(base + 352),
            engine_rpm: f.f64(base + 356),
            engine_water_temp: f.f64(base + 364),
            engine_oil_temp: f.f64(base + 372),
            throttle: f.f64(base + 388),
            brake: f.f64(base + 396),
            clutch: f.f64(base + 412),
            steering: f.f64(base + 404),
            fuel: f.f64(base + 524),
            engine_max_rpm: f.f64(base + ENGINE_MAX_RPM),
            fuel_capacity: f.f64(base + 608),
            suspension_deflection: [0, 1, 2, 3].map(|i| f.f64(wheel(i))),
            on_kerb: [0, 1, 2, 3].map(on_kerb),
        }
    }
}

/// The `rF2ScoringInfo` fields the listener reads.
#[derive(Debug, Clone, PartialEq)]
struct ScoringInfo {
    track_name: Option<String>,
    game_phase: u8,
    sector_flag: [i8; 3],
    start_light: u8,
    num_red_lights: u8,
}

/// The `rF2VehicleScoring` fields the listener reads for the player's entry.
#[derive(Debug, Clone, Copy, PartialEq)]
struct VehicleScoring {
    sector: i8,
    control: i8,
    flag: u8,
}

fn scoring_vehicle_base(index: usize) -> usize {
    SCORING_VEHICLES_OFFSET + index * SCORING_VEHICLE_SIZE
}

impl ScoringInfo {
    fn decode(block: &[u8]) -> Self {
        let f = Fields(block);
        let at = |offset: usize| SCORING_INFO_OFFSET + offset;
        ScoringInfo {
            track_name: decode_utf8(f.c_chars(at(0), 64)),
            game_phase: f.u8(at(108)),
            sector_flag: [f.i8(at(110)), f.i8(at(111)), f.i8(at(112))],
            start_light: f.u8(at(113)),
            num_red_lights: f.u8(at(114)),
        }
    }
}

impl VehicleScoring {
    fn decode(block: &[u8], index: usize) -> Self {
        let f = Fields(block);
        let base = scoring_vehicle_base(index);
        VehicleScoring { sector: f.i8(base + 102), control: f.i8(base + 197), flag: f.u8(base + 504) }
    }
}

/// The player's scoring slot: the first of the first 120 scoring entries flagged
/// `isPlayer`.
fn find_player_scoring(scoring: &[u8]) -> Option<usize> {
    let f = Fields(scoring);
    (0..PLAYER_SEARCH_LIMIT).find(|&i| f.bool(scoring_vehicle_base(i) + IS_PLAYER))
}

/// The player's telemetry slot: the slot (among the first 120) whose `id` equals
/// the id of a scoring entry flagged `isPlayer`, trying player entries in order.
fn find_player_telemetry(telemetry: &[u8], scoring: &[u8]) -> Option<usize> {
    let (t, s) = (Fields(telemetry), Fields(scoring));
    (0..PLAYER_SEARCH_LIMIT).filter(|&i| s.bool(scoring_vehicle_base(i) + IS_PLAYER)).find_map(|i| {
        let id = s.i32(scoring_vehicle_base(i));
        (0..PLAYER_SEARCH_LIMIT).find(|&j| t.i32(telemetry_vehicle_base(j)) == id)
    })
}

/// `car_name` as the listener shapes it: cut one character before the `#` of a "Name #12" suffix
/// (so a leading `#` drops the last character instead).
fn car_name_without_number(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let Some(hash) = chars.iter().position(|&c| c == '#') else { return name.to_owned() };
    let end = if hash == 0 { chars.len() - 1 } else { hash - 1 };
    chars[..end].iter().collect()
}

/// `(roll, pitch, yaw)` in degrees from the `ori` rows, as the rF2 listener computes them.
fn rf2_orientation(ori: [Vec3; 3]) -> (f64, f64, f64) {
    let [ox, oy, oz] = ori;
    let roll = oy[0].atan2((ox[0] * ox[0] + oz[0] * oz[0]).sqrt()) * RFACTOR_DEG_PER_RAD;
    let pitch = (-oy[2]).atan2((ox[2] * ox[2] + oz[2] * oz[2]).sqrt()) * RFACTOR_DEG_PER_RAD;
    let yaw = -((-ox[2]).atan2(oz[2])) * RFACTOR_DEG_PER_RAD;
    (roll, pitch, yaw)
}

/// What one processed tick reads: the player's telemetry and scoring entries plus the session info.
struct Rf2Frame {
    car: VehicleTelemetry,
    entry: VehicleScoring,
    info: ScoringInfo,
}

impl Rf2Frame {
    /// The flag chain (first match wins).
    fn flag(&self) -> f64 {
        rf2_flag(&self.info, &self.entry)
    }

    /// Writes the channels.
    fn write(&self, t: &mut Telemetry, previous_sample: PreviousSample, pause_when_not_playing: bool) {
        let previous = begin_tick(t);
        // While the player's car is not under local control (AI, remote, replay) every channel reads 0.
        if pause_when_not_playing && self.entry.control != 0 {
            return;
        }
        let c = &self.car;
        let speed = speed_of(c.local_vel);
        let rpm = round_to_int(c.engine_rpm);
        let lateral_velocity = round_6(-c.local_vel[0]);
        let (roll, pitch, yaw) = rf2_orientation(c.ori);
        let [defl_fl, defl_fr, defl_rl, defl_rr] = c.suspension_deflection;
        let rumble = |on_kerb: Option<bool>| if on_kerb == Some(true) { KERB_RUMBLE } else { 0.0 };
        let slip = |side: WheelSide| {
            Some(estimated_wheel_slip(
                side,
                &SlipInputs {
                    lateral_velocity,
                    speed,
                    rpm: rpm?,
                    gear: i64::from(c.gear),
                    previous_speed: previous_sample.speed,
                    previous_rpm: previous_sample.rpm,
                    throttle: c.throttle,
                    brake: c.brake,
                },
            ))
        };

        t.rpm = rpm.unwrap_or(previous.rpm);
        t.flag = self.flag();
        t.raw_flag = f64::from(self.info.game_phase);
        t.max_rpm = round_to_int(c.engine_max_rpm).unwrap_or(previous.max_rpm);
        t.gear = f64::from(c.gear);
        t.speed = speed;
        t.steering_wheel_angle = -c.steering;
        t.longitudinal_acceleration = round_6(-c.local_accel[2]);
        t.lateral_acceleration = round_6(c.local_accel[0]);
        t.vertical_acceleration = round_6(c.local_accel[1]);
        t.vertical_velocity = round_6(c.local_vel[1]);
        t.longitudinal_velocity = round_6(c.local_vel[2]);
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
        t.suspension_travel_lf = round_6(defl_fl);
        t.suspension_travel_rf = round_6(defl_fr);
        t.suspension_travel_lr = round_6(defl_rl);
        t.suspension_travel_rr = round_6(defl_rr);
        // lr reads wheel 1 (front-right) and rf reads wheel 2 (rear-left).
        let [kerb_0, kerb_1, kerb_2, kerb_3] = c.on_kerb;
        t.rumble_pitch_lf = rumble(kerb_0);
        t.rumble_pitch_lr = rumble(kerb_1);
        t.rumble_pitch_rf = rumble(kerb_2);
        t.rumble_pitch_rr = rumble(kerb_3);
        // A `car_name` that cannot be decoded answers 0, so a non-UTF-8 name empties the vehicle.
        t.vehicle = c.vehicle_name.as_deref().map(car_name_without_number).unwrap_or_default();
        // A track name that is not UTF-8 makes `track_loc` fail; the dispatcher then stores 0 (empty here).
        t.location = self.info.track_name.clone().unwrap_or_default();
        t.oil_temperature = c.engine_oil_temp;
        t.water_temperature = c.engine_water_temp;
        t.fuel_level = percent_or_zero(c.fuel, c.fuel_capacity);
    }
}

fn rf2_flag(info: &ScoringInfo, entry: &VehicleScoring) -> f64 {
    let phase = info.game_phase;
    let code = if phase == 8 {
        11
    } else if info.sector_flag.contains(&entry.sector) {
        6
    } else if phase == 4 {
        20
    } else if phase == 3 || phase == 6 {
        21
    } else if entry.flag == BLUE_FLAG {
        2
    } else if phase == 5 && info.start_light > info.num_red_lights {
        22
    } else {
        0
    };
    f64::from(code)
}

/// The rFactor 2 / Le Mans Ultimate listener (key `rf2`).
pub struct Rfactor2 {
    opener: Box<dyn BlockOpener>,
    /// The kept mapping objects (telemetry and scoring).
    telemetry_handle: KeptMapping,
    scoring_handle: KeptMapping,
    pause_when_not_playing: bool,
    pause_reader: Option<super::NativePauseReader>,
    telemetry: Option<Vec<u8>>,
    scoring: Option<Vec<u8>>,
    /// Player slots from the last complete search; `None` before the first one.
    car_index: Option<usize>,
    score_index: Option<usize>,
    previous: PreviousSample,
}

impl Rfactor2 {
    /// Listener reading the bridge mirrors. `pause_when_not_playing` is the
    /// `APP/pause_telemetry_when_not_playing` setting (default on).
    pub fn new(pause_when_not_playing: bool) -> Self {
        Self::with_opener(Box::new(BridgeMirrors), pause_when_not_playing)
    }

    /// Listener reading mappings through `opener`.
    pub fn with_opener(opener: Box<dyn BlockOpener>, pause_when_not_playing: bool) -> Self {
        Rfactor2 {
            opener,
            telemetry_handle: KeptMapping::new(RF2_TELEMETRY_MAPPING),
            scoring_handle: KeptMapping::new(RF2_SCORING_MAPPING),
            pause_when_not_playing,
            pause_reader: None,
            telemetry: None,
            scoring: None,
            car_index: None,
            score_index: None,
            previous: PreviousSample::default(),
        }
    }

    /// Change the compatibility pause value used when no settings reader is installed.
    pub fn set_pause_when_not_playing(&mut self, pause: bool) {
        self.pause_when_not_playing = pause;
    }

    /// Use the current settings at each getter instead of the compatibility bool.
    pub fn set_pause_reader(&mut self, reader: super::NativePauseReader) {
        self.pause_reader = Some(reader);
    }

    /// The previous snapshot's player speed (vector length) and rounded rpm; 0 and 0 when there is no
    /// snapshot, no player slot yet, or the rpm cannot be rounded.
    fn previous_sample(&self) -> PreviousSample {
        let (Some(block), Some(index)) = (&self.telemetry, self.car_index) else { return PreviousSample::default() };
        let car = VehicleTelemetry::decode(block, index);
        round_to_int(car.engine_rpm)
            .map(|rpm| PreviousSample { speed: speed_of(car.local_vel), rpm })
            .unwrap_or_default()
    }

    /// The liveness test: keep the slip reference, make new telemetry and
    /// scoring mapping objects on every call (create-or-open, replacing the kept objects) with a copy of
    /// each (a failure leaves the other snapshot as it is), locate the player in both, and require the player's
    /// `engineMaxRPM > 0` (otherwise both objects and both snapshots are dropped).
    fn refresh(&mut self) -> bool {
        self.previous = self.previous_sample();
        let Ok(telemetry) = self.telemetry_handle.remake(self.opener.as_mut()) else { return false };
        let Ok(scoring) = self.scoring_handle.remake(self.opener.as_mut()) else {
            self.telemetry = Some(telemetry);
            return false;
        };
        let car_index = python_index(find_player_telemetry(&telemetry, &scoring));
        self.car_index = Some(car_index);
        self.score_index = Some(python_index(find_player_scoring(&scoring)));
        if Fields(&telemetry).f64(telemetry_vehicle_base(car_index) + ENGINE_MAX_RPM) <= 0.0 {
            self.telemetry_handle.release();
            self.scoring_handle.release();
            self.telemetry = None;
            self.scoring = None;
            return false;
        }
        self.telemetry = Some(telemetry);
        self.scoring = Some(scoring);
        true
    }

    fn frame(&self) -> Option<Rf2Frame> {
        let (telemetry, scoring) = (self.telemetry.as_ref()?, self.scoring.as_ref()?);
        Some(Rf2Frame {
            car: VehicleTelemetry::decode(telemetry, self.car_index?),
            entry: VehicleScoring::decode(scoring, self.score_index?),
            info: ScoringInfo::decode(scoring),
        })
    }
}

impl Source for Rfactor2 {
    fn id(&self) -> &'static str {
        "rf2"
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        use LegacyValue::{Float, Int, Str};
        let pause = super::native_pause(&self.pause_reader, self.pause_when_not_playing)?;
        let car = || Some(VehicleTelemetry::decode(self.telemetry.as_ref()?, self.car_index?));
        let info = || Some(ScoringInfo::decode(self.scoring.as_ref()?));
        let entry = || Some(VehicleScoring::decode(self.scoring.as_ref()?, self.score_index?));
        if pause && entry()?.control != 0 {
            return Some(Int(0));
        }
        let value = match name {
            "rpm" => return round_to_int(car()?.engine_rpm).and_then(crate::udp_racing::native_int),
            "max_rpm" => return round_to_int(car()?.engine_max_rpm).and_then(crate::udp_racing::native_int),
            "gear" => return Some(Int(i64::from(car()?.gear))),
            "speed" => speed_of(car()?.local_vel),
            "flag" => return Some(Int(rf2_flag(&info()?, &entry()?) as i64)),
            "raw_flag" => return Some(Int(i64::from(info()?.game_phase))),
            "car_name" => {
                return Some(
                    car().and_then(|c| c.vehicle_name).map_or(Int(0), |name| Str(car_name_without_number(&name))),
                );
            }
            "track_loc" => return info()?.track_name.map(Str),
            "fuel_level" => {
                return Some(
                    car()
                        .filter(|c| c.fuel_capacity != 0.0)
                        .map_or(Int(0), |c| Float(percent_or_zero(c.fuel, c.fuel_capacity))),
                );
            }
            "throttle" => car()?.throttle,
            "brake" => car()?.brake,
            "clutch" => car()?.clutch,
            "steering_wheel_angle" => -car()?.steering,
            "lateral_acceleration" => round_6(car()?.local_accel[0]),
            "longitudinal_acceleration" => round_6(-car()?.local_accel[2]),
            "vertical_acceleration" => round_6(car()?.local_accel[1]),
            "lateral_velocity" => round_6(-car()?.local_vel[0]),
            "longitudinal_velocity" => round_6(car()?.local_vel[2]),
            "vertical_velocity" => round_6(car()?.local_vel[1]),
            "roll" | "pitch" | "yaw" => {
                let (roll, pitch, yaw) = rf2_orientation(car()?.ori);
                match name {
                    "roll" => roll,
                    "pitch" => pitch,
                    _ => yaw,
                }
            }
            "wheel_slip_lf" | "wheel_slip_lr" | "wheel_slip_rf" | "wheel_slip_rr" => {
                let c = car()?;
                let side =
                    if matches!(name, "wheel_slip_lf" | "wheel_slip_lr") { WheelSide::Left } else { WheelSide::Right };
                return Some(native_estimated_wheel_slip(
                    side,
                    &SlipInputs {
                        lateral_velocity: round_6(-c.local_vel[0]),
                        speed: speed_of(c.local_vel),
                        rpm: round_to_int(c.engine_rpm)?,
                        gear: i64::from(c.gear),
                        previous_speed: self.previous.speed,
                        previous_rpm: self.previous.rpm,
                        throttle: c.throttle,
                        brake: c.brake,
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
                round_6(car()?.suspension_deflection[i])
            }
            "rumble_pitch_lf" | "rumble_pitch_lr" | "rumble_pitch_rf" | "rumble_pitch_rr" => {
                // The middle wheels are swapped for rumble only.
                let i = match name {
                    "rumble_pitch_lf" => 0,
                    "rumble_pitch_lr" => 1,
                    "rumble_pitch_rf" => 2,
                    _ => 3,
                };
                return Some(Int(if car()?.on_kerb[i]? { 40 } else { 0 }));
            }
            "oil_temperature" => car()?.engine_oil_temp,
            "water_temperature" => car()?.engine_water_temp,
            _ => return Some(Int(0)),
        };
        Some(Float(value))
    }

    fn channel_read_raised(&self, name: &str) -> bool {
        self.native_channel(name).is_none()
    }

    fn read_native(&self, name: &str) -> crate::NativeRead {
        self.native_channel(name).map_or(crate::NativeRead::Raised, crate::NativeRead::Value)
    }

    fn receive_native(&mut self, _telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        Ok(true)
    }

    fn is_running(&mut self) -> Result<bool, SourceError> {
        Ok(self.refresh())
    }

    /// The receive step (always true) plus the channel mapping.
    fn poll(&mut self, telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        if let Some(frame) = self.frame() {
            frame.write(telemetry, self.previous, self.pause_when_not_playing);
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared_memory::block_reader::fake::{BlockBuilder, FakeBlocks};

    const PLAYER_ID: i32 = 4242;

    /// Telemetry block with the player's car in slot `slot`.
    fn telemetry_block(slot: usize) -> BlockBuilder {
        let mut b = BlockBuilder::new(RF2_TELEMETRY_MAPPING.size);
        let v = telemetry_vehicle_base(slot);
        b.i32(v, PLAYER_ID)
            .bytes(v + 32, b"Oreca 07 #38\0")
            .f64(v + 184, 3.0) // localVel
            .f64(v + 192, 0.0)
            .f64(v + 200, -4.0)
            .f64(v + 208, 1.25) // localAccel
            .f64(v + 216, -9.5)
            .f64(v + 224, 2.0)
            .f64(v + 232, 1.0) // ori[0]
            .f64(v + 248, 0.1)
            .f64(v + 256, 0.05) // ori[1]
            .f64(v + 264, 1.0)
            .f64(v + 272, 0.02)
            .f64(v + 280, -0.1) // ori[2]
            .f64(v + 296, 1.0)
            .i32(v + 352, 4) // gear
            .f64(v + 356, 8499.5) // engineRPM
            .f64(v + 364, 80.0) // engineWaterTemp
            .f64(v + 372, 95.0) // engineOilTemp
            .f64(v + 388, 0.0) // throttle
            .f64(v + 396, 1.0) // brake
            .f64(v + 404, -0.3) // steering
            .f64(v + 524, 45.0) // fuel
            .f64(v + 532, 9000.4) // engineMaxRPM
            .f64(v + 608, 90.0); // mFuelCapacity
        for (i, (defl, terrain)) in
            [(0.011, &b"RMBL\0"[..]), (0.012, b"RMBL\0"), (0.013, b"GRASS\0"), (0.014, b"ROAD\0")]
                .into_iter()
                .enumerate()
        {
            let w = v + WHEEL_BASE + i * WHEEL_SIZE;
            b.f64(w, defl).bytes(w + 160, terrain);
        }
        b
    }

    /// Scoring block with the player's entry in slot `slot`.
    fn scoring_block(slot: usize) -> BlockBuilder {
        let mut b = BlockBuilder::new(RF2_SCORING_MAPPING.size);
        b.bytes(SCORING_INFO_OFFSET, b"Le Mans\0")
            .u8(SCORING_INFO_OFFSET + 108, 5)
            .i8(SCORING_INFO_OFFSET + 110, -1)
            .i8(SCORING_INFO_OFFSET + 111, -1)
            .i8(SCORING_INFO_OFFSET + 112, -1);
        let e = scoring_vehicle_base(slot);
        b.i32(e, PLAYER_ID).i8(e + 102, 1).u8(e + 196, 1).i8(e + 197, 0);
        b
    }

    fn running(blocks: &FakeBlocks, telemetry: Vec<u8>, scoring: Vec<u8>, pause: bool) -> Rfactor2 {
        blocks.set(RF2_TELEMETRY_MAPPING, telemetry);
        blocks.set(RF2_SCORING_MAPPING, scoring);
        let mut l = Rfactor2::with_opener(Box::new(blocks.clone()), pause);
        assert!(l.is_running().unwrap());
        l
    }

    fn polled(telemetry: Vec<u8>, scoring: Vec<u8>, pause: bool) -> Telemetry {
        let mut l = running(&FakeBlocks::default(), telemetry, scoring, pause);
        let mut t = Telemetry::off();
        assert!(l.poll(&mut t).unwrap());
        t
    }

    #[test]
    fn native_getters_preserve_kinds_gates_and_failure_boundaries() {
        use LegacyValue::{Float, Int, Str};
        let blocks = FakeBlocks::default();
        let mut source = Rfactor2::with_opener(Box::new(blocks.clone()), true);
        assert!(source.poll(&mut Telemetry::off()).unwrap());
        assert!(source.channel_read_raised("unknown"), "pause gate reads scoring before matching");
        source.set_pause_when_not_playing(false);
        assert_eq!(source.native_channel("unknown"), Some(Int(0)));
        assert_eq!(source.native_channel("car_name"), Some(Int(0)), "local name catch");
        assert_eq!(source.native_channel("fuel_level"), Some(Int(0)), "local fuel catch");
        assert!(source.channel_read_raised("rpm"));
        source = running(&blocks, telemetry_block(0).build(), scoring_block(0).build(), true);
        for (name, expected) in [
            ("rpm", Int(8500)),
            ("max_rpm", Int(9000)),
            ("gear", Int(4)),
            ("speed", Float(5.0)),
            ("car_name", Str("Oreca 07".into())),
            ("track_loc", Str("Le Mans".into())),
            ("flag", Int(0)),
            ("raw_flag", Int(5)),
            ("fuel_level", Float(50.0)),
            ("throttle", Float(0.0)),
            ("brake", Float(1.0)),
            ("clutch", Float(0.0)),
            ("rumble_pitch_lr", Int(40)),
            ("rumble_pitch_rf", Int(0)),
            ("wheel_slip_rr", Int(0)),
        ] {
            assert_eq!(source.native_channel(name), Some(expected), "{name}");
        }
        assert!(source.is_running().unwrap());
        assert!(matches!(source.native_channel("wheel_slip_rr"), Some(Float(_))));
        let mut telemetry = telemetry_block(0);
        let base = telemetry_vehicle_base(0);
        telemetry
            .f64(base + 356, f64::NAN)
            .f64(base + 532, f64::INFINITY)
            .f64(base + 608, 0.0)
            .bytes(base + 32, b"\xff\0")
            .bytes(base + WHEEL_BASE + 160, b"\xff\0");
        source.telemetry = Some(telemetry.build());
        let mut scoring = scoring_block(0);
        scoring.bytes(SCORING_INFO_OFFSET, b"\xff\0");
        source.scoring = Some(scoring.build());
        assert_eq!(source.native_channel("car_name"), Some(Int(0)));
        assert_eq!(source.native_channel("fuel_level"), Some(Int(0)));
        for name in ["rpm", "max_rpm", "track_loc", "wheel_slip_rr", "rumble_pitch_lf"] {
            assert!(source.channel_read_raised(name), "{name}");
        }
        let mut scoring = scoring_block(0);
        scoring.i8(scoring_vehicle_base(0) + 197, 1);
        source.scoring = Some(scoring.build());
        for name in ["rpm", "car_name", "track_loc", "rumble_pitch_lf", "unknown"] {
            assert_eq!(source.native_channel(name), Some(Int(0)), "gate precedes failed read: {name}");
        }
        source.set_pause_when_not_playing(false);
        source.scoring = None;
        assert_eq!(source.native_channel("speed"), Some(Float(5.0)), "telemetry survives scoring failure");
        assert!(source.channel_read_raised("track_loc"));
    }

    #[test]
    fn mapping_constants() {
        assert_eq!(RF2_TELEMETRY_MAPPING.name, "$rFactor2SMMP_Telemetry$");
        assert_eq!(RF2_SCORING_MAPPING.name, "$rFactor2SMMP_Scoring$");
        assert_eq!(telemetry_vehicle_base(VEHICLE_SLOTS), RF2_TELEMETRY_MAPPING.size);
        assert_eq!(scoring_vehicle_base(VEHICLE_SLOTS), RF2_SCORING_MAPPING.size);
    }

    #[test]
    fn get_channel_maps_the_blocks() {
        let t = polled(telemetry_block(5).build(), scoring_block(9).build(), true);
        assert_eq!(t.rpm, 8500.0); // round(8499.5) -> 8500 (even)
        assert_eq!(t.max_rpm, 9000.0);
        assert_eq!(t.gear, 4.0);
        assert_eq!(t.speed, 5.0);
        assert_eq!(t.raw_flag, 5.0);
        assert_eq!(t.steering_wheel_angle, 0.3);
        assert_eq!(t.longitudinal_acceleration, -2.0);
        assert_eq!(t.lateral_acceleration, 1.25);
        assert_eq!(t.vertical_acceleration, -9.5);
        assert_eq!(t.vertical_velocity, 0.0);
        assert_eq!(t.longitudinal_velocity, -4.0);
        assert_eq!(t.lateral_velocity, -3.0);
        let k = RFACTOR_DEG_PER_RAD;
        let (ox, oy, oz): (Vec3, Vec3, Vec3) = ([1.0, 0.0, 0.1], [0.05, 1.0, 0.02], [-0.1, 0.0, 1.0]);
        assert_eq!(t.roll, oy[0].atan2((ox[0] * ox[0] + oz[0] * oz[0]).sqrt()) * k);
        assert_eq!(t.pitch, -(-((-oy[2]).atan2(((-ox[2]) * (-ox[2]) + (-oz[2]) * (-oz[2])).sqrt())) * k));
        assert_eq!(t.yaw, -((-ox[2]).atan2(oz[2])) * k);
        assert_eq!(
            [t.suspension_travel_lf, t.suspension_travel_rf, t.suspension_travel_lr, t.suspension_travel_rr],
            [0.011, 0.012, 0.013, 0.014]
        );
        // Wheels 0 and 1 are on a kerb: lf (wheel 0) and lr (wheel 1).
        assert_eq!(
            [t.rumble_pitch_lf, t.rumble_pitch_lr, t.rumble_pitch_rf, t.rumble_pitch_rr],
            [40.0, 40.0, 0.0, 0.0]
        );
        assert_eq!(t.vehicle, "Oreca 07");
        assert_eq!(t.location, "Le Mans");
        assert_eq!(t.oil_temperature, 95.0);
        assert_eq!(t.water_temperature, 80.0);
        assert_eq!(t.fuel_level, 50.0);
        assert_eq!(t.flag, 0.0);
    }

    #[test]
    fn get_channel_flag_chain() {
        let flag_of = |edit: &dyn Fn(&mut BlockBuilder)| {
            let mut s = scoring_block(0);
            edit(&mut s);
            polled(telemetry_block(0).build(), s.build(), true).flag
        };
        let entry = scoring_vehicle_base(0);
        let info = SCORING_INFO_OFFSET;
        assert_eq!(
            flag_of(&|s| {
                s.u8(info + 108, 8).i8(info + 110, 1);
            }),
            11.0
        );
        assert_eq!(
            flag_of(&|s| {
                s.i8(info + 111, 1);
            }),
            6.0
        );
        assert_eq!(
            flag_of(&|s| {
                s.u8(info + 108, 4).u8(entry + 504, 6);
            }),
            20.0
        );
        assert_eq!(
            flag_of(&|s| {
                s.u8(info + 108, 3);
            }),
            21.0
        );
        assert_eq!(
            flag_of(&|s| {
                s.u8(info + 108, 6);
            }),
            21.0
        );
        assert_eq!(
            flag_of(&|s| {
                s.u8(entry + 504, 6).u8(info + 113, 3).u8(info + 114, 1);
            }),
            2.0
        );
        assert_eq!(
            flag_of(&|s| {
                s.u8(info + 113, 3).u8(info + 114, 1);
            }),
            22.0
        );
        assert_eq!(
            flag_of(&|s| {
                s.u8(info + 113, 1).u8(info + 114, 1);
            }),
            0.0
        );
    }

    #[test]
    fn get_channel_pause_gate_on_non_player_control() {
        let mut s = scoring_block(0);
        s.i8(scoring_vehicle_base(0) + 197, 1); // AI
        let gated = polled(telemetry_block(0).build(), s.build(), true);
        assert_eq!((gated.rpm, gated.gear, gated.raw_flag, gated.fuel_level), (0.0, 0.0, 0.0, 0.0));
        assert_eq!((gated.vehicle.as_str(), gated.location.as_str()), ("", ""));
        assert_eq!(polled(telemetry_block(0).build(), s.build(), false).gear, 4.0);
    }

    #[test]
    fn get_channel_pause_setting_applies_on_the_next_poll() {
        let blocks = FakeBlocks::default();
        let mut s = scoring_block(0);
        s.i8(scoring_vehicle_base(0) + 197, 1); // AI
        let mut l = running(&blocks, telemetry_block(0).build(), s.build(), true);
        let mut t = Telemetry::off();
        l.poll(&mut t).unwrap();
        assert_eq!(t.gear, 0.0);
        l.set_pause_when_not_playing(false);
        l.poll(&mut t).unwrap();
        assert_eq!(t.gear, 4.0);
    }

    #[test]
    fn native_receive_does_not_replay_rfactor_getters() {
        let blocks = FakeBlocks::default();
        let mut source = running(&blocks, telemetry_block(0).build(), scoring_block(0).build(), true);
        let mut frame = Telemetry::off();
        frame.rpm = 123.0;
        frame.vehicle = "retained compatibility frame".into();
        assert!(source.receive_native(&mut frame).unwrap());
        assert_eq!(frame.rpm, 123.0, "original receiveData only returns True");
        assert_eq!(frame.vehicle, "retained compatibility frame");
    }

    #[test]
    fn native_pause_rfactor_reads_required_current_value_once_per_getter() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};
        let blocks = FakeBlocks::default();
        let mut scoring = scoring_block(0);
        scoring.i8(scoring_vehicle_base(0) + 197, 1);
        let mut source = running(&blocks, telemetry_block(0).build(), scoring.build(), true);
        let value = Arc::new(Mutex::new(Some(LegacyValue::from("no"))));
        let calls = Arc::new(AtomicUsize::new(0));
        source.set_pause_reader({
            let (value, calls) = (value.clone(), calls.clone());
            Arc::new(move || {
                calls.fetch_add(1, Ordering::SeqCst);
                value.lock().unwrap().clone()
            })
        });
        assert!(source.is_running().unwrap());
        assert!(source.receive_native(&mut Telemetry::off()).unwrap());
        assert_eq!(calls.load(Ordering::SeqCst), 0, "only getChannel reads the setting");
        assert_eq!(source.read_native("rpm"), crate::NativeRead::Value(LegacyValue::Int(8500)));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        *value.lock().unwrap() = Some(LegacyValue::from("y"));
        assert_eq!(source.read_native("rpm"), crate::NativeRead::Value(LegacyValue::Int(0)));
        *value.lock().unwrap() = Some(LegacyValue::from("no"));
        source.scoring = None;
        assert_eq!(source.read_native("rpm"), crate::NativeRead::Value(LegacyValue::Int(8500)));
        for raw in [Some(LegacyValue::from("invalid")), Some(LegacyValue::None), None] {
            *value.lock().unwrap() = raw;
            assert_eq!(source.read_native("unknown"), crate::NativeRead::Raised);
            assert_eq!(source.read_native("car_name"), crate::NativeRead::Raised);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 9);
        *value.lock().unwrap() = Some(LegacyValue::Bool(false));
        assert_eq!(source.read_native("unknown"), crate::NativeRead::Value(LegacyValue::Int(0)));
    }

    #[test]
    fn get_channel_car_name_decode_error_empties_vehicle() {
        let blocks = FakeBlocks::default();
        let mut l = running(&blocks, telemetry_block(0).build(), scoring_block(0).build(), true);
        let mut t = Telemetry::off();
        l.poll(&mut t).unwrap();
        assert_eq!(t.vehicle, "Oreca 07");
        let mut bad = telemetry_block(0);
        bad.bytes(telemetry_vehicle_base(0) + 32, b"\xff#1\0");
        blocks.set(RF2_TELEMETRY_MAPPING, bad.build());
        assert!(l.is_running().unwrap());
        l.poll(&mut t).unwrap();
        assert_eq!(t.vehicle, "", "rFactor2Listener.getChannel('car_name') returns 0 on a decode error");
    }

    #[test]
    fn check_status_player_search() {
        let blocks = FakeBlocks::default();
        // Player in scoring slot 9, its telemetry in slot 5.
        let l = running(&blocks, telemetry_block(5).build(), scoring_block(9).build(), true);
        assert_eq!((l.car_index, l.score_index), (Some(5), Some(9)));
        // No player flagged: both indexes are -1, the last slot.
        let mut s = scoring_block(0);
        s.u8(scoring_vehicle_base(0) + 196, 0);
        let mut t = telemetry_block(VEHICLE_SLOTS - 1);
        t.f64(telemetry_vehicle_base(VEHICLE_SLOTS - 1) + 532, 7000.0);
        let l = running(&blocks, t.build(), s.build(), true);
        assert_eq!((l.car_index, l.score_index), (Some(127), Some(127)));
    }

    #[test]
    fn check_status_liveness_and_partial_failures() {
        let blocks = FakeBlocks::default();
        let mut l = Rfactor2::with_opener(Box::new(blocks.clone()), true);
        // No game: both mappings create zero-filled sections, engineMaxRPM is 0, and both objects are dropped.
        assert!(!l.is_running().unwrap(), "zero-filled sections");
        assert!(l.telemetry.is_none() && l.car_index.is_some());
        assert_eq!(blocks.held_count(RF2_TELEMETRY_MAPPING.name), 0);
        assert_eq!(blocks.held_count(RF2_SCORING_MAPPING.name), 0);
        blocks.set(RF2_TELEMETRY_MAPPING, telemetry_block(0).build());
        blocks.set(RF2_SCORING_MAPPING, scoring_block(0).build());
        assert!(l.is_running().unwrap());
        assert!(l.is_running().unwrap());
        // Each call makes new objects before the previous ones go: one of each is held.
        assert_eq!(blocks.held_count(RF2_TELEMETRY_MAPPING.name), 1);
        assert_eq!(blocks.held_count(RF2_SCORING_MAPPING.name), 1);
        let mut t = telemetry_block(0);
        t.f64(telemetry_vehicle_base(0) + 532, 0.0);
        blocks.set(RF2_TELEMETRY_MAPPING, t.build());
        assert!(!l.is_running().unwrap(), "engineMaxRPM <= 0");
        assert!(l.telemetry.is_none() && l.scoring.is_none());
        assert_eq!(blocks.held_count(RF2_TELEMETRY_MAPPING.name), 0);
    }

    #[test]
    fn check_status_previous_sample() {
        let blocks = FakeBlocks::default();
        let mut l = running(&blocks, telemetry_block(0).build(), scoring_block(0).build(), true);
        assert_eq!(l.previous, PreviousSample::default(), "first call has no snapshot");
        assert!(l.is_running().unwrap());
        assert_eq!(l.previous, PreviousSample { speed: 5.0, rpm: 8500.0 });
        // A NaN rpm makes the previous sample 0, 0.
        let mut t = telemetry_block(0);
        t.f64(telemetry_vehicle_base(0) + 356, f64::NAN);
        blocks.set(RF2_TELEMETRY_MAPPING, t.build());
        assert!(l.is_running().unwrap());
        assert!(l.is_running().unwrap());
        assert_eq!(l.previous, PreviousSample::default());
        let mut out = Telemetry::off();
        out.rpm = 77.0;
        out.wheel_slip_rr = 0.5;
        l.poll(&mut out).unwrap();
        assert_eq!(out.rpm, 77.0, "round(nan) raises: value kept");
        assert_eq!(out.wheel_slip_rr, 0.5);
    }

    #[test]
    fn car_name_cut_before_number() {
        assert_eq!(car_name_without_number("Oreca 07 #38"), "Oreca 07");
        assert_eq!(car_name_without_number("Porsche 963"), "Porsche 963");
        assert_eq!(car_name_without_number("#5 Car"), "#5 Ca");
        assert_eq!(car_name_without_number("A#1"), "");
    }
}
