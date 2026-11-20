//! The widget tree the interface is shown with: nodes kept by the program, rendered to HTML, with change
//! tracking, event listeners and the websocket protocol of the client script.
//!
//! Every widget lives in the [`Gui`] arena and is addressed by its [`WidgetId`]; its identifier (the HTML `id`, the
//! name the browser sends events for) is a number, unless set explicitly.

pub mod classes;
pub mod client;
pub mod gui;
pub mod parameters;

use std::collections::HashMap;
use std::sync::Arc;

use indexmap::IndexMap;
use serde_json::Value;

/// A widget of the [`Gui`] arena.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct WidgetId(pub usize);

/// A child of a node: a widget or a text.
#[derive(Debug, Clone)]
pub enum Child {
    Widget(WidgetId),
    Text(String),
}

/// One node of the tree: an HTML tag with its attributes, style and children.
#[derive(Debug, Clone)]
pub struct Node {
    pub tag: String,
    attributes: IndexMap<String, Option<String>>,
    style: IndexMap<String, String>,
    /// The children by key, in render order (re-adding a key moves it to the end).
    children: IndexMap<String, Child>,
    parent: Option<WidgetId>,
    changed: bool,
    backup_repr: String,
    /// The widget class the node was made with (what its events are looked up by).
    pub py_type: String,
}

impl Node {
    fn new(tag: &str) -> Node {
        Node {
            tag: tag.to_owned(),
            attributes: IndexMap::new(),
            style: IndexMap::new(),
            children: IndexMap::new(),
            parent: None,
            changed: true,
            backup_repr: String::new(),
            py_type: String::new(),
        }
    }
}

/// A connected listener, called with the emitter and the event's arguments; the userdata is captured by the closure.
pub type JsonListener<A> = Arc<dyn Fn(&mut A, WidgetId, &[Value]) + Send + Sync>;
pub type NativeListener<A> =
    Arc<dyn Fn(&mut A, WidgetId, &[openrigcue_model::native_value::LegacyValue]) + Send + Sync>;

pub enum Listener<A> {
    Json(JsonListener<A>),
    Native(NativeListener<A>),
}

impl<A> Clone for Listener<A> {
    fn clone(&self) -> Self {
        match self {
            Self::Json(callback) => Self::Json(callback.clone()),
            Self::Native(callback) => Self::Native(callback.clone()),
        }
    }
}
pub type JavascriptSink = Arc<dyn Fn(&str) + Send + Sync>;

/// The text translation hooks (`translate`, `is_excluded_key`).
pub trait Translate: Send + Sync {
    fn translate(&self, text: &str) -> String;
    fn is_excluded_key(&self, text: &str) -> bool;
}

/// No translation (tests).
pub struct Untranslated;

impl Translate for Untranslated {
    fn translate(&self, text: &str) -> String {
        text.to_owned()
    }
    fn is_excluded_key(&self, _text: &str) -> bool {
        false
    }
}

/// The widget arena with its connected listeners and the messages for the browsers.
pub struct Gui<A> {
    nodes: Vec<Node>,
    by_identifier: HashMap<String, WidgetId>,
    identifiers: Vec<String>,
    listeners: HashMap<(WidgetId, String), Listener<A>>,
    root: Option<WidgetId>,
    /// Whether an update is pending.
    need_update: bool,
    /// `execute_javascript` messages not yet sent.
    javascript: Vec<String>,
    javascript_sink: Option<JavascriptSink>,
    pub translator: Arc<dyn Translate>,
    next_identifier: u64,
}

/// HTML-escapes `text` (`&`, `<`, `>`; quotes are kept).
pub fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// A style dictionary as text: `k:v` joined by `;`.
fn jsonize(style: &IndexMap<String, String>) -> String {
    style.iter().map(|(k, v)| format!("{k}:{v}")).collect::<Vec<_>>().join(";")
}

