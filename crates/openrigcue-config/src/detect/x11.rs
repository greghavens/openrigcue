//! Window titles from the X server (pure-Rust X11 client, no C libraries).
//!
//! Proton games render through XWayland on Wayland desktops, so their windows are listed by the X server's window
//! manager in `_NET_CLIENT_LIST` on both X11 and Wayland sessions. Native Wayland windows are not visible here.
//!
//! Two things differ where no desktop window manager runs, as in a gamescope session (Game Mode):
//! - gamescope keeps no `_NET_CLIENT_LIST`; its clients are the root window's own children, and the viewable
//!   ones are listed then;
//! - such a session runs one X server for Steam and another for the games (`STEAM_GAME_DISPLAY_0`), and a user
//!   service has no `$DISPLAY` at all, so the user's other local X servers are listed after `$DISPLAY`'s.

use std::collections::BTreeMap;
use std::path::Path;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{Atom, AtomEnum, ClientMessageEvent, ConnectionExt, EventMask, MapState, Window};
use x11rb::rust_connection::RustConnection;

use super::snapshot::WindowTitles;

/// Longest title read, in 32-bit units.
const TITLE_LENGTH: u32 = 1024;

/// Where the local X servers have their sockets (`X<display number>`).
const SOCKET_DIR: &str = "/tmp/.X11-unix";

struct Session {
    connection: RustConnection,
    root: Window,
    client_list: Atom,
    net_wm_name: Atom,
    utf8_string: Atom,
    active_window: Atom,
    supported: Atom,
}

/// The first matching window is absent, or its activation request failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowActivationError {
    /// No matching title is available, including when the X server cannot be enumerated.
    NoWindow,
    /// The window manager lacks activation support, or the X request failed.
    Failed,
}

/// Titles of the top-level windows on `$DISPLAY`, then on the user's other local X servers. Reconnects on the next call after any failure; a server that cannot be reached
/// or read contributes no titles and does not stop the others.
#[derive(Default)]
pub struct X11WindowTitles {
    /// The session on `$DISPLAY`.
    session: Option<Session>,
    others: OtherServers<Session>,
}

impl X11WindowTitles {
    /// Ask the EWMH window manager to activate the first case-insensitive title match on `$DISPLAY`.
    /// Success means the request was submitted; the window manager can refuse focus.
    pub fn activate(&mut self, title: &str) -> Result<(), WindowActivationError> {
        if self.session.is_none() {
            self.session = connect(None);
        }
        let Some(windows) = self.session.as_ref().and_then(windows_of) else {
            self.session = None;
            return Err(WindowActivationError::NoWindow);
        };
        let session = self.session.as_ref().expect("read the connected window list");
        let result =
            activate_matching(windows.into_iter().map(|window| (window, session.title(window))), title, |window| {
                request_activation(session, window)
            });
        if result == Err(WindowActivationError::Failed) {
            self.session = None;
        }
        result
    }
}

fn activate_matching(
    windows: impl IntoIterator<Item = (Window, String)>,
    title: &str,
    activate: impl FnOnce(Window) -> Result<(), WindowActivationError>,
) -> Result<(), WindowActivationError> {
    let wanted = title.to_uppercase();
    let window = windows
        .into_iter()
        .find_map(|(window, title)| title.to_uppercase().contains(&wanted).then_some(window))
        .ok_or(WindowActivationError::NoWindow)?;
    activate(window)
}

fn activation_event(window: Window, active_window: Atom) -> ClientMessageEvent {
    // This application has no native X11 toplevel or X11 user-event timestamp.
    ClientMessageEvent::new(32, window, active_window, [1, x11rb::CURRENT_TIME, 0, 0, 0])
}

fn request_activation(session: &Session, window: Window) -> Result<(), WindowActivationError> {
    let request = || -> Option<()> {
        let supported = session
            .connection
            .get_property(false, session.root, session.supported, AtomEnum::ATOM, 0, u32::MAX)
            .ok()?
            .reply()
            .ok()?;
        if !supported.value32()?.any(|atom| atom == session.active_window) {
            return None;
        }
        session
            .connection
            .send_event(
                false,
                session.root,
                EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
                activation_event(window, session.active_window),
            )
            .ok()?
            .check()
            .ok()?;
        session.connection.flush().ok()
    };
    request().ok_or(WindowActivationError::Failed)
}

