//! Tests of the interface: the frame, navigation, theme, language, notifications, profiles, search and help.

use std::collections::BTreeSet;

use serde_json::{json, Value};

use super::*;
use crate::app::tests::{loaded, FakeBackend};
use crate::backend::Method;
use crate::host::UiApp;

fn html(app: &MyApp, region: Region) -> String {
    render(app, region)
}

/// The whole page as the browser has it.
fn page(app: &mut MyApp) -> String {
    app.gui.full_html()
}

#[test]
fn the_page_is_the_new_frame_only() {
    let mut app = loaded(FakeBackend::new());
    navigate(&mut app, Screen::Home, 0);
    let page = page(&mut app);
    assert!(page.contains("class=\"orc redline\""), "Redline frame on the root");
    for region in Region::ALL {
        assert!(page.contains(&format!("id=\"{}\"", region.id())), "{region:?}");
    }
    for old in ["app-header", "top-nav", "global-search", "nav-sidebar", "notification-container", "lang-menu-btn"] {
        assert!(!page.contains(old), "old frame part {old} is shown");
    }
    // The four sections, the language switcher top right and the Activity button.
    let nav = html(&app, Region::Nav);
    for group in Group::ALL {
        assert!(nav.contains(&format!("data-arg=\"{}\"", group.key())));
    }
    assert!(nav.contains("nav-item active\" data-act=\"nav\" data-arg=\"Tuning\""));
    let top = html(&app, Region::Top);
    assert!(top.contains("id=\"language-switcher\"") && top.contains(">EN<"));
    assert!(top.contains("activity-button"));
    assert!(html(&app, Region::MobileNav).contains("class=\"mobile-nav\""));
    let head = html(&app, Region::Head);
    assert!(head.contains("Active profile") && head.contains("value=\"manage\""));
    assert!(html(&app, Region::Save).contains("data-act=\"save\""));
    assert!(html(&app, Region::Main).contains("role=\"tablist\""));
    // The action receiver is not part of the page.
    assert!(!page.contains(ACTION_ID));
    drop(app.take_threads());
}

#[test]
fn redline_is_the_default_and_retired_or_unknown_themes_map_to_it() {
    for (stored, theme) in [
        (json!(null), Theme::Redline),
        (json!("classic"), Theme::Redline),
        (json!("orbit"), Theme::Redline),
        (json!("apex"), Theme::Apex),
        (json!("vector"), Theme::Vector),
    ] {
        let backend = FakeBackend::new();
        backend.docs.lock().unwrap()["settings"]["app"]["theme"] = stored.clone();
        let mut app = loaded(backend);
        assert_eq!(app.ui.theme, theme, "{stored}");
        assert!(page(&mut app).contains(&format!("class=\"orc {}\"", theme.id())));
        drop(app.take_threads());
    }
    assert_eq!(openrigcue_config::settings::Settings::default().app.theme, "redline");
}

#[test]
fn the_theme_stored_with_the_settings_defaults_to_redline() {
    let app = openrigcue_config::settings::Settings::default();
    assert_eq!(Theme::from_setting(&app.app.theme), Theme::Redline);
}

#[test]
fn theme_switch_applies_at_once_and_keeps_the_screen_and_values() {
    let backend = FakeBackend::new();
    // A game runs, so the Motion values can be edited.
    backend.docs.lock().unwrap()["status"]["session"]["game"] = serde_json::json!("acc");
    let mut app = loaded(backend.clone());
    navigate(&mut app, Screen::Motion, 1);
    dispatch(&mut app, "set", "Motion:pitch", "13").unwrap();
    dispatch(&mut app, "more", "motion:1", "1").unwrap();
    // Settings → App → General → Theme.
    navigate(&mut app, Screen::App, 0);
    let general = html(&app, Region::Main);
    assert!(general.contains("data-arg=\"App:theme\""), "{general}");
    assert!(general.contains("<option value=\"0\" selected>Redline</option>"));
    dispatch(&mut app, "set", "App:theme", "2").unwrap();
    assert_eq!(app.ui.theme, Theme::Vector);
    assert_eq!(backend.docs.lock().unwrap()["settings"]["app"]["theme"], json!("vector"));
    assert!(page(&mut app).contains("class=\"orc vector\""));
    assert_eq!(app.ui.screen, Screen::App, "the screen stays");
    navigate(&mut app, Screen::Motion, 1);
    assert_eq!(crate::backend::num(app.tune(&["motion_effects", "pitch"])), 13.0, "the edited value stays");
    assert!(app.ui.open_more.contains("motion:1"), "open disclosures stay");
    dispatch(&mut app, "theme", "", "apex").unwrap();
    assert!(page(&mut app).contains("class=\"orc apex\""));
    drop(app.take_threads());
}

#[test]
fn each_theme_draws_its_own_frame() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend);
    navigate(&mut app, Screen::Home, 0);
    let frames: Vec<(String, String, String, String)> = Theme::ALL
        .into_iter()
        .map(|theme| {
            app.ui.theme = theme;
            (html(&app, Region::Nav), html(&app, Region::Top), html(&app, Region::Live), html(&app, Region::Save))
        })
        .collect();
    let [redline, apex, vector] = &frames[..] else { unreachable!() };
    // Redline: the pit-wall bar without a side note; its station name; the instrument panel and channels.
    assert!(!redline.0.contains("side-bottom") && redline.1.contains("ORC / REDLINE"));
    assert!(redline.2.contains("class=\"instrument\"") && redline.2.contains("channel-strip"));
    assert!(redline.3.contains("SAVE SETUP"));
    // Apex: the workspace sidebar with its session note; the rig name; the session with its speed trace.
    assert!(apex.0.contains("Workspace") && apex.0.contains("side-bottom"));
    assert!(apex.1.contains("My simulation rig"));
    assert!(apex.2.contains("telemetry-art") && !apex.2.contains("channel-strip"));
    assert!(apex.3.contains("Save profile"));
    // Vector: the system select rail with its instrument note; the console name; the attitude display.
    assert!(vector.0.contains("System select") && vector.0.contains("rail-note"));
    assert!(vector.1.contains("ORC / VECTOR"));
    assert!(vector.2.contains("hud-display"), "stand-by shows the attitude display");
    drop(app.take_threads());
}

#[test]
fn readouts_show_live_telemetry_only() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend);
    navigate(&mut app, Screen::Home, 0);
    let live = html(&app, Region::Live);
    assert!(live.contains("—"), "stand-by shows no values");
    for sample in ["184", "Assetto", "SAMPLE", "Sample", "demo", "DEMO"] {
        assert!(!live.contains(sample), "sample data {sample}");
    }
    let session = session::Session {
        state: session::State::Telemetry,
        game_key: "acc".into(),
        game: "Assetto Corsa Competizione".into(),
        vehicle: "Porsche 911 GT3 R".into(),
        location: "Spa".into(),
        racing: true,
    };
    let telemetry = json!({"speed_kmh": 201.4, "gear": 5, "rpm": 7000, "max_rpm": 9000, "pitch": 1.5,
                           "roll": -2.0, "vertical_acceleration": 0.3});
    apply_tick(&mut app, session, Some(telemetry));
    let live = html(&app, Region::Live);
    assert!(live.contains(">201<") && live.contains("GEAR 5") && live.contains("7000 RPM"));
    assert!(live.contains("PORSCHE 911 GT3 R · SPA") || live.contains("Porsche 911 GT3 R · Spa"));
    assert!(live.contains("-02.0°"));
    drop(app.take_threads());
}

