//! Tests of Tuning → LED: the shift point and segments, the segments offered by their number, the segment
//! types and the colour dialog.

use serde_json::json;

use super::*;
use crate::app::tests::{loaded, FakeBackend};
use crate::host::UiApp;
use crate::ui::{dispatch, navigate, Region};

fn led(backend: &std::sync::Arc<FakeBackend>) -> MyApp {
    let mut app = loaded(backend.clone());
    navigate(&mut app, Screen::Led, 0);
    app
}

#[test]
fn the_screen_stays_editable_in_stand_by_and_offers_the_segments_by_their_number() {
    let backend = FakeBackend::new();
    let mut app = led(&backend);
    let html = main(&app);
    assert!(!html.contains("Tuning is unavailable in Stand By mode") && html.contains(">General</option>"));
    dispatch(&mut app, "set", "LED:gear_shift_perc", "85").unwrap();
    assert_eq!(backend.docs.lock().unwrap()["tune"]["gear_shift_perc"], json!(85));
    dispatch(&mut app, "set", "LED:nbr_segments", "2").unwrap();
    assert_eq!(backend.docs.lock().unwrap()["settings"]["leds"]["nbr_segments"], json!(2));
    let html = main(&app);
    assert!(html.contains(">Segment 2</option>") && !html.contains(">Segment 3</option>"));
    drop(app.take_threads());
}

#[test]
fn segment_ranges_types_and_colours_are_stored_in_the_led_settings() {
    let backend = FakeBackend::new();
    let mut app = led(&backend);
    dispatch(&mut app, "set", "LED:segment1:high", "40").unwrap();
    dispatch(&mut app, "set", "LED:segment1", "5").unwrap();
    dispatch(&mut app, "set", "LED:segment1dropdown", "3").unwrap();
    {
        let docs = backend.docs.lock().unwrap();
        let segment = &docs["settings"]["leds"]["segments"][0];
        assert_eq!((segment["start"].clone(), segment["end"].clone()), (json!(5), json!(40)));
        assert_eq!(segment["kind"], json!("rpm_flag"));
    }
    dispatch(&mut app, "action", "LED:segment1color:", "").unwrap();
    let dialog = crate::ui::render(&app, Region::Dialog);
    assert!(dialog.contains("Segment 1 color") && dialog.contains("type=\"color\""));
    dispatch(&mut app, "dialog-field", "color", "#FF7952").unwrap();
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert_eq!(backend.docs.lock().unwrap()["settings"]["leds"]["segments"][0]["color"], json!([255, 121, 82]));
    app.ui.section = 1;
    assert!(main(&app).contains("#ff7952"), "the row shows the colour");
    assert!(parse_color("red").is_err());
    drop(app.take_threads());
}

#[test]
fn the_led_texts_have_entries() {
    let backend = FakeBackend::new();
    let mut app = led(&backend);
    backend.translations.lock().unwrap().clear();
    for section in 0..5 {
        app.ui.section = section;
        main(&app);
    }
    tuning::open_dialog(&mut app, Dialog::LedColor(ColorChoice { segment: 2, color: "#000000".into() }));
    crate::ui::render(&app, Region::Dialog);
    let looked_up = backend.translations.lock().unwrap().iter().cloned().collect();
    assert_eq!(crate::app::tests::texts_without_entry(&looked_up, &[]), Vec::<String>::new());
    drop(app.take_threads());
}

/// The opening tag of the element with `id` in `html`.
fn tag<'a>(html: &'a str, id: &str) -> &'a str {
    let start = html.find(&format!("id=\"{id}\"")).unwrap_or_else(|| panic!("no element {id}"));
    let start = html[..start].rfind('<').unwrap();
    let end = start + html[start..].find('>').unwrap();
    &html[start..=end]
}

#[test]
fn a_segment_edit_keeps_the_other_segments_as_the_service_has_them() {
    let backend = FakeBackend::new();
    let mut app = led(&backend);
    // Another client changed the second segment; the documents are not read again yet.
    backend.docs.lock().unwrap()["settings"]["leds"]["segments"][1]["start"] = json!(77);
    dispatch(&mut app, "set", "LED:segment1dropdown", "9").unwrap();
    let docs = backend.docs.lock().unwrap();
    assert_eq!(docs["settings"]["leds"]["segments"][0]["kind"], json!("off"));
    assert_eq!(docs["settings"]["leds"]["segments"][1]["start"], json!(77));
    drop(docs);
    drop(app.take_threads());
}

