//! Tuning → Downloaded Profiles: the profiles of the library on this computer (it never contacts the
//! network), by category (the settings group picker) and filter, each with its game, vehicle, type, author,
//! description and state. Load applies one: for another game or vehicle the edit context changes first and
//! the review opens when its controls have loaded; the review applies only the profile's category to the
//! current tune, and Save keeps it.

use openrigcue_config::catalog::registry::Registry;
use openrigcue_config::synced_profiles::{Category, Entry, FileState, Target, Vehicle};
use openrigcue_model::native_value::LegacyValue;
use openrigcue_model::python_compare::native_equal;

use super::super::components;
use super::super::html::{esc, t, tr};
use super::super::notices;
use super::super::{themes, Region, Screen};
use super::reload;
use super::tuning::{self, Dialog};
use crate::app::MyApp;
use crate::backend::{native_bool, native_int, native_int_as_float, native_int_float, BackendExt};

/// The categories of the group picker, after "All categories".
const CATEGORIES: [Category; 5] =
    [Category::Shakers, Category::Belt, Category::Rumble, Category::Haptic, Category::Motion];

/// The group picker's index of a category.
pub fn category_index(category: Category) -> usize {
    CATEGORIES.iter().position(|c| *c == category).map_or(0, |i| i + 1)
}

/// The category the group picker shows (`None`: every category).
fn selected_category(app: &MyApp) -> Option<Category> {
    app.ui.section.checked_sub(1).and_then(|i| CATEGORIES.get(i)).copied()
}

/// The catalog text of a category.
fn profile_type(category: Category) -> &'static str {
    match category {
        Category::Shakers => "Shakers",
        other => other.page(),
    }
}

/// The screen a category's controls are on.
fn screen_of(category: Category) -> Screen {
    match category {
        Category::Motion => Screen::Motion,
        Category::Shakers => Screen::Shake,
        Category::Belt => Screen::Belt,
        Category::Rumble => Screen::Sim3d,
        Category::Haptic => Screen::HapticR,
    }
}

fn game_name(app: &MyApp, id: &str) -> String {
    openrigcue_config::catalog::Catalog::builtin().game(id).map_or_else(|| tr(app, "Unknown game"), |g| g.name.clone())
}

fn vehicle_text(app: &MyApp, vehicle: Option<&Vehicle>) -> String {
    match vehicle {
        Some(Vehicle::Named(name)) => name.clone(),
        Some(all) => tr(app, all.label()),
        None => tr(app, "Unavailable"),
    }
}

fn state_text(app: &MyApp, state: &FileState) -> String {
    match state {
        FileState::Invalid(message) => message.clone(),
        fixed => tr(app, fixed.label()),
    }
}

/// Reads the library when the screen opens.
pub fn on_enter(app: &mut MyApp) {
    match app.backend.cached_profiles() {
        Ok(entries) => {
            app.ui.tuning.library = entries;
            app.ui.tuning.library_error = None;
        }
        Err(failure) => {
            app.ui.tuning.library = Vec::new();
            app.ui.tuning.library_error = Some(failure.detail);
        }
    }
}

/// Whether the profile in use may switch to another game or vehicle (no game telemetry runs).
fn can_switch_context(app: &MyApp) -> bool {
    app.backend.session_value("listener_running").is_ok_and(|l| l == LegacyValue::None || l.as_str() == Some("none"))
}

/// Whether an entry is for the running game and vehicle.
fn compatible(app: &MyApp, entry: &Entry) -> bool {
    app.backend.game_value().ok().as_ref().and_then(LegacyValue::as_str) == Some(entry.target.game.as_str())
        && match &entry.target.vehicle {
            Vehicle::Game(_) => true,
            Vehicle::Named(name) => {
                app.backend.session_value("vehicle_name").ok().as_ref().and_then(LegacyValue::as_str) == Some(name)
            }
        }
}