impl WindowTitles for X11WindowTitles {
    fn titles(&mut self) -> Vec<String> {
        if self.session.is_none() {
            self.session = connect(None);
        }
        let titles = self.session.as_ref().and_then(titles_of);
        if titles.is_none() {
            self.session = None;
        }
        let mut titles = titles.unwrap_or_default();
        // `$DISPLAY`'s server is only left out of the others while its own session answers.
        let preferred = self.session.as_ref().and_then(|_| local_display(std::env::var("DISPLAY").ok().as_deref()));
        let others = other_displays(preferred, local_servers(Path::new(SOCKET_DIR), own_user()));
        titles.extend(self.others.titles(&others, |display| connect(Some(&format!(":{display}")))));
        titles
    }
}

/// What the window list needs from one X server.
trait Server {
    /// The root's `_NET_CLIENT_LIST`: `Some(None)` when the property does not exist, `None` when it could not be
    /// read as a window list.
    fn client_list(&self) -> Option<Option<Vec<Window>>>;
    /// The root window's children, bottom-most first.
    fn root_children(&self) -> Option<Vec<Window>>;
    /// The window is mapped and so are its ancestors. A window that no longer exists is not viewable.
    fn viewable(&self, window: Window) -> bool;
    fn title(&self, window: Window) -> String;
}

impl Server for Session {
    fn client_list(&self) -> Option<Option<Vec<Window>>> {
        let reply = self
            .connection
            .get_property(false, self.root, self.client_list, AtomEnum::WINDOW, 0, u32::MAX)
            .ok()?
            .reply()
            .ok()?;
        if reply.type_ == x11rb::NONE {
            return Some(None);
        }
        let listed = reply.value32()?.collect();
        Some(Some(listed))
    }

    fn root_children(&self) -> Option<Vec<Window>> {
        Some(self.connection.query_tree(self.root).ok()?.reply().ok()?.children)
    }

    fn viewable(&self, window: Window) -> bool {
        self.connection
            .get_window_attributes(window)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .is_some_and(|attributes| attributes.map_state == MapState::VIEWABLE)
    }

    fn title(&self, window: Window) -> String {
        let property = |name: Atom, kind: Atom| {
            let reply = self.connection.get_property(false, window, name, kind, 0, TITLE_LENGTH).ok()?.reply().ok()?;
            (!reply.value.is_empty()).then_some(reply.value)
        };
        if let Some(utf8) = property(self.net_wm_name, self.utf8_string) {
            return String::from_utf8_lossy(&utf8).into_owned();
        }
        property(AtomEnum::WM_NAME.into(), AtomEnum::STRING.into())
            .map(|latin1| latin1.into_iter().map(char::from).collect())
            .unwrap_or_default()
    }
}

/// The top-level windows of a server: the window manager's client list where one is kept, exactly as listed;
/// otherwise the root's viewable children (no reparenting window manager stands between the root and its clients
/// on such a server). `None` when the server could not be asked.
fn windows_of(server: &impl Server) -> Option<Vec<Window>> {
    match server.client_list()? {
        Some(listed) => Some(listed),
        None => Some(server.root_children()?.into_iter().filter(|window| server.viewable(*window)).collect()),
    }
}

fn titles_of<S: Server>(server: &S) -> Option<Vec<String>> {
    Some(windows_of(server)?.into_iter().map(|window| server.title(window)).collect())
}

/// Sessions on the user's other X servers, kept between calls, by display number.
struct OtherServers<S> {
    sessions: BTreeMap<u32, S>,
}

impl<S> Default for OtherServers<S> {
    fn default() -> Self {
        OtherServers { sessions: BTreeMap::new() }
    }
}

