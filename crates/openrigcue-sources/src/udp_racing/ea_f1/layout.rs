//! Byte layouts of the F1 2019–2025 packets.
//!
//! The packets are byte-packed structures. The offsets below are the field offsets of the game's UDP
//! specification, and `packet_sizes` is the size of every packet type (packet version 1).

use super::names;

/// Packet ids (`PacketID`).
pub(super) mod packet_id {
    pub const MOTION: u8 = 0;
    pub const SESSION: u8 = 1;
    pub const PARTICIPANTS: u8 = 4;
    pub const CAR_TELEMETRY: u8 = 6;
    pub const CAR_STATUS: u8 = 7;
    pub const MOTION_EX: u8 = 13;
}

/// `PacketHeader` field offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct HeaderLayout {
    pub size: usize,
    pub packet_version_at: usize,
    pub packet_id_at: usize,
    pub player_car_at: usize,
}

/// A per-car array inside a packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CarArray {
    pub at: usize,
    pub stride: usize,
}

/// Where the player-car wheel arrays and local velocity live (`PacketMotionData` until 2022, then
/// `PacketMotionEx`). Arrays are four float32 in the order RL, RR, FL, FR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct WheelDataLayout {
    pub packet_id: u8,
    pub suspension_position_at: usize,
    pub suspension_velocity_at: usize,
    pub wheel_slip_at: usize,
    /// `localVelocityX`, followed by Y and Z.
    pub local_velocity_at: usize,
}

/// Fields within one `CarMotionData` entry (all seasons).
pub(super) mod car_motion {
    pub const STRIDE: usize = 60;
    pub const G_FORCE_LATERAL: usize = 36;
    pub const G_FORCE_LONGITUDINAL: usize = 40;
    pub const G_FORCE_VERTICAL: usize = 44;
    pub const YAW: usize = 48;
    pub const PITCH: usize = 52;
    pub const ROLL: usize = 56;
}

/// Fields within one `CarTelemetryData` entry (all seasons).
pub(super) mod car_telemetry {
    pub const SPEED: usize = 0;
    pub const STEER: usize = 6;
    pub const GEAR: usize = 15;
    pub const ENGINE_RPM: usize = 16;
}

/// Fields within one `CarStatusData` entry (all seasons).
pub(super) mod car_status {
    pub const FUEL_IN_TANK: usize = 5;
    pub const FUEL_CAPACITY: usize = 9;
    pub const MAX_RPM: usize = 17;
}

/// Everything season specific: layout, name tables and the listener's per-season behaviour.
#[derive(Debug)]
pub(super) struct SeasonSpec {
    /// Listener id and `NETWORK_PORT` key.
    pub id: &'static str,
    pub default_port: u16,
    /// `header.packetFormat` (the season year).
    pub packet_format: u16,
    pub header: HeaderLayout,
    /// `(packetId, size)` of every accepted packet.
    pub packet_sizes: &'static [(u8, usize)],
    pub car_count: usize,
    pub motion_cars_at: usize,
    pub wheel_data: WheelDataLayout,
    pub telemetry: CarArray,
    pub surface_type_at: usize,
    pub status: CarArray,
    pub fia_flags_at: usize,
    pub participants: CarArray,
    pub team_id_at: usize,
    pub session_track_id_at: usize,
    pub teams: &'static [(u8, &'static str)],
    pub tracks: &'static [(i8, &'static str)],
    /// `speed * factor`: 0.6214 until 2022 (km/h to mph), 0.27778 from 2023 (km/h to m/s).
    pub speed_factor: f64,
    /// `pitch * -1` (from 2022).
    pub invert_pitch: bool,
    /// `localVelocityZ * -1` (from 2023).
    pub invert_longitudinal_velocity: bool,
    /// The receiver resets the packet store when a receive fails (from 2021).
    pub forget_packets_on_receive_failure: bool,
    /// The receiver is "connected" only while the player's team id is in the team table (from 2022).
    pub require_known_team: bool,
}

