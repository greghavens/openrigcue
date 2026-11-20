//! Datagram decoding of Project Motor Racing.
//!
//! Every datagram starts with `u8 packet type`, `u16 packet version`. Fields are little-endian and read in
//! sequence; a read past the end fails the whole packet.
//! Only the values the listener uses are kept;
//! the other fields are skipped by size.

use super::super::support::wire::{decode_utf8_ignoring_errors, Fields};

/// Why a packet could not be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Malformed {
    #[error("read past the end of the datagram")]
    Short,
    /// The race session state is not one of its members (0 to 2).
    #[error("race session state {0} has no member")]
    UnknownSessionState(u8),
}

/// The packet type: the first byte of every datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketKind {
    RaceInfo,
    ParticipantRaceState,
    ParticipantVehicleTelemetry,
    SessionStopped,
}

impl PacketKind {
    /// The packet type of a byte; `None` for an unknown value.
    pub fn from_byte(value: u8) -> Option<Self> {
        match value {
            0 => Some(PacketKind::RaceInfo),
            1 => Some(PacketKind::ParticipantRaceState),
            2 => Some(PacketKind::ParticipantVehicleTelemetry),
            3 => Some(PacketKind::SessionStopped),
            _ => None,
        }
    }
}

/// Three floats.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Vec3 {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

/// The participant race state values the listener uses.
#[derive(Debug, Clone, PartialEq)]
pub struct RaceState {
    pub vehicle_id: i32,
    pub is_player: bool,
    pub vehicle_name: String,
}

/// The wheel telemetry values the listener uses.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Wheel {
    pub slip_ratio: f64,
    pub spring_strain: f64,
}

/// The vehicle telemetry values the listener uses.
#[derive(Debug, Clone, PartialEq)]
pub struct VehicleTelemetry {
    pub vehicle_id: i32,
    /// In packet order (front-left, front-right, rear-left, rear-right for a car).
    pub wheels: Vec<Wheel>,
    /// The quaternion: `(x, y, z, w)`.
    pub quaternion: [f64; 4],
    pub velocity_local: Vec3,
    pub acceleration_local: Vec3,
    pub overall_speed: f64,
    pub engine_rpm: f64,
    pub gear: i32,
}

/// Bytes of one wheel block.
const WHEEL_LEN: usize = 124;
/// Offsets inside a wheel block: `slip_ratio` follows `contact_material_hash` (i32), `ang_vel`,
/// `linear_speed`, `slide_ls`, `force_ls`, `moment_ls` (Vec3 each), `contact_radius`, `pressure`,
/// `inclination`; `spring_strain` follows `slip_angle`, `tread` (Vec3), `carcass`, `internal_air`,
/// `well_air`, `rim`, `brake`.
const WHEEL_SLIP_RATIO: usize = 60;
const WHEEL_SPRING_STRAIN: usize = 100;
/// `pos_ws` (Vec3) before the quaternion.
const POSITION_LEN: usize = 12;
/// `angular_velocity_ws`, `angular_velocity_ls`, `velocity_ws` (Vec3 each) before `velocity_ls`.
const BEFORE_VELOCITY_LOCAL: usize = 36;
/// `acceleration_ws` (Vec3) between `velocity_ls` and `acceleration_ls`.
const BEFORE_ACCELERATION_LOCAL: usize = 12;
/// `forward_speed`, `sideslip` between `overall_speed` and `engine_rpm`.
const BEFORE_ENGINE_RPM: usize = 8;
/// From `engine_rev_ratio` to `output_efficiency` (27 floats), then the seven `u8` flags from
/// `starter_active` to `speed_limiter_active`.
const ENGINE_TO_FLAGS_LEN: usize = 27 * 4 + 7;
/// Two floats per gear in the optional gear table.
const GEAR_ENTRY_LEN: usize = 8;
/// `steering`, `throttle`, `brake`, `clutch`, `handbrake` before `gear`.
const CONTROLS_LEN: usize = 20;

