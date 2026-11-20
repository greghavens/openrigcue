//! When the active tune must be reloaded (evaluated every main-loop tick).

use crate::import::value::LegacyValue;
use openrigcue_model::python_compare::native_equal;

/// The vehicle a listener reports. The integer `0` means "no vehicle known"; a name may be
/// any string, including `''` (which the store treats like `0`, but the reload rule does not).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Vehicle {
    Unknown,
    Named(String),
}

impl Vehicle {
    pub fn named(name: impl Into<String>) -> Self {
        Vehicle::Named(name.into())
    }

    pub fn native_value(&self) -> LegacyValue {
        match self {
            Self::Unknown => LegacyValue::Int(0),
            Self::Named(name) => LegacyValue::Str(name.clone()),
        }
    }

    /// Text projection for display and older consumers; comparisons and lookups use the raw value.
    pub fn from_native(value: &LegacyValue) -> Self {
        if native_equal(value, &LegacyValue::Int(0)) {
            Self::Unknown
        } else {
            Self::Named(openrigcue_model::python_repr::str_value(value))
        }
    }
}

/// The previous-tick state the reload rule compares against, and the rule itself.
#[derive(Debug, Clone, PartialEq)]
pub struct ReloadTracker {
    /// The game running on the previous tick; `None` before the first tick.
    previous_game: Option<String>,
    /// The vehicle reported on the previous tick; a null value before the first tick.
    previous_vehicle: LegacyValue,
    /// The last vehicle reported while a game ran (`0` otherwise).
    last_vehicle: Vehicle,
    last_vehicle_value: LegacyValue,
    /// A reload was requested regardless of the rule.
    forced: bool,
}

impl Default for ReloadTracker {
    fn default() -> Self {
        ReloadTracker {
            previous_game: None,
            previous_vehicle: LegacyValue::None,
            last_vehicle: Vehicle::Unknown,
            last_vehicle_value: LegacyValue::Int(0),
            forced: false,
        }
    }
}

/// Games that mean "no game": the stand-by profile is active.
fn is_standby(game: &str) -> bool {
    matches!(game, "test" | "emulator" | "none" | "")
}

impl ReloadTracker {
    /// Requests a reload on the next tick (Cancel, profile import).
    pub fn force_reload(&mut self) {
        self.forced = true;
    }

    /// Forgets the last vehicle, as leaving profile edit mode
    /// without a listener and picking a game on the Games page do.
    pub fn forget_vehicle(&mut self) {
        self.last_vehicle = Vehicle::Unknown;
        self.last_vehicle_value = LegacyValue::Int(0);
    }

    /// The vehicle the UI shows and saves to while the listener momentarily reports none.
    pub fn displayed_vehicle<'a>(&'a self, current: &'a Vehicle) -> &'a Vehicle {
        match current {
            Vehicle::Unknown => &self.last_vehicle,
            named => named,
        }
    }

    /// Use the last actual value for numeric zero, then convert to display text.
    pub fn displayed_native_vehicle(&self, current: &LegacyValue) -> Vehicle {
        Vehicle::from_native(if native_equal(current, &LegacyValue::Int(0)) {
            &self.last_vehicle_value
        } else {
            current
        })
    }

    pub fn last_native_vehicle(&self) -> &LegacyValue {
        &self.last_vehicle_value
    }

    /// One main-loop tick: the reload rule and then the loop's bookkeeping (remember a reported vehicle, clear it
    /// when no game runs, store this tick's game and vehicle). Returns whether to reload.
    pub fn tick(&mut self, game: &str, vehicle: &Vehicle) -> bool {
        self.tick_native(game, &vehicle.native_value())
    }

    /// The same reload rule without reducing listener values to display text.
    pub fn tick_native(&mut self, game: &str, vehicle: &LegacyValue) -> bool {
        let reload = self.decide_native(game, vehicle);
        if !native_equal(vehicle, &LegacyValue::Int(0)) {
            self.remember_actual_vehicle(vehicle);
        }
        if game == "none" {
            self.forget_vehicle();
        }
        self.remember_vehicle_input(vehicle);
        self.remember_game_input(game);
        reload
    }

