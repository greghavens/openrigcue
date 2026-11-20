//! Apex: a focused, low-glare cockpit. A left sidebar with the workspace sections and the session note; the
//! start screen leads with the current session and its speed trace; lime marks only actions and selection.
//! Notifications sit bottom right, with a fine severity edge, above the sticky save bar.

use super::super::html::{esc, t, tr};
use super::super::notices::Policy;
use super::super::session::State;
use super::super::shell;
use crate::app::MyApp;

/// Apex's notification rules.
pub const POLICY: Policy = Policy { max_visible: 2, success_ms: 6000 };

/// The sidebar: brand, workspace sections, and the session note.
pub fn nav(app: &MyApp) -> String {
    let state = app.ui.session.state;
    format!(
        "<aside class=\"sidebar\">{}<div><p class=\"eyebrow\">{}</p><nav aria-label=\"{}\">{}</nav></div><div class=\"side-bottom\"><span class=\"pill\"><span class=\"{}\"></span>{}</span><p>{}<br>{}</p></div></aside>",
        shell::brand(),
        t(app, "Workspace"),
        t(app, "Main"),
        shell::nav_items(app),
        shell::state_dot(state),
        t(app, state.label()),
        t(app, "Desktop & companion"),
        t(app, "One tuning workspace."),
    )
}

/// The top bar: the rig, the place, and the tools.
pub fn top(app: &MyApp) -> String {
    format!(
        "<header class=\"topbar\"><div class=\"topbar-left\"><span class=\"{}\"></span><strong>{}</strong><small>/ {}</small></div>{}</header>",
        shell::state_dot(app.ui.session.state),
        t(app, "My simulation rig"),
        t(app, app.ui.screen.key()),
        shell::top_actions(app)
    )
}

/// The start screen's heading.
pub fn home_heading(app: &MyApp) -> String {
    format!(
        "<section class=\"page-heading\"><div><div class=\"eyebrow\">{}</div><h1>{}</h1><p>{}</p></div><div class=\"profile\">{}</div></section>",
        t(app, "Simulation instrument panel"),
        t(app, "Precision, without the noise."),
        t(app, "On the track. In the air. Beyond the atmosphere."),
        shell::profile_select(app)
    )
}

/// The current session and its speed trace.
pub fn live(app: &MyApp) -> String {
    let s = &app.ui.session;
    let r = &app.ui.readouts;
    let game = if s.state == State::Standby { tr(app, "Stand By") } else { s.game.clone() };
    let speed = if r.live { super::super::html::num(r.speed.round()) } else { "—".into() };
    let unit = (if r.metric { "KM/H" } else { "MPH" }).to_owned();
    let units = if s.racing && r.live {
        format!("{unit} · {} {}", tr(app, "Gear").to_uppercase(), super::super::session::gear_text(r.gear))
    } else {
        unit
    };
    format!(
        "<section class=\"hero\" aria-label=\"{}\"><div class=\"hero-copy\"><span class=\"pill\"><span class=\"{}\"></span>{}</span><div class=\"eyebrow\">{}</div><h2>{}</h2><p>{}</p></div><div class=\"telemetry-art\"><div class=\"speed\"><strong>{}</strong><small>{}</small></div><div class=\"wave\"><svg viewBox=\"0 0 240 100\" fill=\"none\" aria-hidden=\"true\"><path d=\"{}\" stroke=\"currentColor\" stroke-width=\"2\"{}/><path d=\"M0 85H240\" stroke=\"currentColor\" opacity=\".2\"/></svg></div><span class=\"telemetry-label\">{}</span></div></section>",
        t(app, "Session"),
        shell::state_dot(s.state),
        t(app, s.state.label()),
        t(app, "Current session"),
        esc(&game),
        shell::session_detail(app),
        esc(&speed),
        esc(&units),
        shell::trace_path(&r.trace),
        if r.live { "" } else { " opacity=\".35\"" },
        esc(&tr(app, "Telemetry / speed trace").to_uppercase()),
    )
}

/// The tuning panel's heading.
pub fn tuning_head(app: &MyApp) -> (String, String) {
    (t(app, "Tune every cue."), t(app, "Tune the response to your simulation."))
}

/// The rig status and, on the start screen, the tuning tip.
pub fn rig(app: &MyApp) -> String {
    let note = if app.ui.screen == super::super::Screen::Home {
        format!(
            "<section class=\"aside-note\"><div class=\"eyebrow\">{}</div><h3>{}</h3><p>{}</p><button type=\"button\" class=\"text-button\" id=\"compare\" data-act=\"compare\">{} ↗</button></section>",
            t(app, "Tuning tip"),
            t(app, "Start with a balanced feel."),
            t(app, "Change one setting at a time. Keep the profile you like before exploring further."),
            t(app, "Compare with saved values")
        )
    } else {
        String::new()
    };
    format!(
        "<aside class=\"right-column\"><section class=\"panel\"><div class=\"panel-head\"><h2>{}</h2><span class=\"eyebrow\">{}</span></div>{}</section>{note}</aside>",
        t(app, "Rig status"),
        shell::device_count(app),
        shell::devices(app)
    )
}

/// The save bar.
pub fn save(app: &MyApp) -> String {
    format!(
        "<div class=\"savebar\"><div class=\"save-state\" id=\"save-state\" aria-live=\"polite\">{}</div><div class=\"buttons\"><button type=\"button\" id=\"reset\" class=\"quiet\" data-act=\"revert\">{}</button><button type=\"button\" id=\"save\" class=\"primary\" data-act=\"save\">{}</button></div></div>",
        shell::save_state(app),
        t(app, "Revert"),
        t(app, "Save profile")
    )
}
