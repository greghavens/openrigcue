//! Games → PC and Games → Console: the service's game list (searchable, six per page), each game's Setup
//! dialog (the patch that configures it, its set-up instructions, its UDP port with the port forwarding list,
//! and the launch steps for launchers other than Steam), and on Console the set-up steps, the PlayStation
//! address and the Auto / Manual game detection with the game chosen in Manual mode.
//!
//! The list comes from `GET games?page=PC|Console`; a game is configured with `POST games/{key}/patch`, its
//! port and forwarding are confirmed with `PUT games/{key}/port`, its instructions are `GET
//! games/{key}/instructions?console=`, the bridge launch steps `GET games/bridge-path` and the forced game
//! `PUT games/detection`.

use openrigcue_model::native_value::LegacyValue;
use openrigcue_model::python_compare::native_equal;
use serde_json::{json, Value};

use super::super::components;
use super::super::dialogs_hardware::{self as dh, Dialog as Shared};
use super::super::html::{esc, t, tf, tr};
use super::super::notices::{self, Severity};
use super::super::{read, themes, Region, Screen};
use crate::app::MyApp;
use crate::backend::{native_int, segment, text, BackendExt};

/// The games shown per page.
pub const PAGE: usize = 6;

/// One game of the service's list.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Game {
    pub key: String,
    pub name: String,
    /// `not_installed`, `ready_to_play`, `instructions` or `config`.
    pub status: String,
    /// The entry's kind (`config` or `instructions`), which decides the port hint.
    pub entry_type: String,
    /// The `NETWORK_PORT` key of the game's UDP port, when it has one.
    pub port_key: Option<String>,
    /// The game sends telemetry only when started through the bridge.
    pub bridge: bool,
}

/// One of the two lists (PC, Console) with its search text and page.
#[derive(Debug, Default)]
pub struct List {
    pub games: Vec<Game>,
    pub loaded: bool,
    pub filter: String,
    pub page: usize,
}

/// What the screen keeps between passes (`app.ui.devices`).
#[derive(Debug, Default)]
pub struct State {
    pub pc: List,
    pub console: List,
    /// Console game detection: Manual (a chosen game is forced) or Auto.
    pub manual: bool,
    /// The Console game chosen in Manual mode (its list key).
    pub chosen: Option<String>,
}

impl State {
    fn list(&self, console: bool) -> &List {
        if console {
            &self.console
        } else {
            &self.pc
        }
    }

    fn list_mut(&mut self, console: bool) -> &mut List {
        if console {
            &mut self.console
        } else {
            &mut self.pc
        }
    }
}

/// A game's Setup dialog.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Setup {
    pub console: bool,
    pub game: Game,
    /// The set-up instructions (the service's HTML), or why they could not be read.
    pub instructions: Option<Result<String, String>>,
    /// A game ran when the dialog opened: the port cannot be changed.
    pub locked: bool,
    /// The UDP port field: the text its number box shows (read as a whole number when the setup is applied).
    pub port: String,
    /// The forwarding targets (`ip:port`).
    pub forwarding: Vec<String>,
    /// The forwarding list is the game's stored setting, so each change is stored at once.
    pub forwarding_stored: bool,
    pub forward_ip: String,
    /// The forwarding port field: the text its number box shows, which a new target takes as it is.
    pub forward_port: String,
    pub forwarding_open: bool,
    /// The launch steps for other launchers (the service's answer), or why they could not be read.
    pub bridge: Option<Result<Value, String>>,
    pub bridge_open: bool,
}

/// Why a game's port and forwarding list cannot be changed while a game runs.
const LOCKED: &str = "Game port settings are locked while a profile is in use. Exit the game, then try again";

/// The dialogs of this screen.
#[derive(Debug, Clone, PartialEq)]
pub enum Dialog {
    Setup(Box<Setup>),
}

fn console(screen: Screen) -> bool {
    screen == Screen::Console
}

fn mode_name(console: bool) -> &'static str {
    if console {
        "Console"
    } else {
        "PC"
    }
}

/// The status with spaces for underscores, capitalized; `ready_to_play` reads "Game is ready".
pub fn status_text(status: &str) -> String {
    if status == "ready_to_play" {
        return "Game is ready".to_owned();
    }
    let words = status.replace('_', " ");
    let mut chars = words.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars.flat_map(char::to_lowercase)).collect(),
        None => String::new(),
    }
}

