//! Tests of Settings → Shortcuts: the service's table by page, the search, Assign through the shortcut editor
//! and the stored bindings.

use std::sync::Arc;

use openrigcue_model::native_value::LegacyValue;
use serde_json::{json, Value};

use super::*;
use crate::app::tests::{loaded, FakeBackend};
use crate::backend::{Backend, Failure, Method};
use crate::host::UiApp;
use crate::ui::dialogs::Dialog;
use crate::ui::{dispatch, navigate, Theme};

/// The fixture service with a shortcut table.
struct HotkeysBackend {
    inner: Arc<FakeBackend>,
    view: Value,
}

impl Backend for HotkeysBackend {
    fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value, Failure> {
        if method == Method::Get && path == "hotkeys" {
            self.inner.calls.lock().unwrap().push((method, path.to_owned(), None));
            return Ok(self.view.clone());
        }
        self.inner.call(method, path, body)
    }
    fn native_setting(&self, section: &str, key: &str) -> Result<LegacyValue, Failure> {
        self.inner.native_setting(section, key)
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
        self.inner.excluded_strings()
    }
}

fn action(name: &str, label: &str, keys: &str, joystick: &str, button: &str) -> Value {
    json!({"action": name, "label": label, "keys": keys, "joystick": joystick, "button": button,
           "buttons": if button.is_empty() { String::new() } else { format!("{joystick}^{button}") }})
}

fn view(denied: bool) -> Value {
    json!({
        "joysticks": ["Wheel"],
        "capturing": false,
        "keyboard_denied": denied,
        "groups": [
            {"page": "Wind", "actions": [action("power_wind_up", "Wind Power Up", "ctrl+w", "", "")]},
            {"page": "Motion", "actions": [
                action("motion_box_switch", "Motion On/Off", "", "Wheel", "7"),
                action("motion_compensation_switch", "Motion Compensation On/Off", "", "", "")
            ]},
            {"page": "HapticR", "actions": [action("haptic_switch", "HapticR On/Off", "", "", "")]},
            {"page": "Motion Compensation", "actions": [
                action("motion_compensation_switch", "Motion Compensation On/Off", "", "", ""),
                action("motion_compensation_recenter", "Re-center", "", "", "")
            ]}
        ]
    })
}

fn on_shortcuts(backend: &Arc<FakeBackend>, denied: bool) -> MyApp {
    let mut app = loaded(backend.clone());
    app.backend = Arc::new(HotkeysBackend { inner: backend.clone(), view: view(denied) });
    navigate(&mut app, Screen::Shortcuts, 0);
    drop(app.take_threads());
    app
}

#[test]
fn the_table_lists_each_page_once_and_leaves_out_switched_off_screens() {
    let backend = FakeBackend::new();
    let app = on_shortcuts(&backend, false);
    let (groups, denied) = table(&app);
    let pages: Vec<&str> = groups.iter().map(|(page, _)| page.as_str()).collect();
    assert_eq!(pages, ["Wind", "Motion", "Motion Compensation"], "HapticR is off");
    assert!(!denied);
    let compensation = &groups[2].1;
    assert_eq!(compensation.len(), 1, "an action listed by two pages stays with the first");
    assert_eq!(compensation[0].action, "motion_compensation_recenter");
}

#[test]
fn the_screen_shows_the_selected_page_and_the_bindings() {
    let backend = FakeBackend::new();
    let mut app = on_shortcuts(&backend, false);
    let wind = main(&app);
    assert!(wind.contains(">Wind</option>") && wind.contains(">Motion</option>"));
    assert!(wind.contains("Wind Power Up") && wind.contains("ctrl+w"));
    assert!(!wind.contains("Motion On/Off"));
    assert!(wind.contains("data-arg=\"Shortcuts:power_wind_up:\""));
    dispatch(&mut app, "section", "", "1").unwrap();
    let motion = main(&app);
    assert!(motion.contains("Motion On/Off") && motion.contains("Wheel · 7"));
    assert!(motion.contains("Unassigned"), "an action without a binding says so");
    assert!(!motion.contains("keyboard access rule"));
}

