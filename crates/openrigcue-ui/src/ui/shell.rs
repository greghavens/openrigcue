//! The parts of the frame every theme builds from: navigation items, the brand, the top-bar tools, the
//! language and search menus, the Active profile select, the device rows and the phone navigation.

use super::html::{esc, icon, t, tf, tr};
use super::session::{self, State};
use super::{Group, Popup, Screen};
use crate::app::MyApp;

/// The four navigation items; the current section's is active.
pub fn nav_items(app: &MyApp) -> String {
    let current = app.ui.screen.group();
    Group::ALL
        .iter()
        .map(|group| {
            let active = *group == current;
            format!(
                "<button type=\"button\" class=\"nav-item{}\" data-act=\"nav\" data-arg=\"{}\"{}>{}<span>{}</span></button>",
                if active { " active" } else { "" },
                group.key(),
                if active { " aria-current=\"page\"" } else { "" },
                icon(group.icon()),
                t(app, group.key())
            )
        })
        .collect()
}

/// The brand: the OpenRigCue mark and name.
pub fn brand() -> String {
    "<div class=\"brand\" translate=\"no\"><span class=\"brand-mark\"><img src=\"/res:images/openrigcue/mark.svg\" alt=\"\"></span><span>OpenRigCue<span class=\"brand-dot\">.</span></span></div>".into()
}

/// The status dot class of the session state.
pub fn state_dot(state: State) -> &'static str {
    match state {
        State::Telemetry => "status-dot",
        State::GameRunning | State::Editing => "status-dot wait",
        State::Standby => "status-dot idle",
    }
}

/// The top bar's tools: search, Activity with its count, and the language switcher.
pub fn top_actions(app: &MyApp) -> String {
    let count = app.ui.notices.history.len();
    let code = esc(&app.ui.language);
    format!(
        "<div class=\"top-actions\"><button type=\"button\" class=\"quiet search-button\" id=\"search-button\" aria-label=\"{search}\" title=\"{search}\" aria-haspopup=\"dialog\" aria-expanded=\"{search_open}\" data-act=\"popup\" data-arg=\"search\">{search_icon}</button><button type=\"button\" class=\"activity-button\" aria-expanded=\"{drawer}\" aria-controls=\"activity-drawer\" data-act=\"drawer\">{activity_icon} {activity} <b id=\"event-count\">{count}</b></button><button type=\"button\" id=\"language-switcher\" class=\"quiet language-button\" aria-label=\"{language}\" aria-haspopup=\"menu\" aria-expanded=\"{language_open}\" aria-controls=\"language-menu\" translate=\"no\" data-act=\"popup\" data-arg=\"language\"><span>{code}</span><svg viewBox=\"0 0 12 12\" fill=\"none\" stroke=\"currentColor\" aria-hidden=\"true\"><path d=\"m3 4 3 3 3-3\"/></svg></button></div>",
        search = t(app, "Search"),
        search_open = app.ui.popup == Some(Popup::Search),
        search_icon = icon("search"),
        drawer = app.ui.drawer,
        activity_icon = icon("activity"),
        activity = t(app, "Activity"),
        language = t(app, "Language"),
        language_open = app.ui.popup == Some(Popup::Language),
    )
}

/// The languages: code and own name.
pub const LANGUAGES: [(&str, &str); 8] = [
    ("EN", "English"),
    ("ES", "Español"),
    ("IT", "Italiano"),
    ("FR", "Français"),
    ("PT", "Português"),
    ("DE", "Deutsch"),
    ("JA", "日本語"),
    ("ZH", "中文"),
];

