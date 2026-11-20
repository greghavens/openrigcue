//! Tests of Games → PC and Games → Console: the list, search and pages, the Setup dialog (patch, instructions,
//! port, forwarding, other launchers), the refusals while a game runs, the PlayStation address, the game
//! detection, the start-up, the failures of the service's list and the texts of every theme.

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use super::*;
use crate::app::tests::FakeBackend;
use crate::backend::{Backend, Failure, Method};
use crate::host::UiApp;
use crate::ui::{dialogs, dispatch, navigate, Theme};

/// The app's fake service plus the games routes.
struct GamesBackend {
    inner: Arc<FakeBackend>,
    calls: Mutex<Vec<(Method, String, Option<Value>)>>,
    /// The patch answer: `None` succeeds, else the failure's status and text.
    patch_failure: Mutex<Option<(u16, String)>>,
    restart_required: Mutex<bool>,
    table_error: Mutex<bool>,
    pc_rows: Mutex<Vec<Value>>,
    /// The answer of both lists (`GET games?page=`), in place of the rows above.
    table_answer: Mutex<Option<Value>>,
    /// `PUT games/detection` fails.
    detection_error: Mutex<bool>,
    /// `GET games/bridge-path` fails.
    bridge_error: Mutex<bool>,
}

fn row(key: &str, name: &str, status: &str, entry_type: &str, port_key: Option<&str>) -> Value {
    json!({"key": key, "name": name, "status": status, "entry_type": entry_type, "port_key": port_key})
}

impl GamesBackend {
    fn new() -> Arc<GamesBackend> {
        let inner = FakeBackend::new();
        {
            let mut docs = inner.docs.lock().unwrap();
            docs["settings"]["network_ports"] = json!({"f12020": 20782, "gt7": 33740});
            docs["settings"]["port_forwarding"] = json!({"gt7": ["127.0.0.1:20000"]});
            docs["settings"]["app"]["playstation_address"] = json!("");
            docs["games/bridge-path"] = json!({"bridge": "/opt/bridge.exe", "launch_helper": "/opt/launch", "launch_option": "/opt/launch %command%", "bridge_present": false});
        }
        let mut pc = vec![
            row("ac", "Assetto Corsa", "ready_to_play", "ready_to_play", None),
            row("f12020", "F1 2020", "config", "config", Some("f12020")),
            row("iracing", "IRacing", "not_installed", "config", None),
        ];
        pc[0]["bridge"] = json!(true);
        for i in 0..5 {
            pc.push(row(&format!("g{i}"), &format!("Game {i}"), "ready_to_play", "ready_to_play", None));
        }
        Arc::new(GamesBackend {
            inner,
            calls: Mutex::new(Vec::new()),
            patch_failure: Mutex::new(None),
            restart_required: Mutex::new(true),
            table_error: Mutex::new(false),
            pc_rows: Mutex::new(pc),
            table_answer: Mutex::new(None),
            detection_error: Mutex::new(false),
            bridge_error: Mutex::new(false),
        })
    }

    fn calls_to(&self, method: Method, path: &str) -> Vec<Option<Value>> {
        self.calls.lock().unwrap().iter().filter(|(m, p, _)| *m == method && p == path).map(|c| c.2.clone()).collect()
    }

    fn detections(&self) -> Vec<Value> {
        self.calls_to(Method::Put, "games/detection").into_iter().map(|b| b.unwrap()["entry"].clone()).collect()
    }

    fn run_game(&self, game: &str) {
        self.inner.docs.lock().unwrap()["status"]["session"]["game"] = json!(game);
    }
}

impl Backend for GamesBackend {
    fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value, Failure> {
        self.calls.lock().unwrap().push((method, path.to_owned(), body.clone()));
        if path.starts_with("games?page=") {
            if let Some(answer) = self.table_answer.lock().unwrap().as_ref() {
                return Ok(answer.clone());
            }
        }
        if path == "games/detection" && *self.detection_error.lock().unwrap() {
            return Err(Failure { status: 500, detail: "KeyError: 'vehicle_name'".into() });
        }
        if path == "games/bridge-path" && *self.bridge_error.lock().unwrap() {
            return Err(Failure { status: 500, detail: "i/o error".into() });
        }
        if path.starts_with("games?page=") && *self.table_error.lock().unwrap() {
            return Ok(
                json!({"rows": [row("ac", "Assetto Corsa", "ready_to_play", "ready_to_play", None)], "error": "f12020: permission denied"}),
            );
        }
        match path {
            "games?page=PC" => Ok(Value::Array(self.pc_rows.lock().unwrap().clone())),
            "games?page=Console" => Ok(json!([
                row("testmode", "*Stand By Mode", "ready_to_play", "ready_to_play", None),
                row("f12020", "F1 2020", "instructions", "config", Some("f12020")),
                row("gt7", "Grand Turismo 7 PS4/PS5", "instructions", "instructions", Some("gt7")),
            ])),
            "games/detection" => Ok(Value::Null),
            "games/bridge-path" => Ok(self.inner.docs.lock().unwrap()["games/bridge-path"].clone()),
            p if p.starts_with("games/") && p.ends_with("/patch") => match self.patch_failure.lock().unwrap().clone() {
                Some((status, detail)) => Err(Failure { status, detail }),
                None => Ok(Value::Null),
            },
            p if p.starts_with("games/") && p.ends_with("/port") => {
                Ok(json!({"restart_required": *self.restart_required.lock().unwrap()}))
            }
            _ => self.inner.call(method, path, body),
        }
    }

    fn upload(&self, path: &str, bytes: Vec<u8>) -> Result<Value, Failure> {
        self.inner.upload(path, bytes)
    }

    fn fetch(&self, path: &str) -> Result<(Vec<u8>, String), Failure> {
        self.calls.lock().unwrap().push((Method::Get, path.to_owned(), None));
        if path.starts_with("games/gt7/instructions") {
            Ok((b"<p>Port 33740 on 127.0.0.1</p>".to_vec(), String::new()))
        } else {
            Err(Failure { status: 404, detail: "no instructions page".into() })
        }
    }

    fn translate(&self, text: &str) -> String {
        self.inner.translate(text)
    }

    fn excluded_strings(&self) -> Vec<String> {
        Vec::new()
    }
}

/// The app after the page load and one main-loop pass, on a screen.
fn app_on(backend: &Arc<GamesBackend>, screen: Screen) -> MyApp {
    let mut app = MyApp::new(backend.clone());
    let body = app.gui.create("body", "BODY");
    let root = app.main(body);
    app.gui.set_root(root);
    crate::remi::gui::dispatch(&mut app, body, "onload", &Default::default());
    app.main_loop_pass();
    drop(app.take_threads());
    navigate(&mut app, screen, 0);
    backend.calls.lock().unwrap().clear();
    app
}

fn index_of(app: &MyApp, console: bool, key: &str) -> usize {
    app.ui.devices.games.list(console).games.iter().position(|g| g.key == key).unwrap()
}

fn setup(app: &MyApp) -> Setup {
    match dh::current(app) {
        Some(Shared::Games(Dialog::Setup(setup))) => (**setup).clone(),
        other => panic!("no Setup dialog: {other:?}"),
    }
}

fn warnings(app: &MyApp) -> Vec<(String, String)> {
    app.ui
        .notices
        .history
        .iter()
        .filter(|n| n.severity == Severity::Warning)
        .map(|n| (n.title.clone(), n.message.clone()))
        .collect()
}

