//! Tests of Tuning → Profiles: the list of games and vehicles, its edit, reset, delete and export, the export
//! of the profile in use, Export All, the downloads they offer, and the launcher's import hand-over.

use std::sync::{Arc, Mutex};

use openrigcue_model::native_value::Dict;

use super::*;
use crate::app::tests::{loaded, FakeBackend};
use crate::backend::{Backend, Failure, Method};
use crate::host::UiApp;
use crate::ui::{dispatch, navigate};

/// A service with stored game rows and a controllable session identity; records the identity reads, the
/// edits and the exports.
struct ProfilesBackend {
    inner: Arc<FakeBackend>,
    rows: Mutex<Vec<(String, LegacyValue)>>,
    reads: Mutex<Vec<String>>,
    native_edits: Mutex<Vec<LegacyValue>>,
    exports: Mutex<Vec<LegacyValue>>,
    exports_at: Mutex<Vec<(String, LegacyValue, Option<LegacyValue>)>>,
    location: Mutex<LegacyValue>,
    game: Mutex<LegacyValue>,
    vehicle: Mutex<LegacyValue>,
    previous_vehicle: Mutex<LegacyValue>,
    identity_reads: Mutex<Vec<&'static str>>,
    failed_identity_read: Mutex<Option<&'static str>>,
    fail_delete: Mutex<bool>,
    fail_export: Mutex<bool>,
    fail_fetch: Mutex<bool>,
    download_native: Mutex<Option<(LegacyValue, LegacyValue)>>,
    download_reads: Mutex<Vec<String>>,
}

impl ProfilesBackend {
    fn identity(&self, field: &'static str) -> Result<(), Failure> {
        self.identity_reads.lock().unwrap().push(field);
        if *self.failed_identity_read.lock().unwrap() == Some(field) {
            Err(Failure { status: 500, detail: format!("KeyError: '{field}'") })
        } else {
            Ok(())
        }
    }

    fn export_failure(&self) -> Result<(), Failure> {
        if *self.fail_export.lock().unwrap() {
            Err(Failure { status: 500, detail: "ValueError: fixture profile export failed".into() })
        } else {
            Ok(())
        }
    }
}

impl Backend for ProfilesBackend {
    fn native_setting(&self, section: &str, key: &str) -> Result<LegacyValue, Failure> {
        if let Some((host, port)) = self.download_native.lock().unwrap().as_ref() {
            if section == "APP" && (key == "webui_hostname" || key == "webui_port") {
                self.download_reads.lock().unwrap().push(key.into());
                return Ok(if key == "webui_hostname" { host.clone() } else { port.clone() });
            }
        }
        self.inner.native_setting(section, key)
    }
    fn location_value(&self) -> LegacyValue {
        self.identity_reads.lock().unwrap().push("location");
        self.location.lock().unwrap().clone()
    }
    fn game_value(&self) -> Result<LegacyValue, Failure> {
        self.identity("game")?;
        Ok(self.game.lock().unwrap().clone())
    }
    fn session_value(&self, parameter: &str) -> Result<LegacyValue, Failure> {
        if parameter != "vehicle_name" {
            return self.inner.session_value(parameter);
        }
        self.identity("vehicle")?;
        Ok(self.vehicle.lock().unwrap().clone())
    }
    fn previous_vehicle_value(&self) -> Result<LegacyValue, Failure> {
        self.identity("previous_vehicle")?;
        Ok(self.previous_vehicle.lock().unwrap().clone())
    }
    fn profile_export_at(
        &self,
        game: &str,
        vehicle: &LegacyValue,
        location: Option<&LegacyValue>,
    ) -> Result<(Vec<u8>, String), Failure> {
        self.identity_reads.lock().unwrap().push("export");
        self.exports_at.lock().unwrap().push((game.into(), vehicle.clone(), location.cloned()));
        self.export_failure()?;
        Ok((b"current".to_vec(), "current.srs".into()))
    }
    fn profile_export(&self, _game: &str, vehicle: &LegacyValue) -> Result<(Vec<u8>, String), Failure> {
        self.exports.lock().unwrap().push(vehicle.clone());
        self.export_failure()?;
        Ok((b"profile".to_vec(), "profile.srs".into()))
    }
    fn begin_profile_edit(&self, game: &str, vehicle: &LegacyValue) -> Result<(), Failure> {
        self.native_edits.lock().unwrap().push(vehicle.clone());
        self.inner.begin_profile_edit(game, vehicle)
    }
    fn profile_row(&self, game: &str) -> Result<Option<LegacyValue>, Failure> {
        self.reads.lock().unwrap().push(game.into());
        Ok(self.rows.lock().unwrap().iter().find(|(key, _)| key == game).map(|(_, row)| row.clone()))
    }
    fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value, Failure> {
        if method == Method::Delete && path.starts_with("tunes/") {
            if *self.fail_delete.lock().unwrap() {
                return Err(Failure { status: 500, detail: "RuntimeError: fixture profile deletion failed".into() });
            }
            if let Some((game, vehicle)) = path["tunes/".len()..].split_once("/vehicles/") {
                for (key, row) in self.rows.lock().unwrap().iter_mut() {
                    if key == game {
                        let mut kept = Dict::new();
                        let vehicles = row.item_owned("vehicles").unwrap();
                        for (name, value) in vehicles.as_dict().unwrap().iter() {
                            if name.to_text() != vehicle {
                                kept.insert(name.clone(), value.clone());
                            }
                        }
                        let mut fields = Dict::new();
                        fields.insert("vehicles", kept.into());
                        *row = fields.into();
                    }
                }
            }
        }
        self.inner.call(method, path, body)
    }
    fn upload(&self, path: &str, bytes: Vec<u8>) -> Result<Value, Failure> {
        self.inner.upload(path, bytes)
    }
    fn fetch(&self, path: &str) -> Result<(Vec<u8>, String), Failure> {
        if *self.fail_fetch.lock().unwrap() {
            return Err(Failure { status: 500, detail: "ValueError: fixture profile export failed".into() });
        }
        self.inner.fetch(path)
    }
    fn translate(&self, text: &str) -> String {
        self.inner.translate(text)
    }
    fn excluded_strings(&self) -> Vec<String> {
        Vec::new()
    }
}

