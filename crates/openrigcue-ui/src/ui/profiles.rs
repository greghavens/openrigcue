//! Profiles: the Active profile's Save and Revert, and Tuning → Profiles (opened from the Active profile
//! select's "Manage profiles…"): every game's tune with its vehicles, to edit, reset, delete or export, plus
//! import, export of the profile in use and export of everything.

use openrigcue_model::native_value::{Key, LegacyValue};
use serde_json::{json, Value};

use super::components;
use super::dialogs::{self, Dialog};
use super::html::{esc, t, tf, tr};
use super::{notices, themes, Region, Screen};
use crate::app::MyApp;
use crate::backend::{segment, text, BackendExt};

/// How many games a page of the list shows.
pub const PAGE: usize = 10;

/// One game of the list with its vehicles.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub game: String,
    pub name: String,
    pub vehicles: Vec<String>,
}

/// The profile list's state.
#[derive(Debug, Default)]
pub struct State {
    pub entries: Vec<Entry>,
    pub filter: String,
    pub page: usize,
    /// The profile offered for import, before its confirmation.
    pub pending_import: Option<openrigcue_config::profiles::ProfileFile>,
}

// ---- Save and Revert -------------------------------------------------------------------------------------

/// Save: with a vehicle to choose, or a library profile to confirm, the save dialog opens; otherwise the
/// profile is saved for the game.
pub fn save_clicked(app: &mut MyApp) -> Result<(), String> {
    app.backend.capture_window_geometry();
    let vehicle = app.ui.session.vehicle.clone();
    let game = match app.backend.game_value().map_err(|error| error.detail)? {
        LegacyValue::Str(game) => game,
        other => return Err(format!("TypeError: can only concatenate str (not \"{}\") to str", other.type_name())),
    };
    let library = app.ui.tuning.library_applied;
    if library || !vehicle.is_empty() {
        dialogs::open(app, Dialog::SaveChoice { vehicle, game, choice: 0, library });
        Ok(())
    } else {
        save_at(app, None)
    }
}

/// Saves for the vehicle (or the game): the profile, the speaker layout, the settings; the edit ends.
pub fn save_at(app: &mut MyApp, vehicle: Option<&str>) -> Result<(), String> {
    app.backend.save_profile(vehicle).map_err(|failure| failure.detail)?;
    if vehicle.is_some() {
        // The vehicle has its own profile now.
        app.ml.vehicle_level = true;
    }
    app.backend.store_shaker_effects().map_err(|failure| failure.detail)?;
    let _ = app.backend.post("settings/save", json!({}));
    app.clear_profile_edit_context()?;
    app.ui.tuning.library_applied = false;
    app.ui.snapshot = Some(app.docs.tune.clone());
    notices::success(app, "Success", "Settings and profile have been saved!");
    super::refresh(app, &[Region::Save, Region::Head]);
    Ok(())
}

/// Revert: while a profile is being edited the edit ends at once; otherwise the user confirms first.
pub fn revert_clicked(app: &mut MyApp) -> Result<(), String> {
    if app.backend.control_value(&["profile_edit"]).map_err(|error| error.detail)?.is_truthy() {
        app.clear_profile_edit_context()?;
        app.ui.tuning.library_applied = false;
        app.ui.resnapshot = true;
        notices::confirmation(app, "Changes reverted", "The profile is no longer being edited.");
    } else {
        dialogs::open(app, Dialog::Revert);
    }
    Ok(())
}

/// The revert was confirmed: the library profile choice is dropped and the service reloads the stored profile.
pub fn revert_confirmed(app: &mut MyApp) {
    app.ui.tuning.library_applied = false;
    super::screens::library::cancel(app);
    let _ = app.backend.post("profile/cancel", json!({}));
    app.ui.resnapshot = true;
    notices::confirmation(app, "Changes reverted", "Restored the saved settings.");
}

// ---- The profile list ------------------------------------------------------------------------------------

/// Reads the list: the games of the catalogue by name, each with its stored vehicles.
pub fn load(app: &mut MyApp) {
    let mut games: Vec<Value> = app.docs.games.as_array().cloned().unwrap_or_default();
    games.sort_by_key(|g| text(g.get("name").unwrap_or(&Value::Null)).to_lowercase());
    let mut entries = Vec::new();
    for game in games {
        let id = text(game.get("id").unwrap_or(&Value::Null));
        if id == "ccd" || id.is_empty() {
            continue;
        }
        let name = text(game.get("name").unwrap_or(&Value::Null));
        let vehicles = match app.backend.profile_row(&id) {
            Ok(Some(tune)) => vehicles_of(&tune),
            Ok(None) => Vec::new(),
            Err(failure) => {
                tracing::warn!(%failure, game = id, "profile list");
                Vec::new()
            }
        };
        entries.push(Entry { game: id, name, vehicles });
    }
    app.ui.profiles.entries = entries;
}

