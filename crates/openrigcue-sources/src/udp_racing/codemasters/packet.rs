//! The Codemasters "extradata" motion packet: little-endian float32 fields.
//!
//! Four layouts: extradata=1 (38 floats), extradata=3 (64 floats),
//! the F1 packet (76 floats) and the WRC packet (70 floats). Field order (float index): 0 time,
//! 1 lapTime, 2 lapDistance, 3 totalDistance, 4–6 x/y/z, 7 speed, 8–10 xv/yv/zv, 11–13 rx/ry/rz,
//! 14–16 fx/fy/fz, 17–20 susp_pos_rl/rr/fl/fr, 21–24 susp_vel_rl/rr/fl/fr, 25–28 wheel speeds, 29 throttle,
//! 30 steer, 31 brake, 32 clutch, 33 gear, 34 gforce_lat, 35 gforce_lon, 36 lap, 37 rpm; extradata=3 adds
//! 38–46 (sli_pro_native_support … fuel_capacity), 47 in_pits … 59 team_info, 60 total_laps, 61 track_size,
//! 62 last_lap_time, 63 max_rpm; the WRC layout continues to 69 (vehicle_fia_flags); the F1 packet
//! continues 64 idle_rpm … 70 era, 71 engine_temperature, 72 gforce_vert, 73–75 ang_vel_x/y/z.

/// One of the packet layouts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// extradata=1: 38 floats (152 bytes).
    Extradata1,
    /// extradata=3: 64 floats (256 bytes).
    Extradata3,
    /// The WRC packet: 70 floats (280 bytes).
    Wrc,
    /// The F1 packet: 76 floats (304 bytes).
    Extended,
}

impl Layout {
    /// Number of float32 fields.
    pub const fn floats(self) -> usize {
        match self {
            Layout::Extradata1 => 38,
            Layout::Extradata3 => 64,
            Layout::Wrc => 70,
            Layout::Extended => 76,
        }
    }

    /// Exact byte length of the layout.
    pub const fn bytes(self) -> usize {
        self.floats() * 4
    }
}

/// A value per wheel, in the packet's naming.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Corners {
    pub rear_left: f64,
    pub rear_right: f64,
    pub front_left: f64,
    pub front_right: f64,
}

/// Car body axes and world velocity used by the orientation and heave derivations.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Frame {
    /// World velocity `xv, yv, zv` (m/s).
    pub velocity: [f64; 3],
    /// Car right/roll unit vector `rx, ry, rz`.
    pub right: [f64; 3],
    /// Car forward unit vector `fx, fy, fz`.
    pub forward: [f64; 3],
}

/// Fields from float 38 on (`Extradata3` and the longer layouts).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SessionFields {
    pub team_info: f64,
    pub max_rpm: f64,
}

/// Fields only the F1 packet carries.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ExtendedFields {
    pub era: f64,
    pub gforce_vert: f64,
    pub angular_velocity: [f64; 3],
}

/// The fields the listeners read, widened to f64.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MotionPacket {
    pub time: f64,
    pub speed: f64,
    pub frame: Frame,
    pub suspension_position: Corners,
    pub suspension_velocity: Corners,
    pub steer: f64,
    pub gear: f64,
    pub gforce_lat: f64,
    pub gforce_lon: f64,
    pub rpm: f64,
    /// Present for every layout but `Extradata1`.
    pub session: Option<SessionFields>,
    /// Present for the F1 packet only.
    pub extended: Option<ExtendedFields>,
}

impl MotionPacket {
    /// Decode `bytes` as `layout`; `None` unless the length is exactly the layout's size.
    pub fn decode(bytes: &[u8], layout: Layout) -> Option<MotionPacket> {
        if bytes.len() != layout.bytes() {
            return None;
        }
        let f = |index: usize| {
            let at = index * 4;
            f64::from(f32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]))
        };
        let corners = |first: usize| Corners {
            rear_left: f(first),
            rear_right: f(first + 1),
            front_left: f(first + 2),
            front_right: f(first + 3),
        };
        let session = (layout != Layout::Extradata1).then(|| SessionFields { team_info: f(59), max_rpm: f(63) });
        let extended = (layout == Layout::Extended).then(|| ExtendedFields {
            era: f(70),
            gforce_vert: f(72),
            angular_velocity: [f(73), f(74), f(75)],
        });
        Some(MotionPacket {
            time: f(0),
            speed: f(7),
            frame: Frame {
                velocity: [f(8), f(9), f(10)],
                right: [f(11), f(12), f(13)],
                forward: [f(14), f(15), f(16)],
            },
            suspension_position: corners(17),
            suspension_velocity: corners(21),
            steer: f(30),
            gear: f(33),
            gforce_lat: f(34),
            gforce_lon: f(35),
            rpm: f(37),
            session,
            extended,
        })
    }

    /// Decode a prefix of `bytes`: needs at least `layout.bytes()` bytes.
    pub fn decode_prefix(bytes: &[u8], layout: Layout) -> Option<MotionPacket> {
        MotionPacket::decode(bytes.get(..layout.bytes())?, layout)
    }
}