#[test]
fn the_pc_list_pages_six_games_and_searches() {
    let backend = GamesBackend::new();
    let mut app = app_on(&backend, Screen::Pc);
    assert_eq!(backend.calls_to(Method::Get, "games?page=PC").len(), 0, "read before the calls were cleared");
    let html = main(&app);
    assert!(html.to_lowercase().contains("pc games"), "the panel title in the theme's capitals");
    assert!(html.contains("1–6 / 8") && html.contains(">Assetto Corsa<"));
    assert!(!html.contains(">Game 4<"), "the seventh game is on the next page");
    // A game not installed has nothing to set up.
    let iracing = index_of(&app, false, "iracing");
    assert!(html.contains(&format!("data-arg=\"PC:setup:{iracing}\" disabled")));
    assert!(html.contains(">Not installed<") && html.contains(">Config<"));
    dispatch(&mut app, "action", "PC:page-next:", "").unwrap();
    let html = main(&app);
    assert!(html.contains("7–8 / 8") && html.contains(">Game 4<"));
    // Search resets to the first page and matches names only (case-insensitive).
    dispatch(&mut app, "set", "PC:search", "f1").unwrap();
    let html = main(&app);
    assert!(html.contains(">F1 2020<") && html.contains("1–1 / 1") && !html.contains(">Assetto Corsa<"));
    dispatch(&mut app, "set", "PC:search", "not installed").unwrap();
    assert!(main(&app).contains("No matches"), "a status text matches no game");
    dispatch(&mut app, "set", "PC:search", "zzz").unwrap();
    assert!(main(&app).contains("No matches"));
    // Reload games reads the list again; opening the screen reads it too.
    dispatch(&mut app, "action", "PC:reload:", "").unwrap();
    navigate(&mut app, Screen::Pc, 0);
    assert_eq!(backend.calls_to(Method::Get, "games?page=PC").len(), 2);
    assert!(main(&app).contains(">Reload games<"));
    drop(app.take_threads());
}

#[test]
fn a_failed_check_keeps_the_games_before_it_and_warns() {
    let backend = GamesBackend::new();
    let mut app = app_on(&backend, Screen::Pc);
    *backend.table_error.lock().unwrap() = true;
    dispatch(&mut app, "action", "PC:reload:", "").unwrap();
    assert_eq!(app.ui.devices.games.pc.games.len(), 1);
    assert!(warnings(&app).iter().any(|(_, m)| m.contains("permission denied")));
    drop(app.take_threads());
}

#[test]
fn apply_setup_patches_the_game_then_confirms_its_port() {
    let backend = GamesBackend::new();
    let mut app = app_on(&backend, Screen::Pc);
    let f1 = index_of(&app, false, "f12020");
    dispatch(&mut app, "action", &format!("PC:setup:{f1}"), "").unwrap();
    let html = crate::ui::render(&app, Region::Dialog);
    assert!(html.contains("F1 2020 — PC setup"), "{html}");
    assert!(html.contains("value=\"20782\"") && html.contains("min=\"1\" max=\"49152\""));
    assert!(html.contains(">Apply setup<"));
    assert!(!html.contains("go through the instructions again"), "the hint is for instructions games only");
    dispatch(&mut app, "dialog-field", "port", "20800").unwrap();
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert_eq!(backend.calls_to(Method::Post, "games/f12020/patch").len(), 1);
    let ports = backend.calls_to(Method::Put, "games/f12020/port");
    assert_eq!(ports, vec![Some(json!({"port": 20800, "forwarding": []}))]);
    assert!(app.ui.dialog.is_none());
    assert!(warnings(&app).iter().any(|(t, m)| t == "Restart" && m.contains("RESTARTED")));
    assert!(app.ui.notices.history.iter().any(|n| n.title == "Game setup saved"));
    assert_eq!(backend.calls_to(Method::Get, "games?page=PC").len(), 1, "the list is read again");
    drop(app.take_threads());
}

#[test]
fn setup_is_refused_while_a_game_runs_and_reports_service_refusals() {
    let backend = GamesBackend::new();
    let mut app = app_on(&backend, Screen::Pc);
    let f1 = index_of(&app, false, "f12020");
    backend.run_game("acc");
    dispatch(&mut app, "action", &format!("PC:setup:{f1}"), "").unwrap();
    assert!(warnings(&app).iter().any(|(_, m)| m.contains("Game port settings are locked")));
    assert!(crate::ui::render(&app, Region::Dialog).contains("id=\"game-port\" class=\"field-input\" type=\"number\" value=\"20782\" aria-label=\"UDP port\" data-act=\"dialog-field\" data-arg=\"port\" min=\"1\" max=\"49152\" step=\"1\" disabled"));
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert!(warnings(&app).iter().any(|(t, _)| t == "Game Running"));
    assert!(backend.calls_to(Method::Post, "games/f12020/patch").is_empty());
    assert!(app.ui.dialog.is_some(), "the dialog stays to try again");
    dispatch(&mut app, "dialog-close", "", "").unwrap();
    // Steam running: the service's text, and nothing else happens.
    backend.run_game("none");
    *backend.patch_failure.lock().unwrap() = Some((409, "Steam is running. Close Steam, then try again.".into()));
    dispatch(&mut app, "action", &format!("PC:setup:{f1}"), "").unwrap();
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert!(warnings(&app).iter().any(|(t, m)| t == "Steam Running" && m.starts_with("Steam is running")));
    assert_eq!(backend.calls_to(Method::Put, "games/f12020/port").len(), 1, "the port is confirmed apart");
    // Any other failure is an error.
    *backend.patch_failure.lock().unwrap() = Some((400, "configuring failed".into()));
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert!(app
        .ui
        .notices
        .history
        .iter()
        .any(|n| n.severity == Severity::Error && n.message.contains("configuring failed")));
    drop(app.take_threads());
}

#[test]
fn port_forwarding_is_checked_and_a_stored_list_changes_at_once() {
    let backend = GamesBackend::new();
    let mut app = app_on(&backend, Screen::Console);
    let gt7 = index_of(&app, true, "gt7");
    dispatch(&mut app, "action", &format!("Console:setup:{gt7}"), "").unwrap();
    let html = crate::ui::render(&app, Region::Dialog);
    assert!(html.contains("<p>Port 33740 on 127.0.0.1</p>"), "the instructions HTML");
    assert!(html.contains("go through the instructions again"));
    assert!(html.contains("Port Forwarding · 1") && html.contains(">20000<"));
    assert!(backend.calls.lock().unwrap().iter().any(|(_, p, _)| p == "games/gt7/instructions?console=true"));
    dispatch(&mut app, "dialog-field", "fwd-ip", "999.1.1.1").unwrap();
    dispatch(&mut app, "dialog-button", "fwd-add", "").unwrap();
    assert!(warnings(&app).iter().any(|(_, m)| m == "999.1.1.1 is not a correctly formed IP address"));
    dispatch(&mut app, "dialog-field", "fwd-ip", "127.0.0.1").unwrap();
    dispatch(&mut app, "dialog-field", "fwd-port", "20000").unwrap();
    dispatch(&mut app, "dialog-button", "fwd-add", "").unwrap();
    assert!(warnings(&app).iter().any(|(_, m)| m == "Forwarding to this IP and Port is already set up"));
    dispatch(&mut app, "dialog-field", "fwd-ip", "192.168.1.20").unwrap();
    dispatch(&mut app, "dialog-field", "fwd-port", "20001").unwrap();
    dispatch(&mut app, "dialog-button", "fwd-add", "").unwrap();
    assert_eq!(setup(&app).forwarding, ["127.0.0.1:20000", "192.168.1.20:20001"]);
    let stored = backend.calls_to(Method::Patch, "settings");
    assert!(stored
        .iter()
        .flatten()
        .any(|p| p["port_forwarding"]["gt7"] == json!(["127.0.0.1:20000", "192.168.1.20:20001"])));
    dispatch(&mut app, "dialog-button", "fwd-delete:0", "").unwrap();
    assert_eq!(setup(&app).forwarding, ["192.168.1.20:20001"]);
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert_eq!(
        backend.calls_to(Method::Put, "games/gt7/port"),
        vec![Some(json!({"port": 33740, "forwarding": ["192.168.1.20:20001"]}))]
    );
    assert!(backend.calls_to(Method::Post, "games/gt7/patch").is_empty(), "an instructions game is not patched");
    drop(app.take_threads());
}

