//! The data definition the listeners register (definition 1) and the aircraft state decoded from its data
//! blocks, for the FSX, Prepar3D and MSFS listeners.

use super::wire::DataType;

/// Which simulator family a listener talks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Simulator {
    /// Listener `fsx`: Flight Simulator X.
    Fsx,
    /// Listener `p3d`: Prepar3D.
    Prepar3d,
    /// Listener `fs2k`: Microsoft Flight Simulator 2020 / 2024.
    Msfs,
}

/// One requested simulation variable and where its value goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Datum {
    Title,
    EngineRpm(usize),
    EngineMaxRpmPercent(usize),
    AirspeedIndicated,
    PitchDegrees,
    BankDegrees,
    HeadingDegreesTrue,
    RotationVelocityX,
    RotationVelocityY,
    RotationVelocityZ,
    AccelerationX,
    AccelerationY,
    AccelerationZ,
    WindX,
    WindY,
    WindZ,
    CanopyOpen,
    Afterburner1,
    GearCenterPosition,
    GearLeftPosition,
    GearRightPosition,
    GearAnimation(usize),
    GearAnimationAux,
    OnGround,
    CameraState,
    /// Compatibility placeholder for a caller-defined entry without a stored destination.
    Unread,
}

impl Datum {
    fn index(self) -> Option<u32> {
        Some(match self {
            Self::Title => 0,
            Self::EngineRpm(i) if i < 4 => 1 + i as u32,
            Self::EngineMaxRpmPercent(i) if i < 4 => 5 + i as u32,
            Self::AirspeedIndicated => 9,
            Self::PitchDegrees => 10,
            Self::BankDegrees => 11,
            Self::HeadingDegreesTrue => 12,
            Self::RotationVelocityX => 13,
            Self::RotationVelocityY => 14,
            Self::RotationVelocityZ => 15,
            Self::AccelerationX => 16,
            Self::AccelerationY => 17,
            Self::AccelerationZ => 18,
            Self::WindX => 19,
            Self::WindY => 20,
            Self::WindZ => 21,
            Self::CanopyOpen => 22,
            Self::Afterburner1 => 23,
            Self::GearCenterPosition => 24,
            Self::GearLeftPosition => 25,
            Self::GearRightPosition => 26,
            Self::GearAnimation(i) if i < 3 => 27 + i as u32,
            Self::GearAnimationAux => 30,
            Self::OnGround => 31,
            Self::CameraState => 32,
            _ => return None,
        })
    }
}

/// One entry added to the data definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataDefinition {
    pub name: &'static str,
    pub units: &'static str,
    pub data_type: DataType,
    pub datum: Datum,
}

const fn float(name: &'static str, units: &'static str, datum: Datum) -> DataDefinition {
    DataDefinition { name, units, data_type: DataType::Float32, datum }
}