/// The open menu: the language menu or the search.
pub fn popup(app: &MyApp) -> String {
    match app.ui.popup {
        Some(Popup::Language) => {
            let items: String = LANGUAGES
                .iter()
                .map(|(code, name)| {
                    format!(
                        "<button type=\"button\" role=\"menuitemradio\" aria-checked=\"{}\" data-act=\"language\" data-arg=\"{code}\" lang=\"{}\"><span>{name}</span><small>{code}</small></button>",
                        app.ui.language == *code,
                        code.to_lowercase()
                    )
                })
                .collect();
            format!(
                "<div id=\"language-menu\" class=\"language-menu\" role=\"menu\" aria-label=\"{}\" translate=\"no\" data-anchor=\"language-switcher\">{items}</div>",
                t(app, "Language")
            )
        }
        Some(Popup::Search) => {
            let results = search_results(app);
            let list: String = if app.ui.search.chars().count() < 3 {
                format!("<p class=\"search-hint\">{}</p>", t(app, "Enter 3 or more characters"))
            } else if results.is_empty() {
                format!("<p class=\"search-hint\">{}</p>", t(app, "No matches"))
            } else {
                results
                    .iter()
                    .map(|result| {
                        format!(
                            "<button type=\"button\" role=\"option\" data-act=\"search-go\" data-arg=\"{}:{}{}\"><span>{}</span><small>{}</small></button>",
                            result.screen.key(),
                            result.section,
                            result.games_page.map(|page| format!(":{page}")).unwrap_or_default(),
                            esc(&result.text),
                            esc(&result.place)
                        )
                    })
                    .collect()
            };
            format!(
                "<div id=\"search-menu\" class=\"language-menu search-menu\" role=\"dialog\" aria-label=\"{label}\" data-anchor=\"search-button\"><input id=\"search-input\" class=\"field-input\" type=\"search\" value=\"{value}\" placeholder=\"{placeholder}\" aria-label=\"{label}\" data-act=\"search\" data-live autocomplete=\"off\" data-autofocus><div class=\"search-results\" role=\"listbox\">{list}</div></div>",
                label = t(app, "Search"),
                value = esc(&app.ui.search),
                placeholder = t(app, "search OpenRigCue"),
            )
        }
        None => String::new(),
    }
}

/// One text the search can find: the text, where it is, and the screen and settings group that show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub text: String,
    pub place: String,
    pub screen: Screen,
    pub section: usize,
    /// The page of the Games list (PC, Console) that lists the game found.
    pub games_page: Option<usize>,
}

/// Every text of the index that contains the search (three or more characters), all of them, in screen order.
pub fn search_results(app: &MyApp) -> Vec<SearchResult> {
    let needle = app.ui.search.trim().to_lowercase();
    if needle.chars().count() < 3 {
        return Vec::new();
    }
    app.ui
        .search_index
        .iter()
        .flatten()
        .filter(|result| result.text.to_lowercase().contains(&needle))
        .cloned()
        .collect()
}

/// The texts the search looks through, in the selected language: for every offered screen (the start screen
/// aside) its title, every text and text field each of its settings groups shows (headings, labels, notes,
/// buttons, table rows such as the telemetry channels), the groups' titles, the labels of every control of
/// its description (rows hidden now included) and, on PC and Console, every game of the list with its
/// status. Choices of a select are not searched. The Games lists are read first when they were not yet.
pub fn search_index(app: &mut MyApp) -> Vec<SearchResult> {
    use super::screens::games;
    if !app.ui.devices.games.pc.loaded {
        games::load(app, false);
    }
    if !app.ui.devices.games.console.loaded {
        games::load(app, true);
    }
    let (screen, section) = (app.ui.screen, app.ui.section);
    let mut index = Index::default();
    for target in Screen::ALL {
        if target != Screen::Home && target.available(app) {
            index_screen(app, target, &mut index);
        }
    }
    app.ui.screen = screen;
    app.ui.section = section;
    index.results
}

/// The search index being built; a text listed once for a destination is not listed again (whatever its
/// capitals: a theme can show a heading in capitals). A game's page is a destination of its own; a text
/// without one is the same destination as any page of its screen and group.
#[derive(Default)]
struct Index {
    results: Vec<SearchResult>,
}

impl Index {
    fn add(&mut self, text: &str, place: &str, screen: Screen, section: usize, games_page: Option<usize>) {
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.is_empty() {
            return;
        }
        let known = self.results.iter().any(|known| {
            known.text.to_lowercase() == text.to_lowercase()
                && known.screen == screen
                && known.section == section
                && (known.games_page == games_page || known.games_page.is_none() || games_page.is_none())
        });
        if !known {
            self.results.push(SearchResult { text, place: place.to_owned(), screen, section, games_page });
        }
    }
}