/// Sequential little-endian reader.
struct Cursor<'a> {
    data: &'a [u8],
    ofs: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Cursor { data, ofs: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.ofs)
    }

    /// The next `len` bytes; fails when the datagram is shorter.
    fn take(&mut self, len: usize) -> Result<Fields<'a>, Malformed> {
        let block = Fields::new(self.data).bytes(self.ofs, len).map_err(|_| Malformed::Short)?;
        self.ofs += len;
        Ok(Fields::new(block))
    }

    fn skip(&mut self, len: usize) -> Result<(), Malformed> {
        self.take(len).map(|_| ())
    }

    /// One byte.
    fn u8(&mut self) -> Result<u8, Malformed> {
        self.take(1)?.u8(0).map_err(|_| Malformed::Short)
    }

    /// A little-endian `i32`.
    fn i32(&mut self) -> Result<i32, Malformed> {
        self.take(4)?.i32(0).map_err(|_| Malformed::Short)
    }

    /// A little-endian `f32`.
    fn f32(&mut self) -> Result<f64, Malformed> {
        self.take(4)?.f32(0).map_err(|_| Malformed::Short)
    }

    /// Three little-endian `f32`.
    fn vec3(&mut self) -> Result<Vec3, Malformed> {
        Ok(Vec3 { x: self.f32()?, y: self.f32()?, z: self.f32()? })
    }

    /// A length-prefixed string: a length byte, then that many bytes decoded as UTF-8 with invalid
    /// sequences dropped. The slice is cut at the end of the datagram without failing, but the offset still
    /// advances by the full length, so the next read fails.
    fn pascal_string(&mut self) -> Result<String, Malformed> {
        let len = usize::from(self.u8()?);
        let start = self.ofs.min(self.data.len());
        let end = (self.ofs + len).min(self.data.len());
        self.ofs += len;
        Ok(decode_utf8_ignoring_errors(&self.data[start..end]))
    }

    /// Skips a float list the listener does not use: a count byte, then that many
    /// floats.
    fn skip_float_list(&mut self) -> Result<(), Malformed> {
        let count = usize::from(self.u8()?);
        self.skip(count * 4)
    }

    /// The `packet_type` byte and `u16 packet_version` that open every packet. The type was validated by
    /// the receive loop before decoding.
    fn header(&mut self) -> Result<(), Malformed> {
        self.skip(3)
    }
}

/// Decodes a race-info packet. The listener keeps the result without reading it, so
/// only success or failure matters: six strings (`track` to `game_mode`), five floats (`layout_length` to
/// `track_temp`), `is_laps`, `state` (must be a race session state) and `num_participants`.
pub fn check_race_info(data: &[u8]) -> Result<(), Malformed> {
    let mut c = Cursor::new(data);
    c.header()?;
    for _ in 0..6 {
        c.pascal_string()?;
    }
    c.skip(5 * 4 + 1)?;
    let state = c.u8()?;
    c.skip(1)?;
    if state > 2 {
        return Err(Malformed::UnknownSessionState(state));
    }
    Ok(())
}

/// Decodes a participant race state: `i32 vehicle_id`, `u8 is_player`, four strings
/// (`vehicle_name`, `driver_name`, `livery_id`, `vehicle_class`), `i32 race_pos`, `i32 current_lap`, three
/// floats (`current_lap_time`, `best_lap_time`, `lap_progress`), `i32 current_sector`, two float lists
/// (`current_sector_times`, `best_sector_times`), three `u8` (`in_pits`, `session_finished`, `dq`) and
/// `u32 flags`.
pub fn parse_race_state(data: &[u8]) -> Result<RaceState, Malformed> {
    let mut c = Cursor::new(data);
    c.header()?;
    let vehicle_id = c.i32()?;
    let is_player = c.u8()? != 0;
    let vehicle_name = c.pascal_string()?;
    for _ in 0..3 {
        c.pascal_string()?;
    }
    c.skip(2 * 4 + 3 * 4 + 4)?;
    c.skip_float_list()?;
    c.skip_float_list()?;
    c.skip(3 + 4)?;
    Ok(RaceState { vehicle_id, is_player, vehicle_name })
}