#[test]
fn the_search_looks_through_every_page() {
    let backend = FakeBackend::new();
    let mut app = on_shortcuts(&backend, false);
    dispatch(&mut app, "set", "Shortcuts:filter", "re-cen").unwrap();
    assert_eq!(app.ui.shortcuts.filter, "re-cen");
    let html = main(&app);
    assert!(html.contains("Re-center") && !html.contains("Wind Power Up"));
    assert!(!html.contains("id=\"section-picker\""), "the page picker steps aside while searching");
    assert!(html.contains("value=\"re-cen\""), "the field keeps the text");
    dispatch(&mut app, "set", "Shortcuts:filter", "Motion").unwrap();
    let html = main(&app);
    assert!(html.contains("Motion On/Off") && html.contains("Re-center"), "a page name finds its actions");
    dispatch(&mut app, "set", "Shortcuts:filter", "nothing like this").unwrap();
    assert!(main(&app).contains("No matches"));
    dispatch(&mut app, "set", "Shortcuts:filter", "").unwrap();
    assert!(main(&app).contains("id=\"section-picker\""));
}

#[test]
fn assign_opens_the_editor_and_confirm_stores_the_binding() {
    let backend = FakeBackend::new();
    let mut app = on_shortcuts(&backend, false);
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "action", "Shortcuts:power_wind_up:", "").unwrap();
    let Some(Dialog::Shortcut(mut editor)) = app.ui.dialog.clone() else { panic!("the shortcut editor") };
    assert_eq!(editor.actions, vec![("power_wind_up".to_owned(), "Wind Power Up".to_owned())]);
    assert!(backend.calls.lock().unwrap().iter().any(|(m, p, _)| *m == Method::Post && p == "hotkeys/capture"));
    editor.keys = "ctrl+alt+w".into();
    editor.joystick = "Wheel".into();
    editor.button = "3".into();
    app.ui.dialog = Some(Dialog::Shortcut(editor));
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert!(app.ui.dialog.is_none());
    let calls = backend.calls.lock().unwrap();
    assert!(calls.iter().any(|(m, p, _)| *m == Method::Delete && p == "hotkeys/capture"));
    let written: Vec<&Value> = calls
        .iter()
        .filter(|(m, p, _)| *m == Method::Patch && p == "settings")
        .filter_map(|(_, _, b)| b.as_ref())
        .collect();
    let text = format!("{written:?}");
    assert!(text.contains("ctrl+alt+w") && text.contains("Wheel^3"), "{text}");
    drop(calls);
    // An action the service does not list has no editor.
    assert!(dispatch(&mut app, "action", "Shortcuts:no_such_action:", "").is_err());
    drop(app.take_threads());
}

#[test]
fn a_keyboard_without_access_is_explained() {
    let backend = FakeBackend::new();
    let app = on_shortcuts(&backend, true);
    assert!(main(&app).contains("keyboard access rule"));
}

#[test]
fn every_theme_draws_the_screen() {
    let backend = FakeBackend::new();
    let mut app = on_shortcuts(&backend, false);
    for theme in Theme::ALL {
        app.ui.theme = theme;
        assert!(main(&app).contains("Wind Power Up"), "{theme:?}");
    }
}

// ---- The shortcut editor ----------------------------------------------------------------------------------

/// A service whose shortcut table can change, answering the capture reads from a queue and refusing the
/// binding write named in `fail_assignment`; it records the reads and writes of the bindings.
struct EditorBackend {
    inner: Arc<FakeBackend>,
    view: std::sync::Mutex<Value>,
    captures: std::sync::Mutex<std::collections::VecDeque<Result<Value, Failure>>>,
    fail_assignment: std::sync::Mutex<Option<String>>,
    operations: std::sync::Mutex<Vec<String>>,
}

impl Backend for EditorBackend {
    fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value, Failure> {
        self.operations.lock().unwrap().push(format!("{method:?} {path}"));
        if method == Method::Get && path == "hotkeys" {
            return Ok(self.view.lock().unwrap().clone());
        }
        if method == Method::Get && path.starts_with("hotkeys/capture?") {
            return self.captures.lock().unwrap().pop_front().unwrap_or_else(|| Ok(json!({})));
        }
        self.inner.call(method, path, body)
    }
    fn native_setting(&self, section: &str, key: &str) -> Result<LegacyValue, Failure> {
        self.operations.lock().unwrap().push(format!("read {section}.{key}"));
        self.inner.native_setting(section, key)
    }
    fn assign_setting_value(&self, section: &str, key: &str, value: &LegacyValue) -> Result<(), Failure> {
        self.operations.lock().unwrap().push(format!("write {section}.{key}"));
        if self.fail_assignment.lock().unwrap().as_deref() == Some(key) {
            return Err(Failure { status: 500, detail: "fixture assignment failure".into() });
        }
        self.inner.assign_setting_value(section, key, value)
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
        self.inner.excluded_strings()
    }
}

