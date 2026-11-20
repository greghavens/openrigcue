//! What the routine controllers and the Routines page ask of the desktop:
//! window titles and activation, starting a program (with environment variables expanded), sleeping and
//! finding a window through UI Automation.
//!
//! Window titles come from X11 plus a one-shot UI Automation helper in the selected Wine prefix.
//! Control requests never start telemetry workers, and the helper reports COM failures.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

mod automation;
use automation::Helper;
pub use automation::{Provider, UiaWindow};
use openrigcue_model::automation::Request;

pub use openrigcue_config::detect::WindowActivationError as ActivateError;
use openrigcue_config::detect::{WindowTitles, X11WindowTitles};

/// The desktop services of the routines.
pub trait Desktop: Send + Sync {
    /// Provider failures remain visible rather than becoming a fabricated timeout.
    fn automation_error(&self) -> Option<String> {
        None
    }
    /// The titles of all windows.
    fn all_titles(&self) -> Vec<String>;
    /// Starts a program: an error when it cannot be started.
    fn popen(&self, command: &str) -> std::io::Result<()>;
    /// An environment variable (for variable expansion).
    fn env(&self, name: &str) -> Option<String>;
    /// Sleeps for `duration`.
    fn sleep(&self, duration: Duration);
    /// Searches for the window named `name` (depth 2) for a maximum time at an interval: the
    /// window when it exists.
    fn window_control_exists(
        &self,
        name: Option<&str>,
        max_search_seconds: f64,
        search_interval_seconds: f64,
    ) -> Option<UiaWindow>;
    /// Activates the first window whose title contains `title`.
    fn activate(&self, title: &str) -> Result<(), ActivateError>;
    /// The titles of the windows [`Desktop::activate`] can reach.
    fn activation_titles(&self) -> Vec<String> {
        self.all_titles()
    }
}

/// Whether the window titled `title` cannot be activated because the session is a Wayland session
/// (`WAYLAND_DISPLAY` set, or `XDG_SESSION_TYPE` is `wayland`) and the window is not among those the X11
/// window lookup sees. A window shown through the X compatibility server is still reached.
pub fn window_needs_x11_session(desktop: &dyn Desktop, title: &str) -> bool {
    let wayland = desktop.env("WAYLAND_DISPLAY").is_some_and(|display| !display.is_empty())
        || desktop.env("XDG_SESSION_TYPE").is_some_and(|kind| kind.eq_ignore_ascii_case("wayland"));
    wayland && windows_with_title(&desktop.activation_titles(), title).is_empty()
}

/// The desktop of the running session.
pub struct SystemDesktop {
    titles: Mutex<X11WindowTitles>,
    helper: Arc<Helper>,
    error: Mutex<Option<String>>,
}
impl Default for SystemDesktop {
    fn default() -> Self {
        Self {
            titles: Mutex::new(X11WindowTitles::default()),
            helper: Arc::new(Helper::default()),
            error: Mutex::new(None),
        }
    }
}

impl Desktop for SystemDesktop {
    fn automation_error(&self) -> Option<String> {
        crate::host::lock(&self.error).take()
    }
    fn all_titles(&self) -> Vec<String> {
        let mut titles = crate::host::lock(&self.titles).titles();
        if let Ok(result) = self.helper.request(Request::Titles) {
            for title in result.values {
                if !titles.contains(&title) {
                    titles.push(title)
                }
            }
        }
        titles
    }

    fn popen(&self, command: &str) -> std::io::Result<()> {
        // No shell: the whole string is the program.
        self.helper.request(Request::Launch { command: command.into() }).map(|_| ()).map_err(std::io::Error::other)
    }

    fn env(&self, name: &str) -> Option<String> {
        // Windows environment names are case-insensitive, so a name matches whatever its case.
        let wanted = name.to_uppercase();
        std::env::var(name).ok().or_else(|| {
            std::env::vars_os().find_map(|(key, value)| {
                (key.to_str()?.to_uppercase() == wanted).then(|| value.into_string().ok()).flatten()
            })
        })
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }

    fn window_control_exists(&self, name: Option<&str>, max_search_seconds: f64, interval: f64) -> Option<UiaWindow> {
        let name = name?;
        let result = (|| {
            let provider = self.helper.session()?;
            let window = UiaWindow::cached_root(provider, name.into())?;
            let found = window.exists_for(
                Duration::from_secs_f64(max_search_seconds).as_millis() as u64,
                Duration::from_secs_f64(interval).as_millis() as u64,
            )?;
            Ok::<_, String>(found.then_some(window))
        })();
        match result {
            Ok(window) => window,
            Err(error) => {
                tracing::warn!(%error,"routine automation unavailable");
                *crate::host::lock(&self.error) = Some(error);
                None
            }
        }
    }

