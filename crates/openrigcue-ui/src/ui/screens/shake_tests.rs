//! Tests of Tuning → Shake: its groups and writes, the game feature rule, the Test and Filter buttons, the
//! filter dialog, the hot keys and the default engine type.

use serde_json::json;

use super::*;
use crate::app::tests::{loaded, FakeBackend};
use crate::backend::{Backend, Failure, Method};
use crate::host::UiApp;
use crate::ui::{dispatch, navigate, Region};

use super::super::tuning::game_without;

fn shake(backend: &std::sync::Arc<FakeBackend>, game: &str) -> MyApp {
    backend.docs.lock().unwrap()["status"]["session"]["game"] = json!(game);
    let mut app = loaded(backend.clone());
    navigate(&mut app, Screen::Shake, 0);
    app
}

fn stored(backend: &FakeBackend, path: &[&str]) -> serde_json::Value {
    let docs = backend.docs.lock().unwrap();
    path.iter().fold(docs["tune"].clone(), |value, key| value[*key].clone())
}

#[test]
fn groups_rows_and_writes_follow_the_page() {
    let backend = FakeBackend::new();
    let mut app = shake(&backend, "acc");
    let html = main(&app);
    for group in ["Common", "Racing", "Flight"] {
        assert!(html.contains(&format!(">{group}</option>")), "{group}");
    }
    assert!(html.contains("More settings · ") && html.contains(">Downloaded profiles<"));
    assert!(html.contains("data-arg=\"Shake:engine:toggle\"") && html.contains(">Filter<"));
    assert!(!html.contains(">Test<"), "the Test buttons need enable_test_effect");
    dispatch(&mut app, "set", "Shake:engine_type", "1").unwrap();
    assert_eq!(stored(&backend, &["shaker_engine_type"]), json!("E"));
    dispatch(&mut app, "set", "Shake:engine", "13").unwrap();
    assert_eq!(stored(&backend, &["shaker_effects", "engine"]), json!(13));
    dispatch(&mut app, "set", "Shake:engine:toggle", "0").unwrap();
    assert_eq!(stored(&backend, &["shaker_switch", "engine"]), json!(false));
    assert!(main(&app).contains("id=\"c-shake-engine\" type=\"range\"") && main(&app).contains(" disabled"));
    drop(app.take_threads());
}

#[test]
fn effects_the_game_does_not_use_are_disabled_or_hidden() {
    let backend = FakeBackend::new();
    let game = game_without("heave");
    let mut app = shake(&backend, &game);
    assert!(main(&app).contains("Not used by this game"), "road bump follows the heave column");
    backend.docs.lock().unwrap()["settings"]["features"]["hide_effects_not_in_game"] = json!(true);
    app.refresh_docs();
    assert!(!main(&app).contains("data-arg=\"Shake:road_bump\""));
    drop(app.take_threads());
}

#[test]
fn test_plays_on_the_sound_device_and_asks_for_one_without() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["settings"]["shakers"]["test_effect_enabled"] = json!(true);
    let mut app = shake(&backend, "acc");
    assert!(main(&app).contains(">Test<"));
    let page = page();
    let index = page
        .control("engine")
        .unwrap()
        .buttons
        .iter()
        .position(|b| matches!(b, Widget::Button { run: Action::TestShakerEffect { .. }, .. }));
    let arg = format!("Shake:engine:{}", index.unwrap());
    dispatch(&mut app, "action", &arg, "").unwrap();
    assert!(app.ui.notices.history.last().unwrap().message.contains("sound device"));
    backend.docs.lock().unwrap()["settings"]["shakers"]["sound_card"] = json!("USB Sound Device");
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "action", &arg, "").unwrap();
    assert!(backend
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|(m, p, _)| *m == Method::Post && p.starts_with("shakers/test/engine")));
    drop(app.take_threads());
}

#[test]
fn the_filter_dialog_writes_live_and_cancel_restores() {
    let backend = FakeBackend::new();
    let mut app = shake(&backend, "acc");
    let before = stored(&backend, &["shaker_cutoff", "road_bump"]);
    dispatch(&mut app, "action", "Shake:road_bump:0", "").unwrap();
    let dialog = crate::ui::render(&app, Region::Dialog);
    assert!(dialog.contains(">Filter · ") && dialog.contains(">Strength<") && !dialog.contains(">Fade Off<"));
    dispatch(&mut app, "dialog-field", "enabled", "1").unwrap();
    dispatch(&mut app, "dialog-field", "cutoff", "120").unwrap();
    assert_eq!(stored(&backend, &["shaker_cutoff", "road_bump"]), json!(120));
    assert_eq!(stored(&backend, &["shaker_dsp_type", "road_bump"]), json!(1));
    dispatch(&mut app, "dialog-close", "", "").unwrap();
    assert_eq!(stored(&backend, &["shaker_cutoff", "road_bump"]), before);
    // The engine's dialog: Speed and the fade-off range; Confirm keeps the values.
    dispatch(&mut app, "action", "Shake:engine:0", "").unwrap();
    let dialog = crate::ui::render(&app, Region::Dialog);
    assert!(dialog.contains(">Speed<") && dialog.contains(">Fade Off<") && dialog.contains(">Cutoff<"));
    dispatch(&mut app, "dialog-field", "fade_start", "20").unwrap();
    dispatch(&mut app, "dialog-field", "fade_end", "10").unwrap();
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert_eq!(stored(&backend, &["shaker_cutoff", "engine_fadeoff_start"]), json!(20));
    assert_eq!(stored(&backend, &["shaker_cutoff", "engine_fadeoff_end"]), json!(20), "the end stays above the start");
    assert!(app.ui.dialog.is_none());
    drop(app.take_threads());
}