/// The service's shortcut table for the editor: two actions, the connected button boxes and the keyboard
/// access.
fn editor_view(joysticks: &[&str], denied: bool) -> Value {
    json!({"groups": [{"page": "Joystick", "actions": [
        {"action": "joystick_switch", "label": "Toggle Joystick", "keys": "", "buttons": ""},
        {"action": "telemetry_capture", "label": "Toggle Telemetry Capture", "keys": "typed-stale", "buttons": ""}
    ]}], "joysticks": joysticks, "keyboard_denied": denied})
}

fn editor_app(joysticks: &[&str], denied: bool) -> (MyApp, Arc<FakeBackend>, Arc<EditorBackend>) {
    let inner = FakeBackend::new();
    let mut app = loaded(inner.clone());
    drop(app.take_threads());
    let backend = Arc::new(EditorBackend {
        inner: inner.clone(),
        view: std::sync::Mutex::new(editor_view(joysticks, denied)),
        captures: Default::default(),
        fail_assignment: Default::default(),
        operations: Default::default(),
    });
    app.backend = backend.clone();
    (app, inner, backend)
}

/// Stores an action's bindings in the service (`None` leaves a field out; both `None` removes the action).
fn bind(inner: &FakeBackend, action: &str, keys: Option<&str>, buttons: Option<&str>) {
    let mut docs = inner.docs.lock().unwrap();
    let bindings = docs["settings"]["shortcuts"]["bindings"].as_object_mut().unwrap();
    if keys.is_none() && buttons.is_none() {
        bindings.remove(action);
        return;
    }
    let mut binding = serde_json::Map::new();
    if let Some(keys) = keys {
        binding.insert("keys".into(), json!(keys));
    }
    if let Some(buttons) = buttons {
        binding.insert("buttons".into(), json!(buttons));
    }
    bindings.insert(action.into(), Value::Object(binding));
}

fn open_editor(app: &mut MyApp, actions: &[&str]) {
    let actions: Vec<String> = actions.iter().map(|a| (*a).to_owned()).collect();
    crate::ui::shortcuts::open(app, &actions).unwrap();
}

fn editor(app: &MyApp) -> crate::ui::shortcuts::Editor {
    match &app.ui.dialog {
        Some(Dialog::Shortcut(editor)) => editor.clone(),
        other => panic!("the shortcut editor, not {other:?}"),
    }
}

fn dialog(app: &MyApp) -> String {
    crate::ui::render(app, crate::ui::Region::Dialog)
}

fn stored_binding(backend: &EditorBackend, key: &str) -> Option<LegacyValue> {
    backend.inner.native_setting("SHORTCUTS", key).ok()
}

#[test]
fn the_editor_opens_on_a_missing_binding_with_empty_fields_and_starts_the_capture() {
    let (mut app, inner, backend) = editor_app(&["Wheel"], false);
    bind(&inner, "joystick_switch", None, None);
    inner.calls.lock().unwrap().clear();
    open_editor(&mut app, &["joystick_switch"]);
    let editor = editor(&app);
    assert_eq!(editor.actions, vec![("joystick_switch".to_owned(), "Toggle Joystick".to_owned())]);
    assert_eq!((editor.keys.as_str(), editor.joystick.as_str(), editor.button.as_str()), ("", "None", ""));
    assert!(backend.operations.lock().unwrap().contains(&"read SHORTCUTS.joystick_switch_keys".to_owned()));
    assert!(inner.calls.lock().unwrap().iter().any(|(m, p, _)| *m == Method::Post && p == "hotkeys/capture"));
    assert!(dialog(&app).contains("Toggle Joystick"));
    drop(app.take_threads());
}

