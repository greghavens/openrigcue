//! The active tune and the service side of the UI main loop (every 0.5 s).
//!
//! The UI main loop runs in the process that also holds the pages; `deferred_main` starts
//! it once the page has loaded (at the UI application's start). [`MainLoop::start`] is the
//! part before the loop's first iteration: the stored settings go through the widgets that show them
//! (sliders, ranges, spin boxes, dropdowns and switches), whose handlers write back what the
//! widget holds, and the start-up runs of the motion model change. [`MainLoop::tick`] is one iteration, in the
//! loop's order: the test-control corrections, the `ui_actions` slider steps and
//! compensation shortcuts, the profile reload (the profile load followed by the same write-back of every tuning
//! widget) with the game configuration check, the device notifications of the hardware status box, the
//! notification clean-up (which empties `game_errors` while no game runs) and the calibration check. An error the
//! loop does not handle ends the loop.
//!
//! The widgets are the page descriptions of `openrigcue_ui::spec`: their ranges, steps and conversions decide what
//! a value becomes on its way through the widget ([`load`]).

use std::path::PathBuf;
use std::sync::{Arc, LockResult, Mutex, MutexGuard};
use std::time::Duration;

use openrigcue_config::catalog::{Catalog, FeatureTable, GameKind};
use openrigcue_config::import::value::LegacyValue;
use openrigcue_config::profiles::{ProfileError, ProfileLevel, ReloadTracker, Vehicle};
use openrigcue_config::settings::Settings;
use openrigcue_config::tunes::Tune;
use openrigcue_effects::accessories::dispatcher::Accessories;
use openrigcue_ui::catalog::ModelFlags;
use openrigcue_ui::spec::{Page, Store};
use serde::Serialize;

use self::load::{Documents, Forced, Loader};
use crate::api::ApiContext;
use crate::games::GameSetup;
use crate::notifications::{udp_port_owner, HardwareWidget};
use crate::runtime::Shared;
use crate::state::{ServiceEvent, ServiceState};
use crate::store::ConfigStore;

/// Period of the UI main loop.
pub const MAIN_LOOP_PERIOD: Duration = Duration::from_millis(500);

/// The calibration check's pause between the looks at `belt_homing` while the belt calibrates.
pub const CALIBRATION_POLL: Duration = Duration::from_millis(300);

/// The active tune and where it was loaded from.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ActiveProfile {
    /// Game id the tune was loaded for (`none` in stand-by).
    pub game: String,
    /// Vehicle the tune was loaded for.
    #[serde(serialize_with = "serialize_vehicle")]
    pub vehicle: Vehicle,
    pub tune: Tune,
    /// Whether the tune is the vehicle's own or the game's (the UI greys a vehicle shown with its game's tune).
    #[serde(serialize_with = "serialize_level")]
    pub level: ProfileLevel,
    /// Counts reloads and edits, so the loops know when to rebuild what they derive from the tune.
    pub generation: u64,
    /// Changes only when a profile load replaces the tune, not when a widget edits it.
    pub load_generation: u64,
    #[serde(skip)]
    native: LegacyValue,
    #[serde(skip)]
    projection: LegacyValue,
}

fn serialize_vehicle<S: serde::Serializer>(vehicle: &Vehicle, serializer: S) -> Result<S::Ok, S::Error> {
    match vehicle {
        Vehicle::Unknown => serializer.serialize_none(),
        Vehicle::Named(name) => serializer.serialize_some(name),
    }
}

fn serialize_level<S: serde::Serializer>(level: &ProfileLevel, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(match level {
        ProfileLevel::Game => "game",
        ProfileLevel::Vehicle => "vehicle",
    })
}

impl ActiveProfile {
    /// Loads the tune of `game` and `vehicle`: the vehicle's tune when stored, else the
    /// game's; a game missing from the profile table is an error.
    pub fn load(
        store: &ConfigStore,
        game: &str,
        vehicle: &Vehicle,
        generation: u64,
    ) -> Result<ActiveProfile, ProfileError> {
        Self::load_native(store, game, &vehicle.native_value(), generation)
    }

    pub fn load_native(
        store: &ConfigStore,
        game: &str,
        vehicle_value: &LegacyValue,
        generation: u64,
    ) -> Result<ActiveProfile, ProfileError> {
        let vehicle = Vehicle::from_native(vehicle_value);
        let (key, _) = openrigcue_config::profiles::profile_key(game, &Vehicle::Unknown);
        let row = store
            .profile_row(key)
            .map_err(|error| ProfileError::Native(error.to_string()))?
            .ok_or_else(|| ProfileError::UnknownGame(key.to_owned()))?;
        let (mut native, level) =
            openrigcue_config::profiles::ProfileStore::resolve_native_identity_row(key, vehicle_value, &row)?;
        if !matches!(native, LegacyValue::Object(_)) {
            native = LegacyValue::Object(openrigcue_model::native_value::NativeObject::new(native));
        }
        let tune = Tune::from_legacy(&native).unwrap_or_else(|_| {
            let mut profiles = openrigcue_config::profiles::ProfileStore::default();
            profiles.accept_native_game(key, Some(row));
            profiles.resolve(game, &vehicle).map(|(tune, _)| tune).unwrap_or_else(|_| {
                openrigcue_config::tunes::FactoryTunes::builtin().get("emulator").expect("factory stand-by").clone()
            })
        });
        let projection = LegacyValue::from(tune.to_json());
        Ok(ActiveProfile {
            game: game.to_owned(),
            vehicle,
            tune,
            level,
            generation,
            load_generation: generation,
            native,
            projection,
        })
    }

    /// The stored active value plus edits from consumers that use the typed view. Without such an edit this is
    /// the stored value itself. With one, the stored value keeps what it holds: the edit is made in copies of the
    /// dictionaries it falls into, and every other member is still the stored one.
    pub fn native_tune(&self) -> LegacyValue {
        let mut native = self.native.clone();
        let typed = LegacyValue::from(self.tune.to_json());
        openrigcue_config::import::reconcile_detached(&mut native, &self.projection, &typed);
        native
    }

    /// The tune fields the SimTech driver reads, as stored, reconciled without copying unrelated profile rows.
    pub(crate) fn native_simtech_tune(&self) -> LegacyValue {
        if self.native.resolved().as_dict().is_none() || self.projection.as_dict().is_none() {
            return self.native_tune();
        }
        let mut fields = openrigcue_model::native_value::Dict::new();
        for (key, current) in [("scn6_speed", self.tune.scn6_speed), ("scn6_acceleration", self.tune.scn6_acceleration)]
        {
            if let Some(value) = self.native_tune_field(&[key], Some(LegacyValue::from(serde_json::json!(current)))) {
                fields.insert(key, value);
            }
        }
        fields.into()
    }

    /// The two Thanos filter values, retaining their stored kinds and absent keys.
    pub(crate) fn native_thanos_tune(&self) -> LegacyValue {
        if self.native.resolved().as_dict().is_none() || self.projection.as_dict().is_none() {
            return self.native_tune();
        }
        let mut fields = openrigcue_model::native_value::Dict::new();
        for (key, current) in [
            ("thanos_spike_filter_horizontal", self.tune.thanos_spike_filter_horizontal),
            ("thanos_spike_filter_vertical", self.tune.thanos_spike_filter_vertical),
        ] {
            if let Some(value) = self.native_tune_field(&[key], Some(LegacyValue::from(serde_json::json!(current)))) {
                fields.insert(key, value);
            }
        }
        fields.into()
    }

    /// The four stored fields read by the telemetry dispatcher. Ordinary dictionary lookups reconcile only
    /// the selected scalar; malformed ancestors go through the full reconciliation.
    pub(crate) fn native_telemetry_tune(&self) -> LegacyValue {
        use openrigcue_config::import::value::Dict;
        let mut fields = Dict::new();
        for (key, current) in
            [("default_max_speed_kmh", self.tune.default_max_speed_kmh), ("gear_shift_perc", self.tune.gear_shift_perc)]
        {
            if let Some(value) = self.native_tune_field(&[key], Some(LegacyValue::from(serde_json::json!(current)))) {
                fields.insert(key, value);
            }
        }
        let mut effects = Dict::new();
        for (key, current) in
            [("pitch_type", self.tune.motion_effects.pitch_type), ("roll_type", self.tune.motion_effects.roll_type)]
        {
            let current = current.map(|value| LegacyValue::from(serde_json::json!(value)));
            if let Some(value) = self.native_tune_field(&["motion_effects", key], current) {
                effects.insert(key, value);
            }
        }
        fields.insert("motion_effects", LegacyValue::Dict(effects));
        LegacyValue::Dict(fields)
    }

    fn native_tune_field(&self, path: &[&str], current: Option<LegacyValue>) -> Option<LegacyValue> {
        let full_lookup = || path.iter().try_fold(self.native_tune(), |value, key| value.item_owned(key).ok());
        let (key, parents) = path.split_last()?;
        let (mut native, mut before) = (self.native.clone(), self.projection.clone());
        for parent in parents {
            let (Some(next_native), Some(next_before)) =
                (native.item_owned(parent).ok(), before.item_owned(parent).ok())
            else {
                return full_lookup();
            };
            (native, before) = (next_native, next_before);
        }
        let (native, before) = (native.resolved(), before.resolved());
        let (Some(native), Some(before)) = (native.as_dict(), before.as_dict()) else {
            return full_lookup();
        };
        if before.get(key) == current.as_ref() {
            native.get(key).cloned()
        } else {
            current
        }
    }

    pub fn tune_json(&self) -> Result<serde_json::Value, openrigcue_config::import::de::ConvertError> {
        openrigcue_config::import::de::from_legacy(&self.native_tune())
    }

    /// Accepts an exact in-memory dictionary edit. The raw value remains authoritative when a typed
    /// consumer cannot represent it; successfully convertible fields still update the typed view.
    pub fn replace_native_tune(&mut self, native: LegacyValue) {
        if let Ok(tune) = Tune::from_legacy(&native) {
            self.tune = tune;
        } else if let Some(fields) = native.resolved().as_dict() {
            // `native` may be the stored value itself edited in place, or share dictionaries with it, so the
            // stored value cannot tell which fields the edit changed: every field the typed view does not
            // already hold is tried.
            let mut candidate = LegacyValue::from(self.tune.to_json());
            for (key, value) in fields.iter() {
                if candidate.as_dict().and_then(|fields| fields.get_key(key)) == Some(value) {
                    continue;
                }
                let mut next = candidate.clone();
                next.as_dict_mut().expect("typed tune dictionary").insert(key.clone(), value.clone());
                if let Ok(tune) = Tune::from_legacy(&next) {
                    self.tune = tune;
                    candidate = next;
                }
            }
        }
        self.projection = LegacyValue::from(self.tune.to_json());
        self.native = if matches!(native, LegacyValue::Object(_)) {
            native
        } else {
            LegacyValue::Object(openrigcue_model::native_value::NativeObject::new(native))
        };
    }

