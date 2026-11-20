//! Typed descriptions of every page of the UI: which widget edits which stored value, with which range, step and
//! conversion. The browser renders these descriptions; the values
//! are pinned by the tests of each page builder.

mod hardware;
mod setup;
mod tuning;

use serde::Serialize;

use crate::slider::{RangeScale, SliderScale};

pub use hardware::hardware_page;
pub use setup::{app_page, compensation_page, debug_page, joystick_page, routines_page, telemetry_page, ROUTINES};
pub use tuning::{belt_page, haptic_page, iflag_page, led_page, motion_page, shake_page, sim3d_page, wind_page};

/// Which document a value lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Store {
    /// The active tune (`/api/v1/profile`).
    Tune,
    /// The global settings (`/api/v1/settings`).
    Settings,
    /// Run-time switches and test controls (`/api/v1/controls`).
    Controls,
}

/// A stored value: document plus key path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Field {
    pub store: Store,
    pub path: Vec<String>,
}

impl Field {
    pub fn tune(path: &[&str]) -> Field {
        Field { store: Store::Tune, path: path.iter().map(|p| (*p).to_owned()).collect() }
    }

    pub fn settings(path: &[&str]) -> Field {
        Field { store: Store::Settings, path: path.iter().map(|p| (*p).to_owned()).collect() }
    }

    pub fn controls(path: &[&str]) -> Field {
        Field { store: Store::Controls, path: path.iter().map(|p| (*p).to_owned()).collect() }
    }
}

/// How a widget value becomes the stored value (what the handler assigns).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Write {
    /// The slider's float as is.
    Number,
    /// The value truncated to an integer.
    Int,
    /// A switch stored as a boolean.
    Bool,
    /// A switch stored as 0/1 (an integer field).
    BoolInt,
    /// The Wind page's default max speed: the truncated value in km/h, the truncated value times 1.60934 in mph.
    SpeedFromUnit,
}

/// How a stored value becomes the widget value before it is set on a slider, range slider or spin box.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Read {
    Number,
    /// The value parsed as a float and truncated.
    IntOfFloat,
    /// The default max speed times 0.621371, truncated, in mph; the value itself in km/h.
    SpeedToUnit,
}

/// One entry of a dropdown: the text shown and the value stored.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Choice {
    pub label: String,
    pub value: serde_json::Value,
}

impl Choice {
    pub fn new(label: &str, value: impl Into<serde_json::Value>) -> Choice {
        Choice { label: label.to_owned(), value: value.into() }
    }
}

/// Something a button does; the browser maps each to its API call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    /// The Shake page's test button: the effect's test when a sound card is selected.
    TestShakerEffect { effect: String },
    /// The Shake page's config button: the per-effect filter dialog.
    ShakerFilter { effect: String },
    /// The Hardware page's channel test button: the channel's test sound when a sound card is selected.
    TestSpeaker { speaker: String },
    /// The Hardware page's device test button: wind, LED or tachometer "Always On" for seven seconds.
    HardwareTest { target: String },
    /// The Hardware page's motion test button: starts or stops the motion test.
    MotionTestToggle,
    /// The Hardware page's belt test button: starts or stops the belt test.
    BeltTestToggle,
    /// The Hardware page's iFlag test button.
    IflagTest,
    /// The Hardware page's rumble test button of one port.
    RumbleTest { port: u8 },
    /// The Hardware page's haptic test button of one wheel.
    HapticTest { wheel: u8 },
    /// The Hardware page's VNM button: the dialog of the actuators' reversed directions.
    VnmReverse,
    /// The Hardware page's DOF Reality button: the dialog of the controller's Kp setting.
    DofRealityKp,
    /// The Belt page's calibration button: asks for confirmation, then calibrates.
    BeltCalibrate,
    /// The Motion page's tuning wizard button.
    Intellitune,
    /// The LED page's colour button of one segment: the colour picker.
    LedColor { segment: u8 },
    /// The Motion Compensation page's calibrate button of one axis: the axis offset becomes the negated raw
    /// sensor value of the axis.
    CompensationCalibrate { axis: String },
    /// The Telemetry page's export button: a snapshot of the telemetry.
    TelemetrySnapshot,
    /// The Telemetry page's reset button: the recorded minimum and maximum values start again.
    TelemetryReset,
    /// A page's header buttons: open the page `left` of the top menu entry `top`.
    Goto { top: String, left: String },
    /// The Yaw VR centre button (Motion page header, Hardware page): sets `motion_homing` to 1.
    YawVrCenter,
    /// The Hardware page's limits button: the Yaw VR dialog.
    YawVrLimits,
    /// The Hardware page's axis assignment button: the axis assignment dialog.
    AxisAssignment,
    /// The Hardware page's controller settings button: the PT-ACTUATOR / Thanos controller dialog.
    ThanosSettings,
    /// The Hardware page's Duplicate button: copies the selected speaker layout under a new name.
    DuplicateLayout,
    /// The Hardware page's Remove button: removes the selected speaker layout.
    RemoveLayout,
    /// The Hardware page's config button of a speaker (by its upper-case key): the custom speaker matrix dialog.
    SpeakerMatrix { speaker: String },
    /// The App page's reset button: the factory reset.
    FactoryReset,
    /// A link opened in a new window (Tuning Tips).
    OpenUrl { url: String },
    /// A page's shortcut button: the shortcut editor with these actions.
    Shortcuts { actions: Vec<String> },
    /// The Routines page's config button: the routine's options dialog.
    RoutineOptions { routine: String },
    /// The Debug page's reload: the captured output into the text box.
    DebugReload,
}