/// Whether a game has anything to set up (Setup is enabled).
fn configurable(game: &Game) -> bool {
    matches!(game.status.as_str(), "instructions" | "config") || game.port_key.is_some() || game.bridge
}

/// Whether a game is running (no change of the game set-up is allowed then).
fn game_running(app: &MyApp) -> Result<bool, String> {
    let game = app.backend.game_value().map_err(|error| error.detail)?;
    Ok(!native_equal(&game, &LegacyValue::Str("none".into())))
}

/// Reads the list of a page from the service. When the service reports that a game's installed / patched check
/// failed, the games before it stay listed and the failure is shown as a warning.
pub fn load(app: &mut MyApp, console: bool) {
    let answer = app.backend.get(&format!("games?page={}", mode_name(console)));
    let (rows, error) = match answer {
        Ok(Value::Array(rows)) => (rows, None),
        Ok(other) => {
            let native = other
                .get("native_error")
                .and_then(|e| Some(format!("{}: {}", e.get("class_name")?.as_str()?, e.get("message")?.as_str()?)));
            let error = other.get("error").filter(|e| !e.is_null()).map(|e| native.unwrap_or_else(|| text(e)));
            (other.get("rows").and_then(Value::as_array).cloned().unwrap_or_default(), error)
        }
        Err(failure) => (Vec::new(), Some(failure.detail)),
    };
    let games = rows
        .iter()
        .map(|row| Game {
            key: text(&row["key"]),
            name: text(&row["name"]),
            status: text(&row["status"]),
            entry_type: text(&row["entry_type"]),
            port_key: row["port_key"].as_str().map(str::to_owned),
            bridge: row["bridge"].as_bool() == Some(true),
        })
        .collect();
    let list = app.ui.devices.games.list_mut(console);
    list.games = games;
    list.loaded = true;
    if let Some(error) = error {
        tracing::error!(%error, "game list");
        notices::warning(app, "Alert", &error);
    }
}

/// The games of a list whose name contains its search text (ignoring case; the text is not trimmed), in the
/// service's order. A search of nothing but spaces shows every game.
pub fn filtered(app: &MyApp, console: bool) -> Vec<(usize, &Game)> {
    let list = app.ui.devices.games.list(console);
    let every = list.filter.replace(' ', "").is_empty();
    let needle = list.filter.to_lowercase();
    list.games.iter().enumerate().filter(|(_, game)| every || game.name.to_lowercase().contains(&needle)).collect()
}