/// A stored game row with these vehicle keys.
fn row(keys: &[Key]) -> LegacyValue {
    let vehicles: Dict = keys.iter().map(|key| (key.clone(), LegacyValue::None)).collect();
    let mut row = Dict::new();
    row.insert("vehicles", vehicles.into());
    row.into()
}

/// The started app on a service whose "ac" (Alpha) row is `first` and whose "acc" (Zulu) row has no
/// vehicles; game "ac" runs with no vehicle and no location.
fn fixture(first: LegacyValue) -> (MyApp, Arc<ProfilesBackend>) {
    let inner = FakeBackend::new();
    let mut app = loaded(inner.clone());
    let backend = Arc::new(ProfilesBackend {
        inner,
        rows: Mutex::new(vec![("ac".into(), first), ("acc".into(), row(&[]))]),
        reads: Mutex::new(Vec::new()),
        native_edits: Mutex::new(Vec::new()),
        exports: Mutex::new(Vec::new()),
        exports_at: Mutex::new(Vec::new()),
        location: Mutex::new(LegacyValue::Int(0)),
        game: Mutex::new(LegacyValue::Str("ac".into())),
        vehicle: Mutex::new(LegacyValue::Int(0)),
        previous_vehicle: Mutex::new(LegacyValue::Int(0)),
        identity_reads: Mutex::new(Vec::new()),
        failed_identity_read: Mutex::new(None),
        fail_delete: Mutex::new(false),
        fail_export: Mutex::new(false),
        fail_fetch: Mutex::new(false),
        download_native: Mutex::new(None),
        download_reads: Mutex::new(Vec::new()),
    });
    app.backend = backend.clone();
    app.docs.games = json!([{"id": "acc", "name": "Zulu"}, {"id": "ac", "name": "Alpha"}]);
    (app, backend)
}

/// Whether a notice with this message was raised.
fn noticed(app: &MyApp, message: &str) -> bool {
    app.ui.notices.history.iter().any(|n| n.message == message)
}

/// The vehicle names listed under a game.
fn listed(app: &MyApp, game: &str) -> Vec<String> {
    app.ui.profiles.entries.iter().find(|e| e.game == game).map(|e| e.vehicles.clone()).unwrap_or_default()
}

#[test]
fn export_current_reads_the_vehicle_then_location_and_game_and_a_failed_export_keeps_no_download() {
    let (mut app, backend) = fixture(row(&[]));
    *backend.fail_export.lock().unwrap() = true;
    *backend.game.lock().unwrap() = "acc".into();
    *backend.vehicle.lock().unwrap() = "Current car".into();
    *backend.location.lock().unwrap() = "Track".into();
    backend.identity_reads.lock().unwrap().clear();
    let error = dispatch(&mut app, "profile-export-current", "", "").unwrap_err();
    assert_eq!(error, "ValueError: fixture profile export failed");
    assert_eq!(*backend.exports_at.lock().unwrap(), [("acc".into(), "Current car".into(), Some("Track".into()))]);
    assert_eq!(*backend.identity_reads.lock().unwrap(), ["vehicle", "location", "game", "export"]);
    assert!(app.downloads.is_empty());
    assert!(!noticed(&app, "The file opens in your browser for download."));
    drop(app.take_threads());
}