#[test]
fn failing_led_writes_are_reported_and_change_nothing() {
    let backend = FakeBackend::new();
    let mut app = led(&backend);
    let count = segment_count(&app);
    let color = backend.docs.lock().unwrap()["settings"]["leds"]["segments"][0]["color"].clone();
    backend.docs.lock().unwrap()["settings_failure"] = json!("RuntimeError: LED assignment failed");
    dispatch(&mut app, "action", "LED:segment1color:", "").unwrap();
    dispatch(&mut app, "dialog-field", "color", "#010203").unwrap();
    assert_eq!(dispatch(&mut app, "dialog-ok", "", ""), Err("RuntimeError: LED assignment failed".into()));
    assert_eq!(backend.docs.lock().unwrap()["settings"]["leds"]["segments"][0]["color"], color);
    assert_eq!(dispatch(&mut app, "set", "LED:nbr_segments", "1"), Err("RuntimeError: LED assignment failed".into()));
    assert_eq!(segment_count(&app), count);
    let html = main(&app);
    assert!(html.contains(&format!(">Segment {count}</option>")));
    drop(app.take_threads());
}

#[test]
fn stored_led_values_show() {
    let backend = FakeBackend::new();
    let mut app = led(&backend);
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["tune"]["gear_shift_perc"] = json!(37);
        docs["settings"]["leds"]["nbr_segments"] = json!("2");
        docs["settings"]["leds"]["segments"][0]["start"] = json!(2);
        docs["settings"]["leds"]["segments"][0]["end"] = json!(3);
        docs["settings"]["leds"]["segments"][0]["kind"] = json!("flag");
    }
    app.refresh_docs();
    let html = main(&app);
    assert!(tag(&html, "c-led-gear_shift_perc").contains("value=\"37\"") && html.contains(">37 %<"));
    assert!(tag(&html, "c-led-nbr_segments").contains("value=\"2\""));
    assert!(html.contains(">Segment 2</option>") && !html.contains(">Segment 3</option>"), "number text counts");
    app.ui.section = 1;
    let html = main(&app);
    assert!(tag(&html, "c-led-segment1").contains("value=\"2\"") && html.contains(">2 - 3<"));
    assert!(html.contains("<option value=\"2\" selected>Flag</option>"), "{html}");
    drop(app.take_threads());
}

#[test]
fn segment_ranges_reach_the_strips_last_led_only() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["settings"]["leds"]["nbr_leds"] = json!(60);
    let mut app = led(&backend);
    app.ui.section = 1;
    let html = main(&app);
    assert!(tag(&html, "c-led-segment1").contains("min=\"1\" max=\"60\""), "{html}");
    assert!(tag(&html, "c-led-segment1-high").contains("min=\"1\" max=\"60\""));
    let segment = |backend: &FakeBackend| {
        let docs = backend.docs.lock().unwrap();
        let segment = &docs["settings"]["leds"]["segments"][0];
        (segment["start"].clone(), segment["end"].clone())
    };
    dispatch(&mut app, "set", "LED:segment1", "10").unwrap();
    dispatch(&mut app, "set", "LED:segment1:high", "100").unwrap();
    assert_eq!(segment(&backend), (json!(10), json!(60)), "the end stops at the last LED");
    dispatch(&mut app, "set", "LED:segment1", "75").unwrap();
    assert_eq!(segment(&backend), (json!(60), json!(60)), "the start stops at the end");
    dispatch(&mut app, "set", "LED:segment1", "0").unwrap();
    assert_eq!(segment(&backend), (json!(1), json!(60)), "and at the first LED");
    // A segment above the number of segments is not offered and takes nothing.
    dispatch(&mut app, "set", "LED:nbr_segments", "1").unwrap();
    let before = backend.docs.lock().unwrap()["settings"]["leds"]["segments"][1].clone();
    dispatch(&mut app, "set", "LED:segment2", "5").unwrap();
    dispatch(&mut app, "set", "LED:segment2:high", "6").unwrap();
    assert_eq!(backend.docs.lock().unwrap()["settings"]["leds"]["segments"][1], before);
    drop(app.take_threads());
}