#[test]
fn confirm_ends_the_capture_and_stores_the_keys_then_the_buttons() {
    let (mut app, inner, backend) = editor_app(&["Wheel"], false);
    bind(&inner, "telemetry_capture", Some("old"), Some("Wheel^2"));
    open_editor(&mut app, &["telemetry_capture"]);
    let mut edited = editor(&app);
    edited.keys = "new".into();
    app.ui.dialog = Some(Dialog::Shortcut(edited));
    *backend.fail_assignment.lock().unwrap() = Some("telemetry_capture_btns".into());
    backend.operations.lock().unwrap().clear();
    inner.calls.lock().unwrap().clear();
    assert_eq!(dispatch(&mut app, "dialog-ok", "", ""), Err("fixture assignment failure".into()));
    assert!(app.ui.dialog.is_none());
    assert!(inner.calls.lock().unwrap().iter().any(|(m, p, _)| *m == Method::Delete && p == "hotkeys/capture"));
    assert_eq!(stored_binding(&backend, "telemetry_capture_keys"), Some("new".into()), "the keys are kept");
    let operations = backend.operations.lock().unwrap().clone();
    let at = |op: &str| operations.iter().position(|o| o == op).unwrap_or_else(|| panic!("{op}: {operations:?}"));
    assert!(at("write SHORTCUTS.telemetry_capture_keys") < at("write SHORTCUTS.telemetry_capture_btns"));
    assert!(
        operations.iter().position(|o| o == "Delete hotkeys/capture").unwrap()
            < at("write SHORTCUTS.telemetry_capture_keys")
    );
    // A binding without buttons: unchanged keys are stored again and the buttons are created empty.
    *backend.fail_assignment.lock().unwrap() = None;
    bind(&inner, "telemetry_capture", Some("same"), None);
    open_editor(&mut app, &["telemetry_capture"]);
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert_eq!(stored_binding(&backend, "telemetry_capture_keys"), Some("same".into()));
    assert_eq!(stored_binding(&backend, "telemetry_capture_btns"), Some("".into()));
    // A button box and its button are stored as `<box>^<button>`.
    open_editor(&mut app, &["telemetry_capture"]);
    dispatch(&mut app, "dialog-field", "joystick", "1").unwrap();
    let mut edited = editor(&app);
    edited.button = "5".into();
    app.ui.dialog = Some(Dialog::Shortcut(edited));
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert_eq!(stored_binding(&backend, "telemetry_capture_btns"), Some("Wheel^5".into()));
    drop(app.take_threads());
}

#[test]
fn without_button_boxes_the_list_is_disabled_and_confirm_stores_an_empty_button_binding() {
    let (mut app, inner, backend) = editor_app(&[], false);
    bind(&inner, "telemetry_capture", Some("ctrl+alt+t"), Some(""));
    open_editor(&mut app, &["telemetry_capture"]);
    let html = dialog(&app);
    assert!(
        html.contains("<select id=\"shortcut-joystick\" class=\"field-input\" disabled><option>None</option></select>")
    );
    assert_eq!(editor(&app).joystick, "None");
    backend.operations.lock().unwrap().clear();
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert_eq!(stored_binding(&backend, "telemetry_capture_keys"), Some("ctrl+alt+t".into()));
    assert_eq!(stored_binding(&backend, "telemetry_capture_btns"), Some("".into()));
    assert!(backend.operations.lock().unwrap().contains(&"write SHORTCUTS.telemetry_capture_btns".to_owned()));
    // A button box connected since: the next opening offers it.
    *backend.view.lock().unwrap() = editor_view(&["Wheel"], false);
    open_editor(&mut app, &["telemetry_capture"]);
    let html = dialog(&app);
    let start = html.find("<select id=\"shortcut-joystick\"").unwrap();
    let select = &html[start..start + html[start..].find('>').unwrap()];
    assert!(!select.contains("disabled"), "{select}");
    assert!(html.contains("<option value=\"1\">Wheel</option>"));
    drop(app.take_threads());
}

