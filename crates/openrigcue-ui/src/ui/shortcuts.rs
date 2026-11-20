//! The shortcut editor: assigns a keyboard combination and a button-box button to an action. The service
//! captures the input while the editor is open (`hotkeys/capture`); the editor polls it every 0.1 s and
//! stores the bindings as `[SHORTCUTS] <action>_keys` and `<action>_btns` (`<controller>^<button>`).

use std::time::Duration;

use serde_json::{json, Value};

use super::dialogs::{self, Dialog};
use super::html::{esc, t, tr};
use super::{components, Region};
use crate::app::MyApp;
use crate::backend::{segment, text, BackendExt};

/// The editor's state.
#[derive(Debug, Clone, PartialEq)]
pub struct Editor {
    /// The actions it edits: key and label.
    pub actions: Vec<(String, String)>,
    pub selected: usize,
    /// The captured or stored keyboard combination.
    pub keys: String,
    /// The connected button boxes.
    pub joysticks: Vec<String>,
    /// The chosen button box (`None` for none).
    pub joystick: String,
    pub button: String,
    /// The service cannot read any keyboard for lack of permission.
    pub keyboard_denied: bool,
    /// The keyboard reads already shown.
    reads: u64,
}

/// The service's shortcut table: each action's label, the connected controllers and whether no keyboard can
/// be read.
fn hotkeys(app: &MyApp) -> (Vec<(String, String)>, Vec<String>, bool) {
    let view = app.backend.get("hotkeys").unwrap_or(Value::Null);
    let mut actions = Vec::new();
    for group in view.get("groups").and_then(Value::as_array).into_iter().flatten() {
        for action in group.get("actions").and_then(Value::as_array).into_iter().flatten() {
            let field = |k: &str| text(action.get(k).unwrap_or(&Value::Null));
            actions.push((field("action"), field("label")));
        }
    }
    let joysticks =
        view.get("joysticks").and_then(Value::as_array).map(|j| j.iter().map(text).collect()).unwrap_or_default();
    let denied = view.get("keyboard_denied").and_then(Value::as_bool).unwrap_or(false);
    (actions, joysticks, denied)
}

fn setting(app: &MyApp, key: &str, suffix: &str) -> String {
    app.backend
        .native_setting("SHORTCUTS", &format!("{key}_{suffix}"))
        .map(|value| openrigcue_model::python_repr::str_value(&value))
        .unwrap_or_default()
}

/// The stored bindings of the selected action.
fn load(app: &MyApp, editor: &mut Editor) {
    let Some((key, _)) = editor.actions.get(editor.selected) else { return };
    editor.keys = setting(app, key, "keys");
    let buttons = setting(app, key, "btns");
    // `<button box>^<button>`; anything after a second `^` is not part of the button.
    let mut parts = buttons.split('^');
    let (joystick, button) = (parts.next().unwrap_or("").to_owned(), parts.next().unwrap_or("").to_owned());
    if editor.joysticks.contains(&joystick) {
        editor.joystick = joystick;
        editor.button = button;
    } else {
        editor.joystick = "None".into();
        editor.button.clear();
    }
}

/// Opens the editor for `actions` (one action, or a page's list) and starts the capture.
pub fn open(app: &mut MyApp, actions: &[String]) -> Result<(), String> {
    let (known, joysticks, keyboard_denied) = hotkeys(app);
    let actions: Vec<(String, String)> = actions
        .iter()
        .map(|key| {
            let label = known.iter().find(|(k, _)| k == key).map_or_else(|| key.clone(), |(_, label)| label.clone());
            (key.clone(), label)
        })
        .collect();
    let mut editor = Editor {
        actions,
        selected: 0,
        keys: String::new(),
        joysticks,
        joystick: "None".into(),
        button: String::new(),
        keyboard_denied,
        reads: 0,
    };
    load(app, &mut editor);
    // The dialog this one replaces ends its own work first (an editor open before ends its capture).
    dialogs::open(app, Dialog::Shortcut(editor));
    let _ = app.backend.post("hotkeys/capture", json!({}));
    schedule(app);
    Ok(())
}