#[test]
fn navigation_opens_each_section_and_the_page_selector() {
    let mut app = loaded(FakeBackend::new());
    for (group, screen) in
        [("Games", Screen::Pc), ("Hardware", Screen::Hardware), ("Settings", Screen::App), ("Tuning", Screen::Home)]
    {
        dispatch(&mut app, "nav", group, "").unwrap();
        assert_eq!(app.ui.screen, screen);
        assert!(html(&app, Region::Nav).contains(&format!("nav-item active\" data-act=\"nav\" data-arg=\"{group}\"")));
    }
    dispatch(&mut app, "open-tuning", "", "").unwrap();
    assert_eq!(app.ui.screen, Screen::Motion);
    let head = html(&app, Region::Head);
    assert!(head.contains("id=\"screen-picker\"") && head.contains("value=\"Routines\""));
    assert!(!head.contains("Player") && !head.contains("Recorder") && !head.contains("Editor"), "no 4D videos");
    dispatch(&mut app, "screen", "", "Telemetry").unwrap();
    assert_eq!(app.ui.screen, Screen::Telemetry);
    // An old page name opens its screen.
    app.goto_page("Setup", "Motion Compensation");
    assert_eq!(app.ui.screen, Screen::Compensation);
    drop(app.take_threads());
}

#[test]
fn language_switcher_selects_the_language_for_every_region() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    dispatch(&mut app, "popup", "language", "").unwrap();
    let menu = html(&app, Region::Popup);
    for (code, name) in shell::LANGUAGES {
        assert!(menu.contains(&format!("data-arg=\"{code}\"")) && menu.contains(name));
    }
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "language", "DE", "").unwrap();
    assert!(backend
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|(m, p, b)| *m == Method::Put && p == "i18n" && b.as_ref() == Some(&json!({"code": "DE"}))));
    assert_eq!(app.ui.popup, None);
    drop(app.take_threads());
}

#[test]
fn a_language_change_runs_the_device_status_rules_again() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    app.ml.previous_hardware_snapshot = Some(openrigcue_model::native_value::LegacyValue::None);
    dispatch(&mut app, "language", "DE", "").unwrap();
    assert!(app.ml.previous_hardware_snapshot.is_none(), "the next pass builds the status texts in the new language");
    drop(app.take_threads());
}

#[test]
fn successes_leave_after_six_seconds_and_at_most_two_toasts_show() {
    let mut app = loaded(FakeBackend::new());
    app.alert_notification("Success", "First", "green", 3.0);
    let toasts = html(&app, Region::Toasts);
    assert!(toasts.contains("data-life=\"6000\""), "{toasts}");
    app.alert_notification("Warning", "Second", "orange", 3.0);
    app.alert_notification("Success", "Third", "green", 3.0);
    let toasts = html(&app, Region::Toasts);
    assert!(!toasts.contains("First") && toasts.contains("Second") && toasts.contains("Third"));
    assert!(toasts.contains("toast warning\" data-toast") && toasts.contains("data-life=\"0\""), "warnings stay");
    let drawer = html(&app, Region::Drawer);
    for text in ["First", "Second", "Third"] {
        assert!(drawer.contains(text), "Activity keeps {text}");
    }
    assert!(html(&app, Region::Top).contains("<b id=\"event-count\">3</b>"));
    // The browser reports the end of a success's time; it stays in Activity.
    let third = app.ui.notices.toasts[1];
    dispatch(&mut app, "toast-expire", &third.to_string(), "").unwrap();
    assert!(!html(&app, Region::Toasts).contains("Third"));
    assert!(html(&app, Region::Drawer).contains("Third"));
    dispatch(&mut app, "drawer", "", "").unwrap();
    assert!(!html(&app, Region::Drawer).contains(" hidden>"));
    dispatch(&mut app, "drawer-close", "", "").unwrap();
    assert!(html(&app, Region::Drawer).contains(" hidden>"));
    drop(app.take_threads());
}

#[test]
fn closing_a_service_toast_dismisses_its_notification_and_other_toasts_only_leave() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    backend.docs.lock().unwrap()["notifications"] = json!([
        {"id": 5, "kind": "belt_calibration", "title": "Calibrate the belt", "message": "The belt needs a calibration", "colour": "red"}
    ]);
    notices::synchronize(&mut app);
    let toast = app.ui.notices.service[&5];
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "toast-dismiss", &toast.to_string(), "").unwrap();
    assert!(
        backend.calls.lock().unwrap().iter().any(|(m, p, _)| *m == Method::Delete && p == "notifications/5"),
        "× tells the service, as Dismiss does"
    );
    assert!(!html(&app, Region::Toasts).contains("Calibrate the belt"));
    assert!(app.ui.notices.get(toast).is_some_and(|n| !n.open), "its actions go, as with Dismiss");
    // A toast without a Dismiss button only leaves the screen; its expiry tells the service nothing.
    notices::warning(&mut app, "Alert", "A page warning");
    let warning = *app.ui.notices.toasts.last().unwrap();
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "toast-dismiss", &warning.to_string(), "").unwrap();
    assert!(!html(&app, Region::Toasts).contains("A page warning"));
    assert!(!backend.calls.lock().unwrap().iter().any(|(m, _, _)| *m == Method::Delete));
    backend.docs.lock().unwrap()["notifications"] = json!([
        {"id": 6, "kind": "belt_calibration", "title": "Calibrate again", "message": "x", "colour": "red"}
    ]);
    notices::synchronize(&mut app);
    let expiring = app.ui.notices.service[&6];
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "toast-expire", &expiring.to_string(), "").unwrap();
    assert!(!backend.calls.lock().unwrap().iter().any(|(m, _, _)| *m == Method::Delete));
    drop(app.take_threads());
}

#[test]
fn service_notifications_keep_their_actions_and_faults_stay_inline() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    backend.docs.lock().unwrap()["notifications"] = json!([
        {"id": 1, "kind": "game_not_configured", "title": "This game is not set up", "message": "Configure the game", "colour": "red"},
        {"id": 3, "kind": "port_in_use", "process": "SimHub.exe", "port": 10001, "title": "Game Telemetry Not Detected", "message": "unused", "colour": "red"},
        {"id": 4, "kind": "device_permissions", "devices": ["/dev/ttyUSB0"], "title": "Controller access required", "message": "unused", "colour": "red"}
    ]);
    notices::synchronize(&mut app);
    let toasts = html(&app, Region::Toasts);
    assert!(
        toasts.contains("This game is not set up") && toasts.contains(">Game Page<") && toasts.contains(">Dismiss<")
    );
    let faults = html(&app, Region::Fault);
    assert!(faults.contains("class=\"critical\"") && faults.contains("SimHub.exe occupies port 10001"));
    assert!(faults.contains(">Install<"), "the device access fault keeps its Install button");
    // Shown once.
    let count = app.ui.notices.history.len();
    notices::synchronize(&mut app);
    assert_eq!(app.ui.notices.history.len(), count);
    // Dismiss calls the service.
    let game = app.ui.notices.service[&1];
    dispatch(&mut app, "notice", &format!("{game}:1"), "").unwrap();
    assert!(backend.calls.lock().unwrap().iter().any(|(m, p, _)| *m == Method::Delete && p == "notifications/1"));
    // Install runs on its own thread, once.
    let install = app.ui.notices.service[&4];
    drop(app.take_threads());
    dispatch(&mut app, "notice", &format!("{install}:0"), "").unwrap();
    dispatch(&mut app, "notice", &format!("{install}:0"), "").unwrap();
    assert_eq!(app.take_threads().len(), 1, "a second click starts nothing");
    assert!(html(&app, Region::Fault).contains("aria-busy=\"true\""));
    drop(app.take_threads());
    // A withdrawn fault leaves the page and stays in Activity.
    backend.docs.lock().unwrap()["notifications"] = json!([]);
    notices::synchronize(&mut app);
    assert!(html(&app, Region::Fault).is_empty());
    assert!(html(&app, Region::Drawer).contains("SimHub.exe"));
    drop(app.take_threads());
}