impl<A: 'static> Gui<A> {
    pub fn new(translator: Arc<dyn Translate>) -> Gui<A> {
        Gui {
            nodes: Vec::new(),
            by_identifier: HashMap::new(),
            identifiers: Vec::new(),
            listeners: HashMap::new(),
            root: None,
            need_update: false,
            javascript: Vec::new(),
            javascript_sink: None,
            translator,
            // Large numbers, unique per object.
            next_identifier: 140_000_000_000_000,
        }
    }

    /// A new node with its tag, identifier and class.
    pub fn create(&mut self, tag: &str, class: &str) -> WidgetId {
        let id = WidgetId(self.nodes.len());
        let identifier = self.next_identifier.to_string();
        self.next_identifier += 16;
        let mut node = Node::new(tag);
        node.py_type = class.to_owned();
        node.attributes.insert("id".into(), Some(identifier.clone()));
        self.nodes.push(node);
        self.by_identifier.insert(identifier.clone(), id);
        self.identifiers.push(identifier);
        self.set_attr_class(id, class);
        id
    }

    pub fn node(&self, id: WidgetId) -> &Node {
        &self.nodes[id.0]
    }

    /// How many widgets the arena holds (`WidgetId(0)` up to it).
    pub fn widget_count(&self) -> usize {
        self.nodes.len()
    }

    fn node_mut(&mut self, id: WidgetId) -> &mut Node {
        &mut self.nodes[id.0]
    }

    /// The node's attributes as rendered.
    pub fn attributes_html(&self, id: WidgetId) -> String {
        self.repr_attributes(id)
    }

    pub fn identifier(&self, id: WidgetId) -> &str {
        &self.identifiers[id.0]
    }

    /// Sets the widget's identifier.
    pub fn set_identifier(&mut self, id: WidgetId, identifier: &str) {
        let old = std::mem::replace(&mut self.identifiers[id.0], identifier.to_owned());
        self.by_identifier.remove(&old);
        self.by_identifier.insert(identifier.to_owned(), id);
        self.set_attr(id, "id", Some(identifier));
    }

    pub fn by_identifier(&self, identifier: &str) -> Option<WidgetId> {
        self.by_identifier.get(identifier).copied()
    }

    // ---- change tracking ------------------------------------------------------------------------------------

    /// Marks the node as changed; the change climbs to the root.
    fn need_update(&mut self, id: WidgetId) {
        self.node_mut(id).changed = true;
        let mut id = id;
        loop {
            match self.nodes[id.0].parent {
                Some(parent) => id = parent,
                None => {
                    if Some(id) == self.root {
                        self.need_update = true;
                    }
                    return;
                }
            }
        }
    }

    /// Whether an update is pending for the application.
    pub fn needs_update(&self) -> bool {
        self.need_update
    }

    pub fn set_root(&mut self, id: WidgetId) {
        self.root = Some(id);
        self.need_update = true;
    }

    pub fn root(&self) -> Option<WidgetId> {
        self.root
    }

    // ---- attributes and style -------------------------------------------------------------------------------

    pub fn attr(&self, id: WidgetId, key: &str) -> Option<&str> {
        self.node(id).attributes.get(key).and_then(|v| v.as_deref())
    }

    pub fn has_attr(&self, id: WidgetId, key: &str) -> bool {
        self.node(id).attributes.contains_key(key)
    }

    /// Sets an attribute (`None`: an attribute without value, rendered as its name alone).
    pub fn set_attr(&mut self, id: WidgetId, key: &str, value: Option<&str>) {
        // Assigning the current value neither moves the key nor flags a change.
        if self.node(id).attributes.get(key).is_some_and(|current| current.as_deref() == value) {
            return;
        }
        self.node_mut(id).attributes.insert(key.to_owned(), value.map(str::to_owned));
        self.need_update(id);
    }

    /// Deletes an attribute (no error when missing).
    pub fn del_attr(&mut self, id: WidgetId, key: &str) -> bool {
        let removed = self.node_mut(id).attributes.shift_remove(key).is_some();
        if removed {
            self.need_update(id);
        }
        removed
    }

    pub fn style(&self, id: WidgetId, key: &str) -> Option<&str> {
        self.node(id).style.get(key).map(String::as_str)
    }

    /// Sets one style key as a style update, which flags a change whatever the values.
    pub fn set_style(&mut self, id: WidgetId, key: &str, value: &str) {
        self.node_mut(id).style.insert(key.to_owned(), value.to_owned());
        self.need_update(id);
    }

    // ---- classes ------------------------------------------------------------------------------------------

    /// Sets the whole class attribute.
    pub fn set_attr_class(&mut self, id: WidgetId, class: &str) {
        self.set_attr(id, "class", Some(class));
    }

    pub fn attr_class(&self, id: WidgetId) -> &str {
        self.attr(id, "class").unwrap_or("")
    }

    /// Adds a class: it is appended after a space, also when it is already there.
    pub fn add_class(&mut self, id: WidgetId, class: &str) {
        let joined = format!("{} {class}", self.attr_class(id));
        self.set_attr(id, "class", Some(&joined));
    }

    /// Removes a class: the class attribute is split at spaces, the class removed (the first occurrence only, a
    /// missing one ignored), and joined again.
    pub fn remove_class(&mut self, id: WidgetId, class: &str) {
        let current = self.attr_class(id).to_owned();
        let mut classes: Vec<&str> = current.split(' ').collect();
        if let Some(position) = classes.iter().position(|c| *c == class) {
            classes.remove(position);
        }
        self.set_attr(id, "class", Some(&classes.join(" ")));
    }

    pub fn has_class(&self, id: WidgetId, class: &str) -> bool {
        self.attr_class(id).split(' ').any(|c| c == class)
    }

    // ---- children ----------------------------------------------------------------------------------------

    /// Adds a widget as the child `key` of `parent`: the child records its parent, and the key moves to the
    /// end of the render order.
    fn add_child(&mut self, parent: WidgetId, key: &str, child: WidgetId) {
        let parent_identifier = self.identifier(parent).to_owned();
        {
            // The child's `data-parent-widget` attribute: a change only for another parent.
            let node = self.node_mut(child);
            let same_parent = node
                .attributes
                .get("data-parent-widget")
                .is_some_and(|current| current.as_deref() == Some(parent_identifier.as_str()));
            node.attributes.insert("data-parent-widget".into(), Some(parent_identifier));
            node.parent = Some(parent);
            if !same_parent {
                node.changed = true;
            }
        }
        // The render list always moves the key to the end; the child assignment flags a change only for another
        // value.
        let node = self.node_mut(parent);
        let same = matches!(node.children.get(key), Some(Child::Widget(current)) if *current == child);
        node.children.shift_remove(key);
        node.children.insert(key.to_owned(), Child::Widget(child));
        if !same {
            self.need_update(parent);
        }
    }

    /// Adds a text as the child `key` of `parent`; a child under the key `text` is translated unless
    /// `do_translation` is false.
    pub fn add_child_text(&mut self, parent: WidgetId, key: &str, text: &str, do_translation: bool) {
        let stored = if key == "text" && do_translation { self.translator.translate(text) } else { text.to_owned() };
        let node = self.node_mut(parent);
        // The key moves to the end of the render list; an unchanged text flags no change.
        let same = matches!(node.children.get(key), Some(Child::Text(current)) if *current == stored);
        node.children.shift_remove(key);
        node.children.insert(key.to_owned(), Child::Text(stored));
        if !same {
            self.need_update(parent);
        }
    }

    pub fn child(&self, parent: WidgetId, key: &str) -> Option<WidgetId> {
        match self.node(parent).children.get(key) {
            Some(Child::Widget(id)) => Some(*id),
            _ => None,
        }
    }

    pub fn child_text(&self, parent: WidgetId, key: &str) -> Option<&str> {
        match self.node(parent).children.get(key) {
            Some(Child::Text(text)) => Some(text),
            _ => None,
        }
    }

    /// The widget children, in render order, with their keys.
    pub fn children(&self, parent: WidgetId) -> Vec<(String, WidgetId)> {
        self.node(parent)
            .children
            .iter()
            .filter_map(|(k, c)| match c {
                Child::Widget(id) => Some((k.clone(), *id)),
                Child::Text(_) => None,
            })
            .collect()
    }

    pub fn parent(&self, id: WidgetId) -> Option<WidgetId> {
        self.node(id).parent
    }

    /// Removes the widget `child` from `parent`.
    pub fn remove_child(&mut self, parent: WidgetId, child: WidgetId) {
        let key = self
            .node(parent)
            .children
            .iter()
            .find(|(_, c)| matches!(c, Child::Widget(id) if *id == child))
            .map(|(k, _)| k.clone());
        if let Some(key) = key {
            self.node_mut(parent).children.shift_remove(&key);
            self.need_update(parent);
        }
    }

    /// Appends a child to a container: the key defaults to the child's identifier; returns the key.
    pub fn append(&mut self, parent: WidgetId, child: WidgetId, key: &str) -> String {
        let key = if key.is_empty() { self.identifier(child).to_owned() } else { key.to_owned() };
        self.add_child(parent, &key, child);
        key
    }

    // ---- text ----------------------------------------------------------------------------------------------

    /// Sets the shown text of a widget, translated unless the translator excludes it, and HTML-escaped.
    pub fn set_text(&mut self, id: WidgetId, text: &str) {
        let shown = if text.is_empty() || self.translator.is_excluded_key(text) {
            text.to_owned()
        } else {
            self.translator.translate(text)
        };
        self.add_child_text(id, "text", &escape(&shown), false);
    }

    /// The shown text, as [`Gui::set_text`] stored it (escaped).
    pub fn get_text(&self, id: WidgetId) -> String {
        self.child_text(id, "text").unwrap_or_default().to_owned()
    }

    // ---- events --------------------------------------------------------------------------------------------

    /// Connects an event of a widget: sets the event's JavaScript (when it has one) and connects `callback` (or
    /// disconnects with `None`).
    pub fn on(&mut self, id: WidgetId, event: &str, js: Option<&str>, flags: JsFlags, callback: Option<Listener<A>>) {
        if let Some(js) = js {
            let identifier = self.identifier(id).to_owned();
            let mut code = js.replace("%(emitter_identifier)s", &identifier).replace("%(event_name)s", event);
            if flags.stop_propagation {
                code.push_str("event.stopPropagation();");
            }
            if flags.prevent_default {
                code.push_str("event.preventDefault();");
            }
            self.set_attr(id, event, Some(&code));
        }
        match callback {
            Some(callback) => {
                self.listeners.insert((id, event.to_owned()), callback);
            }
            None => {
                self.listeners.remove(&(id, event.to_owned()));
            }
        }
    }

    pub fn listener(&self, id: WidgetId, event: &str) -> Option<Listener<A>> {
        self.listeners.get(&(id, event.to_owned())).cloned()
    }

    /// Queues JavaScript `code` for the browsers.
    pub fn execute_javascript(&mut self, code: &str) {
        if let Some(send) = &self.javascript_sink {
            send(code);
        } else {
            self.javascript.push(code.to_owned());
        }
    }

    /// The running application sends spontaneous messages synchronously. Standalone fixtures retain the queue
    /// for inspecting generated JavaScript.
    pub fn set_javascript_sink(&mut self, sink: JavascriptSink) {
        self.javascript_sink = Some(sink);
    }

    /// The `execute_javascript` messages since the last call.
    pub fn take_javascript(&mut self) -> Vec<String> {
        std::mem::take(&mut self.javascript)
    }

    // ---- rendering -----------------------------------------------------------------------------------------

    fn repr_attributes(&self, id: WidgetId) -> String {
        let node = self.node(id);
        let mut parts = Vec::with_capacity(node.attributes.len() + 1);
        for (k, v) in &node.attributes {
            if k == "style" {
                continue;
            }
            parts.push(match v {
                Some(v) => format!("{k}=\"{v}\""),
                None => k.clone(),
            });
        }
        if !node.style.is_empty() {
            parts.push(format!("style=\"{}\"", jsonize(&node.style)));
        }
        parts.join(" ")
    }

    fn inner_html(&mut self, id: WidgetId, changed: &mut Vec<(WidgetId, String)>) -> String {
        let children: Vec<Child> = self.node(id).children.values().cloned().collect();
        let mut html = String::new();
        for child in children {
            match child {
                Child::Text(text) => html.push_str(&text),
                Child::Widget(child) => html.push_str(&self.repr(child, changed)),
            }
        }
        html
    }

    /// Renders the node's HTML; nodes changed since their last rendering are collected (the topmost changed node
    /// of each branch).
    pub fn repr(&mut self, id: WidgetId, changed: &mut Vec<(WidgetId, String)>) -> String {
        let mut local = Vec::new();
        let inner = self.inner_html(id, &mut local);
        let is_changed = self.node(id).changed;
        if is_changed || !local.is_empty() {
            let node = self.node(id);
            let attributes = self.repr_attributes(id);
            let html = format!("<{tag} {attributes}>{inner}</{tag}>", tag = node.tag);
            self.node_mut(id).backup_repr = html;
        }
        if is_changed {
            let html = self.node(id).backup_repr.clone();
            changed.push((id, html));
            self.node_mut(id).changed = false;
        } else {
            changed.extend(local);
        }
        self.node(id).backup_repr.clone()
    }

    /// The GUI update: the update messages (`1`, the identifier, `,`, the HTML) of the changed widgets.
    pub fn gui_update(&mut self) -> Vec<String> {
        let mut messages = Vec::new();
        if let Some(root) = self.root {
            let mut changed = Vec::new();
            self.repr(root, &mut changed);
            for (id, html) in changed {
                messages.push(format!("1{},{}", self.identifier(id), client::to_websocket(&html)));
            }
        }
        self.need_update = false;
        messages
    }

    /// The root's HTML, every node marked as rendered (what a new browser receives).
    pub fn full_html(&mut self) -> String {
        match self.root {
            Some(root) => {
                let mut changed = Vec::new();
                self.repr(root, &mut changed)
            }
            None => String::new(),
        }
    }
}

/// The options of an event connection: whether its script stops the event's propagation and whether it
/// prevents the default action.
#[derive(Debug, Clone, Copy, Default)]
pub struct JsFlags {
    pub stop_propagation: bool,
    pub prevent_default: bool,
}

impl JsFlags {
    pub const NONE: JsFlags = JsFlags { stop_propagation: false, prevent_default: false };
}

/// The text of a float (shortest round-trip form, always with a fraction or exponent).
pub fn py_float_str(x: f64) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    let abs = x.abs();
    if abs != 0.0 && !(1e-4..1e16).contains(&abs) {
        // The exponent form is used outside [1e-4, 1e16).
        let s = format!("{x:e}");
        let (mantissa, exponent) = s.split_once('e').unwrap_or((&s, "0"));
        let exponent: i32 = exponent.parse().unwrap_or(0);
        let sign = if exponent < 0 { '-' } else { '+' };
        return format!("{mantissa}e{sign}{:02}", exponent.abs());
    }
    let s = format!("{x}");
    if s.contains('.') {
        s
    } else {
        format!("{s}.0")
    }
}

#[cfg(test)]
mod tests;
