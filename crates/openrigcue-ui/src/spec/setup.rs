//! Setup → Motion Compensation, Joystick and App; Games → Telemetry.

use super::{actions, scale, Action, Choice, Control, Field, Page, Read, Section, Write};

/// The tuning entries of the left menu, in order (every entry is visible): the
/// choices of the App page's Startup Page dropdown.
pub const STARTUP_PAGES: [&str; 10] =
    ["Wind", "Shake", "Motion", "Belt", "HapticR", "SIM3D", "IFlag", "LED", "Tach", "Routines"];

/// The log levels, in order.
pub const LOG_LEVELS: [(&str, u32); 5] =
    [("Critical", 50), ("Error", 40), ("Warning", 30), ("Info", 20), ("Debug", 10)];

/// The App page and what its rows store: the version and the theme (General), the startup page, the option switches, voice
/// notifications, the shift sound, the log level and the measurement unit (the language menu is in the top bar).
pub fn app_page() -> Page {
    let switch =
        |id: &str, label: &str, key: &str| Control::switch(id, label, Field::settings(&["app", key]), Write::Bool);
    Page::new(
        "App",
        "App",
        vec![
            Section::new(
                "General",
                vec![
                    Control::readout("version", "App"),
                    Control::select(
                        "theme",
                        "Theme",
                        crate::ui::Theme::ALL.iter().map(|theme| Choice::new(theme.name(), theme.id())).collect(),
                        Field::settings(&["app", "theme"]),
                    ),
                ],
            ),
            Section::new(
                "Startup Options",
                vec![
                    Control::select(
                        "startup_page",
                        "Startup Page",
                        STARTUP_PAGES.iter().map(|p| Choice::new(p, *p)).collect(),
                        Field::settings(&["app", "start_page"]),
                    ),
                    switch("x_button_exit", "Quit When Window Closes", "close_button_quits"),
                    switch("start_minimized", "Start Minimized", "start_minimized"),
                ],
            ),
            Section::new(
                "Alerts / Notifications",
                vec![
                    switch("voice_notifications", "Voice Notifications", "voice_notifications"),
                    switch("shift_sound", "Shift Sound", "gear_shift_sound"),
                ],
            ),
            Section::new("Network", vec![Control::readout("ip", "IP")]),
            Section::new(
                "Logging",
                vec![Control::select(
                    "log_level",
                    "Level",
                    LOG_LEVELS.iter().map(|(label, level)| Choice::new(label, *level)).collect(),
                    Field::settings(&["app", "log_level"]),
                )
                .quietly()],
            ),
            Section::new(
                "Measurement",
                vec![Control::select(
                    "measurement_unit",
                    "Units",
                    vec![Choice::new("km/h", "kmh"), Choice::new("mph", "mph")],
                    Field::settings(&["app", "speed_unit"]),
                )],
            ),
            Section::new(
                "Reset",
                vec![Control::button("reset", "Factory Defaults", "Restore Factory Settings", Action::FactoryReset)],
            ),
            // The Credits separator and the FMOD label are appended unconditionally.
            Section::keyed(
                "credits_container",
                "Credits",
                vec![Control::text("credits", "Made with FMOD Studio by Firelight Technologies Pty Ltd.")],
            ),
        ],
    )
}

