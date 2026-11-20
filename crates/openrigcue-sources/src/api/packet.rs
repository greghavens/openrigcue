//! Packets of the public telemetry API (UDP port 33001), laid out with x86-64 C struct
//! alignment: little-endian, 4-byte numbers aligned to 4.
//!
//! | tag | version | packet | size |
//! |---|---|---|---|
//! | `api` / `srs` | 101 | [`BasicPacket`] | 28 |
//! | `api` / `srs` | 102 | [`MotionPacket`] | 236 |
//! | `api` / `srs` | 103 | [`MotionPacket`] with longitudinal velocity and steering angle | 244 |
//! | `lla` | any | [`FanPacket`] | 28 |

/// Bytes of the header every packet starts with: 3-byte tag, 1 byte padding, `u32` version.
pub const HEADER_LEN: usize = 8;
/// Size of a version 101 or `lla` packet.
pub const BASIC_LEN: usize = 28;
/// Size of a version 102 packet.
pub const MOTION_102_LEN: usize = 236;
/// Size of a version 103 packet.
pub const MOTION_103_LEN: usize = 244;
/// Length of the three text fields of 102/103.
pub const TEXT_LEN: usize = 50;

/// The tag and version of a datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub tag: [u8; 3],
    pub version: u32,
}

impl Header {
    /// Decodes the first [`HEADER_LEN`] bytes; `None` for a shorter datagram.
    pub fn parse(datagram: &[u8]) -> Option<Self> {
        let bytes = datagram.get(..HEADER_LEN)?;
        let mut reader = Reader::new(bytes);
        Some(Header { tag: reader.tag(), version: reader.u32() })
    }
}

/// The version 101 packet.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BasicPacket {
    pub speed: f32,
    pub wind_direction: f32,
    pub rpm: f32,
    pub max_rpm: f32,
    pub gear: i32,
}

impl BasicPacket {
    /// Decodes a datagram of exactly [`BASIC_LEN`] bytes.
    pub fn parse(datagram: &[u8]) -> Option<Self> {
        let mut reader = Reader::exact(datagram, BASIC_LEN)?;
        Some(BasicPacket {
            speed: reader.f32(),
            wind_direction: reader.f32(),
            rpm: reader.f32(),
            max_rpm: reader.f32(),
            gear: reader.i32(),
        })
    }
}

/// The `lla` packet: the fan powers in place of speed and wind direction.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FanPacket {
    pub version: u32,
    pub left_fan_power: f32,
    pub right_fan_power: f32,
    pub rpm: f32,
    pub max_rpm: f32,
    pub gear: i32,
}

impl FanPacket {
    /// Decodes a datagram of exactly [`BASIC_LEN`] bytes.
    pub fn parse(datagram: &[u8]) -> Option<Self> {
        let mut reader = Reader::exact(datagram, BASIC_LEN)?;
        Some(FanPacket {
            version: Header::parse(datagram)?.version,
            left_fan_power: reader.f32(),
            right_fan_power: reader.f32(),
            rpm: reader.f32(),
            max_rpm: reader.f32(),
            gear: reader.i32(),
        })
    }
}

/// Front-left, front-right, rear-left, rear-right.
pub type Corners<T> = [T; 4];

/// The version 102 and 103 packets.
#[derive(Debug, Clone, PartialEq)]
pub struct MotionPacket {
    pub game: [u8; TEXT_LEN],
    pub vehicle_name: [u8; TEXT_LEN],
    pub location: [u8; TEXT_LEN],
    /// m/s.
    pub speed: f32,
    pub rpm: f32,
    pub max_rpm: f32,
    /// -1 reverse, 0 neutral, 1.. forward.
    pub gear: i32,
    /// Degrees.
    pub pitch: f32,
    pub roll: f32,
    pub yaw: f32,
    pub lateral_velocity: f32,
    /// m/s; version 103 only.
    pub longitudinal_velocity: Option<f32>,
    /// G.
    pub lateral_acceleration: f32,
    pub vertical_acceleration: f32,
    pub longitudinal_acceleration: f32,
    pub suspension_travel: Corners<f32>,
    /// 1 = rumble surface.
    pub wheel_terrain: Corners<u32>,
    /// Version 103 only.
    pub steering_wheel_angle: Option<f32>,
}