/// The body.
pub fn main(app: &MyApp) -> String {
    let console = console(app.ui.screen);
    let screen = mode_name(console);
    let mut body = String::new();
    if console {
        let device = if app.win32() { "PC" } else { "magicbox" };
        let lines = [
            format!("{}{device}{}", tr(app, "Connect your console and the "), tr(app, " to the same network.")),
            format!(
                "{}{device}{}{device}.",
                tr(app, "Where you can, wire both the console and the "),
                tr(app, " to your router with ETHERNET CABLES, as Wi-Fi can add lag. Never run an Ethernet cable straight from the console to the ")
            ),
            tr(app, "Set the game up on your console by following its instructions below."),
            tr(app, "Switch GAME DETECTION to MANUAL, then pick the game you are going to play from the list below"),
            tr(app, "Console games transmit telemetry only while the vehicle is moving."),
        ];
        let steps: String = lines.iter().map(|line| format!("<p>{}</p>", esc(line))).collect();
        body.push_str(&components::field(&t(app, "Instructions"), &steps, ""));
        body.push_str(&components::field(
            &t(app, "Playstation IP"),
            &format!(
                "<input id=\"console-playstation-ip\" class=\"field-input\" type=\"text\" value=\"{}\" aria-label=\"{}\" data-act=\"set\" data-arg=\"Console:playstation_ip\" autocomplete=\"off\">",
                esc(&text(app.setting(&["app", "playstation_address"]))),
                t(app, "Playstation IP")
            ),
            "",
        ));
        let options = vec![("0".to_owned(), tr(app, "Auto")), ("1".to_owned(), tr(app, "Manual"))];
        let selected = if app.ui.devices.games.manual { "1" } else { "0" };
        body.push_str(&components::field(
            &t(app, "Game Detection"),
            &components::select(
                "console-detection",
                &tr(app, "Game Detection"),
                &options,
                selected,
                "set",
                "Console:detection",
            ),
            "",
        ));
    }
    let list = app.ui.devices.games.list(console);
    body.push_str(&components::field(
        &t(app, "Find a game"),
        &format!(
            "<input id=\"games-search-{}\" class=\"field-input\" type=\"search\" value=\"{}\" placeholder=\"{}\" aria-label=\"{}\" data-act=\"set\" data-arg=\"{screen}:search\" autocomplete=\"off\">",
            screen.to_lowercase(),
            esc(&list.filter),
            t(app, "Search installed and supported games"),
            t(app, "Find a game")
        ),
        "",
    ));
    let games = filtered(app, console);
    let pages = games.len().div_ceil(PAGE).max(1);
    let page = list.page.min(pages - 1);
    let state = &app.ui.devices.games;
    for (index, game) in games.iter().skip(page * PAGE).take(PAGE) {
        let mut buttons = format!(
            "<button type=\"button\" data-act=\"action\" data-arg=\"{screen}:setup:{index}\"{}>{}</button>",
            if configurable(game) { "" } else { " disabled" },
            t(app, "Setup")
        );
        let chosen = console && state.manual && state.chosen.as_deref() == Some(game.key.as_str());
        if console && state.manual {
            buttons.push_str(&format!(
                "<button type=\"button\"{} data-act=\"action\" data-arg=\"Console:use:{index}\" aria-pressed=\"{chosen}\">{}</button>",
                if chosen { " class=\"primary\"" } else { "" },
                t(app, "Use this game")
            ));
        }
        let mut hint = t(app, &status_text(&game.status));
        if chosen {
            hint = format!("{hint} · {}", t(app, "Selected game"));
        }
        body.push_str(&format!(
            "<div class=\"control-row\" data-game=\"{}\"><div class=\"control-label\"><strong translate=\"no\">{}</strong></div><small>{hint}</small><div class=\"control-body action-row\">{buttons}</div></div>",
            esc(&game.key),
            esc(&game.name)
        ));
    }
    if games.is_empty() {
        body.push_str(&components::field(
            &t(app, "No matches"),
            &format!("<p>{}</p>", t(app, "Try another game name.")),
            "",
        ));
    } else {
        let start = page * PAGE;
        body.push_str(&format!(
            "<div class=\"device pager\"><button type=\"button\" data-act=\"action\" data-arg=\"{screen}:page-prev:\"{}>{}</button><small>{}–{} / {}</small><button type=\"button\" data-act=\"action\" data-arg=\"{screen}:page-next:\"{}>{}</button></div>",
            if page == 0 { " disabled" } else { "" },
            t(app, "Previous"),
            start + 1,
            (start + PAGE).min(games.len()),
            games.len(),
            if page + 1 >= pages { " disabled" } else { "" },
            t(app, "Next")
        ));
    }
    let reload = format!(
        "<button type=\"button\" data-act=\"action\" data-arg=\"{screen}:reload:\">{}</button>",
        t(app, "Reload games")
    );
    body.push_str(&components::actions(app, &reload, &format!("games-{}", screen.to_lowercase())));
    components::panel(&themes::title(app, if console { "Console games" } else { "PC games" }), "", &body, "")
}

/// Work when the screen opens: the list is read again.
pub fn on_enter(app: &mut MyApp, screen: Screen) {
    load(app, console(screen));
}

/// A value of the screen changed: the search text, the PlayStation address or the game detection.
pub fn set(app: &mut MyApp, screen: Screen, control: &str, part: &str, value: &str) -> Result<(), String> {
    let console = console(screen);
    match (control, part, console) {
        ("search", "", _) => {
            let list = app.ui.devices.games.list_mut(console);
            list.filter = value.to_owned();
            list.page = 0;
            Ok(())
        }
        ("playstation_ip", "", true) => playstation_ip(app, value),
        ("detection", "", true) => detection(app, value == "1"),
        _ => Err(format!("no {} value {control:?}", mode_name(console))),
    }
}

