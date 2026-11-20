//! The widget tree's semantics: rendering, change tracking, keys, texts, events.

use std::collections::HashMap;
use std::sync::Arc;

use openrigcue_model::native_value::LegacyValue;
use serde_json::{json, Value};

use super::classes::{Bound, Signature};
use super::gui::{cb, dispatch, dispatch_checked, parse_parameters, HasGui};
use super::*;

struct App {
    gui: Gui<App>,
    seen: Vec<Value>,
}

impl HasGui for App {
    fn gui(&mut self) -> &mut Gui<App> {
        &mut self.gui
    }

    fn class_event(
        &mut self,
        _: WidgetId,
        signature: &'static Signature,
        bound: &Bound,
    ) -> Option<Result<Vec<LegacyValue>, String>> {
        (signature.qualname == "OrcAction.onact")
            .then(|| Ok(bound.positional.iter().map(|text| LegacyValue::Str(format!("<{text}>"))).collect()))
    }

    fn app_method(&mut self, _: WidgetId, name: &str, parameters: &[(&str, &str)]) -> Option<Result<(), String>> {
        (name == "onpixelratio").then(|| {
            self.seen.push(json!(parameters.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>()));
            Ok(())
        })
    }
}

fn app() -> App {
    App { gui: Gui::new(Arc::new(Untranslated)), seen: Vec::new() }
}

/// Rendering: tag, attributes in insertion order, the `style` text, children in key order.
#[test]
fn renders_tags_attributes_styles_and_children() {
    let mut a = app();
    let g = &mut a.gui;
    let label = g.create("p", "Label");
    g.set_text(label, "a<b");
    g.set_style(label, "width", "150px");
    let row = g.create("div", "orc-region");
    g.set_attr(row, "aria-live", Some("polite"));
    g.set_attr(row, "hidden", None);
    g.append(row, label, "label");
    let id = g.identifier(row).to_owned();
    let lid = g.identifier(label).to_owned();
    g.set_root(row);
    assert_eq!(
        g.full_html(),
        format!(
            "<div id=\"{id}\" class=\"orc-region\" aria-live=\"polite\" hidden>\
             <p id=\"{lid}\" class=\"Label\" data-parent-widget=\"{id}\" style=\"width:150px\">a&lt;b</p></div>"
        )
    );
    g.set_identifier(row, "orc-main");
    assert_eq!(g.by_identifier("orc-main"), Some(row));
    assert_eq!(g.by_identifier(&id), None);
    assert!(g.attributes_html(row).starts_with("id=\"orc-main\" class=\"orc-region\""));
}

/// Only the topmost changed widget of a branch is sent again; unchanged siblings are not.
#[test]
fn updates_only_changed_widgets() {
    let mut a = app();
    let g = &mut a.gui;
    let one = g.create("div", "OrcRegion");
    let two = g.create("div", "OrcRegion");
    let root = g.create("div", "Container");
    g.append(root, one, "one");
    g.append(root, two, "two");
    g.set_root(root);
    g.full_html();
    assert!(g.gui_update().is_empty());
    // The same HTML again changes nothing.
    g.add_child_text(two, "html", "<b>2</b>", false);
    g.gui_update();
    g.add_child_text(two, "html", "<b>2</b>", false);
    assert!(!g.needs_update());
    g.add_child_text(two, "html", "<b>3</b>", false);
    assert!(g.needs_update());
    let messages = g.gui_update();
    assert_eq!(messages.len(), 1);
    assert!(messages[0].starts_with(&format!("1{},", g.identifier(two))));
    g.add_child_text(two, "html", "<b>4</b>", false);
    g.set_style(root, "color", "red");
    let messages = g.gui_update();
    assert_eq!(messages.len(), 1);
    assert!(messages[0].starts_with(&format!("1{},", g.identifier(root))));
}

/// Appending under an existing key moves it to the end of the render order; removing a child drops its key.
#[test]
fn readding_a_key_moves_it_last() {
    let mut a = app();
    let g = &mut a.gui;
    let one = g.create("div", "OrcRegion");
    let two = g.create("div", "OrcRegion");
    let root = g.create("div", "Container");
    g.append(root, one, "one");
    g.append(root, two, "two");
    g.append(root, one, "one");
    let keys = |g: &Gui<App>| g.children(root).into_iter().map(|(key, _)| key).collect::<Vec<_>>();
    assert_eq!(keys(g), ["two", "one"]);
    g.remove_child(root, two);
    assert_eq!(keys(g), ["one"]);
    assert_eq!(g.parent(one), Some(root));
    assert_eq!(g.child(root, "one"), Some(one));
}

/// A text is translated unless the translator excludes it, and escaped.
#[test]
fn texts_are_translated_and_escaped() {
    struct Language;
    impl Translate for Language {
        fn is_excluded_key(&self, text: &str) -> bool {
            text == "COM3"
        }
        fn translate(&self, text: &str) -> String {
            format!("translated: {text}")
        }
    }
    let mut a = App { gui: Gui::new(Arc::new(Language)), seen: Vec::new() };
    let label = a.gui.create("p", "Label");
    a.gui.set_text(label, "Wind & <fans>");
    assert_eq!(a.gui.get_text(label), "translated: Wind &amp; &lt;fans&gt;");
    a.gui.set_text(label, "COM3");
    assert_eq!(a.gui.get_text(label), "COM3");
    a.gui.set_text(label, "");
    assert_eq!(a.gui.get_text(label), "");
}