const HEADER_2019: HeaderLayout = HeaderLayout { size: 23, packet_version_at: 4, packet_id_at: 5, player_car_at: 22 };
const HEADER_2020: HeaderLayout = HeaderLayout { size: 24, packet_version_at: 4, packet_id_at: 5, player_car_at: 22 };
const HEADER_2023: HeaderLayout = HeaderLayout { size: 29, packet_version_at: 5, packet_id_at: 6, player_car_at: 27 };

const MOTION_WHEELS_2020: WheelDataLayout = WheelDataLayout {
    packet_id: packet_id::MOTION,
    suspension_position_at: 1344,
    suspension_velocity_at: 1360,
    wheel_slip_at: 1408,
    local_velocity_at: 1424,
};
const MOTION_EX_WHEELS: WheelDataLayout = WheelDataLayout {
    packet_id: packet_id::MOTION_EX,
    suspension_position_at: 29,
    suspension_velocity_at: 45,
    wheel_slip_at: 93,
    local_velocity_at: 161,
};

pub(super) const F1_2019: SeasonSpec = SeasonSpec {
    id: "f12019",
    default_port: 20780,
    packet_format: 2019,
    header: HEADER_2019,
    packet_sizes: &[(0, 1343), (1, 149), (2, 843), (3, 32), (4, 1104), (5, 843), (6, 1347), (7, 1143)],
    car_count: 20,
    motion_cars_at: 23,
    wheel_data: WheelDataLayout {
        packet_id: packet_id::MOTION,
        suspension_position_at: 1223,
        suspension_velocity_at: 1239,
        wheel_slip_at: 1287,
        local_velocity_at: 1303,
    },
    telemetry: CarArray { at: 23, stride: 66 },
    surface_type_at: 62,
    status: CarArray { at: 23, stride: 56 },
    fia_flags_at: 38,
    participants: CarArray { at: 24, stride: 54 },
    team_id_at: 2,
    session_track_id_at: 30,
    teams: names::TEAMS_2019,
    tracks: names::TRACKS_2019,
    speed_factor: 0.6214,
    invert_pitch: false,
    invert_longitudinal_velocity: false,
    forget_packets_on_receive_failure: false,
    require_known_team: false,
};

pub(super) const F1_2020: SeasonSpec = SeasonSpec {
    id: "f12020",
    default_port: 20782,
    packet_format: 2020,
    header: HEADER_2020,
    packet_sizes: &[
        (0, 1464),
        (1, 251),
        (2, 1190),
        (3, 35),
        (4, 1213),
        (5, 1102),
        (6, 1307),
        (7, 1344),
        (8, 839),
        (9, 1169),
    ],
    car_count: 22,
    motion_cars_at: 24,
    wheel_data: MOTION_WHEELS_2020,
    telemetry: CarArray { at: 24, stride: 58 },
    surface_type_at: 54,
    status: CarArray { at: 24, stride: 60 },
    fia_flags_at: 42,
    participants: CarArray { at: 25, stride: 54 },
    team_id_at: 2,
    session_track_id_at: 31,
    teams: names::TEAMS_2020,
    tracks: names::TRACKS_2020,
    speed_factor: 0.6214,
    invert_pitch: false,
    invert_longitudinal_velocity: false,
    forget_packets_on_receive_failure: false,
    require_known_team: false,
};

pub(super) const F1_2021: SeasonSpec = SeasonSpec {
    id: "f12021",
    default_port: 20783,
    packet_format: 2021,
    header: HEADER_2020,
    packet_sizes: &[
        (0, 1464),
        (1, 625),
        (2, 970),
        (3, 36),
        (4, 1257),
        (5, 1102),
        (6, 1347),
        (7, 1058),
        (8, 839),
        (9, 1191),
        (10, 618),
        (11, 1155),
    ],
    car_count: 22,
    motion_cars_at: 24,
    wheel_data: MOTION_WHEELS_2020,
    telemetry: CarArray { at: 24, stride: 60 },
    surface_type_at: 56,
    status: CarArray { at: 24, stride: 47 },
    fia_flags_at: 28,
    participants: CarArray { at: 25, stride: 56 },
    team_id_at: 3,
    session_track_id_at: 31,
    teams: names::TEAMS_2021,
    tracks: names::TRACKS_2021,
    speed_factor: 0.6214,
    invert_pitch: false,
    invert_longitudinal_velocity: false,
    forget_packets_on_receive_failure: true,
    require_known_team: false,
};