/// The open editor, in front or held behind a Downloaded Profiles review (its capture goes on there), and
/// whether it is in front.
fn open_editor(app: &mut MyApp) -> Option<(&mut Editor, bool)> {
    dialogs::open_dialogs_mut(app).find_map(|(dialog, front)| match dialog {
        Dialog::Shortcut(editor) => Some((editor, front)),
        _ => None,
    })
}

/// Polls the capture again in 0.1 s while the editor is open (in front or held).
fn schedule(app: &mut MyApp) {
    app.timer(Duration::from_millis(100), |app| {
        let Some((editor, _)) = open_editor(app) else { return };
        let path = format!("hotkeys/capture?joystick={}", segment(&editor.joystick));
        let backend = app.backend.clone();
        if let Some(shared) = app.shared_handle() {
            app.start_thread(
                "shortcut_capture",
                Box::new(move |_| {
                    let captured = backend.get(&path).ok();
                    finish(&mut crate::host::lock(&shared), captured);
                }),
            );
        } else {
            let captured = backend.get(&path).ok();
            finish(app, captured);
        }
    });
}

/// A capture read arrived: a pressed button and a new keyboard combination fill the fields (a held editor
/// shows them when it comes back).
fn finish(app: &mut MyApp, captured: Option<Value>) {
    let Some((editor, front)) = open_editor(app) else { return };
    let mut changed = false;
    if let Some(captured) = captured {
        if let Some(button) = captured.get("button").and_then(Value::as_str) {
            if editor.button != button {
                editor.button = button.to_owned();
                changed = true;
            }
        }
        let reads = captured.get("reads").and_then(Value::as_u64).unwrap_or(0);
        if reads != editor.reads {
            editor.reads = reads;
            if let Some(keys) = captured.get("keys").and_then(Value::as_str) {
                changed |= editor.keys != keys;
                editor.keys = keys.to_owned();
            }
        }
    }
    if changed && front {
        super::refresh(app, &[Region::Dialog]);
    }
    schedule(app);
}

/// Ends the capture (the editor's dialog runs it whenever it is closed or replaced).
pub fn stop(app: &mut MyApp) {
    let _ = app.backend.delete("hotkeys/capture");
}

/// A field of the editor changed: the action (its stored bindings show) or the button box.
pub fn field(app: &mut MyApp, name: &str, value: &str) -> Result<(), String> {
    let index: usize = value.parse().unwrap_or(0);
    let Some(Dialog::Shortcut(mut editor)) = app.ui.dialog.clone() else { return Ok(()) };
    match name {
        "action" => {
            editor.selected = index.min(editor.actions.len().saturating_sub(1));
            load(app, &mut editor);
        }
        "joystick" => {
            editor.joystick =
                if index == 0 { "None".into() } else { editor.joysticks.get(index - 1).cloned().unwrap_or_default() };
        }
        _ => {}
    }
    app.ui.dialog = Some(Dialog::Shortcut(editor));
    super::refresh(app, &[Region::Dialog]);
    Ok(())
}

/// The editor's buttons: Clear of the keyboard or button field.
pub fn act(app: &mut MyApp, act: &str, arg: &str) -> Result<(), String> {
    if act == "shortcut-clear" {
        if let Some(Dialog::Shortcut(editor)) = app.ui.dialog.as_mut() {
            if arg == "keyboard" {
                editor.keys.clear();
            } else {
                editor.button.clear();
            }
        }
        super::refresh(app, &[Region::Dialog]);
    }
    Ok(())
}

