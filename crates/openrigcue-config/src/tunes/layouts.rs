//! Speaker layouts for the bass shakers (the `shakers` table of `metadata.db`, seeded from
//! `data/srs/shaker_layouts.json` and extended by the migrations).
//!
//! A layout gives, for each of the six sound-card channels, whether the speaker is used and how
//! strongly (0–10) each per-corner effect drives it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Per-speaker level of every channel effect, in registry order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelEffectLevels {
    pub engine: f64,
    pub gear: f64,
    pub afterburner: f64,
    pub cannon_firing: f64,
    pub missile_firing: f64,
    pub landing_gear_left: f64,
    pub landing_gear_nose: f64,
    pub landing_gear_right: f64,
    pub touchdown_left: f64,
    pub touchdown_nose: f64,
    pub touchdown_right: f64,
    pub turbulence: f64,
    pub canopy: f64,
    pub suspension_lf: f64,
    pub suspension_rf: f64,
    pub suspension_lr: f64,
    pub suspension_rr: f64,
    pub wheel_slip_lf: f64,
    pub wheel_slip_rf: f64,
    pub wheel_slip_lr: f64,
    pub wheel_slip_rr: f64,
    pub road_bump: f64,
    pub brake: f64,
    pub abs_lock: f64,
    pub accel: f64,
    pub impact: f64,
    pub brake_lf: f64,
    pub brake_rf: f64,
    pub accel_lr: f64,
    pub accel_rr: f64,
    pub lat_l: f64,
    pub lat_r: f64,
    pub traction_loss_l: f64,
    pub traction_loss_r: f64,
}

/// One speaker channel of a layout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpeakerChannel {
    pub enable: bool,
    pub sfx: ChannelEffectLevels,
}

/// The six channels of a layout (`channels`: FL, FR, CE, SUB, RL, RR).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpeakerLayout {
    #[serde(rename = "FL")]
    pub front_left: SpeakerChannel,
    #[serde(rename = "FR")]
    pub front_right: SpeakerChannel,
    #[serde(rename = "CE")]
    pub center: SpeakerChannel,
    #[serde(rename = "SUB")]
    pub subwoofer: SpeakerChannel,
    #[serde(rename = "RL")]
    pub rear_left: SpeakerChannel,
    #[serde(rename = "RR")]
    pub rear_right: SpeakerChannel,
}

/// Layout name → layout. `SHAKEBOX.profile` names the active one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(transparent)]
pub struct ShakerLayouts(pub BTreeMap<String, SpeakerLayout>);

impl ShakerLayouts {
    /// The layout named by `SHAKEBOX.profile`.
    pub fn get(&self, name: &str) -> Option<&SpeakerLayout> {
        self.0.get(name)
    }

    /// Stores the edited active layout on Save.
    pub fn set(&mut self, name: &str, layout: SpeakerLayout) {
        self.0.insert(name.to_owned(), layout);
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }
}
