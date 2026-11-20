//! The interface's dialogs: one open at a time, drawn in the dialog region as a modal `<dialog class="panel">`
//! with a heading, a body and its buttons. Field changes arrive as `dialog-field` (name, value), the confirm
//! button as `dialog-ok`, Cancel / Escape / × as `dialog-close`.

use serde_json::Value;

use super::html::{esc, t, tr};
use super::screens::motion::{self, WizardStep};
use super::{components, notices, profiles, shortcuts, Region};
use crate::app::MyApp;
use crate::backend::{segment, text, BackendExt};

/// An open dialog and what it holds.
#[derive(Debug, Clone, PartialEq)]
pub enum Dialog {
    /// Save: store the settings for the vehicle or for the game; a library profile asks for confirmation.
    SaveChoice { vehicle: String, game: String, choice: usize, library: bool },
    /// Revert: throw away the changes to the profile?
    Revert,
    /// Reset a game's profile, or delete a vehicle's.
    ProfileConfirm { game: String, vehicle: Option<String> },
    /// Import the offered profile?
    Import { name: String },
    /// A message.
    Message { title: String, text: String },
    /// A game's telemetry set-up instructions (the service's HTML).
    Instructions { title: String, html: String },
    /// The sound device for game audio, before the default-device fix.
    SoundDevice { notice: u64, devices: Vec<(String, String)>, selected: usize },
    /// The sound fix is running (OK appears after 30 s).
    ApplyingFix { default: bool, ready: bool },
    /// The mobile companion's address and QR code.
    Companion { url: String, image: String },
    /// The shortcut editor.
    Shortcut(shortcuts::Editor),
    /// Autotune.
    Wizard(WizardStep),
    /// The dialogs of the Games and Hardware screens (see [`super::dialogs_hardware`]).
    Hardware(super::dialogs_hardware::Dialog),
    /// The dialogs of the Settings screens (see [`super::screens::app`]).
    Settings(super::screens::app::SettingsDialog),
    /// The dialogs of the tuning effect screens (see [`super::screens::tuning`]).
    Tuning(super::screens::tuning::Dialog),
}

/// Opens a dialog. An open one is replaced: what it runs ends first ([`ended`]), except Autotune and a
/// Downloaded Profiles selection waiting for its game or vehicle, which go on behind the new dialog and come
/// back when that closes.
pub fn open(app: &mut MyApp, dialog: Dialog) {
    use super::screens::tuning::Dialog as Tuning;
    match app.ui.dialog.take() {
        Some(Dialog::Wizard(step)) if !matches!(dialog, Dialog::Wizard(_)) => {
            app.ui.tuning.wizard_behind = Some(step);
        }
        Some(Dialog::Tuning(Tuning::LibraryPending(pending)))
            if !matches!(dialog, Dialog::Tuning(Tuning::LibraryPending(_))) =>
        {
            app.ui.tuning.library_behind = Some(pending);
        }
        Some(replaced) => ended(app, &replaced),
        None => {}
    }
    app.ui.dialog = Some(dialog);
    app.ui.popup = None;
    super::refresh(app, &[Region::Dialog, Region::Popup]);
}

/// Closes the dialog; what it runs ends ([`ended`]). The dialog a Downloaded Profiles review opened in front of
/// comes back; else Autotune, when it went on behind it; else a Downloaded Profiles selection that waited behind
/// it.
pub fn close(app: &mut MyApp) {
    if let Some(closed) = app.ui.dialog.take() {
        ended(app, &closed);
    }
    if let Some(behind) = app.ui.tuning.dialog_behind.take() {
        app.ui.dialog = Some(*behind);
    } else if let Some(step) = app.ui.tuning.wizard_behind.take() {
        app.ui.dialog = Some(Dialog::Wizard(step));
    } else if let Some(pending) = app.ui.tuning.library_behind.take() {
        app.ui.dialog = Some(Dialog::Tuning(super::screens::tuning::Dialog::LibraryPending(pending)));
    }
    super::refresh(app, &[Region::Dialog]);
}

/// The open dialogs: the one in front, then the one held behind a Downloaded Profiles review, which comes back
/// when the review closes. What a dialog runs (its timers, polls and service updates) acts on it in either
/// place; `true` marks the one in front.
pub fn open_dialogs(app: &MyApp) -> impl Iterator<Item = (&Dialog, bool)> + '_ {
    let held = app.ui.tuning.dialog_behind.as_deref().map(|dialog| (dialog, false));
    app.ui.dialog.as_ref().map(|dialog| (dialog, true)).into_iter().chain(held)
}