/// The PlayStation address: an empty value or an IP address is stored; anything else is refused with a warning.
pub fn playstation_ip(app: &mut MyApp, value: &str) -> Result<(), String> {
    if !value.is_empty() && !is_ip_address(value) {
        let message = format!("{value}{}", tr(app, " is not a correctly formed IP address"));
        notices::warning(app, "Alert", &message);
        return Ok(());
    }
    app.backend
        .assign_setting_value("APP", "playstation_ip", &LegacyValue::Str(value.into()))
        .map_err(|error| error.detail)?;
    if let Ok(settings) = app.backend.get("settings") {
        app.docs.settings = settings;
    }
    Ok(())
}

/// Game detection: Manual chooses the first listed game (Stand By on Console); Auto clears the choice and
/// forces no game.
pub fn detection(app: &mut MyApp, manual: bool) -> Result<(), String> {
    if manual == app.ui.devices.games.manual {
        return Ok(());
    }
    app.ui.devices.games.manual = manual;
    if manual {
        if !app.ui.devices.games.console.loaded {
            load(app, true);
        }
        // Choosing the first game cannot fail the switch itself.
        if !app.ui.devices.games.console.games.is_empty() {
            if let Err(error) = choose(app, 0) {
                tracing::warn!(error, "first game of manual detection");
            }
        }
        Ok(())
    } else {
        app.ui.devices.games.chosen = None;
        force(app, None)
    }
}

/// Forces the detected game: an entry key, or none.
fn force(app: &mut MyApp, entry: Option<&str>) -> Result<(), String> {
    app.backend.put("games/detection", json!({ "entry": entry })).map(|_| ()).map_err(|error| error.detail)
}

/// The Console game at `index` of the list is chosen in Manual mode: it is marked and forced (the first entry
/// in name order with its name; Stand By forces none).
pub fn choose(app: &mut MyApp, index: usize) -> Result<(), String> {
    let game = app.ui.devices.games.console.games.get(index).cloned().ok_or("IndexError: no such game")?;
    app.ui.devices.games.chosen = Some(game.key.clone());
    let mut entries: Vec<_> = openrigcue_config::patch::GAME_ENTRIES.iter().collect();
    entries.sort_by_key(|entry| entry.name.to_uppercase());
    let key = entries
        .into_iter()
        .find(|entry| entry.name == game.name)
        .map(|entry| entry.key)
        .ok_or("UnboundLocalError: cannot access local variable 'game' where it is not associated with a value")?;
    if key.split('_').next() == Some("testmode") {
        force(app, None)
    } else {
        force(app, Some(key))
    }
}

/// A button of the screen: Setup or Use this game of a listed game, the pages and Reload games.
pub fn action(app: &mut MyApp, screen: Screen, control: &str, index: Option<usize>) -> Result<(), String> {
    let console = console(screen);
    match control {
        "setup" => open_setup(app, console, index.ok_or("no game")?),
        "use" if console => {
            if !app.ui.devices.games.manual {
                return Ok(());
            }
            choose(app, index.ok_or("no game")?)
        }
        "page-prev" | "page-next" => {
            let pages = filtered(app, console).len().div_ceil(PAGE).max(1);
            let list = app.ui.devices.games.list_mut(console);
            let page = list.page.min(pages - 1);
            list.page = if control == "page-next" { (page + 1).min(pages - 1) } else { page.saturating_sub(1) };
            Ok(())
        }
        "reload" => {
            load(app, console);
            Ok(())
        }
        _ => Err(format!("no {} button {control:?}", mode_name(console))),
    }
}