    /// The profile loaded at start: the stand-by (`emulator`) tune, which the start checks
    /// of the configuration guarantee (`Configuration::open_for` fails without it).
    pub fn standby(store: &ConfigStore) -> ActiveProfile {
        ActiveProfile::load(store, "none", &Vehicle::Unknown, 0)
            .expect("the configuration's start checks guarantee the emulator tune")
    }
}

/// The vehicle a session reports.
pub fn session_vehicle(name: Option<&str>) -> Vehicle {
    name.map_or(Vehicle::Unknown, Vehicle::named)
}

// ---- the pages' widgets as the main loop drives them ----------------------------------------------------------

/// The pages `deferred_main` builds, from the settings they are built with.
fn pages(settings: &Settings) -> Vec<Page> {
    openrigcue_ui::ui_document(openrigcue_ui::BuildInputs {
        smoothing_step: settings.features.smoothing_slider_step,
        six_dof: matches!(settings.motion.model.as_str(), "H6" | "P6"),
    })
    .pages
}

/// The model table entry of `model`; an unknown model is an error.
fn model_flags(model: &str) -> Result<&'static ModelFlags, UnknownModel> {
    openrigcue_ui::catalog::models().iter().find(|m| m.name == model).ok_or_else(|| UnknownModel(model.to_owned()))
}

/// The error for a model the model table does not have.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("KeyError: {0:?}")]
pub struct UnknownModel(pub String);

/// Finds a control by page key and content key.
fn control<'a>(pages: &'a [Page], page: &str, id: &str) -> Option<&'a openrigcue_ui::spec::Control> {
    pages.iter().find(|p| p.key == page)?.control(id)
}

/// Runs a [`Loader`] over the tune and settings, then takes back what the handlers wrote.
struct Pass<'a> {
    pages: &'a [Page],
    loader: Loader<'a>,
}

impl<'a> Pass<'a> {
    fn new(pages: &'a [Page], original: &'a Documents, settings: &Settings) -> Pass<'a> {
        let mph = settings.app.speed_unit == openrigcue_config::settings::MeasurementUnit::MilesPerHour;
        Pass { pages, loader: Loader::new(original, mph) }
    }

    /// Sets one row's widget (slider, range, spin box, dropdown or switch) from its stored value (or `forced`).
    /// A row the page does not have is skipped.
    fn set(&mut self, page: &str, id: &str, forced: Option<Forced>) {
        if let Some(control) = control(self.pages, page, id) {
            self.loader.load(control, forced);
        }
    }

    /// Sets a row's range slider from its stored values, within limits the widget was given beforehand.
    fn set_range_within(&mut self, page: &str, id: &str, limit_min: f64, limit_max: f64) {
        if let Some(control) = control(self.pages, page, id) {
            self.loader.load_range_within(control, limit_min, limit_max);
        }
    }

    /// The documents after the handlers' writes, as typed values; a document the writes made unreadable stays
    /// as it was.
    fn finish(self, tune: &Tune, settings: &Settings) -> (Tune, Settings) {
        let edited = self.loader.into_documents();
        let tune = serde_json::from_value(edited.tune).unwrap_or_else(|error| {
            tracing::warn!(%error, "tune write-back rejected");
            tune.clone()
        });
        let settings = serde_json::from_value(edited.settings).unwrap_or_else(|error| {
            tracing::warn!(%error, "settings write-back rejected");
            settings.clone()
        });
        (tune, settings)
    }
}

fn documents(tune: &Tune, settings: &Settings) -> Documents {
    Documents {
        tune: serde_json::to_value(tune).unwrap_or_default(),
        settings: serde_json::to_value(settings).unwrap_or_default(),
        controls: serde_json::Value::Null,
    }
}

/// One column of the game's feature row; `None` when there is no row for the game or
/// no such column.
fn feature(game: &str, column: &str) -> Option<bool> {
    let table = FeatureTable::builtin();
    let row = table.game(game)?;
    table.columns().iter().any(|c| c == column).then(|| row.enabled().iter().any(|c| c == column))
}

/// The running game's catalog type, `racing` when the lookup fails (stand-by, test).
fn is_racing(game: &str) -> bool {
    Catalog::builtin().game(game).is_none_or(|g| g.kind == GameKind::Racing)
}

/// The models whose reload overrides the Motion rows.
fn scn6150_disabled_effects(model: &str) -> Option<&'static [&'static str]> {
    match model {
        "STR-SCN6150" => Some(&[
            "yaw",
            "heave",
            "traction_loss",
            "traction_loss_front",
            "wheel_slip",
            "rumble_strips",
            "afterburner",
            "cannon_firing",
            "canopy",
            "landing_gear",
            "missile_firing",
            "abs_lock",
        ]),
        "STR-SCN6150-TL" => Some(&[
            "heave",
            "traction_loss_front",
            "wheel_slip",
            "rumble_strips",
            "afterburner",
            "cannon_firing",
            "canopy",
            "landing_gear",
            "missile_firing",
            "abs_lock",
        ]),
        _ => None,
    }
}

/// The reload's write-back (under the UI update lock, after the profile load): every
/// tuning row is set from the loaded tune and its handler stores what the widget holds, rounded to the slider's
/// step, clamped to its range, through the row's conversions. On top of the rows' own values:
/// - unless `INTELLIBOX.nbr_fans` is the text 2, wind curving is switched off and the curving effect set to 1;
/// - an iFlag pattern the game does not support is switched off;
/// - the racing-only Motion rows are left alone for other games, the Belt Tensioner row for models without a belt
///   tensioner motor, Reaction Speed for SIMTECHREVOLUTION models (which set their actuator speed and
///   acceleration rows instead);
/// - the STR-SCN6150 models force their fixed Motion values and switch their unsupported effects off.
///
/// Returns the tune and the settings the handlers wrote (Reaction Speed also writes `MOTIONBOX.reaction_speed`).
pub fn reload_write_back(tune: &Tune, settings: &Settings, game: &str) -> Result<(Tune, Settings), UnknownModel> {
    let model = model_flags(&settings.motion.model)?;
    let pages = pages(settings);
    let original = documents(tune, settings);
    let mut pass = Pass::new(&pages, &original, settings);
    let simtech = model.manufacturer == "SIMTECHREVOLUTION";
    let racing = is_racing(game);
    let registry = openrigcue_config::catalog::registry::Registry::builtin();
    for page in &pages {
        // The HapticR rows exist only with `APP.enable_hapticr` on.
        if page.key == "HapticR" && !settings.app.enable_hapticr {
            continue;
        }
        for row in page.controls() {
            let skipped = match (page.key.as_str(), row.id.as_str()) {
                (
                    "Motion",
                    "gear_smoothing_factor" | "min_traction_loss_activation" | "min_traction_loss_front_activation",
                ) => !racing,
                ("Motion", "belt_tensioner") => !model.belt_tensioner_motor,
                ("Motion", "reaction_speed") => simtech,
                ("Motion", "scn6_speed" | "scn6_acceleration") => !simtech,
                _ => false,
            };
            if skipped {
                continue;
            }
            pass.loader.load_max_telemetry(row);
            if row.field.as_ref().is_none_or(|f| f.store != Store::Tune) {
                continue;
            }
            let forced = match (page.key.as_str(), row.id.as_str()) {
                ("Wind", "wind_curving") if settings.wind.fan_count != 2 => Some(Forced::Bool(false)),
                ("Wind", "curving_effect") if settings.wind.fan_count != 2 => Some(Forced::Number(1.0)),
                ("IFlag", name)
                    if registry.switchable_flag_names().any(|f| f == name) && feature(game, name) == Some(false) =>
                {
                    Some(Forced::Bool(false))
                }
                _ => None,
            };
            pass.loader.load(row, forced);
        }
    }
    if let Some(disabled) = scn6150_disabled_effects(&settings.motion.model) {
        pass.set("Motion", "motion_max_horizontal", Some(Forced::Number(100.0)));
        pass.set("Motion", "motion_smoothing_factor", Some(Forced::Number(1.0)));
        pass.set("Motion", "min_traction_loss_activation", Some(Forced::Number(0.0)));
        pass.set("Motion", "min_traction_loss_front_activation", Some(Forced::Number(0.0)));
        pass.set("Motion", "motion_main_actuators_sway", Some(Forced::Number(100.0)));
        pass.set("Motion", "motion_main_actuators_surge", Some(Forced::Number(100.0)));
        pass.set("Motion", "motion_main_actuators_traction_loss", Some(Forced::Number(0.0)));
        pass.set("Motion", "motion_main_actuators_traction_loss_front", Some(Forced::Number(0.0)));
        for effect in disabled.iter().filter(|e| **e != "traction_loss") {
            if let Some(row) = control(&pages, "Motion", effect) {
                if let Some(toggle) = &row.toggle {
                    pass.loader.write(&toggle.field, serde_json::Value::Bool(false));
                }
            }
        }
    }
    Ok(pass.finish(tune, settings))
}

/// A motion model change, past the handler's first-time guard: `MOTIONBOX.model` takes the value;
/// without "2 TL" in the model name the front traction loss is zeroed and switched off
/// (the activation spin box and the main-actuator slider go to 0, the switch goes off),
/// with it the stored values go through those rows and the switch is
/// turned on; under the Windows rules a model without IntelliComp selects the WITMotion sensor
/// (past the sensor type handler's own first-time guard).
pub fn motion_model_change(
    tune: &Tune,
    settings: &Settings,
    value: &str,
    windows: bool,
) -> Result<(Tune, Settings), UnknownModel> {
    let mut settings = settings.clone();
    settings.motion.model = value.to_owned();
    let pages = pages(&settings);
    let original = documents(tune, &settings);
    let mut pass = Pass::new(&pages, &original, &settings);
    if !value.contains("2 TL") {
        pass.set("Motion", "min_traction_loss_front_activation", Some(Forced::Number(0.0)));
        pass.set("Motion", "motion_main_actuators_traction_loss_front", Some(Forced::Number(0.0)));
        pass.set_toggle("Motion", "traction_loss_front", false);
    } else {
        pass.set("Motion", "min_traction_loss_front_activation", None);
        pass.set("Motion", "motion_main_actuators_traction_loss_front", None);
        pass.set_toggle("Motion", "traction_loss_front", true);
    }
    if windows && value != "None" && !model_flags(value)?.enable_intellicomp {
        pass.loader.write(
            &openrigcue_ui::spec::Field::settings(&["compensation", "sensor"]),
            serde_json::Value::String("witmotion".into()),
        );
    }
    Ok(pass.finish(tune, &settings))
}

impl Pass<'_> {
    /// Sets the on/off switch of an effect row.
    fn set_toggle(&mut self, page: &str, id: &str, value: bool) {
        if let Some(toggle) = control(self.pages, page, id).and_then(|c| c.toggle.as_ref()) {
            self.loader.write(&toggle.field, serde_json::Value::Bool(value));
        }
    }
}