#[test]
fn other_launchers_shows_the_bridge_launch_steps() {
    let backend = GamesBackend::new();
    let mut app = app_on(&backend, Screen::Pc);
    let ac = index_of(&app, false, "ac");
    dispatch(&mut app, "action", &format!("PC:setup:{ac}"), "").unwrap();
    dispatch(&mut app, "dialog-field", "open-bridge", "1").unwrap();
    let html = crate::ui::render(&app, Region::Dialog);
    assert!(html.contains(">Other launchers<"));
    assert!(html.contains("readonly value=\"/opt/launch %command%\""));
    assert!(html.contains("The bridge file is missing.") && html.contains("%command% is where the launcher"));
    assert!(!html.contains(">Apply setup<"), "a ready game without a port has nothing to apply");
    dispatch(&mut app, "dialog-close", "", "").unwrap();
    backend.inner.docs.lock().unwrap()["games/bridge-path"] = json!({"searched": ["/a", "/b"]});
    dispatch(&mut app, "action", &format!("PC:setup:{ac}"), "").unwrap();
    assert!(crate::ui::render(&app, Region::Dialog).contains("The bridge files were not found in /a, /b"));
    drop(app.take_threads());
}

#[test]
fn console_stores_the_playstation_address_and_runs_the_game_detection() {
    let backend = GamesBackend::new();
    let mut app = app_on(&backend, Screen::Console);
    let html = main(&app);
    assert!(html.contains("Console games") && html.contains("Connect your console and the PC to the same network."));
    assert!(html.contains("data-arg=\"Console:playstation_ip\"") && html.contains("id=\"console-detection\""));
    dispatch(&mut app, "set", "Console:playstation_ip", "192.168.1.9x").unwrap();
    assert!(warnings(&app).iter().any(|(_, m)| m == "192.168.1.9x is not a correctly formed IP address"));
    assert!(backend.calls_to(Method::Patch, "settings").is_empty());
    dispatch(&mut app, "set", "Console:playstation_ip", "192.168.1.90").unwrap();
    assert!(backend
        .calls_to(Method::Patch, "settings")
        .iter()
        .flatten()
        .any(|p| p["app"]["playstation_address"] == json!("192.168.1.90")));
    assert_eq!(app.setting(&["app", "playstation_address"]), &json!("192.168.1.90"));
    dispatch(&mut app, "set", "Console:playstation_ip", "").unwrap();
    assert_eq!(app.setting(&["app", "playstation_address"]), &json!(""), "an empty address is stored");
    // Auto offers no choice; Manual chooses the first game (Stand By forces none) and offers each game.
    assert!(!main(&app).contains("Use this game"));
    dispatch(&mut app, "set", "Console:detection", "1").unwrap();
    assert_eq!(backend.detections(), [Value::Null]);
    let html = main(&app);
    assert!(html.contains("Use this game") && html.contains("Selected game"));
    let gt7 = index_of(&app, true, "gt7");
    dispatch(&mut app, "action", &format!("Console:use:{gt7}"), "").unwrap();
    assert_eq!(backend.detections(), [Value::Null, json!("gt7")]);
    assert_eq!(app.ui.devices.games.chosen.as_deref(), Some("gt7"));
    dispatch(&mut app, "set", "Console:detection", "0").unwrap();
    assert_eq!(backend.detections(), [Value::Null, json!("gt7"), Value::Null]);
    assert!(app.ui.devices.games.chosen.is_none() && !main(&app).contains("Use this game"));
    drop(app.take_threads());
}

#[test]
fn the_start_reads_both_lists_and_forces_no_game() {
    let backend = GamesBackend::new();
    let mut app = MyApp::new(backend.clone());
    start(&mut app).unwrap();
    assert_eq!(backend.calls_to(Method::Get, "games?page=PC").len(), 1);
    assert_eq!(backend.calls_to(Method::Get, "games?page=Console").len(), 1);
    assert_eq!(backend.detections(), [Value::Null]);
    rules(&mut app).unwrap();
}

#[test]
fn ip_addresses_are_checked_strictly() {
    for ok in ["127.0.0.1", "192.168.1.50", "0.0.0.0", "::", "::1", "fe80::1%eth0", "::ffff:1.2.3.4", "1:2:3:4:5:6:7:8"]
    {
        assert!(is_ip_address(ok), "{ok}");
    }
    for bad in [
        "",
        "1.2.3",
        "01.2.3.4",
        "256.1.1.1",
        "1.2.3.4.5",
        "localhost",
        " 1.2.3.4",
        "1::2::3",
        "fe80::1%",
        "1.2.3.4/32",
    ] {
        assert!(!is_ip_address(bad), "{bad}");
    }
    assert_eq!(status_text("not_installed"), "Not installed");
    assert_eq!(status_text("ready_to_play"), "Game is ready");
}

#[test]
fn every_theme_draws_the_screens_and_every_text_has_an_entry() {
    let backend = GamesBackend::new();
    let mut app = app_on(&backend, Screen::Pc);
    backend.inner.translations.lock().unwrap().clear();
    for theme in Theme::ALL {
        app.ui.theme = theme;
        for screen in [Screen::Pc, Screen::Console] {
            navigate(&mut app, screen, 0);
            let html = main(&app);
            let title = if screen == Screen::Pc { "PC games" } else { "Console games" };
            let shown = if theme.capitals() { title.to_uppercase() } else { title.to_owned() };
            assert!(html.contains(&shown), "{theme:?} {screen:?}");
        }
    }
    app.ui.devices.games.manual = true;
    app.ui.devices.games.chosen = Some("gt7".into());
    navigate(&mut app, Screen::Console, 0);
    main(&app);
    for (console, key) in [(false, "f12020"), (false, "ac"), (true, "gt7"), (true, "f12020")] {
        let index = index_of(&app, console, key);
        backend.run_game(if key == "gt7" { "acc" } else { "none" });
        open_setup(&mut app, console, index).unwrap();
        crate::ui::render(&app, Region::Dialog);
        dialogs::close(&mut app);
    }
    backend.inner.docs.lock().unwrap()["games/bridge-path"] = json!({"searched": ["/a"]});
    let index = index_of(&app, false, "ac");
    open_setup(&mut app, false, index).unwrap();
    crate::ui::render(&app, Region::Dialog);
    let looked_up: std::collections::BTreeSet<String> =
        backend.inner.translations.lock().unwrap().iter().cloned().collect();
    let missing = crate::app::tests::texts_without_entry(&looked_up, &[]);
    assert_eq!(missing, Vec::<String>::new(), "texts without an entry in en.json");
    drop(app.take_threads());
}

/// The app after the page load and one main-loop pass (the start included), on no screen of its own.
fn started(backend: &Arc<GamesBackend>) -> MyApp {
    let mut app = MyApp::new(backend.clone());
    let body = app.gui.create("body", "BODY");
    let root = app.main(body);
    app.gui.set_root(root);
    crate::remi::gui::dispatch(&mut app, body, "onload", &Default::default());
    app.main_loop_pass();
    drop(app.take_threads());
    app
}