/// Opens a game's Setup dialog: its instructions, its port (locked while a game runs, with a warning) and its
/// forwarding list, and the launch steps for other launchers.
pub fn open_setup(app: &mut MyApp, console: bool, index: usize) -> Result<(), String> {
    let game = app.ui.devices.games.list(console).games.get(index).cloned().ok_or("IndexError: no such game")?;
    let mut setup = Setup { console, forward_ip: "127.0.0.1".into(), forward_port: "10000".into(), ..Setup::default() };
    if game.status == "instructions" {
        setup.instructions = Some(
            app.backend
                .fetch(&format!("games/{}/instructions?console={console}", segment(&game.key)))
                .map(|(bytes, _)| String::from_utf8_lossy(&bytes).into_owned())
                .map_err(|error| error.detail),
        );
    }
    if let Some(port_key) = &game.port_key {
        setup.locked = game_running(app)?;
        if setup.locked {
            notices::warning(
                app,
                "Alert",
                &tr(app, "Game port settings are locked while a profile is in use. Exit the game, then try again"),
            );
        }
        let port = app.backend.native_setting("NETWORK_PORT", port_key).map_err(|error| error.detail)?;
        setup.port = native_int(&port)?.to_string();
        let configured = app.backend.native_setting("PORT_FORWARDING", port_key).ok();
        setup.forwarding_stored = configured.is_some();
        setup.forwarding = configured.map(|value| forwarding_items(&value)).transpose()?.unwrap_or_default();
    }
    if game.bridge {
        setup.bridge = Some(app.backend.get("games/bridge-path").map_err(|error| error.detail));
    }
    setup.game = game;
    dh::open(app, Shared::Games(Dialog::Setup(Box::new(setup))));
    Ok(())
}

/// The targets of a stored forwarding list.
fn forwarding_items(value: &LegacyValue) -> Result<Vec<String>, String> {
    match value {
        LegacyValue::List(values) | LegacyValue::Tuple(values) | LegacyValue::Set(values) => values
            .iter()
            .map(|value| match value {
                // Each target is `address:port`.
                LegacyValue::Str(item) if item.contains(':') => Ok(item.clone()),
                LegacyValue::Str(_) => Err("IndexError: list index out of range".to_owned()),
                other => Err(format!("AttributeError: '{}' object has no attribute 'split'", other.type_name())),
            })
            .collect(),
        other => Err(format!("TypeError: '{}' object is not iterable", other.type_name())),
    }
}

fn forwarding_value(items: &[String]) -> LegacyValue {
    LegacyValue::List(items.iter().cloned().map(LegacyValue::Str).collect())
}

