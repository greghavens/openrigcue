//! RaceRoom Racing Experience / RaceRoom EvO through the `$R3E` block (SDK 3.4
//! layout, byte-packed, 43 996 bytes), plus the car-name table the game ships in `r3e-data.json`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};

use openrigcue_model::{native_value::LegacyValue, python_numeric::python_int_text, python_repr::str_value, Telemetry};

use super::block_reader::{BlockOpener, BridgeMirrors, Fields, KeptMapping, MappingSpec};
use super::listener_channels::begin_tick;
use super::python_numeric::{rad_to_deg, round_6, round_to_int, truncate_to_int};
use crate::{Source, SourceError};

/// The game's mapping, opened read-only.
pub const RACEROOM_MAPPING: MappingSpec = MappingSpec { name: "$R3E", size: 43996 };

/// The data file, relative to the Steam library's game folders; used to find the game and its data folder.
pub const R3E_DATA_FILE: &str = "raceroom racing experience\\Game\\GameData\\General\\r3e-data.json";

/// The file read from the data folder.
const R3E_DATA_FILE_NAME: &str = "r3e-data.json";

/// Body of the car-table loading thread: with the game found and its data folder present,
/// `r3e-data.json` in that folder, when it is a file,
/// is read into the table ([`RaceRoomCarNames::load`]).
fn load_r3e_cars(plugin_dir: impl FnOnce() -> Option<PathBuf>, table: &RwLock<RaceRoomCarNames>) {
    let Some(plugin_dir) = plugin_dir().filter(|dir| dir.is_dir()) else { return };
    let json_file = plugin_dir.join(R3E_DATA_FILE_NAME);
    if json_file.is_file() {
        if let Ok(text) = std::fs::read_to_string(&json_file) {
            publish_car_names(&text, table, || {});
        }
    }
}

/// Factor from rad/s to rev/min.
const RPS_TO_RPM: f64 = 9.549296596;
/// `R3E_MTRL_TYPE_RUMBLE`.
const SURFACE_RUMBLE: i32 = 5;
/// `rumble_pitch_*` value on a kerb.
const KERB_RUMBLE: f64 = 40.0;
/// `R3E_CONTROL_PLAYER`.
const CONTROL_PLAYER: i32 = 0;

/// RaceRoom car model id to display name, from `r3e-data.json` (`cars.<id>.Name`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RaceRoomCarNames(HashMap<i32, (String, LegacyValue)>);

impl RaceRoomCarNames {
    /// Loads the table from an already located file: a missing, unreadable or
    /// malformed file yields an empty table; an entry whose id is not an integer or whose value has no `Name`
    /// is skipped and loading goes on with the next one.
    pub fn load(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .map(|text| Self::parse(&text))
            .unwrap_or_default()
    }

    /// Parse the text of `r3e-data.json` (see [`RaceRoomCarNames::load`]).
    pub fn parse(json: &str) -> Self {
        let mut names = HashMap::new();
        parse_car_names(json, |id, text, name| {
            names.insert(id, (text, name));
        });
        RaceRoomCarNames(names)
    }

    /// Display name of a model id.
    pub fn get(&self, model_id: i32) -> Option<&str> {
        self.0.get(&model_id).map(|(text, _)| text.as_str())
    }

    fn native(&self, model_id: i32) -> Option<&LegacyValue> {
        self.0.get(&model_id).map(|(_, value)| value)
    }
}

fn parse_car_names(json: &str, mut publish: impl FnMut(i32, String, LegacyValue)) {
    let Ok(LegacyValue::Dict(root)) = openrigcue_config::import::json::parse(json) else { return };
    let Some(LegacyValue::Dict(cars)) = root.get("cars") else { return };
    for (key, car) in cars.iter() {
        let openrigcue_model::native_value::Key::Str(key) = key else { continue };
        // Name is read before converting the key; either failure skips the entry.
        let Some(name) = car.as_dict().and_then(|car| car.get("Name")) else { continue };
        let Some(CarKey::Id(id)) = parse_car_key(key) else { continue };
        publish(id, str_value(name), name.clone());
    }
}

fn publish_car_names(json: &str, table: &RwLock<RaceRoomCarNames>, mut after: impl FnMut()) {
    parse_car_names(json, |id, text, name| {
        table.write().unwrap_or_else(PoisonError::into_inner).0.insert(id, (text, name));
        after();
    });
}

/// A car key converted to an integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CarKey {
    /// An id a `model_id` can hold.
    Id(i32),
    /// A valid integer outside the `i32` range of `model_id`: it is stored, but no car ever matches it.
    Unmatchable,
}

/// Integer from a decimal string: surrounding whitespace, a sign and single `_` between digits
/// are allowed. `None` for any other text.
fn parse_car_key(text: &str) -> Option<CarKey> {
    let normalized = python_int_text(text)?;
    Some(normalized.parse().map_or(CarKey::Unmatchable, CarKey::Id))
}