#[test]
fn hot_keys_step_the_effects_within_their_range() {
    let backend = FakeBackend::new();
    let mut app = shake(&backend, "acc");
    dispatch(&mut app, "set", "Shake:engine", "20").unwrap();
    dispatch(&mut app, "set", "Shake:gear", "5").unwrap();
    backend.docs.lock().unwrap()["ui_actions"] = json!(["shake_box_engine_up", "shake_box_gear_down"]);
    let pending = |action: &str| backend.ui_action_pending(action);
    assert!(apply_hotkeys(&mut app, &pending).unwrap());
    assert_eq!(stored(&backend, &["shaker_effects", "engine"]), json!(20));
    assert_eq!(stored(&backend, &["shaker_effects", "gear"]), json!(4));
    drop(app.take_threads());
}

#[test]
fn a_profile_without_an_engine_type_gets_the_original_and_the_library_opens_at_shakers() {
    let backend = FakeBackend::new();
    let mut app = shake(&backend, "acc");
    backend.docs.lock().unwrap()["tune"].as_object_mut().unwrap().remove("shaker_engine_type");
    app.refresh_docs();
    super::super::reload::profile_load(&mut app).unwrap();
    assert_eq!(stored(&backend, &["shaker_engine_type"]), json!("O"));
    // Any stored type but "O", none included, is the Enhanced one.
    for other in [serde_json::Value::Null, json!("X"), json!(0), json!("o")] {
        backend.docs.lock().unwrap()["tune"]["shaker_engine_type"] = other.clone();
        app.refresh_docs();
        super::super::reload::profile_load(&mut app).unwrap();
        assert_eq!(stored(&backend, &["shaker_engine_type"]), json!("E"), "{other}");
    }
    dispatch(&mut app, "action", "Shake:@library:", "").unwrap();
    assert_eq!((app.ui.screen, app.ui.section), (Screen::DownloadedProfiles, 1));
    drop(app.take_threads());
}

#[test]
fn the_filter_dialog_texts_have_entries() {
    let backend = FakeBackend::new();
    let mut app = shake(&backend, "acc");
    backend.translations.lock().unwrap().clear();
    for effect in ["engine", "road_bump"] {
        app.ui.dialog = None;
        open_filter(&mut app, effect).unwrap();
        crate::ui::render(&app, Region::Dialog);
    }
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

fn profile_patches_of(backend: &FakeBackend, table: &str) -> Vec<serde_json::Value> {
    backend.patches("profile").into_iter().filter(|patch| patch.get(table).is_some()).collect()
}

#[test]
fn stored_effect_values_and_switches_show_and_a_stored_engine_type_stays() {
    let backend = FakeBackend::new();
    let mut app = shake(&backend, "acc");
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["tune"]["shaker_engine_type"] = json!("E");
        docs["tune"]["shaker_effects"]["engine"] = json!(7);
        docs["tune"]["shaker_switch"]["engine"] = json!(false);
        docs["tune"]["shaker_switch"]["road_bump"] = json!("n");
        docs["tune"]["shaker_switch"]["impact"] = json!(true);
    }
    app.refresh_docs();
    backend.calls.lock().unwrap().clear();
    super::super::reload::profile_load(&mut app).unwrap();
    assert!(profile_patches_of(&backend, "shaker_engine_type").is_empty(), "a stored engine type stays");
    assert_eq!(stored(&backend, &["shaker_engine_type"]), json!("E"));
    let html = main(&app);
    assert!(html.contains("<option value=\"1\" selected>Enhanced</option>"));
    let engine = tag(&html, "c-shake-engine");
    assert!(engine.contains("value=\"7\"") && engine.contains(" disabled"), "{engine}");
    assert!(!tag(&html, "c-shake-engine-toggle").contains(" checked"));
    assert!(tag(&html, "c-shake-road_bump").contains(" disabled"), "the text n is off");
    assert!(!tag(&html, "c-shake-road_bump-toggle").contains(" checked"));
    assert!(!tag(&html, "c-shake-impact").contains(" disabled"));
    assert!(tag(&html, "c-shake-impact-toggle").contains(" checked"));
    drop(app.take_threads());
}

