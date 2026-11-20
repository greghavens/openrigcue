//! Speakers, channel groups and the effect each group belongs to.

/// One of the six 5.1 outputs, in the order of the output buffer and of the
/// mix-level lists (front left, front right, center, LFE, surround left, surround right).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Speaker {
    FrontLeft,
    FrontRight,
    Center,
    Sub,
    RearLeft,
    RearRight,
}

impl Speaker {
    /// The speakers in output order (FL, FR, CE, SUB, RL, RR).
    pub const ALL: [Speaker; 6] =
        [Speaker::FrontLeft, Speaker::FrontRight, Speaker::Center, Speaker::Sub, Speaker::RearLeft, Speaker::RearRight];

    /// Position in [`Speaker::ALL`] and in every per-speaker array of this module.
    pub fn index(self) -> usize {
        self as usize
    }
}

/// One channel group; every sample plays through exactly one group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Group {
    Engine,
    Gear,
    Afterburner,
    CannonFiring,
    MissileFiring,
    LandingGearLeft,
    LandingGearNose,
    LandingGearRight,
    TouchdownLeft,
    TouchdownNose,
    TouchdownRight,
    Turbulence,
    Canopy,
    SuspensionLf,
    SuspensionRf,
    SuspensionLr,
    SuspensionRr,
    WheelSlipLf,
    WheelSlipRf,
    WheelSlipLr,
    WheelSlipRr,
    RoadBump,
    Brake,
    AbsLock,
    Accel,
    Impact,
    BrakeLf,
    BrakeRf,
    AccelLr,
    AccelRr,
    LatLeft,
    LatRight,
    TractionLossLeft,
    TractionLossRight,
}

/// Number of channel groups.
pub const GROUP_COUNT: usize = 34;

/// Per-group values, indexed by [`Group::index`].
pub type PerGroup<T> = [T; GROUP_COUNT];

impl Group {
    /// The groups in table order, which is the order they are built in.
    pub const ALL: [Group; GROUP_COUNT] = [
        Group::Engine,
        Group::Gear,
        Group::Afterburner,
        Group::CannonFiring,
        Group::MissileFiring,
        Group::LandingGearLeft,
        Group::LandingGearNose,
        Group::LandingGearRight,
        Group::TouchdownLeft,
        Group::TouchdownNose,
        Group::TouchdownRight,
        Group::Turbulence,
        Group::Canopy,
        Group::SuspensionLf,
        Group::SuspensionRf,
        Group::SuspensionLr,
        Group::SuspensionRr,
        Group::WheelSlipLf,
        Group::WheelSlipRf,
        Group::WheelSlipLr,
        Group::WheelSlipRr,
        Group::RoadBump,
        Group::Brake,
        Group::AbsLock,
        Group::Accel,
        Group::Impact,
        Group::BrakeLf,
        Group::BrakeRf,
        Group::AccelLr,
        Group::AccelRr,
        Group::LatLeft,
        Group::LatRight,
        Group::TractionLossLeft,
        Group::TractionLossRight,
    ];

    /// Position in [`Group::ALL`].
    pub fn index(self) -> usize {
        self as usize
    }

    /// The tuning effect (a `shaker_effects` key of the tune) that sets this group's level.
    pub fn effect(self) -> Effect {
        use Group::*;
        match self {
            Engine => Effect::Engine,
            Gear => Effect::Gear,
            Afterburner => Effect::Afterburner,
            CannonFiring => Effect::CannonFiring,
            MissileFiring => Effect::MissileFiring,
            LandingGearLeft | LandingGearNose | LandingGearRight => Effect::LandingGear,
            TouchdownLeft | TouchdownNose | TouchdownRight => Effect::Touchdown,
            Turbulence => Effect::Turbulence,
            Canopy => Effect::Canopy,
            SuspensionLf | SuspensionRf | SuspensionLr | SuspensionRr => Effect::Suspension,
            WheelSlipLf | WheelSlipRf | WheelSlipLr | WheelSlipRr => Effect::WheelSlip,
            RoadBump => Effect::RoadBump,
            Brake | BrakeLf | BrakeRf => Effect::Deceleration,
            AbsLock => Effect::AbsLock,
            Accel | AccelLr | AccelRr => Effect::Acceleration,
            Impact => Effect::Impact,
            LatLeft | LatRight => Effect::LateralAcceleration,
            TractionLossLeft | TractionLossRight => Effect::TractionLoss,
        }
    }