/// Adds the texts of one screen to the index (see [`search_index`]).
fn index_screen(app: &mut MyApp, screen: Screen, index: &mut Index) {
    let title = tr(app, screen.key());
    let group = tr(app, screen.group().key());
    let place = |section: &str| {
        if section.is_empty() {
            format!("{group} / {title}")
        } else {
            format!("{group} / {title} / {section}")
        }
    };
    index.add(&title, &group, screen, 0, None);
    // The games first: a game's row the screen shows now is listed with its page.
    if matches!(screen, Screen::Pc | Screen::Console) {
        use super::screens::games;
        let list = if screen == Screen::Console { &app.ui.devices.games.console } else { &app.ui.devices.games.pc };
        for (position, game) in list.games.iter().enumerate() {
            let page = Some(position / games::PAGE);
            index.add(&game.name, &place(""), screen, 0, page);
            index.add(&tr(app, &games::status_text(&game.status)), &place(""), screen, 0, page);
        }
    }
    app.ui.screen = screen;
    app.ui.section = 0;
    let first = super::screens::main(app);
    let mut sections = picker_sections(&first);
    if sections.is_empty() {
        sections.push((0, String::new()));
    }
    let mut rendered: Vec<(usize, String, Vec<String>)> = Vec::new();
    for (number, name) in sections {
        app.ui.section = number;
        let html = if number == 0 { first.clone() } else { super::screens::main(app) };
        rendered.push((number, name, page_texts(&html)));
    }
    // A text every group shows (the panel's own buttons) belongs to the screen, not to each group: it is
    // listed at the first group only.
    let shared: Vec<String> = match rendered.split_first() {
        Some((first, rest)) if !rest.is_empty() => {
            first.2.iter().filter(|text| rest.iter().all(|(_, _, texts)| texts.contains(text))).cloned().collect()
        }
        _ => Vec::new(),
    };
    for (position, (number, name, texts)) in rendered.iter().enumerate() {
        index.add(name, &place(name), screen, *number, None);
        for text in texts {
            if position == 0 || !shared.contains(text) {
                index.add(text, &place(name), screen, *number, None);
            }
        }
    }
    if let Some(page) = super::screens::page(app, screen) {
        for (number, section) in page.sections.iter().enumerate() {
            let name = tr(app, &section.title);
            index.add(&name, &place(&name), screen, number, None);
            for control in &section.controls {
                index.add(&tr(app, &control.label), &place(&name), screen, number, None);
            }
        }
    }
}

/// A Games search result was chosen: its list shows every game, at the page that lists the game.
pub fn show_games_page(app: &mut MyApp, screen: Screen, page: usize) {
    let list = match screen {
        Screen::Pc => &mut app.ui.devices.games.pc,
        Screen::Console => &mut app.ui.devices.games.console,
        _ => return,
    };
    list.filter.clear();
    list.page = page;
}

/// The settings groups a screen's group picker offers: number and title.
fn picker_sections(html: &str) -> Vec<(usize, String)> {
    let Some(start) = html.find("data-act=\"section\"") else { return Vec::new() };
    let picker = &html[start..];
    let picker = &picker[..picker.find("</select>").unwrap_or(picker.len())];
    picker
        .split("<option value=\"")
        .skip(1)
        .filter_map(|option| {
            let (number, rest) = option.split_once('"')?;
            let shown = rest.split_once('>')?.1.split("</option>").next()?;
            Some((number.parse().ok()?, unescape(shown)))
        })
        .collect()
}

/// The texts a page's markup shows: each text between tags and the value of each text field, with the
/// entities read and the white space collapsed. The choices of a select, scripts and styles are left out.
pub fn page_texts(html: &str) -> Vec<String> {
    let mut texts = Vec::new();
    let mut hidden = 0usize;
    let push = |raw: &str, texts: &mut Vec<String>| {
        let text = unescape(raw).split_whitespace().collect::<Vec<_>>().join(" ");
        if !text.is_empty() {
            texts.push(text);
        }
    };
    let mut rest = html;
    while let Some(open) = rest.find('<') {
        if hidden == 0 {
            push(&rest[..open], &mut texts);
        }
        let after = &rest[open + 1..];
        let Some(close) = after.find('>') else {
            rest = "";
            break;
        };
        let tag = &after[..close];
        let (closing, body) = match tag.strip_prefix('/') {
            Some(body) => (true, body),
            None => (false, tag),
        };
        let name: String =
            body.chars().take_while(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase();
        match (name.as_str(), closing) {
            ("option" | "script" | "style", false) => hidden += 1,
            ("option" | "script" | "style", true) => hidden = hidden.saturating_sub(1),
            ("input", false) if hidden == 0 => {
                if matches!(attribute(body, "type").unwrap_or("text"), "text" | "search") {
                    if let Some(value) = attribute(body, "value") {
                        push(value, &mut texts);
                    }
                }
            }
            _ => {}
        }
        rest = &after[close + 1..];
    }
    if hidden == 0 {
        push(rest, &mut texts);
    }
    texts
}

/// The value of an attribute of a tag's markup.
fn attribute<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let marker = format!(" {name}=\"");
    let start = tag.find(&marker)? + marker.len();
    tag[start..].split('"').next()
}

