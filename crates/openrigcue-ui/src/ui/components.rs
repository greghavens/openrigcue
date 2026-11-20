//! The shared controls of the interface: panels, rows, sliders, switches, steppers, selects, tabs, the
//! "More settings" and per-control "Options" disclosures, and the control row of a typed page description.
//!
//! Every interactive element carries `data-act` (the action name) and `data-arg` (its argument); the browser
//! sends the element's value with it (see `res/js/openrigcue-ui.js`). Ids are stable so that focus survives a
//! redraw.

use serde_json::Value;

use super::html::{esc, num, t};
use crate::app::MyApp;
use crate::backend::text;
use crate::spec::{Control, Widget};

/// How many rows a panel shows before "More settings".
pub const VISIBLE_ROWS: usize = 4;

/// A panel with a heading, optional head controls and a body.
pub fn panel(title: &str, head: &str, body: &str, class: &str) -> String {
    format!(
        "<section class=\"panel{}\"><div class=\"panel-head\"><h2>{title}</h2>{head}</div>{body}</section>",
        if class.is_empty() { String::new() } else { format!(" {class}") }
    )
}

/// A labelled row: the label, an optional hint and the body.
pub fn field(label: &str, body: &str, hint: &str) -> String {
    format!(
        "<div class=\"control-row\"><div class=\"control-label\"><strong>{label}</strong></div>{}<div class=\"control-body\">{body}</div></div>",
        if hint.is_empty() { String::new() } else { format!("<small>{hint}</small>") }
    )
}

/// The rows, the first [`VISIBLE_ROWS`] shown and the rest under "More settings · N".
pub fn more(app: &MyApp, rows: &[String], key: &str) -> String {
    if rows.len() <= VISIBLE_ROWS {
        return rows.concat();
    }
    let open = app.ui.open_more.contains(key);
    format!(
        "{}<details class=\"more\" data-act=\"more\" data-arg=\"{}\"{}><summary class=\"device\">{} · {}</summary>{}</details>",
        rows[..VISIBLE_ROWS].concat(),
        esc(key),
        if open { " open" } else { "" },
        t(app, "More settings"),
        rows.len() - VISIBLE_ROWS,
        rows[VISIBLE_ROWS..].concat()
    )
}

/// A control's extras under its "Options" disclosure.
pub fn options(app: &MyApp, extras: &str, key: &str) -> String {
    if extras.is_empty() {
        return String::new();
    }
    let key = format!("options:{key}");
    format!(
        "<details class=\"options\" data-act=\"more\" data-arg=\"{}\"{}><summary>{}</summary><div class=\"options-body\">{extras}</div></details>",
        esc(&key),
        if app.ui.open_more.contains(&key) { " open" } else { "" },
        t(app, "Options")
    )
}

/// A page's header buttons under its "Actions" disclosure.
pub fn actions(app: &MyApp, buttons: &str, key: &str) -> String {
    if buttons.is_empty() {
        return String::new();
    }
    let key = format!("actions:{key}");
    format!(
        "<details class=\"more\" data-act=\"more\" data-arg=\"{}\"{}><summary class=\"device\">{}</summary><div class=\"device action-row\">{buttons}</div></details>",
        esc(&key),
        if app.ui.open_more.contains(&key) { " open" } else { "" },
        t(app, "Actions")
    )
}

/// A button sending `act` with `arg`.
pub fn button(label: &str, act: &str, arg: &str, class: &str) -> String {
    format!(
        "<button type=\"button\"{} data-act=\"{act}\" data-arg=\"{}\">{label}</button>",
        if class.is_empty() { String::new() } else { format!(" class=\"{class}\"") },
        esc(arg)
    )
}

/// A select; `options` are value and shown text. Its change sends `act` with `arg` and the value.
pub fn select(id: &str, label: &str, options: &[(String, String)], selected: &str, act: &str, arg: &str) -> String {
    let options: String = options
        .iter()
        .map(|(value, shown)| {
            format!(
                "<option value=\"{}\"{}>{}</option>",
                esc(value),
                if value == selected { " selected" } else { "" },
                esc(shown)
            )
        })
        .collect();
    format!(
        "<select id=\"{id}\" class=\"field-input\" aria-label=\"{}\" data-act=\"{act}\" data-arg=\"{}\">{options}</select>",
        esc(label),
        esc(arg)
    )
}