/// A wrapper recording the tune assignments, refusing the engine type's while `reject` is set.
struct AssignmentProbe {
    inner: std::sync::Arc<FakeBackend>,
    reject: std::sync::atomic::AtomicBool,
    assigned: std::sync::Mutex<Vec<(Vec<String>, LegacyValue)>>,
}

impl Backend for AssignmentProbe {
    fn call(&self, method: Method, path: &str, body: Option<serde_json::Value>) -> Result<serde_json::Value, Failure> {
        self.inner.call(method, path, body)
    }
    fn assign_tune_value(&self, path: &[&str], value: &LegacyValue) -> Result<(), Failure> {
        self.assigned.lock().unwrap().push((path.iter().map(|p| (*p).to_owned()).collect(), value.clone()));
        if path == ["shaker_engine_type"] && self.reject.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(Failure { status: 500, detail: "TypeError: engine default assignment failed".into() });
        }
        self.inner.assign_tune_value(path, value)
    }
    fn upload(&self, path: &str, bytes: Vec<u8>) -> Result<serde_json::Value, Failure> {
        self.inner.upload(path, bytes)
    }
    fn fetch(&self, path: &str) -> Result<(Vec<u8>, String), Failure> {
        self.inner.fetch(path)
    }
    fn translate(&self, text: &str) -> String {
        self.inner.translate(text)
    }
    fn excluded_strings(&self) -> Vec<String> {
        self.inner.excluded_strings()
    }
}

#[test]
fn a_profile_without_the_engine_type_stores_o_and_a_refused_write_is_reported() {
    let inner = FakeBackend::new();
    let mut app = shake(&inner, "acc");
    let backend = std::sync::Arc::new(AssignmentProbe {
        inner: inner.clone(),
        reject: std::sync::atomic::AtomicBool::new(true),
        assigned: Default::default(),
    });
    app.backend = backend.clone();
    inner.docs.lock().unwrap()["tune"].as_object_mut().unwrap().remove("shaker_engine_type");
    app.refresh_docs();
    let load = super::super::reload::profile_load;
    assert_eq!(load(&mut app), Err("TypeError: engine default assignment failed".into()));
    let engine: Vec<_> =
        backend.assigned.lock().unwrap().iter().filter(|(path, _)| path == &["shaker_engine_type"]).cloned().collect();
    assert_eq!(engine, [(vec!["shaker_engine_type".to_owned()], LegacyValue::Str("O".into()))]);
    assert!(inner.docs.lock().unwrap()["tune"].get("shaker_engine_type").is_none());
    backend.reject.store(false, std::sync::atomic::Ordering::SeqCst);
    load(&mut app).unwrap();
    assert_eq!(stored(&inner, &["shaker_engine_type"]), json!("O"));
    assert!(main(&app).contains("<option value=\"0\" selected>Original</option>"));
    drop(app.take_threads());
}

#[test]
fn malformed_effect_values_are_shown_as_zero_and_not_rewritten_by_a_profile_load() {
    let backend = FakeBackend::new();
    let mut app = shake(&backend, "acc");
    for bad in [serde_json::Value::Null, json!("bad"), json!([])] {
        backend.docs.lock().unwrap()["tune"]["shaker_effects"]["engine"] = bad.clone();
        app.refresh_docs();
        backend.calls.lock().unwrap().clear();
        super::super::tuning::on_profile_load(&mut app);
        assert!(profile_patches_of(&backend, "shaker_effects").is_empty(), "{bad}");
        assert_eq!(stored(&backend, &["shaker_effects", "engine"]), bad);
        assert!(tag(&main(&app), "c-shake-engine").contains("value=\"0\""), "{bad}");
    }
    backend.docs.lock().unwrap()["tune"]["shaker_effects"].as_object_mut().unwrap().remove("engine");
    app.refresh_docs();
    super::super::tuning::on_profile_load(&mut app);
    assert!(backend.docs.lock().unwrap()["tune"]["shaker_effects"].get("engine").is_none());
    assert!(tag(&main(&app), "c-shake-engine").contains("value=\"0\""));
    drop(app.take_threads());
}

