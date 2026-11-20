//! Tests of Tuning → IFlag: the flag switches, the credits, and the flags the running game does not support.

use serde_json::json;

use super::*;
use crate::app::tests::{loaded, FakeBackend};
use crate::host::UiApp;
use crate::ui::{dispatch, navigate};

fn iflag(backend: &std::sync::Arc<FakeBackend>, game: &str) -> MyApp {
    backend.docs.lock().unwrap()["status"]["session"]["game"] = json!(game);
    let mut app = loaded(backend.clone());
    navigate(&mut app, Screen::IFlag, 0);
    app
}

#[test]
fn flags_are_stored_as_booleans_and_the_credits_show() {
    let backend = FakeBackend::new();
    let mut app = iflag(&backend, "acc");
    let html = main(&app);
    for group in ["RPM", "Start Lights", "Flags", "Pits", "Spotter", "Credits"] {
        assert!(html.contains(&format!(">{group}</option>")), "{group}");
    }
    let flag =
        page().controls().find(|c| c.field.is_some() && app.game_feature(&c.id) != Some(false)).unwrap().id.clone();
    dispatch(&mut app, "set", &format!("IFlag:{flag}"), "1").unwrap();
    assert_eq!(backend.docs.lock().unwrap()["tune"]["iflag"][&flag], json!(true));
    dispatch(&mut app, "set", &format!("IFlag:{flag}"), "0").unwrap();
    assert_eq!(backend.docs.lock().unwrap()["tune"]["iflag"][&flag], json!(false));
    let credits = page().sections.iter().position(|s| s.title == "Credits").unwrap();
    dispatch(&mut app, "section", "", &credits.to_string()).unwrap();
    assert!(main(&app).contains("The iFlag is a design by Petr Vostřel."));
    drop(app.take_threads());
}

#[test]
fn unsupported_flags_are_disabled_and_switched_off_on_a_profile_load() {
    let backend = FakeBackend::new();
    let game = super::super::tuning::game_without("rpm");
    backend.docs.lock().unwrap()["tune"]["iflag"]["rpm"] = json!(true);
    let mut app = iflag(&backend, &game);
    assert_eq!(backend.docs.lock().unwrap()["tune"]["iflag"]["rpm"], json!(false), "the profile load switched it off");
    assert!(main(&app).contains("Not used by this game"));
    dispatch(&mut app, "set", "IFlag:rpm", "1").unwrap();
    assert_eq!(backend.docs.lock().unwrap()["tune"]["iflag"]["rpm"], json!(false), "it cannot be switched on");
    drop(app.take_threads());
}

/// A flag the game `acc` supports and the settings group it is in.
fn supported_flag(app: &MyApp) -> (String, usize) {
    let page = page();
    page.sections
        .iter()
        .enumerate()
        .find_map(|(index, section)| {
            section
                .controls
                .iter()
                .find(|c| c.field.is_some() && app.game_feature(&c.id) != Some(false))
                .map(|c| (c.id.clone(), index))
        })
        .unwrap()
}

#[test]
fn a_flag_write_without_the_flag_table_fails_and_stores_nothing() {
    let backend = FakeBackend::new();
    let mut app = iflag(&backend, "acc");
    let (flag, _) = supported_flag(&app);
    // The service's profile changed and the documents are not read again yet.
    backend.docs.lock().unwrap()["tune"]["iflag"] = serde_json::Value::Null;
    backend.calls.lock().unwrap().clear();
    let error = dispatch(&mut app, "set", &format!("IFlag:{flag}"), "1").unwrap_err();
    assert!(error.starts_with("TypeError:"), "{error}");
    assert!(backend.patches("profile").is_empty());
    assert_eq!(backend.docs.lock().unwrap()["tune"]["iflag"], serde_json::Value::Null);
    app.refresh_docs();
    assert!(dispatch(&mut app, "set", &format!("IFlag:{flag}"), "0").unwrap_err().starts_with("TypeError:"));
    assert!(backend.patches("profile").is_empty());
    drop(app.take_threads());
}

#[test]
fn stored_flags_show() {
    let backend = FakeBackend::new();
    let mut app = iflag(&backend, "acc");
    let (flag, section) = supported_flag(&app);
    app.ui.section = section;
    for value in [true, false, true] {
        backend.docs.lock().unwrap()["tune"]["iflag"][&flag] = json!(value);
        app.refresh_docs();
        let html = main(&app);
        let id = format!("id=\"c-iflag-{flag}\"");
        let start = html.find(&id).unwrap();
        let switch = &html[start..start + html[start..].find('>').unwrap()];
        assert_eq!(switch.contains(" checked"), value, "{switch}");
    }
    drop(app.take_threads());
}
