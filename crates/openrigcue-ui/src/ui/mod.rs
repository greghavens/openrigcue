//! The OpenRigCue interface: the approved Redline, Apex and Vector designs, built by the program and shown by
//! the browser.
//!
//! # How it works
//!
//! The program keeps the interface's state ([`UiState`], `app.ui`) and renders it to HTML; the browser shows
//! that HTML and sends the user's actions back. The page is split into [`Region`]s (navigation, top bar,
//! faults, page heading, session instruments, screen body, rig panel, save bar, phone navigation, toasts,
//! Activity drawer, dialog, menus). Each region is one widget whose content is replaced when it changes:
//! [`refresh`] renders the named regions again and only those whose HTML changed travel to the browser.
//! The main loop calls [`tick`] every pass, which reads the session, the live telemetry and the rig status and
//! refreshes the live regions; the service's notifications arrive through [`notices::synchronize`].
//!
//! Every action the user takes reaches [`dispatch`] as `(act, arg, value)`: the browser script
//! (`res/js/openrigcue-ui.js`) sends, for any element with `data-act`, the action name, its `data-arg` and the
//! element's value (a checkbox sends `1`/`0`). Dispatch runs the action through the service calls the app has
//! always used (`crate::backend::Backend` and the `MyApp` store setters such as `set_tune_native_checked`),
//! then refreshes the regions the action changed. A failing call becomes a warning notice.
//!
//! # Themes
//!
//! Each theme is its own frame ([`themes`]): its navigation, top bar, start-screen heading, session
//! instruments, rig panel, save bar, wording and notification policy are written by the theme's own module.
//! The style sheet `res/css/openrigcue-ui.css` (the approved stylesheet, scoped by the frame's `.orc` class
//! and the theme's class) gives each theme its look and placement; the browser script gives each its
//! motion (toast timing with the hover/focus pause, the drawer, menus, dialogs and tab keys). The theme is
//! stored as `[APP] theme`; a missing, retired or unknown value is Redline ([`Theme::from_setting`]).
//! Switching theme renders every region again in the new frame; the screen, the section, open dialogs and
//! the unsaved edit (kept by the service) stay.
//!
//! # Adding a screen
//!
//! 1. **Where.** One module per screen under `src/ui/screens/` (e.g. `screens/shake.rs`). The [`Screen`]
//!    variant exists for every destination of the design, with its group, catalog key and subtitle.
//!    Register the module in `screens/mod.rs`: add it to [`screens::page`] if it is built from a typed page
//!    description (`crate::spec`), to [`screens::main`] (its body), [`screens::set`] (value changes),
//!    [`screens::action`] (buttons) and, if it needs it, [`screens::on_enter`] (work when it opens).
//! 2. **Markup.** Build the body with [`components`]: [`components::panel`] (one panel per settings group,
//!    with the group picker [`screens::section_picker`] in its head), [`components::spec_row`] for each control
//!    of the description (slider, range, switch, select, stepper, button, readout, text; the per-control
//!    toggle, Max Telemetry and row buttons go under its "Options" disclosure automatically),
//!    [`components::more`] (first four rows, the rest under "More settings · N"), [`components::actions`] (the
//!    page's header buttons), [`components::field`], [`components::select`], [`components::button`] and
//!    [`components::tabs`] for anything else. Use only the classes of the approved stylesheet plus those
//!    `openrigcue-ui.css` adds for these components; never inline styles. Every visible text goes through
//!    [`html::t`] / [`html::tf`] (catalog key, `^name^` placeholders) and needs an entry in all eight locale
//!    files. Theme-specific wording comes from [`themes::title`] (capitals in Vector and Redline).
//! 3. **State.** Fill a [`components::RowState`] per control from the stored values ([`read::field`] reads a
//!    description's field from the documents the main loop keeps current) and from the page's rules (visible,
//!    enabled, label, endpoints, which row buttons apply). Visibility and enabling rules are evaluated on every
//!    render: the main loop re-renders the screen each pass, so a rule that depends on the hardware or the
//!    running game follows it without extra work. Keep live readouts out of the screen body's sliders (put
//!    them in a readout row) so a redraw never fights a drag.
//! 4. **Writes.** In the screen's `set`, map each control id to its service call, e.g.
//!    `app.set_tune_native_checked(&["motion_effects", id], &value)`, `app.set_shaker_value_checked(...)`,
//!    `app.set_setting_native_checked(section, key, &value)`, `app.set_control_checked(path, value)`,
//!    `app.backend.<call>`. Convert the browser's text with [`read::number`], [`read::int`], [`read::whole`],
//!    [`read::exact_int`] and [`read::checked`].
//! 5. **Dialogs.** Add a variant to [`dialogs::Dialog`], render it in [`dialogs::render`] with
//!    [`dialogs::frame`] and handle its fields/buttons in [`dialogs::act`]. Field values arrive as
//!    `dialog-field` (name, value) and are kept in the variant; `dialog-ok` confirms, `dialog-close` cancels.
//!    The shortcut editor ([`shortcuts`]) and the device/sound dialogs are examples.
//! 6. **Notifications.** Use [`notices::success`], [`notices::alert`] (severity from a colour),
//!    [`notices::warning`] or [`notices::raise`] with actions. `MyApp::alert_notification` and `MyApp::alert`
//!    lead here too.
//! 7. **Navigation.** [`navigate`] opens a screen (and settings group); `MyApp::goto_page` maps a section and
//!    page name (`Setup` → `Hardware`, …) to its screen.
//! 8. **Hot keys and main-loop rules.** Values a screen writes on its own (hot-key steps, model-specific
//!    overrides on profile load, per-pass rules) live in the screen module and are called from [`tick`] or the
//!    main loop, reading and writing the stored values (see `screens::motion::apply_hotkeys`). Service reads
//!    that can wait on a device run without holding the application (the `_shared` variants).
//! 9. **Tests.** Add tests beside the screen (`screens/<name>_tests.rs`) using the fixtures of
//!    `crate::app::tests`: render the screen, dispatch `set`/`action`, and assert the service calls.

