//! Runtime switches, test modes and published session state: the runtime parameters
//! that the service and its UI read or change while running.

use std::collections::BTreeSet;
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use openrigcue_cueing::TestSliders;
use openrigcue_model::native_value::LegacyValue;
use serde::{Deserialize, Serialize};

mod test_values;
use test_values::NativeMotionTests;

/// A value shared between the service threads and the API.
#[derive(Debug, Default)]
pub struct Shared<T>(Arc<RwLock<T>>);

impl<T> Clone for Shared<T> {
    fn clone(&self) -> Self {
        Shared(Arc::clone(&self.0))
    }
}

impl<T> Shared<T> {
    pub fn new(value: T) -> Self {
        Shared(Arc::new(RwLock::new(value)))
    }

    pub fn read(&self) -> RwLockReadGuard<'_, T> {
        self.0.read().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn write(&self) -> RwLockWriteGuard<'_, T> {
        self.0.write().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<T: Clone> Shared<T> {
    pub fn snapshot(&self) -> T {
        self.read().clone()
    }
}

/// A three-state hardware button of the sidebar (`0` off, `1` auto, `2` always on).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputMode {
    Off,
    Auto,
    AlwaysOn,
}

impl OutputMode {
    /// The mode's integer value.
    pub fn code(self) -> i64 {
        match self {
            OutputMode::Off => 0,
            OutputMode::Auto => 1,
            OutputMode::AlwaysOn => 2,
        }
    }
}

/// The sidebar switches, with their start-up values ([`Default`]).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Switches {
    /// The motion switch.
    pub motion: bool,
    /// The shaker switch.
    pub shakers: bool,
    /// The motion compensation switch.
    pub compensation: bool,
    /// The belt switch; the profile reload sets it while a game runs and clears it in stand-by.
    pub belt: bool,
    /// The HUD switch.
    pub hud: bool,
    /// The iFlag switch.
    pub iflag: bool,
    /// The SIM3D switch.
    pub sim3d: bool,
    /// The haptic switch.
    pub haptic: bool,
    /// The wind mode (left/right fans).
    pub wind: OutputMode,
    /// The centre wind mode.
    pub wind_center: OutputMode,
    /// The LED mode.
    pub led: OutputMode,
    /// The tachometer mode.
    pub tach: OutputMode,
    /// Joystick motion delivers as the `joy` listener.
    pub joystick: bool,
    /// The Joystick page's Status switch, separate from the `joy` listener switch.
    pub joystick_status: i64,
}

impl Default for Switches {
    fn default() -> Self {
        Switches {
            motion: true,
            shakers: true,
            compensation: true,
            belt: false,
            hud: true,
            iflag: true,
            sim3d: true,
            haptic: true,
            wind: OutputMode::Auto,
            wind_center: OutputMode::Auto,
            led: OutputMode::Auto,
            tach: OutputMode::Auto,
            joystick: false,
            joystick_status: 0,
        }
    }
}

/// The Hardware page test controls.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TestControls {
    /// Full-rate motion test: test mode runs at the motion refresh rate instead of 7 Hz.
    pub motion_test: bool,
    /// The eight motion test sliders (−100…100).
    #[serde(with = "sliders_serde")]
    pub motion_sliders: TestSliders,
    /// Whether the belt test runs.
    pub belt_test: bool,
    /// The belt test value (0…100).
    pub belt_test_value: f64,
    /// Belt homing: 0 not calibrated, 1 calibrated, 2 requested, 3 running.
    pub belt_homing: u8,
    /// Whether the iFlag test runs.
    pub iflag_test: bool,
    /// The haptic test: 0, or the wheel 1…4 under test.
    pub haptic_test: i64,
    /// The rumble test: 0, or the SIM3D port 1…4 under test.
    pub rumble_test: i64,
}

/// The Telemetry page controls.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TelemetryControls {
    /// The min/max reset request, cleared by the UI only after its cells are blanked.
    pub reset_min_max: bool,
    /// "Capture" switch: the UI shows values only while it is on and a game runs.
    pub capture: bool,
    /// Channel recorded for the chart.
    pub plot_channel: Option<String>,
    /// Intellitune capture.
    pub intellitune_capture: bool,
    /// "Export" switch.
    pub export: bool,
}

/// Everything the UI can switch at run time.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Controls {
    pub switches: Switches,
    pub tests: TestControls,
    pub telemetry: TelemetryControls,
    /// A game picked manually on the Games/Console page or by a profile edit.
    pub forced_game: Option<String>,
    /// The profile edit in progress: a Profile Manager Edit or an import in stand-by.
    /// Set and cleared through `/api/v1/profile/edit`.
    pub profile_edit: Option<ProfileEdit>,
    /// The hotkey-held canned effects.
    pub canned: CannedEffects,
    /// The motion test entries as dynamically typed values, alongside the numeric API view.
    #[serde(skip)]
    native_motion_tests: NativeMotionTests,
    #[serde(skip)]
    native_belt_test_value: Option<test_values::NativeTestValue>,
}

/// The canned effects (initially all unset): 1 while the hotkey of a canned effect is held, 0 after its
/// release, unset before its first key or button event (readers then take 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CannedEffects {
    pub cannon_firing: Option<u8>,
    pub missile_firing: Option<u8>,
    pub ejection_seat: Option<u8>,
}

/// The profile edit context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileEdit {
    pub game: String,
    /// The edited vehicle, `None` for the game level.
    pub vehicle: Option<String>,
    /// Whether no listener ran when the edit started.
    pub started_without_listener: bool,
}

/// What the telemetry loop publishes after every tick.
#[derive(Debug, Clone, PartialEq)]
pub struct NativeIdentity {
    pub projection: Option<String>,
    pub value: LegacyValue,
}