/// The entries the category and the filter keep, with their index in the library.
fn shown(app: &MyApp) -> Vec<(usize, &Entry, String)> {
    let category = selected_category(app);
    let needle = app.ui.tuning.library_filter.trim().to_lowercase();
    app.ui
        .tuning
        .library
        .iter()
        .enumerate()
        .filter(|(_, entry)| category.is_none_or(|c| entry.target.page == c))
        .filter_map(|(index, entry)| {
            let parts = [
                vehicle_text(app, entry.source_vehicle.as_ref()),
                tr(app, profile_type(entry.target.page)),
                entry.author.clone(),
                entry.description.clone(),
                state_text(app, &entry.state),
            ];
            let searchable = format!("{} {}", game_name(app, &entry.target.game), parts.join(" ")).to_lowercase();
            let line = parts.iter().filter(|p| !p.is_empty()).map(String::as_str).collect::<Vec<_>>().join(" · ");
            (needle.is_empty() || searchable.contains(&needle)).then_some((index, entry, line))
        })
        .collect()
}

/// The body.
pub fn main(app: &MyApp) -> String {
    let mut groups = vec![(0, tr(app, "All categories"))];
    groups.extend(CATEGORIES.iter().enumerate().map(|(i, c)| (i + 1, tr(app, profile_type(*c)))));
    let picker = super::section_picker(app, &groups, app.ui.section.min(CATEGORIES.len()));
    let filter = components::field(
        &t(app, "Find a profile"),
        &format!(
            "<input id=\"library-filter\" class=\"field-input\" type=\"search\" value=\"{}\" placeholder=\"{}\" aria-label=\"{}\" data-act=\"set\" data-arg=\"Downloaded Profiles:filter\" data-live autocomplete=\"off\">",
            esc(&app.ui.tuning.library_filter),
            t(app, "Filter game, vehicle, author, description or category"),
            t(app, "Find a profile")
        ),
        "",
    );
    let switchable = can_switch_context(app);
    let mut rows = String::new();
    for (index, entry, line) in shown(app) {
        let ready = entry.state == FileState::Ready;
        let reachable = compatible(app, entry) || switchable;
        let title = if !reachable {
            format!(" title=\"{}\"", t(app, "Stop game telemetry before editing another game or vehicle."))
        } else {
            String::new()
        };
        rows.push_str(&format!(
            "<div class=\"control-row profile-row\"><div class=\"control-label\"><strong translate=\"no\">{}</strong></div><small translate=\"no\">{}</small><div class=\"control-body action-row\"><button type=\"button\" data-act=\"action\" data-arg=\"Downloaded Profiles:select:{index}\"{title}{}>{}</button></div></div>",
            esc(&game_name(app, &entry.target.game)),
            esc(&line),
            if ready && reachable { "" } else { " disabled" },
            t(app, "Select")
        ));
    }
    let empty = if rows.is_empty() {
        let text = match &app.ui.tuning.library_error {
            Some(error) => esc(error),
            None => t(app, "No downloaded profiles here yet. Sync your library first; browsing never contacts SRS."),
        };
        components::field(&t(app, "No downloaded profiles"), &format!("<p>{text}</p>"), "")
    } else {
        String::new()
    };
    let info = format!(
        "<div class=\"screen-note\"><p>{}</p></div>",
        t(app, "Select a profile, then click Apply. Use Save to keep your changes or Cancel to discard them.")
    );
    let tools = format!(
        "<div class=\"device action-row\">{}{}</div>",
        components::button(&t(app, "Refresh"), "action", "Downloaded Profiles:refresh:", ""),
        components::button(&t(app, "My profiles"), "action", "Downloaded Profiles:profiles:", "")
    );
    components::panel(
        &themes::title(app, "Downloaded Profiles"),
        &picker,
        &format!("{info}{filter}{rows}{empty}{tools}"),
        "",
    )
}

/// The filter text changed.
pub fn set(app: &mut MyApp, control: &str, _part: &str, value: &str) -> Result<(), String> {
    match control {
        "filter" => {
            app.ui.tuning.library_filter = value.to_owned();
            Ok(())
        }
        _ => Err(format!("no Downloaded Profiles value {control:?}")),
    }
}