    fn activate(&self, title: &str) -> Result<(), ActivateError> {
        crate::host::lock(&self.titles).activate(title)
    }

    fn activation_titles(&self) -> Vec<String> {
        // Activation goes through the X11 window list only.
        crate::host::lock(&self.titles).titles()
    }
}

/// The titles that contain `title`, ignoring case.
pub fn windows_with_title(titles: &[String], title: &str) -> Vec<String> {
    let wanted = title.to_uppercase();
    titles.iter().filter(|t| t.to_uppercase().contains(&wanted)).cloned().collect()
}

/// The name of a controller's window (the same lookup for the three controllers): the first title that starts
/// with `prefix`.
pub fn get_window_name(desktop: &dyn Desktop, prefix: &str) -> Option<String> {
    desktop.all_titles().into_iter().find(|title| title.starts_with(prefix))
}

/// Windows-style variable expansion of a path: `%name%`, `${name}` and `$name` (letters, digits, `_`, `-`)
/// replaced by a defined variable and left as written otherwise, `%%` and `$$` made single, and text between
/// single quotes copied without expansion.
pub fn expandvars(desktop: &dyn Desktop, path: &str) -> String {
    if !path.contains('$') && !path.contains('%') {
        return path.to_owned();
    }
    let is_varchar = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    let mut path: Vec<char> = path.chars().collect();
    let mut res = String::new();
    // `index` may step to -1 before the loop's increment.
    let mut index: isize = 0;
    let text = |chars: &[char]| chars.iter().collect::<String>();
    let find = |chars: &[char], wanted: char| chars.iter().position(|&c| c == wanted);
    while (index as usize) < path.len() {
        let c = path[index as usize];
        let next = path.get(index as usize + 1).copied();
        if c == '\'' {
            path = path[index as usize + 1..].to_vec();
            match find(&path, c) {
                Some(end) => {
                    res.push(c);
                    res.push_str(&text(&path[..=end]));
                    index = end as isize;
                }
                None => {
                    res.push(c);
                    res.push_str(&text(&path));
                    index = path.len() as isize - 1;
                }
            }
        } else if c == '%' {
            if next == Some('%') {
                res.push(c);
                index += 1;
            } else {
                path = path[index as usize + 1..].to_vec();
                match find(&path, '%') {
                    None => {
                        res.push('%');
                        res.push_str(&text(&path));
                        index = path.len() as isize - 1;
                    }
                    Some(end) => {
                        let var = text(&path[..end]);
                        match desktop.env(&var) {
                            Some(value) => res.push_str(&value),
                            None => res.push_str(&format!("%{var}%")),
                        }
                        index = end as isize;
                    }
                }
            }
        } else if c == '$' {
            if next == Some('$') {
                res.push(c);
                index += 1;
            } else if next == Some('{') {
                path = path[index as usize + 2..].to_vec();
                match find(&path, '}') {
                    None => {
                        res.push_str("${");
                        res.push_str(&text(&path));
                        index = path.len() as isize - 1;
                    }
                    Some(end) => {
                        let var = text(&path[..end]);
                        match desktop.env(&var) {
                            Some(value) => res.push_str(&value),
                            None => res.push_str(&format!("${{{var}}}")),
                        }
                        index = end as isize;
                    }
                }
            } else {
                let mut var = String::new();
                index += 1;
                let mut c = path.get(index as usize).copied();
                while let Some(ch) = c.filter(|&ch| is_varchar(ch)) {
                    var.push(ch);
                    index += 1;
                    c = path.get(index as usize).copied();
                }
                match desktop.env(&var) {
                    Some(value) => res.push_str(&value),
                    None => {
                        res.push('$');
                        res.push_str(&var);
                    }
                }
                if c.is_some() {
                    index -= 1;
                }
            }
        } else {
            res.push(c);
        }
        index += 1;
    }
    res
}

/// The window strings of one controller's `go_to_profile_tab`.
pub struct ProgramWindow {
    /// The window title prefix the window is looked up by.
    pub prefix: &'static str,
    /// The program to start.
    pub exe: &'static str,
    /// The program could not be started.
    pub cannot_find: &'static str,
    /// The wait for the window ended at its ninth step.
    pub wait_too_long: &'static str,
    /// The automation search (3 s, every 1 s) did not find the window.
    pub exists_too_long: &'static str,
}

/// The common start of `go_to_profile_tab`: the window by its title, else the program started and its window
/// waited for (up to ten 1 s steps), then the automation search for the window by name (depth 2; 3 s, every 1 s).
pub fn find_program_window(desktop: &dyn Desktop, program: &ProgramWindow, trace: bool) -> Result<UiaWindow, String> {
    find_program_window_result(desktop, program, trace).map_err(super::RoutineFailure::message)
}