/// The Setup dialog's markup.
pub fn dialog_render(app: &MyApp, dialog: &Dialog) -> String {
    let Dialog::Setup(setup) = dialog;
    let game = &setup.game;
    let mut body = String::new();
    let configuration = match (game.status.as_str(), &setup.instructions) {
        ("instructions", Some(Ok(html))) => format!("<div class=\"dialog-text instructions\">{html}</div>"),
        ("instructions", Some(Err(error))) => format!("<p class=\"warning-text\">{}</p>", esc(error)),
        ("config", _) => {
            format!("<p>{}</p>", t(app, "Apply setup configures the game to send its telemetry to OpenRigCue."))
        }
        (status, _) => format!("<p>{}</p>", t(app, &status_text(status))),
    };
    body.push_str(&components::field(&t(app, "Configuration"), &configuration, &t(app, &status_text(&game.status))));
    if game.port_key.is_some() {
        let hint = if game.entry_type == "instructions" {
            t(app, "Once OpenRigCue has restarted, go through the instructions again to set the game up anew")
        } else {
            String::new()
        };
        // While a game runs, neither the port nor the forwarding list can be changed.
        let locked = if setup.locked { " disabled" } else { "" };
        let mut port = dh::input(
            "game-port",
            &tr(app, "UDP port"),
            "port",
            "number",
            &setup.port,
            &format!("{}{locked}", dh::PORT.attributes()),
        );
        if setup.locked {
            port.push_str(&format!(
                "<p class=\"warning-text\">{}</p>",
                t(app, "Game port settings are locked while a profile is in use. Exit the game, then try again")
            ));
        }
        body.push_str(&components::field(&t(app, "UDP port"), &port, &hint));
        let rows: String = setup
            .forwarding
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let (ip, port) = item.split_once(':').unwrap_or((item.as_str(), ""));
                format!(
                    "<tr><th scope=\"row\" class=\"mono\">{}</th><td class=\"mono\">{}</td><td>{}</td></tr>",
                    esc(ip),
                    esc(port),
                    dh::button(&t(app, "Delete"), &format!("fwd-delete:{index}"), "", !setup.locked)
                )
            })
            .collect();
        let table = if setup.forwarding.is_empty() {
            String::new()
        } else {
            format!(
                "<table class=\"data-table\"><thead><tr><th>{}</th><th>{}</th><th>{}</th></tr></thead><tbody>{rows}</tbody></table>",
                t(app, "IP"),
                t(app, "Port"),
                t(app, "Delete")
            )
        };
        let add = format!(
            "<div class=\"action-row\">{}{}{}</div>",
            dh::input(
                "forward-ip",
                &tr(app, "IP"),
                "fwd-ip",
                "text",
                &setup.forward_ip,
                &format!(" autocomplete=\"off\"{locked}")
            ),
            dh::input(
                "forward-port",
                &tr(app, "Port"),
                "fwd-port",
                "number",
                &setup.forward_port,
                &format!("{}{locked}", dh::PORT.attributes())
            ),
            dh::button(&t(app, "Add"), "fwd-add", "", !setup.locked)
        );
        body.push_str(&format!(
            "<details class=\"options\" data-act=\"dialog-field\" data-arg=\"open-forwarding\"{}><summary>{} · {}</summary><div class=\"options-body\">{add}</div>{table}</details>",
            if setup.forwarding_open { " open" } else { "" },
            t(app, "Port Forwarding"),
            setup.forwarding.len()
        ));
    }
    if let Some(bridge) = &setup.bridge {
        let content = match bridge {
            Err(error) => format!("<p class=\"warning-text\">{}</p>", esc(error)),
            Ok(answer) => match answer.get("searched").and_then(Value::as_array) {
                Some(searched) => {
                    let folders = searched.iter().map(text).collect::<Vec<_>>().join(", ");
                    format!(
                        "<p class=\"warning-text\">{}</p>",
                        tf(app, "The bridge files were not found in ^folders^", &[("folders", &folders)])
                    )
                }
                None => {
                    let mut out = format!("<p>{}</p>", t(app, "This game sends telemetry only when it is started through the OpenRigCue bridge. For a game in your Steam library the Config button sets that up. For a game started by another launcher, copy the launch option below into that launcher's settings for the game."));
                    for (label, key) in
                        [("Bridge", "bridge"), ("Launch helper", "launch_helper"), ("Launch option", "launch_option")]
                    {
                        out.push_str(&components::field(
                            &t(app, label),
                            &format!(
                                "<input class=\"field-input mono\" type=\"text\" readonly value=\"{}\" aria-label=\"{}\" translate=\"no\">",
                                esc(&text(&answer[key])),
                                t(app, label)
                            ),
                            "",
                        ));
                    }
                    if answer["bridge_present"].as_bool() == Some(false) {
                        out.push_str(&format!(
                            "<p class=\"warning-text\">{}</p>",
                            t(app, "The bridge file is missing.")
                        ));
                    }
                    out.push_str(&format!(
                        "<p>{}</p>",
                        tf(app, "^option^ is where the launcher puts its own command for the game; a launcher with a wrapper or command prefix setting takes the launch helper alone. The launch helper adds the bridge to a game started by Proton; a game started any other way starts without the bridge.", &[("option", "%command%")])
                    ));
                    out
                }
            },
        };
        body.push_str(&format!(
            "<details class=\"options\" data-act=\"dialog-field\" data-arg=\"open-bridge\"{}><summary>{}</summary><div class=\"options-body\"><div>{content}</div></div></details>",
            if setup.bridge_open { " open" } else { "" },
            t(app, "Other launchers")
        ));
    }
    let title = tf(app, "^game^ — ^mode^ setup", &[("game", &game.name), ("mode", &tr(app, mode_name(setup.console)))]);
    let confirm = game.status == "config" || game.port_key.is_some();
    let label = t(app, "Apply setup");
    dh::frame(app, &title, &body, confirm.then_some((label.as_str(), true)))
}