/// Asserts that the texts appear in `html` in this order.
fn in_order(html: &str, texts: &[&str]) {
    let mut from = 0;
    for text in texts {
        let at = html[from..].find(text).unwrap_or_else(|| panic!("{text:?} after position {from} in {html}"));
        from += at + text.len();
    }
}

fn keys(app: &MyApp, console: bool) -> Vec<String> {
    app.ui.devices.games.list(console).games.iter().map(|g| g.key.clone()).collect()
}

fn last_warning(app: &MyApp) -> (String, String) {
    warnings(app).last().cloned().expect("a warning")
}

fn patches_of(backend: &GamesBackend, path: &str) -> usize {
    backend.calls_to(Method::Post, path).len()
}

fn native_table_error() -> Value {
    json!({
        "rows": [row("ac", "Assetto Corsa", "ready_to_play", "ready_to_play", None)],
        "error": "f12015: no [NETWORK_PORT] f1 entry",
        "native_error": {"class_name": "KeyError", "message": "'f1'"},
    })
}

const DECODE_ERROR: &str =
    "UnicodeDecodeError: 'charmap' codec can't decode byte 0x81 in position 8209: character maps to <undefined>";

fn native_decode_table_error(game: &str) -> Value {
    let file = if game == "kartkraft" { "Game.ini" } else { "ExtraConfig.xml" };
    json!({
        "rows": [row("ac", "Assetto Corsa", "ready_to_play", "ready_to_play", None)],
        "error": format!("{game}: i/o: fixture/{file} is not valid Ansi text"),
        "native_error": {
            "class_name": "UnicodeDecodeError",
            "message": "'charmap' codec can't decode byte 0x81 in position 8209: character maps to <undefined>",
        },
    })
}

#[test]
fn every_status_has_its_text() {
    assert_eq!(status_text("not_installed"), "Not installed");
    assert_eq!(status_text("ready_to_play"), "Game is ready");
    assert_eq!(status_text("instructions"), "Instructions");
    assert_eq!(status_text("config"), "Config");
}

#[test]
fn the_console_screen_shows_the_steps_the_address_the_detection_then_the_games() {
    let backend = GamesBackend::new();
    let mut app = app_on(&backend, Screen::Console);
    let html = main(&app);
    let steps = [
        "Connect your console and the PC to the same network.",
        "Where you can, wire both the console and the PC to your router with ETHERNET CABLES, as Wi-Fi can add lag. Never run an Ethernet cable straight from the console to the PC.",
        "Set the game up on your console by following its instructions below.",
        "Switch GAME DETECTION to MANUAL, then pick the game you are going to play from the list below",
        "Console games transmit telemetry only while the vehicle is moving.",
    ]
    .map(|step| format!("<p>{}</p>", esc(step)));
    let mut order: Vec<&str> = steps.iter().map(String::as_str).collect();
    order.extend([
        "data-arg=\"Console:playstation_ip\"",
        "id=\"console-detection\"",
        "id=\"games-search-console\"",
        "data-game=\"testmode\"",
        "data-game=\"f12020\"",
        "data-game=\"gt7\"",
    ]);
    in_order(&html, &order);
    // Stand By has nothing to set up; an instructions game has.
    assert!(html.contains("data-arg=\"Console:setup:0\" disabled"));
    assert!(html.contains("data-arg=\"Console:setup:2\">") && html.contains("<small>Instructions</small>"));
    // The search keeps the matching games; a blank search shows every game.
    dispatch(&mut app, "set", "Console:search", "grand").unwrap();
    let html = main(&app);
    assert!(html.contains("data-game=\"gt7\"") && !html.contains("data-game=\"f12020\""));
    assert!(!html.contains("data-game=\"testmode\""));
    dispatch(&mut app, "set", "Console:search", "  ").unwrap();
    let html = main(&app);
    in_order(&html, &["data-game=\"testmode\"", "data-game=\"f12020\"", "data-game=\"gt7\""]);
    drop(app.take_threads());
}

#[test]
fn the_pc_screen_has_no_console_rows_and_a_failed_setup_can_be_applied_again() {
    let backend = GamesBackend::new();
    let mut app = app_on(&backend, Screen::Pc);
    navigate(&mut app, Screen::Pc, 0);
    let html = main(&app);
    assert!(!html.contains("Playstation IP") && !html.contains("console-detection") && !html.contains("Use this game"));
    assert!(backend.detections().is_empty(), "the PC screen forces no game");
    let f1 = index_of(&app, false, "f12020");
    *backend.patch_failure.lock().unwrap() = Some((400, "configuring failed".into()));
    dispatch(&mut app, "action", &format!("PC:setup:{f1}"), "").unwrap();
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert!(app
        .ui
        .notices
        .history
        .iter()
        .any(|n| n.severity == Severity::Error && n.message == "Error: configuring failed"));
    assert!(app.ui.dialog.is_some(), "the setup can be applied again");
    *backend.patch_failure.lock().unwrap() = None;
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert_eq!(patches_of(&backend, "games/f12020/patch"), 2);
    assert!(app.ui.notices.history.iter().any(|n| n.title == "Game setup saved" && n.message == "Game is ready"));
    assert!(app.ui.dialog.is_none());
    drop(app.take_threads());
}

#[test]
fn the_running_game_is_read_from_the_service_before_the_setup_is_applied() {
    for (cached, current, allowed) in [("none", "ac", false), ("ac", "none", true)] {
        let backend = GamesBackend::new();
        let mut app = app_on(&backend, Screen::Pc);
        let f1 = index_of(&app, false, "f12020");
        dispatch(&mut app, "action", &format!("PC:setup:{f1}"), "").unwrap();
        app.docs.status["session"]["game"] = json!(cached);
        backend.run_game(current);
        dispatch(&mut app, "dialog-ok", "", "").unwrap();
        assert_eq!(
            patches_of(&backend, "games/f12020/patch"),
            usize::from(allowed),
            "cached {cached}, current {current}"
        );
        assert_eq!(warnings(&app).iter().any(|(t, _)| t == "Game Running"), !allowed);
        assert_eq!(app.ui.dialog.is_some(), !allowed);
        drop(app.take_threads());
    }
}

#[test]
fn a_failed_game_read_stops_the_setup_before_any_patch() {
    let backend = GamesBackend::new();
    let mut app = app_on(&backend, Screen::Pc);
    let f1 = index_of(&app, false, "f12020");
    let remove_game = |backend: &GamesBackend| {
        backend.inner.docs.lock().unwrap()["status"]["session"].as_object_mut().unwrap().remove("game");
    };
    remove_game(&backend);
    let error = dispatch(&mut app, "action", &format!("PC:setup:{f1}"), "").unwrap_err();
    assert!(error.contains("KeyError: 'game'"), "{error}");
    assert!(app.ui.dialog.is_none());
    backend.run_game("none");
    dispatch(&mut app, "action", &format!("PC:setup:{f1}"), "").unwrap();
    remove_game(&backend);
    let error = dispatch(&mut app, "dialog-ok", "", "").unwrap_err();
    assert!(error.contains("KeyError: 'game'"), "{error}");
    assert_eq!(patches_of(&backend, "games/f12020/patch"), 0);
    assert!(app.ui.dialog.is_some());
    drop(app.take_threads());
}