impl MotionPacket {
    /// Decodes a version 102 datagram of exactly [`MOTION_102_LEN`] bytes.
    pub fn parse_102(datagram: &[u8]) -> Option<Self> {
        Reader::exact(datagram, MOTION_102_LEN).map(|reader| Self::parse(reader, false))
    }

    /// Decodes a version 103 datagram of exactly [`MOTION_103_LEN`] bytes.
    pub fn parse_103(datagram: &[u8]) -> Option<Self> {
        Reader::exact(datagram, MOTION_103_LEN).map(|reader| Self::parse(reader, true))
    }

    fn parse(mut reader: Reader<'_>, extended: bool) -> Self {
        let game = reader.text();
        let vehicle_name = reader.text();
        let location = reader.text();
        reader.align4();
        let speed = reader.f32();
        let rpm = reader.f32();
        let max_rpm = reader.f32();
        let gear = reader.i32();
        let pitch = reader.f32();
        let roll = reader.f32();
        let yaw = reader.f32();
        let lateral_velocity = reader.f32();
        let longitudinal_velocity = extended.then(|| reader.f32());
        let lateral_acceleration = reader.f32();
        let vertical_acceleration = reader.f32();
        let longitudinal_acceleration = reader.f32();
        let suspension_travel = [reader.f32(), reader.f32(), reader.f32(), reader.f32()];
        let wheel_terrain = [reader.u32(), reader.u32(), reader.u32(), reader.u32()];
        let steering_wheel_angle = extended.then(|| reader.f32());
        MotionPacket {
            game,
            vehicle_name,
            location,
            speed,
            rpm,
            max_rpm,
            gear,
            pitch,
            roll,
            yaw,
            lateral_velocity,
            longitudinal_velocity,
            lateral_acceleration,
            vertical_acceleration,
            longitudinal_acceleration,
            suspension_travel,
            wheel_terrain,
            steering_wheel_angle,
        }
    }
}

/// Sequential little-endian reader over a datagram whose length has already been checked.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Reader { bytes, at: 0 }
    }

    /// A reader past the header of a datagram of exactly `len` bytes (other lengths are rejected).
    fn exact(datagram: &'a [u8], len: usize) -> Option<Self> {
        (datagram.len() == len).then_some(Reader { bytes: datagram, at: HEADER_LEN })
    }

    fn take<const N: usize>(&mut self) -> [u8; N] {
        let bytes = self.bytes[self.at..self.at + N].try_into().expect("length checked");
        self.at += N;
        bytes
    }

    /// The 3-byte tag and the padding byte after it.
    fn tag(&mut self) -> [u8; 3] {
        let tag = self.take::<3>();
        self.at += 1;
        tag
    }

    fn text(&mut self) -> [u8; TEXT_LEN] {
        self.take()
    }

    fn align4(&mut self) {
        self.at = self.at.next_multiple_of(4);
    }

    fn f32(&mut self) -> f32 {
        f32::from_le_bytes(self.take())
    }

    fn i32(&mut self) -> i32 {
        i32::from_le_bytes(self.take())
    }

    fn u32(&mut self) -> u32 {
        u32::from_le_bytes(self.take())
    }
}

#[cfg(test)]
pub(crate) mod build {
    //! Packs test datagrams in the packet layouts.

    use super::*;

    pub(crate) fn header(tag: &[u8; 3], version: u32) -> Vec<u8> {
        let mut bytes = tag.to_vec();
        bytes.push(0);
        bytes.extend_from_slice(&version.to_le_bytes());
        bytes
    }

    /// A 28-byte packet: tag, version, four floats and an integer.
    pub(crate) fn basic(tag: &[u8; 3], version: u32, floats: [f32; 4], gear: i32) -> Vec<u8> {
        let mut bytes = header(tag, version);
        for f in floats {
            bytes.extend_from_slice(&f.to_le_bytes());
        }
        bytes.extend_from_slice(&gear.to_le_bytes());
        bytes
    }

    fn text(value: &str) -> [u8; TEXT_LEN] {
        let mut field = [0u8; TEXT_LEN];
        field[..value.len()].copy_from_slice(value.as_bytes());
        field
    }