#[test]
fn export_current_falls_back_to_the_previous_vehicle_for_zero_aliases() {
    for (current, previous, expected) in [
        (LegacyValue::Int(0), LegacyValue::from("Previous car"), LegacyValue::from("Previous car")),
        (LegacyValue::Bool(false), LegacyValue::Int(0), LegacyValue::Int(0)),
        (LegacyValue::Float(-0.0), LegacyValue::from(""), LegacyValue::from("")),
    ] {
        let (mut app, backend) = fixture(row(&[]));
        *backend.fail_export.lock().unwrap() = true;
        *backend.vehicle.lock().unwrap() = current;
        *backend.previous_vehicle.lock().unwrap() = previous;
        backend.identity_reads.lock().unwrap().clear();
        assert!(dispatch(&mut app, "profile-export-current", "", "").is_err());
        assert_eq!(backend.exports_at.lock().unwrap()[0], ("ac".into(), expected, Some(LegacyValue::Int(0))));
        assert_eq!(
            *backend.identity_reads.lock().unwrap(),
            ["vehicle", "previous_vehicle", "location", "game", "export"]
        );
        assert!(app.downloads.is_empty());
        drop(app.take_threads());
    }
}

#[test]
fn export_current_identity_errors_stop_before_later_reads_and_the_export() {
    for (failed, trace) in [
        ("vehicle", vec!["vehicle"]),
        ("previous_vehicle", vec!["vehicle", "previous_vehicle"]),
        ("game", vec!["vehicle", "previous_vehicle", "location", "game"]),
    ] {
        let (mut app, backend) = fixture(row(&[]));
        *backend.failed_identity_read.lock().unwrap() = Some(failed);
        backend.identity_reads.lock().unwrap().clear();
        let error = dispatch(&mut app, "profile-export-current", "", "").unwrap_err();
        assert_eq!(error, format!("KeyError: '{failed}'"));
        assert_eq!(*backend.identity_reads.lock().unwrap(), trace);
        assert!(backend.exports_at.lock().unwrap().is_empty());
        assert!(app.downloads.is_empty());
        drop(app.take_threads());
    }
}

#[test]
fn list_export_failures_are_reported_without_a_download() {
    let (mut app, backend) = fixture(row(&[Key::from("Car")]));
    navigate(&mut app, Screen::Profiles, 0);
    *backend.fail_export.lock().unwrap() = true;
    let error = dispatch(&mut app, "profile-export", "ac", "").unwrap_err();
    assert_eq!(error, "ValueError: fixture profile export failed");
    assert_eq!(*backend.exports.lock().unwrap(), [LegacyValue::Int(0)], "a game exports with the number zero");
    assert!(app.downloads.is_empty());
    assert_eq!(app.ui.screen, Screen::Profiles, "the list stays open");
    // Export All.
    *backend.fail_fetch.lock().unwrap() = true;
    let error = dispatch(&mut app, "profile-export-all", "", "").unwrap_err();
    assert_eq!(error, "ValueError: fixture profile export failed");
    assert!(app.downloads.is_empty() && app.export_all_downloads.is_empty());
    // The profile in use, with its vehicle.
    *backend.vehicle.lock().unwrap() = "Car".into();
    let error = dispatch(&mut app, "profile-export-current", "", "").unwrap_err();
    assert_eq!(error, "ValueError: fixture profile export failed");
    let exported = backend.exports_at.lock().unwrap()[0].clone();
    assert_eq!((exported.0.as_str(), exported.1), ("ac", LegacyValue::Str("Car".into())));
    assert!(app.downloads.is_empty());
    drop(app.take_threads());
}

#[test]
fn export_current_uses_the_remembered_location_and_keeps_empty_names() {
    let (mut app, backend) = fixture(row(&[]));
    *backend.fail_export.lock().unwrap() = true;
    let last = |backend: &ProfilesBackend| backend.exports_at.lock().unwrap().last().unwrap().clone();
    *backend.location.lock().unwrap() = "Track".into();
    app.remember_location().unwrap();
    for zero in [LegacyValue::Int(0), LegacyValue::Bool(false), LegacyValue::Float(-0.0)] {
        *backend.location.lock().unwrap() = zero;
        assert!(dispatch(&mut app, "profile-export-current", "", "").is_err());
        assert_eq!(last(&backend), ("ac".into(), LegacyValue::Int(0), Some("Track".into())));
    }
    // A remembered None is exported as its text; an empty vehicle name stays an empty name.
    *backend.location.lock().unwrap() = LegacyValue::None;
    app.remember_location().unwrap();
    *backend.location.lock().unwrap() = LegacyValue::Int(0);
    *backend.vehicle.lock().unwrap() = "".into();
    assert!(dispatch(&mut app, "profile-export-current", "", "").is_err());
    assert_eq!(last(&backend), ("ac".into(), "".into(), Some("None".into())));
    *backend.location.lock().unwrap() = "".into();
    app.remember_location().unwrap();
    *backend.location.lock().unwrap() = LegacyValue::Int(0);
    assert!(dispatch(&mut app, "profile-export-current", "", "").is_err());
    assert_eq!(last(&backend).2, Some("".into()));
    // Stand-by forgets the location.
    *backend.game.lock().unwrap() = "none".into();
    app.remember_location().unwrap();
    assert!(app.ml.previous_location.is_none());
    *backend.game.lock().unwrap() = "ac".into();
    assert!(dispatch(&mut app, "profile-export-current", "", "").is_err());
    assert_eq!(last(&backend).2, Some(LegacyValue::Int(0)));
    drop(app.take_threads());
}

