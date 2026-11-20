//! Tests of Tuning → Routines: the rows, the Options work and its configuration, the Wayland warning, the
//! start of the enabled routines, and the controllers driving the other programs' windows.

use std::sync::{Arc, Mutex};

use openrigcue_model::automation::{Element, Request, Response, Selector};
use serde_json::json;

use super::desktop::fake::FakeDesktop;
use super::desktop::{Provider, UiaWindow};
use super::*;
use crate::app::tests::{loaded, FakeBackend};
use crate::host::UiApp;
use crate::ui::{dispatch, navigate, Region, Screen};

fn routines(backend: &Arc<FakeBackend>) -> MyApp {
    backend.docs.lock().unwrap()["status"]["session"]["game"] = json!("acc");
    let mut app = loaded(backend.clone());
    navigate(&mut app, Screen::Routines, 0);
    drop(app.take_threads());
    app
}

#[test]
fn rows_show_each_routine_status_and_the_disclaimer() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["tune"]["routines"] = json!({"Fanalab": {"enabled": true, "profile": "3"}});
    let app = routines(&backend);
    let html = main(&app);
    for (name, ..) in ROUTINES {
        assert!(html.contains(&format!("data-arg=\"Routines:{name}:\"")), "{name}");
    }
    assert!(html.contains(">Enabled<") && html.contains(">Disabled<"));
    assert!(html.contains("Routines are an experimental part of OpenRigCue"));
}

#[test]
fn options_read_the_profiles_on_a_thread_and_the_configuration_is_stored() {
    let backend = FakeBackend::new();
    let mut app = routines(&backend);
    dispatch(&mut app, "action", "Routines:Fanalab:", "").unwrap();
    assert!(main(&app).contains("Loading…"));
    assert_eq!(app.take_threads().len(), 1);
    let shared = Arc::new(Mutex::new(app));
    prepare_options(&Arc::downgrade(&shared), &FakeDesktop::default(), "Fanalab", "OpenRigCue");
    let mut app = crate::host::lock(&shared);
    assert!(app.ui.tuning.routine_busy.is_none());
    let dialog = crate::ui::render(&app, Region::Dialog);
    assert!(dialog.contains("Routine Configuration") && dialog.contains(">Auto Setup<"));
    dispatch(&mut app, "dialog-field", "enabled", "1").unwrap();
    dispatch(&mut app, "dialog-field", "profile", "2").unwrap();
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert_eq!(backend.docs.lock().unwrap()["tune"]["routines"]["Fanalab"], json!({"enabled": true, "profile": "2"}));
    assert!(main(&app).contains(">Enabled<"));
}

#[test]
fn a_wayland_session_warns_instead_of_opening_the_options() {
    let backend = FakeBackend::new();
    let app = routines(&backend);
    let shared = Arc::new(Mutex::new(app));
    let desktop = FakeDesktop { env: vec![("XDG_SESSION_TYPE".into(), "wayland".into())], ..Default::default() };
    prepare_options(&Arc::downgrade(&shared), &desktop, "CrewChief", "OpenRigCue");
    let app = crate::host::lock(&shared);
    assert!(app.ui.dialog.is_none());
    assert!(app.ui.notices.history.last().unwrap().message.contains("X11 session"));
}

#[test]
fn a_loaded_profile_starts_the_enabled_routines() {
    let backend = FakeBackend::new();
    let mut app = routines(&backend);
    on_profile_load(&mut app);
    let threads = app.take_threads();
    assert_eq!(threads.iter().map(|t| t.name).collect::<Vec<_>>(), ["launch_routine"]);
    backend.docs.lock().unwrap()["tune"]["routines"] = json!({"CrewChief": {"enabled": true, "profile": "AC"}});
    let shared = Arc::new(Mutex::new(app));
    let desktop = FakeDesktop::with_titles(&["OpenRigCue"]);
    launch_routine(&Arc::downgrade(&shared), &desktop, "OpenRigCue");
    let app = crate::host::lock(&shared);
    let last = app.ui.notices.history.last().expect("the failed routine is recorded");
    assert!(last.title.contains("CrewChief"), "{last:?}");
    assert!(desktop.calls().iter().any(|call| call == "activate OpenRigCue"), "{:?}", desktop.calls());
}

#[test]
fn the_routine_texts_have_entries() {
    let backend = FakeBackend::new();
    let mut app = routines(&backend);
    backend.translations.lock().unwrap().clear();
    main(&app);
    for (name, _, description) in ROUTINES {
        let config = Config {
            routine: name.into(),
            description: description.into(),
            enabled: false,
            profile: None,
            options: vec!["1".into()],
        };
        tuning::open_dialog(&mut app, Dialog::Routine(config));
        crate::ui::render(&app, Region::Dialog);
    }
    let looked_up = backend.translations.lock().unwrap().iter().cloned().collect();
    assert_eq!(crate::app::tests::texts_without_entry(&looked_up, &[]), Vec::<String>::new());
}

/// The routine configuration dialog, when one is open.
fn config(app: &MyApp) -> Option<Config> {
    match tuning::open_one(app) {
        Some(Dialog::Routine(config)) => Some(config.clone()),
        _ => None,
    }
}

