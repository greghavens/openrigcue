//! The Tuning pages: Wind, Shake, Motion, Belt, SIM3D, HapticR, IFlag and LED.

use openrigcue_config::catalog::registry::{EffectCategory, Registry};

use super::{
    actions, goto_hardware, scale, shortcut, Action, Choice, Control, Field, Page, Read, Section, Write, HARDWARE_HINT,
};
use crate::slider::RangeScale;

/// The Wind page and what its rows store: the fan power range, the default max speed, wind curving and its
/// effect, the speed factor and the ejection seat switch.
pub fn wind_page() -> Page {
    let percent = RangeScale::new(0.0, 100.0, 1.0);
    Page::new(
        "Wind",
        "Wind",
        vec![
            Section::new(
                "Left / Right",
                vec![
                    Control::range(
                        "fan_power_perc",
                        "Power Range",
                        percent,
                        Field::tune(&["power_wind_min_speed_perc"]),
                        Field::tune(&["power_wind_max_speed_perc"]),
                        Write::Number,
                    )
                    .showing("{lo}% - {hi}%"),
                    Control::readout("fan_actual_perc", "Actual Power")
                        .with_button("-", shortcut("power_wind_down"))
                        .with_button("+", shortcut("power_wind_up")),
                ],
            ),
            Section::new(
                "Center",
                vec![
                    Control::range(
                        "fan_power_center_perc",
                        "Power Range",
                        percent,
                        Field::tune(&["power_center_wind_min_speed_perc"]),
                        Field::tune(&["power_center_wind_max_speed_perc"]),
                        Write::Number,
                    )
                    .showing("{lo}% - {hi}%"),
                    Control::readout("fan_center_actual_perc", "Actual Power")
                        .with_button("-", shortcut("power_center_wind_down"))
                        .with_button("+", shortcut("power_center_wind_up")),
                ],
            ),
            Section::new(
                "Max Speed",
                vec![Control::slider(
                    "default_max_speed",
                    "Default",
                    scale(0.0, 2000.0, 1.0),
                    Field::tune(&["default_max_speed_kmh"]),
                    Write::SpeedFromUnit,
                )
                .reading(Read::SpeedToUnit)
                .showing("{}{unit}")],
            ),
            Section::new(
                "Wind Curving",
                vec![
                    Control::switch("wind_curving", "Status", Field::tune(&["wind_curving"]), Write::BoolInt),
                    Control::slider(
                        "curving_effect",
                        "Curving Effect",
                        scale(1.0, 10.0, 1.0),
                        Field::tune(&["curving_effect"]),
                        Write::Int,
                    ),
                ],
            ),
            Section::new(
                "Speed",
                vec![Control::slider(
                    "speed_factor",
                    "Factor",
                    scale(1.0, 10.0, 1.0),
                    Field::tune(&["speed_factor"]),
                    Write::Int,
                )],
            ),
            Section::new(
                "Special Effects",
                vec![Control::switch(
                    "ejection_seat",
                    "Ejection Seat",
                    Field::tune(&["enable_ejection_seat"]),
                    Write::Bool,
                )
                .with_button("Shortcut", shortcut("ejection_seat"))],
            ),
        ],
    )
    .locked()
    .with_shortcuts(
        actions(&["power_wind_switch", "power_center_wind_switch"]),
        "Shows the keyboard, gamepad, joystick and buttonbox shortcuts that switch the wind on, off or to auto, and lets you change them.",
    )
    .with_hinted_header("Hardware setup", goto_hardware(), HARDWARE_HINT)
}

