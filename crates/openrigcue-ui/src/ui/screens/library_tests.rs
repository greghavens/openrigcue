//! Tests of Tuning → Downloaded Profiles: the list by category and filter, the review and apply of a
//! profile for the running game, the switch to another game first, and the refusals.

use std::sync::{Arc, Mutex};

use openrigcue_config::synced_profiles::Target;
use serde_json::{json, Value};

use super::*;
use crate::app::tests::{loaded, FakeBackend};
use crate::backend::{Backend, Failure, Method};
use crate::host::UiApp;
use crate::ui::{dispatch, navigate};

/// A service whose library holds the given entries, all with the same content (or the same read failure).
struct LibraryBackend {
    inner: Arc<FakeBackend>,
    entries: Mutex<Vec<Entry>>,
    content: Mutex<Result<LegacyValue, String>>,
    /// How many times a profile's content was read.
    reads: Mutex<usize>,
    /// The vehicle of the loaded profile, when it differs from the session's.
    loaded_vehicle: Mutex<Option<LegacyValue>>,
    /// When set, a translated text is the text between `«` and `»`.
    marks_translations: Mutex<bool>,
}

impl Backend for LibraryBackend {
    fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value, Failure> {
        self.inner.call(method, path, body)
    }
    fn cached_profiles(&self) -> Result<Vec<Entry>, Failure> {
        Ok(self.entries.lock().unwrap().clone())
    }
    fn cached_profile(&self, id: &str) -> Result<(Entry, LegacyValue), Failure> {
        *self.reads.lock().unwrap() += 1;
        let entry = self.entries.lock().unwrap().iter().find(|e| e.id == id).cloned().unwrap();
        let content = self.content.lock().unwrap().clone().map_err(|detail| Failure { status: 500, detail })?;
        Ok((entry, content))
    }
    fn active_profile_vehicle(&self) -> Result<LegacyValue, Failure> {
        match self.loaded_vehicle.lock().unwrap().clone() {
            Some(vehicle) => Ok(vehicle),
            None => self.inner.active_profile_vehicle(),
        }
    }
    fn begin_profile_edit(&self, game: &str, vehicle: &LegacyValue) -> Result<(), Failure> {
        self.inner.call(Method::Post, "profile/edit", Some(json!({"game": game})))?;
        let mut docs = self.inner.docs.lock().unwrap();
        docs["status"]["session"]["game"] = json!(game);
        docs["status"]["session"]["vehicle"] = crate::backend::native_json(vehicle).unwrap();
        Ok(())
    }
    fn upload(&self, path: &str, bytes: Vec<u8>) -> Result<Value, Failure> {
        self.inner.upload(path, bytes)
    }
    fn fetch(&self, path: &str) -> Result<(Vec<u8>, String), Failure> {
        self.inner.fetch(path)
    }
    fn translate(&self, text: &str) -> String {
        if *self.marks_translations.lock().unwrap() {
            format!("«{text}»")
        } else {
            self.inner.translate(text)
        }
    }
    fn excluded_strings(&self) -> Vec<String> {
        Vec::new()
    }
}

fn entry(id: &str, game: &str, page: Category) -> Entry {
    Entry {
        id: id.into(),
        target: Target { game: game.into(), page, vehicle: Vehicle::Game(0) },
        source_vehicle: Some(Vehicle::Game(0)),
        author: "A. Driver".into(),
        description: "DOF REALITY [H3]".into(),
        version: "4".into(),
        state: FileState::Ready,
    }
}

/// The app with "ac" running and a library of a Shakers and a Belt profile for it and a Motion profile for
/// another game.
fn fixture() -> (MyApp, Arc<LibraryBackend>) {
    let (mut app, backend) = library(vec![
        entry("shakers", "ac", Category::Shakers),
        entry("belt", "ac", Category::Belt),
        entry("motion", "acc", Category::Motion),
    ]);
    navigate(&mut app, Screen::DownloadedProfiles, 0);
    (app, backend)
}

/// The app with "ac" running, HapticR enabled and a library of the given entries whose content is the
/// current tune; no screen is opened.
fn library(entries: Vec<Entry>) -> (MyApp, Arc<LibraryBackend>) {
    let inner = FakeBackend::new();
    inner.docs.lock().unwrap()["status"]["session"]["game"] = json!("ac");
    inner.docs.lock().unwrap()["settings"]["app"]["enable_hapticr"] = json!(true);
    let mut app = loaded(inner.clone());
    let content = LegacyValue::from(inner.docs.lock().unwrap()["tune"].clone());
    let backend = Arc::new(LibraryBackend {
        inner,
        entries: Mutex::new(entries),
        content: Mutex::new(Ok(content)),
        reads: Mutex::new(0),
        loaded_vehicle: Mutex::new(None),
        marks_translations: Mutex::new(false),
    });
    app.backend = backend.clone();
    (app, backend)
}

/// The app at Downloaded Profiles (category `section`) with one Ready entry "entry" of `category` for "ac".
fn single(category: Category, section: usize) -> (MyApp, Arc<LibraryBackend>) {
    let (mut app, backend) = library(vec![entry("entry", "ac", category)]);
    navigate(&mut app, Screen::DownloadedProfiles, section);
    (app, backend)
}

fn reads(backend: &LibraryBackend) -> usize {
    *backend.reads.lock().unwrap()
}

/// The newest notice's message.
fn last_message(app: &MyApp) -> String {
    app.ui.notices.history.last().map(|n| n.message.clone()).unwrap_or_default()
}

/// The profile saves the service received.
fn saves(backend: &LibraryBackend) -> Vec<Value> {
    let calls = backend.inner.calls.lock().unwrap();
    calls.iter().filter(|(_, path, _)| path == "profile/save").map(|(_, _, body)| body.clone().unwrap()).collect()
}