    /// The group's name (profile `sfx` key).
    pub fn key(self) -> &'static str {
        use Group::*;
        match self {
            Engine => "engine",
            Gear => "gear",
            Afterburner => "afterburner",
            CannonFiring => "cannon_firing",
            MissileFiring => "missile_firing",
            LandingGearLeft => "landing_gear_left",
            LandingGearNose => "landing_gear_nose",
            LandingGearRight => "landing_gear_right",
            TouchdownLeft => "touchdown_left",
            TouchdownNose => "touchdown_nose",
            TouchdownRight => "touchdown_right",
            Turbulence => "turbulence",
            Canopy => "canopy",
            SuspensionLf => "suspension_lf",
            SuspensionRf => "suspension_rf",
            SuspensionLr => "suspension_lr",
            SuspensionRr => "suspension_rr",
            WheelSlipLf => "wheel_slip_lf",
            WheelSlipRf => "wheel_slip_rf",
            WheelSlipLr => "wheel_slip_lr",
            WheelSlipRr => "wheel_slip_rr",
            RoadBump => "road_bump",
            Brake => "brake",
            AbsLock => "abs_lock",
            Accel => "accel",
            Impact => "impact",
            BrakeLf => "brake_lf",
            BrakeRf => "brake_rf",
            AccelLr => "accel_lr",
            AccelRr => "accel_rr",
            LatLeft => "lat_l",
            LatRight => "lat_r",
            TractionLossLeft => "traction_loss_l",
            TractionLossRight => "traction_loss_r",
        }
    }
}

/// A user-tunable shaker effect (a `shaker_effects` key of the tune).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Effect {
    Engine,
    Gear,
    Suspension,
    RoadBump,
    Impact,
    Acceleration,
    Deceleration,
    LateralAcceleration,
    TractionLoss,
    WheelSlip,
    Afterburner,
    CannonFiring,
    Canopy,
    LandingGear,
    MissileFiring,
    AbsLock,
    Touchdown,
    Turbulence,
}

impl Effect {
    /// Every effect, in the order of the tune's `shaker_effects` entries.
    pub const ALL: [Effect; 18] = [
        Effect::Engine,
        Effect::Gear,
        Effect::Suspension,
        Effect::RoadBump,
        Effect::Impact,
        Effect::Acceleration,
        Effect::Deceleration,
        Effect::LateralAcceleration,
        Effect::TractionLoss,
        Effect::WheelSlip,
        Effect::Afterburner,
        Effect::CannonFiring,
        Effect::Canopy,
        Effect::LandingGear,
        Effect::MissileFiring,
        Effect::AbsLock,
        Effect::Touchdown,
        Effect::Turbulence,
    ];

    /// Position in [`Effect::ALL`].
    pub fn index(self) -> usize {
        self as usize
    }

    /// The effect's tune key.
    pub fn key(self) -> &'static str {
        use Effect::*;
        match self {
            Engine => "engine",
            Gear => "gear",
            Suspension => "suspension",
            RoadBump => "road_bump",
            Impact => "impact",
            Acceleration => "acceleration",
            Deceleration => "deceleration",
            LateralAcceleration => "lateral_acceleration",
            TractionLoss => "traction_loss",
            WheelSlip => "wheel_slip",
            Afterburner => "afterburner",
            CannonFiring => "cannon_firing",
            Canopy => "canopy",
            LandingGear => "landing_gear",
            MissileFiring => "missile_firing",
            AbsLock => "abs_lock",
            Touchdown => "touchdown",
            Turbulence => "turbulence",
        }
    }

    /// Effect by its tune key.
    pub fn from_key(key: &str) -> Option<Effect> {
        Effect::ALL.into_iter().find(|e| e.key() == key)
    }
}

/// Per-speaker values, indexed by [`Speaker::index`].
pub type PerSpeaker = [f64; 6];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_order_matches_global_variables_effects() {
        let keys: Vec<&str> = Group::ALL.iter().map(|g| g.key()).collect();
        assert_eq!(keys[0], "engine");
        assert_eq!(keys[21], "road_bump");
        assert_eq!(keys[33], "traction_loss_r");
        for (i, g) in Group::ALL.iter().enumerate() {
            assert_eq!(g.index(), i);
        }
    }

    #[test]
    fn mapping_matches_shaker_helpers_mapping() {
        assert_eq!(Group::BrakeRf.effect(), Effect::Deceleration);
        assert_eq!(Group::AccelLr.effect(), Effect::Acceleration);
        assert_eq!(Group::LatLeft.effect(), Effect::LateralAcceleration);
        assert_eq!(Group::TouchdownNose.effect(), Effect::Touchdown);
        assert_eq!(Effect::from_key("wheel_slip"), Some(Effect::WheelSlip));
    }
}