/// `r3e_flags` fields used for the flag channel (-1 = unavailable, 0 = off, > 0 = on).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RaceFlags {
    yellow: i32,
    yellow_caused_it: i32,
    yellow_overtake: i32,
    yellow_positions_gained: i32,
    blue: i32,
    black: i32,
    green: i32,
    checkered: i32,
    white: i32,
}

/// The fields of `r3e_shared` the listener reads.
#[derive(Debug, Clone, Copy, PartialEq)]
struct RaceRoomFrame {
    game_paused: i32,
    local_velocity: [f64; 3],
    suspension_deflection: [f64; 4],
    suspension_velocity: [f64; 4],
    start_lights: i32,
    flags: RaceFlags,
    model_id: i32,
    control_type: i32,
    car_speed: f64,
    engine_rps: f64,
    max_engine_rps: f64,
    gear: i32,
    /// `car_orientation` pitch, yaw, roll (rad).
    orientation: [f64; 3],
    local_acceleration: [f64; 3],
    engine_oil_temp: f64,
    tire_on_mtrl: [i32; 4],
}

impl RaceRoomFrame {
    fn decode(bytes: &[u8]) -> Self {
        let f = Fields(bytes);
        let f64s = |base: usize| [f.f64(base), f.f64(base + 8), f.f64(base + 16), f.f64(base + 24)];
        let f32s = |base: usize| [f.f32(base), f.f32(base + 4), f.f32(base + 8)];
        let flag = |offset: usize| f.i32(932 + offset);
        RaceRoomFrame {
            game_paused: f.i32(20),
            local_velocity: [f.f64(104), f.f64(112), f.f64(120)],
            suspension_deflection: f64s(392),
            suspension_velocity: f64s(424),
            start_lights: f.i32(800),
            flags: RaceFlags {
                yellow: flag(0),
                yellow_caused_it: flag(4),
                yellow_overtake: flag(8),
                yellow_positions_gained: flag(12),
                blue: flag(32),
                black: flag(36),
                green: flag(40),
                checkered: flag(44),
                white: flag(48),
            },
            model_id: f.i32(1268),
            control_type: f.i32(1388),
            car_speed: f.f32(1392),
            engine_rps: f.f32(1396),
            max_engine_rps: f.f32(1400),
            gear: f.i32(1408),
            orientation: f32s(1428),
            local_acceleration: f32s(1440),
            engine_oil_temp: f.f32(1484),
            tire_on_mtrl: [f.i32(1952), f.i32(1956), f.i32(1960), f.i32(1964)],
        }
    }

    /// The flag chain (first match wins).
    fn flag(&self) -> f64 {
        let on = |v: i32| v != -1 && v != 0;
        let fl = &self.flags;
        let lights = self.start_lights;
        let code = if on(fl.checkered) {
            11
        } else if fl.blue == -1 && (1..=2).contains(&lights) {
            21
        } else if fl.blue == -1 && (3..=5).contains(&lights) {
            20
        } else if fl.black == 1 && lights == 6 {
            22
        } else if on(fl.green) {
            12
        } else if on(fl.yellow) || on(fl.yellow_caused_it) || on(fl.yellow_overtake) || on(fl.yellow_positions_gained) {
            6
        } else if on(fl.blue) {
            2
        } else if on(fl.black) {
            10
        } else if on(fl.white) {
            3
        } else {
            0
        };
        f64::from(code)
    }

    /// The pause gate: the player is not driving (AI, remote, replay,
    /// unavailable) while the "pause telemetry when not playing" option is on, or the game is paused.
    fn is_gated(&self, pause_when_not_playing: bool) -> bool {
        (pause_when_not_playing && self.control_type != CONTROL_PLAYER) || self.game_paused == 1
    }