/// Test helper: encode float fields (index, value) into a packet of `layout`.
#[cfg(test)]
pub(crate) fn encode(layout: Layout, fields: &[(usize, f32)]) -> Vec<u8> {
    let mut floats = vec![0f32; layout.floats()];
    for &(index, value) in fields {
        floats[index] = value;
    }
    floats.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// Float indices of the packet fields, for building test packets.
#[cfg(test)]
pub(crate) mod field {
    pub const TIME: usize = 0;
    pub const SPEED: usize = 7;
    pub const XV: usize = 8;
    pub const YV: usize = 9;
    pub const ZV: usize = 10;
    pub const RX: usize = 11;
    pub const RY: usize = 12;
    pub const RZ: usize = 13;
    pub const FY: usize = 15;
    pub const FZ: usize = 16;
    pub const SUSP_POS_RL: usize = 17;
    pub const SUSP_POS_RR: usize = 18;
    pub const SUSP_POS_FL: usize = 19;
    pub const SUSP_POS_FR: usize = 20;
    pub const SUSP_VEL_RL: usize = 21;
    pub const SUSP_VEL_FL: usize = 23;
    pub const STEER: usize = 30;
    pub const GEAR: usize = 33;
    pub const GFORCE_LAT: usize = 34;
    pub const GFORCE_LON: usize = 35;
    pub const RPM: usize = 37;
    pub const TEAM_INFO: usize = 59;
    pub const MAX_RPM: usize = 63;
    pub const ERA: usize = 70;
    pub const GFORCE_VERT: usize = 72;
    pub const ANG_VEL_X: usize = 73;
    pub const ANG_VEL_Y: usize = 74;
    pub const ANG_VEL_Z: usize = 75;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_sizes_match_the_struct_formats() {
        // 38, 64, 76 and 70 floats.
        assert_eq!(Layout::Extradata1.bytes(), 152);
        assert_eq!(Layout::Extradata3.bytes(), 256);
        assert_eq!(Layout::Wrc.bytes(), 280);
        assert_eq!(Layout::Extended.bytes(), 304);
    }

    #[test]
    fn decodes_fields_in_the_definition_order() {
        // The field order of the F1 packet.
        let bytes = encode(
            Layout::Extended,
            &[
                (field::TIME, 1.5),
                (field::SPEED, 30.0),
                (field::RY, 0.25),
                (field::SUSP_POS_FR, 7.0),
                (field::SUSP_VEL_RL, -2.0),
                (field::RPM, 812.5),
                (field::TEAM_INFO, 3.0),
                (field::MAX_RPM, 1200.0),
                (field::ERA, 2017.0),
                (field::GFORCE_VERT, 0.5),
                (field::ANG_VEL_Z, -4.0),
            ],
        );
        let p = MotionPacket::decode(&bytes, Layout::Extended).unwrap();
        assert_eq!((p.time, p.speed, p.frame.right[1], p.rpm), (1.5, 30.0, 0.25, 812.5));
        assert_eq!(p.suspension_position.front_right, 7.0);
        assert_eq!(p.suspension_velocity.rear_left, -2.0);
        assert_eq!(p.session, Some(SessionFields { team_info: 3.0, max_rpm: 1200.0 }));
        assert_eq!(
            p.extended,
            Some(ExtendedFields { era: 2017.0, gforce_vert: 0.5, angular_velocity: [0.0, 0.0, -4.0] })
        );
    }

    #[test]
    fn struct_unpack_needs_the_exact_length() {
        // Extradata1 needs exactly 152 bytes; an Extradata3 prefix needs at least 256 bytes.
        let short = encode(Layout::Extradata1, &[]);
        assert!(MotionPacket::decode(&short[..151], Layout::Extradata1).is_none());
        let p = MotionPacket::decode(&short, Layout::Extradata1).unwrap();
        assert_eq!((p.session, p.extended), (None, None));
        let mut long = encode(Layout::Extradata3, &[]);
        long.extend_from_slice(&[0; 8]);
        assert!(MotionPacket::decode_prefix(&long, Layout::Extradata3).is_some());
        assert!(MotionPacket::decode_prefix(&long[..255], Layout::Extradata3).is_none());
    }
}
