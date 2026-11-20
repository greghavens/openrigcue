//! What the tuning effect screens share (Wind, Shake, Belt, SIM3D, HapticR, IFlag, LED, Tach, Routines and
//! Downloaded Profiles): their state between redraws, the stand-by lock, the running game's feature rule, the
//! panel with its settings groups, "More settings" and "Actions", the header buttons, the hot-key steps, the
//! values Compare looks at, their dialogs, and the main-loop work of these screens (profile load, per-pass
//! checks and hot keys).

use openrigcue_config::synced_profiles::Category;
use openrigcue_model::native_value::LegacyValue;
use openrigcue_model::python_compare::native_equal;

use super::super::components::{self, RowState};
use super::super::html::{esc, num, t, tr};
use super::super::{read, themes, Region, Screen};
use super::{belt, hapticr, led, library, routines, shake, sim3d};
use crate::app::MyApp;
use crate::spec::{Action, Control, Field, Page, Read, Section, Store, Widget};

/// What these screens keep between redraws.
#[derive(Debug, Default)]
pub struct State {
    /// The library's profiles, as read when Downloaded Profiles opened.
    pub library: Vec<openrigcue_config::synced_profiles::Entry>,
    /// Why the library could not be read.
    pub library_error: Option<String>,
    /// The Downloaded Profiles filter text.
    pub library_filter: String,
    /// A library profile was applied and is not saved yet: Save asks for confirmation. Set by a completed
    /// category apply, cleared by Save and Revert.
    pub library_applied: bool,
    /// The routine whose options are being prepared (its button reads "Loading…").
    pub routine_busy: Option<String>,
    /// What the last profile load (or applied library profile) left hidden on the Motion screen: the
    /// `roll_type` / `pitch_type` rows and the driving-style choice ([`super::motion::PREFERENCE_PICKER`]).
    pub motion_hidden: std::collections::BTreeSet<String>,
    /// The Autotune step going on behind another dialog (see [`super::super::dialogs::open`]).
    pub wizard_behind: Option<super::motion::WizardStep>,
    /// The Downloaded Profiles selection waiting behind another dialog for its game or vehicle (see
    /// [`super::super::dialogs::open`]).
    pub library_behind: Option<library::Pending>,
    /// The dialog that was in front when a selection waiting behind it opened its review: it stays behind the
    /// review and comes back when that closes (see [`super::super::dialogs::close`]).
    pub dialog_behind: Option<Box<super::super::dialogs::Dialog>>,
}

/// The dialogs of these screens.
#[derive(Debug, Clone, PartialEq)]
pub enum Dialog {
    /// Shake: an effect's filter.
    Filter(shake::Filter),
    /// Belt: "Do you have the belt on?" before the calibration.
    BeltCalibrate,
    /// LED: a segment's colour.
    LedColor(led::ColorChoice),
    /// Routines: a routine's configuration.
    Routine(routines::Config),
    /// Downloaded Profiles: apply this profile?
    LibraryReview(library::Review),
    /// Downloaded Profiles: the selected profile's game or vehicle is loading.
    LibraryPending(library::Pending),
}

/// The open dialog.
pub fn render_dialog(app: &MyApp, dialog: &Dialog) -> String {
    match dialog {
        Dialog::Filter(filter) => shake::filter_dialog(app, filter),
        Dialog::BeltCalibrate => belt::calibrate_dialog(app),
        Dialog::LedColor(choice) => led::color_dialog(app, choice),
        Dialog::Routine(config) => routines::config_dialog(app, config),
        Dialog::LibraryReview(review) => library::review_dialog(app, review),
        Dialog::LibraryPending(pending) => library::pending_dialog(app, pending),
    }
}