    /// Only the reload rule: consume the force flag without advancing later loop bookkeeping.
    pub fn decide_native(&mut self, game: &str, vehicle: &LegacyValue) -> bool {
        if self.forced {
            self.forced = false;
            return true;
        }
        let previous_game = self.previous_game.as_deref();
        if is_standby(game) {
            // Leaving a game reloads the stand-by profile; staying in stand-by does not.
            return previous_game != Some("none") && previous_game != Some(game);
        }
        if previous_game != Some(game) {
            return true;
        }
        // Same game: a vehicle change reloads, unless it is the vehicle already loaded or the
        // vehicle is momentarily unknown after a known one.
        let mut reload = !native_equal(&self.previous_vehicle, vehicle);
        if native_equal(&self.last_vehicle_value, vehicle) {
            reload = false;
        }
        if native_equal(vehicle, &LegacyValue::Int(0)) && !native_equal(&self.last_vehicle_value, &LegacyValue::Int(0))
        {
            reload = false;
        }
        reload
    }

    /// The assignment after telemetry readouts; the caller performs the nonzero guard.
    pub fn remember_actual_vehicle(&mut self, vehicle: &LegacyValue) {
        self.last_vehicle_value = vehicle.clone();
        self.last_vehicle = Vehicle::from_native(vehicle);
    }

    /// Stores this tick's vehicle; runs after sidebar refresh and before notifications.
    pub fn remember_vehicle_input(&mut self, vehicle: &LegacyValue) {
        self.previous_vehicle = vehicle.clone();
    }