/// A button of the screen: Load of an entry, Refresh, My profiles.
pub fn action(app: &mut MyApp, control: &str, index: Option<usize>) -> Result<(), String> {
    match control {
        "select" => {
            let entry = index
                .and_then(|i| app.ui.tuning.library.get(i))
                .cloned()
                .ok_or_else(|| format!("no downloaded profile {index:?}"))?;
            if let Err(error) = select(app, &entry) {
                warn(app, &error);
            }
        }
        "refresh" => on_enter(app),
        "profiles" => super::super::navigate(app, Screen::Profiles, 0),
        _ => return Err(format!("no Downloaded Profiles button {control:?}")),
    }
    Ok(())
}

/// A failure shown as a warning.
fn warn(app: &mut MyApp, error: &str) {
    let message = tr(app, error);
    notices::warning(app, "Downloaded Profiles", &message);
}

/// An entry was chosen: for the running game and vehicle its review opens; otherwise, while no telemetry
/// runs, the edit switches to its game and vehicle and the review waits for their controls.
pub fn select(app: &mut MyApp, entry: &Entry) -> Result<(), String> {
    if matches_context(&*app.backend, &entry.target)? {
        return review(app, &entry.id);
    }
    if !can_switch_context(app) {
        return Err("Stop game telemetry before editing another game or vehicle.".into());
    }
    app.backend.begin_profile_edit(&entry.target.game, &entry.target.vehicle.native()).map_err(|e| e.detail)?;
    if let Ok(controls) = app.backend.get("controls") {
        app.docs.controls = controls;
    }
    tuning::open_dialog(app, Dialog::LibraryPending(Pending { id: entry.id.clone(), target: entry.target.clone() }));
    Ok(())
}

/// Whether the profile in use is the target's game and vehicle with its controls loaded; an error while
/// they are still loading.
pub fn matches_context(backend: &dyn crate::backend::Backend, target: &Target) -> Result<bool, String> {
    if backend.game_value().map_err(|e| e.detail)?.as_str() != Some(&target.game) {
        return Ok(false);
    }
    if backend.active_profile_game().map_err(|e| e.detail)? != target.game {
        return Err("The selected game’s tuning controls are still loading. Try again after they refresh.".into());
    }
    let current_vehicle = backend.session_value("vehicle_name").map_err(|e| e.detail)?;
    let loaded_vehicle = backend.active_profile_vehicle().map_err(|e| e.detail)?;
    let normalized = openrigcue_config::profiles::Vehicle::from_native(&current_vehicle).native_value();
    if !native_equal(&loaded_vehicle, &normalized) {
        return Err("The selected vehicle’s tuning controls are still loading. Try again after they refresh.".into());
    }
    if let Vehicle::Named(expected) = &target.vehicle {
        return Ok(current_vehicle.as_str() == Some(expected));
    }
    Ok(true)
}

/// A selection waiting for its game or vehicle.
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    pub id: String,
    pub target: Target,
}

/// The review of a profile: its id and the question.
#[derive(Debug, Clone, PartialEq)]
pub struct Review {
    pub id: String,
    pub question: String,
}

/// The waiting dialog.
pub fn pending_dialog(app: &MyApp, _pending: &Pending) -> String {
    super::super::dialogs::frame(
        app,
        &t(app, "Downloaded Profiles"),
        &format!(
            "<p class=\"dialog-text\" aria-busy=\"true\">{}</p>",
            t(app, "Loading the selected profile. Close to cancel.")
        ),
        None,
    )
}

/// The review dialog.
pub fn review_dialog(app: &MyApp, review: &Review) -> String {
    super::super::dialogs::frame(
        app,
        &t(app, "Apply Downloaded Profile?"),
        &format!("<p class=\"dialog-text\" translate=\"no\">{}</p>", esc(&review.question)),
        Some((&t(app, "Apply"), true)),
    )
}