/// The compensation axes of the Motion Compensation page (every axis, without the `all` entry): key, label
/// and when the axis has a factor slider (always for sway and surge, only on six-DOF rigs for the rotations, never for heave).
const COMPENSATION_AXES: [(&str, &str, FactorRow); 6] = [
    ("pitch", "Pitch", FactorRow::SixDofOnly),
    ("roll", "Roll", FactorRow::SixDofOnly),
    ("yaw", "Yaw", FactorRow::SixDofOnly),
    ("heave", "Heave", FactorRow::None),
    ("sway", "Sway", FactorRow::Always),
    ("surge", "Surge", FactorRow::Always),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum FactorRow {
    None,
    SixDofOnly,
    Always,
}

/// The Motion Compensation page and what its rows store: each axis's enable and reverse switches and factor
/// slider, the horizontal surge factor and reverse switch, the spin boxes, the roll/pitch swap, the installation
/// mode and the sensor type.
/// `six_dof` is whether the `MOTIONBOX` model is H6 or P6 when the page is built.
pub fn compensation_page(six_dof: bool) -> Page {
    let mut axes = vec![Control::select(
        "sensor_type",
        "Type",
        vec![
            Choice::new("WITMotion", "witmotion"),
            Choice::new("IntelliComp (works without physical hardware)", "virtual"),
        ],
        Field::settings(&["compensation", "sensor"]),
    )
    .quietly()];
    for (axis, label, factor) in COMPENSATION_AXES {
        let switch = Control::switch(axis, label, Field::settings(&["compensation", axis, "enabled"]), Write::Bool);
        // The axis's raw button: pitch, roll and yaw can be calibrated.
        let calibrated = matches!(axis, "pitch" | "roll" | "yaw");
        axes.push(if calibrated {
            switch.with_button("Calibrate", Action::CompensationCalibrate { axis: axis.into() })
        } else {
            switch
        });
        axes.push(
            Control::switch(
                &format!("{axis}_reverse"),
                "",
                Field::settings(&["compensation", axis, "reverse"]),
                Write::Bool,
            )
            .switch_labels("Normal", "Reversed"),
        );
        if factor == FactorRow::Always || (factor == FactorRow::SixDofOnly && six_dof) {
            axes.push(
                Control::slider(
                    &format!("{axis}_factor"),
                    "Factor",
                    scale(0.0, 100.0, 1.0),
                    Field::settings(&["compensation", &format!("{axis}_factor")]),
                    Write::Int,
                )
                .reading(Read::IntOfFloat),
            );
        }
    }
    axes.push(
        Control::slider(
            "surge_horizontal_factor",
            "Horizontal Factor",
            scale(0.0, 100.0, 1.0),
            Field::settings(&["compensation", "surge_horizontal_factor"]),
            Write::Int,
        )
        .reading(Read::IntOfFloat),
    );
    axes.push(
        Control::switch(
            "surge_horizontal_reverse",
            "",
            Field::settings(&["compensation", "surge_horizontal_reverse"]),
            Write::Bool,
        )
        .switch_labels("Normal", "Reversed"),
    );
    let spin = |id: &str, label: &str, max: f64, key: &str| {
        Control::number(id, label, (1.0, max, 1.0), Field::settings(&["compensation", key])).reading(Read::IntOfFloat)
    };
    let mut shortcuts = actions(&[
        "motion_compensation_switch",
        "motion_compensation_yaw_calibrate",
        "motion_compensation_roll_calibrate",
        "motion_compensation_pitch_calibrate",
        "motion_compensation_all_calibrate",
    ]);
    shortcuts.extend(COMPENSATION_AXES.iter().map(|(axis, _, _)| format!("motion_compensation_{axis}_switch")));
    Page::new(
        "Motion Compensation",
        "Motion Compensation",
        vec![
            Section::new("Sensor", axes),
            Section::new(
                "Settings",
                vec![
                    spin("height", "Seated Height (mm)", 1200.0, "height"),
                    Control::switch(
                        "swap",
                        "Swap Roll / Pitch",
                        Field::settings(&["compensation", "swap_roll_pitch"]),
                        Write::Bool,
                    ),
                    Control::select(
                        "installation_mode",
                        "Installation",
                        vec![Choice::new("Horizontal", "H"), Choice::new("Vertical", "V")],
                        Field::settings(&["compensation", "installation"]),
                    )
                    .quietly(),
                    spin(
                        "actuator_distance_left_right",
                        "Left-to-Right Actuator Spacing (mm)",
                        3000.0,
                        "left_right_distance_mm",
                    ),
                    spin(
                        "actuator_distance_front_back",
                        "Front-to-Back Actuator Spacing (mm)",
                        3000.0,
                        "front_back_distance_mm",
                    ),
                    spin(
                        "vertical_actuators_stroke",
                        "Stroke of the Vertical Actuators (mm)",
                        250.0,
                        "actuator_vertical_stroke",
                    ),
                    spin(
                        "surge_stroke",
                        "Stroke of the Horizontal Surge Actuator (mm)",
                        250.0,
                        "surge_horizontal_stroke",
                    ),
                ],
            ),
        ],
    )
    .with_shortcuts(shortcuts, "")
}

/// The Routines page's routines: name and description of each, in the page's order.
pub const ROUTINES: [(&str, &str); 3] = [
    (
        "Fanalab",
        "On entering the game or vehicle, this routine switches the FANALAB app to the profile chosen below automatically",
    ),
    (
        "Simucube 2 True Drive Paddock",
        "On entering the game or vehicle, this routine switches the Simucube 2 True Drive Paddock to the profile chosen below automatically",
    ),
    ("CrewChief", ""),
];

/// The Routines page (Tuning → Routines): the routine table (built by the
/// browser from [`ROUTINES`] and the tune's `routines`) and the disclaimer. The "Premium
/// Subscription Required" row is hidden, as the main loop hides it with Premium.
pub fn routines_page() -> Page {
    let rows = ROUTINES
        .iter()
        .map(|(name, _)| {
            Control::readout(name, name).with_button("Options", Action::RoutineOptions { routine: (*name).to_owned() })
        })
        .collect::<Vec<_>>();
    Page::new(
        "Routines",
        "Routines",
        vec![
            Section::keyed("routines_table", "", rows),
            Section::new(
                "",
                vec![Control::text(
                    "disclaimer",
                    "Routines are an experimental part of OpenRigCue and might be switched off in a later version",
                )],
            ),
        ],
    )
}

/// The Debug page (Setup → Debug): the reload button and the text box it fills.
pub fn debug_page() -> Page {
    Page::new(
        "Debug",
        "Debug",
        vec![Section::new(
            "",
            vec![Control::button("reload", "", "Reload", Action::DebugReload), Control::readout("debug_text", "")],
        )],
    )
}

/// The Joystick page's Status switch (it sets the joystick listener control).
pub fn joystick_page() -> Page {
    Page::new(
        "Joystick",
        "Joystick",
        vec![Section::new(
            "",
            vec![Control::switch("status", "Status", Field::controls(&["switches", "joystick"]), Write::Bool)],
        )],
    )
    .with_shortcuts(actions(&["joystick_switch"]), "")
}

/// The Telemetry page and what its controls do: the capture and export switches, the min/max reset and the
/// export button.
pub fn telemetry_page() -> Page {
    Page::new(
        "Telemetry",
        "Telemetry",
        vec![Section::new(
            "",
            vec![
                Control::switch(
                    "capture_telemetry",
                    "Capture",
                    Field::controls(&["telemetry", "capture"]),
                    Write::Bool,
                )
                .with_button("Reset", Action::TelemetryReset),
                Control::switch("export_telemetry", "Export", Field::controls(&["telemetry", "export"]), Write::Bool),
            ],
        )],
    )
    .with_shortcuts(
        actions(&["telemetry_capture", "telemetry_reset", "telemetry_snapshot", "telemetry_export"]),
        "Shortcuts",
    )
    .with_hinted_header(
        "Save CSV Snapshot",
        Action::TelemetrySnapshot,
        "Capture the current values and save them as CSV",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::Widget;

    /// Motion Compensation: factor sliders for pitch/roll/yaw only on H6/P6, sway/surge always; spin box limits.
    #[test]
    fn compensation_rows() {
        let three_dof = compensation_page(false);
        assert!(three_dof.control("pitch_factor").is_none());
        assert!(three_dof.control("sway_factor").is_some());
        assert!(compensation_page(true).control("yaw_factor").is_some());
        match &three_dof.control("height").unwrap().widget {
            Widget::Number { min, max, .. } => assert_eq!((*min, *max), (1.0, 1200.0)),
            other => panic!("{other:?}"),
        }
        match &three_dof.control("surge_stroke").unwrap().widget {
            Widget::Number { max, .. } => assert_eq!(*max, 250.0),
            other => panic!("{other:?}"),
        }
        let reverse = three_dof.control("pitch_reverse").unwrap();
        assert_eq!(reverse.field.as_ref().unwrap().path, ["compensation", "pitch", "reverse"]);
        assert!(matches!(&reverse.widget, Widget::Switch { off, on } if off == "Normal" && on == "Reversed"));
    }

    /// Motion Compensation: the axis's raw button calibrates the axis, for the axes that can be calibrated
    /// (pitch, roll, yaw); heave, sway and surge have none.
    #[test]
    fn compensation_calibrate_buttons() {
        let page = compensation_page(false);
        for axis in ["pitch", "roll", "yaw"] {
            let buttons = &page.control(axis).unwrap().buttons;
            assert_eq!(buttons.len(), 1, "{axis}");
            assert!(
                matches!(&buttons[0], Widget::Button { run: Action::CompensationCalibrate { axis: a }, .. } if a == axis)
            );
        }
        for axis in ["heave", "sway", "surge"] {
            assert!(page.control(axis).unwrap().buttons.is_empty(), "{axis}");
        }
    }

    /// The App page: log levels Critical..Debug as 50..10, units km/h / mph.
    #[test]
    fn app_rows() {
        let page = app_page();
        match &page.control("log_level").unwrap().widget {
            Widget::Select { options } => {
                assert_eq!(options.first().unwrap().value, 50);
                assert_eq!(options.last().unwrap().label, "Debug");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(page.control("shift_sound").unwrap().field.as_ref().unwrap().path, ["app", "gear_shift_sound"]);
    }

    /// The App page's sections: General (version and theme), Startup Options,
    /// Alerts / Notifications, Network, Logging, Measurement, Reset with "Reset Factory Defaults", Credits with
    /// the FMOD attribution label (always appended; the browser shows the section while the FMOD engine plays);
    /// the language is chosen in the top bar, not on this page.
    #[test]
    fn app_sections_and_factory_reset() {
        let page = app_page();
        let titles: Vec<&str> = page.sections.iter().map(|s| s.title.as_str()).collect();
        let general: Vec<&str> = page.sections[0].controls.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(general, ["version", "theme"]);
        let Widget::Select { options } = &page.control("theme").unwrap().widget else { panic!("theme select") };
        let themes: Vec<(&str, &serde_json::Value)> = options.iter().map(|c| (c.label.as_str(), &c.value)).collect();
        assert_eq!(
            themes,
            [
                ("Redline", &serde_json::json!("redline")),
                ("Apex", &serde_json::json!("apex")),
                ("Vector", &serde_json::json!("vector"))
            ]
        );
        assert_eq!(
            titles,
            [
                "General",
                "Startup Options",
                "Alerts / Notifications",
                "Network",
                "Logging",
                "Measurement",
                "Reset",
                "Credits"
            ]
        );
        assert_eq!(page.sections.last().unwrap().id.as_deref(), Some("credits_container"));
        let credits = page.control("credits").unwrap();
        assert_eq!(credits.label, "Made with FMOD Studio by Firelight Technologies Pty Ltd.");
        assert!(matches!(credits.widget, Widget::Text));
        let reset = page.control("reset").unwrap();
        assert_eq!(reset.label, "Factory Defaults");
        assert!(
            matches!(&reset.widget, Widget::Button { text, run: Action::FactoryReset } if text == "Restore Factory Settings")
        );
        assert!(page.control("language").is_none());
    }
}
