//! Setup → Hardware.

use super::{scale, Action, Choice, Control, Field, Page, Read, Section, Write};

/// The rumble group labels, in order: what a SIM3D port can drive.
pub const RUMBLE_PORT_GROUPS: [&str; 8] =
    ["None", "Throttle", "Brake", "Clutch", "Seat", "Wind Left", "Wind Right", "Wind Center"];

/// The motion test sliders: test slider key, label, left and right text.
pub const MOTION_TEST_SLIDERS: [(&str, &str, &str, &str); 8] = [
    ("pitch", "Test Pitch", "Bwd", "Fwd"),
    ("roll", "Test Roll", "Left", "Right"),
    ("yaw", "Test Yaw", "Rear Left", "Rear Right"),
    ("sway", "Test Lat G Sway", "Left", "Right"),
    ("surge", "Test Long G Surge", "Bwd", "Fwd"),
    ("heave", "Test Vert G Heave", "Down", "Up"),
    ("traction_loss", "Test Traction Loss", "Rear Left", "Rear Right"),
    ("traction_loss_front", "Test Traction Loss", "Front Left", "Front Right"),
];

/// The shaker channel rows of the hardware page, in order, with the API speaker name.
pub const SPEAKER_CHANNELS: [(&str, &str); 6] =
    [("fl", "FL"), ("fr", "FR"), ("ce", "CE"), ("sub", "SUB"), ("rl", "RL"), ("rr", "RR")];

/// The "Enabled" (auto search) switch and the "Com Port" dropdown of a device section. `section` is the
/// settings section, `key` the section name used in the content keys.
fn device_rows(key: &str, section: &str, with_port: bool) -> Vec<Control> {
    let mut rows = vec![Control::switch(
        &format!("auto_search_{key}"),
        "Enabled",
        Field::settings(&[section, "auto_search"]),
        Write::Bool,
    )];
    if with_port {
        rows.push(Control::select(
            &format!("com_port_{key}"),
            "Com Port",
            Vec::new(),
            Field::settings(&[section, "serial_port"]),
        ));
    }
    rows
}