pub mod components;
pub mod dialogs;
pub mod dialogs_hardware;
pub mod html;
pub mod notices;
pub mod profiles;
pub mod read;
pub mod screens;
pub mod session;
pub mod shell;
pub mod shortcuts;
pub mod themes;

use std::collections::BTreeSet;

use openrigcue_model::native_value::LegacyValue;
use serde_json::{json, Value};

use crate::app::MyApp;
use crate::backend::{text, BackendExt};
use crate::remi::{JsFlags, WidgetId};

pub use themes::Theme;

/// The identifier of the widget that receives every action.
pub const ACTION_ID: &str = "orc-act";

/// The four sections of the navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Group {
    Tuning,
    Games,
    Hardware,
    Settings,
}

impl Group {
    pub const ALL: [Group; 4] = [Group::Tuning, Group::Games, Group::Hardware, Group::Settings];

    /// The catalog text of the section, also its action argument.
    pub fn key(self) -> &'static str {
        match self {
            Group::Tuning => "Tuning",
            Group::Games => "Games",
            Group::Hardware => "Hardware",
            Group::Settings => "Settings",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Group::Tuning => "tune",
            Group::Games => "games",
            Group::Hardware => "hardware",
            Group::Settings => "settings",
        }
    }

    /// The screen the section opens.
    pub fn landing(self) -> Screen {
        match self {
            Group::Tuning => Screen::Home,
            Group::Games => Screen::Pc,
            Group::Hardware => Screen::Hardware,
            Group::Settings => Screen::App,
        }
    }

    pub fn from_key(key: &str) -> Option<Group> {
        Group::ALL.into_iter().find(|group| group.key() == key)
    }
}

/// Every destination of the interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Screen {
    /// The start screen: session instruments, the tuning panel and the rig status.
    #[default]
    Home,
    Motion,
    Shake,
    Wind,
    Belt,
    Sim3d,
    HapticR,
    IFlag,
    Led,
    Tach,
    Routines,
    Profiles,
    DownloadedProfiles,
    Pc,
    Console,
    Telemetry,
    Hardware,
    Joystick,
    Compensation,
    App,
    Shortcuts,
    Debug,
    Help,
}

impl Screen {
    pub const ALL: [Screen; 23] = [
        Screen::Home,
        Screen::Motion,
        Screen::Shake,
        Screen::Wind,
        Screen::Belt,
        Screen::Sim3d,
        Screen::HapticR,
        Screen::IFlag,
        Screen::Led,
        Screen::Tach,
        Screen::Routines,
        Screen::Profiles,
        Screen::DownloadedProfiles,
        Screen::Pc,
        Screen::Console,
        Screen::Telemetry,
        Screen::Hardware,
        Screen::Joystick,
        Screen::Compensation,
        Screen::App,
        Screen::Shortcuts,
        Screen::Debug,
        Screen::Help,
    ];

