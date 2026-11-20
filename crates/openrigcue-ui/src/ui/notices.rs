//! The notification center: every notice the app raises (the service's notifications, the pages' alerts, the
//! interface's own confirmations) as toasts, inline connection faults and the Activity history.
//!
//! Rules (the same for every theme; each theme places and draws them):
//! * every notice is recorded in the Activity history, newest first (at most [`HISTORY_LIMIT`] entries; the
//!   oldest ended ones go first);
//! * successes and information leave after [`Policy::success_ms`], paused while hovered or focused (the browser
//!   keeps the clock and reports the end with `toast-expire`);
//! * warnings and errors stay until dismissed;
//! * at most [`Policy::max_visible`] toasts show; an older one leaves the toast area when a newer one arrives
//!   and stays reachable, with its actions, in Activity;
//! * a connection fault stays inline on the page until the service withdraws it.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use super::html::{esc, icon, t};
use crate::app::MyApp;
use crate::backend::{text, BackendExt};

/// How important a notice is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Success,
    Info,
    Warning,
    Error,
}

impl Severity {
    /// The severity of a notification colour name.
    pub fn of_color(color: &str) -> Severity {
        match color.to_ascii_lowercase().as_str() {
            "green" | "#99cc00" => Severity::Success,
            "red" | "#ff0000" | "darkred" => Severity::Error,
            "blue" | "gray" | "grey" => Severity::Info,
            _ => Severity::Warning,
        }
    }

    /// Whether the notice leaves by itself.
    pub fn timed(self) -> bool {
        matches!(self, Severity::Success | Severity::Info)
    }

    fn class(self) -> &'static str {
        match self {
            Severity::Success | Severity::Info => "",
            Severity::Warning => " warning",
            Severity::Error => " error",
        }
    }
}

/// What a notice's button does.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Opens the Activity history.
    ViewActivity,
    /// Confirms a service notification (its first button: Game Page, Instructions, Center, Calibrate).
    Confirm { service: u64 },
    /// Removes a service notification.
    Dismiss { service: u64 },
    /// Installs the device access a service notification asks for.
    Install { service: u64 },
    /// Opens the help.
    Help,
    /// Lets the service fix the sound device set-up.
    FixSound { service: u64, default: bool },
    /// Opens a screen.
    Open { screen: super::Screen },
}

/// One notice.
#[derive(Debug, Clone, PartialEq)]
pub struct Notice {
    pub id: u64,
    pub severity: Severity,
    pub title: String,
    pub message: String,
    /// Button text (catalog text) and action.
    pub actions: Vec<(String, Action)>,
    /// The time it was raised, as `HH:MM` local to the service's clock (UTC when unknown).
    pub at: String,
    /// The service notification it shows, while the service keeps it.
    pub service: Option<u64>,
    /// Shown inline until resolved.
    pub fault: bool,
    /// Whether it still applies (a withdrawn service notification or a resolved fault no longer does).
    pub open: bool,
    /// An action running for it (the Install button is busy).
    pub busy: bool,
    /// Whether it has ended: closed, dismissed, expired, replaced or withdrawn. A notice that newer toasts pushed
    /// out of the toast area has not ended: it is still waiting for a place.
    pub ended: bool,
}

/// The most Activity history entries kept.
pub const HISTORY_LIMIT: usize = 50;

/// The timing and placement rules of a theme's notifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub max_visible: usize,
    pub success_ms: u64,
}

/// The notification state.
#[derive(Debug, Default)]
pub struct Center {
    next: u64,
    /// Every notice, oldest first.
    pub history: Vec<Notice>,
    /// The toasts on screen, oldest first.
    pub toasts: Vec<u64>,
    /// The service notifications shown: service id → notice id.
    pub service: BTreeMap<u64, u64>,
    /// The service notification each prefix shows (one per prefix, a newer one replaces it).
    prefixes: BTreeMap<String, u64>,
    /// The notice the sound fix runs for, while its overlay is open.
    pub fix_for: Option<u64>,
    /// The timer that shows the fix overlay's OK button.
    pub fix_timer: Option<u64>,
}