#[test]
fn game_page_and_sound_fix_actions_run_through_the_service() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    backend.docs.lock().unwrap()["notifications"] = json!([
        {"id": 2, "kind": "surround_required", "title": "Surround required", "message": "Configure surround", "colour": "red"},
        {"id": 7, "kind": "default_sound_card", "title": "Shakers - Default Sound Device", "message": "x", "colour": "red"}
    ]);
    backend.docs.lock().unwrap()["settings"]["shakers"]["sound_card"] = json!("Shaker card");
    backend.docs.lock().unwrap()["hardware"]["sound_cards"] = json!(["None", "Shaker card", "Monitor"]);
    app.refresh_docs();
    notices::synchronize(&mut app);
    let surround = app.ui.notices.service[&2];
    dispatch(&mut app, "notice", &format!("{surround}:1"), "").unwrap();
    assert!(matches!(app.ui.dialog, Some(dialogs::Dialog::ApplyingFix { default: false, ready: false })));
    assert!(backend.calls.lock().unwrap().iter().any(|(m, p, b)| *m == Method::Post
        && p == "audio/fix/surround"
        && b.as_ref() == Some(&json!({"name": "Shaker card"}))));
    assert!(html(&app, Region::Dialog).contains("data-locked"), "no way out until the fix ends");
    // The withdrawal closes the overlay.
    backend.docs.lock().unwrap()["notifications"] = json!([{"id": 7, "kind": "default_sound_card", "title": "Shakers - Default Sound Device", "message": "x", "colour": "red"}]);
    assert!(page(&mut app).contains("Applying Fix"), "the overlay is on screen");
    notices::synchronize(&mut app);
    assert!(app.ui.dialog.is_none());
    assert!(!page(&mut app).contains("Applying Fix"), "the overlay leaves the screen with the notice");
    assert!(app.ui.notices.fix_timer.is_none(), "the OK timer is cancelled");
    // The default-device fix asks for the game audio device first.
    let default = app.ui.notices.service[&7];
    dispatch(&mut app, "notice", &format!("{default}:1"), "").unwrap();
    let Some(dialogs::Dialog::SoundDevice { devices, .. }) = &app.ui.dialog else { panic!("device choice") };
    assert_eq!(devices, &[("Monitor".to_owned(), "Monitor".to_owned())]);
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert!(backend.calls.lock().unwrap().iter().any(|(m, p, b)| *m == Method::Post
        && p == "audio/fix/default"
        && b.as_ref() == Some(&json!({"name": "Monitor"}))));
    drop(app.take_threads());
}

#[test]
fn save_asks_for_the_level_with_a_vehicle_and_revert_confirms() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "save", "", "").unwrap();
    assert!(backend
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|(m, p, b)| *m == Method::Post && p == "profile/save" && b.as_ref() == Some(&json!({"level": "game"}))));
    assert!(app.ui.notices.history.iter().any(|n| n.message == "Settings and profile have been saved!"));
    app.ui.session.vehicle = "GT3".into();
    dispatch(&mut app, "save", "", "").unwrap();
    assert!(matches!(app.ui.dialog, Some(dialogs::Dialog::SaveChoice { .. })));
    let dialog = html(&app, Region::Dialog);
    assert!(dialog.contains("Vehicle: GT3") && dialog.contains("Game: "));
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert!(backend.calls.lock().unwrap().iter().any(|(m, p, b)| *m == Method::Post
        && p == "profile/save"
        && b.as_ref() == Some(&json!({"level": "vehicle", "vehicle": "GT3"}))));
    dispatch(&mut app, "revert", "", "").unwrap();
    assert!(matches!(app.ui.dialog, Some(dialogs::Dialog::Revert)));
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert!(backend.calls.lock().unwrap().iter().any(|(m, p, _)| *m == Method::Post && p == "profile/cancel"));
    drop(app.take_threads());
}

#[test]
fn manage_profiles_lists_edits_resets_and_exports() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    dispatch(&mut app, "profile", "", "manage").unwrap();
    assert_eq!(app.ui.screen, Screen::Profiles);
    assert!(!app.ui.profiles.entries.is_empty());
    let first = app.ui.profiles.entries[0].clone();
    let body = html(&app, Region::Main);
    assert!(body.contains(&first.name) && body.contains("data-act=\"profile-export-all\""));
    dispatch(&mut app, "profiles-filter", "", "zzzz-no-such-game").unwrap();
    assert!(html(&app, Region::Main).contains("Try another game name."));
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "profile-reset", &first.game, "").unwrap();
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert!(backend
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|(m, p, _)| *m == Method::Delete && p == &format!("tunes/{}", crate::backend::segment(&first.game))));
    dispatch(&mut app, "profile-edit", "acc^GT3 Car", "").unwrap();
    assert!(backend.calls.lock().unwrap().iter().any(|(m, p, b)| *m == Method::Post
        && p == "profile/edit"
        && b.as_ref() == Some(&json!({"game": "acc", "vehicle": "GT3 Car"}))));
    assert_eq!(app.ui.screen, Screen::Home);
    drop(app.take_threads());
}

#[test]
fn a_profile_file_is_checked_before_the_import_question() {
    use base64::Engine as _;
    let mut app = loaded(FakeBackend::new());
    let data = base64::engine::general_purpose::STANDARD.encode(b"not a profile");
    dispatch(&mut app, "profile-import", "x.srs", &format!("data:application/octet-stream;base64,{data}")).unwrap();
    assert!(matches!(app.ui.dialog, Some(dialogs::Dialog::Message { .. })), "{:?}", app.ui.dialog);
    assert!(app.ui.profiles.pending_import.is_none());
    drop(app.take_threads());
}

#[test]
fn search_finds_screens_and_settings() {
    let mut app = loaded(FakeBackend::new());
    dispatch(&mut app, "popup", "search", "").unwrap();
    dispatch(&mut app, "search", "", "Sm").unwrap();
    assert!(html(&app, Region::Popup).contains("Enter 3 or more characters"));
    dispatch(&mut app, "search", "", "smoothing").unwrap();
    let results = html(&app, Region::Popup);
    assert!(results.contains("Overall Smoothing") && results.contains("data-arg=\"Motion:0\""), "{results}");
    dispatch(&mut app, "search-go", "Motion:5", "").unwrap();
    assert_eq!((app.ui.screen, app.ui.section), (Screen::Motion, 5));
    assert_eq!(app.ui.popup, None);
    drop(app.take_threads());
}

#[test]
fn rig_status_cycles_device_modes_through_the_service() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    let rig = html(&app, Region::Rig);
    assert!(rig.contains("Motion platform") && rig.contains("Bass shakers") && rig.contains("Wind fans"));
    assert!(rig.contains("More devices"));
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "mode", "Motionbox", "").unwrap();
    assert!(backend.patches("controls").iter().any(|p| p["switches"].get("motion").is_some()));
    dispatch(&mut app, "device", "MCMode", "").unwrap();
    assert_eq!(app.ui.screen, Screen::Compensation);
    drop(app.take_threads());
}

#[test]
fn shortcut_editor_captures_and_stores_bindings() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    backend.calls.lock().unwrap().clear();
    shortcuts::open(&mut app, &["pitch".to_owned()]).unwrap();
    assert!(backend.calls.lock().unwrap().iter().any(|(m, p, _)| *m == Method::Post && p == "hotkeys/capture"));
    let Some(dialogs::Dialog::Shortcut(mut editor)) = app.ui.dialog.clone() else { panic!("editor") };
    editor.keys = "ctrl+shift+p".into();
    app.ui.dialog = Some(dialogs::Dialog::Shortcut(editor));
    assert!(html(&app, Region::Dialog).contains("ctrl+shift+p"));
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert!(app.ui.dialog.is_none());
    assert!(backend.calls.lock().unwrap().iter().any(|(m, p, _)| *m == Method::Delete && p == "hotkeys/capture"));
    drop(app.take_threads());
}