#[test]
fn the_editor_shows_the_stored_binding_rather_than_the_table() {
    let (mut app, inner, _backend) = editor_app(&["Wheel"], false);
    bind(&inner, "telemetry_capture", Some("ctrl+alt+t"), Some("Wheel^7"));
    open_editor(&mut app, &["telemetry_capture"]);
    let opened = editor(&app);
    assert_eq!((opened.keys.as_str(), opened.joystick.as_str(), opened.button.as_str()), ("ctrl+alt+t", "Wheel", "7"));
    let html = dialog(&app);
    assert!(html.contains("value=\"ctrl+alt+t\"") && !html.contains("typed-stale"));
    assert!(html.contains("<option value=\"1\" selected>Wheel</option>") && html.contains("value=\"7\""));
    // Without a button binding, the fields are empty and the box is None.
    bind(&inner, "telemetry_capture", Some("ctrl+alt+t"), None);
    dispatch(&mut app, "dialog-field", "action", "0").unwrap();
    let reloaded = editor(&app);
    assert_eq!((reloaded.joystick.as_str(), reloaded.button.as_str()), ("None", ""));
    // A button box that is not connected is not kept.
    bind(&inner, "telemetry_capture", Some("ctrl+alt+t"), Some("Pedals^3"));
    dispatch(&mut app, "dialog-field", "action", "0").unwrap();
    let reloaded = editor(&app);
    assert_eq!((reloaded.joystick.as_str(), reloaded.button.as_str()), ("None", ""));
    // The button is the part between the first and a second `^`.
    bind(&inner, "telemetry_capture", Some("ctrl+alt+t"), Some("Wheel^7^ignored"));
    dispatch(&mut app, "dialog-field", "action", "0").unwrap();
    let reloaded = editor(&app);
    assert_eq!((reloaded.joystick.as_str(), reloaded.button.as_str()), ("Wheel", "7"));
    drop(app.take_threads());
}

#[test]
fn the_backend_keeps_missing_bindings_missing_and_updates_one_field() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["settings"]["shortcuts"]["bindings"]
        .as_object_mut()
        .unwrap()
        .remove("joystick_switch");
    assert_eq!(
        crate::backend::projected_native_setting(backend.as_ref(), "SHORTCUTS", "joystick_switch_keys")
            .unwrap_err()
            .detail,
        "KeyError: 'joystick_switch_keys'"
    );
    let before =
        backend.docs.lock().unwrap()["settings"]["shortcuts"]["bindings"]["telemetry_capture"]["buttons"].clone();
    backend.assign_setting_value("SHORTCUTS", "telemetry_capture_keys", &"alt+t".into()).unwrap();
    assert_eq!(backend.native_setting("SHORTCUTS", "telemetry_capture_keys").unwrap(), "alt+t".into());
    assert_eq!(
        backend.docs.lock().unwrap()["settings"]["shortcuts"]["bindings"]["telemetry_capture"]["buttons"],
        before
    );
}

#[test]
fn choosing_a_button_box_warns_about_force_feedback_in_the_chosen_language() {
    let warning = "Force feedback may stop working if OpenRigCue is restarted while a game runs";
    let (mut app, inner, _backend) = editor_app(&["Wheel"], false);
    bind(&inner, "telemetry_capture", Some("ctrl+t"), Some(""));
    open_editor(&mut app, &["telemetry_capture"]);
    assert!(!dialog(&app).contains(warning), "no box, no warning");
    dispatch(&mut app, "dialog-field", "joystick", "1").unwrap();
    assert_eq!(editor(&app).joystick, "Wheel");
    assert!(dialog(&app).contains(warning));
    dispatch(&mut app, "dialog-field", "joystick", "0").unwrap();
    assert!(!dialog(&app).contains(warning));
    inner.docs.lock().unwrap()["translations"] = json!({ warning: format!("translated: {warning}") });
    dispatch(&mut app, "dialog-field", "joystick", "1").unwrap();
    assert!(dialog(&app).contains(&format!("translated: {warning}")));
    dispatch(&mut app, "dialog-close", "", "").unwrap();
    assert!(app.ui.dialog.is_none());
    drop(app.take_threads());
}

#[test]
fn the_editor_explains_a_keyboard_without_access_each_time_it_opens() {
    let (mut app, inner, backend) = editor_app(&["Wheel"], false);
    bind(&inner, "telemetry_capture", Some("ctrl+t"), Some(""));
    for denied in [false, true, false] {
        *backend.view.lock().unwrap() = editor_view(&["Wheel"], denied);
        open_editor(&mut app, &["telemetry_capture"]);
        let html = dialog(&app);
        assert_eq!(html.contains("keyboard access rule"), denied, "denied={denied}");
        if denied {
            assert!(html.contains("Keyboard shortcuts need the keyboard access rule to be enabled"));
            assert!(html.contains("/etc/udev/rules.d/70-openrigcue.rules"));
            assert!(html.contains("lets every program running as your user read your keystrokes"));
            assert!(html.contains("Controller and button box shortcuts are not affected."));
        }
        dispatch(&mut app, "dialog-close", "", "").unwrap();
    }
    drop(app.take_threads());
}