/// Published session values, with optional dynamically typed identities alongside their text projections.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct SessionView {
    /// The running game, `None` in stand-by.
    pub game: Option<String>,
    /// The running listener, `None` without one.
    pub listener: Option<String>,
    /// The last game that actually ran.
    pub previous_game: Option<String>,
    /// The vehicle name, `None` when unknown.
    pub vehicle: Option<String>,
    /// The location, `None` when unknown.
    pub location: Option<String>,
    #[serde(skip)]
    pub native_vehicle: Option<NativeIdentity>,
    #[serde(skip)]
    pub native_location: Option<NativeIdentity>,
    /// The shift light indicator.
    pub shift_light: bool,
    /// The steering wheel angle at which the dual wind reaches its maximum.
    pub wind_steering_angle: Option<u32>,
    /// Listener error texts shown as notifications.
    pub game_errors: BTreeSet<String>,
    /// The API listener drives the fans directly.
    pub fan_override: bool,
    /// Every channel is displayed (latched `APP.additional_telemetry`).
    pub show_all_channels: bool,
}

impl SessionView {
    pub fn set_vehicle_value(&mut self, value: LegacyValue, projection: Option<String>) {
        self.vehicle.clone_from(&projection);
        self.native_vehicle = Some(NativeIdentity { projection, value });
    }

    pub fn set_location_value(&mut self, value: LegacyValue, projection: Option<String>) {
        self.location.clone_from(&projection);
        self.native_location = Some(NativeIdentity { projection, value });
    }

    pub fn vehicle_value(&self) -> LegacyValue {
        identity_value(&self.native_vehicle, &self.vehicle)
    }

    pub fn location_value(&self) -> LegacyValue {
        identity_value(&self.native_location, &self.location)
    }

    /// The running listener as a key, `none` without one.
    pub fn listener_key(&self) -> &str {
        self.listener.as_deref().unwrap_or("none")
    }

    /// The running game as a key, `none` in stand-by.
    pub fn game_key(&self) -> &str {
        self.game.as_deref().unwrap_or("none")
    }
}

fn identity_value(native: &Option<NativeIdentity>, projection: &Option<String>) -> LegacyValue {
    native
        .as_ref()
        .filter(|native| &native.projection == projection)
        .map_or_else(|| projection.clone().map_or(LegacyValue::Int(0), LegacyValue::Str), |native| native.value.clone())
}

/// Serde for [`TestSliders`], which has no serde support of its own.
mod sliders_serde {
    use openrigcue_cueing::TestSliders;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[derive(Serialize, Deserialize, Default)]
    #[serde(default)]
    struct Sliders {
        pitch: f64,
        roll: f64,
        yaw: f64,
        sway: f64,
        surge: f64,
        heave: f64,
        traction_loss: f64,
        traction_loss_front: f64,
    }

    pub fn serialize<S: Serializer>(s: &TestSliders, serializer: S) -> Result<S::Ok, S::Error> {
        Sliders {
            pitch: s.pitch,
            roll: s.roll,
            yaw: s.yaw,
            sway: s.sway,
            surge: s.surge,
            heave: s.heave,
            traction_loss: s.traction_loss,
            traction_loss_front: s.traction_loss_front,
        }
        .serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<TestSliders, D::Error> {
        let s = Sliders::deserialize(deserializer)?;
        Ok(TestSliders {
            pitch: s.pitch,
            roll: s.roll,
            yaw: s.yaw,
            sway: s.sway,
            surge: s.surge,
            heave: s.heave,
            traction_loss: s.traction_loss,
            traction_loss_front: s.traction_loss_front,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_identity_values_follow_their_projection_and_explicit_reset() {
        let mut session = SessionView::default();
        assert_eq!(session.vehicle_value(), LegacyValue::Int(0));
        session.set_vehicle_value(LegacyValue::None, Some("None".into()));
        assert_eq!(session.vehicle_value(), LegacyValue::None);
        session.vehicle = Some("Edited".into());
        assert_eq!(session.vehicle_value(), LegacyValue::Str("Edited".into()));
        session.set_vehicle_value(LegacyValue::Int(0), None);
        assert_eq!(session.vehicle_value(), LegacyValue::Int(0));
        session.set_location_value(LegacyValue::Bool(false), None);
        assert_eq!(session.location_value(), LegacyValue::Bool(false));
        let json = serde_json::to_value(session).unwrap();
        assert!(json.get("native_vehicle").is_none());
        assert!(json.get("native_location").is_none());
    }

    /// The start-up values: motion, shakers and compensation on, belt off, hud/iflag/sim3d/haptic on,
    /// wind, centre wind, LED and tachometer on auto, the joy listener off, no motion test, no forced game.
    #[test]
    fn parameters_initial_values() {
        let c = Controls::default();
        let s = c.switches;
        assert!(s.motion && s.shakers && s.compensation && s.hud && s.iflag && s.sim3d && s.haptic);
        assert!(!s.belt && !s.joystick);
        assert_eq!([s.wind, s.wind_center, s.led, s.tach], [OutputMode::Auto; 4]);
        assert!(!c.tests.motion_test);
        assert_eq!(c.tests.motion_sliders, TestSliders::default());
        assert_eq!(c.forced_game, None);
        assert!(!c.telemetry.capture);
        assert_eq!(OutputMode::AlwaysOn.code(), 2);
    }

    #[test]
    fn controls_round_trip_through_json() {
        let mut c = Controls::default();
        c.tests.motion_sliders.heave = 40.0;
        c.switches.led = OutputMode::AlwaysOn;
        let text = serde_json::to_string(&c).unwrap();
        let back: Controls = serde_json::from_str(&text).unwrap();
        assert_eq!(back, c);
    }
}