#[test]
fn the_led_shift_point_shows_while_a_shaker_effect_is_malformed() {
    let backend = FakeBackend::new();
    let mut app = shake(&backend, "acc");
    backend.docs.lock().unwrap()["tune"]["gear_shift_perc"] = json!(87);
    backend.docs.lock().unwrap()["tune"]["shaker_effects"]["engine"] = serde_json::Value::Null;
    app.refresh_docs();
    super::super::tuning::on_profile_load(&mut app);
    navigate(&mut app, Screen::Led, 0);
    let html = super::super::led::main(&app);
    assert!(tag(&html, "c-led-gear_shift_perc").contains("value=\"87\"") && html.contains(">87 %<"));
    assert_eq!(stored(&backend, &["shaker_effects", "engine"]), serde_json::Value::Null);
    drop(app.take_threads());
}

#[test]
fn an_unknown_engine_choice_is_refused_and_nothing_is_stored() {
    let backend = FakeBackend::new();
    let mut app = shake(&backend, "acc");
    let previous = stored(&backend, &["shaker_engine_type"]);
    for value in ["", "7", "Enhanced"] {
        assert!(dispatch(&mut app, "set", "Shake:engine_type", value).is_err(), "{value:?}");
        assert_eq!(stored(&backend, &["shaker_engine_type"]), previous);
    }
    dispatch(&mut app, "set", "Shake:engine_type", "0").unwrap();
    assert_eq!(stored(&backend, &["shaker_engine_type"]), json!("O"));
    drop(app.take_threads());
}

#[test]
fn filter_values_that_are_not_numbers_are_refused_and_each_fade_end_is_stored_on_its_own() {
    let backend = FakeBackend::new();
    let mut app = shake(&backend, "acc");
    backend.docs.lock().unwrap()["tune"]["shaker_cutoff"]["engine_fadeoff_end"] = json!(95);
    app.refresh_docs();
    dispatch(&mut app, "action", "Shake:road_bump:0", "").unwrap();
    let before = stored(&backend, &["shaker_cutoff", "road_bump"]);
    for value in ["NaN", "inf", "bad"] {
        assert!(dispatch(&mut app, "dialog-field", "cutoff", value).is_err(), "{value}");
        assert_eq!(stored(&backend, &["shaker_cutoff", "road_bump"]), before);
    }
    assert!(app.ui.dialog.is_some(), "the dialog stays open");
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    dispatch(&mut app, "action", "Shake:engine:0", "").unwrap();
    let end = stored(&backend, &["shaker_cutoff", "engine_fadeoff_end"]);
    dispatch(&mut app, "dialog-field", "fade_start", "25").unwrap();
    assert!(dispatch(&mut app, "dialog-field", "fade_end", "bad").is_err());
    assert_eq!(stored(&backend, &["shaker_cutoff", "engine_fadeoff_start"]), json!(25));
    assert_eq!(stored(&backend, &["shaker_cutoff", "engine_fadeoff_end"]), end);
    drop(app.take_threads());
}

#[test]
fn the_filter_dialog_shows_the_values_the_service_holds() {
    let backend = FakeBackend::new();
    let mut app = shake(&backend, "acc");
    backend.docs.lock().unwrap()["tune"]["shaker_cutoff"]["road_bump"] = json!(123);
    app.refresh_docs();
    dispatch(&mut app, "action", "Shake:road_bump:0", "").unwrap();
    let dialog = crate::ui::render(&app, Region::Dialog);
    assert!(tag(&dialog, "filter-cutoff").contains("value=\"123\""), "{dialog}");
    drop(app.take_threads());
}

#[test]
fn the_filter_dialog_opens_on_stored_text_and_null_values_without_writing() {
    let backend = FakeBackend::new();
    let mut app = shake(&backend, "acc");
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["tune"]["shaker_cutoff"]["road_bump"] = json!(" 123 ");
        docs["tune"]["shaker_resonance"]["road_bump"] = json!("2");
        docs["tune"]["shaker_cutoff"]["engine_fadeoff_start"] = json!("25");
        docs["tune"]["shaker_cutoff"]["engine_fadeoff_end"] = json!(75);
        docs["tune"]["shaker_cutoff"]["engine"] = serde_json::Value::Null;
    }
    app.refresh_docs();
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "action", "Shake:road_bump:0", "").unwrap();
    let dialog = crate::ui::render(&app, Region::Dialog);
    assert!(tag(&dialog, "filter-cutoff").contains("value=\"123\""));
    assert!(tag(&dialog, "filter-resonance").contains("value=\"2\""));
    dispatch(&mut app, "dialog-close", "", "").unwrap();
    dispatch(&mut app, "action", "Shake:engine:0", "").unwrap();
    let dialog = crate::ui::render(&app, Region::Dialog);
    assert!(tag(&dialog, "filter-fade").contains("value=\"25\"") && dialog.contains(">25 - 75<"));
    dispatch(&mut app, "dialog-close", "", "").unwrap();
    assert!(backend.patches("profile").is_empty(), "opening and cancelling unchanged dialogs writes nothing");
    assert_eq!(stored(&backend, &["shaker_cutoff", "road_bump"]), json!(" 123 "));
    assert_eq!(stored(&backend, &["shaker_cutoff", "engine"]), serde_json::Value::Null);
    drop(app.take_threads());
}

