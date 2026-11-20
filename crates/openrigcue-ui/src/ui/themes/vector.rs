//! Vector: matte avionics. A slim navigation rail with its system select and instrument note; the start screen
//! leads with the attitude display (the engine-speed instrument for a racing game) and a live channel strip,
//! in a compact instrument-panel treatment. Notifications sit bottom right with a fine severity edge.

use super::super::html::{esc, t, tr};
use super::super::notices::Policy;
use super::super::session::State;
use super::super::shell;
use crate::app::MyApp;

/// Vector's notification rules.
pub const POLICY: Policy = Policy { max_visible: 2, success_ms: 6000 };

/// The rail: brand, system select and the instrument note with the session state.
pub fn nav(app: &MyApp) -> String {
    let state = app.ui.session.state;
    format!(
        "<aside class=\"sidebar\">{}<div><p class=\"eyebrow\">{}</p><nav aria-label=\"{}\">{}</nav></div><div class=\"side-bottom\"><p class=\"rail-note\">{}<br>{}</p><span class=\"pill\"><span class=\"{}\"></span>{}</span></div></aside>",
        shell::brand(),
        t(app, "System select"),
        t(app, "Main"),
        shell::nav_items(app),
        esc(&tr(app, "OpenRigCue / instruments").to_uppercase()),
        esc(&tr(app, "Multi-sim controller").to_uppercase()),
        shell::state_dot(state),
        t(app, state.label())
    )
}

/// The console bar: its name, the place, and the tools.
pub fn top(app: &MyApp) -> String {
    format!(
        "<header class=\"topbar\"><div class=\"topbar-left\"><span class=\"{}\"></span><strong translate=\"no\">ORC / VECTOR</strong><small>{}</small></div>{}</header>",
        shell::state_dot(app.ui.session.state),
        esc(&tr(app, app.ui.screen.key()).to_uppercase()),
        shell::top_actions(app)
    )
}

/// The start screen's heading.
pub fn home_heading(app: &MyApp) -> String {
    format!(
        "<section class=\"page-heading\"><div><div class=\"eyebrow\">{}</div><h1>{}</h1><p>{}</p></div><div class=\"profile single\">{}</div></section>",
        t(app, "Multi-sim / instrument console"),
        esc(&tr(app, "Flight systems").to_uppercase()),
        t(app, "Attitude. Acceleration. Absolute control."),
        shell::profile_select(app)
    )
}

/// The session source, the attitude display (the engine-speed instrument for a racing game) and the channel
/// strip. Without a game the attitude display shows.
pub fn live(app: &MyApp) -> String {
    let s = &app.ui.session;
    let r = &app.ui.readouts;
    let racing = s.racing && s.state != State::Standby;
    let game = if s.state == State::Standby { tr(app, "Stand By") } else { s.game.clone() };
    let speed = if r.live { super::super::html::num(r.speed.round()) } else { "—".into() };
    let unit = (if r.metric { "KM/H" } else { "MPH" }).to_owned();
    let units = if racing && r.live {
        format!("{unit} · {} {}", tr(app, "Gear").to_uppercase(), super::super::session::gear_text(r.gear))
    } else {
        unit
    };
    let metrics: [(&str, f64, &str); 3] = if racing {
        [("Long G", r.longitudinal_g, "G"), ("Lateral G", r.lateral_g, "G"), ("Vert G", r.vertical_g, "G")]
    } else {
        [("Pitch", r.pitch, "°"), ("Roll", r.roll, "°"), ("Yaw", r.yaw, "°")]
    };
    let metrics: String = metrics
        .iter()
        .map(|(label, value, unit)| {
            format!(
                "<div><span>{}</span><b>{}<small>{unit}</small></b></div>",
                esc(&tr(app, label).to_uppercase()),
                if r.live { format!("{value:+.2}") } else { "—".into() }
            )
        })
        .collect();
    format!(
        "<section class=\"hero\" aria-label=\"{label}\"><div class=\"hero-copy\"><span class=\"pill\"><span class=\"{dot}\"></span>{link} / {state}</span><div class=\"eyebrow\">{source}</div><h2>{game}</h2><p>{detail}</p><div class=\"source-meta\"><span>{input} <b>{input_state}</b></span><span>{mode} <b>{mode_state}</b></span></div></div><div class=\"instrument\"><div class=\"instrument-heading\"><span>{heading}</span><span class=\"instrument-tag\">{tag}</span></div><div class=\"instrument-body\"><div class=\"instrument-speed\"><span class=\"instrument-caption\">{caption}</span><strong>{speed}</strong><small>{units}</small></div><div class=\"instrument-display\">{display}</div><div class=\"instrument-metrics\">{metrics}</div></div><div class=\"instrument-footer\"><span><span class=\"{dot}\"></span>{valid}</span><span>{footer}</span></div></div></section>{strip}",
        label = t(app, "Session"),
        dot = shell::state_dot(s.state),
        link = esc(&tr(app, "Data link").to_uppercase()),
        state = esc(&tr(app, s.state.label()).to_uppercase()),
        source = t(app, "Simulation source"),
        game = esc(&game.to_uppercase()),
        detail = shell::session_detail(app),
        input = esc(&tr(app, "Input").to_uppercase()),
        input_state = esc(&tr(app, if r.live { "Live" } else { "Idle" }).to_uppercase()),
        mode = esc(&tr(app, "Mode").to_uppercase()),
        mode_state = esc(&tr(app, if s.state == State::Standby { "Stand By" } else if racing { "Circuit" } else { "Flight" }).to_uppercase()),
        heading = esc(&tr(app, if racing { "Dynamics / powertrain" } else { "Attitude / flight director" }).to_uppercase()),
        tag = esc(&tr(app, if r.live { "Live" } else { "Stand By" }).to_uppercase()),
        caption = esc(&tr(app, if racing { "Velocity" } else { "Indicated velocity" }).to_uppercase()),
        units = esc(&units),
        display = if racing { super::race_display(app) } else { super::flight_display(app) },
        valid = esc(&tr(app, if r.live { "Input valid" } else { "No input" }).to_uppercase()),
        footer = esc(&tr(app, "Telemetry visualization / live").to_uppercase()),
        strip = super::channel_strip(app),
    )
}

/// The tuning panel's heading.
pub fn tuning_head(app: &MyApp) -> (String, String) {
    (esc(&tr(app, "Output calibration").to_uppercase()), t(app, "Profile response / manual tuning"))
}

/// The hardware bus and, on the start screen, the review note.
pub fn rig(app: &MyApp) -> String {
    let note = if app.ui.screen == super::super::Screen::Home {
        format!(
            "<section class=\"aside-note\"><div class=\"eyebrow\">{}</div><h3>{}</h3><p>{}</p><button type=\"button\" class=\"text-button\" id=\"compare\" data-act=\"compare\">{} ↗</button></section>",
            t(app, "Profile / revision"),
            t(app, "Review before you commit."),
            t(app, "Compare the current response with your saved setup. Keep the changes that work."),
            esc(&tr(app, "Compare values").to_uppercase())
        )
    } else {
        String::new()
    };
    format!(
        "<aside class=\"right-column\"><section class=\"panel\"><div class=\"panel-head\"><h2>{}</h2><span class=\"eyebrow\">{}</span></div>{}</section>{note}</aside>",
        esc(&tr(app, "Hardware bus").to_uppercase()),
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
        esc(&tr(app, "Save setup").to_uppercase())
    )
}