fn fanalab_config() -> Config {
    Config { routine: "Fanalab".into(), description: String::new(), enabled: true, profile: None, options: Vec::new() }
}

struct WorkerPause {
    entered: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

/// The fake service that records, for each routine read, whether the application was free meanwhile.
struct WorkerReadBackend {
    inner: Arc<FakeBackend>,
    app: Weak<Mutex<MyApp>>,
    observed: Mutex<Vec<(String, bool)>>,
    pause: Mutex<Option<WorkerPause>>,
}

impl WorkerReadBackend {
    fn observe(&self, name: &str) {
        let shared = self.app.upgrade().unwrap();
        self.observed.lock().unwrap().push((name.into(), shared.try_lock().is_ok()));
        if let Some(pause) = self.pause.lock().unwrap().take() {
            pause.entered.send(()).unwrap();
            pause.release.recv_timeout(std::time::Duration::from_secs(3)).unwrap();
        }
    }
}

impl crate::backend::Backend for WorkerReadBackend {
    fn call(
        &self,
        method: crate::backend::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, crate::backend::Failure> {
        self.inner.call(method, path, body)
    }
    fn upload(&self, path: &str, bytes: Vec<u8>) -> Result<serde_json::Value, crate::backend::Failure> {
        self.inner.upload(path, bytes)
    }
    fn fetch(&self, path: &str) -> Result<(Vec<u8>, String), crate::backend::Failure> {
        self.inner.fetch(path)
    }
    fn translate(&self, text: &str) -> String {
        self.inner.translate(text)
    }
    fn excluded_strings(&self) -> Vec<String> {
        self.inner.excluded_strings()
    }
    fn native_tune(&self) -> Result<LegacyValue, crate::backend::Failure> {
        self.observe("tune");
        self.inner.native_tune()
    }
    fn game_value(&self) -> Result<LegacyValue, crate::backend::Failure> {
        self.observe("game");
        self.inner.game_value()
    }
}

#[test]
fn the_routine_work_releases_the_application_during_service_reads() {
    let inner = FakeBackend::new();
    inner.docs.lock().unwrap()["tune"]["routines"] = json!({"Fanalab": {"enabled": false, "profile": "3"}});
    let app = loaded(inner.clone());
    let shared = Arc::new(Mutex::new(app));
    let backend = Arc::new(WorkerReadBackend {
        inner,
        app: Arc::downgrade(&shared),
        observed: Mutex::new(Vec::new()),
        pause: Mutex::new(None),
    });
    lock(&shared).backend = backend.clone();
    let desktop = FakeDesktop::with_titles(&["OpenRigCue"]);
    prepare_options(&Arc::downgrade(&shared), &desktop, "Fanalab", "OpenRigCue");
    launch_routine(&Arc::downgrade(&shared), &desktop, "OpenRigCue");
    let observed = backend.observed.lock().unwrap();
    assert_eq!(
        observed.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(),
        ["tune", "tune", "tune", "tune", "tune", "game"]
    );
    for (name, available) in observed.iter() {
        assert!(available, "the routine work held the application while reading {name}");
    }
    assert_eq!(config(&lock(&shared)).unwrap().profile.as_deref(), Some("3"));
    assert_eq!(desktop.calls(), ["activate OpenRigCue", "activate OpenRigCue"]);
}

#[test]
fn a_timer_runs_while_the_routine_work_waits_for_the_service() {
    use std::sync::mpsc;
    use std::time::Duration;
    let inner = FakeBackend::new();
    let app = loaded(inner.clone());
    let shared = Arc::new(Mutex::new(app));
    let (entered, waiting) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let backend = Arc::new(WorkerReadBackend {
        inner,
        app: Arc::downgrade(&shared),
        observed: Mutex::new(Vec::new()),
        pause: Mutex::new(Some(WorkerPause { entered, release: released })),
    });
    {
        let mut app = lock(&shared);
        app.backend = backend;
        app.attach_shared(Arc::downgrade(&shared));
    }
    let worker_app = Arc::downgrade(&shared);
    let worker = std::thread::spawn(move || {
        prepare_options(&worker_app, &FakeDesktop::with_titles(&["OpenRigCue"]), "Fanalab", "OpenRigCue");
    });
    waiting.recv_timeout(Duration::from_secs(2)).unwrap();
    let (finished, callback) = mpsc::channel();
    let available = if let Ok(mut app) = shared.try_lock() {
        app.timer(Duration::ZERO, move |_| finished.send(()).unwrap());
        true
    } else {
        false
    };
    let ran = available && callback.recv_timeout(Duration::from_secs(2)).is_ok();
    release.send(()).unwrap();
    worker.join().unwrap();
    assert!(ran, "a timer must run while a routine service read waits");
    assert!(config(&lock(&shared)).is_some());
}

#[test]
fn the_routine_failure_texts_have_entries() {
    let backend = FakeBackend::new();
    let mut app = routines(&backend);
    backend.translations.lock().unwrap().clear();
    for (name, _, description) in ROUTINES {
        let config = Config {
            routine: name.into(),
            description: description.into(),
            enabled: true,
            profile: None,
            options: vec![],
        };
        tuning::open_dialog(&mut app, Dialog::Routine(config));
        crate::ui::render(&app, Region::Dialog);
    }
    let mut looked_up: std::collections::BTreeSet<String> =
        backend.translations.lock().unwrap().iter().cloned().collect();
    assert!(looked_up.contains("Routine Configuration"));
    for program in [&super::crewchief::PROGRAM, &super::fanatec::PROGRAM, &super::simecube::PROGRAM] {
        looked_up.extend([program.cannot_find, program.wait_too_long, program.exists_too_long].map(str::to_owned));
    }
    looked_up.extend(
        [
            "Setting the game failed",
            "Setting the profile failed",
            "Setting the Profile failed",
            NEEDS_X11_SESSION,
            ROUTINES_NEED_X11_SESSION,
            "Routine not started: ^routine^",
        ]
        .map(str::to_owned),
    );
    assert_eq!(crate::app::tests::texts_without_entry(&looked_up, &[]), Vec::<String>::new());
}

#[test]
fn the_routines_are_listed_in_order_and_disabled_without_a_routines_table() {
    let backend = FakeBackend::new();
    let app = routines(&backend);
    assert!(app.ml.fatal_error.is_none(), "{:?}", app.ml.fatal_error);
    let html = main(&app);
    let at = |name: &str| html.find(&format!("data-arg=\"Routines:{name}:\"")).unwrap();
    assert!(
        at("Fanalab") < at("Simucube 2 True Drive Paddock") && at("Simucube 2 True Drive Paddock") < at("CrewChief")
    );
    assert_eq!(html.matches(">Disabled<").count(), 3);
    assert!(!html.contains(">Enabled<"));
    assert!(!html.contains("Needs a Premium subscription"), "there is no subscription");
}

#[test]
fn the_status_follows_the_service_once_the_documents_are_read_again() {
    let backend = FakeBackend::new();
    let mut app = routines(&backend);
    backend.docs.lock().unwrap()["tune"]["routines"] = json!({"Fanalab": {"enabled": true, "profile": "Auto Setup"}});
    app.refresh_docs();
    assert!(main(&app).contains(">Enabled<"));
    drop(app.take_threads());
}

#[test]
fn the_status_shown_is_the_one_that_decides_whether_a_routine_starts() {
    // Any stored value that is true starts the routine, so it shows Enabled; an empty one shows Disabled.
    for (stored, shown) in
        [(json!("no"), ">Enabled<"), (json!(2), ">Enabled<"), (json!(""), ">Disabled<"), (json!(0), ">Disabled<")]
    {
        let backend = FakeBackend::new();
        backend.docs.lock().unwrap()["tune"]["routines"] =
            json!({"Fanalab": {"enabled": stored.clone(), "profile": "3"}});
        let mut app = routines(&backend);
        assert!(main(&app).contains(shown), "{stored}");
        assert_eq!(enabled(&app, "Fanalab"), shown == ">Enabled<", "{stored}");
        drop(app.take_threads());
    }
}

#[test]
fn the_stored_status_and_profile_open_with_the_options_and_the_confirmation_stores_them() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["tune"]["routines"] = json!({"Fanalab": {"enabled": false, "profile": "3"}});
    let app = routines(&backend);
    let shared = Arc::new(Mutex::new(app));
    let desktop = FakeDesktop::with_titles(&["OpenRigCue"]);
    prepare_options(&Arc::downgrade(&shared), &desktop, "Fanalab", "OpenRigCue");
    assert_eq!(desktop.calls(), ["activate OpenRigCue"]);
    let mut app = lock(&shared);
    let open = config(&app).unwrap();
    assert_eq!(open.options, ["Auto Setup", "1", "2", "3", "4", "5"]);
    assert_eq!((open.enabled, open.profile.as_deref()), (false, Some("3")));
    let dialog = crate::ui::render(&app, Region::Dialog);
    assert!(dialog.contains("<option value=\"3\" selected>3</option>"), "{dialog}");
    assert!(!dialog.contains("data-arg=\"enabled\" checked"));
    dispatch(&mut app, "dialog-field", "enabled", "1").unwrap();
    dispatch(&mut app, "dialog-field", "profile", "0").unwrap();
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert_eq!(app.tune(&["routines", "Fanalab"]), &json!({"enabled": true, "profile": "Auto Setup"}));
    assert_eq!(
        backend.patches("profile").last(),
        Some(&json!({"routines": {"Fanalab": {"enabled": true, "profile": "Auto Setup"}}}))
    );
    assert!(main(&app).contains(">Enabled<"));
}

#[test]
fn a_first_confirmation_creates_the_routines_table() {
    let backend = FakeBackend::new();
    let app = routines(&backend);
    let shared = Arc::new(Mutex::new(app));
    prepare_options(&Arc::downgrade(&shared), &FakeDesktop::with_titles(&["OpenRigCue"]), "Fanalab", "OpenRigCue");
    let mut app = lock(&shared);
    assert_eq!(config(&app).unwrap().profile.as_deref(), Some("Auto Setup"), "the first profile without a stored one");
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    let patches = backend.patches("profile");
    assert_eq!(patches[patches.len() - 2], json!({"routines": {}}));
    assert_eq!(
        patches[patches.len() - 1],
        json!({"routines": {"Fanalab": {"enabled": false, "profile": "Auto Setup"}}})
    );
}

#[test]
fn a_routines_value_that_is_no_table_stores_nothing() {
    for parent in [serde_json::Value::Null, json!(17), json!([]), json!("bad parent")] {
        let backend = FakeBackend::new();
        let mut app = routines(&backend);
        tuning::open_dialog(&mut app, Dialog::Routine(fanalab_config()));
        backend.docs.lock().unwrap()["tune"]["routines"] = parent.clone();
        let before = backend.patches("profile").len();
        let error = dispatch(&mut app, "dialog-ok", "", "").unwrap_err();
        assert!(error.contains("TypeError:"), "{parent}: {error}");
        assert_eq!(backend.patches("profile").len(), before, "{parent}");
        assert_eq!(backend.docs.lock().unwrap()["tune"]["routines"], parent);
        assert!(app.ui.dialog.is_none());
        drop(app.take_threads());
    }
    // A table read when the options opened, but none left when they are confirmed.
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["tune"]["routines"] = json!({"Fanalab": {"enabled": true, "profile": "Auto Setup"}});
    let app = routines(&backend);
    let shared = Arc::new(Mutex::new(app));
    prepare_options(&Arc::downgrade(&shared), &FakeDesktop::with_titles(&["OpenRigCue"]), "Fanalab", "OpenRigCue");
    let mut app = lock(&shared);
    let before = backend.patches("profile").len();
    backend.docs.lock().unwrap()["tune"]["routines"] = serde_json::Value::Null;
    assert!(dispatch(&mut app, "dialog-ok", "", "").is_err());
    assert_eq!(backend.patches("profile").len(), before);
    assert!(backend.docs.lock().unwrap()["tune"]["routines"].is_null());
}

#[test]
fn a_failed_routine_write_keeps_the_created_table() {
    use crate::backend::{Backend, Failure, Method};
    struct FailRoutineAssignment(Arc<FakeBackend>);
    impl Backend for FailRoutineAssignment {
        fn call(
            &self,
            method: Method,
            path: &str,
            body: Option<serde_json::Value>,
        ) -> Result<serde_json::Value, Failure> {
            if method == Method::Patch
                && path == "profile"
                && body.as_ref().is_some_and(|value| value["routines"].get("Fanalab").is_some())
            {
                return Err(Failure { status: 500, detail: "RuntimeError: fixture routine assignment failed".into() });
            }
            self.0.call(method, path, body)
        }
        fn upload(&self, path: &str, bytes: Vec<u8>) -> Result<serde_json::Value, Failure> {
            self.0.upload(path, bytes)
        }
        fn fetch(&self, path: &str) -> Result<(Vec<u8>, String), Failure> {
            self.0.fetch(path)
        }
        fn translate(&self, text: &str) -> String {
            self.0.translate(text)
        }
        fn excluded_strings(&self) -> Vec<String> {
            self.0.excluded_strings()
        }
    }
    let backend = FakeBackend::new();
    let mut app = routines(&backend);
    tuning::open_dialog(&mut app, Dialog::Routine(fanalab_config()));
    let before = backend.patches("profile").len();
    app.backend = Arc::new(FailRoutineAssignment(backend.clone()));
    let error = dispatch(&mut app, "dialog-ok", "", "").unwrap_err();
    assert!(error.contains("RuntimeError: fixture routine assignment failed"), "{error}");
    assert_eq!(backend.docs.lock().unwrap()["tune"]["routines"], json!({}));
    assert_eq!(app.tune(&["routines"]), &json!({}));
    assert_eq!(&backend.patches("profile")[before..], &[json!({"routines": {}})]);
    assert!(app.ui.dialog.is_none());
    drop(app.take_threads());
}

#[test]
fn a_stored_text_status_counts_as_on_when_the_options_open() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["tune"]["routines"] = json!({"Fanalab": {"enabled": "no", "profile": "1"}});
    let app = routines(&backend);
    let shared = Arc::new(Mutex::new(app));
    prepare_options(&Arc::downgrade(&shared), &FakeDesktop::with_titles(&["OpenRigCue"]), "Fanalab", "OpenRigCue");
    let app = lock(&shared);
    let open = config(&app).unwrap();
    assert!(open.enabled);
    assert_eq!(open.profile.as_deref(), Some("1"));
}