/// Selects the first entry, then applies it from the review.
fn apply_first(app: &mut MyApp) {
    dispatch(app, "action", "Downloaded Profiles:select:0", "").unwrap();
    assert!(matches!(tuning::open_one(app), Some(Dialog::LibraryReview(_))), "{:?}", app.ui.dialog);
    dispatch(app, "dialog-ok", "", "").unwrap();
}

const SAVE_ALERT: &str = "Confirm to adopt this cloud share profile in place of your current one; test it before saving. Cancel throws the changes away (PROFILE IN USE box).";

fn tune(backend: &LibraryBackend) -> Value {
    backend.inner.docs.lock().unwrap()["tune"].clone()
}

#[test]
fn the_list_follows_the_category_and_the_filter() {
    let (mut app, _backend) = fixture();
    let html = main(&app);
    assert!(html.contains(">All categories</option>") && html.contains(">Shakers</option>"));
    assert!(html.contains("DOF REALITY [H3]") && html.contains(">My profiles<"));
    assert_eq!(html.matches("Downloaded Profiles:select:").count(), 3);
    dispatch(&mut app, "section", "", &category_index(Category::Belt).to_string()).unwrap();
    assert_eq!(main(&app).matches("Downloaded Profiles:select:").count(), 1);
    dispatch(&mut app, "section", "", "0").unwrap();
    dispatch(&mut app, "set", "Downloaded Profiles:filter", "motion").unwrap();
    assert_eq!(main(&app).matches("Downloaded Profiles:select:").count(), 1);
    dispatch(&mut app, "set", "Downloaded Profiles:filter", "nothing like it").unwrap();
    assert!(main(&app).contains("No downloaded profiles here yet."));
    drop(app.take_threads());
}

#[test]
fn a_profile_for_the_running_game_is_reviewed_then_applied_to_its_category_only() {
    let (mut app, backend) = fixture();
    let mut content = tune(&backend);
    content["shaker_effects"]["gear"] = json!(7);
    content["speed_factor"] = json!(9);
    *backend.content.lock().unwrap() = Ok(content.into());
    let before = tune(&backend);
    dispatch(&mut app, "action", "Downloaded Profiles:select:0", "").unwrap();
    assert_eq!(tune(&backend), before, "nothing is applied before the review");
    let Some(Dialog::LibraryReview(review)) = tuning::open_one(&app).cloned() else { panic!("review") };
    assert!(review.question.contains("A. Driver") && review.question.contains("Shakers"));
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    let after = tune(&backend);
    assert_eq!(after["shaker_effects"]["gear"], json!(7));
    assert_eq!(after["speed_factor"], before["speed_factor"], "other categories stay");
    assert!(app.ui.tuning.library_applied, "Save asks to adopt the downloaded profile");
    assert_eq!(app.ui.screen, Screen::Shake);
    drop(app.take_threads());
}

#[test]
fn another_game_switches_the_edit_first_and_reviews_once_loaded() {
    let (mut app, backend) = fixture();
    dispatch(&mut app, "action", "Downloaded Profiles:select:2", "").unwrap();
    assert!(backend.inner.calls.lock().unwrap().iter().any(|(m, p, _)| *m == Method::Post && p == "profile/edit"));
    assert!(matches!(tuning::open_one(&app), Some(Dialog::LibraryPending(_))));
    backend.inner.docs.lock().unwrap()["status"]["profile"]["game"] = json!("acc");
    tick(&mut app);
    assert_eq!(app.ui.screen, Screen::Motion);
    assert!(matches!(tuning::open_one(&app), Some(Dialog::LibraryReview(_))));
    drop(app.take_threads());
}

#[test]
fn a_waiting_selection_survives_another_dialog_and_comes_back_when_it_closes() {
    let (mut app, backend) = fixture();
    dispatch(&mut app, "action", "Downloaded Profiles:select:2", "").unwrap();
    let Some(Dialog::LibraryPending(pending)) = tuning::open_one(&app).cloned() else { panic!("waiting") };
    crate::ui::dialogs::open(
        &mut app,
        crate::ui::dialogs::Dialog::Message { title: "Note".into(), text: "Another dialog".into() },
    );
    assert_eq!(app.ui.tuning.library_behind.as_ref(), Some(&pending), "the selection waits behind");
    // The vehicle's controls are still loading: nothing advances.
    *backend.loaded_vehicle.lock().unwrap() = Some(LegacyValue::Str("another vehicle".into()));
    tick(&mut app);
    assert!(matches!(app.ui.dialog, Some(crate::ui::dialogs::Dialog::Message { .. })), "still loading");
    dispatch(&mut app, "dialog-close", "", "").unwrap();
    assert_eq!(tuning::open_one(&app), Some(&Dialog::LibraryPending(pending.clone())), "it comes back");
    assert!(crate::ui::render(&app, Region::Dialog).contains("Loading the selected profile. Close to cancel."));
    *backend.loaded_vehicle.lock().unwrap() = None;
    tick(&mut app);
    assert_eq!(app.ui.screen, Screen::Motion);
    assert!(matches!(tuning::open_one(&app), Some(Dialog::LibraryReview(_))));
    // A selection waiting behind is dropped when the edit ends.
    crate::ui::dialogs::close(&mut app);
    app.ui.tuning.library_behind = Some(pending);
    cancel(&mut app);
    assert!(app.ui.tuning.library_behind.is_none());
    crate::ui::dialogs::close(&mut app);
    assert!(app.ui.dialog.is_none(), "nothing comes back after the cancel");
    drop(app.take_threads());
}