/// Every text the interface shows has an entry in every language: every region of every theme, every screen,
/// and the dialogs.
#[test]
fn every_interface_text_has_an_entry_in_every_language() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["settings"]["app"]["enable_hapticr"] = json!(true);
    let mut app = loaded(backend.clone());
    backend.translations.lock().unwrap().clear();
    let session = session::Session {
        state: session::State::Telemetry,
        game_key: "acc".into(),
        game: "Assetto Corsa Competizione".into(),
        vehicle: "Car".into(),
        location: String::new(),
        racing: true,
    };
    for theme in Theme::ALL {
        app.ui.theme = theme;
        for screen in Screen::ALL {
            app.ui.screen = screen;
            for section in 0..10 {
                app.ui.section = section;
                for region in Region::ALL {
                    render(&app, region);
                }
            }
        }
        for tab in 0..3 {
            app.ui.screen = Screen::Home;
            app.ui.tab = tab;
            render(&app, Region::Main);
        }
        app.ui.session = session.clone();
        app.ui.readouts.live = true;
        render(&app, Region::Live);
        app.ui.session.racing = false;
        render(&app, Region::Live);
        app.ui.session = session::Session::default();
        app.ui.readouts.live = false;
    }
    for popup in [Popup::Language, Popup::Search] {
        app.ui.popup = Some(popup);
        app.ui.search = "motion".into();
        render(&app, Region::Popup);
    }
    for dialog in [
        dialogs::Dialog::SaveChoice { vehicle: "Car".into(), game: "acc".into(), choice: 0, library: true },
        dialogs::Dialog::Revert,
        dialogs::Dialog::ProfileConfirm { game: "acc".into(), vehicle: None },
        dialogs::Dialog::ProfileConfirm { game: "acc".into(), vehicle: Some("Car".into()) },
        dialogs::Dialog::Import { name: "x.srs".into() },
        dialogs::Dialog::SoundDevice { notice: 1, devices: vec![("a".into(), "A".into())], selected: 0 },
        dialogs::Dialog::ApplyingFix { default: true, ready: false },
        dialogs::Dialog::ApplyingFix { default: false, ready: true },
        dialogs::Dialog::Companion { url: "http://127.0.0.1:33733".into(), image: String::new() },
        dialogs::Dialog::Wizard(screens::motion::WizardStep::Intro),
        dialogs::Dialog::Wizard(screens::motion::WizardStep::Countdown(Some(3))),
        dialogs::Dialog::Wizard(screens::motion::WizardStep::Capture(30)),
        dialogs::Dialog::Wizard(screens::motion::WizardStep::Review {
            rows: vec![("pitch".into(), "10".into(), 12)],
            preference: 1,
        }),
    ] {
        app.ui.dialog = Some(dialog);
        render(&app, Region::Dialog);
    }
    shortcuts::open(&mut app, &["pitch".to_owned(), "roll".to_owned()]).unwrap();
    if let Some(dialogs::Dialog::Shortcut(editor)) = app.ui.dialog.as_mut() {
        editor.keyboard_denied = true;
        editor.joysticks = vec!["Wheel".into()];
        editor.joystick = "Wheel".into();
    }
    render(&app, Region::Dialog);
    app.ui.dialog = None;
    for (title, message) in [
        ("Changes reverted", "Restored the saved settings."),
        ("Changes reverted", "The profile is no longer being edited."),
        ("Export", "The file opens in your browser for download."),
        ("Motion platform connected", "Review the settings before you resume."),
        ("Autotune", "Autotune complete"),
    ] {
        notices::confirmation(&mut app, title, message);
    }
    app.ui.screen = Screen::Home;
    compare(&mut app);
    inspect(&mut app);
    app.ui.readouts.live = true;
    inspect(&mut app);
    app.ml.hardware_states.insert("Motionbox".into(), ("red".into(), String::new()));
    backend.docs.lock().unwrap()["settings"]["motion"]["model"] = json!("Yaw2 PRO");
    motion_link(&mut app);
    let notice = app.ui.notices.history.last().unwrap().id;
    app.ui.notices.get_mut(notice).unwrap().open = false;
    render(&app, Region::Drawer);
    let looked_up: BTreeSet<String> = backend.translations.lock().unwrap().iter().cloned().collect();
    let missing = crate::app::tests::texts_without_entry(&looked_up, &["Car", "A", "Wheel", "x.srs"]);
    assert_eq!(missing, Vec::<String>::new(), "interface texts without an entry in en.json");
    drop(app.take_threads());
}

// ---- The stand-by lock and its note -----------------------------------------------------------------------

const STANDBY_NOTE: &str = "Tuning is unavailable in Stand By mode";

/// Whether the screen shows the stand-by note (its rows are locked).
fn locked_note(app: &mut MyApp, screen: Screen) -> bool {
    navigate(app, screen, 0);
    screens::main(app).contains(STANDBY_NOTE)
}

#[test]
fn stand_by_lock_follows_the_game_the_edit_setting_and_always_on_fans() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    // Stand-by: every tuning screen but LED is locked.
    for screen in [Screen::Wind, Screen::Shake, Screen::Motion, Screen::Belt, Screen::Sim3d, Screen::IFlag] {
        assert!(locked_note(&mut app, screen), "{screen:?}");
    }
    assert!(!locked_note(&mut app, Screen::Led), "LED stays editable");
    // The edit-in-stand-by setting, read when the screen is drawn: a true value, or the text "yes".
    for (flag, locked) in [(json!(true), false), (json!("yes"), false), (json!("no"), true), (json!("on"), true)] {
        backend.docs.lock().unwrap()["settings"]["features"]["standby_tuning_editable"] = flag.clone();
        assert_eq!(locked_note(&mut app, Screen::Shake), locked, "{flag}");
    }
    backend.docs.lock().unwrap()["settings"]["features"]["standby_tuning_editable"] = json!(false);
    // A fan output that is Always On opens Wind only; the text "2" is not the number 2.
    for (side, value, wind_locked) in
        [("wind", json!("always_on"), false), ("wind_center", json!("always_on"), false), ("wind", json!("2"), true)]
    {
        backend.docs.lock().unwrap()["status"]["controls"]["switches"][side] = value.clone();
        assert_eq!(locked_note(&mut app, Screen::Wind), wind_locked, "{side} {value}");
        assert!(locked_note(&mut app, Screen::Shake), "{side} {value} leaves Shake locked");
        backend.docs.lock().unwrap()["status"]["controls"]["switches"][side] = json!("auto");
    }
    // A running game opens every screen, whatever the setting says.
    backend.docs.lock().unwrap()["settings"]["features"]["standby_tuning_editable"] = json!("on");
    backend.docs.lock().unwrap()["status"]["session"]["game"] = json!("api");
    app.main_loop_pass();
    for screen in [Screen::Wind, Screen::Shake, Screen::Motion, Screen::Belt] {
        assert!(!locked_note(&mut app, screen), "{screen:?}");
    }
    drop(app.take_threads());
}

#[test]
fn the_stand_by_note_follows_the_current_hide_flag_and_reports_a_failed_hide() {
    for (cached, current) in [(json!("no"), json!("no")), (json!(true), json!(false)), (json!(false), json!("on"))] {
        let backend = FakeBackend::new();
        backend.docs.lock().unwrap()["settings"]["app"]["hide_standby_tuning_alert"] = cached.clone();
        let mut app = loaded(backend.clone());
        backend.docs.lock().unwrap()["settings"]["app"]["hide_standby_tuning_alert"] = current.clone();
        navigate(&mut app, Screen::Wind, 0);
        let body = screens::main(&app);
        assert!(body.contains(STANDBY_NOTE) && body.contains("data-act=\"standby-hide\""), "{cached} → {current}");
        drop(app.take_threads());
    }
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    navigate(&mut app, Screen::Wind, 0);
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "standby-hide", "", "").unwrap();
    assert!(backend
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|(m, p, _)| *m == Method::Post && p == "settings/hide-standby-alert"));
    assert_eq!(app.docs.settings["app"]["hide_standby_tuning_alert"], json!("yes"));
    let body = screens::main(&app);
    assert!(!body.contains(STANDBY_NOTE), "the note is hidden from now on");
    assert!(body.contains(" disabled"), "the rows stay locked");
    // A failed write is reported; the settings are read again all the same.
    backend.docs.lock().unwrap()["settings"]["app"]["hide_standby_tuning_alert"] = json!(false);
    backend.docs.lock().unwrap()["standby_write_failure"] = json!("OSError: write failed");
    assert_eq!(dispatch(&mut app, "standby-hide", "", ""), Err("OSError: write failed".to_owned()));
    assert_eq!(app.docs.settings["app"]["hide_standby_tuning_alert"], json!("yes"));
    drop(app.take_threads());
}