#[test]
fn a_console_game_unknown_to_the_detection_is_marked_but_forces_nothing() {
    let backend = GamesBackend::new();
    *backend.table_answer.lock().unwrap() = Some(json!([
        row("testmode", "*Stand By Mode", "ready_to_play", "ready_to_play", None),
        row("gt7", "Unknown fixture game", "instructions", "instructions", Some("gt7")),
    ]));
    let mut app = app_on(&backend, Screen::Console);
    dispatch(&mut app, "set", "Console:detection", "1").unwrap();
    assert_eq!(backend.detections(), [Value::Null]);
    assert_eq!(
        dispatch(&mut app, "action", "Console:use:1", "").unwrap_err(),
        "UnboundLocalError: cannot access local variable 'game' where it is not associated with a value"
    );
    assert_eq!(app.ui.devices.games.chosen.as_deref(), Some("gt7"));
    assert_eq!(backend.detections(), [Value::Null], "nothing more is forced");
    drop(app.take_threads());
}

#[test]
fn detection_failures_are_reported_except_for_the_first_choice() {
    let backend = GamesBackend::new();
    *backend.detection_error.lock().unwrap() = true;
    let mut app = app_on(&backend, Screen::Console);
    assert!(app.ml.fatal_error.is_none(), "the start's Stand By choice failing is only logged");
    dispatch(&mut app, "set", "Console:detection", "1").unwrap();
    assert!(app.ui.devices.games.manual);
    assert_eq!(app.ui.devices.games.chosen.as_deref(), Some("testmode"), "the first game is chosen anyway");
    backend.calls.lock().unwrap().clear();
    let gt7 = index_of(&app, true, "gt7");
    let error = dispatch(&mut app, "action", &format!("Console:use:{gt7}"), "").unwrap_err();
    assert!(error.contains("KeyError: 'vehicle_name'"), "{error}");
    assert_eq!(app.ui.devices.games.chosen.as_deref(), Some("gt7"));
    assert_eq!(backend.detections(), [json!("gt7")]);
    let error = dispatch(&mut app, "set", "Console:detection", "0").unwrap_err();
    assert!(error.contains("KeyError: 'vehicle_name'"), "{error}");
    assert!(!app.ui.devices.games.manual && app.ui.devices.games.chosen.is_none());
    assert_eq!(backend.detections(), [json!("gt7"), Value::Null]);
    *backend.detection_error.lock().unwrap() = false;
    dispatch(&mut app, "set", "Console:detection", "1").unwrap();
    dispatch(&mut app, "action", &format!("Console:use:{gt7}"), "").unwrap();
    assert_eq!(app.ui.devices.games.chosen.as_deref(), Some("gt7"));
    drop(app.take_threads());
}

#[test]
fn the_forwarding_fields_start_at_the_local_default_and_a_confirmed_change_needs_a_restart() {
    let backend = GamesBackend::new();
    let mut app = app_on(&backend, Screen::Console);
    let gt7 = index_of(&app, true, "gt7");
    dispatch(&mut app, "action", &format!("Console:setup:{gt7}"), "").unwrap();
    let html = crate::ui::render(&app, Region::Dialog);
    assert!(html.contains("id=\"forward-ip\" class=\"field-input\" type=\"text\" value=\"127.0.0.1\""));
    assert!(html.contains("id=\"forward-port\" class=\"field-input\" type=\"number\" value=\"10000\""));
    dispatch(&mut app, "dialog-button", "fwd-add", "").unwrap();
    assert_eq!(setup(&app).forwarding, ["127.0.0.1:20000", "127.0.0.1:10000"]);
    let stored = backend.calls_to(Method::Patch, "settings").into_iter().flatten().collect::<Vec<_>>();
    assert!(stored.contains(&json!({"port_forwarding": {"gt7": ["127.0.0.1:20000", "127.0.0.1:10000"]}})));
    dispatch(&mut app, "dialog-button", "fwd-add", "").unwrap();
    assert_eq!(last_warning(&app).1, "Forwarding to this IP and Port is already set up");
    assert_eq!(backend.calls_to(Method::Patch, "settings").len(), stored.len(), "a duplicate is not stored");
    dispatch(&mut app, "dialog-button", "fwd-delete:1", "").unwrap();
    let stored = backend.calls_to(Method::Patch, "settings").into_iter().flatten().collect::<Vec<_>>();
    assert_eq!(stored.last(), Some(&json!({"port_forwarding": {"gt7": ["127.0.0.1:20000"]}})));
    dispatch(&mut app, "dialog-button", "fwd-add", "").unwrap();
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert_eq!(
        backend.calls_to(Method::Put, "games/gt7/port"),
        [Some(json!({"port": 33740, "forwarding": ["127.0.0.1:20000", "127.0.0.1:10000"]}))]
    );
    assert_eq!(last_warning(&app), ("Restart".into(), "THIS CHANGE TAKES EFFECT AFTER OpenRigCue IS RESTARTED".into()));
    assert_eq!(backend.calls_to(Method::Get, "games?page=Console").len(), 1, "the list is read again");
    assert!(app.ui.dialog.is_none());
    drop(app.take_threads());
}

#[test]
fn the_port_field_takes_the_stored_port_as_a_whole_number() {
    for port in [json!(32740), json!("3_2_740"), json!(32740.9)] {
        let backend = GamesBackend::new();
        let mut app = app_on(&backend, Screen::Console);
        backend.inner.docs.lock().unwrap()["settings"]["network_ports"]["gt7"] = port.clone();
        let gt7 = index_of(&app, true, "gt7");
        dispatch(&mut app, "action", &format!("Console:setup:{gt7}"), "").unwrap();
        assert_eq!(setup(&app).port, "32740", "{port}");
        assert!(crate::ui::render(&app, Region::Dialog).contains("type=\"number\" value=\"32740\""), "{port}");
        drop(app.take_threads());
    }
}

#[test]
fn a_stored_port_that_is_no_whole_number_opens_no_setup() {
    for (value, class) in [(Value::Null, "TypeError:"), (json!("33.5"), "ValueError:")] {
        let backend = GamesBackend::new();
        let mut app = app_on(&backend, Screen::Console);
        backend.inner.docs.lock().unwrap()["settings"]["network_ports"]["gt7"] = value.clone();
        let gt7 = index_of(&app, true, "gt7");
        let error = dispatch(&mut app, "action", &format!("Console:setup:{gt7}"), "").unwrap_err();
        assert!(error.contains(class), "{value}: {error}");
        assert!(app.ui.dialog.is_none());
        drop(app.take_threads());
    }
}

#[test]
fn a_stored_forwarding_list_that_cannot_be_read_opens_no_setup() {
    for (value, expected) in [
        (Value::Null, "TypeError: 'NoneType' object is not iterable"),
        (json!(["127.0.0.1:1234", 7]), "AttributeError: 'int' object has no attribute 'split'"),
        (json!("127.0.0.1:1234"), "TypeError: 'str' object is not iterable"),
        (json!(["127.0.0.1:1234", "10.0.0.2"]), "IndexError: list index out of range"),
    ] {
        let backend = GamesBackend::new();
        let mut app = app_on(&backend, Screen::Console);
        backend.inner.docs.lock().unwrap()["settings"]["port_forwarding"]["gt7"] = value.clone();
        let gt7 = index_of(&app, true, "gt7");
        let error = dispatch(&mut app, "action", &format!("Console:setup:{gt7}"), "").unwrap_err();
        assert!(error.contains(expected), "{value}: {error}");
        assert!(app.ui.dialog.is_none());
        drop(app.take_threads());
    }
    let backend = GamesBackend::new();
    let mut app = app_on(&backend, Screen::Console);
    backend.inner.docs.lock().unwrap()["settings"]["port_forwarding"]["gt7"] = json!(["10.0.0.1:12345"]);
    let gt7 = index_of(&app, true, "gt7");
    dispatch(&mut app, "action", &format!("Console:setup:{gt7}"), "").unwrap();
    let html = crate::ui::render(&app, Region::Dialog);
    assert!(html.contains(">10.0.0.1<") && html.contains(">12345<"), "{html}");
    drop(app.take_threads());
}