/// Opening a filter dialog (any effect's) stores the engine fade-off range as its control holds it: whole numbers,
/// ordered and within 0…100. Cancel restores the values from before.
#[test]
fn opening_a_filter_dialog_tidies_the_engine_fade_off_range() {
    for (effect, (start, end), tidied) in [
        ("engine", (json!(-10), json!(130)), (json!(0), json!(100))),
        ("road_bump", (json!(95.4), json!(80)), (json!(80), json!(95))),
        ("engine", (json!(40.5), json!(60.5)), (json!(40), json!(60))),
    ] {
        let backend = FakeBackend::new();
        let mut app = shake(&backend, "acc");
        {
            let mut docs = backend.docs.lock().unwrap();
            docs["tune"]["shaker_cutoff"]["engine_fadeoff_start"] = start.clone();
            docs["tune"]["shaker_cutoff"]["engine_fadeoff_end"] = end.clone();
        }
        app.refresh_docs();
        dispatch(&mut app, "action", &format!("Shake:{effect}:0"), "").unwrap();
        let range = (
            stored(&backend, &["shaker_cutoff", "engine_fadeoff_start"]),
            stored(&backend, &["shaker_cutoff", "engine_fadeoff_end"]),
        );
        assert_eq!(range, tidied, "{effect} {start} {end}");
        assert!(app.ui.dialog.is_some());
        dispatch(&mut app, "dialog-close", "", "").unwrap();
        assert_eq!(stored(&backend, &["shaker_cutoff", "engine_fadeoff_start"]), start);
        assert_eq!(stored(&backend, &["shaker_cutoff", "engine_fadeoff_end"]), end);
        drop(app.take_threads());
    }
}