/// Decodes a vehicle telemetry packet.
///
/// After `i32 vehicle_id` comes a wheel count and up to that many 124-byte wheel blocks (the loop stops
/// early when a whole block no longer fits). Then the body: `pos_ws`, the quaternion, six vectors, 31
/// floats from `overall_speed` to `output_efficiency` and seven flag bytes. Two optional sections follow,
/// each present only while bytes remain: a gear table (count byte, two floats per gear, unchecked), and a
/// float list (count byte, at most as many floats as fit) followed by `load_bias` when four bytes fit.
/// The packet ends with five control floats and `i32 gear`.
pub fn parse_vehicle_telemetry(data: &[u8]) -> Result<VehicleTelemetry, Malformed> {
    let mut c = Cursor::new(data);
    c.header()?;
    let vehicle_id = c.i32()?;
    let wheels = read_wheels(&mut c)?;
    c.skip(POSITION_LEN)?;
    let quaternion = [c.f32()?, c.f32()?, c.f32()?, c.f32()?];
    c.skip(BEFORE_VELOCITY_LOCAL)?;
    let velocity_local = c.vec3()?;
    c.skip(BEFORE_ACCELERATION_LOCAL)?;
    let acceleration_local = c.vec3()?;
    let overall_speed = c.f32()?;
    c.skip(BEFORE_ENGINE_RPM)?;
    let engine_rpm = c.f32()?;
    c.skip(ENGINE_TO_FLAGS_LEN)?;
    skip_gear_table(&mut c)?;
    skip_load_section(&mut c)?;
    c.skip(CONTROLS_LEN)?;
    let gear = c.i32()?;
    Ok(VehicleTelemetry {
        vehicle_id,
        wheels,
        quaternion,
        velocity_local,
        acceleration_local,
        overall_speed,
        engine_rpm,
        gear,
    })
}

/// The wheel blocks: up to the wheel count, stopping when a whole block no longer fits.
fn read_wheels(c: &mut Cursor) -> Result<Vec<Wheel>, Malformed> {
    let count = c.u8()?;
    let mut wheels = Vec::with_capacity(usize::from(count));
    for _ in 0..count {
        if c.remaining() < WHEEL_LEN {
            break;
        }
        let block = c.take(WHEEL_LEN)?;
        let read = |offset| block.f32(offset).map_err(|_| Malformed::Short);
        wheels.push(Wheel { slip_ratio: read(WHEEL_SLIP_RATIO)?, spring_strain: read(WHEEL_SPRING_STRAIN)? });
    }
    Ok(wheels)
}

/// The gear table, present while bytes remain: a count byte, then two floats per gear (no bound check inside the loop).
fn skip_gear_table(c: &mut Cursor) -> Result<(), Malformed> {
    if c.remaining() == 0 {
        return Ok(());
    }
    let gears = usize::from(c.u8()?);
    c.skip(gears * GEAR_ENTRY_LEN)
}

/// The float list, present while bytes remain: a count byte, then at most as many floats as fit; then
/// `load_bias` when four bytes fit.
fn skip_load_section(c: &mut Cursor) -> Result<(), Malformed> {
    if c.remaining() == 0 {
        return Ok(());
    }
    let count = usize::from(c.u8()?);
    c.skip(count.min(c.remaining() / 4) * 4)?;
    if c.remaining() >= 4 {
        c.skip(4)?;
    }
    Ok(())
}

#[cfg(test)]
pub mod build {
    //! Datagrams laid out as the decoder reads them.

    /// Little-endian datagram builder.
    #[derive(Default)]
    pub struct Datagram(pub Vec<u8>);

    impl Datagram {
        pub fn new(packet_type: u8) -> Self {
            Datagram(vec![packet_type, 1, 0])
        }
        pub fn u8(mut self, v: u8) -> Self {
            self.0.push(v);
            self
        }
        pub fn i32(mut self, v: i32) -> Self {
            self.0.extend(v.to_le_bytes());
            self
        }
        pub fn u32(mut self, v: u32) -> Self {
            self.0.extend(v.to_le_bytes());
            self
        }
        pub fn f32(mut self, v: f32) -> Self {
            self.0.extend(v.to_le_bytes());
            self
        }
        pub fn floats(self, values: &[f32]) -> Self {
            values.iter().fold(self, |d, &v| d.f32(v))
        }
        pub fn pascal(mut self, text: &[u8]) -> Self {
            self.0.push(u8::try_from(text.len()).unwrap());
            self.0.extend(text);
            self
        }
    }

    /// A wheel block: `slip_ratio` and `spring_strain` as given, every other float distinct.
    pub fn wheel(d: Datagram, slip_ratio: f32, spring_strain: f32) -> Datagram {
        let mut d = d.i32(77); // contact_material_hash
        for i in 0..30u8 {
            let value = match 4 + 4 * usize::from(i) {
                60 => slip_ratio,
                100 => spring_strain,
                _ => 1000.0 + f32::from(i),
            };
            d = d.f32(value);
        }
        d
    }

