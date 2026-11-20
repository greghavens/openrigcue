//! Speaker profiles: the per-group speaker matrices.

use super::layout::{Group, PerGroup, PerSpeaker, Speaker};

/// A speaker layout: for every channel group, the level on each speaker (0..~20, profile units).
#[derive(Debug, Clone, PartialEq)]
pub struct SpeakerProfile {
    pub name: String,
    /// Speakers shown as enabled on the hardware page. The mix formula ignores this.
    pub enabled: [bool; 6],
    /// Rows indexed by [`Group::index`], columns by [`Speaker::index`].
    pub levels: PerGroup<PerSpeaker>,
}

impl SpeakerProfile {
    /// Level of `group` on `speaker`, in profile units.
    pub fn level(&self, group: Group, speaker: Speaker) -> f64 {
        self.levels[group.index()][speaker.index()]
    }

    /// Apply a custom-matrix edit of one speaker: `column` (indexed by [`Group::index`]) becomes
    /// that speaker's level for every group, then the speaker's `accel` and `brake` rows (used
    /// while |lateral G| <= 3) are derived as the larger of their corner rows (`accel_lr`/`accel_rr`,
    /// `brake_lf`/`brake_rf`), whatever the edit gave them.
    /// The rebuild that follows it is
    /// [`super::system::ShakerSystem::reload_levels`].
    pub fn edit_speaker_column(&mut self, speaker: Speaker, column: &PerGroup<f64>) {
        let s = speaker.index();
        for (row, &value) in self.levels.iter_mut().zip(column) {
            row[s] = value;
        }
        let larger = |a: Group, b: Group| python_max(self.levels[a.index()][s], self.levels[b.index()][s]);
        let accel = larger(Group::AccelLr, Group::AccelRr);
        let brake = larger(Group::BrakeLf, Group::BrakeRf);
        self.levels[Group::Accel.index()][s] = accel;
        self.levels[Group::Brake.index()][s] = brake;
    }

    /// The empty profile used before a layout is loaded: no
    /// name, no speaker enabled, every level 0.
    pub fn empty() -> SpeakerProfile {
        SpeakerProfile { name: String::new(), enabled: [false; 6], levels: [[0.0; 6]; super::layout::GROUP_COUNT] }
    }

    /// The built-in profile named `name`, as a fresh install holds it.
    pub fn built_in(name: &str) -> Option<SpeakerProfile> {
        BUILT_IN.iter().find(|p| p.name == name).map(BuiltInProfile::to_profile)
    }

    /// Every built-in profile name, in catalogue order.
    pub fn built_in_names() -> impl Iterator<Item = &'static str> {
        BUILT_IN.iter().map(|p| p.name)
    }
}

/// The larger of two values: `b` only when `b > a`, so `a` on ties (and when either is NaN).
fn python_max(a: f64, b: f64) -> f64 {
    if b > a {
        b
    } else {
        a
    }
}

/// Name of the profile selected by the template `config.ini` (`SHAKEBOX.profile`).
pub const DEFAULT_PROFILE: &str = "1 Speaker - Center";

struct BuiltInProfile {
    name: &'static str,
    enabled: [bool; 6],
    levels: PerGroup<PerSpeaker>,
}

impl BuiltInProfile {
    fn to_profile(&self) -> SpeakerProfile {
        SpeakerProfile { name: self.name.to_string(), enabled: self.enabled, levels: self.levels }
    }
}

include!("profiles_data.rs");

/// Output channels a profile needs from the sound card; with fewer, the
/// "Shakers - ... Required" notification is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurroundRequirement {
    /// Fewer device channels than this triggers the warning.
    pub min_channels: u16,
    /// The channel-configuration label named in the message.
    pub label: &'static str,
    /// Whether the message also suggests switching to a 2-channel profile.
    pub suggest_stereo: bool,
}

/// Profile name -> channel requirement.
pub fn surround_requirement(profile: &str) -> Option<SurroundRequirement> {
    const QUAD: SurroundRequirement =
        SurroundRequirement { min_channels: 4, label: "4-channel (Quadraphonic)", suggest_stereo: false };
    const SIX: SurroundRequirement =
        SurroundRequirement { min_channels: 6, label: "6-channel (5.1 Surround)", suggest_stereo: true };
    const CENTER_SUB: SurroundRequirement =
        SurroundRequirement { min_channels: 4, label: "5.1 Surround", suggest_stereo: false };
    match profile {
        "4 Speakers - FL,FR,RL,RR" => Some(QUAD),
        "2 Speakers - Center / Sub" => Some(CENTER_SUB),
        "6 Speakers - 5.1"
        | "SRS Shakebox Elite + Sub + Center"
        | "SRS ShakeKit or SRS Shakebox Elite"
        | "SRS UShake6"
        | "SRS UShake6 Enhanced" => Some(SIX),
        _ => None,
    }
}