// ---- Save and Revert ---------------------------------------------------------------------------------------

/// The calls that change something, in order.
fn writes(backend: &FakeBackend) -> Vec<String> {
    backend.calls.lock().unwrap().iter().filter(|(m, _, _)| *m != Method::Get).map(|(_, p, _)| p.clone()).collect()
}

fn saved_notice(app: &MyApp) -> bool {
    app.ui.notices.history.iter().any(|n| n.message == "Settings and profile have been saved!")
}

#[test]
fn save_reads_the_current_game_and_saves_the_vehicle_that_was_shown() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    app.ui.session.vehicle = "Displayed car".into();
    app.docs.status["session"]["game"] = json!("acc");
    backend.docs.lock().unwrap()["status"]["session"]["game"] = json!("ac");
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "save", "", "").unwrap();
    assert_eq!(backend.calls.lock().unwrap()[0].1, "desktop/capture-geometry", "the window geometry is taken first");
    let Some(dialogs::Dialog::SaveChoice { vehicle, game, .. }) = app.ui.dialog.clone() else { panic!("save dialog") };
    assert_eq!((vehicle.as_str(), game.as_str()), ("Displayed car", "ac"), "the service's current game");
    let dialog = html(&app, Region::Dialog);
    assert!(dialog.contains(">Vehicle: Displayed car</option>") && dialog.contains(">Game: ac</option>"));
    assert!(!writes(&backend).iter().any(|p| p == "profile/save" || p == "settings/save"), "nothing is saved yet");
    // The service's vehicle changes meanwhile: the vehicle the dialog named is saved.
    backend.docs.lock().unwrap()["status"]["session"]["vehicle"] = json!("New SDK car");
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert!(backend.calls.lock().unwrap().iter().any(|(m, p, b)| *m == Method::Post
        && p == "profile/save"
        && b.as_ref() == Some(&json!({"level": "vehicle", "vehicle": "Displayed car"}))));
    assert!(saved_notice(&app));
    // Cancel instead of Confirm saves nothing.
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "save", "", "").unwrap();
    dispatch(&mut app, "dialog-close", "", "").unwrap();
    assert_eq!(writes(&backend), ["desktop/capture-geometry"]);
    drop(app.take_threads());
}

#[test]
fn save_without_a_current_game_opens_nothing() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    app.ui.session.vehicle = "Displayed car".into();
    backend.docs.lock().unwrap()["status"]["session"].as_object_mut().unwrap().remove("game");
    backend.calls.lock().unwrap().clear();
    assert_eq!(dispatch(&mut app, "save", "", ""), Err("KeyError: 'game'".to_owned()));
    assert!(app.ui.dialog.is_none());
    assert_eq!(writes(&backend), ["desktop/capture-geometry"]);
    drop(app.take_threads());
}

#[test]
fn revert_reads_the_current_edit_mode_before_choosing_its_branch() {
    for (cached, current, ends_edit) in [
        (Value::Null, json!({"game": "ac"}), true),
        (json!({"game": "ac"}), Value::Null, false),
        (json!({"game": "ac"}), json!(false), false),
    ] {
        let backend = FakeBackend::new();
        let mut app = loaded(backend.clone());
        app.docs.controls["profile_edit"] = cached.clone();
        backend.docs.lock().unwrap()["status"]["controls"]["profile_edit"] = current.clone();
        backend.calls.lock().unwrap().clear();
        dispatch(&mut app, "revert", "", "").unwrap();
        assert_eq!(matches!(app.ui.dialog, Some(dialogs::Dialog::Revert)), !ends_edit, "{cached} / {current}");
        let deleted = backend.calls.lock().unwrap().iter().any(|(m, p, _)| *m == Method::Delete && p == "profile/edit");
        assert_eq!(deleted, ends_edit, "{cached} / {current}");
        drop(app.take_threads());
    }
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    backend.docs.lock().unwrap()["status"]["controls"].as_object_mut().unwrap().remove("profile_edit");
    backend.calls.lock().unwrap().clear();
    assert_eq!(dispatch(&mut app, "revert", "", ""), Err("KeyError: 'profile_edit'".to_owned()));
    assert!(app.ui.dialog.is_none());
    assert!(!backend.calls.lock().unwrap().iter().any(|(_, p, _)| p == "profile/edit" || p == "profile/cancel"));
    drop(app.take_threads());
}

#[test]
fn save_failures_stop_before_the_later_writes_and_the_success() {
    // The speaker layout cannot be stored: the profile is saved, the settings and the edit are left alone.
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    app.ui.session.vehicle = "Displayed car".into();
    backend.fail_shaker_save.store(true, std::sync::atomic::Ordering::SeqCst);
    dispatch(&mut app, "save", "", "").unwrap();
    backend.calls.lock().unwrap().clear();
    assert_eq!(dispatch(&mut app, "dialog-ok", "", ""), Err("RuntimeError: fixture shaker save failed".to_owned()));
    assert_eq!(writes(&backend), ["profile/save", "shakers/store-effects"]);
    assert!(!saved_notice(&app));
    drop(app.take_threads());
    // The profile cannot be written: nothing follows; the dialog has closed.
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    app.ui.session.vehicle = "Displayed car".into();
    dispatch(&mut app, "save", "", "").unwrap();
    backend.docs.lock().unwrap()["profile_save_failure"] = json!("OSError: fixture database write failed");
    backend.calls.lock().unwrap().clear();
    assert_eq!(dispatch(&mut app, "dialog-ok", "", ""), Err("OSError: fixture database write failed".to_owned()));
    assert_eq!(writes(&backend), ["profile/save"]);
    assert!(app.ui.dialog.is_none() && !saved_notice(&app));
    drop(app.take_threads());
    // The edit cannot be ended: everything before it was saved, in order; the remembered location stays.
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    app.ui.session.vehicle = "Displayed car".into();
    dispatch(&mut app, "save", "", "").unwrap();
    backend.docs.lock().unwrap()["profile_clear_failure"] = json!("RuntimeError: fixture clear failed");
    app.ml.previous_location = Some("Previous track".into());
    backend.calls.lock().unwrap().clear();
    assert_eq!(dispatch(&mut app, "dialog-ok", "", ""), Err("RuntimeError: fixture clear failed".to_owned()));
    assert_eq!(writes(&backend), ["profile/save", "shakers/store-effects", "settings/save", "profile/edit"]);
    assert_eq!(app.ml.previous_location, Some("Previous track".into()));
    assert!(!saved_notice(&app));
    drop(app.take_threads());
    // Without a vehicle the click saves for the game at once; its failure comes from the click.
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    app.ui.session.vehicle.clear();
    backend.docs.lock().unwrap()["profile_save_failure"] = json!("OSError: fixture database write failed");
    backend.calls.lock().unwrap().clear();
    assert_eq!(dispatch(&mut app, "save", "", ""), Err("OSError: fixture database write failed".to_owned()));
    assert!(app.ui.dialog.is_none() && !saved_notice(&app));
    let calls = backend.calls.lock().unwrap().clone();
    let save = calls.iter().find(|(_, p, _)| p == "profile/save").unwrap();
    assert_eq!(save.2.as_ref().unwrap()["level"], "game");
    assert!(!calls.iter().any(|(_, p, _)| p == "shakers/store-effects"));
    drop(app.take_threads());
}