/// An action of the open dialog: `dialog-field` (name, value), `dialog-ok` or `dialog-close`.
pub fn dialog_act(app: &mut MyApp, dialog: Dialog, act: &str, arg: &str, value: &str) -> Result<(), String> {
    match (dialog, act) {
        (Dialog::Filter(filter), "dialog-field") => shake::filter_field(app, filter, arg, value),
        (Dialog::Filter(_), "dialog-ok") => {
            super::super::dialogs::close(app);
            Ok(())
        }
        (Dialog::Filter(filter), "dialog-close") => {
            super::super::dialogs::close(app);
            shake::filter_cancel(app, &filter)
        }
        (Dialog::BeltCalibrate, "dialog-ok") => {
            super::super::dialogs::close(app);
            belt::request_calibration(app)
        }
        (Dialog::LedColor(choice), "dialog-field") => led::color_field(app, choice, value),
        (Dialog::LedColor(choice), "dialog-ok") => {
            super::super::dialogs::close(app);
            led::color_confirmed(app, &choice)
        }
        (Dialog::Routine(config), "dialog-field") => routines::config_field(app, config, arg, value),
        (Dialog::Routine(config), "dialog-ok") => {
            super::super::dialogs::close(app);
            routines::config_confirmed(app, &config)
        }
        (Dialog::LibraryReview(review), "dialog-ok") => {
            super::super::dialogs::close(app);
            library::apply_confirmed(app, &review)
        }
        (_, "dialog-close" | "dialog-ok") => {
            super::super::dialogs::close(app);
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Opens one of these dialogs.
pub fn open_dialog(app: &mut MyApp, dialog: Dialog) {
    super::super::dialogs::open(app, super::super::dialogs::Dialog::Tuning(dialog));
}

/// The open dialog when it is one of these.
pub fn open_one(app: &MyApp) -> Option<&Dialog> {
    match &app.ui.dialog {
        Some(super::super::dialogs::Dialog::Tuning(dialog)) => Some(dialog),
        _ => None,
    }
}

/// Replaces the open dialog of these screens (a field change) and draws it again.
pub fn replace_dialog(app: &mut MyApp, dialog: Dialog) {
    app.ui.dialog = Some(super::super::dialogs::Dialog::Tuning(dialog));
    super::super::refresh(app, &[Region::Dialog]);
}

// ---- Rules -----------------------------------------------------------------------------------------------

/// A boolean setting of the custom features (false when missing or not a boolean word).
pub fn custom_feature(app: &MyApp, key: &str) -> bool {
    app.backend
        .native_setting("CUSTOM_FEATURES", key)
        .ok()
        .and_then(|value| crate::backend::native_bool(&value).ok())
        .unwrap_or(false)
}

/// Whether a page locked in stand-by is locked now: no game runs and editing without one is off. The Wind
/// page stays open while a fan output is Always On.
pub fn locked(app: &MyApp, page_locked: bool, wind: bool) -> bool {
    if !page_locked || app.ui.session.game_key != "none" {
        return false;
    }
    if custom_feature(app, "allow_edit_emulator") {
        return false;
    }
    if wind {
        let always_on = |control: &str| {
            app.backend.control_value(&["switches", control]).is_ok_and(|v| native_equal(&v, &LegacyValue::Int(2)))
        };
        if always_on("wind") || always_on("wind_center") {
            return false;
        }
    }
    true
}

/// Whether the stand-by note was hidden for good.
fn notice_hidden(app: &MyApp) -> bool {
    app.backend
        .native_setting("APP", "disable_tuning_alert_standby")
        .ok()
        .and_then(|value| crate::backend::native_bool(&value).ok())
        .unwrap_or(false)
}

/// The running game's support of a feature column: `(visible, enabled)`. With `hide_effects_not_in_game` an
/// unsupported row is hidden, otherwise it is disabled. Without a row for the game, or with a
/// `hide_effects_not_in_game` that is missing or not a boolean word, the rule does not apply and both stay.
pub fn feature(app: &MyApp, column: &str) -> (bool, bool) {
    let Some(hide) = app
        .backend
        .native_setting("CUSTOM_FEATURES", "hide_effects_not_in_game")
        .ok()
        .and_then(|value| crate::backend::native_bool(&value).ok())
    else {
        return (true, true);
    };
    match app.game_feature(column) {
        Some(supported) if hide => (supported, true),
        Some(supported) => (true, supported),
        None => (true, true),
    }
}

/// The hint of a row the running game does not use.
pub fn unused_hint(app: &MyApp) -> String {
    t(app, "Not used by this game")
}

/// A row's state from its stored values: the value (and a range's upper value) in the form the control shows
/// them (truncated where the description reads whole numbers), and the per-control switch, which disables the
/// row while it is off.
pub fn base_state(app: &MyApp, control: &Control, enabled: bool) -> RowState {
    let mut state = RowState { enabled, row_enabled: enabled, ..RowState::default() };
    if let Some(field) = &control.field {
        state.value = read::field(app, field);
    }
    if let Widget::Range { high, .. } = &control.widget {
        state.high = read::field(app, high);
    }
    if control.read == Read::IntOfFloat {
        state.value = serde_json::json!(components::number(&state.value).trunc());
        state.high = serde_json::json!(components::number(&state.high).trunc());
    }
    if let Some(toggle) = &control.toggle {
        let on = read::truth(&read::field(app, &toggle.field));
        state.toggle = Some(on);
        state.enabled &= on;
    }
    state
}

/// Whether a screen takes a change of `part` of its row `control` (`button` for a row button): the row is
/// shown and that part is enabled, as the row is drawn. A control the page does not have is taken, so that the
/// screen reports it.
pub fn accepts(page: &Page, control: &str, part: &str, state: impl Fn(&Control) -> Option<RowState>) -> bool {
    match page.control(control) {
        Some(row) => state(row).is_some_and(|state| state.part_enabled(part)),
        None => true,
    }
}

/// Whether a screen takes a click of its row `control`'s button `index` (`None`: the row's own button): the
/// row is shown, the button offered and enabled, as the row is drawn. A control the page does not have is
/// taken, so that the screen reports it.
pub fn accepts_button(
    page: &Page,
    control: &str,
    index: Option<usize>,
    state: impl Fn(&Control) -> Option<RowState>,
) -> bool {
    let Some(row) = page.control(control) else { return true };
    state(row).is_some_and(|state| match index {
        Some(index) => state.part_enabled("button") && state.buttons.as_ref().is_none_or(|kept| kept.contains(&index)),
        None => state.part_enabled(""),
    })
}

/// The heading of a settings group (an untitled group is "General").
pub fn section_title(section: &Section) -> &str {
    if section.title.is_empty() {
        "General"
    } else {
        &section.title
    }
}

/// An extra entry of a page's "Actions": its catalog text, its action id (`<screen>:@<id>`), whether it is
/// enabled and its tooltip.
pub struct Extra {
    pub label: &'static str,
    pub id: &'static str,
    pub enabled: bool,
    pub hint: Option<String>,
}

/// The "Downloaded profiles" entry of the Actions of the screens whose profiles the library holds.
pub fn library_extra() -> Extra {
    Extra { label: "Downloaded profiles", id: "library", enabled: true, hint: None }
}

/// Opens Downloaded Profiles at a category.
pub fn open_library(app: &mut MyApp, category: Category) {
    super::super::navigate(app, Screen::DownloadedProfiles, library::category_index(category));
}

/// The panel of a screen built from its page description: the stand-by note, the selected settings group's
/// rows (four, the rest under "More settings"), and the page's header buttons and `extras` under "Actions".
/// `state` gives each row's state (`None` hides it); `header` whether a header button is enabled and its
/// tooltip (`None` hides it); `intro` comes before the rows; `unit` fills a `{unit}` template.
#[allow(clippy::too_many_arguments)]
pub fn panel(
    app: &MyApp,
    screen: Screen,
    page: &Page,
    locked: bool,
    unit: &str,
    state: impl Fn(&Control) -> Option<RowState>,
    header: impl Fn(&Action) -> Option<(bool, Option<String>)>,
    extras: &[Extra],
    intro: &str,
) -> String {
    let visible: Vec<(usize, &Section, Vec<String>)> = page
        .sections
        .iter()
        .enumerate()
        .filter_map(|(index, section)| {
            let rows = super::rows(app, screen, &section.controls, unit, &state);
            (!rows.is_empty()).then_some((index, section, rows))
        })
        .collect();
    let selected = visible.iter().position(|(index, ..)| *index == app.ui.section).unwrap_or(0);
    let groups: Vec<(usize, String)> = visible.iter().map(|(i, s, _)| (*i, tr(app, section_title(s)))).collect();
    let mut body = String::new();
    if locked && !notice_hidden(app) {
        body.push_str(&format!(
            "<div class=\"screen-note\"><p>{}</p><button type=\"button\" class=\"text-button\" data-act=\"standby-hide\">{}</button></div>",
            t(app, "Tuning is unavailable in Stand By mode; start a game to tune."),
            t(app, "Hide this notice from now on")
        ));
    }
    body.push_str(intro);
    let (title, picker) = match visible.get(selected) {
        Some((index, section, rows)) => {
            body.push_str(&components::more(app, rows, &format!("{}:{index}", screen.key())));
            (section_title(section).to_owned(), super::section_picker(app, &groups, *index))
        }
        None => (page.title.clone(), String::new()),
    };
    let mut buttons: String = page
        .header
        .iter()
        .enumerate()
        .filter_map(|(i, button)| match button {
            Widget::Button { text: label, run } => {
                let (enabled, hint) = header(run)?;
                let hint = hint
                    .or_else(|| page.header_hints.get(label).map(|h| t(app, h)))
                    .map(|h| format!(" title=\"{h}\""))
                    .unwrap_or_default();
                Some(format!(
                    "<button type=\"button\" data-act=\"action\" data-arg=\"{}:@header:{i}\"{hint}{}>{}</button>",
                    esc(screen.key()),
                    if enabled { "" } else { " disabled" },
                    t(app, label)
                ))
            }
            _ => None,
        })
        .collect();
    for extra in extras {
        buttons.push_str(&format!(
            "<button type=\"button\" data-act=\"action\" data-arg=\"{}:@{}\"{}{}>{}</button>",
            esc(screen.key()),
            extra.id,
            extra.hint.as_ref().map(|h| format!(" title=\"{}\"", esc(h))).unwrap_or_default(),
            if extra.enabled { "" } else { " disabled" },
            t(app, extra.label)
        ));
    }
    body.push_str(&components::actions(app, &buttons, &screen.key().to_lowercase()));
    components::panel(&themes::title(app, &title), &picker, &body, "tuning-panel")
}

/// The button a screen's `action` names: a header button (`@header`, by index), a row's button (by index) or
/// the row's own button (no index).
pub fn button(page: &Page, control: &str, index: Option<usize>) -> Option<Action> {
    let widget = if control == "@header" {
        index.and_then(|i| page.header.get(i)).cloned()
    } else {
        let row = page.control(control)?;
        match index {
            Some(i) => row.buttons.get(i).cloned(),
            None => Some(row.widget.clone()),
        }
    };
    match widget {
        Some(Widget::Button { run, .. }) => Some(run),
        _ => None,
    }
}

/// What every tuning screen's buttons share: opening a screen and the shortcut editor. `false` when the
/// action is not one of these.
pub fn run_common(app: &mut MyApp, run: &Action) -> Result<bool, String> {
    match run {
        Action::Goto { left, .. } => {
            let screen = match left.as_str() {
                "App" => Screen::Help,
                other => Screen::from_key(other).unwrap_or(Screen::Hardware),
            };
            super::super::navigate(app, screen, 0);
        }
        Action::Shortcuts { actions } => super::super::shortcuts::open(app, actions)?,
        _ => return Ok(false),
    }
    Ok(true)
}

/// The tune values Compare looks at: every tune field of the page (both ends of a range).
pub fn compared(app: &MyApp, page: &Page) -> Vec<(String, Vec<String>)> {
    let mut rows = Vec::new();
    for control in page.controls() {
        let label = tr(app, &control.label);
        if let Some(Field { store: Store::Tune, path }) = &control.field {
            rows.push((label.clone(), path.clone()));
        }
        if let Widget::Range { high: Field { store: Store::Tune, path }, .. } = &control.widget {
            rows.push((format!("{label} · {}", tr(app, "Maximum")), path.clone()));
        }
        if let Some(toggle) = &control.toggle {
            if toggle.field.store == Store::Tune {
                rows.push((format!("{label} · {}", tr(app, "Enabled")), toggle.field.path.clone()));
            }
        }
    }
    rows
}

/// The hot keys of a screen: each `(action, control)` pair's `<action>_up` / `<action>_down` moves the
/// control's slider one step within its range, through the screen's own write (`set`). Whether a value moved.
pub fn step_hotkeys(
    app: &mut MyApp,
    pending: &dyn Fn(&str) -> bool,
    page: &Page,
    steps: &[(String, String)],
    set: fn(&mut MyApp, &str, &str, &str) -> Result<(), String>,
) -> Result<bool, String> {
    let mut moved = false;
    for (action, row) in steps {
        for (suffix, delta) in [("up", 1.0), ("down", -1.0)] {
            if !pending(&format!("{action}_{suffix}")) {
                continue;
            }
            let Some(control) = page.control(row) else { continue };
            let (Widget::Slider { scale }, Some(field)) = (&control.widget, &control.field) else { continue };
            let current = components::number(&read::field(app, field));
            let next = scale.snap(current.trunc() + delta);
            set(app, row, "", &num(next))?;
            moved = true;
        }
    }
    Ok(moved)
}

// ---- Main-loop work ---------------------------------------------------------------------------------------

/// The Shake hot keys while a game runs; the main loop's hot-key pass (which checks the game once) runs them
/// before the Motion keys, then [`apply_hotkeys`].
pub fn apply_shake_hotkeys(app: &mut MyApp, pending: &dyn Fn(&str) -> bool) -> Result<(), String> {
    if shake::apply_hotkeys(app, pending)? {
        super::super::refresh(app, &[Region::Main, Region::Save]);
    }
    Ok(())
}

/// The Belt, SIM3D and (while it is enabled) HapticR hot keys while a game runs, after the Motion keys.
pub fn apply_hotkeys(app: &mut MyApp, pending: &dyn Fn(&str) -> bool) -> Result<(), String> {
    let mut moved = belt::apply_hotkeys(app, pending)?;
    moved |= sim3d::apply_hotkeys(app, pending)?;
    moved |= hapticr::apply_hotkeys(app, pending)?;
    if moved {
        super::super::refresh(app, &[Region::Main, Region::Save]);
    }
    Ok(())
}

/// [`apply_shake_hotkeys`] for the main loop's shared application.
pub(crate) fn apply_shake_hotkeys_shared(shared: &std::sync::Mutex<MyApp>) -> Result<(), String> {
    let backend = crate::host::lock(shared).backend.clone();
    let pending = |action: &str| backend.ui_action_pending(action);
    apply_shake_hotkeys(&mut crate::host::lock(shared), &pending)
}

/// [`apply_hotkeys`] for the main loop's shared application.
pub(crate) fn apply_hotkeys_shared(shared: &std::sync::Mutex<MyApp>) -> Result<(), String> {
    let backend = crate::host::lock(shared).backend.clone();
    let pending = |action: &str| backend.ui_action_pending(action);
    apply_hotkeys(&mut crate::host::lock(shared), &pending)
}

/// A newly loaded profile (a game, vehicle or edit change): the routines start, and every tuning control
/// stores its value the way its widget holds it, with the service's check of the game's telemetry set-up (its
/// "Game Page" / "Instructions" notice) among the Motion rows (see [`super::reload`]). A required value that is
/// missing or unusable stops the rows after it.
pub fn on_profile_load(app: &mut MyApp) {
    if let Err(error) = super::reload::profile_load(app) {
        tracing::error!(%error, "profile load write-back");
    }
}

/// A game of the feature table that lacks `column`, for the tests of the feature rule.
#[cfg(test)]
pub fn game_without(column: &str) -> String {
    let features = crate::catalog::features();
    assert!(features.columns.iter().any(|c| c == column), "{column} is a feature column");
    features
        .games
        .iter()
        .find(|(id, columns)| {
            !columns.iter().any(|c| c == column) && openrigcue_config::catalog::Catalog::builtin().game(id).is_some()
        })
        .map(|(id, _)| id.clone())
        .expect("a game without the column")
}

/// The per-pass checks of these screens: the Downloaded Profiles selection waiting for its game or vehicle.
pub fn tick(app: &mut MyApp) {
    library::tick(app);
}

/// [`tick`] for the main loop's shared application: the service is asked without holding the application.
pub(crate) fn tick_shared(shared: &std::sync::Mutex<MyApp>) {
    library::tick_shared(shared);
}