    /// The catalog text of the screen, also its action argument.
    pub fn key(self) -> &'static str {
        match self {
            Screen::Home => "Tuning",
            Screen::Motion => "Motion",
            Screen::Shake => "Shake",
            Screen::Wind => "Wind",
            Screen::Belt => "Belt",
            Screen::Sim3d => "SIM3D",
            Screen::HapticR => "HapticR",
            Screen::IFlag => "IFlag",
            Screen::Led => "LED",
            Screen::Tach => "Tach",
            Screen::Routines => "Routines",
            Screen::Profiles => "Profiles",
            Screen::DownloadedProfiles => "Downloaded Profiles",
            Screen::Pc => "PC",
            Screen::Console => "Console",
            Screen::Telemetry => "Telemetry",
            Screen::Hardware => "Hardware",
            Screen::Joystick => "Joystick",
            Screen::Compensation => "Motion Compensation",
            Screen::App => "App",
            Screen::Shortcuts => "Shortcuts",
            Screen::Debug => "Debug",
            Screen::Help => "Help",
        }
    }

    pub fn group(self) -> Group {
        match self {
            Screen::Home
            | Screen::Motion
            | Screen::Shake
            | Screen::Wind
            | Screen::Belt
            | Screen::Sim3d
            | Screen::HapticR
            | Screen::IFlag
            | Screen::Led
            | Screen::Tach
            | Screen::Routines
            | Screen::Profiles
            | Screen::DownloadedProfiles => Group::Tuning,
            Screen::Pc | Screen::Console | Screen::Telemetry => Group::Games,
            Screen::Hardware | Screen::Joystick | Screen::Compensation => Group::Hardware,
            Screen::App | Screen::Shortcuts | Screen::Debug | Screen::Help => Group::Settings,
        }
    }

    /// The catalog text of the line under the screen's heading.
    pub fn subtitle(self) -> &'static str {
        match self {
            Screen::Hardware => "Connections, output devices and calibration.",
            Screen::Pc => "Game detection and telemetry setup.",
            Screen::Console => "Console telemetry connections.",
            Screen::Profiles => "Manage your saved tuning.",
            Screen::DownloadedProfiles => "Profiles from the library on this computer.",
            Screen::App => "Application preferences.",
            Screen::Joystick => "Input devices and axis assignments.",
            Screen::Compensation => "Sensor alignment and virtual reality.",
            Screen::Help => "Help, the mobile companion and version information.",
            _ => "Configure the selected section.",
        }
    }

    pub fn from_key(key: &str) -> Option<Screen> {
        Screen::ALL.into_iter().find(|screen| screen.key() == key)
    }

    /// Whether the screen is offered: HapticR only while it is enabled in the settings, Debug only while the
    /// `show_debug_page` custom feature is on.
    pub fn available(self, app: &MyApp) -> bool {
        match self {
            Screen::HapticR => app
                .backend
                .native_setting("APP", "enable_hapticr")
                .ok()
                .and_then(|value| crate::backend::native_bool(&value).ok())
                .unwrap_or(false),
            Screen::Debug => screens::debug::enabled(app),
            _ => true,
        }
    }

    /// The screens of the section, in the order the page selector lists them.
    pub fn of_group(group: Group, app: &MyApp) -> Vec<Screen> {
        Screen::ALL
            .into_iter()
            .filter(|screen| screen.group() == group && *screen != Screen::Home && screen.available(app))
            .collect()
    }
}

/// An open menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Popup {
    Language,
    Search,
}

/// The parts of the page that are drawn separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Region {
    Nav,
    Top,
    Fault,
    Head,
    Live,
    Main,
    Rig,
    Save,
    MobileNav,
    Toasts,
    Drawer,
    Dialog,
    Popup,
}

impl Region {
    pub const ALL: [Region; 13] = [
        Region::Nav,
        Region::Top,
        Region::Fault,
        Region::Head,
        Region::Live,
        Region::Main,
        Region::Rig,
        Region::Save,
        Region::MobileNav,
        Region::Toasts,
        Region::Drawer,
        Region::Dialog,
        Region::Popup,
    ];

    /// The widget identifier of the region.
    pub fn id(self) -> &'static str {
        match self {
            Region::Nav => "orc-nav",
            Region::Top => "orc-top",
            Region::Fault => "orc-fault",
            Region::Head => "orc-head",
            Region::Live => "orc-live",
            Region::Main => "orc-main",
            Region::Rig => "orc-rig",
            Region::Save => "orc-save",
            Region::MobileNav => "orc-mobile-nav",
            Region::Toasts => "orc-toasts",
            Region::Drawer => "orc-drawer",
            Region::Dialog => "orc-dialog",
            Region::Popup => "orc-popup",
        }
    }
}