/// The text of markup with its entities read.
fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
}

/// The label of the active profile: the game (and vehicle) whose tune is loaded, or stand-by.
pub fn profile_label(app: &MyApp) -> String {
    let s = &app.ui.session;
    match s.state {
        State::Standby => tr(app, "Stand By"),
        _ => {
            let mut label = s.game.clone();
            if !s.vehicle.is_empty() {
                label.push_str(" · ");
                label.push_str(&s.vehicle);
            }
            if s.state == State::Editing {
                label.push_str(" · ");
                label.push_str(&tr(app, "Editing Profile"));
            }
            label
        }
    }
}

/// The Active profile select: the profile in use, and "Manage profiles…" (the profile list). While a game runs
/// with a vehicle that has no saved profile of its own, a note under the select says the game's profile is used.
pub fn profile_select(app: &MyApp) -> String {
    let game_profile = game_profile_used(app);
    format!(
        "<label class=\"eyebrow\" for=\"profile\">{}</label><select id=\"profile\" data-act=\"profile\"{}><option value=\"current\" selected>{}</option><option value=\"manage\">{}</option></select>{}",
        t(app, "Active profile"),
        if game_profile { " aria-describedby=\"profile-level\"" } else { "" },
        esc(&profile_label(app)),
        t(app, "Manage profiles…"),
        if game_profile {
            format!(
                "<small id=\"profile-level\" class=\"profile-level\">{}</small>",
                t(app, "This car has no saved profile; the game profile is used")
            )
        } else {
            String::new()
        }
    )
}

/// Whether a game runs with a named vehicle whose own profile is not in use: the game's profile is.
pub fn game_profile_used(app: &MyApp) -> bool {
    let s = &app.ui.session;
    s.state != State::Standby && !s.vehicle.is_empty() && !app.ml.vehicle_level
}

/// The rows of the rig status: each device's name (a link to its set-up), status and mode button; the three
/// main outputs show, the rest are under "More devices".
pub fn devices(app: &MyApp) -> String {
    let devices = session::devices(app);
    let row = |device: &session::Device| {
        let warning = matches!(device.color.as_str(), "yellow" | "red" | "pink");
        let mode = match (device.mode, device.mode_index) {
            (Some((_, cycle)), Some(index)) => format!(
                "<button type=\"button\" class=\"device-mode\" data-act=\"mode\" data-arg=\"{}\" aria-label=\"{}\">{}</button>",
                device.key,
                tf(app, "Switch ^device^ mode", &[("device", &tr(app, device.name))]),
                t(app, session::mode_label(cycle, index))
            ),
            _ => String::new(),
        };
        let status = if device.color.is_empty() && device.key.ends_with("Mode") {
            String::new()
        } else {
            t(app, session::status_label(&device.color))
        };
        format!(
            "<div class=\"device{}\" data-device=\"{}\"><span class=\"device-icon\">{}</span><button type=\"button\" class=\"device-link\" data-act=\"device\" data-arg=\"{}\" title=\"{}\"><h3>{}</h3><small>{}</small></button>{mode}<span class=\"{}\"></span></div>",
            if warning { " warning" } else { "" },
            device.key,
            icon(device.icon),
            device.key,
            esc(&device.detail),
            t(app, device.name),
            if device.detail.is_empty() { status } else { format!("{status} · {}", esc(&device.detail)) },
            match device.color.as_str() {
                "green" => "status-dot",
                "orange" | "" => "status-dot idle",
                _ => "status-dot",
            }
        )
    };
    let (outputs, devices): (Vec<_>, Vec<_>) = devices.iter().partition(|d| is_output_mode(d.key));
    let main: String = devices.iter().take(3).map(|d| row(d)).collect();
    let rest: Vec<String> = devices.iter().skip(3).map(|d| row(d)).collect();
    let key = "devices";
    let more = if rest.is_empty() && outputs.is_empty() {
        String::new()
    } else {
        // The Intellibox's output modes follow the remaining devices under their own heading; they are not
        // devices and are not counted as such.
        let outputs = if outputs.is_empty() {
            String::new()
        } else {
            format!(
                "<div class=\"device\"><span class=\"eyebrow\">{}</span></div>{}",
                t(app, "Wind and LED outputs"),
                outputs.iter().map(|d| row(d)).collect::<String>()
            )
        };
        format!(
            "<details class=\"more\" data-act=\"more\" data-arg=\"{key}\"{}><summary class=\"device\">{} · {}</summary>{}{outputs}</details>",
            if app.ui.open_more.contains(key) { " open" } else { "" },
            t(app, "More devices"),
            rest.len(),
            rest.concat()
        )
    };
    format!("{main}{more}")
}