/// The tabs of a tab list; the selected one is active.
pub fn tabs(app: &MyApp, label: &str, names: &[&str], selected: usize, act: &str) -> String {
    let buttons: String = names
        .iter()
        .enumerate()
        .map(|(index, name)| {
            format!(
                "<button type=\"button\" id=\"tab-{index}\" role=\"tab\" aria-controls=\"controls\" aria-selected=\"{}\" tabindex=\"{}\" class=\"{}\" data-act=\"{act}\" data-arg=\"{index}\">{}</button>",
                index == selected,
                if index == selected { 0 } else { -1 },
                if index == selected { "active" } else { "" },
                t(app, name)
            )
        })
        .collect();
    format!("<div class=\"tabs\" role=\"tablist\" aria-label=\"{}\" data-tabs>{buttons}</div>", t(app, label))
}

/// The current state of one control of a page description.
#[derive(Debug, Clone)]
pub struct RowState {
    /// The main value.
    pub value: Value,
    /// The upper value of a range.
    pub high: Value,
    /// The per-control on/off switch.
    pub toggle: Option<bool>,
    /// The Max Telemetry value.
    pub max: Option<Value>,
    /// The main value can be changed (false also while the per-control switch is off).
    pub enabled: bool,
    /// The whole row can be used: false (the stand-by lock, a feature the running game lacks) disables every
    /// part of it, its switch, Max Telemetry and buttons included.
    pub row_enabled: bool,
    /// Shown instead of the label from the description.
    pub label: Option<String>,
    /// Shown instead of the value text from the description.
    pub output: Option<String>,
    /// The texts at both ends of a slider (defaults: its minimum and maximum).
    pub endpoints: Option<(String, String)>,
    /// Shown instead of the hint from the description.
    pub hint: Option<String>,
    /// The buttons of the description that apply (by index); `None` keeps all.
    pub buttons: Option<Vec<usize>>,
}

impl Default for RowState {
    fn default() -> Self {
        RowState {
            value: Value::Null,
            high: Value::Null,
            toggle: None,
            max: None,
            enabled: true,
            row_enabled: true,
            label: None,
            output: None,
            endpoints: None,
            hint: None,
            buttons: None,
        }
    }
}

impl RowState {
    /// Whether a part of the row takes changes, as the row draws it: `toggle` (the switch), `max` (Max
    /// Telemetry), `button` (a row button), or the main value (any other part).
    pub fn part_enabled(&self, part: &str) -> bool {
        match part {
            "toggle" | "button" => self.row_enabled,
            "max" => self.row_enabled && self.enabled && self.toggle != Some(false),
            _ => self.row_enabled && self.enabled,
        }
    }
}

/// The value as a number (0 when it is none).
pub fn number(value: &Value) -> f64 {
    match value {
        Value::Bool(b) => f64::from(u8::from(*b)),
        Value::String(s) => s.trim().parse().unwrap_or(0.0),
        other => crate::backend::num(other),
    }
}

/// The value text of a slider or range: the description's template with the value(s) filled in.
pub fn output_text(control: &Control, state: &RowState, unit: &str) -> String {
    if let Some(output) = &state.output {
        return output.clone();
    }
    let template = control.shows.clone().unwrap_or_else(|| "{}".into());
    template
        .replace("{lo}", &num(number(&state.value)))
        .replace("{hi}", &num(number(&state.high)))
        .replace("{unit}", unit)
        .replace("{}", &num(number(&state.value)))
}

/// The id of a control's element.
pub fn control_id(screen: &str, id: &str, part: &str) -> String {
    let screen: String = screen.chars().filter(char::is_ascii_alphanumeric).collect::<String>().to_lowercase();
    let id: String = id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '-' }).collect();
    if part.is_empty() {
        format!("c-{screen}-{id}")
    } else {
        format!("c-{screen}-{id}-{part}")
    }
}

