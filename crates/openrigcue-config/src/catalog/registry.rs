//! Application registries the configuration schema is built from: shaker, motion, rumble and
//! haptic effect lists, the iFlag table, motion models and manufacturers, installer languages and
//! the built-in shaker layout names. Data: `data/app_tables.toml`.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::Deserialize;

/// Effect category used to filter sliders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum EffectCategory {
    Common,
    Racing,
    Flight,
}

/// A shaker (bass-shaker) effect (`shaker_effect` table).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ShakerEffectInfo {
    pub id: String,
    pub label: String,
    /// The slider index; `cannon_firing` and `missile_firing` share 11.
    pub index: u32,
    pub category: EffectCategory,
    /// Triggered by a shortcut rather than by telemetry.
    pub shortcut: bool,
}

/// A per-speaker effect entry of a shaker layout (`shaker_channel_effect` table).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ChannelEffectInfo {
    pub id: String,
    pub name: String,
    pub parent: String,
    pub visible: bool,
}

/// A motion effect (`motion_effect` table).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct MotionEffectInfo {
    pub id: String,
    pub label: String,
    /// Whether the effect has a "max telemetry" range slider.
    pub allow_max: bool,
    pub category: EffectCategory,
    pub shortcut: bool,
    /// Whether IntelliTune calibrates it.
    pub intellitune: bool,
}

/// A SIM3D rumble or HapticR effect (`rumble_effect` and `haptic_effect` tables).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GroupedEffectInfo {
    pub id: String,
    pub group: String,
    pub label: String,
}

/// An iFlag code (`flag` table).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct FlagInfo {
    pub code: u32,
    pub group: String,
    pub name: String,
    pub description: String,
}

/// A motion platform model and its manufacturer.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct MotionModelInfo {
    pub name: String,
    pub manufacturer: String,
}

/// A motion manufacturer (`motion_manufacturer` table).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ManufacturerInfo {
    pub name: String,
    pub default_refresh_rate: u32,
    /// Registered only on Windows.
    pub windows_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Registry {
    /// Names of the built-in shaker layouts.
    pub builtin_shaker_layouts: Vec<String>,
    /// Shaker speaker channels (`channels`).
    pub channels: Vec<String>,
    /// NSIS installer language id → UI language.
    pub installer_languages: BTreeMap<String, String>,
    #[serde(rename = "shaker_effect")]
    pub shaker_effects: Vec<ShakerEffectInfo>,
    #[serde(rename = "shaker_channel_effect")]
    pub shaker_channel_effects: Vec<ChannelEffectInfo>,
    #[serde(rename = "motion_effect")]
    pub motion_effects: Vec<MotionEffectInfo>,
    #[serde(rename = "rumble_effect")]
    pub rumble_effects: Vec<GroupedEffectInfo>,
    #[serde(rename = "haptic_effect")]
    pub haptic_effects: Vec<GroupedEffectInfo>,
    #[serde(rename = "flag")]
    pub flags: Vec<FlagInfo>,
    #[serde(rename = "motion_model", default)]
    pub motion_models: Vec<MotionModelInfo>,
    #[serde(rename = "motion_manufacturer")]
    pub motion_manufacturers: Vec<ManufacturerInfo>,
}

impl Registry {
    pub fn builtin() -> &'static Registry {
        static REGISTRY: OnceLock<Registry> = OnceLock::new();
        REGISTRY.get_or_init(|| {
            let mut registry: Registry =
                toml::from_str(include_str!("../../data/app_tables.toml")).expect("data/app_tables.toml is valid");
            registry.motion_models = openrigcue_model::motion_models::motion_models()
                .iter()
                .map(|model| MotionModelInfo {
                    name: model.key.clone(),
                    manufacturer: model.fields["manufacturer"].as_str().expect("model manufacturer is text").into(),
                })
                .collect();
            registry
        })
    }

    /// The manufacturer of a motion model.
    pub fn manufacturer_of(&self, model: &str) -> Option<&str> {
        self.motion_models.iter().find(|m| m.name == model).map(|m| m.manufacturer.as_str())
    }

    /// The manufacturer entry `name` as registered on the given platform.
    pub fn manufacturer(&self, name: &str, windows: bool) -> Option<&ManufacturerInfo> {
        self.motion_manufacturers.iter().find(|m| m.name == name && (windows || !m.windows_only))
    }

    /// Names of the iFlag entries whose group is not `none`, in table order: the keys of a tune's
    /// `iflag` switches.
    pub fn switchable_flag_names(&self) -> impl Iterator<Item = &str> {
        self.flags.iter().filter(|f| f.group != "none").map(|f| f.name.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the contents of the built-in registries.
    #[test]
    fn registry_matches_global_variables() {
        let r = Registry::builtin();
        let ids: Vec<&str> = r.shaker_effects.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "engine",
                "gear",
                "suspension",
                "road_bump",
                "impact",
                "acceleration",
                "deceleration",
                "lateral_acceleration",
                "traction_loss",
                "wheel_slip",
                "afterburner",
                "cannon_firing",
                "canopy",
                "landing_gear",
                "missile_firing",
                "abs_lock",
                "touchdown",
                "turbulence"
            ]
        );
        assert_eq!(r.motion_effects.len(), 18);
        assert_eq!(r.motion_effects[8].id, "traction_loss_front");
        assert!(!r.motion_effects[8].intellitune);
        assert_eq!(r.rumble_effects.len(), 22);
        assert_eq!(r.haptic_effects.len(), 9);
        assert_eq!(r.channels, ["FL", "FR", "CE", "SUB", "RL", "RR"]);
        assert_eq!(r.installer_languages["1031"], "de");
        assert_eq!(r.manufacturer_of("H3"), Some("DOF REALITY"));
        assert_eq!(r.manufacturer_of("None"), Some("None"));
        assert_eq!(r.manufacturer("DOF REALITY", false).unwrap().default_refresh_rate, 30);
        assert!(r.manufacturer("PROSIMU", false).is_none());
        assert_eq!(r.manufacturer("PROSIMU", true).unwrap().default_refresh_rate, 500);
        assert!(r.manufacturer("SIMTECHREVOLUTION", true).is_none(), "simracingtech_enabled is False");
    }

    /// Code 36 has a single entry (spotter_left), at its first position in the table. Code 42 (the
    /// question mark) is left out: nothing produces it.
    #[test]
    fn flag_table_keeps_dict_overwrite_semantics() {
        let r = Registry::builtin();
        let codes: Vec<u32> = r.flags.iter().map(|f| f.code).collect();
        assert_eq!(
            codes,
            [0, 1, 2, 3, 5, 6, 7, 9, 10, 11, 20, 21, 22, 30, 31, 32, 34, 35, 36, 50, 51, 52, 53, 37, 38, 39, 40, 41]
        );
        let f36 = r.flags.iter().find(|f| f.code == 36).unwrap();
        assert_eq!(f36.name, "spotter_left");
        assert_eq!(r.switchable_flag_names().count(), 26);
    }
}