/// The entries after the title, shared by the three listeners in this order.
const COMMON: [DataDefinition; 31] = [
    float("GENERAL ENG RPM:1", "Rpm", Datum::EngineRpm(0)),
    float("GENERAL ENG RPM:2", "Rpm", Datum::EngineRpm(1)),
    float("GENERAL ENG RPM:3", "Rpm", Datum::EngineRpm(2)),
    float("GENERAL ENG RPM:4", "Rpm", Datum::EngineRpm(3)),
    float("GENERAL ENG PCT MAX RPM:1", "Rpm", Datum::EngineMaxRpmPercent(0)),
    float("GENERAL ENG PCT MAX RPM:2", "Rpm", Datum::EngineMaxRpmPercent(1)),
    float("GENERAL ENG PCT MAX RPM:3", "Rpm", Datum::EngineMaxRpmPercent(2)),
    float("GENERAL ENG PCT MAX RPM:4", "Rpm", Datum::EngineMaxRpmPercent(3)),
    float("Airspeed Indicated", "knots", Datum::AirspeedIndicated),
    float("PLANE PITCH DEGREES", "Degrees", Datum::PitchDegrees),
    float("PLANE BANK DEGREES", "Degrees", Datum::BankDegrees),
    float("PLANE HEADING DEGREES TRUE", "Degrees", Datum::HeadingDegreesTrue),
    float("ROTATION VELOCITY BODY X", "Degrees per second", Datum::RotationVelocityX),
    float("ROTATION VELOCITY BODY Y", "Degrees per second", Datum::RotationVelocityY),
    float("ROTATION VELOCITY BODY Z", "Degrees per second", Datum::RotationVelocityZ),
    float("ACCELERATION BODY X", "Feet per second squared", Datum::AccelerationX),
    float("ACCELERATION BODY Y", "Feet per second squared", Datum::AccelerationY),
    float("ACCELERATION BODY Z", "Feet per second squared", Datum::AccelerationZ),
    float("AIRCRAFT WIND X", "knots", Datum::WindX),
    float("AIRCRAFT WIND Y", "knots", Datum::WindY),
    float("AIRCRAFT WIND Z", "knots", Datum::WindZ),
    float("CANOPY OPEN", "Percent", Datum::CanopyOpen),
    float("TURB ENG AFTERBURNER:1", "Bool", Datum::Afterburner1),
    float("Gear Center Position", "Percent", Datum::GearCenterPosition),
    float("Gear Left Position", "Percent", Datum::GearLeftPosition),
    float("Gear Right Position", "Percent", Datum::GearRightPosition),
    float("GEAR ANIMATION POSITION:0", "Percent", Datum::GearAnimation(0)),
    float("GEAR ANIMATION POSITION:1", "Percent", Datum::GearAnimation(1)),
    float("GEAR ANIMATION POSITION:2", "Percent", Datum::GearAnimation(2)),
    float("GEAR ANIMATION POSITION:3", "Percent", Datum::GearAnimationAux),
    float("SIM ON GROUND", "bool", Datum::OnGround),
];

impl Simulator {
    /// Definition 1 of the listener, in registration order (= data block order).
    pub fn definitions(self) -> Vec<DataDefinition> {
        let title = match self {
            Simulator::Prepar3d => {
                DataDefinition { name: "title", units: "", data_type: DataType::String32, datum: Datum::Title }
            }
            _ => DataDefinition { name: "TITLE", units: "", data_type: DataType::String256, datum: Datum::Title },
        };
        let mut definitions = vec![title];
        definitions.extend_from_slice(&COMMON);
        if self == Simulator::Msfs {
            definitions.push(float("CAMERA STATE", "ENUM", Datum::CameraState));
        }
        definitions
    }
}

/// The aircraft state once a data block has been decoded.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AircraftState {
    /// The raw title field, NUL padding included.
    pub title: Vec<u8>,
    pub engine_rpm: [f64; 4],
    pub engine_max_rpm_percent: [f64; 4],
    /// Indicated airspeed, knots.
    pub airspeed: f64,
    pub pitch: f64,
    pub roll: f64,
    pub yaw: f64,
    /// Body rotation velocities: X is pitch, Y yaw, Z roll (deg/s).
    pub pitch_rate: f64,
    pub yaw_rate: f64,
    pub roll_rate: f64,
    /// Body accelerations X (lateral), Y (vertical), Z (longitudinal), ft/s².
    pub lateral_acceleration: f64,
    pub vertical_acceleration: f64,
    pub longitudinal_acceleration: f64,
    /// Aircraft wind X, Y, Z (lateral, vertical, longitudinal), knots.
    pub wind: [f64; 3],
    pub canopy: f64,
    /// The requested engine-1 value, distinct from the unrequested `afterburner_0` getter alias.
    pub afterburner_1: f64,
    /// Gear position percent: center, left, right.
    pub landing_gear: [f64; 3],
    /// Gear animation percent: center, left, right.
    pub gear_animation: [f64; 3],
    pub gear_animation_aux: f64,
    pub on_ground: f64,
    pub camera_state: f64,
    /// None for a fully populated public value; a bit mask while the receiver publishes its first block.
    pub(crate) present: Option<u64>,
}