/// A selection waiting behind another dialog still advances: once its game loads its screen opens and its
/// review comes to the front; the other dialog stays behind the review and comes back when that closes.
#[test]
fn a_selection_waiting_behind_another_dialog_opens_its_review_in_front_once_loaded() {
    use crate::ui::dialogs::Dialog as Any;
    for shared in [false, true] {
        let (mut app, backend) = fixture();
        dispatch(&mut app, "action", "Downloaded Profiles:select:2", "").unwrap();
        let note = Any::Message { title: "Note".into(), text: "Another dialog".into() };
        crate::ui::dialogs::open(&mut app, note.clone());
        assert!(app.ui.tuning.library_behind.is_some(), "the selection waits behind");
        *backend.loaded_vehicle.lock().unwrap() = Some(LegacyValue::Str("another vehicle".into()));
        tick(&mut app);
        assert_eq!(app.ui.dialog.as_ref(), Some(&note), "still loading");
        assert!(app.ui.tuning.library_behind.is_some());
        // The game and vehicle load while the selection is behind the other dialog.
        *backend.loaded_vehicle.lock().unwrap() = None;
        let mut app = if shared {
            let shared = Mutex::new(app);
            tick_shared(&shared);
            shared.into_inner().unwrap()
        } else {
            tick(&mut app);
            app
        };
        assert_eq!(app.ui.screen, Screen::Motion, "shared {shared}");
        assert!(matches!(tuning::open_one(&app), Some(Dialog::LibraryReview(_))), "the review is in front");
        assert!(crate::ui::render(&app, Region::Dialog).contains("Apply Downloaded Profile?"));
        assert!(app.ui.tuning.library_behind.is_none(), "the selection advanced");
        dispatch(&mut app, "dialog-close", "", "").unwrap();
        assert_eq!(app.ui.dialog, Some(note), "the other dialog comes back");
        dispatch(&mut app, "dialog-close", "", "").unwrap();
        assert!(app.ui.dialog.is_none(), "nothing else was waiting");
        drop(app.take_threads());
    }
}

#[test]
fn another_game_is_refused_while_telemetry_runs() {
    let (mut app, backend) = fixture();
    backend.inner.docs.lock().unwrap()["status"]["session"]["listener"] = json!("udp");
    dispatch(&mut app, "action", "Downloaded Profiles:select:2", "").unwrap();
    assert!(app.ui.dialog.is_none());
    let notice = app.ui.notices.history.last().unwrap();
    assert_eq!(notice.message, "Stop game telemetry before editing another game or vehicle.");
    drop(app.take_threads());
}

#[test]
fn the_library_texts_have_entries() {
    let (mut app, backend) = fixture();
    backend.inner.translations.lock().unwrap().clear();
    for section in 0..6 {
        app.ui.section = section;
        main(&app);
    }
    app.ui.tuning.library.clear();
    main(&app);
    tuning::open_dialog(
        &mut app,
        Dialog::LibraryPending(Pending { id: "x".into(), target: entry("x", "ac", Category::Belt).target }),
    );
    crate::ui::render(&app, crate::ui::Region::Dialog);
    let looked_up = backend.inner.translations.lock().unwrap().iter().cloned().collect();
    assert_eq!(crate::app::tests::texts_without_entry(&looked_up, &[]), Vec::<String>::new());
    drop(app.take_threads());
}

/// The fixed texts of a row (vehicle label, profile type, state), the filter's hint and the review question
/// go through the translation; names and free text (game, author, description, a named vehicle, a
/// validation message) are shown as they are.
#[test]
fn fixed_texts_are_translated_and_names_are_not() {
    let (mut app, backend) = single(Category::Belt, category_index(Category::Belt));
    *backend.marks_translations.lock().unwrap() = true;
    let html = main(&app);
    assert!(html.contains("<strong translate=\"no\">Assetto Corsa</strong>"), "{html}");
    assert!(html.contains("«All vehicles» · «Belt» · A. Driver · DOF REALITY [H3] · «Ready»"), "{html}");
    assert!(html.contains("placeholder=\"«Filter game, vehicle, author, description or category»\""));
    dispatch(&mut app, "action", "Downloaded Profiles:select:0", "").unwrap();
    let Some(Dialog::LibraryReview(review)) = tuning::open_one(&app).cloned() else { panic!("review") };
    assert_eq!(
        review.question,
        "«Apply this «Belt» profile by A. Driver to Assetto Corsa — «All vehicles»? Other settings stay unchanged.»"
    );
    dispatch(&mut app, "dialog-close", "", "").unwrap();
    // A named vehicle, a missing origin and a validation message are not looked up.
    {
        let mut entries = backend.entries.lock().unwrap();
        entries[0].source_vehicle = Some(Vehicle::Named("Origin car".into()));
        entries[0].state = FileState::Invalid("bad tune".into());
        let mut other = entry("other", "ac", Category::Belt);
        other.source_vehicle = None;
        other.state = FileState::Missing;
        entries.push(other);
    }
    dispatch(&mut app, "action", "Downloaded Profiles:refresh:", "").unwrap();
    let html = main(&app);
    assert!(html.contains("Origin car · «Belt» · A. Driver · DOF REALITY [H3] · bad tune"), "{html}");
    assert!(html.contains("«Unavailable» · «Belt» · A. Driver · DOF REALITY [H3] · «Missing file»"), "{html}");
    drop(app.take_threads());
}