/// The app on the Routines screen with `routine`'s Options pressed (its work not run yet).
fn options_pressed(backend: &Arc<FakeBackend>, routine: &str) -> Arc<Mutex<MyApp>> {
    let mut app = routines(backend);
    dispatch(&mut app, "action", &format!("Routines:{routine}:"), "").unwrap();
    assert_eq!(app.ui.tuning.routine_busy.as_deref(), Some(routine));
    drop(app.take_threads());
    Arc::new(Mutex::new(app))
}

#[test]
fn a_controller_failure_is_a_warning_and_opens_nothing() {
    let backend = FakeBackend::new();
    let shared = options_pressed(&backend, "CrewChief");
    prepare_options(&Arc::downgrade(&shared), &FakeDesktop::with_titles(&["OpenRigCue"]), "CrewChief", "OpenRigCue");
    let app = lock(&shared);
    assert_eq!(crate::app::tests::alert_text(&app).as_deref(), Some("OpenRigCue could not locate CrewChiefV4.exe"));
    assert!(app.ui.dialog.is_none());
    assert!(app.ui.tuning.routine_busy.is_none());
    assert!(!main(&app).contains("Loading…"));
}

#[test]
fn without_an_app_window_the_options_still_open() {
    let backend = FakeBackend::new();
    let shared = options_pressed(&backend, "Fanalab");
    let desktop = FakeDesktop::default();
    prepare_options(&Arc::downgrade(&shared), &desktop, "Fanalab", "OpenRigCue");
    assert_eq!(desktop.calls(), ["activate OpenRigCue"]);
    let app = lock(&shared);
    assert!(config(&app).is_some());
    assert!(app.ui.tuning.routine_busy.is_none());
}