pub(super) fn find_program_window_result(
    desktop: &dyn Desktop,
    program: &ProgramWindow,
    trace: bool,
) -> super::RoutineResult<UiaWindow> {
    use super::RoutineFailure::{Raised, Rejected};
    let mut window_name = get_window_name(desktop, program.prefix);
    if window_name.is_none() {
        if desktop.popen(&expandvars(desktop, program.exe)).is_err() {
            return Err(Rejected(program.cannot_find.to_owned()));
        }
        let mut idx = 0;
        while idx < 10 {
            window_name = get_window_name(desktop, program.prefix);
            if window_name.is_some() {
                break;
            }
            idx += 1;
            desktop.sleep(Duration::from_secs(1));
        }
        // The ninth step is the failing one (ten misses leave the index at 10, so the wait falls through to the window search).
        if idx == 9 {
            return Err(Rejected(program.wait_too_long.to_owned()));
        }
    }
    if trace {
        // The Crew Chief controller logs `inside profile tab`, reads the title again, and logs
        // `window name is …` and `waiting app window`.
        tracing::debug!("inside profile tab");
        window_name = get_window_name(desktop, program.prefix);
        tracing::debug!(?window_name, "window name is");
        tracing::debug!("waiting app window");
    }
    match desktop.window_control_exists(window_name.as_deref(), 3.0, 1.0) {
        // Activating, focusing and raising the window, and what follows, work on the automation window.
        Some(window) => {
            window.activate().map_err(Raised)?;
            window.focus().map_err(Raised)?;
            window.topmost().map_err(Raised)?;
            Ok(window)
        }
        None => Err(match desktop.automation_error() {
            Some(error) => Raised(error),
            None => Rejected(program.exists_too_long.to_owned()),
        }),
    }
}

#[cfg(test)]
pub mod fake {
    //! A scripted desktop for the tests.

    use super::*;

    #[derive(Default)]
    pub struct FakeDesktop {
        pub titles: Mutex<Vec<String>>,
        pub env: Vec<(String, String)>,
        pub popen_ok: bool,
        pub activation_succeeds: bool,
        pub calls: Mutex<Vec<String>>,
    }

    impl FakeDesktop {
        pub fn with_titles(titles: &[&str]) -> Self {
            let titles = titles.iter().map(|t| (*t).to_owned()).collect();
            FakeDesktop { titles: Mutex::new(titles), ..Default::default() }
        }
        pub fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        fn record(&self, call: String) {
            self.calls.lock().unwrap().push(call);
        }
    }