    /// A version 102 or 103 datagram, from a packet value.
    pub(crate) fn motion(tag: &[u8; 3], version: u32, game: &str, location: &str, p: &MotionPacket) -> Vec<u8> {
        let mut bytes = header(tag, version);
        bytes.extend_from_slice(&text(game));
        bytes.extend_from_slice(&text("vehicle"));
        bytes.extend_from_slice(&text(location));
        bytes.extend_from_slice(&[0, 0]);
        let mut floats = vec![p.speed, p.rpm, p.max_rpm];
        floats.iter().for_each(|f| bytes.extend_from_slice(&f.to_le_bytes()));
        bytes.extend_from_slice(&p.gear.to_le_bytes());
        floats = vec![p.pitch, p.roll, p.yaw, p.lateral_velocity];
        floats.extend(p.longitudinal_velocity);
        floats.extend([p.lateral_acceleration, p.vertical_acceleration, p.longitudinal_acceleration]);
        floats.extend(p.suspension_travel);
        floats.iter().for_each(|f| bytes.extend_from_slice(&f.to_le_bytes()));
        p.wheel_terrain.iter().for_each(|w| bytes.extend_from_slice(&w.to_le_bytes()));
        if let Some(angle) = p.steering_wheel_angle {
            bytes.extend_from_slice(&angle.to_le_bytes());
        }
        bytes
    }

    /// A motion packet value with distinct numbers in every field.
    pub(crate) fn sample(extended: bool) -> MotionPacket {
        MotionPacket {
            game: text("Game"),
            vehicle_name: text("vehicle"),
            location: text("Track"),
            speed: 27.9,
            rpm: 6543.8,
            max_rpm: 8000.2,
            gear: -1,
            pitch: 1.5,
            roll: -2.5,
            yaw: 3.5,
            lateral_velocity: 0.25,
            longitudinal_velocity: extended.then_some(26.0),
            lateral_acceleration: 0.5,
            vertical_acceleration: 1.25,
            longitudinal_acceleration: -0.75,
            suspension_travel: [0.1, 0.2, 0.3, 0.4],
            wheel_terrain: [1, 0, 2, 1],
            steering_wheel_angle: extended.then_some(45.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::build::*;
    use super::*;

    #[test]
    fn header_telemetry_packet_needs_eight_bytes() {
        assert_eq!(Header::parse(&header(b"api", 103)), Some(Header { tag: *b"api", version: 103 }));
        assert_eq!(Header::parse(&header(b"api", 103)[..7]), None);
    }

    #[test]
    fn packet_101_and_lla_layout() {
        let bytes = basic(b"srs", 101, [12.5, 90.0, 3000.0, 7000.0], 3);
        assert_eq!(bytes.len(), BASIC_LEN);
        assert_eq!(
            BasicPacket::parse(&bytes),
            Some(BasicPacket { speed: 12.5, wind_direction: 90.0, rpm: 3000.0, max_rpm: 7000.0, gear: 3 })
        );
        let fans = FanPacket::parse(&basic(b"lla", 7, [60.0, 40.0, 1.0, 2.0], 0)).unwrap();
        assert_eq!((fans.version, fans.left_fan_power, fans.right_fan_power), (7, 60.0, 40.0));
    }

    #[test]
    fn packet_102_and_103_layout() {
        for (extended, len) in [(false, MOTION_102_LEN), (true, MOTION_103_LEN)] {
            let expected = sample(extended);
            let bytes = motion(b"api", if extended { 103 } else { 102 }, "Game", "Track", &expected);
            assert_eq!(bytes.len(), len);
            let parsed = if extended { MotionPacket::parse_103(&bytes) } else { MotionPacket::parse_102(&bytes) };
            assert_eq!(parsed.as_ref(), Some(&expected));
        }
    }

    #[test]
    fn struct_unpack_rejects_any_other_length() {
        let bytes = basic(b"api", 101, [0.0; 4], 0);
        assert_eq!(BasicPacket::parse(&bytes[..BASIC_LEN - 1]), None);
        let mut longer = bytes.clone();
        longer.push(0);
        assert_eq!(BasicPacket::parse(&longer), None);
        let bytes = motion(b"api", 103, "", "", &sample(true));
        assert_eq!(MotionPacket::parse_102(&bytes), None);
    }
}