#[test]
fn a_wayland_session_without_the_app_window_warns_and_frees_the_options() {
    for (name, value) in [("WAYLAND_DISPLAY", "wayland-0"), ("XDG_SESSION_TYPE", "wayland")] {
        let backend = FakeBackend::new();
        let shared = options_pressed(&backend, "Fanalab");
        let desktop = FakeDesktop { env: vec![(name.into(), value.into())], ..Default::default() };
        prepare_options(&Arc::downgrade(&shared), &desktop, "Fanalab", "OpenRigCue");
        assert!(desktop.calls().is_empty(), "{name}: {:?}", desktop.calls());
        let app = lock(&shared);
        assert_eq!(crate::app::tests::alert_text(&app).as_deref(), Some(NEEDS_X11_SESSION), "{name}");
        assert!(app.ui.dialog.is_none());
        assert!(app.ui.tuning.routine_busy.is_none());
    }
    // With the app window reachable, a Wayland session changes nothing.
    let backend = FakeBackend::new();
    let shared = options_pressed(&backend, "Fanalab");
    let desktop = FakeDesktop {
        titles: Mutex::new(vec!["OpenRigCue".into()]),
        env: vec![("WAYLAND_DISPLAY".into(), "wayland-0".into())],
        ..Default::default()
    };
    prepare_options(&Arc::downgrade(&shared), &desktop, "Fanalab", "OpenRigCue");
    assert_eq!(desktop.calls(), ["activate OpenRigCue"]);
    let app = lock(&shared);
    assert!(config(&app).is_some());
    assert!(crate::app::tests::alert_text(&app).is_none());
}