/// One control row of a page description with its state. Its actions are `set` (argument
/// `<screen>:<control>[:toggle|max|high]`, the new value) and `action` (argument `<screen>:<control>:<button>`).
pub fn spec_row(app: &MyApp, screen: &str, control: &Control, state: &RowState, unit: &str) -> String {
    let id = control_id(screen, &control.id, "");
    let arg = format!("{screen}:{}", control.id);
    let label = state.label.clone().unwrap_or_else(|| t(app, &control.label));
    let disabled = if state.part_enabled("") { "" } else { " disabled" };
    let part_disabled = |part: &str| if state.part_enabled(part) { "" } else { " disabled" };
    let hint = state.hint.clone().or_else(|| control.hint.as_ref().map(|h| t(app, h))).unwrap_or_default();
    let hint_html = if hint.is_empty() { String::new() } else { format!("<small id=\"{id}-hint\">{hint}</small>") };
    let described = if hint.is_empty() { String::new() } else { format!(" aria-describedby=\"{id}-hint\"") };
    let mut output = String::new();
    let widget = match &control.widget {
        Widget::Slider { scale } => {
            let shown = output_text(control, state, unit);
            output = format!("<output id=\"{id}-out\" for=\"{id}\">{}</output>", esc(&shown));
            let template = control.shows.clone().unwrap_or_else(|| "{}".into()).replace("{unit}", unit);
            let (low, high) = state.endpoints.clone().unwrap_or_else(|| (esc(&num(scale.min)), esc(&num(scale.max))));
            format!(
                "<input id=\"{id}\" type=\"range\" min=\"{}\" max=\"{}\" step=\"{}\" value=\"{}\" data-act=\"set\" data-arg=\"{}\" data-output=\"{id}-out\" data-template=\"{}\"{described}{disabled}><div class=\"range-endpoints\"><span>{low}</span><span>{high}</span></div>",
                num(scale.min),
                num(scale.max),
                num(scale.step),
                num(number(&state.value)),
                esc(&arg),
                esc(&template),
            )
        }
        Widget::Range { scale, .. } => {
            let shown = output_text(control, state, unit);
            output = format!("<output id=\"{id}-out\" for=\"{id}\">{}</output>", esc(&shown));
            let template = control.shows.clone().unwrap_or_else(|| "{lo} - {hi}".into());
            let range = |part: &str, value: f64, label: &str| {
                format!(
                    "<input id=\"{id}{part}\" type=\"range\" min=\"{}\" max=\"{}\" step=\"{}\" value=\"{}\" data-act=\"set\" data-arg=\"{}{}\" data-output=\"{id}-out\" data-template=\"{}\" data-pair=\"{id}\" data-bound=\"{}\" aria-label=\"{}\"{disabled}>",
                    num(scale.limit_min),
                    num(scale.limit_max),
                    num(scale.step),
                    num(value),
                    esc(&arg),
                    if part.is_empty() { "" } else { ":high" },
                    esc(&template),
                    if part.is_empty() { "low" } else { "high" },
                    esc(label),
                )
            };
            format!(
                "<div class=\"range-pair\">{}{}</div>",
                range("", number(&state.value), &format!("{label} · {}", super::html::tr(app, "Minimum"))),
                range("-high", number(&state.high), &format!("{label} · {}", super::html::tr(app, "Maximum")))
            )
        }
        Widget::Switch { off, on } => {
            let checked = super::read::truth(&state.value);
            format!(
                "<label class=\"switch\"><input id=\"{id}\" type=\"checkbox\" data-act=\"set\" data-arg=\"{}\"{}{described}{disabled}><span>{}</span></label>",
                esc(&arg),
                if checked { " checked" } else { "" },
                t(app, if checked { on } else { off })
            )
        }
        Widget::Select { options } => {
            let current = options.iter().position(|choice| super::read::same(&choice.value, &state.value));
            let mut list: String = String::new();
            if current.is_none() {
                list.push_str("<option value=\"\" selected disabled>—</option>");
            }
            for (index, choice) in options.iter().enumerate() {
                list.push_str(&format!(
                    "<option value=\"{index}\"{}>{}</option>",
                    if Some(index) == current { " selected" } else { "" },
                    t(app, &choice.label)
                ));
            }
            format!(
                "<select id=\"{id}\" class=\"field-input\" aria-label=\"{}\" data-act=\"set\" data-arg=\"{}\"{described}{disabled}>{list}</select>",
                esc(&label),
                esc(&arg)
            )
        }
        Widget::Number { min, max, step } => {
            let value = number(&state.value);
            let shown = state.output.clone().map(|o| format!("<output id=\"{id}-out\">{}</output>", esc(&o)));
            if let Some(shown) = shown {
                output = shown;
            }
            format!(
                "<input id=\"{id}\" class=\"field-input stepper\" type=\"number\" min=\"{}\" max=\"{}\" step=\"{}\" value=\"{}\" aria-label=\"{}\" data-act=\"set\" data-arg=\"{}\"{described}{disabled}>",
                num(*min),
                num(*max),
                num(*step),
                num(value),
                esc(&label),
                esc(&arg)
            )
        }
        Widget::Button { text: shown, .. } => {
            format!(
                "<button type=\"button\" id=\"{id}\" data-act=\"action\" data-arg=\"{}\"{disabled}>{}</button>",
                esc(&format!("{arg}:")),
                t(app, shown)
            )
        }
        Widget::Readout => format!(
            "<span class=\"mono\" id=\"{id}\">{}</span>",
            esc(&state.output.clone().unwrap_or_else(|| text(&state.value)))
        ),
        Widget::Text => String::new(),
    };
    if matches!(control.widget, Widget::Text) {
        return format!(
            "<div class=\"control-row note-row\" data-control=\"{}\"><small>{label}</small></div>",
            esc(&control.id)
        );
    }
    let mut extras = String::new();
    if let Some(on) = state.toggle {
        extras.push_str(&format!(
            "<label class=\"switch\"><input id=\"{id}-toggle\" type=\"checkbox\" data-act=\"set\" data-arg=\"{}:toggle\"{}{}><span>{}</span></label>",
            esc(&arg),
            if on { " checked" } else { "" },
            part_disabled("toggle"),
            t(app, "Enabled")
        ));
    }
    if let (Some(max), Some(spec)) = (&state.max, &control.max_telemetry) {
        extras.push_str(&format!(
            "<label class=\"inline-field\">{} <input id=\"{id}-max\" class=\"field-input stepper\" type=\"number\" min=\"{}\" max=\"{}\" step=\"{}\" value=\"{}\" data-act=\"set\" data-arg=\"{}:max\"{}></label>",
            t(app, "Max Telemetry"),
            num(spec.min),
            num(spec.max),
            num(spec.step),
            num(number(max)),
            esc(&arg),
            part_disabled("max")
        ));
    }
    for (index, button) in control.buttons.iter().enumerate() {
        if state.buttons.as_ref().is_some_and(|kept| !kept.contains(&index)) {
            continue;
        }
        if let Widget::Button { text: shown, .. } = button {
            extras.push_str(&format!(
                "<button type=\"button\" data-act=\"action\" data-arg=\"{}:{index}\"{}>{}</button>",
                esc(&arg),
                part_disabled("button"),
                t(app, shown)
            ));
        }
    }
    format!(
        "<div class=\"control-row{}\" data-control=\"{}\"><div class=\"control-label\"><label for=\"{id}\"><strong>{label}</strong></label>{output}</div>{hint_html}{widget}{}</div>",
        if state.part_enabled("") { "" } else { " is-disabled" },
        esc(&control.id),
        options(app, &extras, &id)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slider::SliderScale;
    use crate::spec::{Field, Write};

    #[test]
    fn output_templates_fill_values_and_units() {
        let control = Control::slider("x", "X", SliderScale::new(0.0, 100.0, 1.0), Field::tune(&["x"]), Write::Int)
            .showing("{}{unit}");
        let state = RowState { value: serde_json::json!(35), ..RowState::default() };
        assert_eq!(output_text(&control, &state, "Km/h"), "35Km/h");
    }

    #[test]
    fn control_ids_are_stable_and_safe() {
        assert_eq!(control_id("Motion Compensation", "pitch_type", ""), "c-motioncompensation-pitch_type");
        assert_eq!(control_id("Motion", "yaw", "max"), "c-motion-yaw-max");
    }
}
