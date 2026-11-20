//! Settings → Debug, offered while the `show_debug_page` custom feature is on: Reload shows the output lines
//! captured for the screen.

use std::sync::Mutex;

use super::super::components;
use super::super::html::{esc, t};
use super::super::themes;
use crate::app::MyApp;
use crate::backend::native_bool;

/// The output lines captured for the Debug screen, oldest first.
pub static CAPTURED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// What the screen keeps between redraws.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct State {
    /// The captured output as of the last Reload (`None` before the first).
    pub output: Option<String>,
}

/// Whether the screen is offered: the `show_debug_page` custom feature, as a boolean (anything else, or a
/// missing value, keeps it hidden).
pub fn enabled(app: &MyApp) -> bool {
    app.backend
        .native_setting("CUSTOM_FEATURES", "show_debug_page")
        .ok()
        .and_then(|value| native_bool(&value).ok())
        .unwrap_or(false)
}

/// The body: Reload and the output it shows.
pub fn main(app: &MyApp) -> String {
    let output = match app.ui.debug.output.as_deref() {
        Some(text) if !text.is_empty() => {
            text.lines().map(|line| format!("<p class=\"mono\">{}</p>", esc(line))).collect::<String>()
        }
        Some(_) => format!("<p class=\"muted\">{}</p>", t(app, "No output has been captured.")),
        None => format!("<p class=\"muted\">{}</p>", t(app, "Press Reload to show the captured output.")),
    };
    let body = components::field(
        &t(app, "Output"),
        &format!(
            "<div class=\"action-row\">{}</div><div id=\"debug-output\" class=\"debug-output\" aria-live=\"polite\">{output}</div>",
            components::button(&t(app, "Reload"), "action", "Debug:reload:", "")
        ),
        "",
    );
    components::panel(&themes::title(app, "Debug"), "", &body, "")
}

/// Reload: the captured lines, joined, become the output.
pub fn action(app: &mut MyApp, control: &str, _index: Option<usize>) -> Result<(), String> {
    match control {
        "reload" => {
            let lines = CAPTURED.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            app.ui.debug.output = Some(lines.join("\n"));
            Ok(())
        }
        _ => Err(format!("no Debug button {control:?}")),
    }
}

#[cfg(test)]
#[path = "debug_tests.rs"]
mod tests;
