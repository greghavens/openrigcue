//! Tuning → LED: the shift point, the number of segments, and each segment's position, type and colour
//! (segments above the number are not offered); the colour dialog; the header actions (shortcuts, hardware
//! set-up). The LED screen stays editable in stand-by.

use openrigcue_model::native_value::LegacyValue;
use serde_json::{json, Value};

use super::super::components::{self, RowState};
use super::super::html::{esc, t, tf};
use super::super::read;
use super::super::Screen;
use super::tuning::{self, Dialog};
use crate::app::MyApp;
use crate::slider::RangeScale;
use crate::spec::{self, Action, Control, Page, Widget};

/// The stored codes of the segment types, in the order of the page's choices.
const KIND_CODES: [&str; 10] = ["R", "S", "F", "RF", "SF", "SR", "SL", "SRF", "SLF", "O"];

/// The page description.
pub fn page() -> Page {
    spec::led_page()
}

/// The number of LEDs of the strip (`LED.nbr_leds`; 150 when it is not a number).
pub fn led_count(app: &MyApp) -> i64 {
    crate::backend::native_int(&LegacyValue::from(app.setting(&["leds", "nbr_leds"]).clone())).unwrap_or(150)
}

/// The scale of a segment's range: whole LEDs from the first to the strip's last.
pub fn segment_scale(app: &MyApp) -> RangeScale {
    RangeScale::new(1.0, led_count(app) as f64, 1.0)
}

/// The page as shown: the segments' ranges reach the strip's last LED.
fn page_for(app: &MyApp) -> Page {
    let mut page = page();
    let limits = segment_scale(app);
    for control in page.sections.iter_mut().flat_map(|s| s.controls.iter_mut()) {
        if let Widget::Range { scale, .. } = &mut control.widget {
            *scale = limits;
        }
    }
    page
}

/// The number of segments (1 to 4).
pub fn segment_count(app: &MyApp) -> usize {
    (components::number(app.setting(&["leds", "nbr_segments"])) as usize).clamp(1, 4)
}

/// The segment (1 to 4) a row belongs to.
fn segment_of(id: &str) -> Option<usize> {
    let rest = id.strip_prefix("segment")?;
    rest.chars().next()?.to_digit(10).map(|n| n as usize).filter(|n| (1..=4).contains(n))
}

/// A segment's colour as `#rrggbb` (black when it is not stored).
pub fn color_of(app: &MyApp, segment: usize) -> String {
    let index = segment.saturating_sub(1).to_string();
    let rgb = app.setting(&["leds", "segments", &index, "color"]);
    let channel = |i: usize| components::number(rgb.get(i).unwrap_or(&Value::Null)).clamp(0.0, 255.0) as u8;
    format!("#{:02x}{:02x}{:02x}", channel(0), channel(1), channel(2))
}

/// A colour swatch: a filled square in the colour.
fn swatch(color: &str) -> String {
    format!(
        "<svg class=\"icon\" viewBox=\"0 0 24 24\" aria-hidden=\"true\"><rect x=\"3\" y=\"3\" width=\"18\" height=\"18\" rx=\"3\" fill=\"{}\"/></svg>",
        esc(color)
    )
}

fn row_state(app: &MyApp, control: &Control) -> Option<RowState> {
    if segment_of(&control.id).is_some_and(|segment| segment > segment_count(app)) {
        return None;
    }
    let mut state = tuning::base_state(app, control, true);
    if let Some(segment) = segment_of(&control.id).filter(|_| control.id.ends_with("color")) {
        let color = color_of(app, segment);
        state.hint = Some(format!("{} {}", swatch(&color), tf(app, "Current color ^color^", &[("color", &color)])));
    }
    Some(state)
}

/// The body.
pub fn main(app: &MyApp) -> String {
    tuning::panel(
        app,
        Screen::Led,
        &page_for(app),
        false,
        "",
        |control| row_state(app, control),
        |_| Some((true, None)),
        &[],
        "",
    )
}

/// The values Compare looks at.
pub fn compared(app: &MyApp) -> Vec<(String, Vec<String>)> {
    tuning::compared(app, &page())
}