/// The row shows the vehicle the profile was made for; the review names the current game and vehicle it
/// applies to; closing the review writes nothing.
#[test]
fn the_origin_vehicle_is_shown_and_the_review_names_the_current_vehicle() {
    let (mut app, backend) = single(Category::Belt, category_index(Category::Belt));
    backend.entries.lock().unwrap()[0].source_vehicle = Some(Vehicle::Named("Origin car".into()));
    backend.inner.docs.lock().unwrap()["status"]["session"]["vehicle"] = json!("Destination car");
    *backend.loaded_vehicle.lock().unwrap() = Some("Destination car".into());
    dispatch(&mut app, "action", "Downloaded Profiles:refresh:", "").unwrap();
    backend.inner.calls.lock().unwrap().clear();
    assert!(main(&app).contains("Origin car · Belt"));
    dispatch(&mut app, "action", "Downloaded Profiles:select:0", "").unwrap();
    let Some(Dialog::LibraryReview(review)) = tuning::open_one(&app).cloned() else { panic!("review") };
    assert!(review.question.contains("Assetto Corsa — Destination car?"), "{}", review.question);
    assert!(!review.question.contains("Origin car") && !review.question.contains("version"));
    dispatch(&mut app, "dialog-close", "", "").unwrap();
    assert!(app.ui.dialog.is_none());
    assert!(backend.inner.patches("profile").is_empty());
    drop(app.take_threads());
}

/// Each tuning screen with library profiles opens Downloaded Profiles at its category from its Actions; the
/// list shows the library's game, vehicle, type, author, description and state without reading a profile's
/// content or contacting the network.
#[test]
fn each_tuning_screen_opens_the_library_at_its_category() {
    for (category, screen) in [
        (Category::Motion, Screen::Motion),
        (Category::Shakers, Screen::Shake),
        (Category::Belt, Screen::Belt),
        (Category::Rumble, Screen::Sim3d),
        (Category::Haptic, Screen::HapticR),
    ] {
        let (mut app, backend) = library(vec![entry("entry", "ac", category)]);
        navigate(&mut app, screen, 0);
        let action = format!("{}:@library", screen.key());
        assert!(crate::ui::screens::main(&app).contains(&format!("data-arg=\"{action}")), "{screen:?}");
        dispatch(&mut app, "action", &action, "").unwrap();
        assert_eq!((app.ui.screen, app.ui.section), (Screen::DownloadedProfiles, category_index(category)));
        let html = main(&app);
        assert!(html.contains("<strong translate=\"no\">Assetto Corsa</strong>"), "{category:?}");
        assert!(
            html.contains(&format!("All vehicles · {} · A. Driver · DOF REALITY [H3] · Ready", profile_type(category))),
            "{html}"
        );
        assert!(!html.contains(" · 4"), "the version is not shown");
        assert!(html.contains("Downloaded Profiles:select:0\">"), "the entry can be selected");
        assert_eq!(reads(&backend), 0);
        assert!(!backend.inner.calls.lock().unwrap().iter().any(|(_, path, _)| path.contains("cloud")));
        drop(app.take_threads());
    }
}

/// For each category: nothing changes before the review is confirmed; Apply writes the category's value
/// (a whole number stays one) and leaves the other categories, and nothing is saved.
#[test]
fn apply_writes_only_its_category_after_the_review_and_saves_nothing() {
    for (category, table, key) in [
        (Category::Belt, "", "belt_surge"),
        (Category::Shakers, "shaker_effects", "gear"),
        (Category::Rumble, "rumble_effects", "brake_gear"),
        (Category::Haptic, "haptic_effects", "wheel_gear"),
    ] {
        let (mut app, backend) = single(category, category_index(category));
        let before = tune(&backend);
        let mut content = before.clone();
        if table.is_empty() {
            content[key] = json!(17);
        } else {
            content[table][key] = json!(17);
        }
        *backend.content.lock().unwrap() = Ok(content.into());
        dispatch(&mut app, "action", "Downloaded Profiles:select:0", "").unwrap();
        assert_eq!(tune(&backend), before, "{category:?}: nothing before the review");
        dispatch(&mut app, "dialog-ok", "", "").unwrap();
        let after = tune(&backend);
        let actual = if table.is_empty() { &after[key] } else { &after[table][key] };
        assert_eq!(actual, &json!(17), "{category:?} {key}");
        assert_eq!(after["motion_max_power"], before["motion_max_power"]);
        assert_eq!(after["speed_factor"], before["speed_factor"]);
        assert!(!backend
            .inner
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|(_, path, _)| path == "profile/save" || path == "settings/save"));
        assert_eq!(reads(&backend), 2, "the review and the apply each read the file");
        assert_eq!(last_message(&app), "Profile applied. Click Save to keep these settings.");
        drop(app.take_threads());
    }
}

/// Apply reads the file and checks the game again: an edited file or another game writes nothing and is
/// explained.
#[test]
fn apply_checks_the_file_and_the_game_again_before_writing() {
    for changed_file in [false, true] {
        let (mut app, backend) = single(Category::Belt, category_index(Category::Belt));
        dispatch(&mut app, "action", "Downloaded Profiles:select:0", "").unwrap();
        let before = tune(&backend);
        if changed_file {
            *backend.content.lock().unwrap() = Err("The downloaded file was edited".into());
        } else {
            backend.inner.docs.lock().unwrap()["status"]["session"]["game"] = json!("gt7");
        }
        dispatch(&mut app, "dialog-ok", "", "").unwrap();
        assert_eq!(tune(&backend), before);
        let expected = if changed_file {
            "The downloaded file was edited"
        } else {
            "The active game or vehicle changed; select the profile again."
        };
        assert_eq!(last_message(&app), expected);
        assert!(!app.ui.tuning.library_applied);
        drop(app.take_threads());
    }
}