impl<S: Server> OtherServers<S> {
    /// The titles on `displays`, in that order. A display without a session is connected to; one that cannot be
    /// connected to or read is skipped and tried again on the next call; sessions of displays no longer named
    /// are closed.
    fn titles(&mut self, displays: &[u32], connect: impl Fn(u32) -> Option<S>) -> Vec<String> {
        self.sessions.retain(|display, _| displays.contains(display));
        let mut titles = Vec::new();
        for display in displays {
            if !self.sessions.contains_key(display) {
                if let Some(session) = connect(*display) {
                    self.sessions.insert(*display, session);
                }
            }
            match self.sessions.get(display).and_then(titles_of) {
                Some(found) => titles.extend(found),
                None => {
                    self.sessions.remove(display);
                }
            }
        }
        titles
    }
}

/// The number of a local display name (`:1`, `:0.0`, `unix:0`); `None` for a remote or malformed one.
fn local_display(display: Option<&str>) -> Option<u32> {
    let (host, rest) = display?.rsplit_once(':')?;
    if !(host.is_empty() || host == "unix") {
        return None;
    }
    rest.split('.').next()?.parse().ok()
}

/// This process's user, as the owner of its `/proc` entry.
fn own_user() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").ok().map(|metadata| metadata.uid())
}

/// Display numbers of the X servers whose socket `X<n>` in `sockets` belongs to `user`, ascending. A folder or
/// entry that cannot be read names no server.
fn local_servers(sockets: &Path, user: Option<u32>) -> Vec<u32> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let (Some(user), Ok(entries)) = (user, std::fs::read_dir(sockets)) else {
        return Vec::new();
    };
    let mut servers: Vec<u32> = entries
        .flatten()
        .filter(|entry| {
            entry.metadata().is_ok_and(|metadata| metadata.file_type().is_socket() && metadata.uid() == user)
        })
        .filter_map(|entry| entry.file_name().to_str()?.strip_prefix('X')?.parse().ok())
        .collect();
    servers.sort_unstable();
    servers.dedup();
    servers
}

/// The servers listed after the preferred display's.
fn other_displays(preferred: Option<u32>, servers: Vec<u32>) -> Vec<u32> {
    servers.into_iter().filter(|display| Some(*display) != preferred).collect()
}