    /// Stores this tick's game; follows the vehicle assignment.
    pub fn remember_game_input(&mut self, game: &str) {
        self.previous_game = Some(game.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn car(name: &str) -> Vehicle {
        Vehicle::named(name)
    }

    /// The first tick in stand-by (previous game None) reloads, later ones do not.
    #[test]
    fn standby_reloads_only_when_leaving_a_game() {
        let mut t = ReloadTracker::default();
        assert!(t.tick("none", &Vehicle::Unknown), "None != 'none' and None != 'none'");
        assert!(!t.tick("none", &Vehicle::Unknown));
        assert!(t.tick("ac", &car("bmw")));
        assert!(t.tick("none", &Vehicle::Unknown), "game left");
        assert!(!t.tick("none", &Vehicle::Unknown));
        assert!(!t.tick("emulator", &Vehicle::Unknown), "coming from 'none' never reloads");
        assert!(t.tick("none", &Vehicle::Unknown), "previous 'emulator' differs from 'none'");
    }

    /// The main loop reads the running game twice per pass: for the reload check and for the previous-game
    /// assignment near the end of the pass. The game listener thread can write it in between when a profile
    /// edit forces a game. The pass then stores a game it never compared, and the next check sees no change:
    /// the pages keep the stand-by profile's widgets (the Motion page's Roll/Pitch Type rows stay hidden)
    /// until the game or vehicle changes again. The two reads are separate steps here.
    #[test]
    fn game_change_between_the_check_and_the_assignment_is_not_reloaded() {
        let unknown = LegacyValue::Int(0);
        let mut t = ReloadTracker::default();
        assert!(t.tick_native("none", &unknown));
        // The pass: the check compares 'none' with 'none', the edit takes effect, then the assignment reads 'dcs'.
        assert!(!t.decide_native("none", &unknown));
        t.remember_vehicle_input(&unknown);
        t.remember_game_input("dcs");
        assert!(!t.decide_native("dcs", &unknown), "the following passes compare 'dcs' with 'dcs'");
        t.remember_game_input("dcs");
        assert!(!t.decide_native("dcs", &unknown));
        // The same edit seen by the check itself reloads.
        let mut t = ReloadTracker::default();
        assert!(t.tick_native("none", &unknown));
        assert!(t.decide_native("dcs", &unknown));
    }

    /// A different game in a running game always reloads.
    #[test]
    fn game_change_reloads() {
        let mut t = ReloadTracker::default();
        t.tick("ac", &car("bmw"));
        assert!(t.tick("acc", &car("bmw")));
    }

    /// Vehicle change, same vehicle, and unknown-vehicle cases.
    #[test]
    fn vehicle_rules() {
        let mut t = ReloadTracker::default();
        assert!(t.tick("ac", &Vehicle::Unknown));
        assert!(!t.tick("ac", &Vehicle::Unknown));
        assert!(t.tick("ac", &car("bmw")), "unknown → bmw reloads");
        assert!(!t.tick("ac", &car("bmw")));
        assert!(!t.tick("ac", &Vehicle::Unknown), "vehicle momentarily unknown keeps the bmw tune");
        assert!(!t.tick("ac", &car("bmw")), "back to the vehicle already loaded");
        assert!(t.tick("ac", &car("audi")));
        assert_eq!(t.displayed_vehicle(&Vehicle::Unknown), &car("audi"));
    }

    /// `''` is a name for the reload rule (only the integer 0 is "unknown").
    #[test]
    fn empty_name_is_not_unknown() {
        let mut t = ReloadTracker::default();
        t.tick("ac", &car("bmw"));
        assert!(t.tick("ac", &car("")));
    }

    /// A forced reload wins once, then the normal rule applies.
    #[test]
    fn forced_reload_is_consumed() {
        let mut t = ReloadTracker::default();
        t.tick("ac", &car("bmw"));
        t.force_reload();
        assert!(t.tick("ac", &car("bmw")));
        assert!(!t.tick("ac", &car("bmw")));
    }

    /// The main loop clears the remembered vehicle when no game runs.
    #[test]
    fn last_vehicle_cleared_without_game() {
        let mut t = ReloadTracker::default();
        t.tick("ac", &car("bmw"));
        t.tick("none", &Vehicle::Unknown);
        assert_eq!(t.displayed_vehicle(&Vehicle::Unknown), &Vehicle::Unknown);
        t.tick("ac", &car("bmw"));
        t.forget_vehicle();
        assert_eq!(t.displayed_vehicle(&Vehicle::Unknown), &Vehicle::Unknown);
    }

    #[test]
    fn native_changes_keep_numeric_equality_and_distinct_text_values() {
        let mut t = ReloadTracker::default();
        assert!(t.tick_native("ac", &LegacyValue::None));
        assert!(!t.tick_native("ac", &LegacyValue::None));
        assert_eq!(t.displayed_native_vehicle(&LegacyValue::Int(0)), car("None"));
        assert!(t.tick_native("ac", &LegacyValue::Str("None".into())));
        assert!(t.tick_native("ac", &LegacyValue::Int(1)));
        assert!(!t.tick_native("ac", &LegacyValue::Bool(true)));
        assert_eq!(t.displayed_native_vehicle(&LegacyValue::Bool(false)), car("True"));
        assert!(!t.tick_native("ac", &LegacyValue::Float(1.0)));
        assert_eq!(t.displayed_native_vehicle(&LegacyValue::Float(-0.0)), car("1.0"));
        assert!(!t.tick_native("ac", &LegacyValue::Int(0)));
        assert!(!t.tick_native("ac", &LegacyValue::Int(1)));
        assert!(t.tick_native("ac", &LegacyValue::Str("1".into())));
        let list = LegacyValue::List(vec![LegacyValue::Int(7)]);
        assert!(t.tick_native("ac", &list));
        assert!(!t.tick_native("ac", &LegacyValue::List(vec![LegacyValue::Float(7.0)])));
        assert!(t.tick_native("ac", &LegacyValue::Str("[7.0]".into())));
        t.forget_vehicle();
        assert_eq!(t.displayed_native_vehicle(&LegacyValue::Int(0)), Vehicle::Unknown);
        t.tick_native("none", &list);
        assert_eq!(t.displayed_native_vehicle(&LegacyValue::Int(0)), Vehicle::Unknown);
    }
}