impl Center {
    /// Records a notice and shows it as a toast (or inline, for a fault); the oldest toasts leave beyond
    /// `max_visible`.
    pub fn push(&mut self, mut notice: Notice, policy: Policy) -> u64 {
        self.next += 1;
        notice.id = self.next;
        notice.open = true;
        notice.ended = false;
        if notice.at.is_empty() {
            notice.at = clock();
        }
        let id = notice.id;
        let fault = notice.fault;
        self.history.push(notice);
        if !fault {
            self.toasts.push(id);
            while self.toasts.len() > policy.max_visible {
                self.toasts.remove(0);
            }
        }
        self.trim();
        id
    }

    /// Keeps the history at [`HISTORY_LIMIT`] entries: the oldest ended ones go first. Only an entry that has
    /// ended (closed, dismissed, expired, replaced or withdrawn) can go; one on screen as a toast or waiting for a
    /// place, an unresolved fault and a notification the service still holds stay.
    fn trim(&mut self) {
        while self.history.len() > HISTORY_LIMIT {
            let toasts = &self.toasts;
            let ended =
                |n: &Notice| n.ended && !toasts.contains(&n.id) && !(n.open && (n.fault || n.service.is_some()));
            let Some(index) = self.history.iter().position(ended) else { return };
            self.history.remove(index);
        }
    }

    pub fn get(&self, id: u64) -> Option<&Notice> {
        self.history.iter().find(|n| n.id == id)
    }

    pub fn get_mut(&mut self, id: u64) -> Option<&mut Notice> {
        self.history.iter_mut().find(|n| n.id == id)
    }

    /// Removes a toast from the screen for good: it has ended (it stays in the history).
    pub fn hide(&mut self, id: u64) {
        self.toasts.retain(|t| *t != id);
        if let Some(notice) = self.get_mut(id) {
            notice.ended = true;
        }
    }

    /// A notice no longer applies: it leaves the screen and its actions go.
    pub fn close(&mut self, id: u64) {
        self.hide(id);
        if let Some(notice) = self.get_mut(id) {
            notice.open = false;
            notice.busy = false;
        }
    }

    /// The faults shown inline.
    pub fn faults(&self) -> impl Iterator<Item = &Notice> {
        self.history.iter().filter(|n| n.fault && n.open)
    }

    /// Clears the history of notices that no longer apply.
    pub fn clear_history(&mut self) {
        let keep: Vec<u64> = self.toasts.clone();
        self.history.retain(|n| n.open || keep.contains(&n.id));
    }
}

/// The current time of day, `HH:MM` (UTC).
fn clock() -> String {
    let seconds = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    format!("{:02}:{:02}", (seconds / 3600) % 24, (seconds / 60) % 60)
}

/// Raises a notice: recorded, spoken when voice notifications are on, and shown.
pub fn raise(app: &mut MyApp, notice: Notice) -> u64 {
    let policy = super::themes::policy(app.ui.theme);
    app.speak_out(&notice.message);
    let id = app.ui.notices.push(notice, policy);
    super::refresh_notices(app);
    id
}

/// A notice with a title and a message (already translated).
pub fn notice(severity: Severity, title: &str, message: &str) -> Notice {
    Notice {
        id: 0,
        severity,
        title: title.to_owned(),
        message: message.to_owned(),
        actions: Vec::new(),
        at: String::new(),
        service: None,
        fault: false,
        open: true,
        busy: false,
        ended: false,
    }
}

/// A success with a translated title and message, spoken when voice notifications are on.
pub fn success(app: &mut MyApp, title: &str, message: &str) {
    let notice = notice(Severity::Success, &app.backend.translate(title), &app.backend.translate(message));
    raise(app, notice);
}

/// A success with a translated title and message, not spoken (a confirmation of what the user just did).
pub fn confirmation(app: &mut MyApp, title: &str, message: &str) {
    let notice = notice(Severity::Success, &app.backend.translate(title), &app.backend.translate(message));
    let policy = super::themes::policy(app.ui.theme);
    app.ui.notices.push(notice, policy);
    super::refresh_notices(app);
}

/// A page alert (`MyApp::alert_notification`): its colour gives its severity; it is spoken.
pub fn alert(app: &mut MyApp, title: &str, message: &str, color: &str) {
    let notice = notice(Severity::of_color(color), &app.backend.translate(title), &app.backend.translate(message));
    raise(app, notice);
}