/// A missing, locally edited or invalid file is listed with its state but cannot be selected.
#[test]
fn missing_edited_and_invalid_files_are_listed_but_cannot_be_selected() {
    for (state, text) in [
        (FileState::Missing, "Missing file"),
        (FileState::Edited, "Locally edited — not applied"),
        (FileState::Invalid("Invalid profile ZIP".into()), "Invalid profile ZIP"),
    ] {
        let (mut app, backend) = library(vec![Entry { state, ..entry("entry", "ac", Category::Motion) }]);
        navigate(&mut app, Screen::DownloadedProfiles, 0);
        let html = main(&app);
        assert!(html.contains(text), "{html}");
        assert!(html.contains("Downloaded Profiles:select:0\" disabled>"), "{html}");
        assert_eq!(reads(&backend), 0);
        drop(app.take_threads());
    }
}

/// Motion applies the file's motion values and leaves shakers, belt and wind.
#[test]
fn motion_applies_its_values_and_leaves_shakers_belt_and_wind() {
    let (mut app, backend) = single(Category::Motion, 0);
    let before = tune(&backend);
    let mut content = before.clone();
    content["motion_max_power"] = json!(63);
    content["motion_enhanced_gforces"] = json!(1);
    content["gear_smoothing_factor"] = json!(21);
    content["min_traction_loss_activation"] = json!(10);
    content["motion_smoothing_factor"] = json!(42);
    content["motion_boost_factor"] = json!(7);
    content["motion_effect_smoothing_factor"] =
        json!({"surge":0,"sway":0,"traction_loss":0,"traction_loss_front":0,"roll":0,"heave":0,"pitch":0,"yaw":0});
    apply_to_current(&mut app, Category::Motion, &content.into()).unwrap();
    let after = tune(&backend);
    assert_eq!(after["motion_max_power"], json!(63));
    assert_eq!(after["motion_smoothing_factor"], json!(42));
    assert_eq!(after["shaker_effects"], before["shaker_effects"]);
    assert_eq!(after["belt_surge"], before["belt_surge"]);
    assert_eq!(after["power_wind_max_speed_perc"], before["power_wind_max_speed_perc"]);
    drop(app.take_threads());
}

/// Applied values go through their controls: sliders round to their steps within their ranges, spin boxes keep
/// within theirs, and a switch takes only a yes or no word (so "no" is off and other words change nothing).
#[test]
fn applied_values_go_through_their_controls() {
    // Shakers.
    let (mut app, backend) = single(Category::Shakers, 0);
    backend.inner.docs.lock().unwrap()["tune"]["shaker_switch"]["engine"] = json!(true);
    backend.inner.docs.lock().unwrap()["tune"]["shaker_switch"]["gear"] = json!(true);
    app.refresh_docs();
    let mut content = tune(&backend);
    content["shaker_effects"]["engine"] = json!(25);
    content["shaker_effects"]["gear"] = json!(7.6);
    content["shaker_switch"]["engine"] = json!("no");
    content["shaker_switch"]["gear"] = json!("maybe");
    apply_to_current(&mut app, Category::Shakers, &content.into()).unwrap();
    let after = tune(&backend);
    assert_eq!(after["shaker_effects"]["engine"], json!(20));
    assert_eq!(after["shaker_effects"]["gear"], json!(8));
    assert_eq!(after["shaker_switch"]["engine"], json!(false), "no is off");
    assert_eq!(after["shaker_switch"]["gear"], json!(true), "a word that is no yes or no changes nothing");
    drop(app.take_threads());
    // Belt: the yes / no switches and the sliders' ranges.
    let (mut app, backend) = single(Category::Belt, 0);
    backend.inner.docs.lock().unwrap()["tune"]["belt_surge_enabled"] = json!(0);
    backend.inner.docs.lock().unwrap()["tune"]["belt_heave_reverse"] = json!(1);
    app.refresh_docs();
    let mut content = tune(&backend);
    content["belt_surge_enabled"] = json!("yes");
    content["belt_heave_reverse"] = json!("no");
    content["belt_surge"] = json!(0);
    content["belt_heave_smoothing"] = json!(140);
    apply_to_current(&mut app, Category::Belt, &content.into()).unwrap();
    let after = tune(&backend);
    assert_eq!(after["belt_surge_enabled"], json!(1));
    assert_eq!(after["belt_heave_reverse"], json!(0));
    assert_eq!(after["belt_surge"], json!(1));
    assert_eq!(after["belt_heave_smoothing"], json!(100));
    drop(app.take_threads());
    // SIM3D: an effect's switch that is no yes or no word is switched on; the fade range is ordered.
    let (mut app, backend) = single(Category::Rumble, 0);
    backend.inner.docs.lock().unwrap()["tune"]["rumble_switch"]["brake_gear"] = json!(false);
    app.refresh_docs();
    let mut content = tune(&backend);
    content["rumble_switch"]["brake_gear"] = json!("maybe");
    content["rumble_effects"]["brake_gear"] = json!(33);
    content["rumble_effects"]["rpm_start_fading"] = json!(90);
    content["rumble_effects"]["rpm_stop_fading"] = json!(80);
    apply_to_current(&mut app, Category::Rumble, &content.into()).unwrap();
    let after = tune(&backend);
    assert_eq!(after["rumble_switch"]["brake_gear"], json!(true));
    assert_eq!(after["rumble_effects"]["brake_gear"], json!(20));
    assert_eq!(
        (after["rumble_effects"]["rpm_start_fading"].clone(), after["rumble_effects"]["rpm_stop_fading"].clone()),
        (json!(80), json!(90))
    );
    drop(app.take_threads());
    // Motion: Max Telemetry within its range, "no" off, an unknown axis type follows the rate.
    let (mut app, backend) = single(Category::Motion, 0);
    {
        let mut docs = backend.inner.docs.lock().unwrap();
        docs["tune"]["motion_switch"]["sway"] = json!(true);
        docs["tune"]["max_telemetry_vars"]["roll_rate"] = json!(9);
    }
    app.refresh_docs();
    let mut content = tune(&backend);
    content["motion_switch"]["sway"] = json!("no");
    content["max_telemetry_vars"]["sway"] = json!(5000);
    content["motion_effects"]["sway"] = json!(30);
    content["motion_effects"]["roll_type"] = json!("sideways");
    content["min_traction_loss_activation"] = json!(-9);
    apply_to_current(&mut app, Category::Motion, &content.into()).unwrap();
    let after = tune(&backend);
    assert_eq!(after["motion_switch"]["sway"], json!(false));
    assert_eq!(after["max_telemetry_vars"]["sway"], json!(1800));
    assert_eq!(after["motion_effects"]["sway"].as_f64(), Some(20.0), "the service keeps effects as floats");
    assert_eq!(after["motion_effects"]["roll_type"], json!("rate"));
    assert_eq!(after["min_traction_loss_activation"].as_f64(), Some(0.0));
    assert!(!app.ui.tuning.motion_hidden.contains("roll_type"));
    drop(app.take_threads());
}