/// A data block shorter than its definition: entry `entry` (a float, a 32-byte or a 256-byte string) cannot be
/// decoded from the bytes that are left. The text describes the size mismatch.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unpack requires a buffer of {size} bytes")]
pub struct ShortBlock {
    pub entry: usize,
    pub size: usize,
}

impl AircraftState {
    /// Decodes a data block: every entry from the front of the block (all or nothing).
    pub fn decode(definitions: &[DataDefinition], block: &[u8]) -> Result<Self, ShortBlock> {
        let mut state = AircraftState::default();
        let mut offset = 0;
        for (index, definition) in definitions.iter().enumerate() {
            let size = definition.data_type.size();
            let field = block.get(offset..offset + size).ok_or(ShortBlock { entry: index, size })?;
            offset += size;
            if definition.data_type == DataType::Float32 {
                let value = f64::from(f32::from_le_bytes([field[0], field[1], field[2], field[3]]));
                state.store(definition.datum, value);
            } else {
                state.title = field.to_vec();
            }
        }
        Ok(state)
    }

    pub(crate) fn empty() -> Self {
        Self { present: Some(0), ..Self::default() }
    }

    pub(crate) fn publish(&mut self, datum: Datum, decoded: &Self) {
        if datum == Datum::Title {
            self.title = decoded.title.clone();
        } else if let Some(value) = decoded.number(datum) {
            self.store(datum, value);
        }
        if let (Some(mask), Some(index)) = (&mut self.present, datum.index()) {
            *mask |= 1 << index;
        }
    }

    pub(crate) fn has(&self, datum: Datum) -> bool {
        self.present.is_none_or(|mask| datum.index().is_some_and(|i| mask & (1 << i) != 0))
    }

    pub(crate) fn number(&self, datum: Datum) -> Option<f64> {
        if !self.has(datum) {
            return None;
        }
        Some(match datum {
            Datum::Title | Datum::Unread => return None,
            Datum::EngineRpm(i) => *self.engine_rpm.get(i)?,
            Datum::EngineMaxRpmPercent(i) => *self.engine_max_rpm_percent.get(i)?,
            Datum::AirspeedIndicated => self.airspeed,
            Datum::PitchDegrees => self.pitch,
            Datum::BankDegrees => self.roll,
            Datum::HeadingDegreesTrue => self.yaw,
            Datum::RotationVelocityX => self.pitch_rate,
            Datum::RotationVelocityY => self.yaw_rate,
            Datum::RotationVelocityZ => self.roll_rate,
            Datum::AccelerationX => self.lateral_acceleration,
            Datum::AccelerationY => self.vertical_acceleration,
            Datum::AccelerationZ => self.longitudinal_acceleration,
            Datum::WindX => self.wind[0],
            Datum::WindY => self.wind[1],
            Datum::WindZ => self.wind[2],
            Datum::CanopyOpen => self.canopy,
            Datum::Afterburner1 => self.afterburner_1,
            Datum::GearCenterPosition => self.landing_gear[0],
            Datum::GearLeftPosition => self.landing_gear[1],
            Datum::GearRightPosition => self.landing_gear[2],
            Datum::GearAnimation(i) => *self.gear_animation.get(i)?,
            Datum::GearAnimationAux => self.gear_animation_aux,
            Datum::OnGround => self.on_ground,
            Datum::CameraState => self.camera_state,
        })
    }