pub(super) const F1_2022: SeasonSpec = SeasonSpec {
    id: "f12022",
    default_port: 20784,
    packet_format: 2022,
    header: HEADER_2020,
    packet_sizes: &[
        (0, 1464),
        (1, 632),
        (2, 972),
        (3, 40),
        (4, 1257),
        (5, 1102),
        (6, 1347),
        (7, 1058),
        (8, 1015),
        (9, 1191),
        (10, 662),
        (11, 1155),
    ],
    car_count: 22,
    motion_cars_at: 24,
    wheel_data: MOTION_WHEELS_2020,
    telemetry: CarArray { at: 24, stride: 60 },
    surface_type_at: 56,
    status: CarArray { at: 24, stride: 47 },
    fia_flags_at: 28,
    participants: CarArray { at: 25, stride: 56 },
    team_id_at: 3,
    session_track_id_at: 31,
    teams: names::TEAMS_2022,
    tracks: names::TRACKS_2022,
    speed_factor: 0.6214,
    invert_pitch: true,
    invert_longitudinal_velocity: false,
    forget_packets_on_receive_failure: true,
    require_known_team: true,
};

pub(super) const F1_2023: SeasonSpec = SeasonSpec {
    id: "f12023",
    default_port: 20785,
    packet_format: 2023,
    header: HEADER_2023,
    packet_sizes: &[
        (0, 1349),
        (1, 644),
        (2, 1131),
        (3, 45),
        (4, 1306),
        (5, 1107),
        (6, 1352),
        (7, 1239),
        (8, 1020),
        (9, 1218),
        (10, 953),
        (11, 1460),
        (12, 231),
        (13, 217),
    ],
    car_count: 22,
    motion_cars_at: 29,
    wheel_data: MOTION_EX_WHEELS,
    telemetry: CarArray { at: 29, stride: 60 },
    surface_type_at: 56,
    status: CarArray { at: 29, stride: 55 },
    fia_flags_at: 28,
    participants: CarArray { at: 30, stride: 58 },
    team_id_at: 3,
    session_track_id_at: 36,
    teams: names::TEAMS_2023,
    tracks: names::TRACKS_2023,
    speed_factor: 0.27778,
    invert_pitch: true,
    invert_longitudinal_velocity: true,
    forget_packets_on_receive_failure: true,
    require_known_team: true,
};

pub(super) const F1_2024: SeasonSpec = SeasonSpec {
    id: "f12024",
    default_port: 20786,
    packet_format: 2024,
    header: HEADER_2023,
    packet_sizes: &[
        (0, 1349),
        (1, 753),
        (2, 1285),
        (3, 45),
        (4, 1350),
        (5, 1133),
        (6, 1352),
        (7, 1239),
        (8, 1020),
        (9, 1218),
        (10, 953),
        (11, 1460),
        (12, 231),
        (13, 237),
        (14, 101),
    ],
    car_count: 22,
    motion_cars_at: 29,
    wheel_data: MOTION_EX_WHEELS,
    telemetry: CarArray { at: 29, stride: 60 },
    surface_type_at: 56,
    status: CarArray { at: 29, stride: 55 },
    fia_flags_at: 28,
    participants: CarArray { at: 30, stride: 60 },
    team_id_at: 3,
    session_track_id_at: 36,
    teams: names::TEAMS_2024,
    tracks: names::TRACKS_2024,
    speed_factor: 0.27778,
    invert_pitch: true,
    invert_longitudinal_velocity: true,
    forget_packets_on_receive_failure: true,
    require_known_team: true,
};