/// Profiles offers the library; an empty library says so, and nothing is read.
#[test]
fn profiles_opens_the_library_and_an_empty_library_says_so() {
    let (mut app, backend) = library(Vec::new());
    navigate(&mut app, Screen::Profiles, 0);
    assert!(crate::ui::screens::main(&app).contains("data-act=\"profile-library\""));
    dispatch(&mut app, "profile-library", "", "").unwrap();
    assert_eq!((app.ui.screen, app.ui.section), (Screen::DownloadedProfiles, 0));
    assert!(
        main(&app).contains("No downloaded profiles here yet. Sync your library first; browsing never contacts SRS.")
    );
    assert_eq!(reads(&backend), 0);
    drop(app.take_threads());
}

/// The filter hides and shows rows and keeps its text; closing the review writes nothing.
#[test]
fn the_filter_and_closing_the_review_change_no_values() {
    let (mut app, backend) = library(vec![
        entry("entry", "ac", Category::Belt),
        Entry { author: "Second Author".into(), ..entry("other", "ac", Category::Motion) },
    ]);
    navigate(&mut app, Screen::DownloadedProfiles, 0);
    let before = tune(&backend);
    dispatch(&mut app, "set", "Downloaded Profiles:filter", "Second").unwrap();
    let html = main(&app);
    assert!(html.contains("value=\"Second\""), "the filter keeps its text");
    assert!(!html.contains("Downloaded Profiles:select:0") && html.contains("Downloaded Profiles:select:1"));
    dispatch(&mut app, "set", "Downloaded Profiles:filter", "").unwrap();
    let html = main(&app);
    assert!(html.contains("Downloaded Profiles:select:0") && html.contains("Downloaded Profiles:select:1"));
    dispatch(&mut app, "action", "Downloaded Profiles:select:0", "").unwrap();
    assert!(matches!(tuning::open_one(&app), Some(Dialog::LibraryReview(_))));
    dispatch(&mut app, "dialog-close", "", "").unwrap();
    assert!(app.ui.dialog.is_none());
    assert_eq!(tune(&backend), before);
    assert_eq!(reads(&backend), 1);
    drop(app.take_threads());
}

/// A file without the category's values writes the defaults: shakers 0, SIM3D and HapticR effects 10, as
/// whole numbers; other categories stay.
#[test]
fn missing_values_get_the_category_defaults() {
    for (category, table, key, expected) in [
        (Category::Shakers, "shaker_effects", "gear", json!(0)),
        (Category::Rumble, "rumble_effects", "brake_gear", json!(10)),
        (Category::Haptic, "haptic_effects", "wheel_gear", json!(10)),
    ] {
        let (mut app, backend) = single(category, 0);
        let before = tune(&backend);
        apply_to_current(&mut app, category, &LegacyValue::from(json!({}))).unwrap();
        let after = tune(&backend);
        assert_eq!(after["belt_surge"], before["belt_surge"]);
        assert_eq!(after["motion_max_power"], before["motion_max_power"]);
        // The number is what counts: a value its control already holds is not written again.
        assert_eq!(after[table][key].as_f64(), expected.as_f64(), "{category:?}");
        drop(app.take_threads());
    }
}

/// In stand-by a profile for a game switches the edit to it and writes nothing; the review opens once its
/// controls have loaded, and a closed review does not come back.
#[test]
fn a_profile_for_another_game_only_switches_the_edit() {
    let (mut app, backend) = single(Category::Belt, category_index(Category::Belt));
    backend.inner.docs.lock().unwrap()["status"]["session"]["game"] = json!("none");
    let before = tune(&backend);
    dispatch(&mut app, "action", "Downloaded Profiles:select:0", "").unwrap();
    assert_eq!(reads(&backend), 0);
    assert_eq!(tune(&backend), before);
    assert!(backend.inner.calls.lock().unwrap().iter().any(|(m, p, _)| *m == Method::Post && p == "profile/edit"));
    assert!(matches!(tuning::open_one(&app), Some(Dialog::LibraryPending(_))));
    assert!(app.ui.notices.history.is_empty(), "{:?}", crate::app::tests::notice_texts(&app));
    tick(&mut app);
    assert_eq!(app.ui.screen, Screen::Belt);
    assert!(matches!(tuning::open_one(&app), Some(Dialog::LibraryReview(_))));
    assert_eq!(reads(&backend), 1);
    assert_eq!(tune(&backend), before);
    dispatch(&mut app, "dialog-close", "", "").unwrap();
    tick(&mut app);
    assert!(app.ui.dialog.is_none());
    assert!(app.ui.notices.history.is_empty(), "{:?}", crate::app::tests::notice_texts(&app));
    drop(app.take_threads());
}