#[test]
fn the_confirmed_port_is_sent_as_a_whole_number() {
    let backend = GamesBackend::new();
    let mut app = app_on(&backend, Screen::Console);
    let gt7 = index_of(&app, true, "gt7");
    dispatch(&mut app, "action", &format!("Console:setup:{gt7}"), "").unwrap();
    // A fraction stays in the box, and applying it is refused: the port is a whole number (nothing is sent).
    dispatch(&mut app, "dialog-field", "port", "32.5").unwrap();
    assert_eq!(setup(&app).port, "32.5");
    assert!(dispatch(&mut app, "dialog-ok", "", "").unwrap_err().starts_with("ValueError"));
    assert!(backend.calls_to(Method::Put, "games/gt7/port").is_empty());
    // Digit groups with underscores are a whole number.
    dispatch(&mut app, "dialog-field", "port", "3_3_741").unwrap();
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    let sent = backend.calls_to(Method::Put, "games/gt7/port");
    assert_eq!(sent.len(), 1);
    assert!(sent[0].as_ref().unwrap()["port"].is_i64(), "{sent:?}");
    assert_eq!(sent[0].as_ref().unwrap()["port"], json!(33741));
    drop(app.take_threads());
}

#[test]
fn the_address_field_shows_the_stored_address_and_a_failed_store_is_reported() {
    let backend = GamesBackend::new();
    backend.inner.docs.lock().unwrap()["settings"]["app"]["playstation_address"] = json!("192.168.1.45");
    let mut app = app_on(&backend, Screen::Console);
    assert!(
        main(&app).contains("id=\"console-playstation-ip\" class=\"field-input\" type=\"text\" value=\"192.168.1.45\"")
    );
    backend.inner.docs.lock().unwrap()["settings"]["app"] = Value::Null;
    let error = dispatch(&mut app, "set", "Console:playstation_ip", "192.168.1.47").unwrap_err();
    assert!(error.contains("TypeError: 'NoneType' object does not support item assignment"), "{error}");
    assert_eq!(backend.inner.docs.lock().unwrap()["settings"]["app"], Value::Null, "nothing is stored");
    drop(app.take_threads());
    // A stored empty address shows an empty field, and the start stores nothing.
    let backend = GamesBackend::new();
    backend.inner.docs.lock().unwrap()["settings"]["app"]["playstation_address"] = Value::Null;
    let mut app = started(&backend);
    assert!(
        !backend
            .calls_to(Method::Patch, "settings")
            .iter()
            .flatten()
            .any(|p| p["app"]["playstation_address"].is_string()),
        "the start stores no address"
    );
    navigate(&mut app, Screen::Console, 0);
    assert!(main(&app).contains("id=\"console-playstation-ip\" class=\"field-input\" type=\"text\" value=\"\""));
    assert_eq!(backend.inner.docs.lock().unwrap()["settings"]["app"]["playstation_address"], Value::Null);
    drop(app.take_threads());
}

#[test]
fn the_instructions_show_in_the_setup_with_the_game_and_its_mode() {
    let backend = GamesBackend::new();
    let mut app = app_on(&backend, Screen::Console);
    let gt7 = index_of(&app, true, "gt7");
    dispatch(&mut app, "action", &format!("Console:setup:{gt7}"), "").unwrap();
    let html = crate::ui::render(&app, Region::Dialog);
    assert!(html.contains("Grand Turismo 7 PS4/PS5 — Console setup"), "{html}");
    assert!(html.contains("<div class=\"dialog-text instructions\"><p>Port 33740 on 127.0.0.1</p></div>"));
    assert_eq!(
        backend.calls.lock().unwrap().iter().filter(|(_, p, _)| p == "games/gt7/instructions?console=true").count(),
        1
    );
    dispatch(&mut app, "dialog-close", "", "").unwrap();
    // A missing instructions page says so in its place.
    let f1 = index_of(&app, true, "f12020");
    dispatch(&mut app, "action", &format!("Console:setup:{f1}"), "").unwrap();
    let html = crate::ui::render(&app, Region::Dialog);
    assert!(html.contains("<p class=\"warning-text\">no instructions page</p>"), "{html}");
    drop(app.take_threads());
}

#[test]
fn a_failed_list_read_keeps_the_games_before_it_on_both_screens_and_a_retry_restores_them() {
    for screen in [Screen::Pc, Screen::Console] {
        let backend = GamesBackend::new();
        let mut app = app_on(&backend, screen);
        let console = screen == Screen::Console;
        let name = mode_name(console);
        let full = keys(&app, console);
        let reload = |app: &mut MyApp| dispatch(app, "action", &format!("{name}:reload:"), "").unwrap();
        *backend.table_error.lock().unwrap() = true;
        reload(&mut app);
        assert_eq!(keys(&app, console), ["ac"], "{screen:?}");
        assert_eq!(last_warning(&app).1, "f12020: permission denied");
        *backend.table_error.lock().unwrap() = false;
        reload(&mut app);
        assert_eq!(keys(&app, console), full);
        // The service's error details name the error.
        *backend.table_answer.lock().unwrap() = Some(native_table_error());
        reload(&mut app);
        assert_eq!(keys(&app, console), ["ac"]);
        assert_eq!(last_warning(&app).1, "KeyError: 'f1'");
        for game in ["kartkraft", "thecrew", "thecrew2"] {
            *backend.table_answer.lock().unwrap() = Some(native_decode_table_error(game));
            reload(&mut app);
            assert_eq!(keys(&app, console), ["ac"], "{game}");
            assert_eq!(last_warning(&app).1, DECODE_ERROR);
        }
        *backend.table_answer.lock().unwrap() = None;
        reload(&mut app);
        assert_eq!(keys(&app, console), full);
        // Opening the screen reads the list too.
        *backend.table_answer.lock().unwrap() = Some(native_table_error());
        navigate(&mut app, screen, 0);
        assert_eq!(keys(&app, console), ["ac"]);
        assert!(main(&app).contains(">Assetto Corsa<"));
        drop(app.take_threads());
    }
}

#[test]
fn malformed_error_details_fall_back_to_the_service_error_text() {
    let backend = GamesBackend::new();
    let mut app = app_on(&backend, Screen::Pc);
    for native in [Value::Null, json!("KeyError: 'f1'"), json!({"class_name": "KeyError"})] {
        let mut answer = native_table_error();
        answer["error"] = json!("KeyError: legacy opaque message");
        answer["native_error"] = native.clone();
        *backend.table_answer.lock().unwrap() = Some(answer);
        dispatch(&mut app, "action", "PC:reload:", "").unwrap();
        assert_eq!(last_warning(&app).1, "KeyError: legacy opaque message", "{native}");
    }
    drop(app.take_threads());
}

#[test]
fn a_failed_list_read_at_start_is_reported_and_the_interface_keeps_running() {
    let backend = GamesBackend::new();
    *backend.table_answer.lock().unwrap() = Some(native_table_error());
    let mut app = MyApp::new(backend.clone());
    assert_eq!(start(&mut app), Ok(()), "the start goes on");
    assert_eq!(keys(&app, false), ["ac"]);
    assert_eq!(keys(&app, true), ["ac"]);
    assert_eq!(warnings(&app).iter().filter(|(_, m)| m == "KeyError: 'f1'").count(), 2, "one per list");
    *backend.table_answer.lock().unwrap() = None;
    let mut app = app_on(&backend, Screen::Pc);
    *backend.table_answer.lock().unwrap() = Some(native_table_error());
    navigate(&mut app, Screen::Pc, 0);
    assert!(main(&app).contains(">Assetto Corsa<"), "the screen still shows the games read");
    drop(app.take_threads());
}