/// The number of ports of a rumble model.
fn rumble_ports(model: &str) -> u8 {
    match model {
        "3 Ports" => 3,
        "4 Ports" => 4,
        _ => 0,
    }
}

/// The settings part of the UI main loop before its first iteration, in its order: each widget is given its
/// stored value and the widget's handler stores what the widget holds (handlers behind a
/// first-time guard store nothing, see the `quiet_load` rows), plus the handlers that change other rows:
/// the LED count change (LED segment limits, then the segments again), the fan count change (wind curving
/// rows) and the two start-up runs of the motion model change.
///
/// Rows whose choices exist only at run time (sound card, speaker layout, com ports) and the switches and text
/// inputs, which only write on a change of the widget, store nothing here.
pub fn start_up(tune: &Tune, settings: &Settings, windows: bool) -> Result<(Tune, Settings), UnknownModel> {
    let (mut tune, mut settings) = (tune.clone(), settings.clone());
    if windows {
        step(&mut tune, &mut settings, |pass, settings| {
            for id in [
                "height",
                "surge_stroke",
                "vertical_actuators_stroke",
                "actuator_distance_left_right",
                "actuator_distance_front_back",
                "surge_factor",
                "surge_horizontal_factor",
                "sway_factor",
            ] {
                pass.set("Motion Compensation", id, None);
            }
            // For the H6 and P6 the factors come from the settings. The page has those rows only for H6/P6,
            // so for other models nothing is set.
            if matches!(settings.motion.model.as_str(), "H6" | "P6") {
                for id in ["yaw_factor", "roll_factor", "pitch_factor"] {
                    pass.set("Motion Compensation", id, None);
                }
            }
            pass.set("Motion Compensation", "sensor_type", None);
            pass.set("Motion Compensation", "installation_mode", None);
        });
    }
    step(&mut tune, &mut settings, |pass, _| {
        for id in ["log_level", "startup_page", "measurement_unit"] {
            pass.set("App", id, None);
        }
    });
    // The motion manufacturer row and its change handler: the handler's first-time guard skips the refresh
    // rate, and the model change it runs returns at that handler's own first-time guard.
    let model = if settings.motion.model == "None" {
        // `MOTIONBOX.model` takes the first entry of the model dropdown.
        let manufacturer = model_flags("None")?.manufacturer.clone();
        openrigcue_ui::catalog::models()
            .iter()
            .find(|m| m.manufacturer == manufacturer)
            .map_or_else(|| "None".to_owned(), |m| m.name.clone())
    } else {
        settings.motion.model.clone()
    };
    // The motion model row is set, which runs the model change handler; then the handler runs once more.
    for _ in 0..2 {
        (tune, settings) = motion_model_change(&tune, &settings, &model, windows)?;
    }
    step(&mut tune, &mut settings, |pass, settings| {
        pass.set("Hardware", "rumble_model", None);
        for port in 1..=rumble_ports(&settings.sim3d.model) {
            pass.set("Hardware", &format!("rumble_port{port}"), None);
        }
        pass.set("Hardware", "belt_calibration_power_slider", None);
        pass.set("Hardware", "belt_calibration_offset_slider", None);
        pass.set("Motion", "refresh_rate", None);
        pass.set("LED", "nbr_segments", None);
    });
    step(&mut tune, &mut settings, |pass, settings| {
        // The LED count spin box is set, which runs its change handler: each segment's limits become 1…value and the
        // stored segment goes through the range again, then `nbr_leds` takes the integer value.
        pass.set("Hardware", "nbr_leds", None);
        let count = openrigcue_ui::slider::py_int(load::spinbox_value(1.0, 150.0, f64::from(settings.leds.nbr_leds)));
        for index in 1..=4 {
            pass.set_range_within("LED", &format!("segment{index}"), 1.0, count);
        }
    });
    step(&mut tune, &mut settings, |pass, _| {
        pass.set("Hardware", "led_model", None);
        pass.set("Hardware", "hud_model", None);
        pass.set("Hardware", "nbr_fans", None);
    });
    // The fan count change: one fan switches wind curving off and sets the curving effect to 1; otherwise the speed
    // factor and wind curving rows are set from the tune.
    step(&mut tune, &mut settings, |pass, settings| {
        if settings.wind.fan_count == 1 {
            pass.set("Wind", "wind_curving", Some(Forced::Bool(false)));
            pass.set("Wind", "curving_effect", Some(Forced::Number(1.0)));
        } else {
            pass.set("Wind", "speed_factor", None);
            pass.set("Wind", "wind_curving", None);
        }
    });
    step(&mut tune, &mut settings, |pass, settings| {
        pass.set("Hardware", "iflag_dim_perc", None);
        pass.set("Hardware", "iflag_orientation", None);
        pass.set("Hardware", "brightness", None);
        let count = f64::from(settings.leds.nbr_leds);
        for index in 1..=4 {
            pass.set_range_within("LED", &format!("segment{index}"), 1.0, count);
            pass.set("LED", &format!("segment{index}dropdown"), None);
        }
        pass.set("Hardware", "wind_type", None);
    });
    Ok((tune, settings))
}