/// Connecting a listener sets the event's code; an event method's body runs before the listener, a name that
/// is no event of the class reaches nothing, and the body's own methods go to the application.
#[test]
fn events_reach_listeners() {
    let mut a = app();
    let action = a.gui.create("div", "OrcAction");
    a.gui.on(action, "onact", None, JsFlags::NONE, cb(|app: &mut App, _, args| app.seen.extend_from_slice(args)));
    let body = a.gui.create("body", "BODY");
    a.gui.on(
        body,
        "onload",
        Some(gui::JS_CALLBACK),
        JsFlags::NONE,
        cb(|app: &mut App, _, _| app.seen.push(json!("loaded"))),
    );
    let identifier = a.gui.identifier(body).to_owned();
    assert_eq!(a.gui.attr(body, "onload"), Some(format!("remi.sendCallback('{identifier}','onload');").as_str()));
    dispatch(&mut a, action, "onact", &parse_parameters("7|act=nav|9|arg=Games"));
    assert_eq!(a.seen, [json!("<nav>"), json!("<Games>"), json!("<>")]);
    a.seen.clear();
    dispatch(&mut a, body, "onload", &HashMap::new());
    dispatch(&mut a, action, "set_text", &parse_parameters("6|text=x"));
    dispatch(&mut a, body, "onpixelratio", &parse_parameters("13|pixel_ratio=2"));
    assert_eq!(a.seen, [json!("loaded"), json!(["pixel_ratio=2"])]);
    assert_eq!(
        dispatch_checked(&mut a, action, "onact", &HashMap::new()).unwrap_err(),
        "TypeError: OrcAction.onact() missing 1 required positional argument: 'act'"
    );
}

/// The parameter packet: `len|key=value` joined by `|`, values may hold `|` and `=`.
#[test]
fn parameters_parse() {
    let p = parse_parameters("6|x=0.25|9|y=a|b=c d");
    assert_eq!(p["x"], "0.25");
    assert_eq!(p["y"], "a|b=c d");
}

#[test]
fn float_strings() {
    assert_eq!(py_float_str(25.0), "25.0");
    assert_eq!(py_float_str(-7.5), "-7.5");
    assert_eq!(py_float_str(1e16), "1e+16");
    assert_eq!(py_float_str(0.0001), "0.0001");
    assert_eq!(py_float_str(0.00001), "1e-05");
    assert_eq!(py_float_str(33.333333333333336), "33.333333333333336");
}

/// Adding a class appends it unconditionally and removing a class removes one occurrence.
#[test]
fn class_strings_keep_duplicates_and_remove_one_occurrence() {
    let mut a = app();
    let root = a.gui.create("div", "orc");
    a.gui.add_class(root, "open");
    a.gui.add_class(root, "open");
    assert_eq!(a.gui.attr_class(root), "orc open open");
    a.gui.remove_class(root, "open");
    assert_eq!(a.gui.attr_class(root), "orc open");
    assert!(a.gui.has_class(root, "open"));
    a.gui.remove_class(root, "open");
    a.gui.remove_class(root, "open");
    assert_eq!(a.gui.attr_class(root), "orc");
    a.gui.set_attr_class(root, "orc redline");
    assert!(a.gui.has_class(root, "redline"));
}

/// An assignment flags nothing when the key already holds that value, a deletion nothing when the key is absent,
/// and a style update always flags a change.
#[test]
fn unchanged_assignments_flag_no_update_but_style_updates_always_do() {
    let mut a = app();
    let g = &mut a.gui;
    let label = g.create("p", "Label");
    let other = g.create("p", "Label");
    let root = g.create("div", "Container");
    g.append(root, label, "label");
    g.append(root, other, "other");
    g.set_root(root);
    g.full_html();
    assert!(g.gui_update().is_empty());
    g.set_attr(label, "title", Some("hint"));
    g.set_text(label, "changed");
    assert_eq!(g.gui_update().len(), 1);
    g.set_attr(label, "title", Some("hint"));
    g.set_text(label, "changed");
    assert!(!g.del_attr(label, "missing"));
    g.append(root, other, "other");
    assert!(!g.needs_update());
    assert!(g.gui_update().is_empty());
    g.set_style(label, "visibility", "hidden");
    assert!(g.needs_update());
    assert_eq!(g.gui_update().len(), 1);
    g.set_style(label, "visibility", "hidden");
    assert!(g.needs_update());
}

/// JavaScript goes to the sink while one is set, else it is queued.
#[test]
fn javascript_is_queued_or_sent() {
    let mut a = app();
    a.gui.execute_javascript("one();");
    assert_eq!(a.gui.take_javascript(), ["one();"]);
    let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = sent.clone();
    a.gui.set_javascript_sink(Arc::new(move |code| sink.lock().unwrap().push(code.to_owned())));
    a.gui.execute_javascript("two();");
    assert!(a.gui.take_javascript().is_empty());
    assert_eq!(*sent.lock().unwrap(), ["two();"]);
}