#[test]
fn a_confirmed_port_stays_confirmed_when_the_list_cannot_be_read_again() {
    // A game that needs configuring: patched, its port confirmed, then the list read fails.
    let backend = GamesBackend::new();
    let mut app = app_on(&backend, Screen::Pc);
    let f1 = index_of(&app, false, "f12020");
    dispatch(&mut app, "action", &format!("PC:setup:{f1}"), "").unwrap();
    *backend.table_answer.lock().unwrap() = Some(native_table_error());
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert_eq!(backend.calls_to(Method::Put, "games/f12020/port").len(), 1);
    assert_eq!(keys(&app, false), ["ac"]);
    assert!(warnings(&app).iter().any(|(_, m)| m == "KeyError: 'f1'"));
    drop(app.take_threads());
    // The Crew games, whose settings file cannot be decoded when the list is read again.
    for (game, name) in [("thecrew", "The Crew"), ("thecrew2", "The Crew 2")] {
        let backend = GamesBackend::new();
        backend.inner.docs.lock().unwrap()["settings"]["network_ports"]["thecrew"] = json!(1337);
        *backend.table_answer.lock().unwrap() = Some(json!([
            row("ac", "Assetto Corsa", "ready_to_play", "ready_to_play", None),
            row(game, name, "config", "dll", Some("thecrew")),
        ]));
        let mut app = app_on(&backend, Screen::Pc);
        let index = index_of(&app, false, game);
        dispatch(&mut app, "action", &format!("PC:setup:{index}"), "").unwrap();
        assert_eq!(setup(&app).port, "1337");
        *backend.table_answer.lock().unwrap() = Some(native_decode_table_error(game));
        dispatch(&mut app, "dialog-ok", "", "").unwrap();
        assert_eq!(backend.calls_to(Method::Put, &format!("games/{game}/port")).len(), 1, "{game}");
        assert_eq!(keys(&app, false), ["ac"]);
        assert!(warnings(&app).iter().any(|(_, m)| m == DECODE_ERROR));
        drop(app.take_threads());
    }
    // A Console game whose list read fails after the confirmation.
    let backend = GamesBackend::new();
    let mut app = app_on(&backend, Screen::Console);
    let gt7 = index_of(&app, true, "gt7");
    dispatch(&mut app, "action", &format!("Console:setup:{gt7}"), "").unwrap();
    *backend.table_error.lock().unwrap() = true;
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert_eq!(backend.calls_to(Method::Put, "games/gt7/port").len(), 1);
    assert_eq!(keys(&app, true), ["ac"]);
    drop(app.take_threads());
}

#[test]
fn a_service_refusal_names_steam_or_the_game_and_the_setup_can_be_applied_again() {
    let backend = GamesBackend::new();
    let mut app = app_on(&backend, Screen::Pc);
    let f1 = index_of(&app, false, "f12020");
    dispatch(&mut app, "action", &format!("PC:setup:{f1}"), "").unwrap();
    let refusals = [
        ("Steam Running", "Steam is running, so OpenRigCue cannot set the game up. Close Steam, then try again."),
        ("Game Running", "A game is running, so OpenRigCue cannot set the game up. Close the game, then try again."),
    ];
    for (tries, (title, detail)) in refusals.into_iter().enumerate() {
        *backend.patch_failure.lock().unwrap() = Some((409, detail.into()));
        dispatch(&mut app, "dialog-ok", "", "").unwrap();
        assert_eq!(patches_of(&backend, "games/f12020/patch"), tries + 1);
        assert!(warnings(&app).contains(&(title.to_owned(), detail.to_owned())), "{title}");
        assert!(app.ui.dialog.is_some(), "{title}: the setup stays to be applied again");
        assert_eq!(backend.calls_to(Method::Put, "games/f12020/port").len(), tries + 1, "the port is confirmed");
    }
    *backend.patch_failure.lock().unwrap() = None;
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert_eq!(patches_of(&backend, "games/f12020/patch"), 3);
    assert!(app.ui.notices.history.iter().any(|n| n.title == "Game setup saved"));
    assert!(app.ui.dialog.is_none());
    drop(app.take_threads());
}

#[test]
fn other_launchers_are_offered_for_every_bridge_game_with_the_paths_to_copy() {
    let backend = GamesBackend::new();
    let mut ac = row("ac", "Assetto Corsa", "ready_to_play", "ready_to_play", None);
    ac["bridge"] = json!(true);
    let mut iracing = row("iracing", "IRacing", "not_installed", "config", None);
    iracing["bridge"] = json!(true);
    *backend.table_answer.lock().unwrap() =
        Some(json!([ac, row("f12020", "F1 2020", "config", "config", Some("f12020")), iracing]));
    backend.inner.docs.lock().unwrap()["games/bridge-path"] = json!({
        "bridge": "/home/u/.local/share/openrigcue/bridge/openrigcue-bridge.exe",
        "bridge_present": true,
        "launch_helper": "/home/u/.local/share/openrigcue/bridge/openrigcue-bridge-run",
        "launch_option": "\"/home/u/.local/share/openrigcue/bridge/openrigcue-bridge-run\" %command%",
    });
    let mut app = app_on(&backend, Screen::Pc);
    let iracing = index_of(&app, false, "iracing");
    assert!(
        main(&app).contains(&format!("data-arg=\"PC:setup:{iracing}\">")),
        "a bridge game not installed has a Setup"
    );
    let f1 = index_of(&app, false, "f12020");
    dispatch(&mut app, "action", &format!("PC:setup:{f1}"), "").unwrap();
    assert!(!crate::ui::render(&app, Region::Dialog).contains("Other launchers"), "not a bridge game");
    dispatch(&mut app, "dialog-close", "", "").unwrap();
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "action", &format!("PC:setup:{iracing}"), "").unwrap();
    assert_eq!(backend.calls_to(Method::Get, "games/bridge-path").len(), 1);
    let html = crate::ui::render(&app, Region::Dialog);
    for value in [
        "/home/u/.local/share/openrigcue/bridge/openrigcue-bridge.exe",
        "/home/u/.local/share/openrigcue/bridge/openrigcue-bridge-run",
        "\"/home/u/.local/share/openrigcue/bridge/openrigcue-bridge-run\" %command%",
    ] {
        assert!(html.contains(&format!("readonly value=\"{}\"", esc(value))), "{value}: {html}");
    }
    assert!(html.contains("This game sends telemetry only when it is started through the OpenRigCue bridge."));
    assert!(html.contains("%command% is where the launcher"));
    assert!(!html.contains("The bridge file is missing."));
    dispatch(&mut app, "dialog-close", "", "").unwrap();
    backend.inner.docs.lock().unwrap()["games/bridge-path"]["bridge_present"] = json!(false);
    dispatch(&mut app, "action", &format!("PC:setup:{iracing}"), "").unwrap();
    assert!(crate::ui::render(&app, Region::Dialog).contains("The bridge file is missing."));
    drop(app.take_threads());
}

#[test]
fn other_launchers_say_why_the_launch_steps_cannot_be_read() {
    let backend = GamesBackend::new();
    *backend.bridge_error.lock().unwrap() = true;
    let mut app = app_on(&backend, Screen::Pc);
    let ac = index_of(&app, false, "ac");
    dispatch(&mut app, "action", &format!("PC:setup:{ac}"), "").unwrap();
    let html = crate::ui::render(&app, Region::Dialog);
    assert!(html.contains(">Other launchers<") && html.contains("<p class=\"warning-text\">i/o error</p>"), "{html}");
    assert!(!html.contains("readonly value="), "nothing to copy");
    drop(app.take_threads());
}

