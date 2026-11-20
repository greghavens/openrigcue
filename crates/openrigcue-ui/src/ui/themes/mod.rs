//! The three themes. Each theme is its own frame: its navigation (Apex's sidebar, Vector's slim rail,
//! Redline's horizontal pit-wall bar), its top bar, its session instruments, its rig panel and save bar, its
//! wording and its notification placement. Each theme's module writes its own markup; the style sheet gives
//! each its look and the browser script its motion.

pub mod apex;
pub mod redline;
pub mod vector;

use super::notices::Policy;
use crate::app::MyApp;
use crate::backend::text;

/// A theme of the interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Theme {
    /// Race engineering: carbon black, bone-white controls, hot orange, square edges. The default.
    #[default]
    Redline,
    /// A focused, low-glare cockpit: lime for actions and selection, crisp 4 px cards.
    Apex,
    /// Matte avionics: charcoal green, phosphor accents, fine divisions.
    Vector,
}

impl Theme {
    /// The themes, in the order the Theme select lists them (the default first).
    pub const ALL: [Theme; 3] = [Theme::Redline, Theme::Apex, Theme::Vector];

    /// The value stored as `[APP] theme`, also the CSS class of the frame.
    pub fn id(self) -> &'static str {
        match self {
            Theme::Redline => "redline",
            Theme::Apex => "apex",
            Theme::Vector => "vector",
        }
    }

    /// The name the Theme select shows.
    pub fn name(self) -> &'static str {
        match self {
            Theme::Redline => "Redline",
            Theme::Apex => "Apex",
            Theme::Vector => "Vector",
        }
    }

    /// The theme a stored value selects: a missing, retired ("classic") or unknown value is Redline.
    pub fn from_setting(value: &str) -> Theme {
        let value = value.trim();
        Theme::ALL
            .into_iter()
            .find(|theme| theme.id().eq_ignore_ascii_case(value) || theme.name().eq_ignore_ascii_case(value))
            .unwrap_or_default()
    }

    /// The theme the settings select.
    pub fn configured(app: &MyApp) -> Theme {
        Theme::from_setting(&text(app.setting(&["app", "theme"])))
    }

    /// Whether the theme sets headings in capitals (Vector and Redline).
    pub fn capitals(self) -> bool {
        matches!(self, Theme::Vector | Theme::Redline)
    }
}

/// A theme's notification rules: two toasts at most, successes for six seconds (paused while hovered or
/// focused). Placement and the severity edge are the theme's style.
pub fn policy(theme: Theme) -> Policy {
    match theme {
        Theme::Redline => redline::POLICY,
        Theme::Apex => apex::POLICY,
        Theme::Vector => vector::POLICY,
    }
}

/// The navigation of the theme.
pub fn nav(app: &MyApp) -> String {
    match app.ui.theme {
        Theme::Redline => redline::nav(app),
        Theme::Apex => apex::nav(app),
        Theme::Vector => vector::nav(app),
    }
}

/// The top bar of the theme.
pub fn top(app: &MyApp) -> String {
    match app.ui.theme {
        Theme::Redline => redline::top(app),
        Theme::Apex => apex::top(app),
        Theme::Vector => vector::top(app),
    }
}

/// The start screen's heading of the theme (its wording and the Active profile select).
pub fn home_heading(app: &MyApp) -> String {
    match app.ui.theme {
        Theme::Redline => redline::home_heading(app),
        Theme::Apex => apex::home_heading(app),
        Theme::Vector => vector::home_heading(app),
    }
}

/// The session instruments of the theme (start screen).
pub fn live(app: &MyApp) -> String {
    match app.ui.theme {
        Theme::Redline => redline::live(app),
        Theme::Apex => apex::live(app),
        Theme::Vector => vector::live(app),
    }
}

/// The tuning panel's title and subtitle of the theme.
pub fn tuning_head(app: &MyApp) -> (String, String) {
    match app.ui.theme {
        Theme::Redline => redline::tuning_head(app),
        Theme::Apex => apex::tuning_head(app),
        Theme::Vector => vector::tuning_head(app),
    }
}

/// The right column of the theme: the rig panel, and on the start screen the compare note.
pub fn rig(app: &MyApp) -> String {
    match app.ui.theme {
        Theme::Redline => redline::rig(app),
        Theme::Apex => apex::rig(app),
        Theme::Vector => vector::rig(app),
    }
}

/// The save bar of the theme.
pub fn save(app: &MyApp) -> String {
    match app.ui.theme {
        Theme::Redline => redline::save(app),
        Theme::Apex => apex::save(app),
        Theme::Vector => vector::save(app),
    }
}

/// A screen title as the theme sets it.
pub fn title(app: &MyApp, text: &str) -> String {
    let shown = super::html::tr(app, text);
    super::html::esc(&if app.ui.theme.capitals() { shown.to_uppercase() } else { shown })
}