/// The interface's state.
#[derive(Default)]
pub struct UiState {
    /// Whether the interface has been built.
    pub mounted: bool,
    /// The page's root widget.
    pub root: Option<WidgetId>,
    /// The widget of each region.
    pub regions: Vec<(Region, WidgetId)>,
    pub theme: Theme,
    /// The selected language code.
    pub language: String,
    pub screen: Screen,
    /// The settings group shown on the screen.
    pub section: usize,
    /// The start screen's tuning tab (Motion, Shake, Wind).
    pub tab: usize,
    pub notices: notices::Center,
    /// Whether the Activity drawer is open.
    pub drawer: bool,
    pub popup: Option<Popup>,
    /// The search text.
    pub search: String,
    /// The texts the search looks through, gathered from every screen when the search starts (see
    /// [`shell::search_index`]); dropped when the search opens again, the language changes or a screen opens.
    pub search_index: Option<Vec<shell::SearchResult>>,
    pub dialog: Option<dialogs::Dialog>,
    /// The disclosures the user opened (they stay open across redraws).
    pub open_more: BTreeSet<String>,
    pub session: session::Session,
    pub readouts: session::Readouts,
    /// The tune as last loaded or saved (the save state and Compare read it).
    pub snapshot: Option<Value>,
    snapshot_generation: Option<u64>,
    snapshot_game: String,
    /// Take a new snapshot at the next pass (after a revert).
    pub resnapshot: bool,
    /// The profile list.
    pub profiles: profiles::State,
    /// The motion platform fault shown inline.
    motion_fault: Option<u64>,
    /// What the Games and Hardware screens keep between passes (lists, live readings, filters, pages).
    pub devices: dialogs_hardware::State,
    /// Settings → Shortcuts: the search text.
    pub shortcuts: screens::shortcuts::State,
    /// Settings → Debug: the output of the last Reload.
    pub debug: screens::debug::State,
    /// The tuning effect screens: the library, its filter, the running Tach test and routine options.
    pub tuning: screens::tuning::State,
}

impl UiState {
    fn node(&self, region: Region) -> Option<WidgetId> {
        self.regions.iter().find(|(r, _)| *r == region).map(|(_, id)| *id)
    }
}

/// A widget of the frame: `tag` with `class`.
fn node(app: &mut MyApp, tag: &str, class: &str) -> WidgetId {
    let id = app.gui.create(tag, "OrcRegion");
    app.gui.set_attr_class(id, class);
    id
}

/// Builds the interface in `root` (the page's root widget) and draws it.
pub fn mount(app: &mut MyApp, root: WidgetId) {
    if let Some(label) = app.gui.child(root, "loading_label") {
        app.gui.remove_child(root, label);
    }
    app.ui.root = Some(root);
    app.ui.theme = Theme::configured(app);
    app.ui.language = app
        .backend
        .get("i18n")
        .ok()
        .and_then(|catalog| catalog.get("language").map(text))
        .filter(|code| !code.is_empty())
        .unwrap_or_else(|| "EN".into());
    let action = app.gui.create("div", "OrcAction");
    app.gui.set_identifier(action, ACTION_ID);
    app.gui.on(
        action,
        "onact",
        None,
        JsFlags::NONE,
        crate::app::native_callback(|app, args| {
            let part = |index: usize| match args.get(index) {
                Some(LegacyValue::Str(text)) => text.clone(),
                _ => String::new(),
            };
            let (act, arg, value) = (part(0), part(1), part(2));
            if let Err(error) = dispatch(app, &act, &arg, &value) {
                tracing::warn!(act, arg, %error, "interface action failed");
                let message = app.backend.translate("An internal error occurred: ^detail^").replace("^detail^", &error);
                notices::warning(app, "Alert", &message);
            }
            Ok(())
        }),
    );
    // The frame: the shell (navigation and workspace with its content), then the overlays.
    let mut regions = Vec::new();
    let mut region = |app: &mut MyApp, which: Region| {
        let id = node(app, "div", "orc-region");
        app.gui.set_identifier(id, which.id());
        regions.push((which, id));
        id
    };
    let shell = node(app, "div", "shell");
    let nav = region(app, Region::Nav);
    app.gui.append(shell, nav, "");
    let workspace = node(app, "div", "workspace");
    let top = region(app, Region::Top);
    app.gui.append(workspace, top, "");
    let content = node(app, "main", "content");
    for which in [Region::Fault, Region::Head, Region::Live] {
        let id = region(app, which);
        app.gui.append(content, id, "");
    }
    let grid = node(app, "div", "grid");
    for which in [Region::Main, Region::Rig] {
        let id = region(app, which);
        app.gui.append(grid, id, "");
    }
    app.gui.append(content, grid, "");
    let save = region(app, Region::Save);
    app.gui.append(content, save, "");
    app.gui.append(workspace, content, "");
    app.gui.append(shell, workspace, "");
    app.gui.append(root, shell, "");
    let mobile = region(app, Region::MobileNav);
    app.gui.append(root, mobile, "");
    let toast_area = node(app, "div", "toast-area");
    app.gui.set_attr(toast_area, "aria-live", Some("polite"));
    app.gui.set_attr(toast_area, "aria-relevant", Some("additions"));
    let toasts = region(app, Region::Toasts);
    app.gui.append(toast_area, toasts, "");
    app.gui.append(root, toast_area, "");
    for which in [Region::Drawer, Region::Dialog, Region::Popup] {
        let id = region(app, which);
        app.gui.append(root, id, "");
    }
    app.ui.regions = regions;
    app.ui.session = session::read(&*app.backend);
    app.ui.mounted = true;
    refresh_all(app);
}