/// The game's UDP port box keeps a typed port within 1…49152; text that is no number keeps the port shown.
#[test]
fn the_game_port_box_keeps_a_typed_port_within_its_limits() {
    let backend = GamesBackend::new();
    let mut app = app_on(&backend, Screen::Console);
    let gt7 = index_of(&app, true, "gt7");
    dispatch(&mut app, "action", &format!("Console:setup:{gt7}"), "").unwrap();
    for (typed, shown) in [("70000", "49152"), ("0", "1"), ("-8", "1"), ("1e9", "49152.0"), ("", "49152.0")] {
        dispatch(&mut app, "dialog-field", "port", typed).unwrap();
        assert_eq!(setup(&app).port, shown, "{typed:?}");
    }
    dispatch(&mut app, "dialog-field", "port", "60000").unwrap();
    assert_eq!(setup(&app).port, "49152");
    assert!(crate::ui::render(&app, Region::Dialog).contains("value=\"49152\""));
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert_eq!(backend.calls_to(Method::Put, "games/gt7/port")[0].as_ref().unwrap()["port"], json!(49152));
    drop(app.take_threads());
}

/// The forwarding port box keeps a typed port within 1…49152, and a new target takes the port as the box shows it.
#[test]
fn the_forwarding_port_box_keeps_a_typed_port_within_its_limits() {
    let backend = GamesBackend::new();
    let mut app = app_on(&backend, Screen::Console);
    let gt7 = index_of(&app, true, "gt7");
    dispatch(&mut app, "action", &format!("Console:setup:{gt7}"), "").unwrap();
    assert!(crate::ui::render(&app, Region::Dialog)
        .contains("id=\"forward-port\" class=\"field-input\" type=\"number\" value=\"10000\" aria-label=\"Port\" data-act=\"dialog-field\" data-arg=\"fwd-port\" min=\"1\" max=\"49152\" step=\"1\">"));
    dispatch(&mut app, "dialog-field", "fwd-port", "99999").unwrap();
    assert_eq!(setup(&app).forward_port, "49152");
    dispatch(&mut app, "dialog-field", "fwd-port", "abc").unwrap();
    assert_eq!(setup(&app).forward_port, "49152", "text that is no number keeps the port");
    dispatch(&mut app, "dialog-button", "fwd-add", "").unwrap();
    assert_eq!(setup(&app).forwarding, ["127.0.0.1:20000", "127.0.0.1:49152"]);
    dispatch(&mut app, "dialog-field", "fwd-port", "0").unwrap();
    dispatch(&mut app, "dialog-button", "fwd-add", "").unwrap();
    assert_eq!(setup(&app).forwarding, ["127.0.0.1:20000", "127.0.0.1:49152", "127.0.0.1:1"]);
    drop(app.take_threads());
}

/// While a game runs, the forwarding list cannot be changed: Add and Delete are off, refused with a warning, and
/// the stored setting is not written.
#[test]
fn the_forwarding_list_is_locked_while_a_game_runs() {
    let backend = GamesBackend::new();
    let mut app = app_on(&backend, Screen::Console);
    let gt7 = index_of(&app, true, "gt7");
    backend.run_game("acc");
    dispatch(&mut app, "action", &format!("Console:setup:{gt7}"), "").unwrap();
    let html = crate::ui::render(&app, Region::Dialog);
    assert!(html.contains("data-arg=\"fwd-add\" disabled") && html.contains("data-arg=\"fwd-delete:0\" disabled"));
    assert!(html.contains("data-arg=\"fwd-port\" min=\"1\" max=\"49152\" step=\"1\" disabled"));
    assert!(html.contains("data-arg=\"fwd-ip\" autocomplete=\"off\" disabled"));
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "dialog-field", "fwd-ip", "192.168.1.20").unwrap();
    dispatch(&mut app, "dialog-field", "fwd-port", "20001").unwrap();
    dispatch(&mut app, "dialog-button", "fwd-add", "").unwrap();
    dispatch(&mut app, "dialog-button", "fwd-delete:0", "").unwrap();
    assert_eq!(setup(&app).forwarding, ["127.0.0.1:20000"]);
    assert_eq!((setup(&app).forward_ip.as_str(), setup(&app).forward_port.as_str()), ("127.0.0.1", "10000"));
    assert!(backend.calls_to(Method::Patch, "settings").is_empty(), "the stored list is not written");
    assert_eq!(last_warning(&app).1, LOCKED);
    drop(app.take_threads());
}

/// A game that needs configuring: when its patch fails or is refused, its port is still confirmed.
#[test]
fn a_failed_patch_still_confirms_the_port() {
    for failure in [(409, "A game is running, so OpenRigCue cannot set the game up."), (400, "configuring failed")] {
        let backend = GamesBackend::new();
        let mut app = app_on(&backend, Screen::Pc);
        let f1 = index_of(&app, false, "f12020");
        dispatch(&mut app, "action", &format!("PC:setup:{f1}"), "").unwrap();
        dispatch(&mut app, "dialog-field", "port", "20800").unwrap();
        *backend.patch_failure.lock().unwrap() = Some((failure.0, failure.1.into()));
        dispatch(&mut app, "dialog-ok", "", "").unwrap();
        assert_eq!(patches_of(&backend, "games/f12020/patch"), 1);
        assert_eq!(
            backend.calls_to(Method::Put, "games/f12020/port"),
            vec![Some(json!({"port": 20800, "forwarding": []}))],
            "{failure:?}"
        );
        assert!(warnings(&app).iter().any(|(t, _)| t == "Restart"), "{failure:?}");
        assert!(app.ui.dialog.is_some(), "{failure:?}: the dialog stays to apply the patch again");
        drop(app.take_threads());
    }
}

#[test]
fn the_search_matches_names_only_and_is_not_trimmed() {
    let backend = GamesBackend::new();
    let mut app = app_on(&backend, Screen::Pc);
    let names = |app: &MyApp| -> Vec<String> { filtered(app, false).iter().map(|(_, g)| g.name.clone()).collect() };
    let every = names(&app).len();
    // Only spaces: every game.
    dispatch(&mut app, "set", "PC:search", "   ").unwrap();
    assert_eq!(names(&app).len(), every);
    // A status text or the status's shown text matches nothing.
    for status in ["config", "Config", "not installed"] {
        dispatch(&mut app, "set", "PC:search", status).unwrap();
        assert!(names(&app).is_empty(), "{status}");
    }
    // The text as typed, spaces included.
    dispatch(&mut app, "set", "PC:search", "2020 ").unwrap();
    assert!(names(&app).is_empty(), "no name has a space after 2020");
    dispatch(&mut app, "set", "PC:search", " 2020").unwrap();
    assert_eq!(names(&app), ["F1 2020"]);
    dispatch(&mut app, "set", "PC:search", "1 2").unwrap();
    assert_eq!(names(&app), ["F1 2020"]);
    drop(app.take_threads());
}

#[test]
fn the_playstation_address_is_checked_as_typed() {
    let backend = GamesBackend::new();
    let mut app = app_on(&backend, Screen::Console);
    dispatch(&mut app, "set", "Console:playstation_ip", " 192.168.1.90 ").unwrap();
    assert!(warnings(&app).iter().any(|(_, m)| m == " 192.168.1.90  is not a correctly formed IP address"));
    assert!(backend.calls_to(Method::Patch, "settings").is_empty(), "surrounding spaces are not removed");
    dispatch(&mut app, "set", "Console:playstation_ip", " ").unwrap();
    assert!(
        warnings(&app).iter().any(|(_, m)| m == "  is not a correctly formed IP address"),
        "a lone space is not an empty address"
    );
    assert!(backend.calls_to(Method::Patch, "settings").is_empty());
    drop(app.take_threads());
}