/// Opens the review of a profile for the running game and vehicle.
fn review(app: &mut MyApp, id: &str) -> Result<(), String> {
    let (entry, _) = app.backend.cached_profile(id).map_err(|e| e.detail)?;
    if !matches_context(&*app.backend, &entry.target)? {
        return Err("Select this profile’s game and vehicle in the Profile Manager before applying it.".into());
    }
    if !screen_of(entry.target.page).available(app) {
        return Err("This tuning category is disabled for the current setup.".into());
    }
    let current_vehicle = app.backend.session_value("vehicle_name").map_err(|error| error.detail)?;
    let destination = if native_equal(&current_vehicle, &LegacyValue::Int(0)) {
        tr(app, "All vehicles")
    } else {
        crate::backend::text(&crate::backend::native_json(&current_vehicle)?)
    };
    let kind = tr(app, profile_type(entry.target.page));
    let game = game_name(app, &entry.target.game);
    let question =
        tr(app, "Apply this ^type^ profile by ^author^ to ^game^ — ^vehicle^? Other settings stay unchanged.")
            .split('^')
            .enumerate()
            .map(|(index, part)| match (index % 2, part) {
                (1, "type") => kind.as_str(),
                (1, "author") => entry.author.as_str(),
                (1, "game") => game.as_str(),
                (1, "vehicle") => destination.as_str(),
                _ => part,
            })
            .collect::<String>();
    tuning::open_dialog(app, Dialog::LibraryReview(Review { id: id.to_owned(), question }));
    Ok(())
}

/// The edit ended or was thrown away: a selection still waiting for its game or vehicle is dropped.
pub fn cancel(app: &mut MyApp) {
    app.ui.tuning.library_behind = None;
    if matches!(tuning::open_one(app), Some(Dialog::LibraryPending(_))) {
        super::super::dialogs::close(app);
    }
}

/// The selection waiting for its game or vehicle: its own dialog in front, or held behind another dialog.
fn waiting(app: &MyApp) -> Option<Pending> {
    match tuning::open_one(app) {
        Some(Dialog::LibraryPending(pending)) => Some(pending.clone()),
        _ => app.ui.tuning.library_behind.clone(),
    }
}

/// The per-pass check: a selection waiting for its game or vehicle, in front or behind another dialog, opens
/// its category's screen and its review once their controls have loaded.
pub fn tick(app: &mut MyApp) {
    let Some(pending) = waiting(app) else { return };
    if matches!(matches_context(&*app.backend, &pending.target), Ok(true)) {
        open_pending(app, &pending);
    }
}

/// [`tick`] for the main loop's shared application: the identity checks run without holding it, and a
/// selection cancelled meanwhile does not open.
pub(crate) fn tick_shared(shared: &std::sync::Mutex<MyApp>) {
    let (pending, backend) = {
        let app = crate::host::lock(shared);
        let Some(pending) = waiting(&app) else { return };
        (pending, app.backend.clone())
    };
    if !matches!(matches_context(&*backend, &pending.target), Ok(true)) {
        return;
    }
    let mut app = crate::host::lock(shared);
    if waiting(&app).as_ref() == Some(&pending) {
        open_pending(&mut app, &pending);
    }
}

/// The waiting selection's game and vehicle have loaded: its category's screen and its review open, in front.
/// Its own dialog closes; a dialog it waited behind stays behind the review and comes back when that closes.
fn open_pending(app: &mut MyApp, pending: &Pending) {
    use super::super::dialogs::{self, Dialog as Any};
    if matches!(tuning::open_one(app), Some(Dialog::LibraryPending(_))) {
        dialogs::close(app);
    } else {
        app.ui.tuning.library_behind = None;
        match app.ui.dialog.take() {
            Some(Any::Wizard(step)) => app.ui.tuning.wizard_behind = Some(step),
            Some(front) => app.ui.tuning.dialog_behind = Some(Box::new(front)),
            None => {}
        }
    }
    super::super::navigate(app, screen_of(pending.target.page), 0);
    if let Err(error) = review(app, &pending.id) {
        warn(app, &error);
        if app.ui.dialog.is_none() {
            // No review: the dialog held behind comes back.
            dialogs::close(app);
        }
    }
}

