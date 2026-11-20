//! Tests of Settings → Debug: the custom-feature gate and Reload.

use std::sync::atomic::{AtomicUsize, Ordering};

use openrigcue_model::native_value::LegacyValue;
use serde_json::json;

use super::*;
use crate::app::tests::{loaded, FakeBackend};
use crate::backend::{Backend, Failure, Method};
use crate::host::UiApp;
use crate::ui::{dispatch, navigate, Group, Screen, Theme};

/// The captured lines are shared by every test of the screen.
static CAPTURED_TEST: Mutex<()> = Mutex::new(());

#[test]
fn debug_is_offered_only_while_the_custom_feature_is_on() {
    for (value, offered) in [(json!(true), true), (json!("yes"), true), (json!(false), false), (json!("bad"), false)] {
        let backend = FakeBackend::new();
        backend.docs.lock().unwrap()["settings"]["features"]["show_debug_page"] = value.clone();
        let mut app = loaded(backend);
        assert_eq!(Screen::of_group(Group::Settings, &app).contains(&Screen::Debug), offered, "{value}");
        navigate(&mut app, Screen::Debug, 0);
        assert_eq!(app.ui.screen, if offered { Screen::Debug } else { Screen::App }, "{value}");
        drop(app.take_threads());
    }
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["settings"]["features"].as_object_mut().unwrap().remove("show_debug_page");
    let app = loaded(backend);
    assert!(!enabled(&app), "a missing feature keeps the screen hidden");
}

#[test]
fn reload_shows_the_captured_output() {
    let _captured = CAPTURED_TEST.lock().unwrap();
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["settings"]["features"]["show_debug_page"] = json!(true);
    let mut app = loaded(backend);
    navigate(&mut app, Screen::Debug, 0);
    assert!(main(&app).contains("Press Reload to show the captured output."));
    dispatch(&mut app, "action", "Debug:reload:", "").unwrap();
    assert_eq!(app.ui.debug.output.as_deref(), Some(""));
    assert!(main(&app).contains("No output has been captured."));
    CAPTURED.lock().unwrap().extend(["<one>".to_owned(), "&two".to_owned()]);
    dispatch(&mut app, "action", "Debug:reload:", "").unwrap();
    let html = main(&app);
    assert!(html.contains("<p class=\"mono\">&lt;one&gt;</p><p class=\"mono\">&amp;two</p>"), "{html}");
    CAPTURED.lock().unwrap().clear();
    for theme in Theme::ALL {
        app.ui.theme = theme;
        assert!(main(&app).contains("data-arg=\"Debug:reload:\""), "{theme:?}");
    }
    drop(app.take_threads());
}

/// The fake service whose `show_debug_page` custom feature is `value`.
struct DebugBackend {
    inner: std::sync::Arc<FakeBackend>,
    value: Result<LegacyValue, Failure>,
    reads: AtomicUsize,
}

impl Backend for DebugBackend {
    fn call(&self, method: Method, path: &str, body: Option<serde_json::Value>) -> Result<serde_json::Value, Failure> {
        self.inner.call(method, path, body)
    }
    fn native_setting(&self, section: &str, key: &str) -> Result<LegacyValue, Failure> {
        if (section, key) == ("CUSTOM_FEATURES", "show_debug_page") {
            self.reads.fetch_add(1, Ordering::SeqCst);
            self.value.clone()
        } else {
            self.inner.native_setting(section, key)
        }
    }
    fn upload(&self, path: &str, bytes: Vec<u8>) -> Result<serde_json::Value, Failure> {
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

/// The app on `value` as the stored feature (the projected settings say the opposite of `projected`).
fn with_feature(value: Result<LegacyValue, Failure>, projected: bool) -> (MyApp, std::sync::Arc<DebugBackend>) {
    let inner = FakeBackend::new();
    inner.docs.lock().unwrap()["settings"]["features"]["show_debug_page"] = json!(projected);
    let backend = std::sync::Arc::new(DebugBackend { inner, value, reads: AtomicUsize::new(0) });
    let mut app = MyApp::new(backend.clone());
    let body = app.gui.create("body", "BODY");
    let root = app.main(body);
    app.gui.set_root(root);
    crate::remi::gui::dispatch(&mut app, body, "onload", &Default::default());
    app.main_loop_pass();
    drop(app.take_threads());
    (app, backend)
}

#[test]
fn the_gate_reads_the_stored_feature_value() {
    for (value, offered) in [
        (LegacyValue::Bool(true), true),
        (LegacyValue::Bool(false), false),
        (LegacyValue::Str("YeS".into()), true),
        (LegacyValue::Str("no".into()), false),
        (LegacyValue::Str("Y".into()), true),
        (LegacyValue::Str("n".into()), false),
        (LegacyValue::Int(1), true),
        (LegacyValue::Int(0), false),
    ] {
        let (app, backend) = with_feature(Ok(value.clone()), !offered);
        backend.reads.store(0, Ordering::SeqCst);
        assert_eq!(Screen::of_group(Group::Settings, &app).contains(&Screen::Debug), offered, "{value:?}");
        assert!(backend.reads.load(Ordering::SeqCst) > 0, "the stored value is read, not the projected settings");
    }
}

#[test]
fn an_unreadable_feature_hides_the_screen_and_the_start_goes_on() {
    let failure = |detail: &str| Err(Failure { status: 500, detail: detail.into() });
    for value in [
        failure("KeyError: 'CUSTOM_FEATURES'"),
        failure("KeyError: 'show_debug_page'"),
        Ok(LegacyValue::Str("bad".into())),
        Ok(LegacyValue::Str("ON".into())),
        Ok(LegacyValue::Str("off".into())),
        Ok(LegacyValue::Int(2)),
        Ok(LegacyValue::None),
        Ok(LegacyValue::Float(1.0)),
        Ok(LegacyValue::List(vec![])),
        Ok(LegacyValue::Tuple(vec![])),
    ] {
        let shown = format!("{value:?}");
        let (mut app, _) = with_feature(value, true);
        assert!(!Screen::of_group(Group::Settings, &app).contains(&Screen::Debug), "{shown}");
        assert!(Screen::of_group(Group::Settings, &app).contains(&Screen::App), "{shown}");
        navigate(&mut app, Screen::Debug, 0);
        assert_eq!(app.ui.screen, Screen::App, "{shown}");
        drop(app.take_threads());
    }
}

#[test]
fn the_output_is_shown_as_captured_without_translation() {
    let _captured = CAPTURED_TEST.lock().unwrap();
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["settings"]["features"]["show_debug_page"] = json!(true);
    backend.docs.lock().unwrap()["translations"] = json!({"<one>": "translated one", "&two": "translated two"});
    let mut app = loaded(backend);
    navigate(&mut app, Screen::Debug, 0);
    CAPTURED.lock().unwrap().extend(["<one>".to_owned(), "&two".to_owned()]);
    dispatch(&mut app, "action", "Debug:reload:", "").unwrap();
    let html = main(&app);
    assert!(html.contains("<p class=\"mono\">&lt;one&gt;</p><p class=\"mono\">&amp;two</p>"), "{html}");
    assert!(!html.contains("translated"));
    CAPTURED.lock().unwrap().clear();
    dispatch(&mut app, "action", "Debug:reload:", "").unwrap();
    assert!(main(&app).contains("No output has been captured."));
    drop(app.take_threads());
}
