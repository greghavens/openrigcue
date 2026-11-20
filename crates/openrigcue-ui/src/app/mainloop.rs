//! The UI main loop (its own thread): the start (launcher import, start-up rules, first installation), then
//! every pass the hot keys and hardware rules, the profile reload, the remembered location, the hardware
//! status, the belt calibration check, the notifications and the interface's live regions, followed by a
//! 0.5 s pause.

use openrigcue_model::{native_value::LegacyValue, python_compare::native_equal};
use serde_json::Value;

use std::sync::{Mutex, Weak};

use super::{MyApp, MAIN_LOOP_PERIOD};
use crate::backend::{at, text, truthy, BackendExt, HardwareNotification};
use crate::host::lock;

/// What the main loop keeps between iterations.
#[derive(Default)]
pub struct LoopState {
    pub started: bool,
    pub fatal_error: Option<String>,
    /// The previous location; `None` stands for the number zero it has at the start and after a reset.
    pub previous_location: Option<LegacyValue>,
    /// The profile load the interface last took (the service numbers its profile loads).
    pub loaded_generation: Option<u64>,
    /// The profile in use is the vehicle's own: taken at each profile load, and set once the profile is saved for
    /// the vehicle. Until then the game's profile is in use (the vehicle has no saved profile).
    pub vehicle_level: bool,
    /// The Intellibox and belt status of the previous iteration.
    pub previous_intellibox_status: Option<LegacyValue>,
    pub previous_belt_status: Option<LegacyValue>,
    /// The belt homing state of the previous iteration.
    pub previous_belt_homing: Option<LegacyValue>,
    /// What the hardware status rules last read; they run again only when it changes.
    pub previous_hardware_snapshot: Option<LegacyValue>,
    /// Each device's status colour and its explanation, as the hardware status last found them.
    pub hardware_states: std::collections::BTreeMap<String, (String, String)>,
}

/// The main loop thread, with the UI update lock around only the conditional reload.
pub fn main_loop(app: Weak<Mutex<MyApp>>) {
    loop {
        let Some(shared) = app.upgrade() else { return };
        if !iteration_shared(&shared) {
            return;
        }
        drop(shared);
        std::thread::sleep(MAIN_LOOP_PERIOD);
    }
}

/// One isolated pass; acquiring update_lock never retains the application-state mutex.
pub(super) fn iteration_shared(shared: &Mutex<MyApp>) -> bool {
    let (backend, update_lock) = {
        let app = lock(shared);
        (app.backend.clone(), app.update_lock.clone())
    };
    let docs = super::read_docs(&*backend);
    let failure = {
        let mut app = lock(shared);
        app.apply_docs(docs);

        app.ml.fatal_error.clone()
    };
    if report_failure(failure) {
        return false;
    }
    let begin = {
        let mut app = lock(shared);
        if app.ml.started {
            false
        } else {
            app.ml.started = true;
            true
        }
    };
    if begin {
        if let Err(error) = start_shared(shared) {
            return shared_failure(shared, error);
        }
    }
    if let Err(error) = before_reload_rules_shared(shared) {
        return shared_failure(shared, error);
    }
    // The reload decision and the profile load use service state, not GUI state.
    // Keep the explicit UI update lock around the latter without blocking independent
    // Timer callbacks on Rust's application mutex while profile I/O is in flight.
    let reload = match backend.profile_reload_needed() {
        Ok(Some(reload)) => reload.then_some(Reload::Current),
        Ok(None) => {
            if let Err(error) = backend.synchronize_profile() {
                return shared_failure(shared, error.to_string());
            }
            let docs = super::read_docs(&*backend);
            let mut app = lock(shared);
            app.apply_docs(docs);
            loaded_reload(&app)
        }
        Err(error) => return shared_failure(shared, error.detail),
    };
    if let Some(reload) = reload {
        let failure = {
            let _update_guard = update_lock.lock();
            if matches!(reload, Reload::Current) {
                if let Err(error) = backend.load_current_profile() {
                    return shared_failure(shared, error.detail);
                }
            }
            let docs = matches!(reload, Reload::Current).then(|| super::read_docs(&*backend));
            let mut app = lock(shared);
            if let Some(docs) = docs {
                app.apply_docs(docs);
            }
            take_profile_load(&mut app);
            app.ml.fatal_error.clone()
        };
        if report_failure(failure) {
            return false;
        }
    }
    let failure = after_reload_rules_shared(shared);
    if report_failure(failure) {
        return false;
    }
    if let Err(error) = refresh_hardware_status_shared(shared) {
        return shared_failure(shared, error);
    }
    // These steps only use service state. Port-owner lookup and
    // notification persistence must not retain the GUI's state mutex.
    if let Err(error) = finish_service_pass(&*backend) {
        return shared_failure(shared, error);
    }
    crate::ui::notices::synchronize_shared(shared);
    if let Err(error) = finish_parameters_shared(shared) {
        return shared_failure(shared, error);
    }
    crate::ui::tick_shared(shared);
    !report_failure(lock(shared).ml.fatal_error.clone())
}

fn shared_failure(shared: &Mutex<MyApp>, error: String) -> bool {
    lock(shared).ml.fatal_error = Some(error.clone());
    report_failure(Some(error));
    false
}

fn report_failure(error: Option<String>) -> bool {
    if let Some(error) = error {
        tracing::error!(%error, "Exception in thread main_loop_ui");
        true
    } else {
        false
    }
}