/// Opening a filter dialog stores only the tidied fade-off range: the effect's filter switch, cutoff and
/// resonance are left exactly as stored, even when their controls would hold them differently, and the dialog
/// still opens. Cancel leaves them as they were.
#[test]
fn opening_a_filter_dialog_stores_only_the_fade_off_range() {
    let cases = [
        // (switch, cutoff, resonance) stored, none of them as their controls hold them.
        (json!(true), json!(600), json!(0)),
        (json!("0"), json!(123.5), json!(40.5)),
        (json!(1), json!(-4), json!(100.2)),
        (json!(2), json!(" 123 "), serde_json::Value::Null),
    ];
    for (switch, cutoff, resonance) in cases {
        let backend = FakeBackend::new();
        let mut app = shake(&backend, "acc");
        {
            let mut docs = backend.docs.lock().unwrap();
            docs["tune"]["shaker_dsp_type"]["road_bump"] = switch.clone();
            docs["tune"]["shaker_cutoff"]["road_bump"] = cutoff.clone();
            docs["tune"]["shaker_resonance"]["road_bump"] = resonance.clone();
            docs["tune"]["shaker_cutoff"]["engine_fadeoff_start"] = json!(120);
            docs["tune"]["shaker_cutoff"]["engine_fadeoff_end"] = json!(95);
        }
        app.refresh_docs();
        backend.calls.lock().unwrap().clear();
        dispatch(&mut app, "action", "Shake:road_bump:0", "").unwrap();
        assert!(app.ui.dialog.is_some(), "{switch} {cutoff} {resonance}: the dialog opens");
        let after = (
            stored(&backend, &["shaker_dsp_type", "road_bump"]),
            stored(&backend, &["shaker_cutoff", "road_bump"]),
            stored(&backend, &["shaker_resonance", "road_bump"]),
        );
        assert_eq!(after, (switch.clone(), cutoff.clone(), resonance.clone()), "{switch} {cutoff} {resonance}");
        assert!(after.1.is_f64() == cutoff.is_f64() && after.2.is_f64() == resonance.is_f64(), "{after:?}");
        // The only writes are the two ends of the range.
        let written: Vec<String> = backend
            .patches("profile")
            .iter()
            .flat_map(|patch| {
                let tables = patch.as_object().cloned().unwrap_or_default();
                tables
                    .into_iter()
                    .flat_map(|(table, keys)| {
                        keys.as_object()
                            .cloned()
                            .unwrap_or_default()
                            .into_iter()
                            .map(move |(key, _)| format!("{table}.{key}"))
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(
            written,
            ["shaker_cutoff.engine_fadeoff_start", "shaker_cutoff.engine_fadeoff_end"],
            "{switch} {cutoff} {resonance}"
        );
        assert_eq!(
            (
                stored(&backend, &["shaker_cutoff", "engine_fadeoff_start"]),
                stored(&backend, &["shaker_cutoff", "engine_fadeoff_end"]),
            ),
            (json!(95), json!(100))
        );
        // Cancel leaves the switch, cutoff and resonance as they were.
        dispatch(&mut app, "dialog-close", "", "").unwrap();
        for (table, before) in
            [("shaker_dsp_type", &switch), ("shaker_cutoff", &cutoff), ("shaker_resonance", &resonance)]
        {
            assert_eq!(&stored(&backend, &[table, "road_bump"]), before, "{table} after cancel");
        }
        drop(app.take_threads());
    }
}

#[test]
fn cancel_restores_the_snapshot_and_leaves_a_missing_value_missing() {
    let backend = FakeBackend::new();
    let mut app = shake(&backend, "acc");
    backend.docs.lock().unwrap()["tune"]["shaker_resonance"].as_object_mut().unwrap().remove("road_bump");
    app.refresh_docs();
    let before = stored(&backend, &["shaker_cutoff", "road_bump"]);
    dispatch(&mut app, "action", "Shake:road_bump:0", "").unwrap();
    assert!(tag(&crate::ui::render(&app, Region::Dialog), "filter-resonance").contains("value=\"0\""));
    dispatch(&mut app, "dialog-field", "cutoff", "321").unwrap();
    assert_eq!(stored(&backend, &["shaker_cutoff", "road_bump"]), json!(321));
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "dialog-close", "", "").unwrap();
    assert_eq!(stored(&backend, &["shaker_cutoff", "road_bump"]), before);
    assert!(profile_patches_of(&backend, "shaker_resonance").is_empty());
    assert!(backend.docs.lock().unwrap()["tune"]["shaker_resonance"].get("road_bump").is_none());
    assert!(app.ui.dialog.is_none());
    drop(app.take_threads());
}

#[test]
fn the_filter_switch_stores_whole_numbers_and_a_stored_two_counts_as_on() {
    let backend = FakeBackend::new();
    let mut app = shake(&backend, "acc");
    dispatch(&mut app, "action", "Shake:road_bump:0", "").unwrap();
    dispatch(&mut app, "dialog-field", "enabled", "0").unwrap();
    assert_eq!(stored(&backend, &["shaker_dsp_type", "road_bump"]), json!(0));
    assert!(tag(&crate::ui::render(&app, Region::Dialog), "filter-cutoff").contains(" disabled"));
    dispatch(&mut app, "dialog-field", "enabled", "1").unwrap();
    assert_eq!(stored(&backend, &["shaker_dsp_type", "road_bump"]), json!(1));
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    backend.docs.lock().unwrap()["tune"]["shaker_dsp_type"]["road_bump"] = json!(2);
    app.refresh_docs();
    dispatch(&mut app, "action", "Shake:road_bump:0", "").unwrap();
    let dialog = crate::ui::render(&app, Region::Dialog);
    assert!(tag(&dialog, "filter-enabled").contains(" checked"));
    assert!(!tag(&dialog, "filter-cutoff").contains(" disabled"));
    assert!(!tag(&dialog, "filter-resonance").contains(" disabled"));
    drop(app.take_threads());
}

#[test]
fn a_filter_write_without_its_table_fails_and_stores_nothing() {
    let backend = FakeBackend::new();
    let mut app = shake(&backend, "acc");
    dispatch(&mut app, "action", "Shake:road_bump:0", "").unwrap();
    backend.docs.lock().unwrap()["tune"].as_object_mut().unwrap().remove("shaker_cutoff");
    let error = dispatch(&mut app, "dialog-field", "cutoff", "87").unwrap_err();
    assert!(error.contains("KeyError") && error.contains("shaker_cutoff"), "{error}");
    assert!(backend.docs.lock().unwrap()["tune"].get("shaker_cutoff").is_none());
    drop(app.take_threads());
}

#[test]
fn fade_ends_are_stored_as_whole_numbers_and_cancel_restores_their_saved_kinds() {
    let backend = FakeBackend::new();
    let mut app = shake(&backend, "acc");
    backend.docs.lock().unwrap()["tune"]["shaker_cutoff"]["engine_fadeoff_start"] = json!(85.5);
    backend.docs.lock().unwrap()["tune"]["shaker_cutoff"]["engine_fadeoff_end"] = json!(95.0);
    app.refresh_docs();
    dispatch(&mut app, "action", "Shake:engine:0", "").unwrap();
    dispatch(&mut app, "dialog-field", "fade_start", "25").unwrap();
    dispatch(&mut app, "dialog-field", "fade_end", "78").unwrap();
    assert_eq!(stored(&backend, &["shaker_cutoff", "engine_fadeoff_start"]), json!(25));
    assert_eq!(stored(&backend, &["shaker_cutoff", "engine_fadeoff_end"]), json!(78));
    let dialog = crate::ui::render(&app, Region::Dialog);
    assert!(dialog.contains(">25 - 78<"));
    dispatch(&mut app, "dialog-close", "", "").unwrap();
    assert_eq!(stored(&backend, &["shaker_cutoff", "engine_fadeoff_start"]), json!(85.5));
    assert_eq!(stored(&backend, &["shaker_cutoff", "engine_fadeoff_end"]), json!(95.0));
    drop(app.take_threads());
}

#[test]
fn test_buttons_follow_the_test_effect_setting() {
    let backend = FakeBackend::new();
    let mut app = shake(&backend, "acc");
    for (flag, offered) in
        [(json!("no"), false), (json!("False"), false), (json!("bad"), false), (json!("y"), true), (json!(true), true)]
    {
        backend.docs.lock().unwrap()["settings"]["shakers"]["test_effect_enabled"] = flag.clone();
        app.refresh_docs();
        assert_eq!(main(&app).contains(">Test<"), offered, "{flag}");
    }
    // A value the settings keep as text: the buttons stay away.
    let settings = openrigcue_config::settings::Settings::default()
        .apply_ui_patch(&json!({"shakers": {"test_effect_enabled": "bad"}}))
        .unwrap();
    let raw = LegacyValue::Dict(settings.native_snapshot());
    assert_eq!(
        crate::backend::native_field(&raw, &["SHAKEBOX", "enable_test_effect"]).unwrap(),
        &LegacyValue::from("bad")
    );
    backend.docs.lock().unwrap()["settings"] = settings.ui_view().unwrap();
    app.refresh_docs();
    let html = main(&app);
    assert!(!html.contains(">Test<") && html.contains(">Filter<"));
    drop(app.take_threads());
}

/// A wrapper recording the reads and the request of an effect's test.
struct EffectTestBackend {
    inner: std::sync::Arc<FakeBackend>,
    events: std::sync::Mutex<Vec<&'static str>>,
    listeners: std::sync::Mutex<std::collections::VecDeque<Result<LegacyValue, Failure>>>,
    test_failure: std::sync::Mutex<Option<String>>,
}

impl Backend for EffectTestBackend {
    fn call(&self, method: Method, path: &str, body: Option<serde_json::Value>) -> Result<serde_json::Value, Failure> {
        if method == Method::Post && path.starts_with("shakers/test/") {
            self.events.lock().unwrap().push("test");
            self.inner.calls.lock().unwrap().push((method, path.into(), body));
            return self
                .test_failure
                .lock()
                .unwrap()
                .clone()
                .map_or(Ok(serde_json::Value::Null), |detail| Err(Failure { status: 500, detail }));
        }
        self.inner.call(method, path, body)
    }
    fn native_setting(&self, section: &str, key: &str) -> Result<LegacyValue, Failure> {
        if section == "SHAKEBOX" && key == "soundcard" {
            self.events.lock().unwrap().push("soundcard");
        }
        self.inner.native_setting(section, key)
    }
    fn session_value(&self, parameter: &str) -> Result<LegacyValue, Failure> {
        if parameter == "listener_running" {
            self.events.lock().unwrap().push("listener");
            if let Some(value) = self.listeners.lock().unwrap().pop_front() {
                return value;
            }
        }
        self.inner.session_value(parameter)
    }
    fn upload(&self, path: &str, bytes: Vec<u8>) -> Result<serde_json::Value, Failure> {
        self.inner.upload(path, bytes)
    }
    fn fetch(&self, path: &str) -> Result<(Vec<u8>, String), Failure> {
        self.inner.fetch(path)
    }
    fn translate(&self, text: &str) -> String {
        self.inner.translate(text)
    }
    fn excluded_strings(&self) -> Vec<String> {
        self.inner.excluded_strings()
    }
}

/// The app on Shake with the effects' Test buttons and `card` as the shakers' sound device, its service
/// recording the test's reads; and the engine's Test argument.
fn effect_test(card: &str) -> (MyApp, std::sync::Arc<EffectTestBackend>, String) {
    let inner = FakeBackend::new();
    {
        let mut docs = inner.docs.lock().unwrap();
        docs["settings"]["shakers"]["test_effect_enabled"] = json!(true);
        docs["settings"]["shakers"]["sound_card"] = json!(card);
    }
    let mut app = shake(&inner, "acc");
    let backend = std::sync::Arc::new(EffectTestBackend {
        inner,
        events: Default::default(),
        listeners: Default::default(),
        test_failure: Default::default(),
    });
    app.backend = backend.clone();
    let index = page()
        .control("engine")
        .unwrap()
        .buttons
        .iter()
        .position(|b| matches!(b, Widget::Button { run: Action::TestShakerEffect { .. }, .. }))
        .unwrap();
    backend.events.lock().unwrap().clear();
    backend.inner.calls.lock().unwrap().clear();
    (app, backend, format!("Shake:engine:{index}"))
}

fn effect_test_posts(backend: &EffectTestBackend) -> Vec<serde_json::Value> {
    let calls = backend.inner.calls.lock().unwrap();
    calls
        .iter()
        .filter(|(m, p, _)| *m == Method::Post && p == "shakers/test/engine")
        .map(|(_, _, b)| b.clone().unwrap())
        .collect()
}

#[test]
fn the_effect_test_reads_the_sound_device_first_and_sends_the_listeners() {
    let (mut app, backend, arg) = effect_test("USB Sound Device");
    backend.listeners.lock().unwrap().extend([Ok(LegacyValue::from("none")), Ok(LegacyValue::from("none"))]);
    dispatch(&mut app, "action", &arg, "").unwrap();
    let events = backend.events.lock().unwrap().clone();
    assert_eq!((events.first(), events.last()), (Some(&"soundcard"), Some(&"test")), "{events:?}");
    assert!(events[1..events.len() - 1].iter().all(|e| *e == "listener") && events.len() > 2, "{events:?}");
    assert_eq!(effect_test_posts(&backend), [json!({"current_listener": "none", "test_listener": "test"})]);
    assert!(app.ui.notices.history.last().unwrap().message.contains("Playing the test"));
    drop(app.take_threads());
}

#[test]
fn without_a_sound_device_the_effect_test_reads_nothing_more_and_asks_for_one() {
    let (mut app, backend, arg) = effect_test("None");
    dispatch(&mut app, "action", &arg, "").unwrap();
    assert_eq!(*backend.events.lock().unwrap(), ["soundcard"]);
    assert!(effect_test_posts(&backend).is_empty());
    assert!(app.ui.notices.history.last().unwrap().message.contains("sound device"));
    drop(app.take_threads());
}

#[test]
fn a_failing_effect_test_or_listener_read_is_reported() {
    let (mut app, backend, arg) = effect_test("USB Sound Device");
    *backend.test_failure.lock().unwrap() = Some("TypeError: fixture group failure".into());
    let error = dispatch(&mut app, "action", &arg, "").unwrap_err();
    assert!(error.contains("TypeError: fixture group failure"), "{error}");
    assert_eq!(backend.events.lock().unwrap().last(), Some(&"test"));
    *backend.test_failure.lock().unwrap() = None;
    backend.inner.calls.lock().unwrap().clear();
    backend
        .listeners
        .lock()
        .unwrap()
        .extend([Err(Failure { status: 500, detail: "KeyError: 'listener_running'".into() })]);
    let error = dispatch(&mut app, "action", &arg, "").unwrap_err();
    assert!(error.contains("KeyError: 'listener_running'"), "{error}");
    assert!(effect_test_posts(&backend).is_empty(), "no test after a failed listener read");
    drop(app.take_threads());
}

#[test]
fn locked_and_unused_effects_disable_and_refuse_their_switch_and_buttons() {
    let filter = page()
        .control("road_bump")
        .unwrap()
        .buttons
        .iter()
        .position(|b| matches!(b, Widget::Button { run: Action::ShakerFilter { .. }, .. }))
        .unwrap();
    // An effect the game does not use (road bump follows the heave column).
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["tune"]["shaker_switch"]["road_bump"] = json!(true);
    let mut app = shake(&backend, &game_without("heave"));
    let html = main(&app);
    assert!(tag(&html, "c-shake-road_bump-toggle").contains(" disabled"));
    assert!(html.contains(&format!("data-arg=\"Shake:road_bump:{filter}\" disabled>Filter<")));
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "set", "Shake:road_bump:toggle", "0").unwrap();
    dispatch(&mut app, "set", "Shake:road_bump", "4").unwrap();
    assert!(backend.patches("profile").is_empty() && backend.patches("shakers").is_empty());
    dispatch(&mut app, "action", &format!("Shake:road_bump:{filter}"), "").unwrap();
    open_filter(&mut app, "road_bump").unwrap();
    assert!(app.ui.dialog.is_none(), "no filter dialog for it");
    drop(app.take_threads());
    // Stand-by: every effect's switch and buttons.
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    navigate(&mut app, Screen::Shake, 0);
    let html = main(&app);
    assert!(tag(&html, "c-shake-engine-toggle").contains(" disabled"));
    let engine_filter = page()
        .control("engine")
        .unwrap()
        .buttons
        .iter()
        .position(|b| matches!(b, Widget::Button { run: Action::ShakerFilter { .. }, .. }))
        .unwrap();
    dispatch(&mut app, "action", &format!("Shake:engine:{engine_filter}"), "").unwrap();
    assert!(app.ui.dialog.is_none());
    drop(app.take_threads());
}