/// The requirement `profile` misses on a device with `channels` outputs, if any
/// (a warning is due when `channels < min_channels`).
pub fn surround_shortfall(profile: &str, channels: u16) -> Option<SurroundRequirement> {
    surround_requirement(profile).filter(|r| channels < r.min_channels)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_json_values_survive() {
        // '6 Speakers - 5.1' road_bump = 8/8/12/12/8/8.
        let p = SpeakerProfile::built_in("6 Speakers - 5.1").unwrap();
        assert_eq!(p.levels[Group::RoadBump.index()], [8.0, 8.0, 12.0, 12.0, 8.0, 8.0]);
        // '2 Speakers - Center / Sub' engine really has RR = 10 although RR is disabled.
        let cs = SpeakerProfile::built_in("2 Speakers - Center / Sub").unwrap();
        assert_eq!(cs.level(Group::Engine, Speaker::RearRight), 10.0);
        assert!(!cs.enabled[Speaker::RearRight.index()]);
    }

    #[test]
    fn migrations_copy_groups() {
        // The rows added by migrations v158/v206/v234/v321/v324.
        let p = SpeakerProfile::built_in("4 Speakers - FL,FR,RL,RR").unwrap();
        assert_eq!(p.levels[Group::WheelSlipLr.index()], p.levels[Group::SuspensionLr.index()]);
        assert_eq!(p.levels[Group::MissileFiring.index()], p.levels[Group::CannonFiring.index()]);
        assert_eq!(p.levels[Group::AbsLock.index()], p.levels[Group::Brake.index()]);
        assert_eq!(p.levels[Group::TouchdownNose.index()], p.levels[Group::LandingGearNose.index()]);
        assert_eq!(p.levels[Group::Turbulence.index()], p.levels[Group::Impact.index()]);
        // v205: '4 Speakers' landing_gear_nose = 0/5/0/0/5/0.
        assert_eq!(p.levels[Group::LandingGearNose.index()], [0.0, 5.0, 0.0, 0.0, 5.0, 0.0]);
    }

    #[test]
    fn ushake6_first_install_engine_levels() {
        // v264 copy of '6 Speakers - 5.1', then v342 (first installation) CE/SUB engine = 4.
        let u = SpeakerProfile::built_in("SRS UShake6").unwrap();
        assert_eq!(u.level(Group::Engine, Speaker::Center), 4.0);
        assert_eq!(u.level(Group::Engine, Speaker::Sub), 4.0);
        let six = SpeakerProfile::built_in("6 Speakers - 5.1").unwrap();
        assert_eq!(u.levels[Group::Gear.index()], six.levels[Group::Gear.index()]);
        let e = SpeakerProfile::built_in("SRS UShake6 Enhanced").unwrap();
        assert_eq!(e.level(Group::Engine, Speaker::FrontLeft), 6.0);
        assert_eq!(SpeakerProfile::built_in_names().count(), 9);
    }

    #[test]
    fn template_profile_is_built_in() {
        assert!(SpeakerProfile::built_in(DEFAULT_PROFILE).is_some());
    }

    #[test]
    fn on_confirm_adv_shakers_derives_accel_and_brake() {
        let mut p = SpeakerProfile::built_in("6 Speakers - 5.1").unwrap();
        let before = p.clone();
        let fl = Speaker::FrontLeft;
        let mut column: PerGroup<f64> = std::array::from_fn(|g| before.levels[g][fl.index()]);
        column[Group::AccelLr.index()] = 3.0;
        column[Group::AccelRr.index()] = 14.0;
        column[Group::BrakeLf.index()] = 9.0;
        column[Group::BrakeRf.index()] = 2.0;
        // Values typed for the derived rows themselves are overwritten.
        column[Group::Accel.index()] = 20.0;
        column[Group::Brake.index()] = 0.0;
        column[Group::Engine.index()] = 7.0;
        p.edit_speaker_column(fl, &column);
        assert_eq!(p.level(Group::Accel, fl), 14.0);
        assert_eq!(p.level(Group::Brake, fl), 9.0);
        assert_eq!(p.level(Group::Engine, fl), 7.0);
        assert_eq!(p.level(Group::AccelLr, fl), 3.0);
        // Other speakers are untouched.
        for s in [Speaker::FrontRight, Speaker::Center, Speaker::Sub, Speaker::RearLeft, Speaker::RearRight] {
            for g in Group::ALL {
                assert_eq!(p.level(g, s), before.level(g, s));
            }
        }
    }

    #[test]
    fn check_device_changed_requirements() {
        assert_eq!(surround_shortfall("SRS UShake6", 2).unwrap().min_channels, 6);
        assert!(surround_shortfall("SRS UShake6", 6).is_none());
        assert_eq!(surround_shortfall("2 Speakers - Center / Sub", 2).unwrap().label, "5.1 Surround");
        assert!(surround_shortfall("1 Speaker - Center", 2).is_none());
        assert!(surround_shortfall("6 Speakers - 5.1", 4).unwrap().suggest_stereo);
    }
}