    /// Everything after the wheels up to the optional sections, with the used values as given.
    pub struct Body {
        pub quaternion: [f32; 4],
        pub velocity_local: [f32; 3],
        pub acceleration_local: [f32; 3],
        pub overall_speed: f32,
        pub engine_rpm: f32,
    }

    pub fn body(d: Datagram, b: &Body) -> Datagram {
        let mut d = d.floats(&[9.0, 9.0, 9.0]).floats(&b.quaternion); // pos_ws, quat
        d = d.floats(&[8.0; 9]).floats(&b.velocity_local); // ang_ws, ang_ls, vel_ws, vel_ls
        d = d.floats(&[7.0; 3]).floats(&b.acceleration_local); // acc_ws, acc_ls
        d = d.f32(b.overall_speed).floats(&[6.0, 6.0]).f32(b.engine_rpm); // overall_speed .. engine_rpm
        d = d.floats(&[5.0; 27]); // engine_rev_ratio .. output_efficiency
        (0..7).fold(d, |d, _| d.u8(1)) // starter_active .. speed_limiter_active
    }

    /// `steering`, `throttle`, `brake`, `clutch`, `handbrake`, `gear`.
    pub fn controls(d: Datagram, gear: i32) -> Datagram {
        d.floats(&[0.1, 0.2, 0.3, 0.4, 0.5]).i32(gear)
    }
}

#[cfg(test)]
mod tests {
    use super::build::*;
    use super::*;

    fn sample_body() -> Body {
        Body {
            quaternion: [0.1, 0.2, 0.3, 0.9],
            velocity_local: [1.5, 2.5, 3.5],
            acceleration_local: [-0.5, 0.25, 4.0],
            overall_speed: 42.75,
            engine_rpm: 6123.5,
        }
    }

    fn four_wheel_packet() -> Datagram {
        let mut d = Datagram::new(2).i32(7).u8(4);
        for i in 0..4u8 {
            d = wheel(d, -0.1 * f32::from(i + 1), 0.01 * f32::from(i + 1));
        }
        body(d, &sample_body())
    }

    #[test]
    fn packet_kinds_follow_udp_packet_type() {
        // Packet types: RaceInfo 0 .. SessionStopped 3; anything else is unknown.
        assert_eq!(PacketKind::from_byte(0), Some(PacketKind::RaceInfo));
        assert_eq!(PacketKind::from_byte(2), Some(PacketKind::ParticipantVehicleTelemetry));
        assert_eq!(PacketKind::from_byte(3), Some(PacketKind::SessionStopped));
        assert_eq!(PacketKind::from_byte(4), None);
    }

    #[test]
    fn vehicle_telemetry_fields() {
        // Vehicle telemetry: wheels, quat, velocity_ls, acceleration_ls, speeds, gear.
        let d = controls(four_wheel_packet().u8(0).u8(0).f32(0.5), -1);
        let vt = parse_vehicle_telemetry(&d.0).unwrap();
        assert_eq!(vt.vehicle_id, 7);
        assert_eq!(vt.wheels.len(), 4);
        assert_eq!(vt.wheels[2], Wheel { slip_ratio: f64::from(-0.3f32), spring_strain: f64::from(0.03f32) });
        assert_eq!(vt.quaternion, [0.1f32, 0.2, 0.3, 0.9].map(f64::from));
        assert_eq!(vt.velocity_local, Vec3 { x: 1.5, y: 2.5, z: 3.5 });
        assert_eq!(vt.acceleration_local, Vec3 { x: -0.5, y: 0.25, z: 4.0 });
        assert_eq!((vt.overall_speed, vt.engine_rpm, vt.gear), (42.75, 6123.5, -1));
    }

    #[test]
    fn optional_sections_are_read_only_while_bytes_remain() {
        // Vehicle telemetry: the gear table and the float list are read while bytes remain; here the gear table
        // holds two gears and the float list three floats followed by load_bias.
        let d = four_wheel_packet().u8(2).floats(&[1.0, 2.0, 3.0, 4.0]).u8(3).floats(&[1.0, 2.0, 3.0]).f32(0.5);
        let d = controls(d, 4);
        assert_eq!(parse_vehicle_telemetry(&d.0).unwrap().gear, 4);
    }

    #[test]
    fn float_list_count_is_capped_by_the_remaining_bytes() {
        // Vehicle telemetry: a float list longer than the datagram consumes every remaining float,
        // so the controls that follow can no longer be read.
        let d = controls(four_wheel_packet().u8(0).u8(200), 3);
        assert_eq!(parse_vehicle_telemetry(&d.0), Err(Malformed::Short));
    }