/// [`open_dialogs`], to change what they hold.
pub fn open_dialogs_mut(app: &mut MyApp) -> impl Iterator<Item = (&mut Dialog, bool)> + '_ {
    let held = app.ui.tuning.dialog_behind.as_deref_mut().map(|dialog| (dialog, false));
    app.ui.dialog.as_mut().map(|dialog| (dialog, true)).into_iter().chain(held)
}

/// Closes the open dialog `pick` finds: in front as [`close`] does, or held behind a Downloaded Profiles review,
/// where it ends ([`ended`]) and no longer comes back.
pub fn close_open(app: &mut MyApp, pick: impl Fn(&Dialog) -> bool) {
    if app.ui.dialog.as_ref().is_some_and(&pick) {
        close(app);
    } else if app.ui.tuning.dialog_behind.as_deref().is_some_and(&pick) {
        if let Some(held) = app.ui.tuning.dialog_behind.take() {
            ended(app, &held);
        }
    }
}

/// A dialog left the screen, closed or replaced by another one: the service work it started ends (the
/// shortcut editor's input capture).
fn ended(app: &mut MyApp, dialog: &Dialog) {
    if let Dialog::Shortcut(_) = dialog {
        shortcuts::stop(app);
    }
}

/// A dialog's frame: heading, body, Cancel (or Close) and an optional confirm button (text, enabled).
pub fn frame(app: &MyApp, title: &str, body: &str, confirm: Option<(&str, bool)>) -> String {
    let (cancel, ok) = match confirm {
        Some((label, enabled)) => (
            t(app, "Cancel"),
            format!(
                "<button type=\"button\" class=\"primary\" data-act=\"dialog-ok\"{}>{label}</button>",
                if enabled { "" } else { " disabled" }
            ),
        ),
        None => (t(app, "Close"), String::new()),
    };
    format!(
        "<dialog class=\"panel orc-dialog\" id=\"orc-dialog-box\" data-modal aria-labelledby=\"orc-dialog-title\"><div class=\"panel-head\"><h2 id=\"orc-dialog-title\">{title}</h2><button type=\"button\" class=\"quiet dialog-x\" data-act=\"dialog-close\" aria-label=\"{close}\">×</button></div><div class=\"dialog-body\">{body}</div><div class=\"device dialog-buttons\"><button type=\"button\" data-act=\"dialog-close\">{cancel}</button>{ok}</div></dialog>",
        close = t(app, "Close dialog"),
    )
}