#[test]
fn starting_routines_in_a_wayland_session_warns_once_and_starts_nothing() {
    let wayland = || FakeDesktop { env: vec![("WAYLAND_DISPLAY".into(), "wayland-0".into())], ..Default::default() };
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["tune"]["routines"] = json!({"Fanalab": {"enabled": true, "profile": "1"}});
    let shared = Arc::new(Mutex::new(loaded(backend)));
    let desktop = wayland();
    launch_routine(&Arc::downgrade(&shared), &desktop, "OpenRigCue");
    assert!(desktop.calls().is_empty());
    assert_eq!(crate::app::tests::alert_text(&lock(&shared)).as_deref(), Some(ROUTINES_NEED_X11_SESSION));
    // With no routine enabled nothing is said.
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["tune"]["routines"] = json!({"Fanalab": {"enabled": false}});
    let shared = Arc::new(Mutex::new(loaded(backend)));
    launch_routine(&Arc::downgrade(&shared), &wayland(), "OpenRigCue");
    assert!(crate::app::tests::alert_text(&lock(&shared)).is_none());
}

#[test]
fn an_enabled_routine_without_a_profile_ends_the_start() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["tune"]["routines"] =
        json!({"Fanalab": {"enabled": true}, "CrewChief": {"enabled": true, "profile": null}});
    let shared = Arc::new(Mutex::new(loaded(backend)));
    let desktop = FakeDesktop::default();
    launch_routine(&Arc::downgrade(&shared), &desktop, "OpenRigCue");
    assert!(desktop.calls().is_empty(), "no routine and no window: {:?}", desktop.calls());
}

