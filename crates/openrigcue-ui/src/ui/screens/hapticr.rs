//! Tuning → HapticR (offered while `enable_hapticr` of the App settings is on): the wheel haptics with their
//! switches and the advanced engine, wheel slip / lock and traction loss settings, with the rules, actions and
//! hot keys of SIM3D (see [`super::sim3d::Kind`]). A loaded profile's values go through
//! [`super::reload`].

use openrigcue_config::catalog::registry::{GroupedEffectInfo, Registry};
use openrigcue_config::synced_profiles::Category;

use super::super::Screen;
use super::sim3d::Kind;
use super::tuning;
use crate::app::MyApp;
use crate::spec::{self, Page};

/// HapticR.
pub const HAPTICR: Kind = Kind {
    screen: Screen::HapticR,
    page,
    effects_key: "haptic_effects",
    switch_key: "haptic_switch",
    effects: haptic_effects,
    rpm_start_row: "rpm_start_haptic_perc",
    prefix: "haptic_",
    category: Category::Haptic,
    shortcuts_hint: "Shows the keyboard, gamepad, joystick and buttonbox shortcuts that switch HapticR off or to auto, and lets you change them.",
};

/// The page description.
pub fn page() -> Page {
    spec::haptic_page(Registry::builtin())
}

fn haptic_effects() -> &'static [GroupedEffectInfo] {
    &Registry::builtin().haptic_effects
}

/// Whether HapticR is enabled in the settings.
pub fn enabled(app: &MyApp) -> bool {
    Screen::HapticR.available(app)
}

/// The body.
pub fn main(app: &MyApp) -> String {
    HAPTICR.main(app)
}

/// The values Compare looks at.
pub fn compared(app: &MyApp) -> Vec<(String, Vec<String>)> {
    tuning::compared(app, &page())
}

/// A value of the screen changed.
pub fn set(app: &mut MyApp, control: &str, part: &str, value: &str) -> Result<(), String> {
    HAPTICR.set(app, control, part, value)
}

/// A button of the screen.
pub fn action(app: &mut MyApp, control: &str, index: Option<usize>) -> Result<(), String> {
    HAPTICR.action(app, control, index)
}

/// The HapticR hot keys, while HapticR is enabled.
pub fn apply_hotkeys(app: &mut MyApp, pending: &dyn Fn(&str) -> bool) -> Result<bool, String> {
    if !enabled(app) {
        return Ok(false);
    }
    HAPTICR.apply_hotkeys(app, pending)
}

#[cfg(test)]
#[path = "hapticr_tests.rs"]
mod tests;