/// An action of the Setup dialog.
pub fn dialog_act(app: &mut MyApp, dialog: Dialog, act: &str, arg: &str, value: &str) -> Result<(), String> {
    let Dialog::Setup(mut setup) = dialog;
    match act {
        "dialog-close" => super::super::dialogs::close(app),
        "dialog-field" => {
            match arg {
                // The ports' number boxes keep a typed port within 1…49152; nothing changes while a game runs.
                "port" | "fwd-ip" | "fwd-port" if setup.locked => return Ok(()),
                "port" => setup.port = dh::PORT.take_text(value, &setup.port)?,
                "fwd-ip" => setup.forward_ip = value.trim().to_owned(),
                "fwd-port" => setup.forward_port = dh::PORT.take_text(value, &setup.forward_port)?,
                "open-forwarding" => setup.forwarding_open = read::checked(value),
                "open-bridge" => setup.bridge_open = read::checked(value),
                _ => return Ok(()),
            }
            dh::update(app, Shared::Games(Dialog::Setup(setup)));
        }
        "dialog-button" => {
            if setup.locked && (arg == "fwd-add" || arg.starts_with("fwd-delete:")) {
                // The forwarding list cannot be changed while a game runs.
                notices::warning(app, "Alert", &tr(app, LOCKED));
            } else if arg == "fwd-add" {
                add_forwarding(app, &mut setup)?;
            } else if let Some(index) = arg.strip_prefix("fwd-delete:") {
                let index: usize = index.parse().map_err(|_| "bad forwarding row")?;
                if index < setup.forwarding.len() {
                    setup.forwarding.remove(index);
                    store_forwarding(app, &setup)?;
                }
            }
            setup.forwarding_open = true;
            dh::update(app, Shared::Games(Dialog::Setup(setup)));
        }
        "dialog-ok" => apply(app, &setup)?,
        _ => {}
    }
    Ok(())
}

/// Adds the forwarding target of the IP and port fields: the address must be an IP address and the target new.
fn add_forwarding(app: &mut MyApp, setup: &mut Setup) -> Result<(), String> {
    let ip = setup.forward_ip.clone();
    if !is_ip_address(&ip) {
        let message = format!("{ip}{}", tr(app, " is not a correctly formed IP address"));
        notices::warning(app, "Alert", &message);
        return Ok(());
    }
    let target = format!("{ip}:{}", setup.forward_port);
    if setup.forwarding.contains(&target) {
        notices::warning(app, "Alert", &tr(app, "Forwarding to this IP and Port is already set up"));
        return Ok(());
    }
    setup.forwarding.push(target);
    store_forwarding(app, setup)
}

/// A forwarding list that is the game's stored setting changes it at once.
fn store_forwarding(app: &mut MyApp, setup: &Setup) -> Result<(), String> {
    let Some(port_key) = setup.game.port_key.as_deref() else { return Ok(()) };
    if setup.forwarding_stored {
        app.backend
            .assign_setting_value("PORT_FORWARDING", port_key, &forwarding_value(&setup.forwarding))
            .map_err(|error| error.detail)?;
        if let Ok(settings) = app.backend.get("settings") {
            app.docs.settings = settings;
        }
    }
    Ok(())
}

/// Apply setup: refused while a game runs; a game that needs configuring is patched (a refusal of the service
/// says why), then the port and forwarding are confirmed, also when the patch failed. A changed port needs a
/// restart, which a warning says; the list is read again.
fn apply(app: &mut MyApp, setup: &Setup) -> Result<(), String> {
    let game = &setup.game;
    if game_running(app)? {
        let (title, message) = if game.status == "config" {
            ("Game Running", "A game is running, so OpenRigCue cannot set the game up. Close the game, then try again.")
        } else {
            ("Alert", "Game port settings are locked while a profile is in use. Exit the game, then try again")
        };
        notices::warning(app, title, &tr(app, message));
        return Ok(());
    }
    let mut reload = false;
    // The patch and the port are set up apart: a patch that fails or is refused does not keep the port from
    // being confirmed, and the dialog stays open to apply the patch again.
    let mut patched = true;
    if game.status == "config" {
        match app.backend.post(&format!("games/{}/patch", segment(&game.key)), Value::Null) {
            Ok(_) => {
                notices::confirmation(app, "Game setup saved", "Game is ready");
                reload = true;
            }
            Err(refused) if refused.status == 409 => {
                let title = if refused.detail.contains("Steam") { "Steam Running" } else { "Game Running" };
                notices::warning(app, title, &refused.detail);
                patched = false;
            }
            Err(failure) => {
                let notice =
                    notices::notice(Severity::Error, &game.name, &format!("{}: {}", tr(app, "Error"), failure.detail));
                notices::raise(app, notice);
                patched = false;
                reload = true;
            }
        }
    }
    if game.port_key.is_some() && !setup.locked {
        // The port box's text as a whole number (a fraction is refused).
        let port = native_int(&LegacyValue::Str(setup.port.clone()))?;
        let restart = app
            .backend
            .confirm_game_port(&game.key, port, &forwarding_value(&setup.forwarding))
            .map_err(|error| error.detail)?;
        if restart {
            reload = true;
            notices::warning(app, "Restart", &tr(app, "THIS CHANGE TAKES EFFECT AFTER OpenRigCue IS RESTARTED"));
        }
    }
    if !patched {
        if reload {
            load(app, setup.console);
        }
        return Ok(());
    }
    super::super::dialogs::close(app);
    if reload {
        load(app, setup.console);
    }
    super::super::refresh(app, &[Region::Main]);
    Ok(())
}