/// The vehicle names of a game's stored tune, sorted.
fn vehicles_of(tune: &LegacyValue) -> Vec<String> {
    let Ok(vehicles) = tune.item_owned("vehicles") else { return Vec::new() };
    let resolved = vehicles.resolved();
    let mut names: Vec<String> = resolved
        .as_dict()
        .map(|dict| {
            dict.keys().filter_map(|key| if let Key::Str(name) = key { Some(name.clone()) } else { None }).collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// The entries the filter keeps, each with the vehicles it shows. A filter of nothing but spaces keeps every
/// game and vehicle. Otherwise the filter (spaces included, any case) keeps the vehicles whose name holds it,
/// and a game stays when its name or one of its vehicles holds it; a game kept for its own name shows only its
/// matching vehicles.
pub fn filtered(app: &MyApp) -> Vec<(&Entry, Vec<&String>)> {
    let filter = &app.ui.profiles.filter;
    let entries = app.ui.profiles.entries.iter();
    if filter.replace(' ', "").is_empty() {
        return entries.map(|entry| (entry, entry.vehicles.iter().collect())).collect();
    }
    let needle = filter.to_lowercase();
    entries
        .filter_map(|entry| {
            let vehicles: Vec<&String> =
                entry.vehicles.iter().filter(|vehicle| vehicle.to_lowercase().contains(&needle)).collect();
            (entry.name.to_lowercase().contains(&needle) || !vehicles.is_empty()).then_some((entry, vehicles))
        })
        .collect()
}

/// Tuning → Profiles.
pub fn main(app: &MyApp) -> String {
    let s = &app.ui.session;
    let editing = s.state == super::session::State::Editing;
    let active = components::field(
        &t(app, "Active profile"),
        &format!(
            "<p>{}</p>{}",
            esc(&super::shell::profile_label(app)),
            if editing {
                components::button(&t(app, "Stop editing"), "profile-stop-edit", "", "")
            } else {
                String::new()
            }
        ),
        &if s.state == super::session::State::Standby { String::new() } else { t(app, s.state.label()) },
    );
    let filter = components::field(
        &t(app, "Find a game"),
        &format!(
            "<input id=\"profiles-filter\" class=\"field-input\" type=\"search\" value=\"{}\" placeholder=\"{}\" aria-label=\"{}\" data-act=\"profiles-filter\" data-live autocomplete=\"off\">",
            esc(&app.ui.profiles.filter),
            t(app, "Search games and vehicles"),
            t(app, "Find a game")
        ),
        "",
    );
    let list = filtered(app);
    let pages = list.len().div_ceil(PAGE).max(1);
    let page = app.ui.profiles.page.min(pages - 1);
    let rows: String = list
        .iter()
        .skip(page * PAGE)
        .take(PAGE)
        .map(|(entry, shown)| {
            let game = components::button(&t(app, "Edit"), "profile-edit", &entry.game, "")
                + &components::button(&t(app, "Reset"), "profile-reset", &entry.game, "")
                + &components::button(&t(app, "Export"), "profile-export", &entry.game, "");
            let vehicles: String = shown
                .iter()
                .map(|vehicle| {
                    let key = format!("{}^{vehicle}", entry.game);
                    format!(
                        "<div class=\"control-row profile-row vehicle\"><div class=\"control-label\"><span>{}</span></div><div class=\"control-body action-row\">{}{}{}</div></div>",
                        esc(vehicle),
                        components::button(&t(app, "Edit"), "profile-edit", &key, ""),
                        components::button(&t(app, "Delete"), "profile-delete", &key, ""),
                        components::button(&t(app, "Export"), "profile-export", &key, "")
                    )
                })
                .collect();
            format!(
                "<div class=\"control-row profile-row\"><div class=\"control-label\"><strong translate=\"no\">{}</strong></div><small>{}</small><div class=\"control-body action-row\">{game}</div></div>{vehicles}",
                esc(&entry.name),
                if entry.vehicles.is_empty() {
                    t(app, "Game profile")
                } else {
                    tf(app, "Game profile · ^count^ vehicles", &[("count", &entry.vehicles.len().to_string())])
                }
            )
        })
        .collect();
    let empty = if list.is_empty() {
        components::field(&t(app, "No matches"), &format!("<p>{}</p>", t(app, "Try another game name.")), "")
    } else {
        String::new()
    };
    let pager = if pages > 1 {
        format!(
            "<div class=\"device pager\">{}<small>{}</small>{}</div>",
            components::button(&t(app, "Previous"), "profiles-page", "prev", ""),
            tf(app, "Page ^page^ of ^pages^", &[("page", &(page + 1).to_string()), ("pages", &pages.to_string())]),
            components::button(&t(app, "Next"), "profiles-page", "next", "")
        )
    } else {
        String::new()
    };
    let tools = format!(
        "<div class=\"device action-row\"><label class=\"button-like\" for=\"profile-import\">{}<input id=\"profile-import\" type=\"file\" accept=\".srs,.json,.zip\" data-act=\"profile-import\" data-file hidden></label>{}{}{}</div>",
        t(app, "Import"),
        components::button(&t(app, "Export Profile"), "profile-export-current", "", ""),
        components::button(&t(app, "Export All"), "profile-export-all", "", ""),
        components::button(&t(app, "Downloaded profiles"), "profile-library", "", "")
    );
    components::panel(&themes::title(app, "Profiles"), "", &format!("{active}{filter}{rows}{empty}{pager}{tools}"), "")
}

/// The vehicle part of a `game^vehicle` argument: the vehicle's name, or the number zero for the game.
fn split(arg: &str) -> (String, Option<String>) {
    match arg.split_once('^') {
        Some((game, vehicle)) => (game.to_owned(), Some(vehicle.to_owned())),
        None => (arg.to_owned(), None),
    }
}

/// A profile list action.
pub fn act(app: &mut MyApp, act: &str, arg: &str, value: &str) -> Result<(), String> {
    match act {
        "profiles-filter" => {
            app.ui.profiles.filter = value.to_owned();
            app.ui.profiles.page = 0;
            super::refresh(app, &[Region::Main]);
        }
        "profiles-page" => {
            let pages = filtered(app).len().div_ceil(PAGE).max(1);
            let page = app.ui.profiles.page.min(pages - 1);
            app.ui.profiles.page = if arg == "next" { (page + 1).min(pages - 1) } else { page.saturating_sub(1) };
            super::refresh(app, &[Region::Main]);
        }
        "profile-edit" => {
            let (game, vehicle) = split(arg);
            let vehicle = vehicle.map_or(LegacyValue::Int(0), LegacyValue::Str);
            app.backend.begin_profile_edit(&game, &vehicle).map_err(|error| error.detail)?;
            if let Ok(controls) = app.backend.get("controls") {
                app.docs.controls = controls;
            }
            app.ui.resnapshot = true;
            super::navigate(app, Screen::Home, 0);
        }
        "profile-stop-edit" => {
            app.clear_profile_edit_context()?;
            app.ui.tuning.library_applied = false;
            app.ui.resnapshot = true;
            super::refresh(app, &[Region::Main, Region::Head]);
        }
        "profile-reset" => dialogs::open(app, Dialog::ProfileConfirm { game: arg.to_owned(), vehicle: None }),
        "profile-delete" => {
            let (game, vehicle) = split(arg);
            dialogs::open(app, Dialog::ProfileConfirm { game, vehicle });
        }
        "profile-export" => {
            let (game, vehicle) = split(arg);
            let vehicle = vehicle.map_or(LegacyValue::Int(0), LegacyValue::Str);
            let (data, name) = app.backend.profile_export(&game, &vehicle).map_err(|error| error.detail)?;
            download(app, &name, data, false)?;
        }
        "profile-export-current" => export_current(app)?,
        "profile-export-all" => {
            let (data, _) = app.backend.fetch("profiles/export-all").map_err(|error| error.detail)?;
            download(app, "srs_export_all.zip", data, true)?;
        }
        "profile-import" => {
            use base64::Engine as _;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(value.split_once(',').map_or(value, |(_, data)| data))
                .map_err(|error| format!("ValueError: {error}"))?;
            offer_import(app, arg, bytes);
        }
        "profile-library" => super::navigate(app, Screen::DownloadedProfiles, 0),
        _ => return Err(format!("unknown profile action {act:?}")),
    }
    Ok(())
}

/// A confirmed reset (game) or delete (vehicle).
pub fn confirm(app: &mut MyApp, game: &str, vehicle: Option<&str>) -> Result<(), String> {
    match vehicle {
        None => app.backend.delete(&format!("tunes/{}", segment(game))).map(|_| ()).map_err(|error| error.detail)?,
        Some(vehicle) => app
            .backend
            .delete(&format!("tunes/{}/vehicles/{}", segment(game), segment(vehicle)))
            .map(|_| ())
            .map_err(|error| error.detail)?,
    }
    load(app);
    super::refresh(app, &[Region::Main]);
    Ok(())
}

/// The profile in use, exported with its vehicle and location.
fn export_current(app: &mut MyApp) -> Result<(), String> {
    use openrigcue_model::{python_compare::native_equal, python_repr::str_value};
    let zero = LegacyValue::Int(0);
    let current = app.backend.session_value("vehicle_name").map_err(|error| error.detail)?;
    let vehicle = if native_equal(&current, &zero) {
        let previous = app.backend.previous_vehicle_value().map_err(|error| error.detail)?;
        if native_equal(&previous, &zero) {
            zero.clone()
        } else {
            LegacyValue::Str(str_value(&previous))
        }
    } else {
        LegacyValue::Str(str_value(&current))
    };
    let current = app.backend.location_value();
    let location = if native_equal(&current, &zero) {
        match app.ml.previous_location.as_ref() {
            Some(previous) if !native_equal(previous, &zero) => LegacyValue::Str(str_value(previous)),
            _ => zero.clone(),
        }
    } else {
        LegacyValue::Str(str_value(&current))
    };
    let game = app.backend.game_value().map_err(|error| error.detail)?;
    let game = crate::backend::game_catalog_key(&game).map_err(|error| error.detail)?.to_owned();
    let (data, name) = app.backend.profile_export_at(&game, &vehicle, Some(&location)).map_err(|error| error.detail)?;
    download(app, &name, data, false)
}

/// Offers a file for download at `/<id>/download` and opens it in the browser at the interface's own host
/// and port.
pub fn download(app: &mut MyApp, name: &str, data: Vec<u8>, export_all: bool) -> Result<(), String> {
    let id = format!("orc-download-{}", app.downloads.len() + app.export_all_downloads.len() + 1);
    if export_all {
        app.export_all_downloads.insert(id.clone(), 0);
    }
    app.downloads.insert(id.clone(), (name.to_owned(), data));
    let setting = |key: &str| match app.backend.native_setting("APP", key) {
        Ok(LegacyValue::Str(value)) => Ok(value),
        Ok(value) => Err(format!("TypeError: can only concatenate str (not \"{}\") to str", value.type_name())),
        Err(failure) => Err(failure.detail),
    };
    let url = format!("http://{}:{}/{id}/download", setting("webui_hostname")?, setting("webui_port")?);
    crate::net::open_browser(&url);
    notices::confirmation(app, "Export", "The file opens in your browser for download.");
    Ok(())
}

/// A profile file offered for import: it is checked, then the import question opens; a file that is not a
/// profile, or one the service refuses, is explained.
pub fn offer_import(app: &mut MyApp, name: &str, bytes: Vec<u8>) {
    let backend = app.backend.clone();
    let checked = check_import(&*backend, Ok(bytes));
    finish_offer(app, name, checked);
}

/// Reads a profile file's content and has the service check it: the profile, or why it is refused.
fn check_import(
    backend: &dyn crate::backend::Backend,
    bytes: Result<Vec<u8>, String>,
) -> Result<openrigcue_config::profiles::ProfileFile, String> {
    let bytes = bytes?;
    let content = openrigcue_config::profiles::read_profile_content(&bytes).map_err(|error| error.to_string())?;
    let file = openrigcue_config::profiles::ProfileFile::from_content(content).map_err(|error| error.to_string())?;
    backend.upload("profiles/import-check", bytes).map_err(|error| error.detail)?;
    Ok(file)
}

/// The checked offer: the import question, or the reason it is refused.
fn finish_offer(app: &mut MyApp, name: &str, checked: Result<openrigcue_config::profiles::ProfileFile, String>) {
    match checked {
        Ok(file) => {
            app.ui.profiles.pending_import = Some(file);
            dialogs::open(app, Dialog::Import { name: name.to_owned() });
        }
        Err(message) => {
            app.ui.profiles.pending_import = None;
            let title = tr(app, "Import Profile");
            dialogs::open(app, Dialog::Message { title, text: app.backend.translate(&message) });
        }
    }
}

/// The text of an unreadable file.
const REFUSED: &str = "Import refused: this file is not a valid OpenRigCue profile";

/// The argument of the import call (`/import/api_set_file_path?value1=…`): `value1` and nothing else.
pub(crate) fn import_argument(params: &[(String, String)]) -> Result<&str, String> {
    const API_SET_FILE_PATH: crate::remi::classes::Signature = crate::remi::classes::Signature {
        qualname: "RemoteImport.api_set_file_path",
        params: &["value1"],
        required: 1,
        varargs: false,
        varkw: false,
    };
    crate::remi::classes::bind(
        &API_SET_FILE_PATH,
        false,
        params.iter().map(|(key, value)| (key.as_str(), value.as_str())),
    )?;
    Ok(params.iter().find(|(key, _)| key == "value1").map(|(_, value)| value.as_str()).expect("bound"))
}

/// A download call (`/<id>/download`) takes no argument.
pub(crate) fn download_arguments(class: &str, params: &[(String, String)]) -> Result<(), String> {
    if let Some((key, _)) = params.first() {
        return Err(if key == "self" {
            format!("TypeError: {class}.download() got multiple values for argument 'self'")
        } else {
            crate::remi::classes::unexpected_keyword(&format!("{class}.download"), &[], key)
        });
    }
    Ok(())
}

fn pointer_io_error(error: std::io::Error) -> String {
    let class = match error.kind() {
        std::io::ErrorKind::NotFound => "FileNotFoundError",
        std::io::ErrorKind::PermissionDenied => "PermissionError",
        _ => "OSError",
    };
    format!("{class}: {error}")
}

/// The path of the profile file the launcher handed over: this start's `--profile_path` when `value1` is
/// `new_instance`; for `existing_instance` the path written in `profile.tmp` in the documents folder, which is
/// then deleted.
pub(crate) fn import_path(app: &MyApp, value1: &str) -> Result<String, String> {
    let required_text = |path: &[&str]| {
        let value = crate::backend::field(&app.docs.status, path)?;
        value.as_str().map(str::to_owned).ok_or_else(|| "TypeError: expected str, bytes or os.PathLike object".into())
    };
    match value1 {
        "new_instance" => required_text(&["arguments", "profile_path"]),
        "existing_instance" => {
            let pointer = std::path::PathBuf::from(required_text(&["documents"])?).join("profile.tmp");
            // The launcher writes UTF-8; the file is closed before it is removed.
            let path = std::fs::read_to_string(&pointer).map_err(pointer_io_error)?;
            std::fs::remove_file(&pointer).map_err(pointer_io_error)?;
            Ok(path)
        }
        _ => {
            Err("UnboundLocalError: cannot access local variable 'path' where it is not associated with a value".into())
        }
    }
}

/// The launcher handed over a profile file (`new_instance`: this start's `--profile_path`;
/// `existing_instance`: the hand-over file in the documents folder).
pub fn offer_launcher_import(app: &mut MyApp, value1: &str) -> Result<(), String> {
    let path = import_path(app, value1)?;
    app.ml_import_path = Some(path.clone());
    let backend = app.backend.clone();
    let checked = check_import(&*backend, std::fs::read(&path).map_err(|_| REFUSED.to_owned()));
    finish_offer(app, &path, checked);
    Ok(())
}

/// [`offer_launcher_import`] without holding the application while the service checks the file.
pub(crate) fn offer_launcher_import_shared(shared: &std::sync::Mutex<MyApp>, value1: &str) -> Result<(), String> {
    let (path, backend) = {
        let mut app = crate::host::lock(shared);
        let path = import_path(&app, value1)?;
        app.ml_import_path = Some(path.clone());
        (path, app.backend.clone())
    };
    let checked = check_import(&*backend, std::fs::read(&path).map_err(|_| REFUSED.to_owned()));
    finish_offer(&mut crate::host::lock(shared), &path, checked);
    Ok(())
}

/// The import was confirmed: the service imports the offered profile.
pub fn import_confirmed(app: &mut MyApp) {
    match app.ui.profiles.pending_import.take() {
        Some(file) => match app.backend.confirm_profile_import(&file) {
            Ok(()) => notices::confirmation(app, "Import Profile", "Import complete. Click SAVE to keep the profile."),
            Err(failure) => notices::warning(app, "Import Profile", &failure.detail),
        },
        None => notices::warning(app, "Import Profile", &tr(app, REFUSED)),
    }
    load(app);
    super::refresh(app, &[Region::Main]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vehicle_arguments_split_at_the_caret() {
        assert_eq!(split("acc"), ("acc".to_owned(), None));
        assert_eq!(split("acc^Porsche 911"), ("acc".to_owned(), Some("Porsche 911".to_owned())));
    }
}

#[cfg(test)]
#[path = "profiles_tests.rs"]
mod profiles_tests;