/// The HTML of a region.
fn render(app: &MyApp, region: Region) -> String {
    match region {
        Region::Nav => themes::nav(app),
        Region::Top => themes::top(app),
        Region::Fault => notices::faults(app),
        Region::Head => screens::heading(app),
        Region::Live => {
            if app.ui.screen == Screen::Home {
                themes::live(app)
            } else {
                String::new()
            }
        }
        Region::Main => screens::main(app),
        Region::Rig => themes::rig(app),
        Region::Save => themes::save(app),
        Region::MobileNav => shell::mobile_nav(app),
        Region::Toasts => notices::toasts(app),
        Region::Drawer => notices::drawer(app),
        Region::Dialog => dialogs::render(app),
        Region::Popup => shell::popup(app),
    }
}

/// Draws the regions again; only the changed ones reach the browser.
pub fn refresh(app: &mut MyApp, regions: &[Region]) {
    if !app.ui.mounted {
        return;
    }
    for region in regions {
        let html = render(app, *region);
        if let Some(id) = app.ui.node(*region) {
            app.gui.add_child_text(id, "html", &html, false);
        }
    }
}

/// Draws everything again, in the current theme.
pub fn refresh_all(app: &mut MyApp) {
    if let Some(root) = app.ui.root {
        let class = format!("orc {}", app.ui.theme.id());
        app.gui.set_attr_class(root, &class);
        app.gui.set_attr(root, "data-theme", Some(app.ui.theme.id()));
        let language = if app.ui.language == "ZH" { "zh".to_owned() } else { app.ui.language.to_lowercase() };
        app.gui.set_attr(root, "lang", Some(&language));
    }
    refresh(app, &Region::ALL);
}

/// Draws the notification regions again.
pub fn refresh_notices(app: &mut MyApp) {
    refresh(app, &[Region::Toasts, Region::Drawer, Region::Fault, Region::Top]);
}

/// Opens a screen at a settings group. Every navigation ends by writing the settings file
/// ([`write_settings`]), so a settings change made on the screen left is on disk.
pub fn navigate(app: &mut MyApp, screen: Screen, section: usize) {
    let screen = if screen.available(app) { screen } else { screen.group().landing() };
    app.ui.screen = screen;
    app.ui.section = section;
    app.ui.popup = None;
    app.ui.search_index = None;
    screens::on_enter(app, screen);
    write_settings(app);
    refresh(
        app,
        &[
            Region::Nav,
            Region::Top,
            Region::Head,
            Region::Live,
            Region::Main,
            Region::Rig,
            Region::Save,
            Region::MobileNav,
            Region::Popup,
        ],
    );
    app.gui.execute_javascript("window.scrollTo({top:0,behavior:'instant'});");
}

/// Writes the settings file: the service applies the selected language again and writes the whole
/// configuration, the settings changed since the last write included (the Settings screens' changes are kept
/// in memory until then).
pub fn write_settings(app: &MyApp) {
    if let Err(failure) = app.backend.post("i18n/reapply", json!({})) {
        tracing::warn!(%failure, "settings file not written");
    }
}