    /// Writes the channels.
    fn write(&self, t: &mut Telemetry, names: &RaceRoomCarNames, pause_when_not_playing: bool) {
        let previous = begin_tick(t);
        if self.is_gated(pause_when_not_playing) {
            return;
        }
        let rumble = |surface: i32| if surface == SURFACE_RUMBLE { KERB_RUMBLE } else { 0.0 };
        let [pitch, yaw, roll] = self.orientation;
        let [accel_x, accel_y, accel_z] = self.local_acceleration;
        let [vel_x, vel_y, vel_z] = self.local_velocity;
        // R3E wheel order is front-left, front-right, rear-left, rear-right.
        let [defl_fl, defl_fr, defl_rl, defl_rr] = self.suspension_deflection;
        let [svel_fl, svel_fr, svel_rl, svel_rr] = self.suspension_velocity;
        let [mtrl_fl, mtrl_fr, mtrl_rl, mtrl_rr] = self.tire_on_mtrl;

        t.rpm = truncate_to_int(self.engine_rps * RPS_TO_RPM).unwrap_or(previous.rpm);
        t.max_rpm = truncate_to_int(self.max_engine_rps * RPS_TO_RPM).unwrap_or(previous.max_rpm);
        t.gear = f64::from(self.gear);
        t.speed = round_to_int(self.car_speed).unwrap_or(previous.speed);
        // `car_name` falls back to 0 (no name) for ids missing from the table; `track_loc` always falls back to
        // 0 because the track name bytes are not decoded as text.
        if let Some(name) = names.get(self.model_id) {
            t.vehicle = name.to_owned();
        }
        t.lateral_acceleration = accel_x;
        t.longitudinal_acceleration = -accel_z;
        t.vertical_acceleration = accel_y;
        t.yaw = rad_to_deg(yaw);
        t.roll = rad_to_deg(roll);
        t.pitch = rad_to_deg(pitch);
        t.vertical_velocity = round_6(vel_y);
        t.lateral_velocity = -round_6(vel_x);
        t.longitudinal_velocity = round_6(vel_z);
        t.suspension_travel_lf = round_6(defl_fl);
        t.suspension_travel_lr = round_6(defl_rl);
        t.suspension_travel_rf = round_6(defl_fr);
        t.suspension_travel_rr = round_6(defl_rr);
        t.suspension_velocity_lf = round_6(svel_fl);
        t.suspension_velocity_lr = round_6(svel_rl);
        t.suspension_velocity_rf = round_6(svel_fr);
        t.suspension_velocity_rr = round_6(svel_rr);
        t.oil_temperature = self.engine_oil_temp;
        t.rumble_pitch_lf = rumble(mtrl_fl);
        t.rumble_pitch_rf = rumble(mtrl_fr);
        t.rumble_pitch_lr = rumble(mtrl_rl);
        t.rumble_pitch_rr = rumble(mtrl_rr);
        t.flag = self.flag();
        t.raw_flag = -1.0;
    }
}

/// The RaceRoom listener (key `rre`).
pub struct RaceRoom {
    opener: Box<dyn BlockOpener>,
    /// The kept mapping object.
    handle: KeptMapping,
    pause_when_not_playing: bool,
    pause_reader: Option<super::NativePauseReader>,
    /// The car-name table, filled by [`load_r3e_cars`] in its own thread.
    car_names: Arc<RwLock<RaceRoomCarNames>>,
    frame: Option<RaceRoomFrame>,
}

impl RaceRoom {
    /// Listener reading the bridge mirror of [`RACEROOM_MAPPING`]. `pause_when_not_playing` is the
    /// `APP/pause_telemetry_when_not_playing` setting (default on).
    pub fn new(pause_when_not_playing: bool, car_names: RaceRoomCarNames) -> Self {
        Self::with_opener(Box::new(BridgeMirrors), pause_when_not_playing, car_names)
    }

    /// Listener reading the bridge mirror of [`RACEROOM_MAPPING`] whose car table is loaded in the background:
    /// a thread is
    /// started and the table stays empty until the thread has read the file.
    ///
    /// `plugin_dir` locates the game's data folder through the Steam libraries
    /// with [`R3E_DATA_FILE`]; it answers
    /// `None` when the game was not found (or the lookup failed, which ends the thread). It runs in the thread.
    pub fn with_car_loader(
        pause_when_not_playing: bool,
        plugin_dir: impl FnOnce() -> Option<PathBuf> + Send + 'static,
    ) -> Self {
        let listener = Self::new(pause_when_not_playing, RaceRoomCarNames::default());
        let table = Arc::clone(&listener.car_names);
        let started = std::thread::Builder::new()
            .name("threadLoadRRECars".to_owned())
            .spawn(move || load_r3e_cars(plugin_dir, &table));
        if let Err(error) = started {
            tracing::error!(%error, "rreListener: threadLoadRRECars could not start");
        }
        listener
    }