/// Whether a rig status row is one of the Intellibox's output modes (Wind L/R, Wind Center, LED) rather than
/// a device.
fn is_output_mode(key: &str) -> bool {
    matches!(key, "WindMode" | "WindCenterMode" | "LedMode")
}

/// The number of devices, as the rig panel heads it (the Intellibox's output modes are not devices).
pub fn device_count(app: &MyApp) -> String {
    let count = session::devices(app).iter().filter(|d| !is_output_mode(d.key)).count();
    tf(app, "^count^ devices", &[("count", &count.to_string())])
}

/// The phone navigation.
pub fn mobile_nav(app: &MyApp) -> String {
    format!("<nav class=\"mobile-nav\" aria-label=\"{}\">{}</nav>", t(app, "Mobile"), nav_items(app))
}

/// The save state text.
pub fn save_state(app: &MyApp) -> String {
    if super::dirty(app) {
        format!("● {}", t(app, "Unsaved changes"))
    } else {
        format!("✓ {}", t(app, "All changes saved"))
    }
}

/// The session line of the hero: vehicle and location.
pub fn session_detail(app: &MyApp) -> String {
    let s = &app.ui.session;
    let parts: Vec<&str> = [s.vehicle.as_str(), s.location.as_str()].into_iter().filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        match s.state {
            State::Standby => t(app, "Start a game to receive telemetry."),
            _ => t(app, "No vehicle reported yet."),
        }
    } else {
        esc(&parts.join(" · "))
    }
}

/// The trace of the last speeds as an SVG path (240 × 100).
pub fn trace_path(trace: &[f64]) -> String {
    if trace.len() < 2 {
        return "M0 60H240".into();
    }
    let max = trace.iter().copied().fold(1.0_f64, f64::max);
    let step = 240.0 / (session::TRACE_LENGTH - 1) as f64;
    let offset = 240.0 - step * (trace.len() - 1) as f64;
    let mut path = String::new();
    for (index, value) in trace.iter().enumerate() {
        let x = offset + step * index as f64;
        let y = 85.0 - (value / max) * 70.0;
        path.push_str(&format!("{}{x:.1} {y:.1}", if index == 0 { "M" } else { "L" }));
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_trace_ends_at_the_right_edge() {
        assert_eq!(trace_path(&[]), "M0 60H240");
        let path = trace_path(&[0.0, 100.0]);
        assert!(path.starts_with('M') && path.ends_with("240.0 15.0"), "{path}");
    }

    /// The heading's count, the visible rows and the "More devices" number add up to the devices; the
    /// Intellibox's output modes are listed apart and not counted.
    #[test]
    fn the_device_count_matches_the_rows() {
        for hapticr in [false, true] {
            let backend = crate::app::tests::FakeBackend::new();
            backend.docs.lock().unwrap()["settings"]["app"]["enable_hapticr"] = serde_json::json!(hapticr);
            let app = crate::app::tests::loaded(backend);
            let all = session::devices(&app);
            let devices = all.iter().filter(|d| !is_output_mode(d.key)).count();
            assert_eq!(devices, if hapticr { 9 } else { 8 });
            assert_eq!(device_count(&app), format!("{devices} devices"));
            let html = super::devices(&app);
            let (shown, more) = html.split_once("<details").unwrap();
            let visible = shown.matches("data-device=").count();
            let hidden: usize =
                more.split("More devices · ").nth(1).unwrap().split('<').next().unwrap().parse().unwrap();
            assert_eq!(visible + hidden, devices, "{hapticr}");
            assert_eq!(html.matches("data-device=").count(), all.len(), "every row is listed once");
            assert!(more.contains("Wind and LED outputs") && more.contains("data-device=\"MCMode\""));
        }
    }
}