    fn store(&mut self, datum: Datum, value: f64) {
        let slot = match datum {
            Datum::Title | Datum::Unread => return,
            Datum::EngineRpm(i) => &mut self.engine_rpm[i],
            Datum::EngineMaxRpmPercent(i) => &mut self.engine_max_rpm_percent[i],
            Datum::AirspeedIndicated => &mut self.airspeed,
            Datum::PitchDegrees => &mut self.pitch,
            Datum::BankDegrees => &mut self.roll,
            Datum::HeadingDegreesTrue => &mut self.yaw,
            Datum::RotationVelocityX => &mut self.pitch_rate,
            Datum::RotationVelocityY => &mut self.yaw_rate,
            Datum::RotationVelocityZ => &mut self.roll_rate,
            Datum::AccelerationX => &mut self.lateral_acceleration,
            Datum::AccelerationY => &mut self.vertical_acceleration,
            Datum::AccelerationZ => &mut self.longitudinal_acceleration,
            Datum::WindX => &mut self.wind[0],
            Datum::WindY => &mut self.wind[1],
            Datum::WindZ => &mut self.wind[2],
            Datum::CanopyOpen => &mut self.canopy,
            Datum::Afterburner1 => &mut self.afterburner_1,
            Datum::GearCenterPosition => &mut self.landing_gear[0],
            Datum::GearLeftPosition => &mut self.landing_gear[1],
            Datum::GearRightPosition => &mut self.landing_gear[2],
            Datum::GearAnimation(i) => &mut self.gear_animation[i],
            Datum::GearAnimationAux => &mut self.gear_animation_aux,
            Datum::OnGround => &mut self.on_ground,
            Datum::CameraState => &mut self.camera_state,
        };
        *slot = value;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The definition has 32 entries (33 for MSFS with CAMERA STATE), title STRING256 except P3D's 32-byte `title`.
    #[test]
    fn definition_lists() {
        let fsx = Simulator::Fsx.definitions();
        assert_eq!(fsx.len(), 32);
        assert_eq!((fsx[0].name, fsx[0].data_type), ("TITLE", DataType::String256));
        assert_eq!(fsx[31].name, "SIM ON GROUND");
        let p3d = Simulator::Prepar3d.definitions();
        assert_eq!((p3d[0].name, p3d[0].data_type), ("title", DataType::String32));
        let msfs = Simulator::Msfs.definitions();
        assert_eq!(msfs.len(), 33);
        assert_eq!((msfs[32].name, msfs[32].units), ("CAMERA STATE", "ENUM"));
    }

    /// Decoding consumes the block entry by entry; a short block fails as a whole.
    #[test]
    fn decode_block() {
        let defs = Simulator::Prepar3d.definitions();
        let mut block = b"Cessna".to_vec();
        block.resize(32, 0);
        for i in 0..31 {
            block.extend_from_slice(&(i as f32).to_le_bytes());
        }
        let state = AircraftState::decode(&defs, &block).unwrap();
        assert_eq!(&state.title[..6], b"Cessna");
        assert_eq!(state.engine_rpm, [0.0, 1.0, 2.0, 3.0]);
        assert_eq!(state.airspeed, 8.0);
        assert_eq!((state.pitch_rate, state.yaw_rate, state.roll_rate), (12.0, 13.0, 14.0));
        assert_eq!(state.wind, [18.0, 19.0, 20.0]);
        // These raw fields remain available even though the dispatcher does not request them.
        assert_eq!((state.afterburner_1, state.gear_animation_aux), (22.0, 29.0));
        assert_eq!((state.canopy, state.landing_gear), (21.0, [23.0, 24.0, 25.0]));
        assert_eq!(state.gear_animation, [26.0, 27.0, 28.0]);
        assert_eq!(state.on_ground, 30.0);
        let short = AircraftState::decode(&defs, &block[..block.len() - 1]).unwrap_err();
        assert_eq!(short, ShortBlock { entry: 31, size: 4 });
        assert_eq!(short.to_string(), "unpack requires a buffer of 4 bytes");
        assert_eq!(
            AircraftState::decode(&defs, b"Cessna").unwrap_err().to_string(),
            "unpack requires a buffer of 32 bytes"
        );
    }
}