/// The open dialog.
pub fn render(app: &MyApp) -> String {
    let Some(dialog) = &app.ui.dialog else { return String::new() };
    match dialog {
        Dialog::SaveChoice { vehicle, game, choice, library } => {
            let mut options = Vec::new();
            if !vehicle.is_empty() {
                options.push(("0".to_owned(), tr(app, "Vehicle: ^vehicle^").replace("^vehicle^", vehicle)));
            }
            options.push(((options.len()).to_string(), tr(app, "Game: ^game^").replace("^game^", game)));
            let alert = if *library {
                format!("<p class=\"dialog-text\">{}</p>", t(app, "Confirm to adopt this cloud share profile in place of your current one; test it before saving. Cancel throws the changes away (PROFILE IN USE box)."))
            } else {
                String::new()
            };
            frame(
                app,
                &t(app, "Save"),
                &format!(
                    "{alert}{}",
                    components::field(
                        &t(app, "Store these settings for"),
                        &components::select("save-level", &tr(app, "Store these settings for"), &options, &choice.to_string(), "dialog-field", "choice"),
                        ""
                    )
                ),
                Some((&t(app, "Confirm"), true)),
            )
        }
        Dialog::Revert => frame(
            app,
            &t(app, "Abandon Changes?"),
            &format!("<p class=\"dialog-text\">{}</p>", t(app, "Do you really want to throw away your changes to the profile?")),
            Some((&t(app, "Confirm"), true)),
        ),
        Dialog::ProfileConfirm { vehicle, .. } => frame(
            app,
            &t(app, "Reset or delete this profile?"),
            &format!(
                "<p class=\"dialog-text\">{}</p>",
                t(app, if vehicle.is_some() {
                    "Do you really want to delete this vehicle's settings?"
                } else {
                    "Delete every custom game/vehicle setting and put the game back to its defaults?"
                })
            ),
            Some((&t(app, "Confirm"), true)),
        ),
        Dialog::Import { name } => frame(
            app,
            &t(app, "Import Profile"),
            &format!("<p class=\"dialog-text\">{}{}?</p>", t(app, "Do you want to import the profile at "), esc(name)),
            Some((&t(app, "Confirm"), true)),
        ),
        Dialog::Message { title, text } => {
            frame(app, &esc(title), &format!("<p class=\"dialog-text\">{}</p>", esc(text)), None)
        }
        Dialog::Instructions { title, html } => {
            frame(app, &esc(title), &format!("<div class=\"dialog-text instructions\">{html}</div>"), None)
        }
        Dialog::SoundDevice { devices, selected, .. } => {
            let options: Vec<(String, String)> =
                devices.iter().enumerate().map(|(i, (_, name))| (i.to_string(), name.clone())).collect();
            frame(
                app,
                &t(app, "Set Sound Device as Default"),
                &format!(
                    "<p class=\"dialog-text\">{}</p>{}",
                    t(app, "Select the sound device for game audio (usually a monitor, speakers or headphones)"),
                    components::field(
                        &t(app, "Sound Device"),
                        &components::select("sound-device", &tr(app, "Sound Device"), &options, &selected.to_string(), "dialog-field", "device"),
                        ""
                    )
                ),
                Some((&t(app, "Confirm"), !devices.is_empty())),
            )
        }
        Dialog::ApplyingFix { default, ready } => {
            let message = if *default {
                "OpenRigCue is setting the default audio device automatically. Please do not touch your PC until this process completes."
            } else {
                "OpenRigCue is configuring the surround channels automatically. Please do not touch your PC until this process completes."
            };
            let body = format!("<p class=\"dialog-text\" aria-busy=\"{}\">{}</p>", !ready, t(app, message));
            if *ready {
                frame(app, &t(app, "Applying Fix"), &body, None)
            } else {
                format!(
                    "<dialog class=\"panel orc-dialog\" id=\"orc-dialog-box\" data-modal data-locked aria-labelledby=\"orc-dialog-title\"><div class=\"panel-head\"><h2 id=\"orc-dialog-title\">{}</h2></div><div class=\"dialog-body\">{body}</div></dialog>",
                    t(app, "Applying Fix")
                )
            }
        }
        Dialog::Companion { url, image } => frame(
            app,
            &t(app, "OpenRigCue Mobile Companion"),
            &format!(
                "<div class=\"dialog-text companion\"><p>{}</p><p>{}</p><p class=\"mono\">{}</p><img src=\"{}\" width=\"328\" height=\"328\" alt=\"{}\"></div>",
                t(app, "On your phone or tablet, scan the QR Code shown below"),
                t(app, "or enter this address in its browser"),
                esc(url),
                esc(image),
                t(app, "QR code of the companion address")
            ),
            None,
        ),
        Dialog::Shortcut(editor) => shortcuts::render(app, editor),
        Dialog::Wizard(step) => motion::wizard_dialog(app, step),
        Dialog::Hardware(dialog) => super::dialogs_hardware::render(app, dialog),
        Dialog::Settings(dialog) => super::screens::app::dialog(app, dialog),
        Dialog::Tuning(dialog) => super::screens::tuning::render_dialog(app, dialog),
    }
}