#[test]
fn the_type_list_shows_the_service_labels_and_another_action_shows_its_binding() {
    let (mut app, inner, _backend) = editor_app(&["Wheel"], false);
    bind(&inner, "joystick_switch", Some("ctrl+j"), Some(""));
    bind(&inner, "telemetry_capture", Some("ctrl+alt+t"), Some("Wheel^4"));
    open_editor(&mut app, &["joystick_switch", "telemetry_capture"]);
    let html = dialog(&app);
    assert!(html.contains("<option value=\"0\" selected>Toggle Joystick</option>"), "{html}");
    assert!(html.contains("<option value=\"1\">Toggle Telemetry Capture</option>"));
    assert_eq!((editor(&app).selected, editor(&app).keys.as_str()), (0, "ctrl+j"));
    dispatch(&mut app, "dialog-field", "action", "1").unwrap();
    let chosen = editor(&app);
    assert_eq!((chosen.selected, chosen.keys.as_str(), chosen.button.as_str()), (1, "ctrl+alt+t", "4"));
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert_eq!(inner.native_setting("SHORTCUTS", "telemetry_capture_keys").unwrap(), "ctrl+alt+t".into());
    drop(app.take_threads());
}

#[test]
fn the_capture_fills_the_fields_keeps_them_after_a_failed_read_and_shows_only_new_keys() {
    let (mut app, inner, backend) = editor_app(&["Wheel"], false);
    bind(&inner, "telemetry_capture", Some("before key"), Some("Wheel^before"));
    open_editor(&mut app, &["telemetry_capture"]);
    backend.captures.lock().unwrap().extend([
        Err(Failure { status: 500, detail: "fixture capture unavailable".into() }),
        Ok(json!({"button": "BTN_4", "keys": "ctrl+alt+r", "reads": 17})),
        Ok(json!({"keys": "not read again", "reads": 17})),
    ]);
    let reads = || {
        backend.operations.lock().unwrap().iter().filter(|op| op.starts_with("Get hotkeys/capture?joystick=")).count()
    };
    app.poll(std::time::Instant::now() + std::time::Duration::from_millis(100));
    assert_eq!(reads(), 1);
    let unchanged = editor(&app);
    assert_eq!((unchanged.keys.as_str(), unchanged.button.as_str()), ("before key", "before"));
    app.poll(std::time::Instant::now() + std::time::Duration::from_millis(100));
    assert_eq!(reads(), 2, "the capture is read again after a failure");
    let captured = editor(&app);
    assert_eq!((captured.keys.as_str(), captured.button.as_str()), ("ctrl+alt+r", "BTN_4"));
    assert!(dialog(&app).contains("value=\"ctrl+alt+r\""));
    app.poll(std::time::Instant::now() + std::time::Duration::from_millis(100));
    assert_eq!(reads(), 3);
    assert_eq!(editor(&app).keys, "ctrl+alt+r", "the same keyboard read count keeps the keys");
    dispatch(&mut app, "dialog-close", "", "").unwrap();
    drop(app.take_threads());
}

#[test]
fn closing_the_editor_ends_the_capture_and_its_reads() {
    let (mut app, inner, backend) = editor_app(&["Wheel"], false);
    bind(&inner, "telemetry_capture", Some(""), Some(""));
    open_editor(&mut app, &["telemetry_capture"]);
    let reads =
        || backend.operations.lock().unwrap().iter().filter(|op| op.starts_with("Get hotkeys/capture?")).count();
    app.poll(std::time::Instant::now() + std::time::Duration::from_millis(100));
    assert_eq!(reads(), 1);
    assert_eq!(editor(&app).keys, "", "an empty read changes nothing");
    inner.calls.lock().unwrap().clear();
    dispatch(&mut app, "dialog-close", "", "").unwrap();
    assert!(inner.calls.lock().unwrap().iter().any(|(m, p, _)| *m == Method::Delete && p == "hotkeys/capture"));
    app.poll(std::time::Instant::now() + std::time::Duration::from_millis(100));
    assert_eq!(reads(), 1, "no read once the editor is closed");
    assert_eq!(stored_binding(&backend, "telemetry_capture_keys"), Some("".into()), "cancel stores nothing new");
    drop(app.take_threads());
}