#[test]
fn the_window_of_the_current_game_comes_first() {
    // The game's window is brought to the front instead of the app's.
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["status"]["session"]["game"] = json!("ac");
    let shared = Arc::new(Mutex::new(loaded(backend.clone())));
    let catalog = openrigcue_config::catalog::Catalog::builtin();
    let ac = catalog.game("ac").unwrap().window_name.clone().unwrap();
    let mut desktop = FakeDesktop::with_titles(&[&ac, "OpenRigCue"]);
    desktop.activation_succeeds = true;
    launch_routine(&Arc::downgrade(&shared), &desktop, "OpenRigCue");
    assert_eq!(desktop.calls(), [format!("activate {ac}")]);
    // The game is read when the routines start, not when the app was loaded.
    let current = catalog
        .games()
        .iter()
        .find(|game| game.window_name.as_ref().is_some_and(|window| !window.is_empty() && *window != ac))
        .unwrap();
    let window = current.window_name.clone().unwrap();
    backend.docs.lock().unwrap()["status"]["session"]["game"] = json!(current.id);
    let mut desktop = FakeDesktop::with_titles(&[&window, &ac, "OpenRigCue"]);
    desktop.activation_succeeds = true;
    launch_routine(&Arc::downgrade(&shared), &desktop, "OpenRigCue");
    assert_eq!(desktop.calls(), [format!("activate {window}")]);
    drop(lock(&shared).take_threads());
}

#[test]
fn the_enabled_routines_start_before_the_app_window() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["tune"]["routines"] =
        json!({"Fanalab": {"enabled": true, "profile": "1"}, "CrewChief": {"enabled": false}});
    let shared = Arc::new(Mutex::new(loaded(backend)));
    let desktop = FakeDesktop::default();
    launch_routine(&Arc::downgrade(&shared), &desktop, "OpenRigCue");
    assert_eq!(
        desktop.calls(),
        ["popen %ProgramFiles(x86)%\\Fanatec\\FanaLab\\Control\\FanaLab.exe", "activate OpenRigCue"]
    );
    // A game missing from the service: the app window after the enabled routines.
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["status"]["session"]["game"] = json!("ac");
    let app = loaded(backend.clone());
    backend.docs.lock().unwrap()["status"]["session"].as_object_mut().unwrap().remove("game");
    backend.docs.lock().unwrap()["tune"]["routines"] = json!({"Fanalab": {"enabled": true, "profile": "1"}});
    let shared = Arc::new(Mutex::new(app));
    let ac = openrigcue_config::catalog::Catalog::builtin().game("ac").unwrap().window_name.clone().unwrap();
    let mut desktop = FakeDesktop::with_titles(&[&ac, "OpenRigCue"]);
    desktop.activation_succeeds = true;
    launch_routine(&Arc::downgrade(&shared), &desktop, "OpenRigCue");
    assert_eq!(
        desktop.calls(),
        ["popen %ProgramFiles(x86)%\\Fanatec\\FanaLab\\Control\\FanaLab.exe", "activate OpenRigCue"]
    );
    drop(lock(&shared).take_threads());
}

// ---- The controllers driving the other programs' windows ------------------------------------------------

/// A scripted window tree of the other programs: records every request, and can miss or fail a control.
#[derive(Default)]
struct Tree {
    calls: Mutex<Vec<Request>>,
    missing: Mutex<Option<String>>,
    fail_click: Mutex<Option<String>>,
    fail_operation: Mutex<Option<String>>,
    app_activations: Mutex<Vec<String>>,
}

fn element_label(element: &Element) -> String {
    element
        .path
        .iter()
        .map(|part| match part {
            Selector::Name { value, .. } | Selector::AutomationId { value, .. } => value.clone(),
            Selector::Child { index } => format!("#{index}"),
        })
        .collect::<Vec<_>>()
        .join("/")
}

impl Provider for Tree {
    fn request(&self, request: Request) -> Result<Response, String> {
        self.calls.lock().unwrap().push(request.clone());
        let operation = match &request {
            Request::Children { element } => Some(format!("children:{}", element_label(element))),
            Request::Activate { element } => Some(format!("activate:{}", element_label(element))),
            _ => None,
        };
        if operation.is_some() && *self.fail_operation.lock().unwrap() == operation {
            return Err("fixture native exception".into());
        }
        let mut response = Response { found: true, ..Default::default() };
        match &request {
            Request::Exists { element } => {
                response.found = self.missing.lock().unwrap().as_deref() != Some(element_label(element).as_str())
            }
            Request::Click { element }
                if self.fail_click.lock().unwrap().as_deref() == Some(element_label(element).as_str()) =>
            {
                return Err("fixture click failure".into())
            }
            Request::Children { element } => {
                response.values = if element_label(element) == "gameDefinitionList" {
                    vec!["Vertical".into(), "AC".into()]
                } else {
                    vec!["one".into(), "two".into(), "three".into()]
                }
            }
            Request::Name { element } => {
                if self.missing.lock().unwrap().as_deref() == Some(element_label(element).as_str()) {
                    return Err("fixture missing control".into());
                }
                response.values = vec![match element_label(element).as_str() {
                    "startApplicationButton" => "Stop",
                    "gameDefinitionList/#0" => "Vertical",
                    "gameDefinitionList/#1" => "AC",
                    _ => "Selected",
                }
                .into()]
            }
            _ => {}
        }
        Ok(response)
    }
}

/// A desktop with one program window already open, driven through `tree`.
struct Windows {
    tree: Arc<Tree>,
    title: &'static str,
}