/// A warning that stays until dismissed (`MyApp::alert`, the start's reset notice); not spoken.
pub fn warning(app: &mut MyApp, title: &str, message: &str) {
    let notice = notice(Severity::Warning, &app.backend.translate(title), message);
    let policy = super::themes::policy(app.ui.theme);
    app.ui.notices.push(notice, policy);
    super::refresh_notices(app);
}

/// The prefix of a service notification kind and the text of its first button: notices of one prefix replace
/// each other.
fn kind_parts(kind: &str) -> (&'static str, Option<&'static str>) {
    match kind {
        "game_not_configured" => ("config", Some("Game Page")),
        "game_instructions" => ("instructions", Some("Instructions")),
        "port_in_use" => ("udpaccesserror", None),
        "yaw_vr_not_centered" => ("yawvr", Some("Center")),
        "belt_calibration" => ("belt", Some("Calibrate")),
        "iflag_firmware" => ("iflag", None),
        "device_permissions" => ("driver", Some("Install")),
        "memory_read_refused" => ("memoryreadrefused", None),
        "default_sound_card" => ("defaultsoundcarderror", Some("Learn More")),
        "surround_required" => ("surroundsoundcarderror", Some("Learn More")),
        _ => ("alert", None),
    }
}

/// Whether a service notification kind is a connection fault: it blocks a device or the telemetry and stays
/// until the service resolves it.
pub fn is_fault(kind: &str) -> bool {
    matches!(kind, "port_in_use" | "device_permissions" | "memory_read_refused")
}

/// The title and message of a service notification: a text with values is one translated sentence whose
/// `^name^` parts are filled in; every other notification shows the service's own texts, translated.
fn service_texts(app: &MyApp, kind: &str, value: &Value) -> (String, String) {
    let fill = |template: &str, values: &[(&str, &str)]| {
        values.iter().fold(app.backend.translate(template), |text, (name, value)| text.replace(name, value))
    };
    match kind {
        "game_instructions" => {
            if let Some(id) = value["game"].as_str().filter(|id| !id.is_empty()) {
                let catalog = openrigcue_config::catalog::Catalog::builtin();
                let game = catalog.game(id).map_or(id, |entry| entry.name.as_str());
                return (
                    fill("^game^ telemetry", &[("^game^", game)]),
                    fill("Open the ^game^ setup instructions to enable telemetry.", &[("^game^", game)]),
                );
            }
        }
        "port_in_use" => {
            if let (Some(process), Some(port)) = (value["process"].as_str(), value["port"].as_u64()) {
                return (
                    app.backend.translate(&text(&value["title"])),
                    fill(
                        "No game telemetry reaches OpenRigCue because ^process^ occupies port ^port^. Close ^process^ so that OpenRigCue can receive the telemetry",
                        &[("^process^", process), ("^port^", &port.to_string())],
                    ),
                );
            }
        }
        "device_permissions" => {
            if let Some(devices) = value["devices"].as_array() {
                let devices = devices.iter().map(text).collect::<Vec<_>>().join(", ");
                return (
                    app.backend.translate(&text(&value["title"])),
                    fill("Allow OpenRigCue to access ^devices^.", &[("^devices^", &devices)]),
                );
            }
        }
        "surround_required" => {
            if let (Some(profile), Some(label)) = (value["profile"].as_str(), value["label"].as_str()) {
                let values = [
                    ("^profile^", app.backend.translate(profile)),
                    ("^label^", app.backend.translate(label)),
                    ("^stereo^", app.backend.translate("2 Speakers - Front Left / Right")),
                ];
                let values = values.each_ref().map(|(name, value)| (*name, value.as_str()));
                let message = if value["suggest_stereo"] == true {
                    "The \"^profile^\" profile needs your sound device set to ^label^ in the system sound settings. Click \"Fix automatically\" to have that done for you or switch to a 2-channel profile such as \"^stereo^\"."
                } else {
                    "The \"^profile^\" profile needs your sound device set to ^label^ in the system sound settings. Click \"Fix automatically\" to have that done for you."
                };
                return (fill("Shakers - ^label^ Required", &values), fill(message, &values));
            }
        }
        _ => {}
    }
    (app.backend.translate(&text(&value["title"])), app.backend.translate(&text(&value["message"])))
}