/// The race instrument: the engine speed arc (its share of the maximum) around the gear, and the rpm.
pub fn race_display(app: &MyApp) -> String {
    let r = &app.ui.readouts;
    let share = if r.live && r.max_rpm > 0.0 { (r.rpm / r.max_rpm).clamp(0.0, 1.0) } else { 0.0 };
    let angle = std::f64::consts::PI * (1.0 - share);
    let (x, y) = (150.0 + 111.0 * angle.cos(), 146.0 - 111.0 * angle.sin());
    let arc = if share > 0.0 {
        format!("<path stroke-width=\"13\" d=\"M39 146A111 111 0 0 1 {x:.1} {y:.1}\"/>")
    } else {
        String::new()
    };
    let gear = if r.live { super::session::gear_text(r.gear) } else { "—".into() };
    let rpm = if r.live {
        format!("{} {}", super::html::num(r.rpm.round()), super::html::tr(app, "RPM"))
    } else {
        "— RPM".into()
    };
    format!(
        "<svg class=\"race-display\" viewBox=\"0 0 300 180\" role=\"img\" aria-label=\"{}\"><g fill=\"none\" stroke=\"currentColor\"><path opacity=\".2\" stroke-width=\"13\" d=\"M39 146A111 111 0 0 1 261 146\"/>{arc}<path opacity=\".4\" d=\"M44 153l13-3M42 110l14 2M57 70l12 7M89 37l8 13M131 22l2 14M175 25l-3 14M214 44l-8 11M244 80l-13 6M258 123l-14 1\"/></g><text x=\"150\" y=\"133\" fill=\"var(--text)\" font-family=\"Barlow\" font-size=\"100\" text-anchor=\"middle\">{}</text><text x=\"150\" y=\"163\" fill=\"currentColor\" font-family=\"monospace\" font-size=\"11\" text-anchor=\"middle\">{}</text></svg>",
        super::html::t(app, "Engine speed and selected gear"),
        super::html::esc(&gear),
        super::html::esc(&rpm),
    )
}

/// The flight instrument: the attitude ladder banked by the roll and shifted by the pitch, and the heading.
pub fn flight_display(app: &MyApp) -> String {
    let r = &app.ui.readouts;
    let (roll, pitch) = if r.live { (r.roll, r.pitch.clamp(-30.0, 30.0)) } else { (0.0, 0.0) };
    let heading =
        if r.live { format!("HDG {:03}", (r.yaw.rem_euclid(360.0)).round() as i64 % 360) } else { "HDG —".into() };
    format!(
        "<svg class=\"hud-display\" viewBox=\"0 0 300 180\" role=\"img\" aria-label=\"{}\"><g fill=\"none\" stroke=\"currentColor\"><path opacity=\".3\" d=\"M62 82A89 89 0 0 1 238 82M62 90h176M150 5v13M90 22l8 12M210 22l-8 12M72 45l12 7M228 45l-12 7\"/><g transform=\"rotate({:.1} 150 90) translate(0 {:.1})\"><path d=\"M53 100h60m74 0h60M99 76h33m36 0h33M113 52h25m24 0h25M99 124h33m36 0h33M113 148h25m24 0h25\" stroke-width=\"1.5\"/><path d=\"M53 100v7m194-7v7M99 76v5m102-5v5\"/><g fill=\"currentColor\" stroke=\"none\" font-family=\"monospace\" font-size=\"9\"><text x=\"82\" y=\"79\">10</text><text x=\"206\" y=\"79\">10</text><text x=\"98\" y=\"55\">20</text><text x=\"192\" y=\"55\">20</text><text x=\"78\" y=\"127\">-10</text><text x=\"205\" y=\"127\">-10</text></g></g><path d=\"M117 90h21l12 8 12-8h21M150 77v6\" stroke-width=\"2\"/><path d=\"M144 19l6-9 6 9z\"/><rect x=\"108\" y=\"160\" width=\"84\" height=\"18\"/></g><text x=\"150\" y=\"173\" fill=\"currentColor\" font-family=\"monospace\" font-size=\"10\" text-anchor=\"middle\">{}</text></svg>",
        super::html::t(app, "Attitude indicator with bank and pitch ladder"),
        -roll,
        pitch * 2.4,
        super::html::esc(&heading),
    )
}

/// The live channel strip under the instruments: roll, pitch and heave, with the inspect button.
pub fn channel_strip(app: &MyApp) -> String {
    let r = &app.ui.readouts;
    let channel = |index: u8, name: &str, value: f64, unit: &str, limit: f64| {
        let shown = if r.live { format!("{value:+05.1}{unit}") } else { format!("—{unit}") };
        let level = if r.live { (value.abs() / limit * 100.0).clamp(0.0, 100.0) } else { 0.0 };
        format!(
            "<div><span>{index:02} / {}</span><b>{}</b><i style=\"--level:{level:.0}%\"></i></div>",
            super::html::esc(&super::html::tr(app, name).to_uppercase()),
            super::html::esc(&shown)
        )
    };
    format!(
        "<div class=\"channel-strip\">{}{}{}<button type=\"button\" id=\"channel-inspect\" data-act=\"inspect\">{} {}</button></div>",
        channel(1, "Roll", r.roll, "°", 30.0),
        channel(2, "Pitch", r.pitch, "°", 30.0),
        channel(3, "Heave", r.vertical_g, " g", 2.0),
        super::html::esc(&super::html::tr(app, "Inspect channels").to_uppercase()),
        super::html::icon("tune"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redline_is_the_default_and_unknown_values_map_to_it() {
        assert_eq!(Theme::default(), Theme::Redline);
        assert_eq!(Theme::from_setting(""), Theme::Redline);
        assert_eq!(Theme::from_setting("classic"), Theme::Redline);
        assert_eq!(Theme::from_setting("Studio"), Theme::Redline);
        assert_eq!(Theme::from_setting("apex"), Theme::Apex);
        assert_eq!(Theme::from_setting("Vector"), Theme::Vector);
        assert_eq!(Theme::from_setting("REDLINE"), Theme::Redline);
    }

    #[test]
    fn every_theme_keeps_two_toasts_and_six_seconds() {
        for theme in Theme::ALL {
            let policy = policy(theme);
            assert_eq!(policy.max_visible, 2);
            assert_eq!(policy.success_ms, 6000);
        }
    }
}