impl Desktop for Windows {
    fn all_titles(&self) -> Vec<String> {
        vec![self.title.into()]
    }
    fn popen(&self, _: &str) -> std::io::Result<()> {
        panic!("an open window must not be launched again")
    }
    fn env(&self, _: &str) -> Option<String> {
        None
    }
    fn sleep(&self, _: std::time::Duration) {
        panic!("an open window must not be waited for")
    }
    fn window_control_exists(&self, name: Option<&str>, _: f64, _: f64) -> Option<UiaWindow> {
        Some(UiaWindow::root(self.tree.clone(), name.unwrap().into()))
    }
    fn activate(&self, title: &str) -> Result<(), ActivateError> {
        self.tree.app_activations.lock().unwrap().push(title.into());
        Ok(())
    }
}

fn tree_calls(tree: &Tree) -> Vec<String> {
    tree.calls
        .lock()
        .unwrap()
        .iter()
        .map(|request| match request {
            Request::Activate { element } => format!("activate:{}", element_label(element)),
            Request::Focus { element } => format!("focus:{}", element_label(element)),
            Request::Topmost { element } => format!("topmost:{}", element_label(element)),
            Request::Click { element } => format!("click:{}", element_label(element)),
            Request::Exists { element } => format!("exists:{}", element_label(element)),
            Request::Name { element } => format!("name:{}", element_label(element)),
            Request::Children { element } => format!("children:{}", element_label(element)),
            _ => format!("{request:?}"),
        })
        .collect()
}

#[test]
fn fanalab_clicks_its_controls_in_order_and_passes_over_missing_ones() {
    let tree = Arc::new(Tree::default());
    *tree.missing.lock().unwrap() = Some("Advanced".into());
    let windows = Windows { tree: tree.clone(), title: "FanaLab V1.69" };
    assert_eq!(super::fanatec::activate_profile(&windows, Some("3")).unwrap(), "OK");
    assert_eq!(
        tree_calls(&tree),
        ["activate:", "focus:", "topmost:", "click:Tuning Menu", "name:Advanced", "click:ToggleButtonMode", "click:3"]
    );
    tree.calls.lock().unwrap().clear();
    *tree.missing.lock().unwrap() = Some("Wheel device detected".into());
    assert_eq!(super::fanatec::activate_profile(&windows, Some("3")).unwrap(), "OK");
    assert!(!tree_calls(&tree).iter().any(|call| call.contains("Wheel device detected")));
}

#[test]
fn crewchief_lists_its_games_then_stops_selects_and_starts_again() {
    let tree = Arc::new(Tree::default());
    let windows = Windows { tree: tree.clone(), title: "Crew Chief V4 - 4.18" };
    assert_eq!(super::crewchief::get_list_profiles(&windows).unwrap(), ["AC"]);
    tree.calls.lock().unwrap().clear();
    assert_eq!(super::crewchief::activate_profile(&windows, Some("AC")).unwrap(), "OK");
    let calls = tree_calls(&tree);
    let actions: Vec<_> = calls
        .iter()
        .filter(|call| call.starts_with("click:") || call.starts_with("focus:game"))
        .map(String::as_str)
        .collect();
    assert_eq!(
        actions,
        [
            "click:startApplicationButton",
            "focus:gameDefinitionList/#1",
            "click:gameDefinitionList/#1",
            "click:startApplicationButton"
        ]
    );
}

#[test]
fn simucube_opens_its_third_tab_and_its_listing_fails() {
    let tree = Arc::new(Tree::default());
    let windows = Windows { tree: tree.clone(), title: "Simucube 2 True Drive 2025" };
    assert!(super::simecube::get_list_profiles(&windows).unwrap_err().contains("cannot unpack"));
    assert!(tree_calls(&tree).contains(&"click:MW2.centralWidgetWindow.tabWidget.qt_tabwidget_tabbar/#2".into()));
    assert!(tree_calls(&tree).contains(&"click:Go to offline (fallback) mode".into()));
}

#[test]
fn a_failed_simucube_listing_warns_before_the_app_window_is_brought_up() {
    let backend = FakeBackend::new();
    let shared = options_pressed(&backend, "Simucube 2 True Drive Paddock");
    let tree = Arc::new(Tree::default());
    let windows = Windows { tree: tree.clone(), title: "Simucube 2 True Drive 2025" };
    prepare_options(&Arc::downgrade(&shared), &windows, "Simucube 2 True Drive Paddock", "OpenRigCue");
    assert!(tree.app_activations.lock().unwrap().is_empty(), "the listing fails before the app window");
    assert!(tree_calls(&tree).contains(&"click:Go to offline (fallback) mode".into()));
    let app = lock(&shared);
    assert!(app.ui.dialog.is_none());
    assert!(app.ui.tuning.routine_busy.is_none());
    let alert = crate::app::tests::alert_text(&app).unwrap();
    assert!(alert.contains("cannot unpack"), "{alert}");
}