    /// Listener reading mappings through `opener`.
    pub fn with_opener(
        opener: Box<dyn BlockOpener>,
        pause_when_not_playing: bool,
        car_names: RaceRoomCarNames,
    ) -> Self {
        RaceRoom {
            opener,
            handle: KeptMapping::new(RACEROOM_MAPPING),
            pause_when_not_playing,
            pause_reader: None,
            car_names: Arc::new(RwLock::new(car_names)),
            frame: None,
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

    /// Replace the car-name table.
    pub fn set_car_names(&mut self, car_names: RaceRoomCarNames) {
        *self.car_names.write().unwrap_or_else(PoisonError::into_inner) = car_names;
    }

    /// The liveness test: the read-only mapping is opened when it is not held
    /// (and kept), a snapshot is taken on every call, and a car must be loaded (`max_engine_rps > 0`,
    /// otherwise the mapping and the snapshot are dropped).
    fn refresh(&mut self) -> bool {
        let Ok(bytes) = self.handle.snapshot_read_only(self.opener.as_mut()) else { return false };
        let frame = RaceRoomFrame::decode(&bytes);
        if frame.max_engine_rps <= 0.0 {
            self.handle.release();
            self.frame = None;
            return false;
        }
        self.frame = Some(frame);
        true
    }
}

impl Source for RaceRoom {
    fn id(&self) -> &'static str {
        "rre"
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        use LegacyValue::{Float, Int, Str};
        let pause = super::native_pause(&self.pause_reader, self.pause_when_not_playing)?;
        // Both gates read the shared-memory object before any channel-name branch.
        let f = self.frame.as_ref()?;
        if f.is_gated(pause) {
            return Some(Int(0));
        }
        let value = match name {
            "rpm" => return crate::udp_racing::int_text(f.engine_rps * RPS_TO_RPM).map(Str),
            "max_rpm" => return crate::udp_racing::int_text(f.max_engine_rps * RPS_TO_RPM).map(Str),
            "gear" => return Some(Str(f.gear.to_string())),
            "speed" => return round_to_int(f.car_speed).and_then(crate::udp_racing::int_text).map(Str),
            "car_name" => {
                return Some(
                    self.car_names
                        .read()
                        .unwrap_or_else(PoisonError::into_inner)
                        .native(f.model_id)
                        .cloned()
                        .unwrap_or(Int(0)),
                );
            }
            "track_loc" => return Some(Int(0)),
            "lateral_acceleration" => f.local_acceleration[0],
            "longitudinal_acceleration" => -f.local_acceleration[2],
            "vertical_acceleration" => f.local_acceleration[1],
            "yaw" => rad_to_deg(f.orientation[1]),
            "pitch" => rad_to_deg(f.orientation[0]),
            "roll" => rad_to_deg(f.orientation[2]),
            "vertical_velocity" => round_6(f.local_velocity[1]),
            "lateral_velocity" => -round_6(f.local_velocity[0]),
            "longitudinal_velocity" => round_6(f.local_velocity[2]),
            "suspension_travel_lf" => round_6(f.suspension_deflection[0]),
            "suspension_travel_rf" => round_6(f.suspension_deflection[1]),
            "suspension_travel_lr" => round_6(f.suspension_deflection[2]),
            "suspension_travel_rr" => round_6(f.suspension_deflection[3]),
            "suspension_velocity_lf" => round_6(f.suspension_velocity[0]),
            "suspension_velocity_rf" => round_6(f.suspension_velocity[1]),
            "suspension_velocity_lr" => round_6(f.suspension_velocity[2]),
            "suspension_velocity_rr" => round_6(f.suspension_velocity[3]),
            "oil_temperature" => f.engine_oil_temp,
            "rumble_pitch_lf" | "rumble_pitch_rf" | "rumble_pitch_lr" | "rumble_pitch_rr" => {
                let index = match name {
                    "rumble_pitch_lf" => 0,
                    "rumble_pitch_rf" => 1,
                    "rumble_pitch_lr" => 2,
                    _ => 3,
                };
                return Some(Int(if f.tire_on_mtrl[index] == SURFACE_RUMBLE { 40 } else { 0 }));
            }
            "flag" => return Some(Int(f.flag() as i64)),
            "raw_flag" => return Some(Int(-1)),
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
        if let Some(frame) = self.frame {
            let names = self.car_names.read().unwrap_or_else(PoisonError::into_inner);
            frame.write(telemetry, &names, self.pause_when_not_playing);
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared_memory::block_reader::fake::{BlockBuilder, FakeBlocks};

    fn driving_block() -> BlockBuilder {
        let mut b = BlockBuilder::new(RACEROOM_MAPPING.size);
        b.i32(20, 0) // game_paused
            .f64(104, 0.4) // player.local_velocity.x
            .f64(112, -0.1234567) // .y
            .f64(120, 50.0000004) // .z
            .f64(392, 0.01) // suspension_deflection FL
            .f64(400, 0.02) // FR
            .f64(408, 0.03) // RL
            .f64(416, 0.04) // RR
            .f64(424, 0.1) // suspension_velocity FL
            .f64(432, 0.2) // FR
            .f64(440, 0.3) // RL
            .f64(448, 0.4) // RR
            .i32(800, -1) // start_lights
            .bytes(932, &[0xff; 56]) // every flag -1 (unavailable)
            .i32(1268, 1703) // vehicle_info.model_id
            .i32(1388, 0) // control_type player
            .f32(1392, 52.5) // car_speed
            .f32(1396, 600.0) // engine_rps
            .f32(1400, 900.0) // max_engine_rps
            .i32(1408, 4) // gear
            .f32(1428, 0.02) // car_orientation.pitch
            .f32(1432, 1.0) // .yaw
            .f32(1436, -0.03) // .roll
            .f32(1440, 2.5) // local_acceleration.x
            .f32(1444, 9.75) // .y
            .f32(1448, -4.0) // .z
            .f32(1484, 105.0) // engine_oil_temp
            .i32(1952, 5) // tire_on_mtrl FL rumble
            .i32(1956, 1)
            .i32(1960, 1)
            .i32(1964, 5); // RR rumble
        b
    }

    fn names() -> RaceRoomCarNames {
        RaceRoomCarNames::parse(r#"{"cars": {"1703": {"Name": "BMW M4 GT3", "Class": 1}}}"#)
    }

    fn listener(blocks: &FakeBlocks, pause: bool) -> RaceRoom {
        RaceRoom::with_opener(Box::new(blocks.clone()), pause, names())
    }

    fn poll_block(block: Vec<u8>, pause: bool) -> Telemetry {
        let blocks = FakeBlocks::default();
        blocks.set(RACEROOM_MAPPING, block);
        let mut l = listener(&blocks, pause);
        assert!(l.is_running().unwrap());
        let mut t = Telemetry::off();
        t.location = "kept?".into();
        assert!(l.poll(&mut t).unwrap());
        t
    }

    #[test]
    fn get_channel_maps_the_block() {
        let t = poll_block(driving_block().build(), true);
        assert_eq!(t.rpm, (600.0 * RPS_TO_RPM).trunc());
        assert_eq!(t.max_rpm, (900.0 * RPS_TO_RPM).trunc());
        assert_eq!(t.gear, 4.0);
        assert_eq!(t.speed, 52.0);
        assert_eq!(t.vehicle, "BMW M4 GT3");
        assert_eq!(t.location, "", "track_loc always falls back to 0");
        assert_eq!(t.lateral_acceleration, 2.5);
        assert_eq!(t.longitudinal_acceleration, 4.0);
        assert_eq!(t.vertical_acceleration, 9.75);
        assert_eq!(t.yaw, rad_to_deg(1.0));
        assert_eq!(t.roll, rad_to_deg(f64::from(-0.03f32)));
        assert_eq!(t.pitch, rad_to_deg(f64::from(0.02f32)));
        assert_eq!(t.vertical_velocity, -0.123457);
        assert_eq!(t.lateral_velocity, -0.4);
        assert_eq!(t.longitudinal_velocity, 50.0);
        assert_eq!(
            [t.suspension_travel_lf, t.suspension_travel_rf, t.suspension_travel_lr, t.suspension_travel_rr],
            [0.01, 0.02, 0.03, 0.04]
        );
        assert_eq!(
            [t.suspension_velocity_lf, t.suspension_velocity_rf, t.suspension_velocity_lr, t.suspension_velocity_rr],
            [0.1, 0.2, 0.3, 0.4]
        );
        assert_eq!(t.oil_temperature, 105.0);
        assert_eq!(
            [t.rumble_pitch_lf, t.rumble_pitch_rf, t.rumble_pitch_lr, t.rumble_pitch_rr],
            [40.0, 0.0, 0.0, 40.0]
        );
        assert_eq!(t.flag, 0.0);
        assert_eq!(t.raw_flag, -1.0);
        assert_eq!(t.water_temperature, 0.0);
    }

    #[test]
    fn get_channel_unknown_car_has_no_name() {
        let mut b = driving_block();
        b.i32(1268, 42);
        assert_eq!(poll_block(b.build(), true).vehicle, "");
    }

    #[test]
    fn get_channel_flag_chain() {
        let flag_of = |edit: &dyn Fn(&mut BlockBuilder)| {
            let mut b = driving_block();
            edit(&mut b);
            poll_block(b.build(), true).flag
        };
        // offsets inside r3e_flags at 932
        let (yellow, caused, overtake, gained, blue, black, green, checkered, white) =
            (932, 936, 940, 944, 964, 968, 972, 976, 980);
        assert_eq!(
            flag_of(&|b| {
                b.i32(checkered, 1).i32(green, 1);
            }),
            11.0
        );
        assert_eq!(
            flag_of(&|b| {
                b.i32(checkered, 0).i32(800, 1);
            }),
            21.0
        );
        assert_eq!(
            flag_of(&|b| {
                b.i32(800, 2);
            }),
            21.0
        );
        assert_eq!(
            flag_of(&|b| {
                b.i32(800, 3);
            }),
            20.0
        );
        assert_eq!(
            flag_of(&|b| {
                b.i32(800, 5);
            }),
            20.0
        );
        assert_eq!(
            flag_of(&|b| {
                b.i32(800, 6).i32(black, 1);
            }),
            22.0
        );
        assert_eq!(
            flag_of(&|b| {
                b.i32(800, 6);
            }),
            0.0
        );
        assert_eq!(
            flag_of(&|b| {
                b.i32(800, 1).i32(blue, 0).i32(green, 1);
            }),
            12.0
        );
        assert_eq!(
            flag_of(&|b| {
                b.i32(yellow, 2);
            }),
            6.0
        );
        assert_eq!(
            flag_of(&|b| {
                b.i32(caused, 1);
            }),
            6.0
        );
        assert_eq!(
            flag_of(&|b| {
                b.i32(overtake, 1);
            }),
            6.0
        );
        assert_eq!(
            flag_of(&|b| {
                b.i32(gained, 1);
            }),
            6.0
        );
        assert_eq!(
            flag_of(&|b| {
                b.i32(blue, 1);
            }),
            2.0
        );
        assert_eq!(
            flag_of(&|b| {
                b.i32(black, 2);
            }),
            10.0
        );
        assert_eq!(
            flag_of(&|b| {
                b.i32(white, 1);
            }),
            3.0
        );
        assert_eq!(
            flag_of(&|b| {
                b.i32(white, 0);
            }),
            0.0
        );
    }

    #[test]
    fn get_channel_pause_gates() {
        let mut ai = driving_block();
        ai.i32(1388, 1);
        let gated = poll_block(ai.build(), true);
        assert_eq!((gated.rpm, gated.speed, gated.raw_flag), (0.0, 0.0, 0.0));
        assert_eq!(gated.vehicle, "");
        // With the option off an AI-driven car is reported.
        assert_eq!(poll_block(ai.build(), false).gear, 4.0);
        // game_paused gates regardless of the option.
        let mut paused = driving_block();
        paused.i32(20, 1);
        assert_eq!(poll_block(paused.build(), false).gear, 0.0);
    }

    #[test]
    fn get_channel_pause_setting_applies_on_the_next_poll() {
        let blocks = FakeBlocks::default();
        let mut ai = driving_block();
        ai.i32(1388, 1);
        blocks.set(RACEROOM_MAPPING, ai.build());
        let mut l = listener(&blocks, true);
        assert!(l.is_running().unwrap());
        let mut t = Telemetry::off();
        l.poll(&mut t).unwrap();
        assert_eq!(t.gear, 0.0);
        l.set_pause_when_not_playing(false);
        l.poll(&mut t).unwrap();
        assert_eq!(t.gear, 4.0);
    }

    #[test]
    fn check_status_needs_a_loaded_car_and_keeps_the_mapping() {
        let blocks = FakeBlocks::default();
        let mut l = listener(&blocks, true);
        assert!(!l.is_running().unwrap(), "a missing read-only mmap is zero-filled, so no car is loaded");
        assert_eq!(blocks.read_only_holds(), 1);
        assert_eq!(blocks.held_count(RACEROOM_MAPPING.name), 0, "the zero frame releases its mapping");
        let mut b = driving_block();
        b.f32(1400, 0.0);
        blocks.set(RACEROOM_MAPPING, b.build());
        assert!(!l.is_running().unwrap());
        assert_eq!(blocks.held_count(RACEROOM_MAPPING.name), 0, "max_engine_rps <= 0 drops r3e_smm_handle");
        blocks.set(RACEROOM_MAPPING, driving_block().build());
        assert!(l.is_running().unwrap());
        assert!(l.is_running().unwrap());
        assert_eq!(blocks.open_count(), 3, "r3e_smm_handle is kept while a car is loaded");
        assert_eq!(blocks.held_count(RACEROOM_MAPPING.name), 1);
    }

    /// Background loading: the table is read from `<plugin dir>/r3e-data.json` in the thread; no
    /// plugin folder, a missing folder or a missing file leave it empty.
    #[test]
    fn load_r3e_cars_thread() {
        let dir = tempfile::tempdir().unwrap();
        let general = dir.path().join("General");
        let table = RwLock::new(RaceRoomCarNames::default());
        load_r3e_cars(|| None, &table);
        load_r3e_cars(|| Some(general.clone()), &table);
        std::fs::create_dir(&general).unwrap();
        load_r3e_cars(|| Some(general.clone()), &table);
        assert_eq!(*table.read().unwrap(), RaceRoomCarNames::default());
        std::fs::write(general.join("r3e-data.json"), r#"{"cars": {"1703": {"Name": "BMW M4 GT3"}}}"#).unwrap();
        load_r3e_cars(|| Some(general.clone()), &table);
        assert_eq!(table.read().unwrap().get(1703), Some("BMW M4 GT3"));
        std::fs::write(general.join("r3e-data.json"), "invalid json").unwrap();
        load_r3e_cars(|| Some(general.clone()), &table);
        assert_eq!(table.read().unwrap().get(1703), Some("BMW M4 GT3"), "failed reload retains existing names");
        std::fs::write(general.join("r3e-data.json"), r#"{"cars": {"1703": {"Name": "BMW M4 GT3"}}}"#).unwrap();

        let listener = RaceRoom::with_car_loader(true, move || Some(general));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while listener.car_names.read().unwrap().get(1703).is_none() {
            assert!(std::time::Instant::now() < deadline, "threadLoadRRECars filled the table");
            std::thread::yield_now();
        }
    }

    #[test]
    fn load_r3e_cars_parsing() {
        let n = RaceRoomCarNames::parse(
            r#"{"cars": {"10": {"Name": "A"}, " 11 ": {"Name": "B"}, "x": {"Name": "C"}, "12": {"Name": "D"}}}"#,
        );
        assert_eq!(n.get(10), Some("A"));
        assert_eq!(n.get(11), Some("B"));
        assert_eq!(n.get(12), Some("D"), "a bad entry is skipped, later ones still load");
        let n = RaceRoomCarNames::parse(
            r#"{"cars": {"99999999999": {"Name": "Z"}, "1": {"Name": "A"}, "2": {"Id": 2}, "3": {"Name": "C"}}}"#,
        );
        assert_eq!((n.get(1), n.get(2), n.get(3)), (Some("A"), None, Some("C")));
        assert_eq!(RaceRoomCarNames::parse("not json"), RaceRoomCarNames::default());
        assert_eq!(RaceRoomCarNames::load(Path::new("/nonexistent/r3e-data.json")), RaceRoomCarNames::default());
        assert_eq!(parse_car_key("1_000"), Some(CarKey::Id(1000)));
        assert_eq!(parse_car_key(" -7\n"), Some(CarKey::Id(-7)));
        assert_eq!(parse_car_key("1__0"), None);
        assert_eq!(parse_car_key("99999999999"), Some(CarKey::Unmatchable));
        assert_eq!(parse_car_key(""), None);
        assert_eq!(parse_car_key("　١_٧٠٣　"), Some(CarKey::Id(1703)));
        assert_eq!(parse_car_key(&format!("{}7", "０_".repeat(4299))), Some(CarKey::Id(7)));
        assert_eq!(parse_car_key(&format!("{}7", "０_".repeat(4300))), None);
    }

    #[test]
    fn native_getters_keep_scalar_kinds_and_pause_precedence() {
        use LegacyValue::{Float, Int, Str};
        let blocks = FakeBlocks::default();
        let mut source = listener(&blocks, true);
        let mut telemetry = Telemetry::off();
        telemetry.speed = 71.0;
        assert!(source.poll(&mut telemetry).unwrap(), "receiveData is unconditional");
        assert_eq!(telemetry.speed, 71.0);
        for name in ["speed", "car_name", "track_loc", "unknown"] {
            assert!(source.channel_read_raised(name), "the gate reads smm first: {name}");
        }
        blocks.set(RACEROOM_MAPPING, driving_block().build());
        assert!(source.is_running().unwrap());
        for (name, expected) in [
            ("rpm", Str("5729".into())),
            ("max_rpm", Str("8594".into())),
            ("gear", Str("4".into())),
            ("speed", Str("52".into())),
            ("car_name", Str("BMW M4 GT3".into())),
            ("track_loc", Int(0)),
            ("vertical_velocity", Float(-0.123457)),
            ("lateral_acceleration", Float(2.5)),
            ("oil_temperature", Float(105.0)),
            ("rumble_pitch_lf", Int(40)),
            ("rumble_pitch_rf", Int(0)),
            ("flag", Int(0)),
            ("raw_flag", Int(-1)),
            ("unknown", Int(0)),
        ] {
            assert_eq!(source.native_channel(name), Some(expected), "{name}");
            assert!(!source.channel_read_raised(name));
        }
        for (name, offset) in [("rpm", 1396), ("max_rpm", 1400), ("speed", 1392)] {
            let mut block = driving_block();
            block.f32(offset, f32::NAN);
            blocks.set(RACEROOM_MAPPING, block.build());
            assert!(source.is_running().unwrap());
            assert!(source.channel_read_raised(name), "{name}");
            source.frame.as_mut().unwrap().game_paused = 1;
            assert_eq!(source.native_channel(name), Some(Int(0)), "gate precedes conversion");
        }
        source.frame.as_mut().unwrap().game_paused = 0;
        source.frame.as_mut().unwrap().control_type = 1;
        for name in ["speed", "car_name", "unknown"] {
            assert_eq!(source.native_channel(name), Some(Int(0)));
        }
        source.set_pause_when_not_playing(false);
        assert!(source.channel_read_raised("speed"));
        assert_eq!(source.native_channel("unknown"), Some(Int(0)));
    }

    #[test]
    fn native_receive_does_not_replay_raceroom_getters() {
        let blocks = FakeBlocks::default();
        blocks.set(RACEROOM_MAPPING, driving_block().build());
        let mut source = listener(&blocks, true);
        assert!(source.is_running().unwrap());
        let mut frame = Telemetry::off();
        frame.rpm = 123.0;
        frame.vehicle = "retained compatibility frame".into();
        assert!(source.receive_native(&mut frame).unwrap());
        assert_eq!(frame.rpm, 123.0, "original receiveData only returns True");
        assert_eq!(frame.vehicle, "retained compatibility frame");
    }

    #[test]
    fn native_pause_raceroom_reads_required_current_value_once_per_getter() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Mutex;
        let blocks = FakeBlocks::default();
        let mut block = driving_block();
        block.i32(1388, 1);
        blocks.set(RACEROOM_MAPPING, block.build());
        let mut source = listener(&blocks, true);
        assert!(source.is_running().unwrap());
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
        assert_eq!(source.read_native("rpm"), crate::NativeRead::Value(LegacyValue::from("5729")));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        *value.lock().unwrap() = Some(LegacyValue::from("y"));
        assert_eq!(source.read_native("rpm"), crate::NativeRead::Value(LegacyValue::Int(0)));
        source.frame.as_mut().unwrap().game_paused = 1;
        for raw in [Some(LegacyValue::from("invalid")), Some(LegacyValue::None), None] {
            *value.lock().unwrap() = raw;
            assert_eq!(source.read_native("unknown"), crate::NativeRead::Raised);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 5);
        *value.lock().unwrap() = Some(LegacyValue::Bool(false));
        assert_eq!(
            source.read_native("rpm"),
            crate::NativeRead::Value(LegacyValue::Int(0)),
            "game_paused stays unconditional"
        );
    }

    #[test]
    fn car_table_retains_raw_json_names_and_python_json_constants() {
        use LegacyValue::{Bool, Int, List, None as Null, Str};
        let names = RaceRoomCarNames::parse(
            r#"{"cars":{"١٧٠٣":{"Name":null},"2":{"Name":true},"3":{"Name":[1,"x"]},"4":{"Name":-0},"5":{"Name":NaN},"6":{"Name":Infinity},"7":{"Name":-Infinity}," 2 ":{"Name":false},"8":{"other":0},"9":[]}}"#,
        );
        assert_eq!(names.native(1703), Some(&Null));
        assert_eq!(names.native(2), Some(&Bool(false)), "later integer-equivalent key replaces value");
        assert_eq!(names.native(3), Some(&List(vec![Int(1), Str("x".into())])));
        assert_eq!(names.get(3), Some("[1, 'x']"));
        assert_eq!(names.native(4), Some(&Int(0)));
        // The JSON reader hands out its `NaN` constant as one shared object: the stored name is that
        // handle, and reading through it gives the float.
        let nan = names.native(5).expect("a NaN name is stored");
        assert!(matches!(nan, LegacyValue::Object(_)), "{nan:?}");
        assert!(matches!(nan.resolved(), LegacyValue::Float(v) if v.is_nan()));
        assert_eq!(names.get(5), Some("nan"));
        assert_eq!(names.native(6), Some(&LegacyValue::Float(f64::INFINITY)));
        assert_eq!(names.native(7), Some(&LegacyValue::Float(f64::NEG_INFINITY)));
        assert_eq!(names.native(8), None);
        let blocks = FakeBlocks::default();
        blocks.set(RACEROOM_MAPPING, driving_block().build());
        let mut source = listener(&blocks, true);
        source.set_car_names(names);
        assert!(source.is_running().unwrap());
        assert_eq!(source.native_channel("car_name"), Some(Null));
        assert!(!source.channel_read_raised("car_name"), "a stored None is a successful lookup");
        source.frame.as_mut().unwrap().model_id = 10;
        assert_eq!(source.native_channel("car_name"), Some(Int(0)));
    }
}
#[test]
fn loader_publishes_each_name_before_the_next_entry_and_keeps_existing_keys() {
    let table = RwLock::new(RaceRoomCarNames::parse(r#"{"cars":{"9":{"Name":"Old"}}}"#));
    let mut visible = Vec::new();
    publish_car_names(
        r#"{"cars":{"1":{"Name":"First"},"bad":{"Name":"Skip"},"2":{"Name":null},"3":{"Name":"Last"}}}"#,
        &table,
        || {
            let names = table.try_read().expect("entry lock released before the next source assignment");
            visible.push((names.get(1).map(str::to_owned), names.native(2).cloned(), names.get(3).map(str::to_owned)));
            assert_eq!(names.get(9), Some("Old"));
        },
    );
    assert_eq!(
        visible,
        [
            (Some("First".into()), None, None),
            (Some("First".into()), Some(LegacyValue::None), None),
            (Some("First".into()), Some(LegacyValue::None), Some("Last".into())),
        ]
    );
    publish_car_names("invalid json", &table, || panic!("invalid file must not replace the table"));
    assert_eq!(table.read().unwrap().get(9), Some("Old"));
}
