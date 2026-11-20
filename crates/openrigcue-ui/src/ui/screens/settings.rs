//! Settings → Help: About (the version and the credits), the help texts and the mobile companion.

use super::super::components;
use super::super::html::{t, tf};
use super::super::themes;
use crate::app::MyApp;

/// The help texts, naming the screens by the interface's navigation.
const HELP: [&str; 4] = [
    "Setup: choose your devices and sound output under Hardware → Hardware. Select a profile under Tuning, then inspect incoming game values under Games → Telemetry.",
    "Tuning: the Max Telemetry controls set each effect's input range. Autotune measures game telemetry to help choose those ranges.",
    "Audio: select the shaker sound card and speaker layout under Hardware → Hardware. Use the per-speaker Test buttons to check channel routing. OpenRigCue uses its own sounds by default; Use SRS Sound Samples selects recordings from your supplied SRS installation or installer.",
    "Troubleshooting: set the logging level under Settings → App → Logging and inspect log.txt in your configuration folder. The default folder is $XDG_CONFIG_HOME/openrigcue or ~/.config/openrigcue; --config-dir selects another folder. config.ini contains the application settings.",
];

/// Settings → Help: About first, then the help and the mobile companion.
pub fn help_main(app: &MyApp) -> String {
    let help: String = HELP.iter().map(|text| format!("<p>{}</p>", t(app, text))).collect();
    let version =
        tf(app, "OpenRigCue ^version^ — motion cueing and sim effects", &[("version", env!("CARGO_PKG_VERSION"))]);
    let rows = [
        components::field(
            &t(app, "About OpenRigCue"),
            &format!(
                "<p>{version}</p><p class=\"muted\">{}</p>",
                t(app, "Made with FMOD Studio by Firelight Technologies Pty Ltd.")
            ),
            "",
        ),
        components::field(&t(app, "OpenRigCue Help"), &format!("<div class=\"help-text\">{help}</div>"), ""),
        components::field(
            &t(app, "OpenRigCue Mobile Companion"),
            &components::button(&t(app, "Show QR code"), "companion", "", ""),
            &t(app, "Open OpenRigCue on your phone or tablet."),
        ),
    ];
    components::panel(&themes::title(app, "Help"), "", &rows.concat(), "")
}