/// The service's notifications, read each main-loop pass: new ones are raised, withdrawn ones close (and the
/// sound fix overlay opened for one closes with it).
pub fn synchronize(app: &mut MyApp) {
    let Ok(Value::Array(list)) = app.backend.get("notifications") else { return };
    apply(app, list);
}

/// [`synchronize`] without holding the application while the service answers.
pub(crate) fn synchronize_shared(shared: &std::sync::Mutex<MyApp>) {
    let backend = crate::host::lock(shared).backend.clone();
    let Ok(Value::Array(list)) = backend.get("notifications") else { return };
    apply(&mut crate::host::lock(shared), list);
}

fn apply(app: &mut MyApp, list: Vec<Value>) {
    let active: Vec<u64> = list.iter().filter_map(|v| v["id"].as_u64()).collect();
    let withdrawn: Vec<(u64, u64)> =
        app.ui.notices.service.iter().filter(|(service, _)| !active.contains(service)).map(|(s, n)| (*s, *n)).collect();
    let mut changed = !withdrawn.is_empty();
    for (service, id) in withdrawn {
        app.ui.notices.service.remove(&service);
        app.ui.notices.close(id);
        if let Some(notice) = app.ui.notices.get_mut(id) {
            notice.service = None;
        }
        if app.ui.notices.fix_for == Some(id) {
            app.ui.notices.fix_for = None;
            if let Some(timer) = app.ui.notices.fix_timer.take() {
                app.cancel_timer(timer);
            }
            // The overlay leaves the screen (closing redraws the dialog region), or no longer comes back from
            // behind a Downloaded Profiles review.
            super::dialogs::close_open(app, |dialog| matches!(dialog, super::dialogs::Dialog::ApplyingFix { .. }));
        }
    }
    for value in list {
        let Some(service) = value["id"].as_u64() else { continue };
        if app.ui.notices.service.contains_key(&service) {
            continue;
        }
        show_service(app, service, &value);
        changed = true;
    }
    if changed {
        super::refresh_notices(app);
    }
}

fn show_service(app: &mut MyApp, service: u64, value: &Value) {
    let kind = text(&value["kind"]);
    let (prefix, first) = kind_parts(&kind);
    // One notice per prefix: a newer notification of the same kind replaces the shown one.
    if !matches!(prefix, "alert" | "defaultsoundcarderror" | "surroundsoundcarderror") {
        if let Some(previous) = app.ui.notices.prefixes.get(prefix).copied() {
            app.ui.notices.hide(previous);
        }
    }
    let generic = matches!(kind.as_str(), "default_sound_card" | "surround_required");
    let mut actions = Vec::new();
    if generic {
        actions.push(("Learn More".to_owned(), Action::Help));
        actions.push((
            "Fix automatically".to_owned(),
            Action::FixSound { service, default: kind == "default_sound_card" },
        ));
    } else if prefix != "alert" {
        if let Some(label) = first {
            let action = if prefix == "driver" { Action::Install { service } } else { Action::Confirm { service } };
            actions.push((label.to_owned(), action));
        }
        actions.push(("Dismiss".to_owned(), Action::Dismiss { service }));
    }
    let (title, message) = service_texts(app, &kind, value);
    let severity = if prefix == "alert" { Severity::of_color(&text(&value["colour"])) } else { Severity::Warning };
    let fault = is_fault(&kind);
    let severity = if fault { Severity::Error } else { severity };
    let mut notice = notice(severity, &title, &message);
    notice.actions = actions;
    notice.service = Some(service);
    notice.fault = fault;
    let id = raise_quiet(app, notice);
    app.ui.notices.service.insert(service, id);
    app.ui.notices.prefixes.insert(prefix.to_owned(), id);
}

/// [`raise`] without the refresh (the caller refreshes once).
fn raise_quiet(app: &mut MyApp, notice: Notice) -> u64 {
    let policy = super::themes::policy(app.ui.theme);
    app.speak_out(&notice.message);
    app.ui.notices.push(notice, policy)
}