/// The interactive part of a control.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "widget", rename_all = "snake_case")]
pub enum Widget {
    /// A slider.
    Slider { scale: SliderScale },
    /// A range slider; `high` is the field of the upper pointer.
    Range { scale: RangeScale, high: Field },
    /// A switch with its off and on texts.
    Switch { off: String, on: String },
    /// A drop-down.
    Select { options: Vec<Choice> },
    /// A spin box (default, minimum, maximum, step).
    Number { min: f64, max: f64, step: f64 },
    /// A button without a stored value.
    Button { text: String, run: Action },
    /// A label the page fills from live state.
    Readout,
    /// Explanatory text.
    Text,
}

/// The per-effect on/off switch beside an effect slider; when off, the slider (and the max-telemetry box) is
/// disabled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Toggle {
    pub field: Field,
}

/// The "max telemetry" spin box of a motion effect (default 0, −1800…1800, step 1).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MaxTelemetry {
    pub min: f64,
    pub max: f64,
    pub step: f64,
    /// The `max_telemetry_vars` key; pitch and roll switch to `<axis>_rate` when the axis follows the rate.
    pub key: String,
}

/// One row of a page.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Control {
    /// The content key of the row, which the visibility rules address.
    pub id: String,
    pub label: String,
    #[serde(flatten)]
    pub widget: Widget,
    /// The value the widget edits.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<Field>,
    /// Further fields the same handler writes.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub also: Vec<Field>,
    pub write: Write,
    pub read: Read,
    /// The value label: `{}` is replaced by the truncated value (`{lo}`/`{hi}` for ranges).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shows: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub toggle: Option<Toggle>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_telemetry: Option<MaxTelemetry>,
    /// Buttons at the end of the row (Test, filter dialog, …).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub buttons: Vec<Widget>,
    /// Tooltip (the `title` attribute).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// The handler ignores the first value the main loop sets (the first-time
    /// flags), so loading never writes through this row.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub quiet_load: bool,
}

impl Control {
    fn new(id: &str, label: &str, widget: Widget, field: Option<Field>, write: Write) -> Control {
        Control {
            id: id.to_owned(),
            label: label.to_owned(),
            widget,
            field,
            also: Vec::new(),
            write,
            read: Read::Number,
            shows: None,
            toggle: None,
            max_telemetry: None,
            buttons: Vec::new(),
            hint: None,
            quiet_load: false,
        }
    }

    pub fn slider(id: &str, label: &str, scale: SliderScale, field: Field, write: Write) -> Control {
        Control::new(id, label, Widget::Slider { scale }, Some(field), write).showing("{}")
    }

    pub fn range(id: &str, label: &str, scale: RangeScale, low: Field, high: Field, write: Write) -> Control {
        Control::new(id, label, Widget::Range { scale, high }, Some(low), write)
    }

    pub fn switch(id: &str, label: &str, field: Field, write: Write) -> Control {
        Control::new(id, label, Widget::Switch { off: "Off".into(), on: "On".into() }, Some(field), write)
    }

    pub fn select(id: &str, label: &str, options: Vec<Choice>, field: Field) -> Control {
        Control::new(id, label, Widget::Select { options }, Some(field), Write::Number)
    }

    /// A dropdown that stores nothing itself (its handler drives other controls).
    pub fn picker(id: &str, label: &str, options: Vec<Choice>) -> Control {
        Control::new(id, label, Widget::Select { options }, None, Write::Number)
    }

    pub fn number(id: &str, label: &str, (min, max, step): (f64, f64, f64), field: Field) -> Control {
        Control::new(id, label, Widget::Number { min, max, step }, Some(field), Write::Int)
    }

    pub fn button(id: &str, label: &str, text: &str, run: Action) -> Control {
        Control::new(id, label, Widget::Button { text: text.to_owned(), run }, None, Write::Number)
    }