/// Closing the waiting dialog cancels the selection: no review opens later; a second review replaces the
/// first.
#[test]
fn closing_the_waiting_dialog_cancels_the_selection() {
    let (mut app, backend) = single(Category::Belt, category_index(Category::Belt));
    backend.inner.docs.lock().unwrap()["status"]["session"]["game"] = json!("none");
    dispatch(&mut app, "action", "Downloaded Profiles:select:0", "").unwrap();
    assert!(matches!(tuning::open_one(&app), Some(Dialog::LibraryPending(_))));
    dispatch(&mut app, "dialog-close", "", "").unwrap();
    tick(&mut app);
    assert!(app.ui.dialog.is_none());
    assert_eq!(app.ui.screen, Screen::DownloadedProfiles);
    assert_eq!(reads(&backend), 0);
    // The game has loaded: the review opens at once, and once only.
    dispatch(&mut app, "action", "Downloaded Profiles:select:0", "").unwrap();
    dispatch(&mut app, "action", "Downloaded Profiles:select:0", "").unwrap();
    assert!(matches!(tuning::open_one(&app), Some(Dialog::LibraryReview(_))));
    assert_eq!(crate::ui::render(&app, crate::ui::Region::Dialog).matches("<dialog").count(), 1);
    drop(app.take_threads());
}

/// While another game sends telemetry an entry for a different game is disabled with the reason, and a
/// select is refused again without switching the edit.
#[test]
fn another_running_game_disables_the_entry_and_refuses_the_select() {
    let (mut app, backend) = single(Category::Belt, category_index(Category::Belt));
    {
        let mut docs = backend.inner.docs.lock().unwrap();
        docs["status"]["session"]["game"] = json!("gt7");
        docs["status"]["session"]["listener"] = json!("gt7");
    }
    let html = main(&app);
    assert!(html.contains(
        "Downloaded Profiles:select:0\" title=\"Stop game telemetry before editing another game or vehicle.\" disabled>"
    ), "{html}");
    dispatch(&mut app, "action", "Downloaded Profiles:select:0", "").unwrap();
    assert_eq!(last_message(&app), "Stop game telemetry before editing another game or vehicle.");
    assert!(!backend.inner.calls.lock().unwrap().iter().any(|(_, path, _)| path == "profile/edit"));
    assert!(app.ui.dialog.is_none());
    drop(app.take_threads());
}

/// A profile for the current vehicle waits until that vehicle's profile has loaded, at the select and again
/// at Apply.
#[test]
fn a_vehicle_profile_waits_for_the_loaded_vehicle() {
    let (mut app, backend) = single(Category::Belt, category_index(Category::Belt));
    backend.entries.lock().unwrap()[0].target.vehicle = Vehicle::Named("new car".into());
    backend.inner.docs.lock().unwrap()["status"]["session"]["vehicle"] = json!("new car");
    *backend.loaded_vehicle.lock().unwrap() = Some("old car".into());
    dispatch(&mut app, "action", "Downloaded Profiles:refresh:", "").unwrap();
    let loading = "The selected vehicle’s tuning controls are still loading. Try again after they refresh.";
    dispatch(&mut app, "action", "Downloaded Profiles:select:0", "").unwrap();
    assert!(app.ui.dialog.is_none());
    assert_eq!(last_message(&app), loading);
    *backend.loaded_vehicle.lock().unwrap() = Some("new car".into());
    dispatch(&mut app, "action", "Downloaded Profiles:select:0", "").unwrap();
    assert!(matches!(tuning::open_one(&app), Some(Dialog::LibraryReview(_))));
    let before = tune(&backend);
    *backend.loaded_vehicle.lock().unwrap() = Some("old car".into());
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert_eq!(tune(&backend), before);
    assert_eq!(last_message(&app), loading);
    assert!(!app.ui.tuning.library_applied);
    drop(app.take_threads());
}

/// A service that waits once, inside the current game's read, until the test lets it go on.
struct PausedLibrary {
    inner: Arc<LibraryBackend>,
    wait: Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
}

impl Backend for PausedLibrary {
    fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value, Failure> {
        self.inner.call(method, path, body)
    }
    fn game_value(&self) -> Result<LegacyValue, Failure> {
        if let Some((started, release)) = self.wait.lock().unwrap().take() {
            started.send(()).unwrap();
            release.recv().unwrap();
        }
        self.inner.game_value()
    }
    fn cached_profiles(&self) -> Result<Vec<Entry>, Failure> {
        self.inner.cached_profiles()
    }
    fn cached_profile(&self, id: &str) -> Result<(Entry, LegacyValue), Failure> {
        self.inner.cached_profile(id)
    }
    fn active_profile_vehicle(&self) -> Result<LegacyValue, Failure> {
        self.inner.active_profile_vehicle()
    }
    fn upload(&self, path: &str, bytes: Vec<u8>) -> Result<Value, Failure> {
        self.inner.upload(path, bytes)
    }
    fn fetch(&self, path: &str) -> Result<(Vec<u8>, String), Failure> {
        self.inner.fetch(path)
    }
    fn translate(&self, text: &str) -> String {
        self.inner.translate(text)
    }
    fn excluded_strings(&self) -> Vec<String> {
        Vec::new()
    }
}