/// The screen of a section and page name (what `MyApp::goto_page` is given).
pub fn screen_of_page(global: &str, page: &str) -> Option<Screen> {
    let screen = match (global, page) {
        (_, "Motion Compensation") | ("Setup", "MC") => Screen::Compensation,
        (_, "SIM3D") => Screen::Sim3d,
        ("Setup", "Hardware") => Screen::Hardware,
        (_, page) => Screen::from_key(page)?,
    };
    Some(screen)
}

/// Whether the tune differs from the last loaded or saved one.
pub fn dirty(app: &MyApp) -> bool {
    app.ui.snapshot.as_ref().is_some_and(|snapshot| snapshot != &app.docs.tune)
}

/// Runs one action of the user.
pub fn dispatch(app: &mut MyApp, act: &str, arg: &str, value: &str) -> Result<(), String> {
    match act {
        "nav" => {
            let group = Group::from_key(arg).ok_or_else(|| format!("unknown section {arg:?}"))?;
            navigate(app, group.landing(), 0);
        }
        "screen" => {
            let screen = Screen::from_key(value).ok_or_else(|| format!("unknown screen {value:?}"))?;
            navigate(app, screen, 0);
        }
        "section" => {
            app.ui.section = value.parse().unwrap_or(0);
            refresh(app, &[Region::Main, Region::Head]);
        }
        "tab" => {
            app.ui.tab = arg.parse::<usize>().unwrap_or(0).min(2);
            refresh(app, &[Region::Main]);
        }
        "open-tuning" => {
            let screen = [Screen::Motion, Screen::Shake, Screen::Wind][app.ui.tab.min(2)];
            navigate(app, screen, 0);
        }
        "more" => {
            if read::checked(value) {
                app.ui.open_more.insert(arg.to_owned());
            } else {
                app.ui.open_more.remove(arg);
            }
        }
        "drawer" => {
            app.ui.drawer = !app.ui.drawer;
            refresh(app, &[Region::Drawer, Region::Top]);
        }
        "drawer-open" => {
            app.ui.drawer = true;
            refresh(app, &[Region::Drawer, Region::Top]);
        }
        "drawer-close" => {
            app.ui.drawer = false;
            refresh(app, &[Region::Drawer, Region::Top]);
        }
        "activity-clear" => {
            app.ui.notices.clear_history();
            refresh_notices(app);
        }
        "toast-dismiss" | "toast-expire" => {
            if let Ok(id) = arg.parse::<u64>() {
                // × on a notice with a Dismiss button is that button: the service is told.
                let dismiss = (act == "toast-dismiss")
                    .then(|| app.ui.notices.get(id))
                    .flatten()
                    .filter(|notice| notice.open)
                    .and_then(|notice| {
                        notice.actions.iter().position(|(_, action)| matches!(action, notices::Action::Dismiss { .. }))
                    });
                if let Some(index) = dismiss {
                    return notices::run(app, id, index);
                }
                app.ui.notices.hide(id);
            }
            refresh(app, &[Region::Toasts]);
        }
        "notice" => {
            let (id, index) = arg.split_once(':').ok_or("bad notice argument")?;
            notices::run(app, id.parse().map_err(|_| "bad notice id")?, index.parse().unwrap_or(0))?;
        }
        "popup" => {
            let popup = match arg {
                "language" => Popup::Language,
                _ => Popup::Search,
            };
            app.ui.popup = if app.ui.popup == Some(popup) { None } else { Some(popup) };
            if app.ui.popup == Some(Popup::Search) {
                app.ui.search_index = None;
                if app.ui.search.trim().chars().count() >= 3 {
                    app.ui.search_index = Some(shell::search_index(app));
                }
            }
            refresh(app, &[Region::Popup, Region::Top]);
        }
        "popup-close" => {
            if app.ui.popup.is_some() {
                app.ui.popup = None;
                refresh(app, &[Region::Popup, Region::Top]);
            }
        }
        "language" => set_language(app, arg)?,
        "search" => {
            app.ui.search = value.to_owned();
            if app.ui.search.trim().chars().count() >= 3 && app.ui.search_index.is_none() {
                app.ui.search_index = Some(shell::search_index(app));
            }
            refresh(app, &[Region::Popup]);
        }
        "search-go" => {
            let mut parts = arg.splitn(3, ':');
            let (key, section, games_page) =
                (parts.next().unwrap_or(""), parts.next().unwrap_or("0"), parts.next().and_then(|p| p.parse().ok()));
            let screen = Screen::from_key(key).ok_or_else(|| format!("unknown screen {key:?}"))?;
            app.ui.search.clear();
            if let Some(page) = games_page {
                shell::show_games_page(app, screen, page);
            }
            navigate(app, screen, section.parse().unwrap_or(0));
        }
        "theme" => set_theme(app, value)?,
        "profile" => {
            if value == "manage" {
                navigate(app, Screen::Profiles, 0);
            } else {
                refresh(app, &[Region::Head]);
            }
        }
        "save" => profiles::save_clicked(app)?,
        "revert" => profiles::revert_clicked(app)?,
        "compare" => compare(app),
        "inspect" => inspect(app),
        "mode" => {
            let device = session::devices(app).into_iter().find(|device| device.key == arg);
            if let Some((control, cycle)) = device.and_then(|device| device.mode) {
                session::cycle_mode(app, control, cycle)?;
            }
            refresh(app, &[Region::Rig]);
        }
        "device" => navigate(app, if arg == "MCMode" { Screen::Compensation } else { Screen::Hardware }, 0),
        "set" => {
            let mut parts = arg.splitn(3, ':');
            let (screen, control, part) =
                (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
            let screen = Screen::from_key(screen).ok_or_else(|| format!("unknown screen {screen:?}"))?;
            screens::set(app, screen, control, part, value)?;
            refresh(app, &[Region::Main, Region::Save, Region::Live]);
        }
        "action" => {
            let mut parts = arg.splitn(3, ':');
            let (screen, control, index) =
                (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
            let screen = Screen::from_key(screen).ok_or_else(|| format!("unknown screen {screen:?}"))?;
            screens::action(app, screen, control, index.parse().ok())?;
            refresh(app, &[Region::Main, Region::Save]);
        }
        act if act.starts_with("dialog") || act.starts_with("shortcut") => dialogs::act(app, act, arg, value)?,
        act if act.starts_with("profile") => profiles::act(app, act, arg, value)?,
        "companion" => dialogs::open_companion(app),
        "standby-hide" => {
            let result = app.backend.post("settings/hide-standby-alert", json!({})).map_err(|error| error.detail);
            if let Ok(settings) = app.backend.get("settings") {
                app.docs.settings = settings;
            }
            result?;
            refresh(app, &[Region::Main]);
        }
        _ => return Err(format!("unknown action {act:?}")),
    }
    Ok(())
}

/// Selects a language: the service takes it and every region is drawn again in it.
pub fn set_language(app: &mut MyApp, code: &str) -> Result<(), String> {
    let catalog = app.backend.put("i18n", json!({ "code": code })).map_err(|error| error.detail)?;
    let selected = catalog.get("language").map(text).unwrap_or_else(|| code.to_owned());
    app.ui.language = selected;
    app.ui.popup = None;
    app.ui.search_index = None;
    // The device status texts are built in the language of their last run; run the rules again in the new one.
    app.ml.previous_hardware_snapshot = None;
    refresh_all(app);
    app.gui.execute_javascript("var b=document.getElementById('language-switcher');if(b)b.focus();");
    Ok(())
}

/// Selects a theme: it is stored and shown at once; the screen, open dialogs and unsaved values stay.
pub fn set_theme(app: &mut MyApp, value: &str) -> Result<(), String> {
    let theme = Theme::from_setting(value);
    app.docs.settings = app.backend.assign_app_setting("theme", json!(theme.id())).map_err(|error| error.detail)?;
    app.ui.theme = theme;
    refresh_all(app);
    Ok(())
}

/// Compare: the values of the tuning tab (or the open screen) that differ from the saved ones, as an Activity
/// entry, and the Activity drawer opens.
pub fn compare(app: &mut MyApp) {
    let rows = screens::compare_rows(app);
    let scope = if app.ui.screen == Screen::Home {
        html::tr(app, ["Motion", "Shake", "Wind"][app.ui.tab.min(2)])
    } else {
        html::tr(app, app.ui.screen.key())
    };
    let title = html::tr(app, "Saved → current: ^scope^").replace("^scope^", &scope);
    let message = if rows.is_empty() { html::tr(app, "No changes since the last save.") } else { rows.join(" · ") };
    let mut notice = notices::notice(notices::Severity::Info, &title, &message);
    notice.actions = Vec::new();
    let policy = themes::policy(app.ui.theme);
    let id = app.ui.notices.push(notice, policy);
    app.ui.notices.hide(id);
    app.ui.drawer = true;
    refresh_notices(app);
}

/// Inspect channels: the live readings as an Activity entry, and the Activity drawer opens.
pub fn inspect(app: &mut MyApp) {
    let r = &app.ui.readouts;
    let message = if r.live {
        format!(
            "{} {:+.1}° · {} {:+.1}° · {} {:+.1}° · {} {:+.2} g · {} {:+.2} g · {} {:+.2} g",
            html::tr(app, "Roll"),
            r.roll,
            html::tr(app, "Pitch"),
            r.pitch,
            html::tr(app, "Yaw"),
            r.yaw,
            html::tr(app, "Lateral G"),
            r.lateral_g,
            html::tr(app, "Long G"),
            r.longitudinal_g,
            html::tr(app, "Vert G"),
            r.vertical_g
        )
    } else {
        html::tr(app, "No telemetry is arriving. Start a game to see its channels.")
    };
    let title = html::tr(app, "Input channels");
    let notice = notices::notice(notices::Severity::Info, &title, &message);
    let policy = themes::policy(app.ui.theme);
    let id = app.ui.notices.push(notice, policy);
    app.ui.notices.hide(id);
    app.ui.drawer = true;
    refresh_notices(app);
}

/// One main-loop pass: the session, live readouts, rig status, the motion platform fault, and every live
/// region.
pub fn tick(app: &mut MyApp) {
    if !app.ui.mounted {
        return;
    }
    let session = session::read(&*app.backend);
    let telemetry = (session.state == session::State::Telemetry).then(|| app.backend.get("telemetry").ok()).flatten();
    apply_tick(app, session, telemetry);
}

/// [`tick`] with the service reads made without holding the application.
pub(crate) fn tick_shared(shared: &std::sync::Mutex<MyApp>) {
    let (backend, mounted) = {
        let app = crate::host::lock(shared);
        (app.backend.clone(), app.ui.mounted)
    };
    if !mounted {
        return;
    }
    let session = session::read(&*backend);
    let telemetry = (session.state == session::State::Telemetry).then(|| backend.get("telemetry").ok()).flatten();
    apply_tick(&mut crate::host::lock(shared), session, telemetry);
}

fn apply_tick(app: &mut MyApp, session: session::Session, telemetry: Option<Value>) {
    let readouts = session::readouts(app, &session, telemetry.as_ref(), &app.ui.readouts);
    let generation = app.ml.loaded_generation;
    if app.ui.snapshot.is_none()
        || app.ui.resnapshot
        || generation != app.ui.snapshot_generation
        || session.game_key != app.ui.snapshot_game
    {
        app.ui.snapshot = Some(app.docs.tune.clone());
        app.ui.snapshot_generation = generation;
        app.ui.snapshot_game = session.game_key.clone();
        app.ui.resnapshot = false;
    }
    app.ui.session = session;
    app.ui.readouts = readouts;
    motion_link(app);
    refresh(
        app,
        &[Region::Nav, Region::Top, Region::Head, Region::Live, Region::Main, Region::Rig, Region::Save, Region::Fault],
    );
}

/// The motion platform's connection as an inline fault: shown while a configured platform is not connected,
/// resolved (with a success) when it connects.
fn motion_link(app: &mut MyApp) {
    let (color, detail) = app.ml.hardware_states.get("Motionbox").cloned().unwrap_or_default();
    let configured = app
        .backend
        .native_setting("MOTIONBOX", "model")
        .ok()
        .is_some_and(|model| !matches!(&model, LegacyValue::Str(name) if name == "None" || name.is_empty()));
    let down = configured && color == "red";
    match (down, app.ui.motion_fault) {
        (true, None) => {
            let title = html::tr(app, "Motion platform not connected");
            let message = if detail.is_empty() {
                html::tr(app, "Motion output is paused. Check the power and the connection.")
            } else {
                detail
            };
            let mut notice = notices::notice(notices::Severity::Error, &title, &message);
            notice.fault = true;
            notice.actions = vec![("Hardware setup".into(), notices::Action::Open { screen: Screen::Hardware })];
            let policy = themes::policy(app.ui.theme);
            let id = app.ui.notices.push(notice, policy);
            app.ui.motion_fault = Some(id);
            refresh_notices(app);
        }
        (false, Some(id)) if color == "green" || !configured => {
            app.ui.notices.close(id);
            app.ui.motion_fault = None;
            if configured {
                notices::confirmation(app, "Motion platform connected", "Review the settings before you resume.");
            } else {
                refresh_notices(app);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests;