#[test]
fn remembered_location_is_cleared_when_no_game_runs() {
    let (mut app, backend) = fixture(row(&[]));
    *backend.location.lock().unwrap() = "Track".into();
    app.remember_location().unwrap();
    assert_eq!(app.ml.previous_location, Some("Track".into()));
    *backend.game.lock().unwrap() = "none".into();
    app.remember_location().unwrap();
    assert!(app.ml.previous_location.is_none());
    drop(app.take_threads());
}

/// The list shows the text vehicle keys of a stored tune only; a numeric, boolean or empty key neither stops
/// the list nor gets a row.
#[test]
fn non_text_vehicle_keys_are_left_out_of_the_list() {
    let keys = [Key::Int(5), Key::from("Car"), Key::Bool(true), Key::None, Key::float(1.5).unwrap()];
    let (mut app, backend) = fixture(row(&keys));
    navigate(&mut app, Screen::Profiles, 0);
    assert_eq!(*backend.reads.lock().unwrap(), ["ac", "acc"], "every game is read, by name");
    assert_eq!(listed(&app, "ac"), ["Car"]);
    let body = crate::ui::screens::main(&app);
    assert_eq!(body.matches("data-act=\"profile-edit\"").count(), 3, "two games and one vehicle");
    assert!(body.contains("data-arg=\"ac^Car\""));
    drop(app.take_threads());
}

#[test]
fn games_are_listed_by_name_with_their_vehicles_in_order_and_ccd_hidden() {
    let (mut app, backend) = fixture(row(&[Key::from("ä"), Key::from("Z"), Key::from("a")]));
    app.docs.games.as_array_mut().unwrap().push(json!({"id": "ccd", "name": "Middle"}));
    app.docs.games.as_array_mut().unwrap().push(json!({"id": "missing", "name": "Missing"}));
    navigate(&mut app, Screen::Profiles, 0);
    assert_eq!(*backend.reads.lock().unwrap(), ["ac", "missing", "acc"]);
    let games: Vec<&str> = app.ui.profiles.entries.iter().map(|e| e.game.as_str()).collect();
    assert_eq!(games, ["ac", "missing", "acc"], "by name; ccd is not listed");
    assert_eq!(listed(&app, "ac"), ["Z", "a", "ä"]);
    assert!(listed(&app, "missing").is_empty(), "a game without a stored row has no vehicles");
    let body = crate::ui::screens::main(&app);
    let order: Vec<usize> = ["data-arg=\"ac^Z\"", "data-arg=\"ac^a\"", "data-arg=\"ac^ä\""]
        .iter()
        .map(|arg| body.find(arg).unwrap())
        .collect();
    assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{order:?}");
    // The exports pass the number zero for the game and the name for a vehicle; the download is kept even
    // when the host setting cannot make its address.
    *backend.download_native.lock().unwrap() = Some((LegacyValue::Int(7), LegacyValue::from("33733")));
    assert!(dispatch(&mut app, "profile-export", "ac", "").is_err());
    assert!(dispatch(&mut app, "profile-export", "ac^Z", "").is_err());
    assert_eq!(*backend.exports.lock().unwrap(), [LegacyValue::Int(0), LegacyValue::Str("Z".into())]);
    assert_eq!(app.downloads.len(), 2);
    // Edit of a game passes the number zero, of a vehicle its name.
    dispatch(&mut app, "profile-edit", "ac", "").unwrap();
    navigate(&mut app, Screen::Profiles, 0);
    dispatch(&mut app, "profile-edit", "ac^a", "").unwrap();
    assert_eq!(*backend.native_edits.lock().unwrap(), [LegacyValue::Int(0), LegacyValue::Str("a".into())]);
    drop(app.take_threads());
}