/// Runs a notice's action.
pub fn run(app: &mut MyApp, id: u64, index: usize) -> Result<(), String> {
    let Some(action) = app.ui.notices.get(id).and_then(|n| n.actions.get(index)).map(|(_, a)| a.clone()) else {
        return Ok(());
    };
    match action {
        Action::ViewActivity => {
            app.ui.drawer = true;
            super::refresh_notices(app);
        }
        Action::Help => super::navigate(app, super::Screen::Help, 0),
        Action::Open { screen } => super::navigate(app, screen, 0),
        Action::Dismiss { service } => {
            app.ui.notices.close(id);
            let _ = app.call(app.backend.delete(&format!("notifications/{service}")));
            super::refresh_notices(app);
        }
        Action::Confirm { service } => {
            app.ui.notices.close(id);
            super::refresh_notices(app);
            let Some(answer) = app.call(app.backend.post(&format!("notifications/{service}/confirm"), Value::Null))
            else {
                return Ok(());
            };
            match answer["show"].as_str() {
                Some("game_page") => super::navigate(app, super::Screen::Pc, 0),
                Some("instructions") => {
                    let controls = app.backend.get("controls").map_err(|error| error.detail)?;
                    let forced = controls.get("forced_game").ok_or("KeyError: 'force_game_detection'")?;
                    let name = text(&answer["game"]);
                    // A forced (console) game tries its console instructions first, then the PC ones.
                    let console_shown =
                        !forced.is_null() && super::dialogs::game_instructions(app, &name, true).is_ok();
                    if !console_shown {
                        super::dialogs::game_instructions(app, &name, false)?;
                    }
                }
                _ => {}
            }
        }
        Action::Install { service } => {
            if app.ui.notices.get(id).is_some_and(|n| n.busy) {
                return Ok(());
            }
            if let Some(notice) = app.ui.notices.get_mut(id) {
                notice.busy = true;
            }
            super::refresh_notices(app);
            let backend = app.backend.clone();
            app.start_thread(
                "install_device_access",
                Box::new(move |weak| {
                    let result = backend.post(&format!("device-access/{service}/install"), Value::Null);
                    if let Some(shared) = weak.upgrade() {
                        let mut app = crate::host::lock(&shared);
                        if let Some(notice) = app.ui.notices.get_mut(id) {
                            notice.busy = false;
                        }
                        if app.call(result).is_some() {
                            app.ui.notices.close(id);
                        }
                        super::refresh_notices(&mut app);
                    }
                }),
            );
        }
        Action::FixSound { service: _, default } => {
            if default {
                super::dialogs::open_sound_device(app, id);
            } else {
                let name = text(app.setting(&["shakers", "sound_card"]));
                apply_fix(app, id, "surround", &name);
            }
        }
    }
    Ok(())
}

/// Lets the service fix the sound set-up: an overlay asks the user to wait (its OK button appears after
/// 30 s), and closes when the service withdraws the notification.
pub fn apply_fix(app: &mut MyApp, id: u64, action: &str, name: &str) {
    if let Some(timer) = app.ui.notices.fix_timer.take() {
        app.cancel_timer(timer);
    }
    app.ui.notices.fix_for = Some(id);
    super::dialogs::open(app, super::dialogs::Dialog::ApplyingFix { default: action == "default", ready: false });
    // The OK button appears whether the overlay is in front or held behind a Downloaded Profiles review.
    let timer = app.update_locked_timer(std::time::Duration::from_secs(30), |app| {
        let overlay = super::dialogs::open_dialogs_mut(app)
            .find(|(dialog, _)| matches!(dialog, super::dialogs::Dialog::ApplyingFix { .. }));
        if let Some((super::dialogs::Dialog::ApplyingFix { ready, .. }, front)) = overlay {
            *ready = true;
            if front {
                super::refresh(app, &[super::Region::Dialog]);
            }
        }
    });
    app.ui.notices.fix_timer = Some(timer);
    super::refresh(app, &[super::Region::Dialog]);
    let _ = app.call(app.backend.post(&format!("audio/fix/{action}"), json!({ "name": name })));
}

/// The action buttons of a notice.
fn action_buttons(app: &MyApp, notice: &Notice, class: &str) -> String {
    if !notice.open {
        return String::new();
    }
    notice
        .actions
        .iter()
        .enumerate()
        .map(|(index, (label, _))| {
            let busy = notice.busy && matches!(notice.actions[index].1, Action::Install { .. });
            format!(
                "<button type=\"button\" class=\"{class}\" data-act=\"notice\" data-arg=\"{}:{index}\"{}>{}</button>",
                notice.id,
                if busy { " disabled aria-busy=\"true\"" } else { "" },
                t(app, label)
            )
        })
        .collect()
}