#[test]
fn a_failed_end_of_editing_is_reported_before_anything_changes() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["status"]["controls"]["profile_edit"] = json!({"game": "ac"});
        docs["profile_clear_failure"] = json!("RuntimeError: fixture clear failed");
    }
    app.ml.previous_location = Some("Previous track".into());
    let notices = app.ui.notices.history.len();
    assert_eq!(dispatch(&mut app, "revert", "", ""), Err("RuntimeError: fixture clear failed".to_owned()));
    assert!(app.ui.dialog.is_none(), "no confirmation opens");
    assert_eq!(app.ui.notices.history.len(), notices, "no \"Changes reverted\"");
    assert_eq!(app.ml.previous_location, Some("Previous track".into()));
    assert!(!backend.calls.lock().unwrap().iter().any(|(_, p, _)| p == "profile/cancel"));
    drop(app.take_threads());
}

// ---- The active profile ------------------------------------------------------------------------------------

/// The session after one main-loop tick: its state, the active profile's label.
fn active_profile(app: &mut MyApp) -> (session::State, String) {
    tick(app);
    (app.ui.session.state, shell::profile_label(app))
}

#[test]
fn the_active_profile_follows_the_current_session() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    navigate(&mut app, Screen::Home, 0);
    assert_eq!(active_profile(&mut app), (session::State::Standby, "Stand By".to_owned()));
    // The game's name comes from the service's current catalogue.
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["catalog_games"] = json!([{"id": "ac", "name": "current name"}]);
        docs["status"]["session"]["game"] = json!("ac");
        docs["status"]["previous_actual_vehicle_name"] = json!("Previous car");
        docs["status"]["session"]["vehicle"] = json!("SDK car");
    }
    app.docs.games = json!([{"id": "ac", "name": "stale name"}]);
    // A running game without telemetry shows the previous actual vehicle; a false edit flag is no edit.
    backend.docs.lock().unwrap()["status"]["controls"]["profile_edit"] = json!(false);
    assert_eq!(active_profile(&mut app), (session::State::GameRunning, "current name · Previous car".to_owned()));
    assert!(html(&app, Region::Head).contains("current name · Previous car"));
    // Editing a profile without telemetry.
    backend.docs.lock().unwrap()["status"]["controls"]["profile_edit"] = json!({"game": "ac"});
    assert_eq!(active_profile(&mut app).0, session::State::Editing);
    backend.docs.lock().unwrap()["status"]["controls"]["profile_edit"] = Value::Null;
    // Telemetry: the service's current vehicle, as its value reads.
    backend.docs.lock().unwrap()["status"]["session"]["listener"] = json!("ac");
    backend.docs.lock().unwrap()["status"]["session"]["vehicle"] = json!(1.0);
    backend.docs.lock().unwrap()["status"]["displayed_vehicle"] = json!("stale projection");
    assert_eq!(active_profile(&mut app), (session::State::Telemetry, "current name · 1.0".to_owned()));
    // A vehicle equal to zero falls back to the previous vehicle; a previous vehicle equal to zero is none.
    for (current, previous, label) in [
        (json!(-0.0), json!(7.0), "current name · 7.0"),
        (json!(false), json!(false), "current name"),
        (json!("0"), json!("previous"), "current name · 0"),
    ] {
        backend.docs.lock().unwrap()["status"]["session"]["vehicle"] = current.clone();
        backend.docs.lock().unwrap()["status"]["previous_actual_vehicle_name"] = previous;
        assert_eq!(active_profile(&mut app).1, label, "{current}");
    }
    // A session without a vehicle shows the game alone.
    backend.docs.lock().unwrap()["status"]["session"] = json!({"game": "ac", "listener": "ac"});
    backend.docs.lock().unwrap()["status"]["previous_actual_vehicle_name"] = json!(0);
    assert_eq!(active_profile(&mut app), (session::State::Telemetry, "current name".to_owned()));
    drop(app.take_threads());
}

#[test]
fn a_game_without_a_catalogue_name_shows_its_key() {
    for (game, catalog, label) in [
        (json!("missing game"), None, "missing game"),
        (json!("ac"), Some(json!([{"id": "ac"}])), "ac"),
        (json!(1), None, "1"),
    ] {
        let backend = FakeBackend::new();
        let mut app = loaded(backend.clone());
        {
            let mut docs = backend.docs.lock().unwrap();
            docs["status"]["session"]["game"] = game.clone();
            if let Some(catalog) = catalog {
                docs["catalog_games"] = catalog;
            }
        }
        assert_eq!(active_profile(&mut app), (session::State::GameRunning, label.to_owned()), "{game}");
        assert_eq!(app.ml.fatal_error, None);
        drop(app.take_threads());
    }
}

// ---- The rig panel's device modes ------------------------------------------------------------------------

/// The rig panel row of a device.
fn rig_row(app: &MyApp, key: &str) -> String {
    let rig = shell::devices(app);
    let start = rig.find(&format!("data-device=\"{key}\"")).unwrap_or_else(|| panic!("{key} in {rig}"));
    rig[start..].split("</div>").next().unwrap().to_owned()
}

#[test]
fn rig_modes_follow_the_current_switches_with_numeric_equality() {
    let backend = FakeBackend::new();
    let app = loaded(backend.clone());
    for (value, label) in [
        (json!("off"), ">Off<"),
        (json!("auto"), ">Auto<"),
        (json!("always_on"), ">Always On<"),
        (json!(1.0), ">Auto<"),
        (json!(2), ">Always On<"),
        (json!(false), ">Off<"),
        (json!("2"), ">Off<"),
        (json!(0.5), ">Off<"),
        (Value::Null, ">Off<"),
    ] {
        // The current switch, not the documents of the last pass.
        backend.docs.lock().unwrap()["status"]["controls"]["switches"]["wind"] = value.clone();
        let row = rig_row(&app, "WindMode");
        assert!(row.contains("data-act=\"mode\"") && row.contains(label), "{value}: {row}");
    }
    // A switch that cannot be read shows no mode button; the later rows still show theirs.
    backend.docs.lock().unwrap()["status"]["controls"]["switches"].as_object_mut().unwrap().remove("wind_center");
    assert!(!rig_row(&app, "WindCenterMode").contains("data-act=\"mode\""));
    assert!(rig_row(&app, "LedMode").contains("data-act=\"mode\""));
    assert!(rig_row(&app, "Motionbox").contains(">On<"));
}

#[test]
fn rig_mode_buttons_cycle_the_current_switch_and_report_failures() {
    for (row, field, current, cached, expected) in [
        ("WindMode", "wind", json!("auto"), json!("off"), json!("always_on")),
        ("WindMode", "wind", json!("always_on"), json!("off"), json!("off")),
        ("Motionbox", "motion", json!(true), json!(false), json!(false)),
    ] {
        let backend = FakeBackend::new();
        let mut app = loaded(backend.clone());
        backend.docs.lock().unwrap()["status"]["controls"]["switches"][field] = current;
        app.docs.controls["switches"][field] = cached;
        dispatch(&mut app, "mode", row, "").unwrap();
        assert_eq!(backend.docs.lock().unwrap()["status"]["controls"]["switches"][field], expected, "{row}");
        drop(app.take_threads());
    }
    // An unreadable switch is not written.
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    backend.docs.lock().unwrap()["status"]["controls"]["switches"].as_object_mut().unwrap().remove("wind");
    let before = backend.patches("controls").len();
    assert!(dispatch(&mut app, "mode", "WindMode", "").unwrap_err().starts_with("KeyError:"));
    assert_eq!(backend.patches("controls").len(), before);
    drop(app.take_threads());
    // A failed write keeps the switch the interface knows.
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    let previous = app.docs.controls["switches"]["wind"].clone();
    backend.docs.lock().unwrap()["control_failure"] = json!("TypeError: fixture mode assignment failed");
    assert_eq!(dispatch(&mut app, "mode", "WindMode", ""), Err("TypeError: fixture mode assignment failed".to_owned()));
    assert_eq!(app.docs.controls["switches"]["wind"], previous);
    drop(app.take_threads());
}