/// The main loop's check of a waiting selection reads the game without holding the application, so a timer
/// runs meanwhile; a selection cancelled meanwhile does not open its review.
#[test]
fn the_waiting_check_leaves_the_app_free_and_respects_a_cancel() {
    use std::time::Duration;
    for cancelled in [false, true] {
        let (mut app, inner) = single(Category::Belt, category_index(Category::Belt));
        let target = entry("entry", "ac", Category::Belt).target;
        tuning::open_dialog(&mut app, Dialog::LibraryPending(Pending { id: "entry".into(), target }));
        let (started_tx, started) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        app.backend = Arc::new(PausedLibrary { inner, wait: Mutex::new(Some((started_tx, release_rx))) });
        let shared = Arc::new(Mutex::new(app));
        crate::host::lock(&shared).attach_shared(Arc::downgrade(&shared));
        let work = shared.clone();
        let worker = std::thread::spawn(move || tick_shared(&work));
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        let (ran_tx, ran_rx) = std::sync::mpsc::channel();
        let available = if let Ok(mut app) = shared.try_lock() {
            app.timer(Duration::ZERO, move |_| {
                ran_tx.send(()).unwrap();
            });
            if cancelled {
                crate::ui::dialogs::close(&mut app);
            }
            true
        } else {
            false
        };
        let ran = available && ran_rx.recv_timeout(Duration::from_secs(2)).is_ok();
        release.send(()).unwrap();
        worker.join().unwrap();
        assert!(available, "the check held the application");
        assert!(ran, "the check blocked a timer");
        let mut app = crate::host::lock(&shared);
        assert_eq!(matches!(tuning::open_one(&app), Some(Dialog::LibraryReview(_))), !cancelled);
        drop(app.take_threads());
    }
}

/// After a library profile is applied, Save asks to adopt it even at game level (with the game as the only
/// choice) and saves once confirmed; the next game-level Save is direct again.
#[test]
fn an_applied_library_profile_is_confirmed_once_at_save() {
    let (mut app, backend) = single(Category::Belt, category_index(Category::Belt));
    assert_eq!(app.ui.session.vehicle, "");
    apply_first(&mut app);
    assert!(app.ui.tuning.library_applied);
    dispatch(&mut app, "save", "", "").unwrap();
    assert!(matches!(app.ui.dialog, Some(crate::ui::dialogs::Dialog::SaveChoice { library: true, .. })));
    let dialog = crate::ui::dialogs::render(&app);
    assert!(dialog.contains(SAVE_ALERT), "{dialog}");
    assert!(dialog.contains(">Game: ac<") && !dialog.contains("Vehicle: "), "{dialog}");
    assert!(saves(&backend).is_empty());
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert_eq!(saves(&backend), [json!({"level": "game"})]);
    assert!(!app.ui.tuning.library_applied);
    dispatch(&mut app, "save", "", "").unwrap();
    assert!(app.ui.dialog.is_none());
    assert!(!crate::ui::dialogs::render(&app).contains(SAVE_ALERT));
    assert_eq!(saves(&backend).len(), 2);
    drop(app.take_threads());
}

/// Closing the Save dialog keeps the library confirmation; a confirmed Revert ends it and has the service
/// reload the stored profile.
#[test]
fn the_library_confirmation_survives_a_closed_save_and_ends_with_a_revert() {
    let (mut app, backend) = single(Category::Belt, category_index(Category::Belt));
    apply_first(&mut app);
    dispatch(&mut app, "save", "", "").unwrap();
    dispatch(&mut app, "dialog-close", "", "").unwrap();
    assert!(app.ui.tuning.library_applied);
    dispatch(&mut app, "save", "", "").unwrap();
    assert!(matches!(app.ui.dialog, Some(crate::ui::dialogs::Dialog::SaveChoice { library: true, .. })));
    dispatch(&mut app, "dialog-close", "", "").unwrap();
    // Outside an edit Revert asks first; only the confirmation ends the library choice.
    dispatch(&mut app, "revert", "", "").unwrap();
    assert!(matches!(app.ui.dialog, Some(crate::ui::dialogs::Dialog::Revert)));
    assert!(app.ui.tuning.library_applied);
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert!(!app.ui.tuning.library_applied);
    assert!(backend.inner.calls.lock().unwrap().iter().any(|(_, path, _)| path == "profile/cancel"));
    dispatch(&mut app, "save", "", "").unwrap();
    assert!(app.ui.dialog.is_none());
    assert_eq!(saves(&backend).len(), 1);
    drop(app.take_threads());
}

/// A refused Apply does not ask for the library confirmation; Revert during an edit ends it at once.
#[test]
fn a_refused_apply_sets_no_confirmation_and_an_edit_revert_ends_it() {
    let (mut app, backend) = single(Category::Belt, category_index(Category::Belt));
    dispatch(&mut app, "action", "Downloaded Profiles:select:0", "").unwrap();
    *backend.content.lock().unwrap() = Err("The downloaded file was edited".into());
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert!(!app.ui.tuning.library_applied);
    *backend.content.lock().unwrap() = Ok(LegacyValue::from(tune(&backend)));
    apply_first(&mut app);
    assert!(app.ui.tuning.library_applied);
    backend.inner.docs.lock().unwrap()["status"]["controls"]["profile_edit"] = json!({"game": "ac"});
    dispatch(&mut app, "revert", "", "").unwrap();
    assert!(app.ui.dialog.is_none());
    assert!(!app.ui.tuning.library_applied);
    assert!(backend.inner.calls.lock().unwrap().iter().any(|(m, p, _)| *m == Method::Delete && p == "profile/edit"));
    assert!(saves(&backend).is_empty());
    drop(app.take_threads());
}