/// Once, before the main loop's first pass: both lists are read; with Stand By listed on Console, no game is
/// forced (the detection starts in Auto).
pub fn start(app: &mut MyApp) -> Result<(), String> {
    load(app, false);
    load(app, true);
    if app.ui.devices.games.console.games.iter().any(|game| game.key == "testmode") {
        if let Err(error) = force(app, None) {
            tracing::warn!(error, "game detection at start");
        }
    }
    Ok(())
}

/// Each main-loop pass: nothing (the lists are read when a screen opens or on Reload games).
pub fn rules(app: &mut MyApp) -> Result<(), String> {
    let _ = app;
    Ok(())
}

/// Whether `text` is a valid IPv4 or IPv6 address (the same rules as the service's check).
pub fn is_ip_address(text: &str) -> bool {
    is_ipv4(text) || is_ipv6(text)
}

/// A strict IPv4 address: four decimal octets of 1–3 digits, no leading zeros, each ≤ 255.
fn is_ipv4(text: &str) -> bool {
    let octets: Vec<&str> = text.split('.').collect();
    octets.len() == 4
        && octets.iter().all(|octet| {
            !octet.is_empty()
                && octet.len() <= 3
                && octet.bytes().all(|b| b.is_ascii_digit())
                && !(octet.len() > 1 && octet.starts_with('0'))
                && octet.parse::<u16>().is_ok_and(|value| value <= 255)
        })
}

/// An IPv6 address (no `/`; a `%scope` suffix is split off).
fn is_ipv6(text: &str) -> bool {
    if text.contains('/') {
        return false;
    }
    let address = match text.split_once('%') {
        Some((address, scope)) => {
            if scope.is_empty() || scope.contains('%') {
                return false;
            }
            address
        }
        None => text,
    };
    if address.is_empty() {
        return false;
    }
    let mut parts: Vec<String> = address.split(':').map(str::to_owned).collect();
    if parts.len() < 3 {
        return false;
    }
    if parts.last().is_some_and(|last| last.contains('.')) {
        let tail = parts.pop().unwrap_or_default();
        if !is_ipv4(&tail) {
            return false;
        }
        parts.push("0".to_owned());
        parts.push("0".to_owned());
    }
    const HEXTETS: usize = 8;
    if parts.len() > HEXTETS + 1 {
        return false;
    }
    let inner = &parts[1..parts.len() - 1];
    let skips: Vec<usize> = inner.iter().enumerate().filter(|(_, p)| p.is_empty()).map(|(i, _)| i + 1).collect();
    if skips.len() > 1 {
        return false;
    }
    let (high, low) = match skips.first() {
        Some(&skip) => {
            let mut high = skip;
            let mut low = parts.len() - skip - 1;
            if parts[0].is_empty() {
                high -= 1;
                if high != 0 {
                    return false;
                }
            }
            if parts[parts.len() - 1].is_empty() {
                low -= 1;
                if low != 0 {
                    return false;
                }
            }
            if HEXTETS < high + low + 1 {
                return false;
            }
            (high, low)
        }
        None => {
            if parts.len() != HEXTETS || parts[0].is_empty() || parts[HEXTETS - 1].is_empty() {
                return false;
            }
            (parts.len(), 0)
        }
    };
    let hextet = |part: &String| !part.is_empty() && part.len() <= 4 && part.bytes().all(|b| b.is_ascii_hexdigit());
    parts[..high].iter().all(hextet) && parts[parts.len() - low..].iter().all(hextet)
}

#[cfg(test)]
#[path = "games_tests.rs"]
mod tests;