/// A session on `display` (`None`: `$DISPLAY`).
fn connect(display: Option<&str>) -> Option<Session> {
    let (connection, screen) = x11rb::connect(display).ok()?;
    let root = connection.setup().roots.get(screen)?.root;
    let atom = |name: &[u8]| Some(connection.intern_atom(false, name).ok()?.reply().ok()?.atom);
    let client_list = atom(b"_NET_CLIENT_LIST")?;
    let net_wm_name = atom(b"_NET_WM_NAME")?;
    let utf8_string = atom(b"UTF8_STRING")?;
    let active_window = atom(b"_NET_ACTIVE_WINDOW")?;
    let supported = atom(b"_NET_SUPPORTED")?;
    Some(Session { connection, root, client_list, net_wm_name, utf8_string, active_window, supported })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::os::unix::net::UnixListener;

    #[test]
    fn activation_preserves_first_title_match_and_missing_or_failed_outcomes() {
        let windows = || vec![(10, "Other".into()), (20, "FanaLab Profile".into()), (30, "FANALAB".into())];
        let mut requests = Vec::new();
        assert_eq!(
            activate_matching(windows(), "fanalab", |window| {
                requests.push(window);
                Ok(())
            }),
            Ok(())
        );
        assert_eq!(requests, [20]);
        assert_eq!(
            activate_matching(windows(), "missing", |_| panic!("no activation without a matching window")),
            Err(WindowActivationError::NoWindow)
        );
        assert_eq!(
            activate_matching(windows(), "FanaLab", |_| Err(WindowActivationError::Failed)),
            Err(WindowActivationError::Failed)
        );
    }

    #[test]
    fn activation_message_uses_the_ewmh_application_request_fields() {
        let event = activation_event(0x1234, 0x5678);
        assert_eq!(event.format, 32);
        assert_eq!(event.window, 0x1234);
        assert_eq!(event.type_, 0x5678);
        assert_eq!(event.data.as_data32(), [1, 0, 0, 0, 0]);
    }

    /// An X server as the window list sees it.
    #[derive(Default)]
    struct Fake {
        client_list: Option<Option<Vec<Window>>>,
        children: Option<Vec<Window>>,
        viewable: Vec<Window>,
        titles: Vec<(Window, &'static str)>,
        asked: RefCell<Vec<&'static str>>,
    }

    impl Server for Fake {
        fn client_list(&self) -> Option<Option<Vec<Window>>> {
            self.asked.borrow_mut().push("client list");
            self.client_list.clone()
        }

        fn root_children(&self) -> Option<Vec<Window>> {
            self.asked.borrow_mut().push("children");
            self.children.clone()
        }

        fn viewable(&self, window: Window) -> bool {
            self.viewable.contains(&window)
        }

        fn title(&self, window: Window) -> String {
            self.titles.iter().find(|(known, _)| *known == window).map_or(String::new(), |(_, title)| (*title).into())
        }
    }

    /// A desktop window manager's client list is the window list, as before: in its order, with windows that are
    /// not viewable or have no title, and without looking at the root's children.
    #[test]
    fn a_client_list_is_used_as_listed() {
        let desktop = Fake {
            client_list: Some(Some(vec![7, 3, 9])),
            children: Some(vec![100, 101]),
            viewable: vec![3],
            titles: vec![(3, "rFactor2 v1.1"), (7, "Assetto Corsa"), (100, "frame")],
            ..Fake::default()
        };
        assert_eq!(windows_of(&desktop), Some(vec![7, 3, 9]));
        assert_eq!(titles_of(&desktop).unwrap(), ["Assetto Corsa", "rFactor2 v1.1", ""]);
        assert!(!desktop.asked.borrow().contains(&"children"));
        // An empty list is a list: nothing is open.
        let empty =
            Fake { client_list: Some(Some(Vec::new())), children: Some(vec![1]), viewable: vec![1], ..Fake::default() };
        assert_eq!(windows_of(&empty), Some(Vec::new()));
    }

    /// gamescope (and a bare X server) keep no client list: the root's viewable children are the windows. Hidden
    /// windows, and one that went away while it was asked, are left out.
    #[test]
    fn without_a_client_list_the_viewable_root_children_are_the_windows() {
        let gamescope = Fake {
            client_list: Some(None),
            children: Some(vec![0x20_0001, 0x40_0005, 0x40_0009, 0x60_0002]),
            viewable: vec![0x40_0005, 0x60_0002],
            titles: vec![
                (0x20_0001, "gamescope"),
                (0x40_0005, "Assetto Corsa"),
                (0x40_0009, "hidden"),
                (0x60_0002, ""),
            ],
            ..Fake::default()
        };
        assert_eq!(windows_of(&gamescope), Some(vec![0x40_0005, 0x60_0002]));
        assert_eq!(titles_of(&gamescope).unwrap(), ["Assetto Corsa", ""]);
        let no_windows = Fake { client_list: Some(None), children: Some(Vec::new()), ..Fake::default() };
        assert_eq!(titles_of(&no_windows), Some(Vec::new()));
    }

    /// A server that does not answer is a failure (the session is dropped and connected again), not an empty or
    /// guessed list.
    #[test]
    fn a_server_that_cannot_be_asked_has_no_list() {
        let dead = Fake { client_list: None, children: Some(vec![1]), viewable: vec![1], ..Fake::default() };
        assert_eq!(windows_of(&dead), None);
        assert!(!dead.asked.borrow().contains(&"children"));
        let half_dead = Fake { client_list: Some(None), children: None, ..Fake::default() };
        assert_eq!(titles_of(&half_dead), None);
    }

    #[test]
    fn local_display_numbers() {
        assert_eq!(local_display(Some(":0")), Some(0));
        assert_eq!(local_display(Some(":1.0")), Some(1));
        assert_eq!(local_display(Some("unix:12")), Some(12));
        for other in ["", "0", ":", ":x", "host:0", "192.168.1.2:0.0", "tcp/host:1"] {
            assert_eq!(local_display(Some(other)), None, "{other:?}");
        }
        assert_eq!(local_display(None), None);
    }

    /// The user's X servers are the sockets `X<n>` they own; other files, other users' servers and a missing
    /// folder name none.
    #[test]
    fn servers_are_the_users_sockets() {
        let directory = tempfile::tempdir().unwrap();
        let sockets = directory.path().join(".X11-unix");
        std::fs::create_dir(&sockets).unwrap();
        let _held: Vec<UnixListener> = ["X12", "X0", "X1", "Xwayland", "X-1", "socket"]
            .iter()
            .map(|name| UnixListener::bind(sockets.join(name)).unwrap())
            .collect();
        std::fs::write(sockets.join("X3"), "").unwrap();
        std::fs::create_dir(sockets.join("X4")).unwrap();
        let user = own_user();
        assert!(user.is_some());
        assert_eq!(local_servers(&sockets, user), [0, 1, 12]);
        assert_eq!(local_servers(&sockets, user.map(|user| user + 1)), Vec::<u32>::new());
        assert_eq!(local_servers(&sockets, None), Vec::<u32>::new());
        assert_eq!(local_servers(&directory.path().join("missing"), user), Vec::<u32>::new());
        // The preferred display's server is not listed twice.
        assert_eq!(other_displays(Some(0), vec![0, 1, 12]), [1, 12]);
        assert_eq!(other_displays(None, vec![0, 1]), [0, 1]);
        assert_eq!(other_displays(Some(5), vec![0, 1]), [0, 1]);
    }

    fn titled(titles: &[&'static str]) -> Fake {
        let windows: Vec<Window> = (1..=titles.len() as Window).collect();
        Fake {
            client_list: Some(None),
            children: Some(windows.clone()),
            viewable: windows.clone(),
            titles: windows.into_iter().zip(titles.iter().copied()).collect(),
            ..Fake::default()
        }
    }

    /// Every reachable server contributes its titles in display order; one that cannot be connected to, or stops
    /// answering, is skipped and tried again, and never stops the others.
    #[test]
    fn other_servers_are_listed_and_failures_stay_local() {
        let mut others = OtherServers::<Fake>::default();
        let connects = RefCell::new(Vec::new());
        let games_up = Cell::new(false);
        let connect = |display: u32| {
            connects.borrow_mut().push(display);
            match display {
                0 => Some(titled(&["Steam Big Picture Mode"])),
                1 if games_up.get() => Some(titled(&["Assetto Corsa", "helper"])),
                _ => None,
            }
        };
        // The games' server refuses (not started yet, or no access): the other still lists.
        assert_eq!(others.titles(&[0, 1, 7], connect), ["Steam Big Picture Mode"]);
        assert_eq!(*connects.borrow(), [0, 1, 7]);
        games_up.set(true);
        assert_eq!(others.titles(&[0, 1, 7], connect), ["Steam Big Picture Mode", "Assetto Corsa", "helper"]);
        assert_eq!(*connects.borrow(), [0, 1, 7, 1, 7], "a kept session is not connected again");
        // A session that stops answering is dropped and connected again on the next call.
        others.sessions.get_mut(&0).unwrap().client_list = None;
        assert_eq!(others.titles(&[0, 1], connect), ["Assetto Corsa", "helper"]);
        assert!(!others.sessions.contains_key(&0));
        assert_eq!(others.titles(&[0, 1], connect), ["Steam Big Picture Mode", "Assetto Corsa", "helper"]);
        // A server whose socket is gone is forgotten.
        assert_eq!(others.titles(&[1], connect), ["Assetto Corsa", "helper"]);
        assert_eq!(others.sessions.keys().copied().collect::<Vec<_>>(), [1]);
        assert!(others.titles(&[], connect).is_empty());
        assert!(others.sessions.is_empty());
    }
}