/// The Hardware page (device sections in supported-hardware order) and what its rows store.
pub fn hardware_page() -> Page {
    let percent = scale(0.0, 100.0, 1.0);
    let mut intellibox = device_rows("INTELLIBOX", "wind", true);
    intellibox.extend([
        Control::select(
            "wind_type",
            "Type",
            vec![
                Choice::new("Fan", "F"),
                Choice::new("Blower (discontinued)", "B"),
                Choice::new("Turbo (Fan + Blower) (discontinued)", "T"),
            ],
            Field::settings(&["wind", "fan_kind"]),
        )
        .with_button("Test", Action::HardwareTest { target: "wind".into() }),
        Control::select(
            "nbr_fans",
            "# Fans / Blowers",
            vec![Choice::new("1", 1), Choice::new("2+", 2)],
            Field::settings(&["wind", "fan_count"]),
        ),
        Control::select(
            "led_model",
            "Model",
            vec![Choice::new("RGB", "RGB"), Choice::new("GRB", "GRB")],
            Field::settings(&["leds", "model"]),
        )
        .quietly(),
        Control::number("nbr_leds", "# Leds", (1.0, 150.0, 1.0), Field::settings(&["leds", "nbr_leds"]))
            .with_button("Test", Action::HardwareTest { target: "led".into() }),
        Control::slider("brightness", "Brightness", percent, Field::settings(&["leds", "brightness"]), Write::Int)
            .showing("{}%"),
        Control::slider(
            "shift_light_dim_perc",
            "Brightness",
            percent,
            Field::tune(&["gear_shift_dim_perc"]),
            Write::Number,
        )
        .showing("{} %")
        .with_button("Test", Action::HardwareTest { target: "tach".into() }),
    ]);

    let mut shakers = vec![
        Control::select("soundcard", "Sound Device", Vec::new(), Field::settings(&["shakers", "sound_card"])),
        Control::select("shakers_profile", "Profile", Vec::new(), Field::settings(&["shakers", "speaker_layout"]))
            .with_button("Make a copy of this profile", Action::DuplicateLayout)
            .with_button("Delete the chosen profile", Action::RemoveLayout),
    ];
    shakers.extend(SPEAKER_CHANNELS.iter().map(|(key, speaker)| {
        Control::slider(key, speaker, scale(0.0, 20.0, 1.0), Field::tune(&["shaker_volume", key]), Write::Number)
            .with_button("Test", Action::TestSpeaker { speaker: (*speaker).to_owned() })
            .with_button("Config", Action::SpeakerMatrix { speaker: (*speaker).to_owned() })
    }));
    // The built-in sounds by default, the user's imported SRS recordings on request.
    shakers.push(
        Control::switch(
            "use_srs_sounds",
            "Use SRS Sound Samples",
            Field::settings(&["shakers", "use_srs_sounds"]),
            Write::Bool,
        )
        .hinted("Off: OpenRigCue's built-in shaker sounds. On: the samples imported with the SRS Installer row below, and the SRS FMOD engine sound when the FMOD libraries are installed."),
    );

    let mut motion = device_rows("MOTIONBOX", "motion", false);
    motion.extend([
        Control::picker("motion_manufacturer", "Manufacturer", Vec::new()),
        Control::select("motion_model", "Model", Vec::new(), Field::settings(&["motion", "model"]))
            .with_button("Axis Assignment", Action::AxisAssignment)
            .with_button("Yaw VR", Action::YawVrLimits)
            .with_button("Controller Settings", Action::ThanosSettings)
            .with_button("Move platform to center", Action::YawVrCenter)
            .with_button("Controller Settings", Action::VnmReverse)
            .with_button("Controller Settings", Action::DofRealityKp),
        // The field depends on the manufacturer, so the browser picks it when the port is chosen.
        Control::picker("motion_port", "Vertical PRS200 / 50 Port", Vec::new()),
        Control::select(
            "motion_port2",
            "Horizontal PRS50 Port",
            Vec::new(),
            Field::settings(&["prosimu", "prs200_serial_port_2"]),
        ),
        Control::switch(
            "auto_smoothing_low_speed",
            "Automatic Low-Speed Smoothing",
            Field::settings(&["motion", "auto_smoothing_low_speed"]),
            Write::Bool,
        ),
        Control::switch(
            "invert_tl",
            "Reverse Traction Loss",
            Field::settings(&["prosimu", "invert_traction_loss"]),
            Write::Bool,
        ),
        Control::button("motion_test", "Motion Test", "Start Test", Action::MotionTestToggle),
    ]);
    motion.extend(MOTION_TEST_SLIDERS.iter().map(|(key, label, left, right)| {
        Control::slider(
            key,
            label,
            scale(-100.0, 100.0, 1.0),
            Field::controls(&["tests", "motion_sliders", key]),
            Write::Number,
        )
        .hinted(&format!("{left} / {right}"))
    }));

    let compensation = device_rows("MOTION_COMPENSATION", "compensation", true);

    let mut belt = device_rows("BELT_TENSIONER", "belt", true);
    belt.extend([
        Control::button("belt_test", "Belt Test", "Start Test", Action::BeltTestToggle),
        Control::slider(
            "belt_test_slider",
            "Test",
            percent,
            Field::controls(&["tests", "belt_test_value"]),
            Write::Number,
        ),
        Control::slider(
            "belt_calibration_power_slider",
            "Calibration Power",
            percent,
            Field::settings(&["belt", "calibration_factor"]),
            Write::Int,
        )
        .reading(Read::IntOfFloat),
        Control::slider(
            "belt_calibration_offset_slider",
            "Calibration Retraction",
            percent,
            Field::settings(&["belt", "calibration_retraction"]),
            Write::Int,
        )
        .reading(Read::IntOfFloat),
    ]);

    let mut sim3d = device_rows("SIM3D", "sim3d", true);
    sim3d.push(Control::select(
        "rumble_model",
        "Model",
        ["None", "3 Ports", "4 Ports"].iter().map(|m| Choice::new(m, *m)).collect(),
        Field::settings(&["sim3d", "model"]),
    ));
    for port in 1..=4u8 {
        sim3d.push(
            Control::select(
                &format!("rumble_port{port}"),
                &format!("Port {port}"),
                RUMBLE_PORT_GROUPS.iter().map(|g| Choice::new(g, *g)).collect(),
                Field::settings(&["sim3d", &format!("port{port}")]),
            )
            .with_button("Test", Action::RumbleTest { port }),
        );
    }

    let mut hud = device_rows("HUD", "hud", false);
    hud.push(
        Control::select(
            "hud_model",
            "Model",
            vec![Choice::new("HUD", "H"), Choice::new("BLUETOOTH OBD2 EML327", "B")],
            Field::settings(&["hud", "model"]),
        )
        .quietly(),
    );

    let mut iflag = device_rows("IFLAG", "iflag", true);
    iflag.extend([
        Control::slider("iflag_dim_perc", "Brightness", percent, Field::settings(&["iflag", "brightness"]), Write::Int)
            .showing("{} %"),
        Control::switch("iflag_mirror", "Mirror", Field::settings(&["iflag", "mirror"]), Write::Bool),
        Control::select(
            "iflag_orientation",
            "USB Port Faces",
            vec![Choice::new("Left", 90), Choice::new("Right", 270), Choice::new("Down", 0), Choice::new("Up", 180)],
            Field::settings(&["iflag", "rotation_degrees"]),
        ),
        Control::button("iflag_test", "Test", "Test", Action::IflagTest),
    ]);

    let mut hapticr = device_rows("HAPTICR", "hapticr", true);
    hapticr.extend([
        Control::button("haptic_test_front", "Test Front", "Left", Action::HapticTest { wheel: 1 })
            .with_button("Right", Action::HapticTest { wheel: 2 }),
        Control::button("haptic_test_rear", "Test Rear", "Left", Action::HapticTest { wheel: 3 })
            .with_button("Right", Action::HapticTest { wheel: 4 }),
    ]);

    Page::new(
        "Hardware",
        "Hardware",
        vec![
            Section::keyed("intellibox_container", "Intellibox", intellibox),
            Section::keyed("shakers_container", "Shakers", shakers),
            Section::keyed("motion_container", "Motion", motion),
            Section::keyed("mc_container", "Motion Compensation (WITMotion)", compensation),
            Section::keyed("belt_container", "Belt", belt),
            Section::keyed("sim3d_container", "SIM3D", sim3d),
            Section::keyed("hud_container", "HUD", hud),
            Section::keyed("iflag_container", "IFlag", iflag),
            Section::keyed("hapticr_container", "HapticR", hapticr),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{Store, Widget};

    /// The Hardware page: motion test sliders −100..100 step 1 into the motion test controls, in mapping order.
    #[test]
    fn motion_test_sliders() {
        let page = hardware_page();
        let tests: Vec<&str> = page
            .controls()
            .filter(|c| c.field.as_ref().is_some_and(|f| f.store == Store::Controls && f.path[1] == "motion_sliders"))
            .map(|c| c.id.as_str())
            .collect();
        assert_eq!(tests, ["pitch", "roll", "yaw", "sway", "surge", "heave", "traction_loss", "traction_loss_front"]);
        match &page.control("heave").unwrap().widget {
            Widget::Slider { scale } => assert_eq!((scale.min, scale.max, scale.step), (-100.0, 100.0, 1.0)),
            other => panic!("{other:?}"),
        }
    }

    /// Supported-hardware order: Intellibox 10, Shakebox 20, Motionbox 30, MCMode 40, Belt 50, SIM3D 70, Hud 80,
    /// iFlag 90, HapticR 95.
    #[test]
    fn sections_follow_supported_hardware_order() {
        let page = hardware_page();
        let ids: Vec<&str> = page.sections.iter().map(|s| s.id.as_deref().unwrap()).collect();
        assert_eq!(
            ids,
            [
                "intellibox_container",
                "shakers_container",
                "motion_container",
                "mc_container",
                "belt_container",
                "sim3d_container",
                "hud_container",
                "iflag_container",
                "hapticr_container"
            ]
        );
    }

    /// The shakers section ends with the "Use SRS Sound Samples" switch on
    /// `settings.shakers.use_srs_sounds`, after the six channels.
    #[test]
    fn shakers_sound_source_switch() {
        let page = hardware_page();
        let shakers = page.sections.iter().find(|s| s.id.as_deref() == Some("shakers_container")).unwrap();
        let ids: Vec<&str> = shakers.controls.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["soundcard", "shakers_profile", "fl", "fr", "ce", "sub", "rl", "rr", "use_srs_sounds"]);
        let switch = page.control("use_srs_sounds").unwrap();
        assert!(matches!(switch.widget, Widget::Switch { .. }), "{:?}", switch.widget);
        let field = switch.field.as_ref().unwrap();
        assert_eq!(field.store, Store::Settings);
        assert_eq!(field.path, ["shakers", "use_srs_sounds"]);
    }

    /// The `motion_model` HBox: axis assignment, Yaw VR limits, Thanos, centre, VNM and DOF Reality buttons in that
    /// order; the shakers Profile row with Duplicate / Remove; every channel with Test and the matrix button.
    #[test]
    fn hardware_buttons() {
        let page = hardware_page();
        let actions = |id: &str| -> Vec<Action> {
            page.control(id)
                .unwrap()
                .buttons
                .iter()
                .map(|b| match b {
                    Widget::Button { run, .. } => run.clone(),
                    other => panic!("{other:?}"),
                })
                .collect()
        };
        assert_eq!(
            actions("motion_model"),
            [
                Action::AxisAssignment,
                Action::YawVrLimits,
                Action::ThanosSettings,
                Action::YawVrCenter,
                Action::VnmReverse,
                Action::DofRealityKp
            ]
        );
        assert_eq!(actions("shakers_profile"), [Action::DuplicateLayout, Action::RemoveLayout]);
        assert_eq!(
            actions("sub"),
            [Action::TestSpeaker { speaker: "SUB".into() }, Action::SpeakerMatrix { speaker: "SUB".into() }]
        );
    }

    /// The fan count stores "1" or "2" (the value without its "+"); the iFlag orientation stores the angle.
    #[test]
    fn dropdown_values() {
        let page = hardware_page();
        let options = |id: &str| match &page.control(id).unwrap().widget {
            Widget::Select { options } => {
                options.iter().map(|o| (o.label.clone(), o.value.clone())).collect::<Vec<_>>()
            }
            other => panic!("{other:?}"),
        };
        assert_eq!(options("nbr_fans"), [("1".to_owned(), 1.into()), ("2+".to_owned(), 2.into())]);
        assert_eq!(
            options("iflag_orientation"),
            [
                ("Left".to_owned(), 90.into()),
                ("Right".to_owned(), 270.into()),
                ("Down".to_owned(), 0.into()),
                ("Up".to_owned(), 180.into())
            ]
        );
        assert_eq!(options("hud_model")[1], ("BLUETOOTH OBD2 EML327".to_owned(), "B".into()));
        assert_eq!(options("rumble_port1").len(), 8);
        match &page.control("nbr_leds").unwrap().widget {
            Widget::Number { min, max, step } => assert_eq!((*min, *max, *step), (1.0, 150.0, 1.0)),
            other => panic!("{other:?}"),
        }
    }
}