    pub fn readout(id: &str, label: &str) -> Control {
        Control::new(id, label, Widget::Readout, None, Write::Number)
    }

    pub fn text(id: &str, label: &str) -> Control {
        Control::new(id, label, Widget::Text, None, Write::Number)
    }

    pub fn showing(mut self, template: &str) -> Control {
        self.shows = Some(template.to_owned());
        self
    }

    pub fn reading(mut self, read: Read) -> Control {
        self.read = read;
        self
    }

    pub fn also_writing(mut self, field: Field) -> Control {
        self.also.push(field);
        self
    }

    pub fn toggled_by(mut self, field: Field) -> Control {
        self.toggle = Some(Toggle { field });
        self
    }

    pub fn with_button(mut self, text: &str, run: Action) -> Control {
        self.buttons.push(Widget::Button { text: text.to_owned(), run });
        self
    }

    pub fn quietly(mut self) -> Control {
        self.quiet_load = true;
        self
    }

    pub fn hinted(mut self, hint: &str) -> Control {
        self.hint = Some(hint.to_owned());
        self
    }

    pub fn switch_labels(mut self, off: &str, on: &str) -> Control {
        self.widget = Widget::Switch { off: off.to_owned(), on: on.to_owned() };
        self
    }

    pub fn with_max_telemetry(mut self, key: &str) -> Control {
        self.max_telemetry = Some(MaxTelemetry { min: -1800.0, max: 1800.0, step: 1.0, key: key.to_owned() });
        self
    }
}

/// A titled group of rows.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Section {
    /// The separator's content key when a rule addresses it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub title: String,
    pub controls: Vec<Control>,
}

impl Section {
    pub fn new(title: &str, controls: Vec<Control>) -> Section {
        Section { id: None, title: title.to_owned(), controls }
    }

    pub fn keyed(id: &str, title: &str, controls: Vec<Control>) -> Section {
        Section { id: Some(id.to_owned()), title: title.to_owned(), controls }
    }
}

/// One page of the left menu.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Page {
    /// Left-menu item text.
    pub key: String,
    /// Page header.
    pub title: String,
    /// The page is not enabled in test mode: the content is disabled while no game runs.
    pub locked_in_standby: bool,
    /// Header buttons.
    pub header: Vec<Widget>,
    /// Tooltips of header buttons (their `title` attribute), by button text.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub header_hints: std::collections::BTreeMap<String, String>,
    pub sections: Vec<Section>,
}

impl Page {
    pub fn new(key: &str, title: &str, sections: Vec<Section>) -> Page {
        Page {
            key: key.to_owned(),
            title: title.to_owned(),
            locked_in_standby: false,
            header: Vec::new(),
            header_hints: std::collections::BTreeMap::new(),
            sections,
        }
    }

    pub fn locked(mut self) -> Page {
        self.locked_in_standby = true;
        self
    }

    pub fn with_header(mut self, text: &str, run: Action) -> Page {
        self.header.push(Widget::Button { text: text.to_owned(), run });
        self
    }

    /// A header button with a tooltip.
    pub fn with_hinted_header(mut self, text: &str, run: Action, hint: &str) -> Page {
        if !hint.is_empty() {
            self.header_hints.insert(text.to_owned(), hint.to_owned());
        }
        self.with_header(text, run)
    }

    /// The page's keyboard button (opens the shortcut editor with `actions`).
    pub fn with_shortcuts(self, actions: Vec<String>, hint: &str) -> Page {
        self.with_hinted_header("Shortcuts", Action::Shortcuts { actions }, hint)
    }

    /// Every control of the page, in order.
    pub fn controls(&self) -> impl Iterator<Item = &Control> {
        self.sections.iter().flat_map(|s| s.controls.iter())
    }

    /// The control with the content key `id`.
    pub fn control(&self, id: &str) -> Option<&Control> {
        self.controls().find(|c| c.id == id)
    }
}

/// The header button every tuning page has: the config button, which opens Setup / Hardware.
fn goto_hardware() -> Action {
    Action::Goto { top: "Setup".into(), left: "Hardware".into() }
}

/// The tooltip of the tuning pages' config button.
const HARDWARE_HINT: &str = "Open Setup -> Hardware to set this up";

/// A shortcut list as owned strings.
fn actions(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

/// A single-action shortcut button on a row: the shortcut editor with that one action.
fn shortcut(action: &str) -> Action {
    Action::Shortcuts { actions: vec![action.to_owned()] }
}

/// Slider scale shorthand.
const fn scale(min: f64, max: f64, step: f64) -> SliderScale {
    SliderScale::new(min, max, step)
}