// ---- Search, readouts, the frame's widgets and the action receiver ----------------------------------------

#[test]
fn search_shows_the_hint_for_short_texts_and_closes_with_its_results() {
    let mut app = loaded(FakeBackend::new());
    dispatch(&mut app, "popup", "search", "").unwrap();
    dispatch(&mut app, "search", "", "Mo").unwrap();
    assert!(html(&app, Region::Popup).contains("Enter 3 or more characters"));
    dispatch(&mut app, "search", "", "Motion").unwrap();
    let results = html(&app, Region::Popup);
    assert!(results.contains("data-act=\"search-go\""), "{results}");
    assert_eq!(app.ui.search, "Motion", "the typed text is kept");
    // Leaving the search closes it and its results.
    dispatch(&mut app, "popup-close", "", "").unwrap();
    assert_eq!(app.ui.popup, None);
    assert!(html(&app, Region::Popup).is_empty());
    drop(app.take_threads());
}

#[test]
fn the_wind_actual_power_readouts_follow_the_stored_maxima() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    backend.docs.lock().unwrap()["tune"]["power_center_wind_max_speed_perc"] = json!(73);
    app.refresh_docs();
    navigate(&mut app, Screen::Wind, 0);
    assert!(screens::main(&app).contains(">90%<"), "left / right");
    dispatch(&mut app, "section", "", "1").unwrap();
    assert!(screens::main(&app).contains(">73%<"), "center");
    drop(app.take_threads());
}

#[test]
fn the_page_has_only_widgets_of_known_classes() {
    let app = loaded(FakeBackend::new());
    let mut seen = BTreeSet::new();
    for index in 0..app.gui.widget_count() {
        let class = app.gui.node(crate::remi::WidgetId(index)).py_type.clone();
        assert!(
            ["BODY", "orc", "orc-loading", "orc-loading-message", "OrcRegion", "OrcAction"].contains(&class.as_str()),
            "widget {index} has a class of no interface widget: {class:?}"
        );
        seen.insert(class);
    }
    for class in ["BODY", "OrcRegion", "OrcAction"] {
        assert!(seen.contains(class), "no {class} constructed");
    }
}

/// What the browser sends to the action receiver.
fn act(app: &mut MyApp, parameters: &[(&str, &str)]) -> Result<(), String> {
    let receiver = app.gui.by_identifier(ACTION_ID).expect("the action receiver");
    let parameters: crate::remi::parameters::Parameters =
        parameters.iter().map(|(key, value)| ((*key).into(), (*value).into())).collect();
    app.callback(receiver, "onact", &parameters)
}

#[test]
fn the_action_receiver_binds_its_websocket_arguments() {
    let mut app = loaded(FakeBackend::new());
    act(&mut app, &[("act", "nav"), ("arg", "Games")]).unwrap();
    assert_eq!(app.ui.screen, Screen::Pc, "the name, its argument and an absent value");
    act(&mut app, &[("act", "screen"), ("value", "Telemetry")]).unwrap();
    assert_eq!(app.ui.screen, Screen::Telemetry);
    // Binding errors come before the action runs.
    let error = act(&mut app, &[("arg", "Games")]).unwrap_err();
    assert!(error.starts_with("TypeError: OrcAction.onact() missing 1 required positional argument"), "{error}");
    let error = act(&mut app, &[("act", "nav"), ("other", "x")]).unwrap_err();
    assert!(error.starts_with("TypeError: OrcAction.onact() got an unexpected keyword argument 'other'"), "{error}");
    assert_eq!(app.ui.screen, Screen::Telemetry);
    // A failing action is a warning on screen, not a failure of the call.
    act(&mut app, &[("act", "no-such-action")]).unwrap();
    assert_eq!(
        crate::app::tests::alert_text(&app).as_deref(),
        Some("An internal error occurred: unknown action \"no-such-action\"")
    );
    drop(app.take_threads());
}

#[test]
fn hapticr_is_in_the_rig_panel_for_each_true_value_of_its_setting() {
    for (stored, shown) in
        [(json!("y"), true), (json!("yes"), true), (json!(true), true), (json!("n"), false), (json!("bad"), false)]
    {
        let backend = FakeBackend::new();
        backend.docs.lock().unwrap()["settings"]["app"]["enable_hapticr"] = stored.clone();
        let mut app = loaded(backend.clone());
        assert_eq!(shell::devices(&app).contains("data-device=\"HapticR\""), shown, "{stored}");
        assert_eq!(app.ml.hardware_states.contains_key("HapticR"), shown, "{stored}: its status is recorded");
        drop(app.take_threads());
    }
}

// ---- Settings writes, the game setup check, dialog replacement and the whole search -----------------------

/// The settings writes (`i18n/reapply`, which writes the whole configuration file) the service was asked for.
fn settings_writes(backend: &FakeBackend) -> usize {
    backend.calls.lock().unwrap().iter().filter(|(m, p, _)| *m == Method::Post && p == "i18n/reapply").count()
}

/// A Settings change stays in memory until the user moves on; every way of moving on writes the settings
/// file, after the change.
#[test]
fn every_navigation_writes_the_settings_file() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    navigate(&mut app, Screen::App, 0);
    let before = settings_writes(&backend);
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "set", "App:measurement_unit", "1").unwrap();
    assert_eq!(settings_writes(&backend), 0, "a change alone is not written");
    dispatch(&mut app, "nav", "Tuning", "").unwrap();
    let calls = backend.calls.lock().unwrap().clone();
    let at = |path: &str, method: Method| calls.iter().rposition(|(m, p, _)| *m == method && p == path).unwrap();
    assert!(at("settings", Method::Patch) < at("i18n/reapply", Method::Post), "{calls:?}");
    assert!(before >= 1, "opening a screen wrote too");
    for (act, arg, value) in [
        ("nav", "Settings", ""),
        ("screen", "", "Shortcuts"),
        ("open-tuning", "", ""),
        ("search-go", "Hardware:0", ""),
        ("device", "MCMode", ""),
        ("profile", "", "manage"),
    ] {
        let count = settings_writes(&backend);
        dispatch(&mut app, act, arg, value).unwrap();
        assert_eq!(settings_writes(&backend), count + 1, "{act} {arg} {value}");
    }
    let count = settings_writes(&backend);
    app.goto_page_checked("Setup", "Hardware").unwrap();
    assert_eq!(settings_writes(&backend), count + 1, "a page opened by the program");
    drop(app.take_threads());
}

/// Each profile load asks the service to check the game's telemetry set-up (its "Game Page" / "Instructions"
/// notice); a pass without a load does not.
#[test]
fn each_profile_load_checks_the_game_setup() {
    use std::sync::atomic::Ordering;
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    assert_eq!(backend.game_setup_checks.load(Ordering::SeqCst), 1, "the first load");
    app.main_loop_pass();
    assert_eq!(backend.game_setup_checks.load(Ordering::SeqCst), 1, "no load, no check");
    backend.docs.lock().unwrap()["status"]["profile"]["generation"] = json!(9);
    app.main_loop_pass();
    assert_eq!(backend.game_setup_checks.load(Ordering::SeqCst), 2);
    // A failing check leaves the loop running and the next load checks again.
    backend.docs.lock().unwrap()["game_setup_check_failure"] = json!("RuntimeError: fixture game setup check");
    backend.docs.lock().unwrap()["status"]["profile"]["generation"] = json!(10);
    app.main_loop_pass();
    assert!(app.ml.fatal_error.is_none());
    assert_eq!(backend.game_setup_checks.load(Ordering::SeqCst), 3);
    drop(app.take_threads());
}

