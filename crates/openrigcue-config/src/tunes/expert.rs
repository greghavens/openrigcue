//! The expert tune sets in `data/tunes/expert_profiles.json` (tuners giba, globespy, jeffweaver, marcelo, yves).
//!
//! The sets are in the pre-2.13 tune schema (a single `surge` range, no per-effect smoothing or
//! accessory dicts), so they are not full [`super::Tune`]s. The application reads only the
//! `publish` flags and `max_telemetry_vars` (migration v153, first installation, tuner
//! `jeffweaver`); those parts and the tuner metadata are typed here, the rest of each profile stays
//! in the data file.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::Deserialize;

use crate::import::de::from_legacy;
use crate::import::shipped::ShippedData;
use crate::import::value::LegacyValue;

/// The motion ranges of the legacy schema (`max_telemetry_vars`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ExpertRanges {
    pub pitch: f64,
    pub roll: f64,
    pub yaw: f64,
    pub sway: f64,
    pub surge: f64,
    pub heave: f64,
    pub traction_loss: f64,
    #[serde(default)]
    pub pitch_rate: Option<f64>,
    #[serde(default)]
    pub roll_rate: Option<f64>,
}

/// One vehicle profile of an expert set. (Some carry vehicles nested inside vehicles, left over from
/// the bug migration v123 cleans in `profile.db`; nothing reads them.)
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ExpertTune {
    /// The tuner published this profile.
    pub publish: bool,
    pub max_telemetry_vars: ExpertRanges,
}

/// One game profile of an expert set.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ExpertGame {
    pub publish: bool,
    pub max_telemetry_vars: ExpertRanges,
    pub vehicles: BTreeMap<String, ExpertTune>,
}

/// One tuner's set.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ExpertSet {
    pub name: String,
    pub motion_equipment: String,
    pub shakers_equipment: String,
    pub wind_equipment: String,
    #[serde(rename = "db")]
    pub games: BTreeMap<String, ExpertGame>,
}

/// Tuner alias → set.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(transparent)]
pub struct ExpertProfiles(pub BTreeMap<String, ExpertSet>);

impl ExpertProfiles {
    pub fn builtin() -> &'static ExpertProfiles {
        static PROFILES: OnceLock<ExpertProfiles> = OnceLock::new();
        PROFILES.get_or_init(|| {
            from_legacy(&LegacyValue::Dict(ShippedData::get().expert_tunes.clone()))
                .unwrap_or_else(|e| panic!("data/tunes/expert_profiles.json: {e}"))
        })
    }

    pub fn tuner(&self, alias: &str) -> Option<&ExpertSet> {
        self.0.get(alias)
    }
}