#[test]
fn a_failed_crewchief_listing_warns_before_the_app_window_is_brought_up() {
    let backend = FakeBackend::new();
    let shared = options_pressed(&backend, "CrewChief");
    let tree = Arc::new(Tree::default());
    *tree.fail_operation.lock().unwrap() = Some("children:gameDefinitionList".into());
    let windows = Windows { tree: tree.clone(), title: "Crew Chief V4 - 4.18" };
    prepare_options(&Arc::downgrade(&shared), &windows, "CrewChief", "OpenRigCue");
    assert!(tree.app_activations.lock().unwrap().is_empty(), "the listing fails before the app window");
    assert!(tree_calls(&tree).contains(&"children:gameDefinitionList".into()));
    let app = lock(&shared);
    assert!(app.ui.dialog.is_none());
    assert!(app.ui.tuning.routine_busy.is_none());
    let alert = crate::app::tests::alert_text(&app).unwrap();
    assert!(alert.contains("fixture native exception"), "{alert}");
}

#[test]
fn a_routine_start_stops_on_a_failing_control_but_goes_on_after_a_refusal() {
    for (fail_click, stops) in [("Tuning Menu", true), ("3", false)] {
        let backend = FakeBackend::new();
        backend.docs.lock().unwrap()["tune"]["routines"] = json!({"Fanalab": {"enabled": true, "profile": "3"}});
        let shared = Arc::new(Mutex::new(loaded(backend)));
        let tree = Arc::new(Tree::default());
        *tree.fail_click.lock().unwrap() = Some(fail_click.into());
        let windows = Windows { tree: tree.clone(), title: "FanaLab V1.69" };
        launch_routine(&Arc::downgrade(&shared), &windows, "OpenRigCue");
        assert_eq!(tree.app_activations.lock().unwrap().is_empty(), stops, "failure at {fail_click}");
        assert_eq!(tree_calls(&tree).last(), Some(&format!("click:{fail_click}")));
        drop(lock(&shared).take_threads());
    }
}

#[test]
fn a_failing_simucube_tab_stops_the_start_but_a_failing_profile_list_is_a_refusal() {
    for (failure, stops) in [
        ("MW2.centralWidgetWindow.tabWidget.qt_tabwidget_tabbar", true),
        ("MW2.centralWidgetWindow.tabWidget.qt_tabwidget_stackedwidget.ProfilesTab.OfflineCachedProfilesDialog.cachedprofileslistWidget", false),
    ] {
        let backend = FakeBackend::new();
        backend.docs.lock().unwrap()["tune"]["routines"] =
            json!({"Simucube 2 True Drive Paddock": {"enabled": true, "profile": "Selected"}});
        let shared = Arc::new(Mutex::new(loaded(backend)));
        let tree = Arc::new(Tree::default());
        *tree.fail_operation.lock().unwrap() = Some(format!("children:{failure}"));
        let windows = Windows { tree: tree.clone(), title: "Simucube 2 True Drive 2025" };
        launch_routine(&Arc::downgrade(&shared), &windows, "OpenRigCue");
        assert_eq!(tree.app_activations.lock().unwrap().is_empty(), stops, "failure at {failure}");
        drop(lock(&shared).take_threads());
    }
}

#[test]
fn a_failing_click_stops_fanalab_at_that_control() {
    let tree = Arc::new(Tree::default());
    *tree.fail_click.lock().unwrap() = Some("Tuning Menu".into());
    let windows = Windows { tree: tree.clone(), title: "FanaLab V1.69" };
    assert_eq!(super::fanatec::activate_profile(&windows, Some("3")).unwrap_err(), "fixture click failure");
    assert_eq!(tree_calls(&tree).last().unwrap(), "click:Tuning Menu");
    assert!(!tree_calls(&tree).iter().any(|call| call.contains("Wheel device detected")));
}

#[test]
fn simucube_clicks_the_stored_profile_and_refuses_a_missing_one() {
    let tree = Arc::new(Tree::default());
    let windows = Windows { tree: tree.clone(), title: "Simucube 2 True Drive 2025" };
    assert_eq!(super::simecube::activate_profile(&windows, Some("Selected")).unwrap(), "OK");
    assert!(tree_calls(&tree)
        .iter()
        .any(|call| call.ends_with("cachedprofileslistWidget/#0") && call.starts_with("click:")));
    tree.calls.lock().unwrap().clear();
    assert!(super::simecube::activate_profile(&windows, Some("Absent")).unwrap_err().starts_with("TypeError:"));
    assert!(!tree_calls(&tree)
        .iter()
        .any(|call| call.contains("cachedprofileslistWidget/#") && call.starts_with("click:")));
}

#[test]
fn an_enabled_fanalab_routine_clicks_its_profile_once() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["tune"]["routines"] = json!({"Fanalab": {"enabled": true, "profile": "3"}});
    let shared = Arc::new(Mutex::new(loaded(backend)));
    let tree = Arc::new(Tree::default());
    let windows = Windows { tree: tree.clone(), title: "FanaLab V1.69" };
    launch_routine(&Arc::downgrade(&shared), &windows, "OpenRigCue");
    assert_eq!(tree_calls(&tree).iter().filter(|call| *call == "click:3").count(), 1);
    drop(lock(&shared).take_threads());
}