/// A dialog action.
pub fn act(app: &mut MyApp, act: &str, arg: &str, value: &str) -> Result<(), String> {
    let Some(dialog) = app.ui.dialog.clone() else { return Ok(()) };
    if let Dialog::Hardware(dialog) = dialog {
        return super::dialogs_hardware::act(app, dialog, act, arg, value);
    }
    if let Dialog::Tuning(dialog) = dialog {
        return super::screens::tuning::dialog_act(app, dialog, act, arg, value);
    }
    match act {
        "dialog-close" => {
            match dialog {
                Dialog::ApplyingFix { ready: false, .. } => return Ok(()),
                Dialog::Wizard(_) => {
                    close(app);
                    return motion::wizard_cancel(app);
                }
                _ => {}
            }
            close(app);
        }
        "dialog-field" => {
            let index: usize = value.parse().unwrap_or(0);
            match (app.ui.dialog.as_mut(), arg) {
                (Some(Dialog::SaveChoice { choice, .. }), "choice") => *choice = index,
                (Some(Dialog::SoundDevice { selected, .. }), "device") => *selected = index,
                (Some(Dialog::Wizard(WizardStep::Review { preference, .. })), "preference") => {
                    *preference = index.min(2)
                }
                (Some(Dialog::Shortcut(_)), _) => return shortcuts::field(app, arg, value),
                _ => {}
            }
            super::refresh(app, &[Region::Dialog]);
        }
        "dialog-ok" => match dialog {
            Dialog::SaveChoice { vehicle, choice, .. } => {
                close(app);
                let at_vehicle = !vehicle.is_empty() && choice == 0;
                profiles::save_at(app, at_vehicle.then_some(vehicle.as_str()))?;
            }
            Dialog::Revert => {
                close(app);
                profiles::revert_confirmed(app);
            }
            Dialog::ProfileConfirm { game, vehicle } => {
                close(app);
                profiles::confirm(app, &game, vehicle.as_deref())?;
            }
            Dialog::Import { .. } => {
                close(app);
                profiles::import_confirmed(app);
            }
            Dialog::SoundDevice { notice, devices, selected } => {
                close(app);
                let id = devices.get(selected).map_or_else(|| "None".to_owned(), |(id, _)| id.clone());
                notices::apply_fix(app, notice, "default", &id);
            }
            Dialog::Wizard(_) => motion::wizard_next(app)?,
            Dialog::Shortcut(_) => shortcuts::confirm(app)?,
            Dialog::Settings(dialog) => super::screens::app::dialog_ok(app, &dialog)?,
            _ => close(app),
        },
        act if act.starts_with("shortcut") => shortcuts::act(app, act, arg)?,
        _ => {}
    }
    Ok(())
}

/// A game's telemetry set-up instructions (PC or console), from the service.
pub fn game_instructions(app: &mut MyApp, name: &str, console: bool) -> Result<(), String> {
    let game_name = openrigcue_config::patch::GAME_ENTRIES
        .iter()
        .find(|entry| entry.key == name)
        .map(|entry| entry.name)
        .or_else(|| openrigcue_config::catalog::Catalog::builtin().game(name).map(|game| game.name.as_str()))
        .unwrap_or(name)
        .to_owned();
    let (bytes, _) = app
        .backend
        .fetch(&format!("games/{}/instructions?console={console}", segment(name)))
        .map_err(|error| error.detail)?;
    let title = tr(app, "^game^ — ^mode^ setup")
        .replace("^mode^", &tr(app, if console { "Console" } else { "PC" }))
        .replace("^game^", &game_name);
    open(app, Dialog::Instructions { title, html: String::from_utf8_lossy(&bytes).into_owned() });
    Ok(())
}

/// The sound device choice before the default-device fix: the system's devices except the current one.
pub fn open_sound_device(app: &mut MyApp, notice: u64) {
    let current = text(app.setting(&["shakers", "sound_card"]));
    let cards = app.call(app.backend.get("hardware")).unwrap_or(Value::Null);
    let mut devices: Vec<(String, String)> = Vec::new();
    if let Some(list) = cards["sound_devices"].as_array() {
        for device in list {
            let (Some(id), Some(name)) = (device["id"].as_str(), device["name"].as_str()) else { continue };
            if id != "None" && id != current && !devices.iter().any(|(known, _)| known == id) {
                devices.push((id.to_owned(), name.to_owned()));
            }
        }
    } else {
        for card in cards["sound_cards"].as_array().into_iter().flatten() {
            let name = text(card);
            if name != "None" && name != current && !devices.iter().any(|(_, known)| *known == name) {
                devices.push((name.clone(), name));
            }
        }
    }
    open(app, Dialog::SoundDevice { notice, devices, selected: 0 });
}

/// The mobile companion: its address from the service and the QR code of it.
pub fn open_companion(app: &mut MyApp) {
    match app.backend.post("mobile-companion", serde_json::json!({})) {
        Ok(url) => {
            let url = text(&url);
            let image = crate::qr::png_data_url(&url);
            open(app, Dialog::Companion { url, image });
        }
        // The failure is an alert: translated, and not spoken.
        Err(failure) => app.alert(&failure.detail),
    }
}

/// The text of a dialog's title, for tests.
pub fn title_of(app: &MyApp) -> Option<String> {
    let html = render(app);
    let start = html.find("id=\"orc-dialog-title\">")? + "id=\"orc-dialog-title\">".len();
    Some(html[start..].split('<').next()?.to_owned())
}