/// The start of `main_loop` before its loop: the launcher's import (`--profile_path`), the start-up rules, the
/// start page, and Hardware on the first installation.
fn start(app: &mut MyApp) -> Result<(), String> {
    if !app.status(&["arguments", "profile_path"]).is_null() {
        crate::ui::profiles::offer_launcher_import(app, "new_instance")?;
    }
    crate::ui::screens::hardware::main_loop_start(app)?;
    crate::ui::screens::reload::start(app)?;
    crate::ui::screens::app::open_startup_page(app);
    let start_info = app.call(app.backend.post("start-info", Value::Null)).unwrap_or(Value::Null);
    show_reset_notice(app, &start_info);
    if truthy(at(&start_info, &["first_installation"])) && app.win32() {
        app.goto_page_checked("Setup", "Hardware")?;
    }
    Ok(())
}

/// When the service reset the stored configuration files at this start, a window with the service's title
/// and text tells the user.
fn show_reset_notice(app: &mut MyApp, start_info: &Value) {
    let notice = at(start_info, &["reset"]);
    if notice.is_null() {
        return;
    }
    let message = app.backend.translate(&text(at(notice, &["text"])));
    crate::ui::notices::warning(app, &text(at(notice, &["title"])), &message);
}

fn start_shared(shared: &Mutex<MyApp>) -> Result<(), String> {
    let import = !lock(shared).status(&["arguments", "profile_path"]).is_null();
    if import {
        crate::ui::profiles::offer_launcher_import_shared(shared, "new_instance")?;
    }
    crate::ui::screens::hardware::main_loop_start_shared(shared)?;
    crate::ui::screens::reload::start(&mut lock(shared))?;
    crate::ui::screens::app::open_startup_page(&mut lock(shared));
    let backend = lock(shared).backend.clone();
    let result = backend.post("start-info", Value::Null);
    let start_info = lock(shared).call(result).unwrap_or(Value::Null);
    show_reset_notice(&mut lock(shared), &start_info);
    if truthy(at(&start_info, &["first_installation"])) && lock(shared).win32() {
        lock(shared).goto_page_checked("Setup", "Hardware")?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Reload {
    Current,
    Loaded,
}

/// One pass on a caller-owned app, preserving the same conditional update-lock boundary.
pub fn iteration(app: &mut MyApp) {
    let reload = before_reload(app);
    if app.ml.fatal_error.is_some() {
        return;
    }
    if let Some(reload) = reload {
        let update_lock = app.update_lock.clone();
        let _guard = update_lock.lock();
        perform_reload(app, reload);
    }
    if app.ml.fatal_error.is_none() {
        after_reload(app);
    }
}

fn before_reload(app: &mut MyApp) -> Option<Reload> {
    before_reload_rules(app);
    if app.ml.fatal_error.is_some() {
        return None;
    }
    match app.backend.profile_reload_needed() {
        Ok(Some(reload)) => return reload.then_some(Reload::Current),
        Ok(None) => {}
        Err(error) => {
            app.ml.fatal_error = Some(error.detail);
            return None;
        }
    }
    if let Err(error) = app.backend.synchronize_profile() {
        app.ml.fatal_error = Some(error.to_string());
        return None;
    }
    app.refresh_docs();
    loaded_reload(app)
}

fn before_reload_start(app: &mut MyApp) {
    if app.ml.fatal_error.is_some() {
        return;
    }
    if !app.ml.started {
        app.ml.started = true;
        if let Err(error) = start(app) {
            app.ml.fatal_error = Some(error);
        }
    }
}

fn before_reload_rules(app: &mut MyApp) {
    before_reload_start(app);
    if app.ml.fatal_error.is_some() {
        return;
    }
    if let Err(error) = apply_ui_actions(app) {
        app.ml.fatal_error = Some(error);
        return;
    }
    if let Err(error) = crate::ui::screens::hardware::main_loop_rules(app) {
        app.ml.fatal_error = Some(error);
        return;
    }
    app.backend.clear_ui_actions();
}

fn loaded_reload(app: &MyApp) -> Option<Reload> {
    let generation = app
        .status(&["profile", "load_generation"])
        .as_u64()
        .or_else(|| app.status(&["profile", "generation"]).as_u64());
    (generation != app.ml.loaded_generation).then_some(Reload::Loaded)
}

fn perform_reload(app: &mut MyApp, reload: Reload) {
    if matches!(reload, Reload::Current) {
        if let Err(error) = app.backend.load_current_profile() {
            app.ml.fatal_error = Some(error.detail);
            return;
        }
        app.refresh_docs();
    }
    take_profile_load(app);
}

/// The service loaded a profile: its generation and whether it is the vehicle's own are recorded and the
/// profile-load rules of the screens run.
fn take_profile_load(app: &mut MyApp) {
    app.ml.loaded_generation = app
        .status(&["profile", "load_generation"])
        .as_u64()
        .or_else(|| app.status(&["profile", "generation"]).as_u64());
    app.ml.vehicle_level = truthy(app.status(&["profile", "vehicle_level"]));
    crate::ui::screens::tuning::on_profile_load(app);
}

fn after_reload(app: &mut MyApp) {
    after_reload_rules(app);
    if app.ml.fatal_error.is_none() {
        if let Err(error) = refresh_hardware_status(app) {
            app.ml.fatal_error = Some(error);
        }
    }
    if app.ml.fatal_error.is_some() {
        return;
    }
    if let Err(error) = finish_service_pass(&*app.backend) {
        app.ml.fatal_error = Some(error);
        return;
    }
    after_service_pass(app);
}

/// After the reload: the remembered location, then the tuning screens' per-pass checks.
fn after_reload_rules(app: &mut MyApp) {
    if let Err(error) = app.remember_location() {
        app.ml.fatal_error = Some(error);
        return;
    }
    crate::ui::screens::tuning::tick(app);
}

fn after_reload_rules_shared(shared: &Mutex<MyApp>) -> Option<String> {
    if let Err(error) = remember_location_shared(shared) {
        lock(shared).ml.fatal_error = Some(error.clone());
        return Some(error);
    }
    crate::ui::screens::tuning::tick_shared(shared);
    lock(shared).ml.fatal_error.clone()
}

fn finish_service_pass(backend: &dyn crate::backend::Backend) -> Result<(), String> {
    backend.remember_profile_inputs().map_err(|error| error.detail)?;
    backend.finish_main_loop().map_err(|error| error.to_string())
}

fn after_service_pass(app: &mut MyApp) {
    crate::ui::notices::synchronize(app);
    if let Err(error) = finish_parameters(app) {
        app.ml.fatal_error = Some(error);
    }
    crate::ui::tick(app);
}

pub(super) fn finish_parameters(app: &mut MyApp) -> Result<(), String> {
    // `belt_homing` reaching 3 starts the calibration check on its own thread.
    let belt_homing = app.backend.control_value(&["tests", "belt_homing"]).map_err(|error| error.detail)?;
    if native_equal(&belt_homing, &LegacyValue::Int(3))
        && !app.ml.previous_belt_homing.as_ref().is_some_and(|value| native_equal(value, &LegacyValue::Int(3)))
    {
        crate::ui::screens::belt::start_check_calibration(app);
    }
    app.ml.previous_belt_homing =
        Some(app.backend.control_value(&["tests", "belt_homing"]).map_err(|error| error.detail)?);
    let intellibox_status = app.backend.accessory_status("intellibox").map_err(|error| error.detail)?;
    if !native_equal(&intellibox_status, &LegacyValue::Str("Not Connected".into())) {
        app.ml.previous_intellibox_status =
            Some(app.backend.accessory_status("intellibox").map_err(|error| error.detail)?);
    }
    let belt_status = app.backend.accessory_status("belt").map_err(|error| error.detail)?;
    if !native_equal(&belt_status, &LegacyValue::Str("Not Connected".into())) {
        app.ml.previous_belt_status = Some(app.backend.accessory_status("belt").map_err(|error| error.detail)?);
    }
    Ok(())
}

pub(super) fn finish_parameters_shared(shared: &Mutex<MyApp>) -> Result<(), String> {
    let backend = lock(shared).backend.clone();
    let homing = backend.control_value(&["tests", "belt_homing"]).map_err(|error| error.detail)?;
    {
        let mut app = lock(shared);
        if native_equal(&homing, &LegacyValue::Int(3))
            && !app.ml.previous_belt_homing.as_ref().is_some_and(|value| native_equal(value, &LegacyValue::Int(3)))
        {
            crate::ui::screens::belt::start_check_calibration(&mut app);
        }
    }
    let homing = backend.control_value(&["tests", "belt_homing"]).map_err(|error| error.detail)?;
    lock(shared).ml.previous_belt_homing = Some(homing);
    let intellibox = backend.accessory_status("intellibox").map_err(|error| error.detail)?;
    if !native_equal(&intellibox, &LegacyValue::Str("Not Connected".into())) {
        let current = backend.accessory_status("intellibox").map_err(|error| error.detail)?;
        lock(shared).ml.previous_intellibox_status = Some(current);
    }
    let belt = backend.accessory_status("belt").map_err(|error| error.detail)?;
    if !native_equal(&belt, &LegacyValue::Str("Not Connected".into())) {
        let current = backend.accessory_status("belt").map_err(|error| error.detail)?;
        lock(shared).ml.previous_belt_status = Some(current);
    }
    Ok(())
}

pub(crate) fn remember_location_shared(shared: &Mutex<MyApp>) -> Result<(), String> {
    let backend = lock(shared).backend.clone();
    backend.remember_actual_vehicle().map_err(|error| error.detail)?;
    if !native_equal(&backend.location_value(), &LegacyValue::Int(0)) {
        let location = backend.location_value();
        lock(shared).ml.previous_location = Some(location);
    }
    let game = backend.game_value().map_err(|error| error.detail)?;
    if native_equal(&game, &LegacyValue::Str("none".into())) {
        backend.clear_remembered_vehicle().map_err(|error| error.detail)?;
        lock(shared).ml.previous_location = None;
    }
    Ok(())
}

impl MyApp {
    pub(crate) fn remember_location(&mut self) -> Result<(), String> {
        self.backend.remember_actual_vehicle().map_err(|error| error.detail)?;
        if !native_equal(&self.backend.location_value(), &LegacyValue::Int(0)) {
            self.ml.previous_location = Some(self.backend.location_value());
        }
        let game = self.backend.game_value().map_err(|error| error.detail)?;
        if native_equal(&game, &LegacyValue::Str("none".into())) {
            self.backend.clear_remembered_vehicle().map_err(|error| error.detail)?;
            self.ml.previous_location = None;
        }
        Ok(())
    }
}

/// The queued hot keys step the tuning values while a game runs: Shake, Motion, then Belt, SIM3D and HapticR.
/// A failing write is an error before later actions and before clearing the queue.
pub(super) fn apply_ui_actions(app: &mut MyApp) -> Result<(), String> {
    if native_equal(&app.backend.game_value().map_err(|error| error.detail)?, &LegacyValue::Str("none".into())) {
        return Ok(());
    }
    let backend = app.backend.clone();
    let pending = |action: &str| backend.ui_action_pending(action);
    crate::ui::screens::tuning::apply_shake_hotkeys(app, &pending)?;
    crate::ui::screens::motion::apply_hotkeys(app, &pending)?;
    crate::ui::screens::tuning::apply_hotkeys(app, &pending)
}

fn hardware_config_snapshot(backend: &dyn crate::backend::Backend) -> Result<LegacyValue, String> {
    let mut sections = Vec::new();
    for (section, keys) in [
        ("APP", &["enable_hapticr"][..]),
        ("INTELLIBOX", &["auto_search", "com_port"][..]),
        ("MOTIONBOX", &["auto_search", "model"][..]),
        ("SHAKEBOX", &["soundcard"][..]),
        ("MOTION_COMPENSATION", &["type", "auto_search", "com_port"][..]),
        ("IFLAG", &["auto_search", "com_port"][..]),
        ("SIM3D", &["auto_search", "com_port", "model", "port1", "port2", "port3", "port4"][..]),
        ("HAPTICR", &["auto_search", "com_port"][..]),
        ("HUD", &["auto_search", "model"][..]),
        ("BELT_TENSIONER", &["auto_search", "com_port"][..]),
        ("VNM SIMULATION", &["vnm_com_port"][..]),
        ("PROSIMU", &["prs200_com_port1"][..]),
    ] {
        let values = backend.native_config_section(section, keys).map_err(|error| error.detail)?;
        let LegacyValue::Dict(values) = values else {
            return Err(format!("AttributeError: '{}' object has no attribute 'get'", values.type_name()));
        };
        let fields = keys
            .iter()
            .map(|key| {
                LegacyValue::Tuple(vec![(*key).into(), snapshot_value(values.get(key).unwrap_or(&LegacyValue::None))])
            })
            .collect();
        sections.push(LegacyValue::Tuple(vec![section.into(), LegacyValue::Tuple(fields)]));
    }
    Ok(LegacyValue::Tuple(sections))
}

fn hardware_parameter_snapshot(backend: &dyn crate::backend::Backend) -> Result<LegacyValue, String> {
    let mut values = Vec::new();
    for key in [
        "power_wind_status",
        "power_wind_center_status",
        "led_status",
        "mc_switch",
        "belt_switch",
        "hud_switch",
        "iflag_switch",
        "sim3d_switch",
        "haptic_switch",
        "motion_switch",
        "shake_box_status",
        "intellibox_status",
        "intellibox_model",
        "intellibox_version",
        "intellibox_com_port",
        "motion_status",
        "motion_com_port",
        "motion_warning",
        "motion_error",
        "axis_assignment_status",
        "mc_status",
        "mc_com_port",
        "iflag_status",
        "iflag_version",
        "iflag_dismiss_firmware_notification",
        "sim3d_status",
        "hapticr_status",
        "hud_status",
        "belt_status",
        "belt_model",
        "belt_version",
        "belt_com_port",
        "belt_sn",
        "belt_homing",
        "belt_dismiss_center_notification",
        "Yaw VR",
        "yawvr_dismiss_center_notification",
        "game_running",
        "listener_running",
    ] {
        let value = backend.hardware_parameter(key).map_err(|error| error.detail)?;
        values.push(LegacyValue::Tuple(vec![key.into(), snapshot_value(&value)]));
    }
    Ok(LegacyValue::Tuple(values))
}

/// Everything [`hardware_status`] reads, as one value to compare between passes.
fn hardware_snapshot(app: &MyApp) -> Result<LegacyValue, String> {
    let model = app.backend.native_setting("MOTIONBOX", "model").map_err(|error| error.detail)?;
    let model_definition = if let LegacyValue::Str(model) = model {
        openrigcue_model::motion_models::motion_model(&model)
            .map(|model| snapshot_value(&LegacyValue::from(serde_json::json!(model.fields))))
            .unwrap_or(LegacyValue::None)
    } else {
        LegacyValue::None
    };
    let parameters = hardware_parameter_snapshot(&*app.backend)?;
    let config = hardware_config_snapshot(&*app.backend)?;
    let notifications = app.backend.hardware_notification_state();
    Ok(LegacyValue::Tuple(vec![
        parameters,
        config,
        LegacyValue::Tuple(vec!["motion_model_definition".into(), model_definition]),
        LegacyValue::Tuple(vec![
            "previous_intellibox_status".into(),
            app.ml.previous_intellibox_status.clone().unwrap_or(LegacyValue::None),
        ]),
        LegacyValue::Tuple(vec![
            "previous_belt_status".into(),
            app.ml.previous_belt_status.clone().unwrap_or(LegacyValue::None),
        ]),
        LegacyValue::Tuple(vec!["showing_belt_notification".into(), notifications.showing_belt.into()]),
        LegacyValue::Tuple(vec!["showing_iflag_notification".into(), notifications.showing_iflag.into()]),
    ]))
}

/// Runs the hardware status rules when what they read changed since the last run.
pub(super) fn refresh_hardware_status(app: &mut MyApp) -> Result<(), String> {
    let snapshot = hardware_snapshot(app)?;
    if app.ml.previous_hardware_snapshot.as_ref().is_none_or(|previous| !native_equal(previous, &snapshot)) {
        hardware_status(app)?;
        app.ml.previous_hardware_snapshot = Some(hardware_snapshot(app)?);
    }
    Ok(())
}

fn hardware_snapshot_shared(shared: &Mutex<MyApp>) -> Result<LegacyValue, String> {
    let backend = lock(shared).backend.clone();
    let model = backend.native_setting("MOTIONBOX", "model").map_err(|error| error.detail)?;
    let model_definition = if let LegacyValue::Str(model) = model {
        openrigcue_model::motion_models::motion_model(&model)
            .map(|model| snapshot_value(&LegacyValue::from(serde_json::json!(model.fields))))
            .unwrap_or(LegacyValue::None)
    } else {
        LegacyValue::None
    };
    let parameters = hardware_parameter_snapshot(&*backend)?;
    let config = hardware_config_snapshot(&*backend)?;
    let notifications = backend.hardware_notification_state();
    let app = lock(shared);
    Ok(LegacyValue::Tuple(vec![
        parameters,
        config,
        LegacyValue::Tuple(vec!["motion_model_definition".into(), model_definition]),
        LegacyValue::Tuple(vec![
            "previous_intellibox_status".into(),
            app.ml.previous_intellibox_status.clone().unwrap_or(LegacyValue::None),
        ]),
        LegacyValue::Tuple(vec![
            "previous_belt_status".into(),
            app.ml.previous_belt_status.clone().unwrap_or(LegacyValue::None),
        ]),
        LegacyValue::Tuple(vec!["showing_belt_notification".into(), notifications.showing_belt.into()]),
        LegacyValue::Tuple(vec!["showing_iflag_notification".into(), notifications.showing_iflag.into()]),
    ]))
}

pub(super) fn refresh_hardware_status_shared(shared: &Mutex<MyApp>) -> Result<(), String> {
    let snapshot = hardware_snapshot_shared(shared)?;
    let changed = lock(shared).ml.previous_hardware_snapshot.as_ref().is_none_or(|old| !native_equal(old, &snapshot));
    if changed {
        hardware_status_shared(shared)?;
        let after = hardware_snapshot_shared(shared)?;
        lock(shared).ml.previous_hardware_snapshot = Some(after);
    }
    Ok(())
}

fn snapshot_value(value: &LegacyValue) -> LegacyValue {
    match value {
        LegacyValue::Dict(values) => {
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort_by_cached_key(|key| key.to_text());
            LegacyValue::Tuple(
                keys.into_iter()
                    .map(|key| {
                        LegacyValue::Tuple(vec![
                            key.to_value(),
                            snapshot_value(values.get_key(key).expect("snapshot key exists")),
                        ])
                    })
                    .collect(),
            )
        }
        LegacyValue::List(values) | LegacyValue::Tuple(values) => {
            LegacyValue::Tuple(values.iter().map(snapshot_value).collect())
        }
        LegacyValue::Set(values) => {
            let mut values = values.iter().map(snapshot_value).collect::<Vec<_>>();
            values.sort_by_cached_key(openrigcue_model::python_repr::str_value);
            LegacyValue::Tuple(values)
        }
        value => value.clone(),
    }
}

enum RenderUi<'a> {
    Borrowed(std::cell::RefCell<&'a mut MyApp>),
    Shared(&'a Mutex<MyApp>),
}
impl RenderUi<'_> {
    fn with<T>(&self, operation: impl FnOnce(&mut MyApp) -> T) -> T {
        match self {
            Self::Borrowed(app) => operation(&mut app.borrow_mut()),
            Self::Shared(shared) => operation(&mut lock(shared)),
        }
    }
    fn notifications(&self) {
        match self {
            Self::Borrowed(app) => crate::ui::notices::synchronize(&mut app.borrow_mut()),
            Self::Shared(shared) => crate::ui::notices::synchronize_shared(shared),
        }
    }
}
fn hardware_notification(
    ui: &RenderUi<'_>,
    backend: &dyn crate::backend::Backend,
    notification: HardwareNotification,
) -> Result<(), String> {
    backend.hardware_notification(notification).map_err(|error| error.detail)?;
    ui.notifications();
    Ok(())
}

fn auto_search(backend: &dyn crate::backend::Backend, section: &str) -> Result<bool, String> {
    let value = backend.native_setting(section, "auto_search").map_err(|error| error.detail)?;
    crate::backend::native_bool(&value)
}

fn append_native_text(mut prefix: String, value: LegacyValue) -> Result<String, String> {
    match value {
        LegacyValue::Str(value) => {
            prefix.push_str(&value);
            Ok(prefix)
        }
        value => Err(format!("TypeError: can only concatenate str (not \"{}\") to str", value.type_name())),
    }
}

fn hardware_value(backend: &dyn crate::backend::Backend, name: &str) -> Result<LegacyValue, String> {
    backend.hardware_parameter(name).map_err(|error| error.detail)
}

fn hardware_is(backend: &dyn crate::backend::Backend, name: &str, expected: &str) -> Result<bool, String> {
    Ok(native_equal(&hardware_value(backend, name)?, &LegacyValue::Str(expected.into())))
}

fn hardware_text(backend: &dyn crate::backend::Backend, name: &str) -> Result<String, String> {
    Ok(openrigcue_model::python_repr::str_value(&hardware_value(backend, name)?))
}

/// The hardware status rules: every device's status colour and explanation (recorded in
/// `ml.hardware_states` for the rig panel), and the device notifications they raise.
pub fn hardware_status(app: &mut MyApp) -> Result<(), String> {
    let backend = app.backend.clone();
    hardware_status_rules(&RenderUi::Borrowed(std::cell::RefCell::new(app)), &*backend)
}
pub(super) fn hardware_status_shared(shared: &Mutex<MyApp>) -> Result<(), String> {
    let backend = lock(shared).backend.clone();
    hardware_status_rules(&RenderUi::Shared(shared), &*backend)
}
fn hardware_status_rules(ui: &RenderUi<'_>, backend: &dyn crate::backend::Backend) -> Result<(), String> {
    let t = |s: &str| backend.translate(s);
    let intellibox_tooltip = t("No connection. Check that the USB cable is plugged in");
    let model = crate::catalog::current_model(backend)?;
    let yaw = model.manufacturer == "Yaw VR";
    let not_connected = t(if yaw {
        "No connection. Check that the Yaw VR is powered on and joined to the network"
    } else {
        "No connection. Check that the USB cable is plugged in and the motion platform is powered on."
    });
    let initial_tooltips = std::collections::BTreeMap::from([
        ("Intellibox", intellibox_tooltip),
        ("Motionbox", not_connected.clone()),
        ("Shakebox", t("No connection. Choose the sound device on the Setup -> Hardware page")),
        ("MCMode", t("No connection. Make sure the USB cable is attached.")),
        ("Belt", t("No connection. Make sure the USB cable is attached.")),
        ("Hud", t("No connection. Check that the device has power.")),
        ("iFlag", t("No connection. Check that the device has power.")),
        ("SIM3D", t("No connection. Check that the device has power.")),
        ("HapticR", t("No connection. Check that the device has power.")),
    ]);
    let tooltips = std::cell::RefCell::new(initial_tooltips.clone());
    fn hapticr_enabled(backend: &dyn crate::backend::Backend) -> Result<bool, String> {
        let value = backend.native_setting("APP", "enable_hapticr").map_err(|error| error.detail)?;
        crate::backend::native_bool(&value)
    }
    // Each device's colour and explanation as the rules below find them.
    let colors = std::cell::RefCell::new(std::collections::BTreeMap::<&'static str, String>::new());
    let set = |_: &RenderUi<'_>, key: &'static str, color: &str, tooltip: String| -> Result<(), String> {
        colors.borrow_mut().insert(key, color.to_owned());
        tooltips.borrow_mut().insert(key, tooltip);
        Ok(())
    };
    let set_translated = |ui: &RenderUi<'_>, key: &'static str, color: &str, message: &str| -> Result<(), String> {
        set(ui, key, color, t(message))
    };
    let pinned = |tooltip: &str, device: &str, section: &str| -> Result<String, String> {
        // One sentence after the device's own tooltip; its `^device^` and `^port^` parts are filled in afterwards
        // (a device name that has a translation is looked up).
        let sentence = t(" . Auto search is switched off, so the only port OpenRigCue checks for the ^device^ is ^port^. This can be changed on the Setup -> Hardware page.")
            .replace("^device^", &t(device));
        let port = backend.native_setting(section, "com_port").map_err(|error| error.detail)?;
        let port = append_native_text(String::new(), port)?;
        Ok(format!("{tooltip}{}", sentence.replace("^port^", &port)))
    };

    // Intellibox.
    if !auto_search(backend, "INTELLIBOX")? {
        set_translated(ui, "Intellibox", "orange", "Intellibox is switched off on the Setup -> Hardware page")?;
    } else if hardware_is(backend, "intellibox_status", "Connected")? {
        set(ui, "Intellibox", "green", initial_tooltips["Intellibox"].clone())?;
        // One translated line whose `^name^` parts are filled in afterwards.
        let tip = t("Model: ^model^ - Version: ^version^ - Port: ^port^")
            .replace("^model^", &hardware_text(backend, "intellibox_model")?)
            .replace("^version^", &hardware_text(backend, "intellibox_version")?)
            .replace("^port^", &hardware_text(backend, "intellibox_com_port")?);
        tooltips.borrow_mut().insert("Intellibox", tip);
    } else if hardware_is(backend, "intellibox_status", "Not Connected")?
        && !ui.with(|app| {
            app.ml
                .previous_intellibox_status
                .as_ref()
                .is_some_and(|value| native_equal(value, &LegacyValue::Str("Reconnecting".into())))
        })
    {
        let tooltip = initial_tooltips["Intellibox"].clone();
        let port = backend.native_setting("INTELLIBOX", "com_port").map_err(|error| error.detail)?;
        if native_equal(&port, &LegacyValue::Str(String::new())) {
            set(ui, "Intellibox", "red", tooltip)?;
        } else {
            set(ui, "Intellibox", "red", tooltip.clone())?;
            let tip = pinned(&tooltip, "Intellibox", "INTELLIBOX")?;
            tooltips.borrow_mut().insert("Intellibox", tip);
        }
    } else {
        set(ui, "Intellibox", "pink", initial_tooltips["Intellibox"].clone())?;
        let tip = t(&hardware_text(backend, "intellibox_status")?);
        tooltips.borrow_mut().insert("Intellibox", tip);
    }

    // Motionbox.
    if !auto_search(backend, "MOTIONBOX")? {
        set_translated(ui, "Motionbox", "orange", "Motion is switched off on the Setup -> Hardware page")?;
    } else if hardware_is(backend, "motion_status", "Connected")? {
        // One translated sentence whose `^ports^` part, the list of ports, is filled in afterwards.
        let connected_to = t(" Motion platform attached to: ^ports^");
        let ports = hardware_value(backend, "motion_com_port")?;
        let LegacyValue::Dict(ports) = ports else {
            return Err(format!("AttributeError: '{}' object has no attribute 'items'", ports.type_name()));
        };
        let mut port_list = String::new();
        for (_, port) in ports.iter() {
            port_list.push_str(&openrigcue_model::python_repr::str_value(port));
            port_list.push_str(", ");
        }
        let connected_to = connected_to.replace("^ports^", &port_list);
        if matches!(hardware_value(backend, "axis_assignment_status")?, LegacyValue::Bool(false))
            && crate::catalog::current_model(backend)?.axis_assignment
        {
            set(ui, "Motionbox", "yellow", connected_to.clone())?;
            let tip = t(" The actuator assignment is wrong or unfinished; review it under Setup -> Hardware -> Motion Model -> Gear Button.") + &connected_to;
            tooltips.borrow_mut().insert("Motionbox", tip);
        } else {
            let yaw = hardware_value(backend, "Yaw VR")?;
            let centered = &crate::backend::native_field_owned(&yaw, &["centered"])?;
            let needs_center = matches!(centered, LegacyValue::Bool(false))
                && crate::catalog::current_model(backend)?.manufacturer == "Yaw VR";
            if needs_center {
                set(ui, "Motionbox", "yellow", connected_to.clone())?;
                let tip = t(" The Yaw VR still needs centering; do it on the Tuning -> Motion page.") + &connected_to;
                tooltips.borrow_mut().insert("Motionbox", tip);
                hardware_notification(ui, backend, HardwareNotification::YawVr { needs_center: true })?;
            } else {
                hardware_notification(ui, backend, HardwareNotification::YawVr { needs_center: false })?;
                set(ui, "Motionbox", "green", connected_to)?;
            }
        }
    } else {
        let reference = if crate::catalog::current_model(backend)?.manufacturer == "VNM SIMULATION" {
            backend.native_setting("VNM SIMULATION", "vnm_com_port").map_err(|error| error.detail)?
        } else if crate::catalog::current_model(backend)?.manufacturer == "PROSIMU" {
            backend.native_setting("PROSIMU", "prs200_com_port1").map_err(|error| error.detail)?
        } else {
            LegacyValue::Str(String::new())
        };
        if native_equal(&reference, &LegacyValue::Str(String::new()))
            && crate::catalog::current_model(backend)?.manual_com_port
        {
            set(ui, "Motionbox", "yellow", not_connected.clone())?;
            let tip = t(" Choose the COM port for Motion on the Setup -> Hardware page.") + &not_connected;
            tooltips.borrow_mut().insert("Motionbox", tip);
        } else if !hardware_is(backend, "motion_warning", "")? {
            set(ui, "Motionbox", "yellow", not_connected.clone())?;
            let tip = append_native_text(" ".into(), hardware_value(backend, "motion_warning")?)?;
            tooltips.borrow_mut().insert("Motionbox", tip);
        } else if hardware_value(backend, "motion_error")?.is_truthy() {
            let tip = t("Sending data to the motion platform failed. Restart OpenRigCue, then try again");
            set(ui, "Motionbox", "red", tip)?;
        } else {
            set(ui, "Motionbox", "red", not_connected)?;
        }
    }

    // Shakebox.
    let card = backend.native_setting("SHAKEBOX", "soundcard").map_err(|error| error.detail)?;
    if native_equal(&card, &LegacyValue::Str("None".into())) {
        let tip = initial_tooltips["Shakebox"].clone();
        set(ui, "Shakebox", "red", tip)?;
    } else {
        // One translated sentence whose `^device^` part is filled in afterwards.
        let sentence = t("Shakebox is linked to ^device^");
        let card = backend.native_setting("SHAKEBOX", "soundcard").map_err(|error| error.detail)?;
        let tip = sentence.replace("^device^", &append_native_text(String::new(), card)?);
        set(ui, "Shakebox", "green", tip)?;
    }

    // Motion compensation (win32 rows only).
    if ui.with(|app| app.win32()) {
        let mc_tooltip = initial_tooltips["MCMode"].clone();
        let switch_on = || -> Result<bool, String> {
            let value = hardware_value(backend, "mc_switch")?;
            Ok(native_equal(&value, &LegacyValue::Int(1)))
        };
        let sensor = backend.native_setting("MOTION_COMPENSATION", "type").map_err(|error| error.detail)?;
        if native_equal(&sensor, &LegacyValue::Str("W".into())) {
            if !auto_search(backend, "MOTION_COMPENSATION")? {
                set_translated(
                    ui,
                    "MCMode",
                    "orange",
                    "Motion Compensation is switched off on the Setup -> Hardware page",
                )?;
            } else if hardware_is(backend, "mc_status", "Not Connected")? {
                let port = backend.native_setting("MOTION_COMPENSATION", "com_port").map_err(|error| error.detail)?;
                if native_equal(&port, &LegacyValue::Str(String::new())) {
                    set(ui, "MCMode", "red", mc_tooltip)?;
                } else {
                    set(ui, "MCMode", "red", mc_tooltip.clone())?;
                    let tip = pinned(&mc_tooltip, "WITMOTION", "MOTION_COMPENSATION")?;
                    tooltips.borrow_mut().insert("MCMode", tip);
                }
            } else if switch_on()? {
                // One translated sentence whose `^port^` part is filled in afterwards.
                let sentence = t("Motion compensation sensor attached to ^port^");
                let port = append_native_text(String::new(), hardware_value(backend, "mc_com_port")?)?;
                let tip = sentence.replace("^port^", &port);
                set(ui, "MCMode", "green", tip)?;
            } else {
                set(ui, "MCMode", "red", mc_tooltip)?;
            }
        } else if switch_on()? {
            let tip = t("The virtual motion compensation sensor is on");
            set(ui, "MCMode", "green", tip)?;
        } else {
            set(ui, "MCMode", "red", mc_tooltip)?;
        }
    }

    // iFlag, SIM3D, HapticR.
    for (key, section, config_section, disabled_text, device) in [
        ("iFlag", "iflag", "IFLAG", "iFlag is switched off on the Setup -> Hardware page", "WITMOTION"),
        ("SIM3D", "sim3d", "SIM3D", "SIM3D is switched off on the Setup -> Hardware page", "SIM3D"),
        ("HapticR", "hapticr", "HAPTICR", "HapticR is switched off on the Setup -> Hardware page", "HAPTICR"),
    ] {
        if key == "HapticR" && !hapticr_enabled(backend)? {
            continue;
        }
        let tooltip = initial_tooltips[key].clone();
        if !auto_search(backend, config_section)? {
            set_translated(ui, key, "orange", disabled_text)?;
        } else if hardware_is(backend, &format!("{section}_status"), "Connected")? {
            let old_iflag = if key == "iFlag" {
                let version = hardware_value(backend, "iflag_version")?;
                openrigcue_model::python_compare::numeric_cmp(&version, &LegacyValue::Float(1.3))
                    .map_err(|_| {
                        format!(
                            "TypeError: '<=' not supported between instances of '{}' and 'float'",
                            version.type_name()
                        )
                    })?
                    .is_some_and(|order| order.is_le())
            } else {
                false
            };
            if old_iflag {
                set(ui, key, "yellow", tooltip.clone())?;
                let tip =
                    t(" OpenRigCue cannot work with the firmware on this iFlag; update the firmware. ") + &tooltip;
                tooltips.borrow_mut().insert(key, tip);
                hardware_notification(ui, backend, HardwareNotification::IflagFirmware)?;
            } else {
                set_translated(ui, key, "green", "Connected")?;
            }
        } else {
            let port = backend.native_setting(config_section, "com_port").map_err(|error| error.detail)?;
            if native_equal(&port, &LegacyValue::Str(String::new())) {
                set_translated(ui, key, "red", "Not found")?;
            } else {
                set(ui, key, "red", tooltip.clone())?;
                let tip = pinned(&tooltip, device, config_section)?;
                tooltips.borrow_mut().insert(key, tip);
            }
        }
    }

    // HUD.
    if !auto_search(backend, "HUD")? {
        set_translated(ui, "Hud", "orange", "HUD is switched off on the Setup -> Hardware page")?;
    } else if hardware_is(backend, "hud_status", "Connected")? {
        set_translated(ui, "Hud", "green", "Connected")?;
    } else {
        let hud = hardware_value(backend, "hud_status")?;
        let LegacyValue::Str(hud) = hud else {
            return Err(format!("AttributeError: '{}' object has no attribute 'startswith'", hud.type_name()));
        };
        if hud.starts_with("Installing new firmware") {
            set(ui, "Hud", "pink", initial_tooltips["Hud"].clone())?;
            let tooltip = hardware_text(backend, "hud_status")?;
            tooltips.borrow_mut().insert("Hud", tooltip);
        } else {
            set_translated(ui, "Hud", "red", "Not found")?;
        }
    }

    // Belt.
    if !auto_search(backend, "BELT_TENSIONER")? {
        set_translated(ui, "Belt", "orange", "Motion is switched off on the Setup -> Hardware page")?;
    } else if hardware_is(backend, "belt_status", "Connected")? {
        // One translated line whose `^name^` parts are filled in afterwards.
        let tip = t("Model: ^model^ - Version: ^version^ - Port: ^port^ - Serial Number: ^serial^")
            .replace("^model^", &hardware_text(backend, "belt_model")?)
            .replace("^version^", &hardware_text(backend, "belt_version")?)
            .replace("^port^", &hardware_text(backend, "belt_com_port")?)
            .replace("^serial^", &hardware_text(backend, "belt_sn")?);
        let homing = hardware_value(backend, "belt_homing")?;
        if native_equal(&homing, &LegacyValue::Int(0))
            && native_equal(&hardware_value(backend, "belt_switch")?, &LegacyValue::Int(1))
        {
            set(ui, "Belt", "yellow", tip.clone())?;
            let tip = t(" The belt needs calibrating; do it on the Tuning -> Belt page.") + &tip;
            tooltips.borrow_mut().insert("Belt", tip);
            hardware_notification(ui, backend, HardwareNotification::Belt { needs_calibration: true })?;
        } else {
            hardware_notification(ui, backend, HardwareNotification::Belt { needs_calibration: false })?;
            set(ui, "Belt", "green", tip)?;
        }
    } else if hardware_is(backend, "belt_status", "Not Connected")?
        && !ui.with(|app| {
            app.ml
                .previous_belt_status
                .as_ref()
                .is_some_and(|value| native_equal(value, &LegacyValue::Str("Reconnecting".into())))
        })
    {
        let port = backend.native_setting("BELT_TENSIONER", "com_port").map_err(|error| error.detail)?;
        if native_equal(&port, &LegacyValue::Str(String::new())) {
            set_translated(ui, "Belt", "red", "No USB link to the belt")?;
        } else {
            let base = initial_tooltips["Belt"].clone();
            set(ui, "Belt", "red", base.clone())?;
            let tip = pinned(&base, "Belt", "BELT_TENSIONER")?;
            tooltips.borrow_mut().insert("Belt", tip);
        }
    } else if hardware_is(backend, "belt_status", "Motor Offline")? {
        set_translated(ui, "Belt", "red", "Belt has no power connection")?;
    } else {
        set_translated(ui, "Belt", "pink", "Newer firmware detected, installing...")?;
    }

    // Each device's status colour and explanation, for the interface's rig status.
    let states: Vec<(String, (String, String))> = colors
        .borrow()
        .iter()
        .map(|(key, color)| {
            let tooltip = tooltips.borrow().get(key).cloned().unwrap_or_default();
            ((*key).to_owned(), (color.clone(), tooltip))
        })
        .collect();
    ui.with(|app| app.ml.hardware_states.extend(states));
    Ok(())
}

pub(crate) fn apply_ui_actions_shared(shared: &Mutex<MyApp>) -> Result<(), String> {
    let backend = lock(shared).backend.clone();
    if native_equal(&backend.game_value().map_err(|error| error.detail)?, &LegacyValue::Str("none".into())) {
        return Ok(());
    }
    crate::ui::screens::tuning::apply_shake_hotkeys_shared(shared)?;
    crate::ui::screens::motion::apply_hotkeys_shared(shared)?;
    crate::ui::screens::tuning::apply_hotkeys_shared(shared)
}

fn before_reload_rules_shared(shared: &Mutex<MyApp>) -> Result<(), String> {
    apply_ui_actions_shared(shared)?;
    crate::ui::screens::hardware::main_loop_rules_shared(shared)?;
    let backend = lock(shared).backend.clone();
    backend.clear_ui_actions();
    Ok(())
}