/// The review was confirmed: the profile's category is applied to the current tune (Save keeps it), and its
/// screen opens.
pub fn apply_confirmed(app: &mut MyApp, review: &Review) -> Result<(), String> {
    let result = (|| -> Result<Category, String> {
        let (entry, tune) = app.backend.cached_profile(&review.id).map_err(|e| e.detail)?;
        if !matches_context(&*app.backend, &entry.target)? {
            return Err("The active game or vehicle changed; select the profile again.".into());
        }
        apply_to_current(app, entry.target.page, &tune)?;
        Ok(entry.target.page)
    })();
    match result {
        Ok(category) => {
            app.ui.tuning.library_applied = true;
            notices::alert(app, "Success", "Profile applied. Click Save to keep these settings.", "green");
            super::super::navigate(app, screen_of(category), 0);
        }
        Err(error) => warn(app, &error),
    }
    super::super::refresh(app, &[Region::Save]);
    Ok(())
}

/// What a switch shows for a stored value: its boolean, off when it has none.
fn switch_shown(app: &MyApp, path: &[&str]) -> bool {
    reload::stored(app, path).ok().and_then(|value| native_bool(&value).ok()).unwrap_or(false)
}

/// A downloaded switch value set on a switch: a boolean word, which is written (as `write` makes it) when the
/// switch shows the other state; anything else is an error.
fn switch_value(app: &MyApp, path: &[&str], value: Result<LegacyValue, String>) -> Result<Option<bool>, String> {
    let on = native_bool(&value?)?;
    Ok((switch_shown(app, path) != on).then_some(on))
}

/// Writes a category's values from a downloaded tune into the current tune, each through its control: a
/// slider rounds to its step within its range, a spin box keeps within its range, a range orders and limits
/// its ends, a switch takes only a boolean word. A value the control cannot take leaves the control as it was,
/// or takes the control's default.
pub fn apply_to_current(app: &mut MyApp, category: Category, tune: &LegacyValue) -> Result<(), String> {
    use reload::{as_number, field, range, slider};
    let effect = crate::slider::SliderScale::new(0.0, 20.0, 1.0);
    let percent = crate::slider::SliderScale::new(0.0, 100.0, 1.0);
    match category {
        Category::Shakers => {
            for item in &Registry::builtin().shaker_effects {
                let key = item.id.as_str();
                let value = field(tune, &["shaker_effects", key])
                    .and_then(|value| slider(effect, &value))
                    .or_else(|_| slider(effect, &0.into()))?;
                reload::put_shaker(app, "shaker_effects", key, as_number(value))?;
                if let Ok(Some(on)) = switch_value(app, &["shaker_switch", key], field(tune, &["shaker_switch", key])) {
                    app.set_shaker_value_checked("shaker_switch", key, &LegacyValue::Bool(on))?;
                }
            }
        }
        Category::Belt => {
            let page = super::belt::page();
            for (row, key, is_switch) in reload::BELT_ROWS {
                let result = (|| -> Result<(), String> {
                    if is_switch {
                        if let Some(on) = switch_value(app, &[key], field(tune, &[key]))? {
                            app.set_tune_native_checked(&[key], &LegacyValue::Int(i64::from(on)))?;
                        }
                        Ok(())
                    } else {
                        let Some(crate::spec::Widget::Slider { scale }) = page.control(row).map(|c| c.widget.clone())
                        else {
                            return Ok(());
                        };
                        let value = slider(scale, &field(tune, &[key])?)?;
                        reload::put(app, &[key], LegacyValue::Int(value.trunc() as i64))
                    }
                })();
                if let Err(error) = result {
                    tracing::debug!(%error, row, "belt value of the downloaded profile");
                }
            }
        }
        Category::Rumble | Category::Haptic => {
            let (effects_key, switch_key, effects, start) = if category == Category::Rumble {
                ("rumble_effects", "rumble_switch", &Registry::builtin().rumble_effects, "rpm_start_rumble")
            } else {
                ("haptic_effects", "haptic_switch", &Registry::builtin().haptic_effects, "rpm_start_haptic")
            };
            for item in effects {
                let key = item.id.as_str();
                let value = field(tune, &[effects_key, key])
                    .and_then(|value| slider(effect, &value))
                    .or_else(|_| slider(effect, &10.into()))?;
                reload::put(app, &[effects_key, key], LegacyValue::Int(value.trunc() as i64))?;
                let on = match switch_value(app, &[switch_key, key], field(tune, &[switch_key, key])) {
                    Ok(on) => on,
                    Err(_) => (!switch_shown(app, &[switch_key, key])).then_some(true),
                };
                if let Some(on) = on {
                    app.set_tune_native_checked(&[switch_key, key], &LegacyValue::Bool(on))?;
                }
            }
            let fade = (|| {
                let low = native_int_float(&field(tune, &[effects_key, "rpm_start_fading"])?)?;
                let high = native_int_as_float(&field(tune, &[effects_key, "rpm_stop_fading"])?)?;
                range(FADE, &LegacyValue::Float(low), &LegacyValue::Float(high))
            })();
            let (low, high) = match fade {
                Ok(fade) => fade,
                Err(_) => range(FADE, &LegacyValue::Float(70.0), &LegacyValue::Float(95.0))?,
            };
            reload::put(app, &[effects_key, "rpm_start_fading"], LegacyValue::Int(low.trunc() as i64))?;
            reload::put(app, &[effects_key, "rpm_stop_fading"], LegacyValue::Int(high.trunc() as i64))?;
            for (source, key) in [
                (start, "rpm_start_rumble"),
                ("wheel_slip_min_activation", "wheel_slip_min_activation"),
                ("wheel_lock_min_activation", "wheel_lock_min_activation"),
                ("traction_loss_min_activation", "traction_loss_min_activation"),
            ] {
                let value = field(tune, &[effects_key, source])
                    .and_then(|value| native_int_float(&value))
                    .and_then(|value| slider(percent, &LegacyValue::Float(value)))
                    .or_else(|_| slider(percent, &0.into()))?;
                reload::put(app, &[effects_key, key], as_number(value))?;
            }
        }
        Category::Motion => apply_motion(app, tune)?,
    }
    Ok(())
}