/// The Shake page (effect rows per `effect_type`, Engine Type after the first separator) and what its rows do:
/// the effect sliders and switches, the engine type, and each effect's test and filter buttons.
pub fn shake_page(registry: &Registry) -> Page {
    let mut sections = Vec::new();
    for (index, category) in
        [EffectCategory::Common, EffectCategory::Racing, EffectCategory::Flight].into_iter().enumerate()
    {
        let mut controls = Vec::new();
        if index == 0 {
            controls.push(Control::select(
                "engine_type",
                "Engine Type",
                vec![Choice::new("Original", "O"), Choice::new("Enhanced", "E")],
                Field::tune(&["shaker_engine_type"]),
            ));
        }
        for effect in registry.shaker_effects.iter().filter(|e| e.category == category) {
            controls.push(
                Control::slider(
                    &effect.id,
                    &effect.label,
                    scale(0.0, 20.0, 1.0),
                    Field::tune(&["shaker_effects", &effect.id]),
                    Write::Number,
                )
                .toggled_by(Field::tune(&["shaker_switch", &effect.id]))
                .with_button("Filter", Action::ShakerFilter { effect: effect.id.clone() }),
            );
            let row = controls.last_mut().expect("pushed above");
            if effect.shortcut {
                *row = row.clone().with_button("Shortcut", shortcut(&effect.id));
            }
            *row = row.clone().with_button("Test", Action::TestShakerEffect { effect: effect.id.clone() });
        }
        sections.push(Section::keyed(category_key(category), category_key(category), controls));
    }
    let mut shortcuts = vec!["shake_box_switch".to_owned()];
    for effect in &registry.shaker_effects {
        shortcuts.push(format!("shake_box_{}_up", effect.id));
        shortcuts.push(format!("shake_box_{}_down", effect.id));
    }
    Page::new("Shake", "Shake", sections).locked().with_shortcuts(shortcuts, "").with_hinted_header(
        "Hardware setup",
        goto_hardware(),
        HARDWARE_HINT,
    )
}

fn category_key(category: EffectCategory) -> &'static str {
    match category {
        EffectCategory::Common => "Common",
        EffectCategory::Racing => "Racing",
        EffectCategory::Flight => "Flight",
    }
}

/// The smoothing rows of the Motion page, in page order.
pub const SMOOTHING_AXES: [&str; 8] =
    ["pitch", "roll", "yaw", "sway", "surge", "heave", "traction_loss", "traction_loss_front"];

/// The vertical-actuator rows of the Motion page.
pub const MAIN_ACTUATOR_AXES: [&str; 4] = ["sway", "surge", "traction_loss", "traction_loss_front"];