/// A value of the screen changed: the shift point in the tune, the segments in the LED settings.
pub fn set(app: &mut MyApp, control: &str, part: &str, value: &str) -> Result<(), String> {
    if !tuning::accepts(&page_for(app), control, part, |row| row_state(app, row)) {
        return Ok(());
    }
    match (control, part) {
        ("gear_shift_perc", "") => app.set_tune_native_checked(&["gear_shift_perc"], &read::number(value)?),
        ("nbr_segments", "") => {
            let count = read::int(value)?.clamp(1, 4);
            app.set_setting_native_checked("LED", "nbr_segments", &LegacyValue::Int(count))
        }
        (row, end) if segment_of(row).is_some() && row.len() == "segment1".len() => {
            let segment = segment_of(row).unwrap_or(1);
            let index = (segment - 1).to_string();
            let given = read::int(value)? as f64;
            let start = components::number(app.setting(&["leds", "segments", &index, "start"])).trunc();
            let stop = components::number(app.setting(&["leds", "segments", &index, "end"])).trunc();
            let (low, high) = if end == "high" { (start, given) } else { (given, stop) };
            store_segment(app, segment, segment_scale(app).clamp(low, high))
        }
        (row, "") if row.ends_with("dropdown") => {
            let segment = segment_of(row).ok_or_else(|| format!("no LED value {row:?}"))?;
            let index: usize = value.parse().map_err(|_| format!("ValueError: {value:?}"))?;
            let code = KIND_CODES.get(index).ok_or_else(|| format!("KeyError: {value:?}"))?;
            app.set_setting_native_checked("LED", &format!("segment{segment}_type"), &LegacyValue::Str((*code).into()))
        }
        _ => Err(format!("no LED value {control:?}")),
    }
}

/// A segment's range as its change stores it: the start, then the end, as whole numbers (both kept within
/// the strip by the caller).
fn store_segment(app: &mut MyApp, segment: usize, (low, high): (f64, f64)) -> Result<(), String> {
    app.set_setting_native_checked("LED", &format!("segment{segment}_start"), &LegacyValue::Int(low as i64))?;
    app.set_setting_native_checked("LED", &format!("segment{segment}_end"), &LegacyValue::Int(high as i64))
}

/// A button of the screen: the header actions and a segment's colour.
pub fn action(app: &mut MyApp, control: &str, index: Option<usize>) -> Result<(), String> {
    if control != "@header" && !tuning::accepts_button(&page_for(app), control, index, |row| row_state(app, row)) {
        return Ok(());
    }
    let run = tuning::button(&page(), control, index).ok_or_else(|| format!("no LED button {control:?}"))?;
    if tuning::run_common(app, &run)? {
        return Ok(());
    }
    match run {
        Action::LedColor { segment } => {
            let segment = usize::from(segment) + 1;
            let color = color_of(app, segment);
            tuning::open_dialog(app, Dialog::LedColor(ColorChoice { segment, color }));
            Ok(())
        }
        other => Err(format!("unsupported LED action {other:?}")),
    }
}

/// The colour dialog of a segment (1 to 4) and the colour chosen in it.
#[derive(Debug, Clone, PartialEq)]
pub struct ColorChoice {
    pub segment: usize,
    pub color: String,
}

/// The colour dialog.
pub fn color_dialog(app: &MyApp, choice: &ColorChoice) -> String {
    let title = tf(app, "Segment ^number^ color", &[("number", &choice.segment.to_string())]);
    let body = components::field(
        &t(app, "Color"),
        &format!(
            "<input id=\"led-color\" class=\"field-input\" type=\"color\" value=\"{}\" aria-label=\"{}\" data-act=\"dialog-field\" data-arg=\"color\" data-autofocus> <span class=\"mono\">{}</span>",
            esc(&choice.color),
            t(app, "Color"),
            esc(&choice.color)
        ),
        "",
    );
    super::super::dialogs::frame(app, &title, &body, Some((&t(app, "Apply"), true)))
}

/// A colour was picked in the dialog.
pub fn color_field(app: &mut MyApp, mut choice: ColorChoice, value: &str) -> Result<(), String> {
    parse_color(value)?;
    choice.color = value.to_lowercase();
    tuning::replace_dialog(app, Dialog::LedColor(choice));
    Ok(())
}

/// `#rrggbb` as red, green and blue.
pub fn parse_color(value: &str) -> Result<[i64; 3], String> {
    let hex = value.strip_prefix('#').filter(|h| h.len() == 6).ok_or_else(|| format!("ValueError: {value:?}"))?;
    let channel = |i: usize| i64::from_str_radix(&hex[i..i + 2], 16).map_err(|_| format!("ValueError: {value:?}"));
    Ok([channel(0)?, channel(2)?, channel(4)?])
}

/// The colour dialog was confirmed: the segment takes the colour.
pub fn color_confirmed(app: &mut MyApp, choice: &ColorChoice) -> Result<(), String> {
    let rgb = parse_color(&choice.color)?;
    app.set_setting_native_checked("LED", &format!("segment{}_color", choice.segment), &LegacyValue::from(json!(rgb)))?;
    super::super::refresh(app, &[super::super::Region::Main]);
    Ok(())
}

#[cfg(test)]
#[path = "led_tests.rs"]
mod tests;