/// The toasts.
pub fn toasts(app: &MyApp) -> String {
    let policy = super::themes::policy(app.ui.theme);
    let mut out = String::new();
    for id in &app.ui.notices.toasts {
        let Some(n) = app.ui.notices.get(*id) else { continue };
        let life = if n.severity.timed() { policy.success_ms } else { 0 };
        let mut buttons = action_buttons(app, n, "action");
        if !n.severity.timed() && n.actions.is_empty() {
            buttons = format!(
                "<button type=\"button\" class=\"action\" data-act=\"drawer-open\">{}</button>",
                t(app, "View activity")
            );
        }
        out.push_str(&format!(
            "<div class=\"toast{class}\" data-toast=\"{id}\" data-life=\"{life}\" role=\"status\">{icon}<div><h3>{title}</h3><p>{message}</p>{buttons}</div><button type=\"button\" class=\"close\" aria-label=\"{dismiss}\" data-act=\"toast-dismiss\" data-arg=\"{id}\">×</button></div>",
            class = n.severity.class(),
            icon = icon(if n.severity.timed() { "check" } else { "warning" }),
            title = esc(&n.title),
            message = esc(&n.message),
            dismiss = t(app, "Dismiss notification"),
        ));
    }
    out
}

/// The connection faults, inline.
pub fn faults(app: &MyApp) -> String {
    app.ui
        .notices
        .faults()
        .map(|n| {
            format!(
                "<div class=\"critical\" role=\"alert\" data-fault=\"{}\"><div><strong>{}</strong><p>{}</p></div><div class=\"critical-actions\">{}</div></div>",
                n.id,
                esc(&n.title),
                esc(&n.message),
                action_buttons(app, n, "")
            )
        })
        .collect()
}