/// The RPM fade range of SIM3D and HapticR (0 to 100 %, whole steps).
const FADE: crate::slider::RangeScale = crate::slider::RangeScale::new(0.0, 100.0, 1.0);

/// The motion values of a downloaded tune, each through its control (see [`apply_to_current`]): the axis
/// types (a row without a usable type, or without the current maximum of its type, is hidden), each effect's
/// strength, Max Telemetry and switch, Enhanced G-Forces, gear smoothing, the belt tensioner (50 without a
/// value), the traction loss thresholds (the front one the rear one's without a value), the platform rows (Max
/// Horizontal 100 without a value), the smoothing rows (20 for none or 0, except 0 on DOF Reality platforms) and
/// the vertical actuators (100 without a value). Values the rows require stop the rows after them.
fn apply_motion(app: &mut MyApp, tune: &LegacyValue) -> Result<(), String> {
    use reload::{as_number, field, slider, spin};
    let percent = crate::slider::SliderScale::new(0.0, 100.0, 1.0);
    let mut rate = std::collections::BTreeMap::new();
    for axis in ["roll", "pitch"] {
        let row = format!("{axis}_type");
        if field(tune, &["motion_effects", &row]).is_ok() {
            rate.insert(axis, reload::axis_type(app, axis, Some(tune))?);
        } else {
            app.ui.tuning.motion_hidden.insert(row);
        }
    }
    for effect in &Registry::builtin().motion_effects {
        let key = effect.id.as_str();
        if let Ok(value) = field(tune, &["motion_effects", key])
            .and_then(|value| slider(crate::slider::SliderScale::new(0.0, 20.0, 1.0), &value))
        {
            reload::put(app, &["motion_effects", key], as_number(value))?;
        }
        let source_key = if rate.get(key) == Some(&true) { format!("{key}_rate") } else { key.to_owned() };
        if let Some(max) = field(tune, &["max_telemetry_vars", &source_key])
            .ok()
            .and_then(|max| spin(&max, super::motion::MAX_TELEMETRY_RANGE))
        {
            // The box writes the maximum of the type the axis now follows.
            let stored_key = super::motion::max_key(app, key);
            reload::put(app, &["max_telemetry_vars", &stored_key], LegacyValue::Int(max))?;
        }
        if let Ok(Some(on)) = switch_value(app, &["motion_switch", key], field(tune, &["motion_switch", key])) {
            app.set_tune_native_checked(&["motion_switch", key], &LegacyValue::Bool(on))?;
        }
    }
    let gforces = LegacyValue::Int(native_int(&field(tune, &["motion_enhanced_gforces"])?)?);
    if let Some(on) = switch_value(app, &["motion_enhanced_gforces"], Ok(gforces))? {
        app.set_tune_native_checked(&["motion_enhanced_gforces"], &LegacyValue::Int(i64::from(on)))?;
    }
    let gear = slider(percent, &field(tune, &["gear_smoothing_factor"])?)?;
    reload::put(app, &["gear_smoothing_factor"], LegacyValue::Int(gear.trunc() as i64))?;
    let tensioner = field(tune, &["motion_belt_tensioner"])
        .and_then(|value| slider(percent, &value))
        .or_else(|_| slider(percent, &50.into()))?;
    reload::put(app, &["motion_belt_tensioner"], LegacyValue::Int(tensioner.trunc() as i64))?;
    let activation = |value: &LegacyValue| spin(value, super::motion::ACTIVATION_RANGE);
    let rear = field(tune, &["min_traction_loss_activation"])?;
    if let Some(value) = activation(&rear) {
        reload::put(app, &["min_traction_loss_activation"], LegacyValue::Int(value))?;
    }
    let front = field(tune, &["min_traction_loss_front_activation"]).unwrap_or(rear);
    if let Some(value) = activation(&front) {
        reload::put(app, &["min_traction_loss_front_activation"], LegacyValue::Int(value))?;
    }
    let power = slider(percent, &field(tune, &["motion_max_power"])?)?;
    reload::put(app, &["motion_max_power"], as_number(power))?;
    let horizontal = field(tune, &["motion_max_horizontal"])
        .and_then(|value| slider(percent, &value))
        .or_else(|_| slider(percent, &100.into()))?;
    reload::put(app, &["motion_max_horizontal"], as_number(horizontal))?;
    let smoothing = slider(percent, &field(tune, &["motion_smoothing_factor"])?)?;
    reload::put(app, &["motion_smoothing_factor"], as_number(smoothing))?;
    let boost = slider(crate::slider::SliderScale::new(1.0, 100.0, 1.0), &field(tune, &["motion_boost_factor"])?)?;
    reload::put(app, &["motion_boost_factor"], as_number(boost))?;
    let axis_scale = crate::slider::SliderScale::new(0.0, 100.0, super::motion::smoothing_step(app));
    for axis in ["surge", "sway", "traction_loss", "traction_loss_front", "roll", "heave", "pitch", "yaw"] {
        let value = (|| -> Result<f64, String> {
            let mut value = field(tune, &["motion_effect_smoothing_factor", axis])?;
            if native_equal(&value, &LegacyValue::Int(0)) && !dof_reality(app)? {
                value = LegacyValue::Int(20);
            }
            slider(axis_scale, &value)
        })();
        let value = match value {
            Ok(value) => value,
            Err(_) => slider(axis_scale, &LegacyValue::Int(if dof_reality(app)? { 0 } else { 20 }))?,
        };
        reload::put(app, &["motion_effect_smoothing_factor", axis], LegacyValue::Int(value.trunc() as i64))?;
    }
    for axis in ["surge", "sway", "traction_loss", "traction_loss_front"] {
        let key = format!("motion_main_actuators_{axis}");
        let value =
            field(tune, &[&key]).and_then(|value| slider(percent, &value)).or_else(|_| slider(percent, &100.into()))?;
        reload::put(app, &[&key], LegacyValue::Int(value.trunc() as i64))?;
    }
    Ok(())
}

/// Whether the motion platform is a DOF Reality one (its model must be known).
fn dof_reality(app: &MyApp) -> Result<bool, String> {
    Ok(crate::catalog::current_model(&*app.backend)?.manufacturer == "DOF REALITY")
}

#[cfg(test)]
#[path = "library_tests.rs"]
mod tests;
