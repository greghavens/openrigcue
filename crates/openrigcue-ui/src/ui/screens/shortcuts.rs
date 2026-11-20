//! Settings → Shortcuts: every shortcut action of the service, by the page it belongs to, with its keyboard
//! combination and button-box button. "Find an action" searches every page; Assign opens the shortcut editor
//! (`ui::shortcuts`) for that action, which stores the binding.

use serde_json::Value;

use super::super::components;
use super::super::html::{esc, t, tr};
use super::super::{themes, Screen};
use crate::app::MyApp;
use crate::backend::{text, BackendExt};

/// What the screen keeps between redraws.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct State {
    /// The text of "Find an action".
    pub filter: String,
}

/// One action of the service's shortcut table.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub action: String,
    pub label: String,
    /// The keyboard combination.
    pub keys: String,
    /// The button box and its button.
    pub joystick: String,
    pub button: String,
}

/// The service's shortcut table: each page with its actions (an action listed by two pages stays with the
/// first), the pages of switched-off screens left out, and whether no keyboard can be read.
pub fn table(app: &MyApp) -> (Vec<(String, Vec<Entry>)>, bool) {
    let view = app.backend.get("hotkeys").unwrap_or(Value::Null);
    let mut seen = std::collections::BTreeSet::new();
    let mut groups = Vec::new();
    for group in view.get("groups").and_then(Value::as_array).into_iter().flatten() {
        let page = text(group.get("page").unwrap_or(&Value::Null));
        if Screen::from_key(&page).is_some_and(|screen| !screen.available(app)) {
            continue;
        }
        let mut entries = Vec::new();
        for action in group.get("actions").and_then(Value::as_array).into_iter().flatten() {
            let field = |key: &str| text(action.get(key).unwrap_or(&Value::Null));
            let key = field("action");
            if key.is_empty() || !seen.insert(key.clone()) {
                continue;
            }
            entries.push(Entry {
                label: field("label"),
                keys: field("keys"),
                joystick: field("joystick"),
                button: field("button"),
                action: key,
            });
        }
        if !entries.is_empty() {
            groups.push((page, entries));
        }
    }
    let denied = view.get("keyboard_denied").and_then(Value::as_bool).unwrap_or(false);
    (groups, denied)
}

/// The binding as shown: the keyboard combination and the button box's button, or "Unassigned".
fn binding(app: &MyApp, entry: &Entry) -> String {
    let mut parts = Vec::new();
    if !entry.keys.is_empty() {
        parts.push(entry.keys.clone());
    }
    if !entry.button.is_empty() && !entry.joystick.is_empty() && entry.joystick != "None" {
        parts.push(format!("{} · {}", entry.joystick, entry.button));
    }
    if parts.is_empty() {
        tr(app, "Unassigned")
    } else {
        parts.join(" · ")
    }
}

/// Whether an entry matches the search (its label, action name, page or binding).
fn matches(app: &MyApp, page: &str, entry: &Entry, filter: &str) -> bool {
    let needle = filter.trim().to_lowercase();
    needle.is_empty()
        || [tr(app, &entry.label), entry.action.replace('_', " "), tr(app, page), binding(app, entry)]
            .iter()
            .any(|haystack| haystack.to_lowercase().contains(&needle))
}

/// One action's row; `page` names its page (search results come from every page).
fn row(app: &MyApp, page: Option<&str>, entry: &Entry) -> String {
    format!(
        "<div data-shortcut-row>{}</div>",
        components::field(
            &t(app, &entry.label),
            &format!(
                "<button type=\"button\" data-act=\"action\" data-arg=\"{}\" aria-label=\"{}\">{}</button> <small class=\"mono\">{}</small>",
                esc(&format!("{}:{}:", Screen::Shortcuts.key(), entry.action)),
                esc(&format!("{} · {}", tr(app, "Assign"), tr(app, &entry.label))),
                t(app, "Assign"),
                esc(&binding(app, entry))
            ),
            &page.map(|page| t(app, page)).unwrap_or_default(),
        )
    )
}

/// The body: the page picker, "Find an action" and the actions of the selected page (of every page while
/// searching).
pub fn main(app: &MyApp) -> String {
    let (groups, denied) = table(app);
    let index = app.ui.section.min(groups.len().saturating_sub(1));
    let pages: Vec<(usize, String)> = groups.iter().enumerate().map(|(i, (page, _))| (i, tr(app, page))).collect();
    let filter = &app.ui.shortcuts.filter;
    let mut body = String::new();
    if denied {
        body.push_str(&super::note(&tr(app, "Keyboard shortcuts need the keyboard access rule to be enabled: OpenRigCue cannot read the keyboard now. The rule is the commented line under Hotkeys in /etc/udev/rules.d/70-openrigcue.rules; it lets every program running as your user read your keystrokes. Controller and button box shortcuts are not affected."), ""));
    }
    body.push_str(&components::field(
        &format!("<label for=\"shortcuts-filter\">{}</label>", t(app, "Find an action")),
        &format!(
            "<input id=\"shortcuts-filter\" class=\"field-input\" type=\"search\" value=\"{}\" placeholder=\"{}\" aria-label=\"{}\" data-act=\"set\" data-arg=\"{}:filter\" data-live autocomplete=\"off\">",
            esc(filter),
            t(app, "Search every page's actions"),
            t(app, "Find an action"),
            Screen::Shortcuts.key()
        ),
        "",
    ));
    let rows: Vec<String> = if filter.trim().is_empty() {
        groups.get(index).map(|(_, entries)| entries.iter().map(|e| row(app, None, e)).collect()).unwrap_or_default()
    } else {
        groups
            .iter()
            .flat_map(|(page, entries)| entries.iter().map(move |entry| (page, entry)))
            .filter(|(page, entry)| matches(app, page, entry, filter))
            .map(|(page, entry)| row(app, Some(page), entry))
            .collect()
    };
    if rows.is_empty() {
        body.push_str(&components::field(
            &t(app, "No matches"),
            &format!("<p>{}</p>", t(app, "Try another action name.")),
            "",
        ));
    }
    body.push_str(&rows.concat());
    components::panel(
        &themes::title(app, "Keyboard and button assignments"),
        &if filter.trim().is_empty() { super::section_picker(app, &pages, index) } else { String::new() },
        &body,
        "",
    )
}

/// A value of the screen changed: the search text.
pub fn set(app: &mut MyApp, control: &str, _part: &str, value: &str) -> Result<(), String> {
    match control {
        "filter" => {
            app.ui.shortcuts.filter = value.to_owned();
            Ok(())
        }
        _ => Err(format!("no Shortcuts value {control:?}")),
    }
}

/// Assign: the shortcut editor for the action.
pub fn action(app: &mut MyApp, control: &str, _index: Option<usize>) -> Result<(), String> {
    let (groups, _) = table(app);
    if !groups.iter().any(|(_, entries)| entries.iter().any(|entry| entry.action == control)) {
        return Err(format!("no shortcut action {control:?}"));
    }
    super::super::shortcuts::open(app, &[control.to_owned()])
}

#[cfg(test)]
#[path = "shortcuts_tests.rs"]
mod tests;