#[test]
fn reset_and_delete_questions_follow_the_last_action() {
    let (mut app, backend) = fixture(row(&[Key::from("Car")]));
    navigate(&mut app, Screen::Profiles, 0);
    backend.inner.calls.lock().unwrap().clear();
    let reset = "Delete every custom game/vehicle setting and put the game back to its defaults?";
    let delete = "Do you really want to delete this vehicle&#39;s settings?";
    for (act, arg, expected, other) in [
        ("profile-reset", "ac", reset, delete),
        ("profile-delete", "ac^Car", delete, reset),
        ("profile-reset", "ac", reset, delete),
    ] {
        dispatch(&mut app, act, arg, "").unwrap();
        let dialog = crate::ui::dialogs::render(&app);
        assert!(dialog.contains("Reset or delete this profile?") && dialog.contains(expected), "{dialog}");
        assert!(!dialog.contains(other));
        dispatch(&mut app, "dialog-close", "", "").unwrap();
        assert!(app.ui.dialog.is_none());
    }
    assert!(backend
        .inner
        .calls
        .lock()
        .unwrap()
        .iter()
        .all(|(m, p, _)| !(*m == Method::Delete && p.starts_with("tunes/"))));
    assert!(backend.native_edits.lock().unwrap().is_empty());
    assert_eq!(listed(&app, "ac"), ["Car"]);
    drop(app.take_threads());
}

#[test]
fn a_failed_vehicle_delete_keeps_the_row_until_a_retry_succeeds() {
    let (mut app, backend) = fixture(row(&[Key::from("Car")]));
    navigate(&mut app, Screen::Profiles, 0);
    *backend.fail_delete.lock().unwrap() = true;
    dispatch(&mut app, "profile-delete", "ac^Car", "").unwrap();
    let error = dispatch(&mut app, "dialog-ok", "", "").unwrap_err();
    assert_eq!(error, "RuntimeError: fixture profile deletion failed");
    assert!(app.ui.dialog.is_none());
    assert_eq!(listed(&app, "ac"), ["Car"]);
    assert!(crate::ui::screens::main(&app).contains("data-arg=\"ac^Car\""));
    *backend.fail_delete.lock().unwrap() = false;
    dispatch(&mut app, "profile-delete", "ac^Car", "").unwrap();
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert!(backend
        .inner
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|(m, p, _)| *m == Method::Delete && p == "tunes/ac/vehicles/Car"));
    assert!(listed(&app, "ac").is_empty());
    assert!(!crate::ui::screens::main(&app).contains("data-arg=\"ac^Car\""));
    // A deleted vehicle no longer matches the filter.
    dispatch(&mut app, "profiles-filter", "", "Car").unwrap();
    assert!(crate::ui::screens::main(&app).contains("Try another game name."));
    drop(app.take_threads());
}

#[test]
fn a_failed_reset_is_reported_and_can_be_confirmed_again() {
    let (mut app, backend) = fixture(row(&[Key::from("Car")]));
    navigate(&mut app, Screen::Profiles, 0);
    *backend.fail_delete.lock().unwrap() = true;
    dispatch(&mut app, "profile-reset", "ac", "").unwrap();
    let error = dispatch(&mut app, "dialog-ok", "", "").unwrap_err();
    assert_eq!(error, "RuntimeError: fixture profile deletion failed");
    assert!(app.ui.dialog.is_none());
    assert_eq!(listed(&app, "ac"), ["Car"], "the rows stay");
    *backend.fail_delete.lock().unwrap() = false;
    backend.inner.calls.lock().unwrap().clear();
    dispatch(&mut app, "profile-reset", "ac", "").unwrap();
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert!(backend.inner.calls.lock().unwrap().iter().any(|(m, p, _)| *m == Method::Delete && p == "tunes/ac"));
    drop(app.take_threads());
}

/// The download address is made from the current `[APP] webui_hostname` and `webui_port` as stored (text
/// only), host first; the file is kept for its download before the address is made, and a setting that
/// cannot make the address stops before the browser opens.
#[test]
fn the_download_address_reads_the_current_host_then_port_as_stored() {
    for (native, expected) in [
        (json!({}), "KeyError: 'APP'"),
        (json!({"APP": {}}), "KeyError: 'webui_hostname'"),
        (json!({"APP": {"webui_hostname": 7}}), "TypeError: can only concatenate str (not \"int\") to str"),
        (json!({"APP": {"webui_hostname": "current.host"}}), "KeyError: 'webui_port'"),
        (
            json!({"APP": {"webui_hostname": "current.host", "webui_port": 33733}}),
            "TypeError: can only concatenate str (not \"int\") to str",
        ),
    ] {
        let backend = FakeBackend::new();
        let mut app = loaded(backend.clone());
        app.docs.settings["app"]["web_ui_host"] = json!("stale.host");
        backend.docs.lock().unwrap()["native_settings"] = native;
        let error = dispatch(&mut app, "profile-export-all", "", "").unwrap_err();
        assert_eq!(error, expected);
        assert_eq!(app.downloads.len(), 1);
        assert_eq!(app.downloads.values().next().unwrap(), &("srs_export_all.zip".into(), b"zip".to_vec()));
        assert!(!noticed(&app, "The file opens in your browser for download."), "the browser does not open");
        drop(app.take_threads());
    }
}