    #[test]
    fn gear_table_is_not_bounds_checked() {
        // Vehicle telemetry: the gear table reads two floats per gear without a check.
        let d = controls(four_wheel_packet().u8(9), 3);
        assert_eq!(parse_vehicle_telemetry(&d.0), Err(Malformed::Short));
    }

    #[test]
    fn wheel_loop_stops_when_a_block_no_longer_fits() {
        // Vehicle telemetry: the wheel loop stops when a block no longer fits; the body then runs out of bytes.
        let d = Datagram::new(2).i32(7).u8(1).floats(&[0.0; 20]);
        assert_eq!(parse_vehicle_telemetry(&d.0), Err(Malformed::Short));
        let no_wheels = controls(body(Datagram::new(2).i32(7).u8(0), &sample_body()).u8(0).u8(0).f32(0.5), 2);
        assert!(parse_vehicle_telemetry(&no_wheels.0).unwrap().wheels.is_empty());
    }

    #[test]
    fn controls_are_mandatory() {
        // Vehicle telemetry: steering .. gear follow the optional sections unconditionally.
        assert_eq!(parse_vehicle_telemetry(&four_wheel_packet().0), Err(Malformed::Short));
        // `load_bias` takes four bytes whenever they fit, so a packet without load_bias
        // reads its controls four bytes late and runs out.
        let without_load_bias = controls(four_wheel_packet().u8(0).u8(0), 3);
        assert_eq!(parse_vehicle_telemetry(&without_load_bias.0), Err(Malformed::Short));
    }

    fn race_state(is_player: u8, name: &[u8]) -> Datagram {
        Datagram::new(1)
            .i32(12)
            .u8(is_player)
            .pascal(name)
            .pascal(b"Driver")
            .pascal(b"livery")
            .pascal(b"GT3")
            .i32(3)
            .i32(5)
            .floats(&[90.0, 88.5, 0.4])
            .i32(2)
            .u8(2)
            .floats(&[30.0, 31.0])
            .u8(0)
            .u8(0)
            .u8(1)
            .u8(0)
            .u32(0)
    }

    #[test]
    fn race_state_fields() {
        // Participant race state: vehicle_id, is_player, vehicle_name (invalid UTF-8 dropped).
        let rs = parse_race_state(&race_state(1, b"Car \xffX").0).unwrap();
        assert_eq!(rs, RaceState { vehicle_id: 12, is_player: true, vehicle_name: "Car X".into() });
        assert!(!parse_race_state(&race_state(0, b"A").0).unwrap().is_player);
    }

    #[test]
    fn race_state_needs_every_field() {
        // A participant race state is read up to `flags` (u32); one byte less fails.
        let mut d = race_state(1, b"A").0;
        d.pop();
        assert_eq!(parse_race_state(&d), Err(Malformed::Short));
    }

    #[test]
    fn pascal_strings_are_cut_at_the_end_but_advance_fully() {
        // A string cut at the end of the datagram does not fail, but the offset advances by the full length.
        let d = Datagram::new(1).i32(1).u8(1).u8(50).0;
        let mut with_text = d.clone();
        with_text.extend(b"abc");
        assert_eq!(parse_race_state(&with_text), Err(Malformed::Short));
        let mut c = Cursor::new(&with_text);
        c.skip(8).unwrap();
        assert_eq!(c.pascal_string(), Ok("abc".into()));
        assert_eq!(c.u8(), Err(Malformed::Short));
    }

    fn race_info(state: u8) -> Datagram {
        let d = Datagram::new(0);
        let d = [b"track".as_slice(), b"gp", b"s1", b"dry", b"race", b"mp"].iter().fold(d, |d, s| d.pascal(s));
        d.floats(&[4.5, 600.0, 0.0, 21.0, 30.0]).u8(1).u8(state).u8(20)
    }

    #[test]
    fn race_info_validation() {
        // Race info: the session state accepts 0, 1 and 2 only.
        assert_eq!(check_race_info(&race_info(2).0), Ok(()));
        assert_eq!(check_race_info(&race_info(3).0), Err(Malformed::UnknownSessionState(3)));
        let mut short = race_info(1).0;
        short.pop();
        assert_eq!(check_race_info(&short), Err(Malformed::Short));
    }
}