/// The effect name in title case with spaces for underscores, except the rear traction loss row, which says "Traction Loss Rear".
fn axis_label(axis: &str) -> String {
    if axis == "traction_loss" {
        return "Traction Loss Rear".to_owned();
    }
    axis.split('_')
        .map(|word| {
            let mut chars = word.chars();
            chars.next().map(|c| c.to_uppercase().collect::<String>() + chars.as_str()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The Motion page and what its rows store: the platform settings and reaction speed, the effect sliders,
/// switches and Max Telemetry boxes, the smoothing factors, the refresh rate, the traction loss activations, the
/// gear smoothing and belt tensioner factors, the motion types, Enhanced G-Forces, the vertical actuators and the
/// spike filters. `smoothing_step` is the `smoothing_slider_step` setting of `CUSTOM_FEATURES`, as an integer.
pub fn motion_page(registry: &Registry, smoothing_step: f64) -> Page {
    let percent = scale(0.0, 100.0, 1.0);
    let platform = Section::new(
        "Platform",
        vec![
            Control::slider(
                "motion_max_power",
                "Max Angle",
                percent,
                Field::tune(&["motion_max_power"]),
                Write::Number,
            ),
            Control::slider(
                "motion_max_horizontal",
                "Max Horizontal",
                percent,
                Field::tune(&["motion_max_horizontal"]),
                Write::Number,
            ),
            Control::slider(
                "motion_smoothing_factor",
                "Overall Smoothing",
                percent,
                Field::tune(&["motion_smoothing_factor"]),
                Write::Number,
            ),
            Control::slider(
                "reaction_speed",
                "Reaction Speed",
                scale(70.0, 100.0, 1.0),
                Field::tune(&["motion_reaction_speed"]),
                Write::Int,
            )
            .also_writing(Field::settings(&["motion", "reaction_speed"])),
            Control::slider(
                "motion_boost_factor",
                "Boost",
                scale(1.0, 100.0, 1.0),
                Field::tune(&["motion_boost_factor"]),
                Write::Number,
            ),
        ],
    );
    let mut sections = vec![platform];
    for category in [EffectCategory::Common, EffectCategory::Racing, EffectCategory::Flight] {
        let mut controls = Vec::new();
        if category == EffectCategory::Common {
            controls.push(
                Control::select(
                    "intellitune",
                    "Max Telemetry",
                    ["Soft", "Medium", "Aggressive"].iter().map(|p| Choice::new(p, *p)).collect(),
                    Field::tune(&["intellitune_motion_preference"]),
                )
                .quietly()
                .with_button("Tuning Help", Action::Goto { top: "Setup".into(), left: "App".into() })
                .with_button("Autotune", Action::Intellitune)
                .with_button("Go to Telemetry", Action::Goto { top: "Games".into(), left: "Telemetry".into() }),
            );
        }
        for effect in registry.motion_effects.iter().filter(|e| e.category == category) {
            let mut row = Control::slider(
                &effect.id,
                &effect.label,
                scale(0.0, 20.0, 1.0),
                Field::tune(&["motion_effects", &effect.id]),
                Write::Number,
            )
            .toggled_by(Field::tune(&["motion_switch", &effect.id]));
            if effect.allow_max {
                row = row.with_max_telemetry(&effect.id);
            } else if effect.shortcut {
                row = row.with_button("Shortcut", shortcut(&effect.id));
            }
            controls.push(row);
        }
        let title = category_key(category);
        sections.push(if category == EffectCategory::Common {
            Section::new("Effects", controls)
        } else {
            Section::keyed(title, title, controls)
        });
    }
    sections.push(Section::new(
        "Smoothing Effects",
        SMOOTHING_AXES
            .iter()
            .map(|axis| {
                Control::slider(
                    &format!("motion_smoothing_{axis}"),
                    &axis_label(axis),
                    scale(0.0, 100.0, smoothing_step),
                    Field::tune(&["motion_effect_smoothing_factor", axis]),
                    Write::Int,
                )
            })
            .collect(),
    ));
    let axis_type = |axis: &str, label: &str| {
        Control::select(
            &format!("{axis}_type"),
            label,
            vec![Choice::new("Angle", "angle"), Choice::new("Rate", "rate")],
            Field::tune(&["motion_effects", &format!("{axis}_type")]),
        )
    };
    sections.push(Section::keyed(
        "advanced_separator",
        "Advanced",
        vec![
            Control::slider(
                "refresh_rate",
                "Motion Refresh Rate",
                scale(30.0, 1000.0, 1.0),
                Field::settings(&["motion", "refresh_rate"]),
                Write::Int,
            ),
            Control::number(
                "min_traction_loss_activation",
                "Activation Threshold - Traction Loss Rear",
                (0.0, 1800.0, 1.0),
                Field::tune(&["min_traction_loss_activation"]),
            ),
            Control::number(
                "min_traction_loss_front_activation",
                "Activation Threshold - Traction Loss Front",
                (0.0, 1800.0, 1.0),
                Field::tune(&["min_traction_loss_front_activation"]),
            ),
            Control::slider(
                "gear_smoothing_factor",
                "Gear Smoothing Factor",
                percent,
                Field::tune(&["gear_smoothing_factor"]),
                Write::Int,
            ),
            Control::slider(
                "belt_tensioner",
                "Belt Tensioner Factor",
                percent,
                Field::tune(&["motion_belt_tensioner"]),
                Write::Int,
            )
            .hinted("At 1 the stroke travel is capped at 10%, at 100 it is capped at 1000%"),
            axis_type("roll", "Roll"),
            axis_type("pitch", "Pitch"),
            Control::switch(
                "enhanced_gforces",
                "Enhanced G-Forces",
                Field::tune(&["motion_enhanced_gforces"]),
                Write::BoolInt,
            ),
        ],
    ));
    sections.push(Section::keyed(
        "motion_main_actuators_separator",
        "effects on the vertical actuators",
        MAIN_ACTUATOR_AXES
            .iter()
            .map(|axis| {
                Control::slider(
                    &format!("motion_main_actuators_{axis}"),
                    &axis_label(axis),
                    percent,
                    Field::tune(&[&format!("motion_main_actuators_{axis}")]),
                    Write::Int,
                )
            })
            .collect(),
    ));
    let spike = |position: &str, label: &str| {
        Control::number(
            &format!("thanos_spike_filter_{position}"),
            label,
            (255.0, 64530.0, 30.0),
            Field::tune(&[&format!("thanos_spike_filter_{position}")]),
        )
        .showing("")
    };
    sections.push(Section::keyed(
        "motion_thanos_separator",
        "CONTROLLER",
        vec![
            spike("vertical", "Spike Filter Vertical"),
            spike("horizontal", "Spike Filter Horizontal (TL)"),
            Control::text("thanos_alert", "* These features become available after a Controller Firmware update"),
        ],
    ));
    let mut shortcuts = actions(&["motion_box_switch", "motion_compensation_switch", "motion_box_homing"]);
    for effect in &registry.motion_effects {
        shortcuts.push(format!("motion_box_{}_up", effect.id));
        shortcuts.push(format!("motion_box_{}_down", effect.id));
    }
    for key in [
        "motion_max_power",
        "motion_max_horizontal",
        "motion_smoothing_factor",
        "reaction_speed",
        "motion_boost_factor",
    ] {
        shortcuts.push(format!("motion_box_{key}_up"));
        shortcuts.push(format!("motion_box_{key}_down"));
    }
    for axis in SMOOTHING_AXES {
        shortcuts.push(format!("motion_box_smoothing_{axis}_up"));
        shortcuts.push(format!("motion_box_smoothing_{axis}_down"));
    }
    Page::new("Motion", "Motion", sections)
        .locked()
        .with_hinted_header("Move platform to center", Action::YawVrCenter, "Move platform to center")
        .with_shortcuts(shortcuts, "")
        .with_hinted_header("Hardware setup", goto_hardware(), HARDWARE_HINT)
}

/// The Belt page and what its rows do: the surge, sway and heave rows, and the calibration button with its
/// confirmation.
pub fn belt_page() -> Page {
    let effect = scale(1.0, 20.0, 1.0);
    let smoothing = scale(0.0, 100.0, 1.0);
    let status = |id: &str, label: &str, key: &str| Control::switch(id, label, Field::tune(&[key]), Write::Bool);
    let slider = |id: &str, label: &str, key: &str, s| Control::slider(id, label, s, Field::tune(&[key]), Write::Int);
    Page::new(
        "Belt",
        "Belt",
        vec![
            Section::new(
                "Surge",
                vec![
                    status("belt_surge_enabled", "Status", "belt_surge_enabled"),
                    status("belt_surge_reverse", "Reverse", "belt_surge_reverse"),
                    slider("belt_surge", "Surge", "belt_surge", effect),
                    slider("belt_surge_smoothing", "Surge Smoothing", "belt_surge_smoothing", smoothing),
                ],
            ),
            Section::new(
                "Sway",
                vec![
                    status("belt_sway_curving_enabled", "Status", "belt_sway_curving_enabled"),
                    status("belt_sway_reverse", "Reverse", "belt_sway_curving_reverse"),
                    slider("belt_curving", "Sway", "belt_sway_curving_factor", effect),
                    slider("belt_sway_smoothing", "Sway Smoothing", "belt_sway_smoothing", smoothing),
                ],
            ),
            Section::new(
                "Heave",
                vec![
                    status("belt_heave_enabled", "Status", "belt_heave_enabled"),
                    status("belt_heave_reverse", "Reverse", "belt_heave_reverse"),
                    slider("belt_heave", "Heave", "belt_heave_factor", effect),
                    slider("belt_heave_smoothing", "Heave Smoothing", "belt_heave_smoothing", smoothing),
                ],
            ),
        ],
    )
    .locked()
    .with_hinted_header("Calibrate", Action::BeltCalibrate, "Run belt calibration")
    .with_shortcuts(
        actions(&[
            "belt_switch",
            "belt_homing",
            "belt_surge_up",
            "belt_surge_down",
            "belt_surge_smoothing_up",
            "belt_surge_smoothing_down",
            "belt_heave_up",
            "belt_heave_down",
            "belt_heave_smoothing_up",
            "belt_heave_smoothing_down",
            "belt_sway_curving_up",
            "belt_sway_curving_down",
            "belt_sway_smoothing_up",
            "belt_sway_smoothing_down",
        ]),
        "Shows the keyboard, gamepad, joystick and buttonbox shortcuts that switch the Belt off or to auto, and lets you change them.",
    )
    .with_hinted_header("Hardware setup", goto_hardware(), HARDWARE_HINT)
}

/// The switch action, then `<prefix><key>_up` for every key, then `<prefix><key>_down` for every key (SIM3D and HapticR).
fn up_then_down(
    switch: &str,
    prefix: &str,
    keys: &[openrigcue_config::catalog::registry::GroupedEffectInfo],
) -> Vec<String> {
    let mut list = vec![switch.to_owned()];
    for direction in ["up", "down"] {
        list.extend(keys.iter().map(|effect| format!("{prefix}{}_{direction}", effect.id)));
    }
    list
}

/// The rumble groups minus `none` and the wind groups, which the SIM3D page skips.
const RUMBLE_GROUPS: [(&str, &str); 4] =
    [("throttle", "Throttle"), ("brake", "Brake"), ("clutch", "Clutch"), ("seat", "Seat")];

/// The haptic groups minus `none`.
const HAPTIC_GROUPS: [(&str, &str); 1] = [("wheel", "Wheels")];

/// The four advanced sections the SIM3D and HapticR pages share; `rpm_start_id` is the RPM-start row's key
/// (`rpm_start_rumble_perc` on SIM3D, `rpm_start_haptic_perc` on HapticR).
fn advanced_sections(effects: &str, rpm_start_id: &str) -> Vec<Section> {
    let percent = scale(0.0, 100.0, 1.0);
    let slider = |id: &str, label: &str, key: &str| {
        Control::slider(id, label, percent, Field::tune(&[effects, key]), Write::Number)
            .reading(Read::IntOfFloat)
            .showing("{} %")
    };
    vec![
        Section::new(
            "Advanced Settings - Engine Haptics",
            vec![
                slider(rpm_start_id, "Rumble Start RPM", "rpm_start_rumble"),
                Control::range(
                    "rpm_fading_perc",
                    "RPM Fade Range",
                    RangeScale::new(0.0, 100.0, 1.0),
                    Field::tune(&[effects, "rpm_start_fading"]),
                    Field::tune(&[effects, "rpm_stop_fading"]),
                    Write::Int,
                )
                .reading(Read::IntOfFloat)
                .showing("{lo}% - {hi}%"),
            ],
        ),
        Section::new(
            "Advanced Settings - Wheel Slip / Lock",
            vec![
                slider(
                    "wheel_slip_min_activation_perc",
                    "Activation Threshold - Wheel Slip",
                    "wheel_slip_min_activation",
                ),
                slider(
                    "wheel_lock_min_activation_perc",
                    "Activation Threshold - Wheel Lock",
                    "wheel_lock_min_activation",
                ),
            ],
        ),
        Section::new(
            "Advanced Settings - Traction Loss",
            vec![slider(
                "traction_loss_min_activation_perc",
                "Activation Threshold - Traction Loss",
                "traction_loss_min_activation",
            )],
        ),
    ]
}

/// The SIM3D page and what its rows store: the effect sliders and switches, the RPM start, the RPM fading
/// range and the minimum activations.
pub fn sim3d_page(registry: &Registry) -> Page {
    let mut sections: Vec<Section> = RUMBLE_GROUPS
        .iter()
        .map(|(group, title)| {
            let controls = registry
                .rumble_effects
                .iter()
                .filter(|e| e.group == *group)
                .map(|e| {
                    Control::slider(
                        &e.id,
                        &e.label,
                        scale(0.0, 20.0, 1.0),
                        Field::tune(&["rumble_effects", &e.id]),
                        Write::Int,
                    )
                    .toggled_by(Field::tune(&["rumble_switch", &e.id]))
                })
                .collect();
            Section::keyed(group, title, controls)
        })
        .collect();
    sections.extend(advanced_sections("rumble_effects", "rpm_start_rumble_perc"));
    Page::new("SIM3D", "SIM3D", sections)
        .locked()
        .with_shortcuts(
            up_then_down("rumble_switch", "rumble_", &registry.rumble_effects),
            "Shows the keyboard, gamepad, joystick and buttonbox shortcuts that switch the Belt off or to auto, and lets you change them.",
        )
        .with_header("Hardware setup", goto_hardware())
}

/// The HapticR page: the SIM3D page's rows, storing in `haptic_effects`/`haptic_switch`.
pub fn haptic_page(registry: &Registry) -> Page {
    let mut sections: Vec<Section> = HAPTIC_GROUPS
        .iter()
        .map(|(group, title)| {
            let controls = registry
                .haptic_effects
                .iter()
                .filter(|e| e.group == *group)
                .map(|e| {
                    Control::slider(
                        &e.id,
                        &e.label,
                        scale(0.0, 20.0, 1.0),
                        Field::tune(&["haptic_effects", &e.id]),
                        Write::Int,
                    )
                    .toggled_by(Field::tune(&["haptic_switch", &e.id]))
                })
                .collect();
            Section::keyed(group, title, controls)
        })
        .collect();
    sections.extend(advanced_sections("haptic_effects", "rpm_start_haptic_perc"));
    Page::new("HapticR", "HapticR", sections)
        .locked()
        .with_shortcuts(
            up_then_down("haptic_switch", "haptic_", &registry.haptic_effects),
            "Shows the keyboard, gamepad, joystick and buttonbox shortcuts that switch the Belt off or to auto, and lets you change them.",
        )
        .with_header("Hardware setup", goto_hardware())
}

/// The iFlag groups, in order.
const FLAG_GROUPS: [(&str, &str); 5] =
    [("rpm", "RPM"), ("start", "Start Lights"), ("flag", "Flags"), ("pit", "Pits"), ("spotter", "Spotter")];

/// The iFlag page and its switches (the flag's entry in the tune takes the switch's boolean).
pub fn iflag_page(registry: &Registry) -> Page {
    let mut sections: Vec<Section> = FLAG_GROUPS
        .iter()
        .map(|(group, title)| {
            let controls = registry
                .flags
                .iter()
                .filter(|f| f.group == *group)
                .map(|f| Control::switch(&f.name, &f.description, Field::tune(&["iflag", &f.name]), Write::Bool))
                .collect();
            Section::keyed(group, title, controls)
        })
        .collect();
    sections.push(Section::new("Credits", vec![Control::text("credits", "The iFlag is a design by Petr Vostřel.")]));
    Page::new("IFlag", "IFlag", sections).locked().with_header("Hardware setup", goto_hardware())
}

/// The LED segment types, in order, with their stored names.
pub const LED_SEGMENT_KINDS: [(&str, &str); 10] = [
    ("RPM", "rpm"),
    ("Speed", "speed"),
    ("Flag", "flag"),
    ("RPM + Flag", "rpm_flag"),
    ("Speed + Flag", "speed_flag"),
    ("Spotter Right", "spotter_right"),
    ("Spotter Left", "spotter_left"),
    ("Spotter Right + Flag", "spotter_right_flag"),
    ("Spotter Left + Flag", "spotter_left_flag"),
    ("Off", "off"),
];

/// The LED page and what its rows store: the gear shift, the number of segments, each segment's range, type
/// and colour. The LED page stays editable in stand-by
/// (it is enabled in test mode).
pub fn led_page() -> Page {
    let mut sections = vec![Section::new(
        "",
        vec![
            Control::slider(
                "gear_shift_perc",
                "Shift Point Percentage",
                scale(0.0, 100.0, 1.0),
                Field::tune(&["gear_shift_perc"]),
                Write::Number,
            )
            .showing("{} %"),
            Control::number("nbr_segments", "# Segments", (1.0, 4.0, 1.0), Field::settings(&["leds", "nbr_segments"])),
        ],
    )];
    for index in 0..4u8 {
        let segment = index.to_string();
        let number = index + 1;
        let field = |key: &str| Field::settings(&["leds", "segments", &segment, key]);
        sections.push(Section::keyed(
            &format!("segment{number}"),
            &format!("Segment {number}"),
            vec![
                Control::range(
                    &format!("segment{number}"),
                    "Position",
                    RangeScale::new(1.0, 150.0, 1.0),
                    field("start"),
                    field("end"),
                    Write::Int,
                )
                .showing("{lo} - {hi}"),
                Control::select(
                    &format!("segment{number}dropdown"),
                    "Type",
                    LED_SEGMENT_KINDS.iter().map(|(label, value)| Choice::new(label, *value)).collect(),
                    field("kind"),
                ),
                Control::button(
                    &format!("segment{number}color"),
                    "Color",
                    "Always On",
                    Action::LedColor { segment: index },
                ),
            ],
        ));
    }
    Page::new("LED", "Led", sections)
        .with_shortcuts(
            actions(&["led_switch"]),
            "Shows the keyboard, gamepad, joystick and buttonbox shortcuts that switch the LED strip on, off or to auto, and lets you change them.",
        )
        .with_hinted_header("Hardware setup", goto_hardware(), HARDWARE_HINT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{Store, Widget};

    fn registry() -> &'static Registry {
        Registry::builtin()
    }

    fn slider_of(page: &Page, id: &str) -> (f64, f64, f64) {
        match &page.control(id).unwrap_or_else(|| panic!("{id}")).widget {
            Widget::Slider { scale } => (scale.min, scale.max, scale.step),
            Widget::Range { scale, .. } => (scale.limit_min, scale.limit_max, scale.step),
            other => panic!("{id} is {other:?}"),
        }
    }

    /// Wind: Power Range 0..100 step 1, Default max speed 0..2000, curving effect and factor 1..10.
    #[test]
    fn wind_page_ranges() {
        let page = wind_page();
        assert_eq!(slider_of(&page, "fan_power_perc"), (0.0, 100.0, 1.0));
        assert_eq!(slider_of(&page, "fan_power_center_perc"), (0.0, 100.0, 1.0));
        assert_eq!(slider_of(&page, "default_max_speed"), (0.0, 2000.0, 1.0));
        assert_eq!(slider_of(&page, "curving_effect"), (1.0, 10.0, 1.0));
        assert_eq!(slider_of(&page, "speed_factor"), (1.0, 10.0, 1.0));
        let speed = page.control("default_max_speed").unwrap();
        assert_eq!((speed.write, speed.read), (Write::SpeedFromUnit, Read::SpeedToUnit));
        let range = page.control("fan_power_perc").unwrap();
        assert_eq!(range.field.as_ref().unwrap().path, ["power_wind_min_speed_perc"]);
        assert!(matches!(&range.widget, Widget::Range { high, .. } if high.path == ["power_wind_max_speed_perc"]));
        assert!(page.locked_in_standby);
    }

    /// Shake: Engine Type first, then every effect_mapping entry by effect_type, 0..20 step 1.
    #[test]
    fn shake_page_rows() {
        let page = shake_page(registry());
        let ids: Vec<&str> = page.controls().map(|c| c.id.as_str()).collect();
        assert_eq!(ids[0], "engine_type");
        assert_eq!(
            &ids[1..],
            [
                "engine",
                "road_bump",
                "impact",
                "acceleration",
                "deceleration",
                "lateral_acceleration",
                "gear",
                "suspension",
                "traction_loss",
                "wheel_slip",
                "abs_lock",
                "afterburner",
                "cannon_firing",
                "canopy",
                "landing_gear",
                "missile_firing",
                "touchdown",
                "turbulence"
            ]
        );
        assert_eq!(slider_of(&page, "touchdown"), (0.0, 20.0, 1.0));
        let engine = page.control("engine").unwrap();
        assert_eq!(engine.toggle.as_ref().unwrap().field.path, ["shaker_switch", "engine"]);
        assert_eq!(engine.write, Write::Number, "on_slider_change stores the value as is");
    }

    /// Motion: platform sliders, reaction speed 70..100 writing MOTIONBOX.reaction_speed too, effect
    /// sliders 0..20, max-telemetry boxes only where allow_max, smoothing step from CUSTOM_FEATURES.
    #[test]
    fn motion_page_ranges() {
        let page = motion_page(registry(), 5.0);
        assert_eq!(slider_of(&page, "motion_boost_factor"), (1.0, 100.0, 1.0));
        assert_eq!(slider_of(&page, "reaction_speed"), (70.0, 100.0, 1.0));
        let reaction = page.control("reaction_speed").unwrap();
        assert_eq!(reaction.also, [Field::settings(&["motion", "reaction_speed"])]);
        assert_eq!(slider_of(&page, "pitch"), (0.0, 20.0, 1.0));
        assert!(page.control("pitch").unwrap().max_telemetry.is_some());
        assert!(page.control("wheel_slip").unwrap().max_telemetry.is_none());
        assert_eq!(slider_of(&page, "motion_smoothing_yaw"), (0.0, 100.0, 5.0));
        assert_eq!(page.control("motion_smoothing_traction_loss").unwrap().label, "Traction Loss Rear");
        assert_eq!(page.control("motion_smoothing_traction_loss_front").unwrap().label, "Traction Loss Front");
        assert_eq!(slider_of(&page, "refresh_rate"), (30.0, 1000.0, 1.0));
        assert_eq!(page.control("refresh_rate").unwrap().field.as_ref().unwrap().store, Store::Settings);
        match &page.control("thanos_spike_filter_vertical").unwrap().widget {
            Widget::Number { min, max, step } => assert_eq!((*min, *max, *step), (255.0, 64530.0, 30.0)),
            other => panic!("{other:?}"),
        }
        match &page.control("min_traction_loss_activation").unwrap().widget {
            Widget::Number { min, max, step } => assert_eq!((*min, *max, *step), (0.0, 1800.0, 1.0)),
            other => panic!("{other:?}"),
        }
        let racing: Vec<&str> = page
            .sections
            .iter()
            .find(|s| s.id.as_deref() == Some("Racing"))
            .unwrap()
            .controls
            .iter()
            .map(|c| c.id.as_str())
            .collect();
        assert_eq!(racing, ["traction_loss", "traction_loss_front", "wheel_slip", "rumble_strips", "abs_lock"]);
    }

    /// Belt: effect sliders 1..20, smoothing 0..100, all written as integers.
    #[test]
    fn belt_page_ranges() {
        let page = belt_page();
        assert_eq!(slider_of(&page, "belt_surge"), (1.0, 20.0, 1.0));
        assert_eq!(slider_of(&page, "belt_heave_smoothing"), (0.0, 100.0, 1.0));
        assert_eq!(page.control("belt_curving").unwrap().field.as_ref().unwrap().path, ["belt_sway_curving_factor"]);
        assert_eq!(
            page.control("belt_sway_reverse").unwrap().field.as_ref().unwrap().path,
            ["belt_sway_curving_reverse"]
        );
    }

    /// SIM3D / HapticR: grouped effect sliders and the shared advanced rows.
    #[test]
    fn rumble_and_haptic_pages() {
        let sim3d = sim3d_page(registry());
        assert_eq!(
            sim3d.sections.iter().map(|s| s.title.as_str()).collect::<Vec<_>>()[..4],
            ["Throttle", "Brake", "Clutch", "Seat"]
        );
        assert_eq!(slider_of(&sim3d, "clutch_gear"), (0.0, 20.0, 1.0));
        assert_eq!(sim3d.control("clutch_gear").unwrap().write, Write::Int);
        let fading = sim3d.control("rpm_fading_perc").unwrap();
        assert_eq!((fading.write, fading.read), (Write::Int, Read::IntOfFloat));
        let start = sim3d.control("rpm_start_rumble_perc").unwrap();
        assert_eq!((start.write, start.read), (Write::Number, Read::IntOfFloat));
        let haptic = haptic_page(registry());
        assert!(haptic.control("rpm_start_haptic_perc").is_some());
        assert_eq!(haptic.control("wheel_engine").unwrap().label, "Engine Haptic");
    }

    /// iFlag: one switch per flag whose group is not none, grouped rpm/start/flag/pit/spotter.
    #[test]
    fn iflag_page_switches() {
        let page = iflag_page(registry());
        let flags: Vec<&str> = page.controls().filter(|c| c.field.is_some()).map(|c| c.id.as_str()).collect();
        assert_eq!(flags.len(), 26);
        assert_eq!(&flags[..3], ["rpm", "gear_display", "sli"]);
        assert!(flags.contains(&"caution_waving") && !flags.contains(&"spotter_both"));
    }

    /// LED: gear shift 0..100, # segments 1..4, segment ranges 1..150, ten segment types.
    #[test]
    fn led_page_rows() {
        let page = led_page();
        assert!(!page.locked_in_standby);
        assert_eq!(slider_of(&page, "segment3"), (1.0, 150.0, 1.0));
        let kind = page.control("segment2dropdown").unwrap();
        assert_eq!(kind.field.as_ref().unwrap().path, ["leds", "segments", "1", "kind"]);
        match &kind.widget {
            Widget::Select { options } => assert_eq!(options.len(), 10),
            other => panic!("{other:?}"),
        }
    }

    /// Motion page header: the Yaw VR `center` button before the hardware setup button.
    #[test]
    fn motion_header_buttons() {
        let page = motion_page(registry(), 5.0);
        let header: Vec<(&str, &Action)> = page
            .header
            .iter()
            .map(|w| match w {
                Widget::Button { text, run } => (text.as_str(), run),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(header[0], ("Move platform to center", &Action::YawVrCenter));
        assert_eq!(header[1].0, "Shortcuts");
        let Action::Shortcuts { actions } = header[1].1 else { panic!("{:?}", header[1]) };
        assert_eq!(&actions[..3], ["motion_box_switch", "motion_compensation_switch", "motion_box_homing"]);
        assert_eq!(actions.last().unwrap(), "motion_box_smoothing_traction_loss_front_down");
        assert_eq!(header[2].0, "Hardware setup");
        assert_eq!(header.len(), 3);
    }
}