#[test]
fn a_list_export_keeps_its_download_before_the_host_error() {
    let (mut app, backend) = fixture(row(&[]));
    navigate(&mut app, Screen::Profiles, 0);
    *backend.download_native.lock().unwrap() = Some((LegacyValue::Int(7), LegacyValue::from("33733")));
    let error = dispatch(&mut app, "profile-export", "ac", "").unwrap_err();
    assert_eq!(error, "TypeError: can only concatenate str (not \"int\") to str");
    assert_eq!(*backend.download_reads.lock().unwrap(), ["webui_hostname"]);
    assert_eq!(app.downloads.len(), 1);
    assert_eq!(app.downloads.values().next().unwrap(), &("profile.srs".into(), b"profile".to_vec()));
    assert_eq!(*backend.exports.lock().unwrap(), [LegacyValue::Int(0)]);
    drop(app.take_threads());
}

/// The app after Export All, whose address could not be made: its download is offered.
fn exported_all() -> (MyApp, String) {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    backend.docs.lock().unwrap()["native_settings"] = json!({"APP": {"webui_hostname": 7}});
    assert!(dispatch(&mut app, "profile-export-all", "", "").is_err());
    let identifier = app.export_all_downloads.keys().next().unwrap().clone();
    (app, identifier)
}

#[test]
fn the_export_all_download_answers_once_then_fails_to_unpack() {
    let (mut app, identifier) = exported_all();
    let answer = app.attribute_call(&identifier, "download", &[]).unwrap();
    assert_eq!(answer.content, b"zip");
    assert!(answer.headers.iter().any(|(_, v)| v == "attachment; filename=\"srs_export_all.zip\""));
    assert!(app.attribute_call(&identifier, "download", &[]).is_none());
    assert!(app.attribute_call(&identifier, "download", &[]).is_none());
    assert_eq!(
        app.attribute_call_checked(&identifier, "download", &[]).err().as_deref(),
        Some("ValueError: too many values to unpack (expected 2)")
    );
    assert_eq!(app.downloads.len(), 1, "the failed unpack keeps its download");
    assert_eq!(app.export_all_downloads[&identifier], 4);
    drop(app.take_threads());
}

#[test]
fn downloads_refuse_keyword_arguments_without_counting() {
    let (mut app, identifier) = exported_all();
    assert!(app.attribute_call(&identifier, "download", &[("unexpected".into(), "x".into())]).is_none());
    assert_eq!(app.export_all_downloads[&identifier], 0);
    assert_eq!(app.attribute_call(&identifier, "download", &[]).unwrap().content, b"zip");
    // A profile export answers every request.
    let (mut app, backend) = fixture(row(&[]));
    *backend.download_native.lock().unwrap() = Some((LegacyValue::Int(7), LegacyValue::from("33733")));
    assert!(dispatch(&mut app, "profile-export", "ac", "").is_err());
    let ordinary = app.downloads.keys().next().unwrap().clone();
    for _ in 0..3 {
        assert_eq!(app.attribute_call(&ordinary, "download", &[]).unwrap().content, b"profile");
    }
    assert_eq!(
        app.attribute_call_checked(&ordinary, "download", &[("self".into(), "x".into())]).err().as_deref(),
        Some("TypeError: MyFileDownloader.download() got multiple values for argument 'self'")
    );
    assert_eq!(
        app.attribute_call_checked(&ordinary, "download", &[("bad".into(), "x".into())]).err().as_deref(),
        Some("TypeError: MyFileDownloader.download() got an unexpected keyword argument 'bad'")
    );
    drop(app.take_threads());
}