    impl Desktop for FakeDesktop {
        fn all_titles(&self) -> Vec<String> {
            self.titles.lock().unwrap().clone()
        }
        fn popen(&self, command: &str) -> std::io::Result<()> {
            self.record(format!("popen {command}"));
            if self.popen_ok {
                Ok(())
            } else {
                Err(std::io::Error::from(std::io::ErrorKind::NotFound))
            }
        }
        fn env(&self, name: &str) -> Option<String> {
            self.env.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
        }
        fn sleep(&self, duration: Duration) {
            self.record(format!("sleep {}", duration.as_secs_f64()));
        }
        fn window_control_exists(&self, name: Option<&str>, max: f64, interval: f64) -> Option<UiaWindow> {
            self.record(format!("exists {name:?} {max} {interval}"));
            None
        }
        fn activate(&self, title: &str) -> Result<(), ActivateError> {
            self.record(format!("activate {title}"));
            if windows_with_title(&self.all_titles(), title).is_empty() {
                Err(ActivateError::NoWindow)
            } else if self.activation_succeeds {
                Ok(())
            } else {
                Err(ActivateError::Failed)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeDesktop;
    use super::*;

    /// Variable expansion: defined `%name%` replaced, undefined kept, `%%` kept.
    #[test]
    fn expandvars_follows_ntpath() {
        let desktop = FakeDesktop { env: vec![("ProgramFiles(x86)".into(), "C:\\PF".into())], ..Default::default() };
        assert_eq!(expandvars(&desktop, "%ProgramFiles(x86)%\\Fanatec"), "C:\\PF\\Fanatec");
        assert_eq!(expandvars(&desktop, "%Nope%\\x"), "%Nope%\\x");
        assert_eq!(expandvars(&desktop, "a%%b"), "a%b");
        assert_eq!(expandvars(&desktop, "50%"), "50%");
        assert_eq!(expandvars(&desktop, "%ProgramFiles(x86)"), "%ProgramFiles(x86)");
    }

    /// Variable expansion: `$name`, `${name}`, `$$`, single quotes.
    #[test]
    fn expandvars_dollar_and_quotes() {
        let desktop =
            FakeDesktop { env: vec![("HOME".into(), "H".into()), ("A-B".into(), "x".into())], ..Default::default() };
        assert_eq!(expandvars(&desktop, "$HOME\\a"), "H\\a");
        assert_eq!(expandvars(&desktop, "${HOME}b"), "Hb");
        assert_eq!(expandvars(&desktop, "$A-B.c"), "x.c");
        assert_eq!(expandvars(&desktop, "$NOPE/${NOPE}"), "$NOPE/${NOPE}");
        assert_eq!(expandvars(&desktop, "a$$b"), "a$b");
        assert_eq!(expandvars(&desktop, "${HOME"), "${HOME");
        assert_eq!(expandvars(&desktop, "$"), "$");
        assert_eq!(expandvars(&desktop, "'$HOME'$HOME"), "'$HOME'H");
        assert_eq!(expandvars(&desktop, "'$HOME"), "'$HOME");
    }

    /// Windows by title: case-insensitive substring; `get_window_name`: a prefix match.
    #[test]
    fn window_lookups() {
        let titles = vec!["FanaLab V1.66".to_owned(), "OpenRigCue — Browser".to_owned()];
        assert_eq!(windows_with_title(&titles, "openrigcue"), vec!["OpenRigCue — Browser".to_owned()]);
        let desktop = FakeDesktop { titles: Mutex::new(titles), ..Default::default() };
        assert_eq!(get_window_name(&desktop, "FanaLab V1."), Some("FanaLab V1.66".to_owned()));
        assert_eq!(get_window_name(&desktop, "V1."), None);
    }

    const PROGRAM: ProgramWindow = ProgramWindow {
        prefix: "FanaLab V1.",
        exe: "%ProgramFiles(x86)%\\Fanatec\\FanaLab\\Control\\FanaLab.exe",
        cannot_find: "cannot find",
        wait_too_long: "wait",
        exists_too_long: "exists",
    };

    /// Finding a controller's window: no window and the program failing to start.
    #[test]
    fn program_not_found() {
        let desktop = FakeDesktop::default();
        assert_eq!(find_program_window(&desktop, &PROGRAM, false).unwrap_err(), "cannot find");
        assert_eq!(desktop.calls(), ["popen %ProgramFiles(x86)%\\Fanatec\\FanaLab\\Control\\FanaLab.exe"]);
    }

    /// Finding a controller's window: the program started but no window in ten 1 s steps (the index ends at 10, not 9): the
    /// window search runs with no name.
    #[test]
    fn program_started_without_window() {
        let desktop = FakeDesktop { popen_ok: true, ..Default::default() };
        assert_eq!(find_program_window(&desktop, &PROGRAM, false).unwrap_err(), "exists");
        let calls = desktop.calls();
        assert_eq!(calls.iter().filter(|c| *c == "sleep 1").count(), 10);
        assert_eq!(calls.last().unwrap(), "exists None 3 1");
    }

    /// Finding a controller's window: the window is open: no program start, the automation search by its title.
    #[test]
    fn window_already_open() {
        let desktop = FakeDesktop { titles: Mutex::new(vec!["FanaLab V1.66".into()]), ..Default::default() };
        assert_eq!(find_program_window(&desktop, &PROGRAM, false).unwrap_err(), "exists");
        assert_eq!(desktop.calls(), ["exists Some(\"FanaLab V1.66\") 3 1"]);
    }
}

#[cfg(test)]
mod native_session_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn system_desktop_exists_returns_the_same_retained_routine_session() {
        let dir = std::env::temp_dir().join(format!("openrigcue-system-desktop-session-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let runner = dir.join("runner");
        std::fs::write(
            &runner,
            r#"#!/bin/sh
while IFS= read -r request; do
 printf '%s\n' "$request" >> "$1.requests"
 case "$request" in
  *'"operation":"root"'*) printf '%s\n' '{"found":true,"controls":[0]}';;
  *'"operation":"exists"'*) printf '%s\n' '{"found":true}';;
  *) printf '%s\n' '{"found":true,"values":["Retained window"]}';;
 esac
done
"#,
        )
        .unwrap();
        std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o700)).unwrap();
        let desktop = SystemDesktop {
            titles: Mutex::new(X11WindowTitles::default()),
            helper: Arc::new(Helper { runner: runner.into_os_string(), executable: dir.join("helper"), prefix: None }),
            error: Mutex::new(None),
        };
        let window = desktop.window_control_exists(Some("Fixture"), 3.0, 1.0).unwrap();
        assert_eq!(window.name().unwrap(), "Retained window");
        drop(window);
        let requests: Vec<openrigcue_model::automation::SessionRequest> =
            std::fs::read_to_string(dir.join("helper.requests"))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
        assert_eq!(
            requests[1],
            openrigcue_model::automation::SessionRequest::Exists { control: 0, timeout_ms: 3000, interval_ms: 1000 }
        );
        assert!(matches!(requests[2], openrigcue_model::automation::SessionRequest::Execute { control: 0, .. }));
        assert!(desktop.automation_error().is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