/// One group of widget settings on the current tune and settings; the next group sees what this one wrote.
fn step(tune: &mut Tune, settings: &mut Settings, calls: impl FnOnce(&mut Pass<'_>, &Settings)) {
    let pages = pages(settings);
    let original = documents(tune, settings);
    let mut pass = Pass::new(&pages, &original, settings);
    calls(&mut pass, settings);
    (*tune, *settings) = pass.finish(tune, settings);
}

/// Why the UI main loop ended: an error the loop does not handle.
#[derive(Debug, thiserror::Error)]
pub enum MainLoopEnd {
    /// The slider of a `ui_actions` entry was read, and its row is not on its page.
    #[error(transparent)]
    Slider(#[from] crate::hotkeys::MissingSlider),
    /// A slider callback assigned its value, then rebuilding the shaker channel groups failed.
    #[error("{}: {}", .0.class, .0.message)]
    ShakerRebuild(openrigcue_effects::shakers::tuning::DspError),
    /// The profile load of a game the profile table does not have.
    #[error("load_game_config: {0}")]
    Profile(#[from] ProfileError),
    /// `MOTIONBOX.model` names a model the model table does not have.
    #[error("motion_models: {0}")]
    Model(#[from] UnknownModel),
    /// The game status lookup of the game configuration check failed.
    #[error("game_status: {0}")]
    GameStatus(#[from] openrigcue_config::patch::PatchError),
    /// Iterating the dismissed-game keys in the game configuration check failed.
    #[error("game_config_notifications: {0}")]
    NotificationStorage(#[from] openrigcue_config::StoreError),
}

/// Processing state and its independently readable vehicle metadata.
#[derive(Clone)]
pub struct MainLoopHandle {
    processing: Arc<Mutex<MainLoop>>,
    published_tracker: Shared<ReloadTracker>,
}

impl MainLoopHandle {
    pub fn new(main_loop: MainLoop) -> Self {
        Self { published_tracker: main_loop.published_tracker.clone(), processing: Arc::new(Mutex::new(main_loop)) }
    }

    pub fn lock(&self) -> LockResult<MutexGuard<'_, MainLoop>> {
        self.processing.lock()
    }

    pub fn displayed_native_vehicle(&self, current: &LegacyValue) -> Vehicle {
        self.published_tracker.read().displayed_native_vehicle(current)
    }

    pub fn previous_vehicle_value(&self) -> LegacyValue {
        self.published_tracker.read().last_native_vehicle().clone()
    }
}

/// The UI main loop's state.
pub struct MainLoop {
    tracker: ReloadTracker,
    published_tracker: Shared<ReloadTracker>,
    setup: GameSetup,
    proc_root: PathBuf,
    /// What the hardware status box was last drawn from.
    hardware: Option<HardwareWidget>,
    /// The belt homing state of the previous iteration (`None` before the first iteration).
    previous_belt_homing: Option<u8>,
}

impl MainLoop {
    pub fn new(setup: GameSetup, proc_root: PathBuf) -> MainLoop {
        MainLoop {
            tracker: ReloadTracker::default(),
            published_tracker: Shared::new(ReloadTracker::default()),
            setup,
            proc_root,
            hardware: None,
            previous_belt_homing: None,
        }
    }

    pub fn setup(&self) -> &GameSetup {
        &self.setup
    }

    /// Cancel or a profile import: reload on the next tick.
    pub fn force_reload(&mut self) {
        self.tracker.force_reload();
    }

    fn consume_reload_request(&mut self, state: &ServiceState) {
        if state.consume_profile_reload_request() {
            self.tracker.force_reload();
        }
    }

    /// Leaving profile edit / picking a game: forget the last vehicle.
    pub fn forget_vehicle(&mut self) {
        self.tracker.forget_vehicle();
        *self.published_tracker.write() = self.tracker.clone();
    }

    fn track_vehicle(&mut self, game: &str, vehicle: &LegacyValue) -> bool {
        let reload = self.tracker.tick_native(game, vehicle);
        *self.published_tracker.write() = self.tracker.clone();
        reload
    }

    /// The two vehicle reads after telemetry readouts, before remembering location.
    pub fn remember_actual_vehicle(&mut self, ctx: &ApiContext) {
        let vehicle = ctx.state.session.read().vehicle_value();
        if !openrigcue_model::python_compare::native_equal(&vehicle, &LegacyValue::Int(0)) {
            let vehicle = ctx.state.session.read().vehicle_value();
            self.tracker.remember_actual_vehicle(&vehicle);
            *self.published_tracker.write() = self.tracker.clone();
        }
    }

    /// The independent vehicle/game assignments after sidebar refresh, before notifications.
    pub fn remember_profile_inputs(&mut self, ctx: &ApiContext) {
        self.tracker.remember_vehicle_input(&ctx.state.session.read().vehicle_value());
        self.tracker.remember_game_input(ctx.state.session.read().game_key());
        *self.published_tracker.write() = self.tracker.clone();
    }

    /// The vehicle the UI shows and saves to.
    pub fn displayed_vehicle(&self, current: &Vehicle) -> Vehicle {
        self.tracker.displayed_vehicle(current).clone()
    }

    pub fn displayed_native_vehicle(&self, current: &LegacyValue) -> Vehicle {
        self.tracker.displayed_native_vehicle(current)
    }

    /// The service side of the profile load, called synchronously by the one UI main loop. Actual widget
    /// write-back belongs to that loop's page handlers, not a second declarative copy of the pages.
    pub fn synchronize_profile(&mut self, ctx: &ApiContext) -> Result<(), MainLoopEnd> {
        if self.profile_reload_needed(ctx) {
            self.load_current_profile(ctx)?;
        }
        Ok(())
    }

    pub fn profile_reload_needed(&mut self, ctx: &ApiContext) -> bool {
        let state = &ctx.state;
        let session = state.session.snapshot();
        self.consume_reload_request(state);
        self.tracker.decide_native(session.game_key(), &session.vehicle_value())
    }

    pub fn load_current_profile(&mut self, ctx: &ApiContext) -> Result<(), MainLoopEnd> {
        let state = &ctx.state;
        let session = state.session.snapshot();
        let game = session.game_key();
        let vehicle = session.vehicle_value();
        let generation = state.profile.read().generation + 1;
        let profile = ActiveProfile::load_native(&state.config.read(), game, &vehicle, generation)?;
        let vehicle_level = profile.level == ProfileLevel::Vehicle;
        *state.profile.write() = profile;
        state.announce(ServiceEvent::ProfileLoaded {
            game: game.to_owned(),
            vehicle: match Vehicle::from_native(&vehicle) {
                Vehicle::Named(name) => Some(name),
                Vehicle::Unknown => None,
            },
            vehicle_level,
        });
        Ok(())
    }

    /// The reload's game setup check, after the IntelliTune driving-style update.
    pub fn check_current_game_config(&self, ctx: &ApiContext) -> Result<(), MainLoopEnd> {
        let game = ctx.state.session.read().game_key().to_owned();
        self.check_game_config(&ctx.state, &game)
    }

    /// Notification cleanup at the end of the same UI pass. The UI itself owns calibration and video state.
    pub fn finish_ui_pass(&mut self, ctx: &ApiContext) -> Result<(), MainLoopEnd> {
        self.finish_ui_pass_with_owner(ctx, |port| udp_port_owner(&self.proc_root, port))
    }

    fn finish_ui_pass_with_owner(
        &self,
        ctx: &ApiContext,
        owner: impl FnOnce(u16) -> Option<String>,
    ) -> Result<(), MainLoopEnd> {
        let state = &ctx.state;
        let session = state.session.snapshot();
        let mut notifications = state.notifications.write();
        let before = notifications.clone();
        if let Some(game) = session.game.as_deref() {
            let config = state.config.read();
            let port =
                Catalog::builtin().game(game).and_then(|info| config.settings().network_ports.get(&info.listener));
            notifications.check_port_in_use(game, &session.game_errors, port, owner);
        }
        // The port-owner lookup can wait on the OS. The steps that follow
        // read game/listener/errors again, after that lookup.
        let session = state.session.snapshot();
        notifications.check_memory_read_refused(session.game.as_deref(), &session.game_errors);
        notifications.tidy(
            session.game.as_deref(),
            session.previous_game.as_deref(),
            session.listener.is_some(),
            &session.game_errors,
        );
        if *notifications != before {
            state.announce(ServiceEvent::Notifications);
        }
        drop(notifications);
        if state.session.read().game.is_none() {
            openrigcue_sources::udp_racing::parameters::Parameters::shared().clear_game_errors();
            state.session.write().game_errors.clear();
        }
        Ok(())
    }

    /// The part of the UI main loop before its first iteration ([`start_up`]), on the stand-by tune and the
    /// settings in memory.
    pub fn start(&mut self, ctx: &ApiContext) -> Result<(), MainLoopEnd> {
        let state = &ctx.state;
        let tune = state.profile.read().tune.clone();
        let before = state.config.read().settings().clone();
        let (tune, settings) = start_up(&tune, &before, state.platform.is_windows())?;
        apply_tune(state, tune);
        apply_settings(ctx, &before, settings);
        Ok(())
    }

    /// One iteration of the UI main loop, in its order: the test-control
    /// corrections, the `ui_actions` slider steps and compensation shortcuts (then the queue is emptied), the reload
    /// decision and, on reload, the profile load with its write-back, `belt_switch` and the game configuration
    /// check; then the hardware status box when its inputs changed, the UDP port check, the notification clean-up
    /// (`game_errors` is emptied while no game runs) and the calibration check. `Err` is an error that ends the
    /// loop.
    pub fn tick(&mut self, ctx: &ApiContext) -> Result<(), MainLoopEnd> {
        let state = &ctx.state;
        correct_test_controls(ctx);
        crate::hotkeys::consume_ui_actions(ctx)?;
        let session = state.session.snapshot();
        let game = session.game_key().to_owned();
        let vehicle = session.vehicle_value();
        self.consume_reload_request(state);
        if self.track_vehicle(&game, &vehicle) {
            self.reload(ctx, &game, &vehicle)?;
        }
        let widget = hardware_widget(ctx)?;
        if self.hardware != Some(widget) {
            let mut notifications = state.notifications.write();
            let before = notifications.clone();
            notifications.set_hardware_widget(&widget);
            if *notifications != before {
                state.announce(ServiceEvent::Notifications);
            }
            self.hardware = Some(widget);
        }
        {
            let mut notifications = state.notifications.write();
            let before = notifications.clone();
            // The UDP access check runs while no such notification is shown; the clean-up of the shown ones follows.
            if let Some(game) = session.game.as_deref() {
                let config = state.config.read();
                let port =
                    Catalog::builtin().game(game).and_then(|info| config.settings().network_ports.get(&info.listener));
                notifications
                    .check_port_in_use(game, &session.game_errors, port, |port| udp_port_owner(&self.proc_root, port));
            }
            notifications.check_memory_read_refused(session.game.as_deref(), &session.game_errors);
            notifications.tidy(
                session.game.as_deref(),
                session.previous_game.as_deref(),
                session.listener.is_some(),
                &session.game_errors,
            );
            if *notifications != before {
                state.announce(ServiceEvent::Notifications);
            }
        }
        if session.game.is_none() {
            openrigcue_sources::udp_racing::parameters::Parameters::shared().clear_game_errors();
            state.session.write().game_errors.clear();
        }
        let homing = belt_homing(&ctx.accessories);
        if homing == 3 && self.previous_belt_homing != Some(3) {
            check_calibration(state.clone(), Arc::clone(&ctx.accessories));
        }
        self.previous_belt_homing = Some(homing);
        Ok(())
    }

    /// The reload branch of the main loop.
    fn reload(&mut self, ctx: &ApiContext, game: &str, vehicle: &LegacyValue) -> Result<(), MainLoopEnd> {
        let state = &ctx.state;
        let generation = state.profile.read().generation + 1;
        let mut profile = ActiveProfile::load_native(&state.config.read(), game, vehicle, generation)?;
        let before = state.config.read().settings().clone();
        let (tune, settings) = reload_write_back(&profile.tune, &before, game)?;
        profile.tune = tune;
        let level = profile.level;
        *state.profile.write() = profile;
        apply_settings(ctx, &before, settings);
        state.controls.write().switches.belt = game != "none";
        tracing::info!(game, ?vehicle, ?level, "profile loaded");
        state.announce(ServiceEvent::ProfileLoaded {
            game: game.to_owned(),
            vehicle: match Vehicle::from_native(vehicle) {
                Vehicle::Named(name) => Some(name),
                Vehicle::Unknown => None,
            },
            vehicle_level: level == ProfileLevel::Vehicle,
        });
        self.check_game_config(state, game)
    }

    /// Checks the running game's configuration; a failing game status lookup ends the loop.
    fn check_game_config(&self, state: &ServiceState, game: &str) -> Result<(), MainLoopEnd> {
        let forced = state.controls.read().forced_game.is_some();
        let (status, dismissed) = {
            let config = state.config.read();
            // The game status lookup absorbs any read/decode error of the selected row.
            // A successfully read empty or false value still acknowledges instructions.
            let status = self.setup.status_native(
                game,
                forced,
                &config.settings().network_ports,
                &config.settings().native_snapshot(),
                &|id| config.metadata_row("notifications", id).ok().flatten().is_some(),
            )?;
            let mut dismissed = false;
            if matches!(
                status,
                openrigcue_config::patch::GameStatus::NotPatched | openrigcue_config::patch::GameStatus::Instructions
            ) {
                // Only the keys are iterated, not the values: opaque values
                // do not matter, and membership stops at the first match.
                for key in config.metadata_keys("game_config_notifications")? {
                    if key? == game {
                        dismissed = true;
                        break;
                    }
                }
            }
            (status, dismissed)
        };
        let mut notifications = state.notifications.write();
        let before = notifications.clone();
        notifications.check_game_config(game, &status, dismissed);
        if *notifications != before {
            state.announce(ServiceEvent::Notifications);
        }
        Ok(())
    }
}

/// The active tune after a write-back; a change counts as an edit, so the loops rebuild what they derive from it.
fn apply_tune(state: &ServiceState, tune: Tune) {
    let mut profile = state.profile.write();
    if profile.tune != tune {
        profile.tune = tune;
        profile.generation += 1;
        drop(profile);
        state.announce(ServiceEvent::TuneChanged);
    }
}

/// The settings in memory after a write-back, with the handlers' immediate effects ([`ui_state::settings_changed`]).
///
/// [`ui_state::settings_changed`]: crate::api::ui_state::settings_changed
fn apply_settings(ctx: &ApiContext, before: &Settings, settings: Settings) {
    if settings == *before {
        return;
    }
    ctx.state.config.write().replace_settings(settings.clone());
    crate::api::ui_state::settings_changed(ctx, before, &settings);
    ctx.state.announce(ServiceEvent::Settings);
}

/// The belt homing state.
fn belt_homing(accessories: &Accessories) -> u8 {
    crate::accessories::homing_code(accessories.belt_homing())
}

/// Whether no real game runs (`none` or `test`).
fn idle(game: Option<&str>) -> bool {
    matches!(game, None | Some("test"))
}

/// The main loop's corrections of the test controls, every iteration:
/// - the motion test is allowed only with a model, the platform connected and no game running; otherwise every
///   test slider goes to 0 and the motion test mode is cleared;
/// - while a game runs, the belt test stops and the belt test slider goes to 0;
/// - with the belt switch off, the belt test stops and the belt homing state returns to 0.
fn correct_test_controls(ctx: &ApiContext) {
    let state = &ctx.state;
    let game = state.session.read().game.clone();
    let connected = state.motion.read().link.status == openrigcue_devices::motion::LinkStatus::Connected;
    let has_model = state.config.read().settings().motion.model != "None";
    let mut controls = state.controls.write();
    if !(has_model && connected && idle(game.as_deref())) {
        controls.reset_motion_test_sliders();
        controls.tests.motion_test = false;
    }
    if !idle(game.as_deref()) {
        controls.tests.belt_test = false;
        controls.reset_belt_test_value();
    }
    if !controls.switches.belt {
        controls.tests.belt_test = false;
        drop(controls);
        ctx.accessories.set_belt_homing(crate::accessories::homing_of(0));
    }
}

/// What the hardware status box reads for the device notifications.
fn hardware_widget(ctx: &ApiContext) -> Result<HardwareWidget, UnknownModel> {
    let state = &ctx.state;
    let settings = state.config.read().settings().clone();
    let model = model_flags(&settings.motion.model)?;
    let motion = state.motion.read().clone();
    let accessories = crate::accessories::accessory_status(&ctx.accessories);
    let belt_switch = state.controls.read().switches.belt;
    // The protocol's numeric parameter, retaining non-finite values outside the JSON status view.
    let iflag_version = ctx.accessories.iflag_version().number();
    Ok(HardwareWidget {
        motion_auto_search: settings.motion.auto_search,
        motion_connected: motion.link.status == openrigcue_devices::motion::LinkStatus::Connected,
        axis_assignment_incomplete: model.axis_assignment && !motion.assignment.as_ref().is_some_and(|a| a.status),
        yaw_vr: model.manufacturer == "Yaw VR",
        yaw_vr_centered: ctx.motion.driver_settings().yaw_shared.get().centered,
        iflag_auto_search: settings.iflag.auto_search,
        iflag_connected: accessories.iflag == "Connected",
        iflag_old_firmware: iflag_version.is_some_and(|v| v <= 1.3),
        belt_auto_search: settings.belt.auto_search,
        belt_connected: accessories.belt == "Connected",
        belt_needs_calibration: accessories.belt_homing == 0 && belt_switch,
    })
}

/// The belt calibration check, on its own thread: while `belt_homing` is 2 or 3 it looks again every 0.3 s, then reports
/// "Calibration Completed" when it ended at 1 and "Calibration failed" otherwise (as an alert).
fn check_calibration(state: ServiceState, accessories: Arc<Accessories>) {
    let spawned = std::thread::Builder::new().name("check_calibration".into()).spawn(move || {
        while matches!(belt_homing(&accessories), 2 | 3) {
            std::thread::sleep(CALIBRATION_POLL);
        }
        if belt_homing(&accessories) == 1 {
            crate::notifications::show_alert(&state, "Success", "Calibration has finished", "green");
        } else {
            crate::notifications::show_alert(&state, "Failed", "Calibration did not succeed", "red");
        }
    });
    if let Err(error) = spawned {
        tracing::warn!(%error, "cannot start the calibration check");
    }
}

/// A notification's first button, pressed by the user ([`crate::notifications::Notifications::confirm`]): the
/// notification hides, then "Center" centres the Yaw VR platform (motion homing is requested while the platform is
/// connected, then the "Center command sent to YawVR" alert) and "Calibrate" starts the belt calibration
/// (belt homing is requested with the belt connected and its switch on). "Game
/// Page" and "Instructions" are returned for the page to show. `None` when no such notification is shown.
pub fn confirm_notification(ctx: &ApiContext, id: u64) -> Option<crate::notifications::Confirmed> {
    use crate::notifications::Confirmed;
    let confirmed = ctx.state.notifications.write().confirm(id)?;
    ctx.state.announce(ServiceEvent::Notifications);
    match confirmed {
        Confirmed::CenterYawVr => {
            if ctx.state.motion.read().link.status == openrigcue_devices::motion::LinkStatus::Connected {
                ctx.motion.driver_settings().yaw_shared.update(|s| s.homing_requested = true);
            }
            crate::notifications::show_alert(&ctx.state, "Success", "YawVR was told to center", "green");
        }
        Confirmed::CalibrateBelt => {
            if ctx.accessories.connection_status(openrigcue_effects::accessories::port_scan::Device::Belt)
                == "Connected"
                && ctx.state.controls.read().switches.belt
            {
                ctx.accessories.set_belt_homing(crate::accessories::homing_of(2));
            }
        }
        Confirmed::GamePage | Confirmed::Instructions { .. } => {}
    }
    Some(confirmed)
}

/// The widgets as the main loop drives them: a stored value goes into the widget and the widget's
/// handler stores what the widget then holds.
pub mod load {
    use openrigcue_ui::slider::py_int;
    use openrigcue_ui::spec::{Choice, Control, Field, Read, Store, Widget, Write};
    use serde_json::Value;

    /// The documents the rows are bound to: the tune, the settings and the run-time controls, as the API serves
    /// them.
    #[derive(Debug, Clone, PartialEq)]
    pub struct Documents {
        pub tune: Value,
        pub settings: Value,
        pub controls: Value,
    }

    impl Documents {
        pub fn get(&self, store: Store) -> &Value {
            match store {
                Store::Tune => &self.tune,
                Store::Settings => &self.settings,
                Store::Controls => &self.controls,
            }
        }

        fn get_mut(&mut self, store: Store) -> &mut Value {
            match store {
                Store::Tune => &mut self.tune,
                Store::Settings => &mut self.settings,
                Store::Controls => &mut self.controls,
            }
        }
    }

    /// A value the main loop sets instead of the stored one.
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub enum Forced {
        Number(f64),
        Bool(bool),
    }

    /// The value at `path`; array elements by index.
    pub fn get_path<'a>(document: &'a Value, path: &[String]) -> Option<&'a Value> {
        path.iter().try_fold(document, |node, key| match node {
            Value::Array(items) => items.get(key.parse::<usize>().ok()?),
            Value::Object(map) => map.get(key),
            _ => None,
        })
    }

    /// Sets `path` inside `document`, creating objects on the way; arrays are indexed by number.
    pub fn set_path(document: &mut Value, path: &[String], value: Value) {
        let Some((last, parents)) = path.split_last() else { return };
        let mut node = document;
        for key in parents {
            node = match node {
                Value::Array(items) => match key.parse::<usize>().ok().and_then(|i| items.get_mut(i)) {
                    Some(item) => item,
                    None => return,
                },
                Value::Object(map) => map.entry(key.clone()).or_insert_with(|| Value::Object(Default::default())),
                _ => return,
            };
        }
        match node {
            Value::Array(items) => {
                if let Some(item) = last.parse::<usize>().ok().and_then(|i| items.get_mut(i)) {
                    *item = value;
                }
            }
            Value::Object(map) => {
                map.insert(last.clone(), value);
            }
            _ => {}
        }
    }

    /// A number as the documents carry it: integral values as integers.
    pub fn number(x: f64) -> Value {
        if x.is_finite() && x.fract() == 0.0 && x.abs() < 9_007_199_254_740_992.0 {
            Value::from(x as i64)
        } else {
            serde_json::Number::from_f64(x).map_or(Value::Null, Value::Number)
        }
    }

    /// A stored value as a number (a boolean is 0 or 1).
    fn as_number(value: &Value) -> Option<f64> {
        match value {
            Value::Number(n) => n.as_f64(),
            Value::Bool(b) => Some(f64::from(u8::from(*b))),
            Value::String(text) => text.trim().parse().ok(),
            _ => None,
        }
    }

    /// km/h → mph factor of the main loop (the product is truncated).
    pub const KMH_TO_MPH: f64 = 0.621371;
    /// mph → km/h factor of the Wind page's default max speed row.
    pub const MPH_TO_KMH: f64 = 1.60934;

    /// The widget value a stored value is shown as, before the widget's own rounding.
    pub fn read_value(read: Read, value: f64, mph: bool) -> f64 {
        match read {
            Read::Number => value,
            Read::IntOfFloat => py_int(value),
            Read::SpeedToUnit if mph => py_int(value * KMH_TO_MPH),
            Read::SpeedToUnit => value,
        }
    }

    /// The stored value a widget value becomes (the right-hand side of the row handler's assignment).
    pub fn write_value(write: Write, value: f64, mph: bool) -> Value {
        match write {
            Write::Number => number(value),
            Write::Int => number(py_int(value)),
            Write::Bool => Value::Bool(value != 0.0),
            Write::BoolInt => Value::from(i64::from(value != 0.0)),
            Write::SpeedFromUnit => number(py_int(value * if mph { MPH_TO_KMH } else { 1.0 })),
        }
    }

    /// A spin box change: the value kept inside the spin box's min/max.
    pub fn spinbox_value(min: f64, max: f64, value: f64) -> f64 {
        value.max(min).min(max)
    }

    /// The option a dropdown shows for a stored value: the first entry whose value equals it,
    /// or the last entry when nothing matches.
    pub fn shown_choice<'a>(options: &'a [Choice], stored: &Value) -> Option<&'a Choice> {
        let same = |a: &Value, b: &Value| match (a, b) {
            (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
            _ => a == b,
        };
        options.iter().find(|o| same(&o.value, stored)).or(options.last())
    }

    /// Writes rows into a copy of the documents.
    pub struct Loader<'a> {
        original: &'a Documents,
        edited: Documents,
        mph: bool,
    }

    impl<'a> Loader<'a> {
        pub fn new(original: &'a Documents, mph: bool) -> Loader<'a> {
            Loader { original, edited: original.clone(), mph }
        }

        /// The documents with every write so far.
        pub fn into_documents(self) -> Documents {
            self.edited
        }

        /// A handler's assignment.
        pub fn write(&mut self, field: &Field, value: Value) {
            set_path(self.edited.get_mut(field.store), &field.path, value);
        }

        fn stored(&self, field: &Field) -> Option<f64> {
            get_path(self.original.get(field.store), &field.path).and_then(as_number)
        }

        /// Sets one row's widget (slider, range, spin box, dropdown or switch) with its stored value, or
        /// `forced`. A row without a stored value is left alone.
        pub fn load(&mut self, control: &Control, forced: Option<Forced>) {
            let Some(field) = &control.field else { return };
            let fields: Vec<&Field> = std::iter::once(field).chain(control.also.iter()).collect();
            let stored = get_path(self.original.get(field.store), &field.path);
            match &control.widget {
                Widget::Slider { scale } => {
                    let source = match forced {
                        Some(Forced::Number(value)) => value,
                        Some(Forced::Bool(value)) => f64::from(u8::from(value)),
                        None => match stored.and_then(as_number) {
                            Some(value) => read_value(control.read, value, self.mph),
                            None => return,
                        },
                    };
                    let widget = scale.snap(source);
                    let written = write_value(control.write, widget, self.mph);
                    for f in fields {
                        self.write(f, written.clone());
                    }
                }
                Widget::Range { scale, high } => {
                    self.load_range(control, scale.limit_min, scale.limit_max, scale.step, high);
                }
                Widget::Number { min, max, .. } => {
                    let source = match forced {
                        Some(Forced::Number(value)) => value,
                        Some(Forced::Bool(value)) => f64::from(u8::from(value)),
                        None => match stored.and_then(as_number) {
                            Some(value) => read_value(control.read, value, self.mph),
                            None => return,
                        },
                    };
                    let widget = spinbox_value(*min, *max, source);
                    let written = write_value(control.write, widget, self.mph);
                    for f in fields {
                        self.write(f, written.clone());
                    }
                }
                Widget::Switch { .. } => {
                    // Setting a switch runs its handler only when the switch changes; loading the stored value writes
                    // that value back, so only a forced value writes anything.
                    let value = match forced {
                        Some(Forced::Bool(value)) => value,
                        Some(Forced::Number(value)) => value != 0.0,
                        None => return,
                    };
                    let written = write_value(control.write, f64::from(u8::from(value)), self.mph);
                    for f in fields {
                        self.write(f, written.clone());
                    }
                }
                Widget::Select { options } => {
                    let Some(stored) = stored else { return };
                    let Some(choice) = shown_choice(options, stored) else { return };
                    if control.quiet_load {
                        return;
                    }
                    for f in fields {
                        self.write(f, choice.value.clone());
                    }
                }
                Widget::Button { .. } | Widget::Readout | Widget::Text => {}
            }
        }

        /// Sets a range slider whose limits were changed beforehand.
        pub fn load_range_within(&mut self, control: &Control, limit_min: f64, limit_max: f64) {
            if let Widget::Range { scale, high } = &control.widget {
                self.load_range(control, limit_min, limit_max, scale.step, high);
            }
        }

        fn load_range(&mut self, control: &Control, limit_min: f64, limit_max: f64, step: f64, high: &Field) {
            let Some(low_field) = &control.field else { return };
            let (Some(low), Some(top)) = (self.stored(low_field), self.stored(high)) else { return };
            let scale = openrigcue_ui::slider::RangeScale::new(limit_min, limit_max, step);
            let (low, top) =
                scale.snap(read_value(control.read, low, self.mph), read_value(control.read, top, self.mph));
            let (low, top) = (write_value(control.write, low, self.mph), write_value(control.write, top, self.mph));
            self.write(low_field, low);
            self.write(high, top);
        }

        /// The max-telemetry spin box of a motion row: the reload reads the row's key of `max_telemetry_vars`
        /// (`<axis>_rate` for pitch and roll following the rate), the spin box keeps it within its range and
        /// its handler stores the integer value. A missing key leaves the box alone.
        pub fn load_max_telemetry(&mut self, control: &Control) {
            let Some(max) = &control.max_telemetry else { return };
            let key = max_telemetry_key(&self.original.tune, &max.key);
            let field = Field::tune(&["max_telemetry_vars", &key]);
            let Some(stored) = self.stored(&field) else { return };
            let widget = py_int(spinbox_value(max.min, max.max, stored));
            self.write(&field, number(widget));
        }
    }

    /// The `max_telemetry_vars` key of a motion row: pitch and roll use `<axis>_rate` when the axis type is Rate.
    /// A tune without the axis type reads as Angle.
    pub fn max_telemetry_key(tune: &Value, key: &str) -> String {
        let rate = matches!(key, "pitch" | "roll")
            && get_path(tune, &["motion_effects".to_owned(), format!("{key}_type")]).and_then(Value::as_str)
                == Some("rate");
        if rate {
            format!("{key}_rate")
        } else {
            key.to_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::motion::testing::OfflineHost;
    use crate::motion::MotionHandle;
    use crate::shakers::{ShakersHandle, SilentBeep};
    use crate::state::OriginalPlatform;
    use crate::telemetry::TelemetryHandle;
    use openrigcue_config::profiles::SaveTarget;
    use openrigcue_config::settings::{Configuration, MeasurementUnit};
    use openrigcue_effects::shakers::SampleBank;
    use openrigcue_sources::derive::Dispatcher;

    fn context(tmp: &tempfile::TempDir) -> ApiContext {
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let motion = MotionHandle::for_settings(state.config.read().settings(), Box::new(OfflineHost::default()));
        let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), false);
        let setup = GameSetup::discover(tmp.path(), &Default::default());
        ApiContext {
            state,
            telemetry: TelemetryHandle::new(Dispatcher::new(Vec::new())),
            motion,
            shakers,
            accessories: Arc::new(Accessories::new(std::time::Instant::now())),
            compensation: crate::compensation::VrCompensation::new(None, tmp.path().join("rig-pose")),
            main_loop: MainLoopHandle::new(MainLoop::new(setup, tmp.path().join("proc"))),
            srs_install: None,
            host_ip: "127.0.0.1".into(),
            voice: crate::voice::Voice::off(),
            ui: Arc::new(crate::api::ui_state::UiServices::off()),
            joystick: crate::joystick::JoystickService::offline(),
            hotkeys: crate::hotkeys::Hotkeys::offline(),
        }
    }

    fn tick(ctx: &ApiContext) {
        ctx.main_loop.lock().unwrap().tick(ctx).unwrap();
    }

    #[test]
    fn notification_cleanup_reads_current_listener_after_port_owner_lookup() {
        use crate::notifications::NotificationKind;
        let tmp = tempfile::tempdir().unwrap();
        let ctx = context(&tmp);
        {
            let mut session = ctx.state.session.write();
            session.game = Some("dirt4".into());
            session.game_errors.insert(crate::notifications::PORT_IN_USE_TEXT.into());
        }
        ctx.state.notifications.write().check_game_config(
            "dirt4",
            &openrigcue_config::patch::GameStatus::Instructions,
            false,
        );
        ctx.main_loop
            .lock()
            .unwrap()
            .finish_ui_pass_with_owner(&ctx, |_| {
                ctx.state.session.write().listener = Some("dirt".into());
                None
            })
            .unwrap();
        assert!(
            ctx.state.notifications.read().items.iter().all(|item| item.kind != NotificationKind::GameInstructions),
            "telemetry received during owner lookup must hide the instructions in this same pass"
        );
    }

    #[test]
    fn notification_cleanup_reads_current_game_before_clearing_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = context(&tmp);
        {
            let mut session = ctx.state.session.write();
            session.game = Some("dirt4".into());
            session.game_errors.insert(crate::notifications::PORT_IN_USE_TEXT.into());
        }
        ctx.main_loop
            .lock()
            .unwrap()
            .finish_ui_pass_with_owner(&ctx, |_| {
                ctx.state.session.write().game = None;
                None
            })
            .unwrap();
        assert!(
            ctx.state.session.read().game_errors.is_empty(),
            "a game ending during owner lookup must clear errors in this same pass"
        );
    }

    /// The end of a UI pass raises the refused memory read of Elite Dangerous once per game session, from
    /// the game errors the listener filled.
    #[test]
    fn refused_memory_read_is_notified_once_per_game_session() {
        use crate::notifications::{NotificationKind, MEMORY_READ_REFUSED_TEXT};
        let tmp = tempfile::tempdir().unwrap();
        let ctx = context(&tmp);
        let shown = || {
            let notifications = ctx.state.notifications.read();
            notifications.items.iter().filter(|item| item.kind == NotificationKind::MemoryReadRefused).count()
        };
        let pass = |game: Option<&str>, refused: bool| {
            {
                let mut session = ctx.state.session.write();
                session.game = game.map(str::to_owned);
                if refused {
                    session.game_errors.insert(MEMORY_READ_REFUSED_TEXT.into());
                }
            }
            ctx.main_loop.lock().unwrap().finish_ui_pass_with_owner(&ctx, |_| None).unwrap();
        };
        pass(Some("dirt4"), true);
        assert_eq!(shown(), 0, "another game");
        pass(None, false);
        pass(Some("ed"), false);
        assert_eq!(shown(), 0, "the game is read");
        for _ in 0..3 {
            pass(Some("ed"), true);
        }
        assert_eq!(shown(), 1);
        let id = ctx.state.notifications.read().items.iter().find(|item| item.game == "ed").unwrap().id;
        ctx.state.notifications.write().dismiss(id, "ed");
        pass(Some("ed"), true);
        assert_eq!(shown(), 0, "dismissed for this session");
        pass(None, false);
        assert!(ctx.state.session.read().game_errors.is_empty());
        pass(Some("ed"), true);
        pass(Some("ed"), true);
        assert_eq!(shown(), 1, "the next session");
    }

    fn standby_tune() -> Tune {
        let store = ConfigStore::in_memory(Configuration::first_run());
        ActiveProfile::standby(&store).tune
    }

    /// The start loads the stand-by (`emulator`) tune.
    #[test]
    fn startup_profile_is_standby() {
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        let profile = state.profile.read();
        assert_eq!(profile.game, "none");
        assert_eq!(profile.level, ProfileLevel::Game);
        let emulator = state.config.read().config().profiles.get("emulator").unwrap().gear_shift_perc;
        assert_eq!(profile.tune.gear_shift_perc, emulator);
    }

    #[test]
    fn live_vehicle_snapshot_retains_native_fallback_and_forget_updates() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = context(&tmp);
        let mut processing = ctx.main_loop.lock().unwrap();
        {
            let mut session = ctx.state.session.write();
            session.game = Some("ac".into());
            session.set_vehicle_value(LegacyValue::Float(1.0), None);
        }
        processing.synchronize_profile(&ctx).unwrap();
        assert_eq!(ctx.main_loop.displayed_native_vehicle(&LegacyValue::Bool(false)), Vehicle::Unknown);
        ctx.state.session.write().set_vehicle_value(LegacyValue::Float(2.0), None);
        processing.remember_actual_vehicle(&ctx);
        assert_eq!(ctx.main_loop.displayed_native_vehicle(&LegacyValue::Bool(false)), Vehicle::named("2.0"));
        assert_eq!(ctx.main_loop.displayed_native_vehicle(&LegacyValue::Str("".into())), Vehicle::named(""));
        assert_eq!(ctx.main_loop.displayed_native_vehicle(&LegacyValue::None), Vehicle::named("None"));
        processing.forget_vehicle();
        assert_eq!(ctx.main_loop.displayed_native_vehicle(&LegacyValue::Float(-0.0)), Vehicle::Unknown);
        assert_eq!(ctx.main_loop.displayed_native_vehicle(&LegacyValue::Bool(true)), Vehicle::named("True"));
        processing.track_vehicle("ac", &LegacyValue::Str("remembered".into()));
        processing.track_vehicle("none", &LegacyValue::Int(0));
        assert_eq!(ctx.main_loop.displayed_native_vehicle(&LegacyValue::Int(0)), Vehicle::Unknown);
    }

    #[test]
    fn live_vehicle_snapshot_does_not_publish_a_vehicle_before_failed_profile_load() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = context(&tmp);
        {
            let mut session = ctx.state.session.write();
            session.game = Some("missing-profile".into());
            session.set_vehicle_value(LegacyValue::Str("new car".into()), None);
        }
        let mut processing = ctx.main_loop.lock().unwrap();
        assert!(processing.synchronize_profile(&ctx).is_err());
        assert_eq!(ctx.main_loop.displayed_native_vehicle(&LegacyValue::Int(0)), Vehicle::Unknown);
        assert_eq!(ctx.state.profile.read().game, "none");
    }

    #[test]
    fn profile_load_keeps_belt_mode_until_its_later_widget_reload_stage() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = context(&tmp);
        ctx.state.controls.write().switches.belt = false;
        ctx.state.session.write().game = Some("ac".into());
        ctx.main_loop.lock().unwrap().synchronize_profile(&ctx).unwrap();
        assert!(!ctx.state.controls.read().switches.belt);
    }

    #[test]
    fn reload_decision_defers_load_and_load_reads_current_game_after_the_lock_boundary() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = context(&tmp);
        ctx.state.session.write().game = Some("ac".into());
        let mut processing = ctx.main_loop.lock().unwrap();
        assert!(processing.profile_reload_needed(&ctx));
        assert_eq!(ctx.state.profile.read().game, "none");
        ctx.state.session.write().game = Some("dirtrally2.0".into());
        processing.load_current_profile(&ctx).unwrap();
        assert_eq!(ctx.state.profile.read().game, "dirtrally2.0");
        assert!(!ctx.state.controls.read().switches.belt);
    }

    /// The reload with its profile load: a new game loads its tune, a stored vehicle tune is used at vehicle
    /// level, belt_switch follows game_running.
    #[test]
    fn game_and_vehicle_reload() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = context(&tmp);
        let state = &ctx.state;
        state.config.write().update(|c| {
            let mut tune = c.profiles.resolve("dirtrally2.0", &Vehicle::Unknown).unwrap().0;
            tune.gear_shift_perc = 77.0;
            c.profiles.save("dirtrally2.0", &SaveTarget::Vehicle("Fiesta".into()), tune).unwrap();
        });
        tick(&ctx);
        state.session.write().game = Some("dirtrally2.0".into());
        tick(&ctx);
        assert_eq!(state.profile.read().game, "dirtrally2.0");
        assert_eq!(state.profile.read().level, ProfileLevel::Game);
        assert!(state.controls.read().switches.belt);
        state.session.write().vehicle = Some("Fiesta".into());
        tick(&ctx);
        let profile = state.profile.read().clone();
        assert_eq!(profile.level, ProfileLevel::Vehicle);
        assert_eq!(profile.tune.gear_shift_perc, 77.0);
        state.session.write().game = None;
        state.session.write().vehicle = None;
        tick(&ctx);
        assert_eq!(state.profile.read().game, "none");
        assert!(!state.controls.read().switches.belt);
    }

    #[test]
    fn native_vehicle_change_with_the_same_text_reloads_the_correct_profile() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = context(&tmp);
        ctx.state.config.write().update(|c| {
            let mut tune = c.profiles.resolve("ac", &Vehicle::Unknown).unwrap().0;
            tune.gear_shift_perc = 77.0;
            c.profiles.save("ac", &SaveTarget::Vehicle("None".into()), tune).unwrap();
        });
        {
            let mut session = ctx.state.session.write();
            session.game = Some("ac".into());
            session.set_vehicle_value(LegacyValue::None, Some("None".into()));
        }
        {
            let mut processing = ctx.main_loop.lock().unwrap();
            processing.synchronize_profile(&ctx).unwrap();
            processing.remember_actual_vehicle(&ctx);
            processing.remember_profile_inputs(&ctx);
        }
        let generation = ctx.state.profile.read().generation;
        assert_eq!(ctx.state.profile.read().level, ProfileLevel::Game, "None is not the name 'None'");
        ctx.state.session.write().set_vehicle_value(LegacyValue::Str("None".into()), Some("None".into()));
        ctx.main_loop.lock().unwrap().synchronize_profile(&ctx).unwrap();
        let profile = ctx.state.profile.read();
        assert_eq!(profile.generation, generation + 1);
        assert_eq!(profile.level, ProfileLevel::Vehicle);
        assert_eq!(profile.tune.gear_shift_perc, 77.0);
    }

    #[test]
    fn native_numeric_vehicle_loads_its_key_and_keeps_equal_numeric_changes() {
        use openrigcue_config::import::value::Key;
        let tmp = tempfile::tempdir().unwrap();
        let ctx = context(&tmp);
        let mut row = ctx.state.config.read().profile_row("ac").unwrap().unwrap();
        let mut numeric_tune = row.clone();
        numeric_tune.as_dict_mut().unwrap().insert("gear_shift_perc", LegacyValue::Int(43));
        let mut named_tune = row.clone();
        named_tune.as_dict_mut().unwrap().insert("gear_shift_perc", LegacyValue::Int(77));
        let vehicles = row.as_dict_mut().unwrap().get_mut("vehicles").unwrap().as_dict_mut().unwrap();
        vehicles.insert(Key::Int(1), numeric_tune);
        vehicles.insert("1", named_tune);
        ctx.state.config.write().stage_game_row("ac", Some(row)).unwrap();
        {
            let mut session = ctx.state.session.write();
            session.game = Some("ac".into());
            session.set_vehicle_value(LegacyValue::Int(1), Some("1".into()));
        }
        tick(&ctx);
        let generation = ctx.state.profile.read().generation;
        assert_eq!(ctx.state.profile.read().level, ProfileLevel::Vehicle);
        assert_eq!(ctx.state.profile.read().tune.gear_shift_perc, 43.0);
        ctx.state.session.write().set_vehicle_value(LegacyValue::Float(1.0), Some("1.0".into()));
        tick(&ctx);
        assert_eq!(ctx.state.profile.read().generation, generation);
        assert_eq!(ctx.state.profile.read().tune.gear_shift_perc, 43.0);
        assert_eq!(ctx.main_loop.lock().unwrap().displayed_native_vehicle(&LegacyValue::Int(0)), Vehicle::named("1.0"));
        ctx.state.session.write().set_vehicle_value(LegacyValue::Str("1".into()), Some("1".into()));
        tick(&ctx);
        assert_eq!(ctx.state.profile.read().generation, generation + 1);
        assert_eq!(ctx.state.profile.read().tune.gear_shift_perc, 77.0);
    }

    /// Loading the tune of a game the store does not have is an error, and a
    /// reload of such a game ends the main loop.
    #[test]
    fn unknown_store_entry_raises() {
        let mut configuration = Configuration::first_run();
        configuration.profiles = Default::default();
        let store = ConfigStore::in_memory(configuration);
        assert!(ActiveProfile::load(&store, "acc", &Vehicle::Unknown, 3).is_err());
        let tmp = tempfile::tempdir().unwrap();
        let ctx = context(&tmp);
        ctx.state.session.write().game = Some("wrc".into());
        let ended = ctx.main_loop.lock().unwrap().tick(&ctx);
        assert!(matches!(ended, Err(MainLoopEnd::Profile(_))), "{ended:?}");
    }

    /// The model lookup fails for a model the table does not have.
    #[test]
    fn unknown_model_raises() {
        let mut settings = Settings::default();
        settings.motion.model = "No Such Platform".into();
        assert_eq!(
            reload_write_back(&standby_tune(), &settings, "none").unwrap_err(),
            UnknownModel("No Such Platform".into())
        );
    }

    /// A `ui_actions` slider whose row is not on its page ends the main loop before the reload.
    #[test]
    fn missing_slider_ends_the_main_loop() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = context(&tmp);
        ctx.state.session.write().game = Some("acc".into());
        ctx.hotkeys.queue("belt_sway_curving_up");
        let ended = ctx.main_loop.lock().unwrap().tick(&ctx);
        assert!(matches!(ended, Err(MainLoopEnd::Slider(_))), "{ended:?}");
        assert_eq!(ctx.state.profile.read().game, "none", "the reload did not run");
    }

    /// The notification clean-up empties `game_errors` while no game runs and keeps it while one does.
    #[test]
    fn game_errors_are_emptied_without_a_game() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = context(&tmp);
        ctx.state.session.write().game_errors.insert("bind failed".into());
        ctx.state.session.write().game = Some("acc".into());
        tick(&ctx);
        assert_eq!(ctx.state.session.read().game_errors.len(), 1);
        ctx.state.session.write().game = None;
        tick(&ctx);
        assert!(ctx.state.session.read().game_errors.is_empty());
    }

    /// The reload writes every tuning row back through its widget: Boost (1…100) stores its minimum for 0,
    /// Reaction Speed (70…100, as an integer) also writes `MOTIONBOX.reaction_speed`, an effect slider rounds half to even,
    /// the speed slider converts through mph, the max-telemetry box clamps to ±1800.
    #[test]
    fn reload_goes_through_the_widgets() {
        let mut tune = standby_tune();
        tune.motion_boost_factor = 0.0;
        tune.motion_reaction_speed = 10.0;
        tune.motion_effects.pitch = 6.5;
        tune.default_max_speed_kmh = 300.0;
        let mut settings = Settings::default();
        settings.app.speed_unit = MeasurementUnit::MilesPerHour;
        let mut json = serde_json::to_value(&tune).unwrap();
        json["max_telemetry_vars"]["pitch"] = serde_json::json!(5000);
        let tune: Tune = serde_json::from_value(json).unwrap();
        let (after, settings_after) = reload_write_back(&tune, &settings, "none").unwrap();
        assert_eq!(after.motion_boost_factor, 1.0);
        assert_eq!(after.motion_reaction_speed, 70.0);
        assert_eq!(settings_after.motion.reaction_speed, 70);
        assert_eq!(after.motion_effects.pitch, 6.0);
        // 300 × 0.621371 truncates to 186; 186 × 1.60934 truncates to 299.
        assert_eq!(after.default_max_speed_kmh, 299.0);
        let json = serde_json::to_value(&after).unwrap();
        assert_eq!(json["max_telemetry_vars"]["pitch"], serde_json::json!(1800.0));
        // Every pass in mph goes through the two truncations again: 299 becomes 185, then 297.
        let again = reload_write_back(&after, &settings_after, "none").unwrap().0;
        assert_eq!(again.default_max_speed_kmh, 297.0);
        assert_eq!(Tune { default_max_speed_kmh: after.default_max_speed_kmh, ..again }, after);
    }

    /// Unless `nbr_fans` is the text 2, the reload switches wind curving off and sets the curving effect to 1; racing-only
    /// Motion rows are left alone for a flight game.
    #[test]
    fn reload_forced_and_skipped_rows() {
        let mut tune = standby_tune();
        tune.wind_curving = 1;
        tune.curving_effect = 7.0;
        tune.min_traction_loss_front_activation = 5000.0;
        let mut settings = Settings::default();
        settings.wind.fan_count = 1;
        let (after, _) = reload_write_back(&tune, &settings, "none").unwrap();
        assert_eq!((after.wind_curving, after.curving_effect), (0, 1.0));
        assert_eq!(after.min_traction_loss_front_activation, 1800.0, "the spin box keeps 0…1800");
        settings.wind.fan_count = 2;
        let (after, _) = reload_write_back(&tune, &settings, "dcs").unwrap();
        assert_eq!((after.wind_curving, after.curving_effect), (1, 7.0));
        assert_eq!(after.min_traction_loss_front_activation, 5000.0, "not a racing game: the row is not set");
    }

    /// A motion model change: without "2 TL" the front traction loss is zeroed and switched off; with it the
    /// switch goes on; under the Windows rules a model without IntelliComp selects the WITMotion sensor.
    #[test]
    fn motion_model_change_effects() {
        let mut tune = standby_tune();
        tune.min_traction_loss_front_activation = 40.0;
        tune.motion_main_actuators_traction_loss_front = 30.0;
        tune.motion_switch.traction_loss_front = true;
        let settings = Settings::default();
        let (after, settings_after) = motion_model_change(&tune, &settings, "H3", false).unwrap();
        assert_eq!(settings_after.motion.model, "H3");
        assert_eq!(after.min_traction_loss_front_activation, 0.0);
        assert_eq!(after.motion_main_actuators_traction_loss_front, 0.0);
        assert!(!after.motion_switch.traction_loss_front);
        let two_tl = openrigcue_ui::catalog::models().iter().find(|m| m.name.contains("2 TL")).unwrap();
        let (after, _) = motion_model_change(&after, &settings, &two_tl.name, false).unwrap();
        assert!(after.motion_switch.traction_loss_front);
        let without = openrigcue_ui::catalog::models().iter().find(|m| m.name != "None" && !m.enable_intellicomp);
        let without = without.unwrap();
        let mut virtual_sensor = Settings::default();
        virtual_sensor.compensation.sensor = openrigcue_config::settings::CompensationSensor::Virtual;
        let (_, on_linux) = motion_model_change(&tune, &virtual_sensor, &without.name, false).unwrap();
        assert_eq!(on_linux.compensation.sensor, openrigcue_config::settings::CompensationSensor::Virtual);
        let (_, on_windows) = motion_model_change(&tune, &virtual_sensor, &without.name, true).unwrap();
        assert_eq!(on_windows.compensation.sensor, openrigcue_config::settings::CompensationSensor::Witmotion);
    }

    /// The start-up part: the belt calibration sliders store the integer value within 0…100 (their handlers), the LED
    /// segments are limited to `nbr_leds` (the LED count's change handler).
    #[test]
    fn start_up_writes_settings_through_the_widgets() {
        let tune = standby_tune();
        let mut settings = Settings::default();
        settings.leds.nbr_leds = 20;
        settings.leds.segments[0].start = 5;
        settings.leds.segments[0].end = 60;
        let (_, after) = start_up(&tune, &settings, false).unwrap();
        assert_eq!((after.leds.segments[0].start, after.leds.segments[0].end), (5, 20));
        assert_eq!(after.leds.nbr_leds, 20);
        assert_eq!(start_up(&tune, &after, false).unwrap().1, after, "a second start changes nothing more");
    }

    /// Each of the four iFlag orientations (Left 90, Right 270, Down 0, Up 180) survives the start-up pass.
    #[test]
    fn start_up_keeps_every_iflag_orientation() {
        let tune = standby_tune();
        for degrees in [90, 270, 0, 180] {
            let mut settings = Settings::default();
            settings.iflag.rotation_degrees = degrees;
            let (_, after) = start_up(&tune, &settings, false).unwrap();
            assert_eq!(after.iflag.rotation_degrees, degrees);
        }
    }

    /// Every iteration: a motion test that is not allowed (no platform connected) stops with its sliders at 0; with
    /// the belt switch off the belt test stops and `belt_homing` returns to 0.
    #[test]
    fn live_belt_homing_off_reset_does_not_wait_for_device_io() {
        use std::sync::mpsc;
        use std::time::Duration;
        let tmp = tempfile::tempdir().unwrap();
        let ctx = context(&tmp);
        let mut belt = ctx.accessories.belt.lock().unwrap();
        belt.set_homing(crate::accessories::homing_of(1));
        let work = ctx.clone();
        let (send, receive) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            correct_test_controls(&work);
            send.send(()).unwrap();
        });
        let result = receive.recv_timeout(Duration::from_secs(2));
        drop(belt);
        worker.join().unwrap();
        result.expect("the belt-off global reset must not wait for belt I/O");
        assert_eq!(crate::accessories::homing_code(ctx.accessories.belt_homing()), 0);
    }

    #[test]
    fn test_controls_are_corrected() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = context(&tmp);
        {
            let mut controls = ctx.state.controls.write();
            controls.tests.motion_test = true;
            controls.tests.motion_sliders.heave = 40.0;
            controls.tests.belt_test = true;
        }
        ctx.accessories.belt.lock().unwrap().set_homing(crate::accessories::homing_of(1));
        tick(&ctx);
        let controls = ctx.state.controls.snapshot();
        assert!(!controls.tests.motion_test && !controls.tests.belt_test);
        assert_eq!(controls.tests.motion_sliders, openrigcue_cueing::TestSliders::default());
        assert_eq!(belt_homing(&ctx.accessories), 0);
    }

    /// The calibration check: when `belt_homing` becomes 3 the outcome is reported once it leaves 2/3.
    #[test]
    fn calibration_outcome_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = context(&tmp);
        ctx.state.session.write().game = Some("acc".into());
        tick(&ctx);
        ctx.accessories.belt.lock().unwrap().set_homing(crate::accessories::homing_of(3));
        tick(&ctx);
        tick(&ctx);
        std::thread::sleep(CALIBRATION_POLL * 2);
        assert!(ctx.state.notifications.read().items.iter().all(|n| n.title != "Success"));
        ctx.accessories.belt.lock().unwrap().set_homing(crate::accessories::homing_of(1));
        std::thread::sleep(CALIBRATION_POLL * 3);
        let shown: Vec<String> = ctx.state.notifications.read().items.iter().map(|n| n.message.clone()).collect();
        assert_eq!(shown.iter().filter(|m| *m == "Calibration has finished").count(), 1, "{shown:?}");
    }

    /// The Yaw VR notification's Center button: hidden, `motion_homing` only while connected, the alert always.
    #[test]
    fn yaw_vr_center_button() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = context(&tmp);
        ctx.state.notifications.write().set_hardware_widget(&HardwareWidget {
            motion_auto_search: true,
            motion_connected: true,
            axis_assignment_incomplete: false,
            yaw_vr: true,
            yaw_vr_centered: false,
            iflag_auto_search: false,
            iflag_connected: false,
            iflag_old_firmware: false,
            belt_auto_search: false,
            belt_connected: false,
            belt_needs_calibration: false,
        });
        let id = ctx.state.notifications.read().items[0].id;
        assert_eq!(confirm_notification(&ctx, id), Some(crate::notifications::Confirmed::CenterYawVr));
        assert!(!ctx.motion.driver_settings().yaw_shared.get().homing_requested, "not connected");
        let items = ctx.state.notifications.read().items.clone();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].message, "YawVR was told to center");
        assert_eq!(confirm_notification(&ctx, id), None);
    }

    /// The number at `path` of a tune value.
    fn number_at(value: &LegacyValue, path: &[&str]) -> f64 {
        path.iter().fold(value.clone(), |value, key| value.item_owned(key).unwrap()).py_float().unwrap()
    }

    /// A typed edit shows in every read of the tune while it lasts, and a read writes nothing: the stored value
    /// keeps what it held, so a typed field set back to its earlier value reads as that value again.
    #[test]
    fn reading_the_tune_writes_no_typed_edit_into_the_stored_value() {
        const FIELDS: [&[&str]; 3] =
            [&["power_wind_max_speed_perc"], &["gear_shift_perc"], &["motion_effects", "pitch"]];
        let store = ConfigStore::in_memory(Configuration::first_run());
        let mut profile = ActiveProfile::standby(&store);
        let loaded =
            [profile.tune.power_wind_max_speed_perc, profile.tune.gear_shift_perc, profile.tune.motion_effects.pitch];
        let set = |profile: &mut ActiveProfile, [wind, shift, pitch]: [f64; 3]| {
            profile.tune.power_wind_max_speed_perc = wind;
            profile.tune.gear_shift_perc = shift;
            profile.tune.motion_effects.pitch = pitch;
        };
        let stored = |profile: &ActiveProfile| FIELDS.map(|path| number_at(&profile.native, path));
        let read = |profile: &ActiveProfile| {
            let tune = profile.native_tune();
            let json = profile.tune_json().unwrap();
            assert_eq!(json["power_wind_max_speed_perc"].as_f64(), Some(number_at(&tune, FIELDS[0])));
            let (_, _) = (profile.native_simtech_tune(), profile.native_thanos_tune());
            assert_eq!(
                number_at(&profile.native_telemetry_tune(), FIELDS[1]),
                number_at(&tune, FIELDS[1]),
                "the telemetry fields and the whole tune agree"
            );
            FIELDS.map(|path| number_at(&tune, path))
        };
        assert_eq!(stored(&profile), loaded);
        assert_eq!(read(&profile), loaded);
        let edited = loaded.map(|value| value - 5.0);
        set(&mut profile, edited);
        assert_eq!(read(&profile), edited, "a typed edit is read");
        assert_eq!(read(&profile), edited, "and read again");
        assert_eq!(stored(&profile), loaded, "reading wrote into the stored value");
        set(&mut profile, loaded);
        assert_eq!(read(&profile), loaded, "the typed fields are back at their loaded values");
        assert_eq!(stored(&profile), loaded);
    }

    /// An edit made in place on the value `native_tune` hands out reaches the typed fields through
    /// `replace_native_tune`, also while another stored field keeps the whole tune from converting.
    #[test]
    fn in_place_edits_of_the_read_tune_reach_the_typed_fields() {
        let store = ConfigStore::in_memory(Configuration::first_run());
        let mut profile = ActiveProfile::standby(&store);
        let edit = |profile: &mut ActiveProfile, path: &[&str], value: LegacyValue| {
            let mut native = profile.native_tune();
            native.assign_path(path, value).unwrap();
            profile.replace_native_tune(native);
        };
        let typed = |profile: &ActiveProfile| (profile.tune.gear_shift_perc, profile.tune.motion_effects.pitch);
        edit(&mut profile, &["gear_shift_perc"], LegacyValue::Int(91));
        edit(&mut profile, &["motion_effects", "pitch"], LegacyValue::Int(7));
        assert_eq!(typed(&profile), (91.0, 7.0));
        edit(&mut profile, &["default_max_speed_kmh"], LegacyValue::from("bad"));
        assert!(Tune::from_legacy(&profile.native_tune()).is_err(), "the stored tune no longer converts");
        assert_eq!(typed(&profile), (91.0, 7.0));
        edit(&mut profile, &["gear_shift_perc"], LegacyValue::Int(92));
        assert_eq!(typed(&profile), (92.0, 7.0), "a top-level field");
        edit(&mut profile, &["motion_effects", "pitch"], LegacyValue::Int(8));
        assert_eq!(typed(&profile), (92.0, 8.0), "a field of a nested dictionary");
        assert_eq!(number_at(&profile.native_tune(), &["gear_shift_perc"]), 92.0);
        assert_eq!(number_at(&profile.native_tune(), &["motion_effects", "pitch"]), 8.0);
        // With a typed edit not yet stored, the read tune is a copy that still shares its unedited dictionaries.
        let wind = profile.tune.power_wind_max_speed_perc - 5.0;
        profile.tune.power_wind_max_speed_perc = wind;
        edit(&mut profile, &["motion_effects", "pitch"], LegacyValue::Int(9));
        assert_eq!(typed(&profile), (92.0, 9.0), "a nested field beside a typed edit");
        assert_eq!(profile.tune.power_wind_max_speed_perc, wind, "the typed edit stays");
        assert_eq!(number_at(&profile.native, &["power_wind_max_speed_perc"]), wind, "and is stored with the edit");
        assert_eq!(profile.native_tune().item_owned("default_max_speed_kmh"), Ok(LegacyValue::from("bad")));
    }
}