/// A folder of the system's temporary directory for one test.
fn folder(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("openrigcue-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn a_missing_handover_file_stops_before_the_import_state() {
    let (mut app, _) = fixture(row(&[]));
    app.docs.status = json!({"documents": "/openrigcue-test-missing-pointer-root"});
    app.ml_import_path = Some("previous.srs".into());
    let error = offer_launcher_import(&mut app, "existing_instance").unwrap_err();
    assert!(error.starts_with("FileNotFoundError:"), "{error}");
    assert_eq!(app.ml_import_path.as_deref(), Some("previous.srs"));
    assert!(app.ui.dialog.is_none() && app.ui.profiles.pending_import.is_none());
    drop(app.take_threads());
}

#[test]
fn a_handover_file_that_cannot_be_removed_keeps_the_previous_import_path() {
    use std::os::unix::fs::PermissionsExt as _;
    let documents = folder("locked-pointer");
    let pointer = documents.join("profile.tmp");
    std::fs::write(&pointer, "offered.srs").unwrap();
    std::fs::set_permissions(&documents, std::fs::Permissions::from_mode(0o555)).unwrap();
    let (mut app, _) = fixture(row(&[]));
    app.docs.status = json!({"documents": documents});
    app.ml_import_path = Some("old.srs".into());
    let result = offer_launcher_import(&mut app, "existing_instance");
    std::fs::set_permissions(&documents, std::fs::Permissions::from_mode(0o755)).unwrap();
    let error = result.unwrap_err();
    assert!(error.starts_with("PermissionError:"), "{error}");
    assert!(pointer.exists(), "the hand-over file was read, not removed");
    assert_eq!(app.ml_import_path.as_deref(), Some("old.srs"));
    assert!(app.ui.dialog.is_none());
    std::fs::remove_file(pointer).unwrap();
    std::fs::remove_dir(documents).unwrap();
    drop(app.take_threads());
}

/// The launcher's hand-over call `/import/api_set_file_path` binds its one argument `value1` before any path
/// is read; an unknown instance kind is refused; nothing changes.
#[test]
fn the_launcher_import_arguments_are_bound_before_any_path_operation() {
    let mut app = loaded(FakeBackend::new());
    app.ml_import_path = Some("prior.srs".into());
    for (arguments, expected) in [
        (vec![], "TypeError: RemoteImport.api_set_file_path() missing 1 required positional argument: 'value1'"),
        (
            vec![("self".into(), "x".into())],
            "TypeError: RemoteImport.api_set_file_path() got multiple values for argument 'self'",
        ),
        (
            vec![("value1".into(), "existing_instance".into()), ("bad".into(), "x".into())],
            "TypeError: RemoteImport.api_set_file_path() got an unexpected keyword argument 'bad'",
        ),
        (
            vec![("value1".into(), "unknown".into())],
            "UnboundLocalError: cannot access local variable 'path' where it is not associated with a value",
        ),
    ] {
        let result = app.attribute_call_checked("import", "api_set_file_path", &arguments);
        assert_eq!(result.err().as_deref(), Some(expected));
        assert_eq!(app.ml_import_path.as_deref(), Some("prior.srs"));
        assert!(app.ui.dialog.is_none());
    }
    assert_eq!(
        offer_launcher_import(&mut app, "unknown").unwrap_err(),
        "UnboundLocalError: cannot access local variable 'path' where it is not associated with a value"
    );
    assert_eq!(app.ml_import_path.as_deref(), Some("prior.srs"));
    drop(app.take_threads());
}

#[test]
fn the_handover_file_is_removed_before_its_path_is_offered() {
    let documents = folder("import-pointer");
    let payload = documents.join("offered.srs");
    let pointer = documents.join("profile.tmp");
    std::fs::write(&payload, b"invalid archive still assigns the accepted path").unwrap();
    std::fs::write(&pointer, payload.to_str().unwrap()).unwrap();
    let (mut app, _) = fixture(row(&[]));
    app.docs.status = json!({"documents": documents});
    offer_launcher_import(&mut app, "existing_instance").unwrap();
    assert!(!pointer.exists());
    assert_eq!(app.ml_import_path.as_deref(), payload.to_str());
    assert!(payload.exists(), "only the hand-over file is consumed");
    // The file is not a profile: the refusal is explained and nothing is offered.
    assert!(matches!(&app.ui.dialog, Some(Dialog::Message { title, .. }) if title == "Import Profile"));
    assert!(app.ui.profiles.pending_import.is_none());
    std::fs::remove_file(payload).unwrap();
    std::fs::remove_dir(documents).unwrap();
    drop(app.take_threads());
}

/// While a game runs with a vehicle that has no saved profile, the profile area of every theme says the game's
/// profile is used: from a profile load that is not the vehicle's, until the profile is saved for the vehicle.
#[test]
fn the_profile_area_says_when_the_game_profile_is_used_for_the_vehicle() {
    use crate::ui::session::{Session, State};
    use crate::ui::{Region, Theme};
    const NOTE: &str = "This car has no saved profile; the game profile is used";
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    drop(app.take_threads());
    navigate(&mut app, Screen::Home, 0);
    let session = |state: State, vehicle: &str| Session {
        state,
        game_key: "ac".into(),
        game: "Assetto Corsa".into(),
        vehicle: vehicle.into(),
        racing: true,
        ..Session::default()
    };
    // A profile load of the game's profile.
    backend.docs.lock().unwrap()["status"]["profile"]["vehicle_level"] = json!(false);
    backend.docs.lock().unwrap()["status"]["profile"]["load_generation"] = json!(41);
    app.main_loop_pass();
    assert!(!app.ml.vehicle_level);
    app.ui.session = session(State::Telemetry, "Ferrari 488");
    for theme in Theme::ALL {
        app.ui.theme = theme;
        let html = crate::ui::render(&app, Region::Head);
        assert!(
            html.contains(&format!("<small id=\"profile-level\" class=\"profile-level\">{NOTE}</small>")),
            "{theme:?}"
        );
        assert!(html.contains("<select id=\"profile\" data-act=\"profile\" aria-describedby=\"profile-level\">"));
    }
    // Not in stand-by, nor without a vehicle.
    app.ui.session = session(State::Standby, "");
    assert!(!crate::ui::render(&app, Region::Head).contains(NOTE));
    app.ui.session = session(State::GameRunning, "");
    assert!(!crate::ui::render(&app, Region::Head).contains(NOTE));
    // Saved for the vehicle: its own profile is used.
    app.ui.session = session(State::Telemetry, "Ferrari 488");
    save_at(&mut app, Some("Ferrari 488")).unwrap();
    assert!(app.ml.vehicle_level);
    assert!(!crate::ui::render(&app, Region::Head).contains(NOTE));
    // A load of the game's profile again (another vehicle), then of a vehicle's own.
    backend.docs.lock().unwrap()["status"]["profile"]["load_generation"] = json!(42);
    app.main_loop_pass();
    assert!(!app.ml.vehicle_level);
    backend.docs.lock().unwrap()["status"]["profile"]["vehicle_level"] = json!(true);
    backend.docs.lock().unwrap()["status"]["profile"]["load_generation"] = json!(43);
    app.main_loop_pass();
    assert!(app.ml.vehicle_level);
    // The note is translated in every language.
    for code in ["de", "en", "es", "fr", "it", "ja", "pt", "zh"] {
        let path = format!("{}/../openrigcue-daemon/assets/locales/{code}.json", env!("CARGO_MANIFEST_DIR"));
        let catalog: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let text = catalog[NOTE].as_str().unwrap_or_else(|| panic!("{code}"));
        assert!(code == "en" || text != NOTE, "{code}");
    }
    drop(app.take_threads());
}

#[test]
fn the_filter_shows_only_the_matching_vehicles_of_a_matching_game() {
    let mut app = loaded(FakeBackend::new());
    navigate(&mut app, Screen::Profiles, 0);
    let entry = |game: &str, name: &str, vehicles: &[&str]| Entry {
        game: game.into(),
        name: name.into(),
        vehicles: vehicles.iter().map(|v| (*v).to_owned()).collect(),
    };
    app.ui.profiles.entries = vec![
        entry("ams2", "Automobilista 2", &["Formula Ultimate", "McLaren 720S"]),
        entry("acc", "Assetto Corsa Competizione", &["Ferrari 488", "Porsche 911"]),
        entry("ir", "iRacing", &["Dallara"]),
    ];
    let shown = |app: &mut MyApp, filter: &str| {
        dispatch(app, "profiles-filter", "", filter).unwrap();
        filtered(app)
            .iter()
            .map(|(e, v)| (e.game.clone(), v.iter().map(|v| v.to_string()).collect()))
            .collect::<Vec<(String, Vec<String>)>>()
    };
    let owned = |list: &[(&str, &[&str])]| -> Vec<(String, Vec<String>)> {
        list.iter().map(|(g, v)| ((*g).to_owned(), v.iter().map(|v| (*v).to_owned()).collect())).collect()
    };
    // A matching game shows only its matching vehicles; a game kept for a vehicle shows that vehicle.
    assert_eq!(
        shown(&mut app, "a"),
        owned(&[("ams2", &["Formula Ultimate", "McLaren 720S"]), ("acc", &["Ferrari 488"]), ("ir", &["Dallara"])])
    );
    assert_eq!(shown(&mut app, "assetto"), owned(&[("acc", &[])]));
    assert_eq!(shown(&mut app, "PORSCHE"), owned(&[("acc", &["Porsche 911"])]));
    // Spaces are part of the filter; spaces alone show everything.
    assert_eq!(shown(&mut app, "a 2"), owned(&[("ams2", &[])]));
    assert_eq!(
        shown(&mut app, "  "),
        owned(&[
            ("ams2", &["Formula Ultimate", "McLaren 720S"]),
            ("acc", &["Ferrari 488", "Porsche 911"]),
            ("ir", &["Dallara"])
        ])
    );
    dispatch(&mut app, "profiles-filter", "", "assetto").unwrap();
    let html = crate::ui::screens::main(&app);
    assert!(
        html.contains("Assetto Corsa Competizione") && !html.contains("Ferrari 488") && !html.contains("Porsche 911")
    );
    drop(app.take_threads());
}