/// The capture operations (`hotkeys/capture`) the service was asked for, in order.
fn capture_calls(backend: &FakeBackend) -> Vec<Method> {
    backend.calls.lock().unwrap().iter().filter(|(_, p, _)| p == "hotkeys/capture").map(|(m, _, _)| *m).collect()
}

/// A dialog that replaces the shortcut editor (the launcher's profile, the sound fix, another editor) ends the
/// editor's capture first; a new editor starts its own after that.
#[test]
fn replacing_the_shortcut_editor_ends_its_capture() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    for replace in [
        |app: &mut MyApp| dialogs::open(app, dialogs::Dialog::Import { name: "profile.srs".into() }),
        |app: &mut MyApp| notices::apply_fix(app, 1, "default", "None"),
        |app: &mut MyApp| dialogs::open_companion(app),
    ] {
        shortcuts::open(&mut app, &["pitch".to_owned()]).unwrap();
        backend.calls.lock().unwrap().clear();
        replace(&mut app);
        assert!(!matches!(app.ui.dialog, Some(dialogs::Dialog::Shortcut(_))));
        assert_eq!(capture_calls(&backend), [Method::Delete]);
        dialogs::close(&mut app);
        assert_eq!(capture_calls(&backend), [Method::Delete], "closing the new dialog asks nothing more");
        drop(app.take_threads());
    }
    shortcuts::open(&mut app, &["pitch".to_owned()]).unwrap();
    backend.calls.lock().unwrap().clear();
    shortcuts::open(&mut app, &["roll".to_owned()]).unwrap();
    assert_eq!(capture_calls(&backend), [Method::Delete, Method::Post], "the second editor captures");
    // Every way of closing the editor ends the capture once.
    for (act, value) in [("dialog-close", ""), ("dialog-ok", "")] {
        shortcuts::open(&mut app, &["pitch".to_owned()]).unwrap();
        backend.calls.lock().unwrap().clear();
        dispatch(&mut app, act, "", value).unwrap();
        assert!(app.ui.dialog.is_none());
        assert_eq!(capture_calls(&backend), [Method::Delete], "{act}");
    }
    drop(app.take_threads());
}

/// The texts of markup: between tags and in text fields, without select choices, scripts or styles.
#[test]
fn page_texts_are_the_shown_texts_and_text_fields() {
    let html = "<div><h2>Credits</h2><p>Fish &amp; <b>chips</b>\n  here</p><select><option value=\"1\">Choice</option></select><input type=\"text\" value=\"192.168.1.2\"><input type=\"range\" value=\"5\"><input value=\"plain\"><script>var x;</script><textarea>notes</textarea></div>";
    assert_eq!(shell::page_texts(html), ["Credits", "Fish &", "chips", "here", "192.168.1.2", "plain", "notes"]);
}

/// The search finds what every screen shows, not only titles and labels: the games of the PC and Console
/// lists (each opening its list at the game's page), the telemetry channels, the settings groups and the
/// notes of the Settings screens; every result opens its screen and group; all matches are listed.
#[test]
fn search_finds_every_text_of_every_screen() {
    use super::screens::games::{Game, PAGE};
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    navigate(&mut app, Screen::Home, 0);
    let games: Vec<Game> = (0..PAGE + 2)
        .map(|i| Game {
            key: format!("g{i}"),
            name: if i == PAGE + 1 { "Zanzibar Rally".into() } else { format!("Game {i}") },
            status: "ready_to_play".into(),
            entry_type: "ready_to_play".into(),
            ..Game::default()
        })
        .collect();
    for list in [&mut app.ui.devices.games.pc, &mut app.ui.devices.games.console] {
        list.games = games.clone();
        list.loaded = true;
    }
    // A filtered PC list shows none of the games; the Console list shows its first page.
    app.ui.devices.games.pc.filter = "other".into();
    let search = |app: &mut MyApp, text: &str| {
        dispatch(app, "popup-close", "", "").unwrap();
        dispatch(app, "popup", "search", "").unwrap();
        dispatch(app, "search", "", text).unwrap();
        shell::search_results(app)
    };
    // A game: on PC and on Console, at the list's second page.
    let found = search(&mut app, "zanzibar");
    let places: Vec<(Screen, Option<usize>)> = found.iter().map(|r| (r.screen, r.games_page)).collect();
    assert_eq!(
        places,
        [(Screen::Pc, Some(1)), (Screen::Console, Some(1))],
        "{:?}",
        found.iter().map(|r| &r.text).collect::<Vec<_>>()
    );
    assert!(html(&app, Region::Popup).contains("data-arg=\"PC:0:1\""));
    // A game the list shows now is listed once, with its page.
    let found = search(&mut app, "game 0");
    let places: Vec<(Screen, Option<usize>)> = found.iter().map(|r| (r.screen, r.games_page)).collect();
    assert_eq!(places, [(Screen::Pc, Some(0)), (Screen::Console, Some(0))]);
    dispatch(&mut app, "search-go", "Console:0:1", "").unwrap();
    assert_eq!(app.ui.screen, Screen::Console);
    let list = &app.ui.devices.games.console;
    assert_eq!((list.page, list.filter.as_str()), (1, ""), "every game, at the game's page");
    // (The fake service lists no games: the list read when the screen opened is put back.)
    app.ui.devices.games.console.games = games.clone();
    assert!(screens::main(&app).contains("Zanzibar Rally"), "the list shows the game");
    // A game's status is found too.
    assert!(search(&mut app, "game is ready").iter().any(|r| r.screen == Screen::Pc));
    // A telemetry channel row.
    let channel = crate::catalog::channels().into_iter().find(|c| c.display && c.label.len() > 6).unwrap();
    let found = search(&mut app, channel.label);
    assert!(found.iter().any(|r| r.screen == Screen::Telemetry && r.text == channel.label), "{}", channel.label);
    // Settings groups and the Settings screens' texts.
    let app_page = super::screens::app::page();
    for group in ["Credits", "Measurement"] {
        let number = app_page.sections.iter().position(|s| s.title == group).unwrap();
        let found = search(&mut app, group);
        let result =
            found.iter().find(|r| r.screen == Screen::App && r.text == group).unwrap_or_else(|| panic!("{group}"));
        let same = |r: &&shell::SearchResult| (r.screen, r.section) == (Screen::App, number);
        assert_eq!(found.iter().filter(same).count(), 1, "{group} once, whatever the theme's capitals");
        assert_eq!(result.section, number, "{group}");
        dispatch(&mut app, "search-go", &format!("App:{}", result.section), "").unwrap();
        assert_eq!((app.ui.screen, app.ui.section), (Screen::App, number));
        assert!(screens::main(&app).contains(group), "the group is shown");
    }
    // A button's text and a note, shown only in their group.
    let reset = app_page.sections.iter().position(|s| s.title == "Reset").unwrap();
    let found = search(&mut app, "restore factory");
    assert!(found.iter().any(|r| (r.screen, r.section) == (Screen::App, reset)), "the Reset button");
    let found = search(&mut app, "firelight");
    assert!(found.iter().any(|r| r.screen == Screen::App), "the Credits note");
    let network = app_page.sections.iter().position(|s| s.title == "Network").unwrap();
    let found = search(&mut app, "show qr code");
    assert!(found.iter().any(|r| (r.screen, r.section) == (Screen::App, network)), "the companion button");
    // Every match is listed, however many.
    let found = search(&mut app, "ion");
    assert!(found.len() > 30, "{}", found.len());
    assert_eq!(html(&app, Region::Popup).matches("data-act=\"search-go\"").count(), found.len());
    // The screen and group in use stay as they were.
    assert_eq!(app.ui.screen, Screen::App);
    drop(app.take_threads());
}
