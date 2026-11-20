//! Joystick axis profiles (`metadata.db` table `joystick`): which controller input drives each
//! joystick action of the Joystick page.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One assignment as the Joystick page stores it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AxisMapping {
    /// Joystick device name.
    pub joystick: String,
    /// Input kind as captured (`axis <`, `axis >`, …).
    #[serde(rename = "type", alias = "kind")]
    pub kind: String,
    /// Axis or button index, as text.
    pub trigger: String,
    /// `Single Axis`, …
    pub mode: String,
    pub invert_axis: bool,
}

/// Profile name → action → assignment. The active profile is `Default` unless the user picks another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct JoystickProfiles(pub BTreeMap<String, BTreeMap<String, AxisMapping>>);

impl Default for JoystickProfiles {
    /// A fresh installation on the Windows platform: v95 creates an empty `Default` profile. (On Linux, v325
    /// and v340 then assign the magicbox Arduino Leonardo pedals; see
    /// [`crate::import::first_run_for`].)
    fn default() -> Self {
        JoystickProfiles(BTreeMap::from([("Default".to_owned(), BTreeMap::new())]))
    }
}

impl JoystickProfiles {
    pub fn profile(&self, name: &str) -> Option<&BTreeMap<String, AxisMapping>> {
        self.0.get(name)
    }
}