/// The Activity history drawer.
pub fn drawer(app: &MyApp) -> String {
    let entries: String = app
        .ui
        .notices
        .history
        .iter()
        .rev()
        .enumerate()
        .map(|(index, n)| {
            let when = if index == 0 { t(app, "Latest") } else { t(app, "Earlier") };
            let state = if n.fault && n.open {
                format!(" · {}", t(app, "Unresolved"))
            } else if !n.open && n.service.is_none() && !n.actions.is_empty() {
                format!(" · {}", t(app, "Resolved"))
            } else {
                String::new()
            };
            format!(
                "<li class=\"entry{}\"><small>{when} · {}{state}</small><strong>{}</strong><p>{}</p>{}</li>",
                n.severity.class(),
                esc(&n.at),
                esc(&n.title),
                esc(&n.message),
                if n.open && !n.actions.is_empty() {
                    format!("<div class=\"entry-actions\">{}</div>", action_buttons(app, n, "action"))
                } else {
                    String::new()
                }
            )
        })
        .collect();
    let empty = if app.ui.notices.history.is_empty() {
        format!("<li><p>{}</p></li>", t(app, "No activity yet."))
    } else {
        String::new()
    };
    format!(
        "<aside class=\"drawer\" id=\"activity-drawer\" aria-label=\"{label}\"{hidden}><header><h2>{title}</h2><div class=\"drawer-tools\"><button type=\"button\" class=\"quiet\" data-act=\"activity-clear\">{clear}</button><button type=\"button\" id=\"close-drawer\" aria-label=\"{close}\" data-act=\"drawer-close\" data-autofocus>×</button></div></header><ol id=\"history\">{entries}{empty}</ol></aside>",
        label = t(app, "Activity history"),
        hidden = if app.ui.drawer { "" } else { " hidden" },
        title = t(app, "Activity"),
        clear = t(app, "Clear resolved"),
        close = t(app, "Close activity"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> Policy {
        Policy { max_visible: 2, success_ms: 6000 }
    }

    #[test]
    fn at_most_two_toasts_show_and_history_keeps_every_notice() {
        let mut center = Center::default();
        let a = center.push(notice(Severity::Success, "a", ""), policy());
        let b = center.push(notice(Severity::Warning, "b", ""), policy());
        let c = center.push(notice(Severity::Success, "c", ""), policy());
        assert_eq!(center.toasts, vec![b, c]);
        assert_eq!(center.history.len(), 3);
        assert!(center.get(a).is_some_and(|n| n.open));
    }

    #[test]
    fn the_history_keeps_fifty_entries_and_the_oldest_ended_ones_go_first() {
        let mut center = Center::default();
        let mut fault = notice(Severity::Error, "fault", "");
        fault.fault = true;
        let fault = center.push(fault, policy());
        let mut held = notice(Severity::Warning, "held", "");
        held.service = Some(9);
        let held = center.push(held, policy());
        let first = center.push(notice(Severity::Success, "first", ""), policy());
        center.hide(first);
        for index in 0..60 {
            // Each success expires before the next one arrives.
            let id = center.push(notice(Severity::Success, &index.to_string(), ""), policy());
            center.hide(id);
        }
        assert_eq!(center.history.len(), HISTORY_LIMIT);
        assert!(center.get(fault).is_some(), "an unresolved fault stays");
        assert!(center.get(held).is_some(), "a notification the service holds stays");
        assert!(center.get(first).is_none(), "the oldest ended entry went");
        assert_eq!(center.history.last().map(|n| n.title.as_str()), Some("59"));
        assert_eq!(center.history[2].title, "12", "the oldest ended ones went first");
        center.close(fault);
        center.push(notice(Severity::Success, "next", ""), policy());
        assert!(center.get(fault).is_none(), "a resolved fault is ended and the oldest");
        assert_eq!(center.history.len(), HISTORY_LIMIT);
    }

    /// A warning that newer toasts pushed out of the toast area is still open and waiting for a place: the trim
    /// keeps it and removes the oldest entries that really ended instead.
    #[test]
    fn the_trim_keeps_a_waiting_warning_and_drops_only_ended_entries() {
        let mut center = Center::default();
        let waiting = center.push(notice(Severity::Warning, "waiting", ""), policy());
        let mut ended = Vec::new();
        for index in 0..HISTORY_LIMIT {
            let id = center.push(notice(Severity::Success, &index.to_string(), ""), policy());
            if index < 10 {
                center.hide(id);
                ended.push(id);
            }
        }
        assert!(!center.toasts.contains(&waiting), "newer toasts took its place");
        assert!(center.get(waiting).is_some_and(|n| n.open && !n.ended), "it has not ended");
        assert_eq!(center.history.len(), HISTORY_LIMIT);
        assert!(center.get(waiting).is_some(), "a waiting warning stays");
        assert!(center.get(ended[0]).is_none(), "the oldest ended entry went instead");
        assert!(ended[1..].iter().all(|id| center.get(*id).is_some()));
        // With nothing ended left beyond the limit the history grows rather than drop an open notice.
        for _ in 0..(ended.len() - 1) {
            center.push(notice(Severity::Warning, "more", ""), policy());
        }
        assert!(ended.iter().all(|id| center.get(*id).is_none()), "every ended entry went");
        center.push(notice(Severity::Warning, "one more", ""), policy());
        assert!(center.get(waiting).is_some(), "an open warning is never trimmed");
        assert_eq!(center.history.len(), HISTORY_LIMIT + 1);
    }

    #[test]
    fn faults_stay_inline_until_closed() {
        let mut center = Center::default();
        let mut fault = notice(Severity::Error, "fault", "");
        fault.fault = true;
        let id = center.push(fault, policy());
        assert!(center.toasts.is_empty());
        assert_eq!(center.faults().count(), 1);
        center.close(id);
        assert_eq!(center.faults().count(), 0);
        assert!(center.get(id).is_some(), "the history keeps a resolved fault");
    }

    #[test]
    fn severities_follow_colours_and_only_successes_are_timed() {
        assert_eq!(Severity::of_color("green"), Severity::Success);
        assert_eq!(Severity::of_color("red"), Severity::Error);
        assert_eq!(Severity::of_color("orange"), Severity::Warning);
        assert!(Severity::Success.timed() && Severity::Info.timed());
        assert!(!Severity::Warning.timed() && !Severity::Error.timed());
    }

    #[test]
    fn service_kinds_keep_their_buttons_and_faults() {
        assert_eq!(kind_parts("device_permissions"), ("driver", Some("Install")));
        assert_eq!(kind_parts("yaw_vr_not_centered"), ("yawvr", Some("Center")));
        assert!(is_fault("port_in_use") && is_fault("device_permissions") && is_fault("memory_read_refused"));
        assert!(!is_fault("belt_calibration"));
    }
}