pub(super) const F1_2025: SeasonSpec = SeasonSpec {
    id: "f12025",
    default_port: 20787,
    packet_format: 2025,
    header: HEADER_2023,
    packet_sizes: &[
        (0, 1349),
        (1, 753),
        (2, 1285),
        (3, 45),
        (4, 1284),
        (5, 1133),
        (6, 1352),
        (7, 1239),
        (8, 1042),
        (9, 866),
        (10, 1041),
        (11, 1460),
        (12, 231),
        (13, 273),
        (14, 101),
        (15, 1131),
    ],
    car_count: 22,
    motion_cars_at: 29,
    wheel_data: MOTION_EX_WHEELS,
    telemetry: CarArray { at: 29, stride: 60 },
    surface_type_at: 56,
    status: CarArray { at: 29, stride: 55 },
    fia_flags_at: 28,
    participants: CarArray { at: 30, stride: 57 },
    team_id_at: 3,
    session_track_id_at: 36,
    teams: names::TEAMS_2025,
    tracks: names::TRACKS_2025,
    speed_factor: 0.27778,
    invert_pitch: true,
    invert_longitudinal_velocity: true,
    forget_packets_on_receive_failure: true,
    require_known_team: true,
};

impl SeasonSpec {
    /// The acceptance test of a datagram: header present, `(packetFormat, packetVersion,
    /// packetId)` known, and the datagram exactly the size of that packet type. Returns the packet id.
    pub(super) fn accept(&self, datagram: &[u8]) -> Option<u8> {
        if datagram.len() < self.header.size {
            return None;
        }
        let format = u16::from_le_bytes([datagram[0], datagram[1]]);
        let version = datagram[self.header.packet_version_at];
        let id = datagram[self.header.packet_id_at];
        if format != self.packet_format || version != 1 {
            return None;
        }
        let &(_, size) = self.packet_sizes.iter().find(|(known, _)| *known == id)?;
        (datagram.len() == size).then_some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEASONS: [&SeasonSpec; 7] = [&F1_2019, &F1_2020, &F1_2021, &F1_2022, &F1_2023, &F1_2024, &F1_2025];

    #[test]
    fn speed_factors_are_the_listener_float_constants() {
        // f12019..f12022: 0x3fe3e28240b78034; f12023..f12025: 0x3fd1c725c3dee782.
        assert_eq!(F1_2019.speed_factor.to_bits(), 0x3fe3_e282_40b7_8034);
        assert_eq!(F1_2023.speed_factor.to_bits(), 0x3fd1_c725_c3de_e782);
    }

    #[test]
    fn layouts_fit_inside_their_packets() {
        for spec in SEASONS {
            let size = |id: u8| spec.packet_sizes.iter().find(|(k, _)| *k == id).unwrap().1;
            assert_eq!(
                size(packet_id::MOTION),
                spec.motion_cars_at
                    + spec.car_count * car_motion::STRIDE
                    + if spec.wheel_data.packet_id == packet_id::MOTION { 120 } else { 0 },
                "{}",
                spec.id
            );
            assert!(
                spec.telemetry.at + spec.car_count * spec.telemetry.stride <= size(packet_id::CAR_TELEMETRY),
                "{}",
                spec.id
            );
            assert_eq!(
                spec.status.at + spec.car_count * spec.status.stride,
                size(packet_id::CAR_STATUS),
                "{}",
                spec.id
            );
            assert!(
                spec.participants.at + spec.car_count * spec.participants.stride <= size(packet_id::PARTICIPANTS),
                "{}",
                spec.id
            );
            assert!(spec.wheel_data.local_velocity_at + 12 <= size(spec.wheel_data.packet_id), "{}", spec.id);
            assert!(spec.teams.windows(2).all(|w| w[0].0 < w[1].0) && spec.tracks.windows(2).all(|w| w[0].0 < w[1].0));
        }
    }

    #[test]
    fn unpack_accepts_only_known_exact_size_packets() {
        // Too short, unknown key fields, or a size mismatch are rejected.
        let mut motion = vec![0u8; 1349];
        motion[..2].copy_from_slice(&2023u16.to_le_bytes());
        motion[5] = 1;
        assert_eq!(F1_2023.accept(&motion), Some(packet_id::MOTION));
        assert_eq!(F1_2024.accept(&motion), None, "packetFormat must match");
        motion[5] = 2;
        assert_eq!(F1_2023.accept(&motion), None, "packetVersion must be 1");
        motion[5] = 1;
        assert_eq!(F1_2023.accept(&motion[..1348]), None);
        motion[6] = 14;
        assert_eq!(F1_2023.accept(&motion), None, "no time-trial packet in 2023");
        assert_eq!(F1_2023.accept(&motion[..20]), None);
    }
}