/// Confirm: the capture ends and the bindings are stored.
pub fn confirm(app: &mut MyApp) -> Result<(), String> {
    let Some(Dialog::Shortcut(editor)) = app.ui.dialog.clone() else { return Ok(()) };
    // Closing the editor ends the capture.
    dialogs::close(app);
    let Some((key, _)) = editor.actions.get(editor.selected) else { return Ok(()) };
    app.backend
        .assign_setting_value("SHORTCUTS", &format!("{key}_keys"), &editor.keys.clone().into())
        .map_err(|error| error.detail)?;
    let buttons = if editor.button.is_empty() || editor.joystick == "None" {
        String::new()
    } else {
        format!("{}^{}", editor.joystick, editor.button)
    };
    app.backend.assign_setting_value("SHORTCUTS", &format!("{key}_btns"), &buttons.into()).map_err(|error| error.detail)
}

/// The editor dialog.
pub fn render(app: &MyApp, editor: &Editor) -> String {
    let actions: Vec<(String, String)> =
        editor.actions.iter().enumerate().map(|(i, (_, label))| (i.to_string(), tr(app, label))).collect();
    let action = if actions.len() > 1 {
        components::select(
            "shortcut-action",
            &tr(app, "Type"),
            &actions,
            &editor.selected.to_string(),
            "dialog-field",
            "action",
        )
    } else {
        format!("<p>{}</p>", esc(&actions.first().map(|(_, label)| label.clone()).unwrap_or_default()))
    };
    let mut joysticks = vec![("0".to_owned(), tr(app, "None"))];
    joysticks.extend(editor.joysticks.iter().enumerate().map(|(i, name)| ((i + 1).to_string(), name.clone())));
    let selected_joystick = editor.joysticks.iter().position(|j| *j == editor.joystick).map_or(0, |i| i + 1);
    let joystick_select = if editor.joysticks.is_empty() {
        format!(
            "<select id=\"shortcut-joystick\" class=\"field-input\" disabled><option>{}</option></select>",
            t(app, "None")
        )
    } else {
        components::select(
            "shortcut-joystick",
            &tr(app, "Buttonbox"),
            &joysticks,
            &selected_joystick.to_string(),
            "dialog-field",
            "joystick",
        )
    };
    let captured = |id: &str, value: &str, clear: &str| {
        format!(
            "<div class=\"captured\"><input id=\"{id}\" class=\"field-input mono\" type=\"text\" readonly value=\"{}\" aria-label=\"{}\"><button type=\"button\" data-act=\"shortcut-clear\" data-arg=\"{clear}\">{}</button></div>",
            esc(value),
            t(app, if clear == "keyboard" { "Keyboard" } else { "Shortcut" }),
            t(app, "Clear")
        )
    };
    let access = if editor.keyboard_denied {
        format!("<p class=\"dialog-text muted\">{}</p>", t(app, "Keyboard shortcuts need the keyboard access rule to be enabled: OpenRigCue cannot read the keyboard now. The rule is the commented line under Hotkeys in /etc/udev/rules.d/70-openrigcue.rules; it lets every program running as your user read your keystrokes. Controller and button box shortcuts are not affected."))
    } else {
        String::new()
    };
    let warning = if editor.joystick != "None" {
        format!(
            "<p class=\"dialog-text warning-text\">{}</p>",
            t(app, "Force feedback may stop working if OpenRigCue is restarted while a game runs")
        )
    } else {
        String::new()
    };
    let body = [
        components::field(&t(app, "Type"), &action, ""),
        components::field(
            &t(app, "Keyboard"),
            &captured("shortcut-keys", &editor.keys, "keyboard"),
            &t(app, "Press the key combination now."),
        ),
        access,
        components::field(&t(app, "Buttonbox"), &joystick_select, ""),
        components::field(
            &t(app, "Shortcut"),
            &captured("shortcut-button", &editor.button, "button"),
            &t(app, "Press the button now."),
        ),
        warning,
    ]
    .concat();
    dialogs::frame(app, &t(app, "Shortcuts"), &body, Some((&t(app, "Confirm"), true)))
}
