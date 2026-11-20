//! The event side of the widget tree: the application trait, the websocket dispatch of the body's events and
//! the interface's actions, and the listeners they reach.

use std::collections::HashMap;
use std::sync::Arc;

use openrigcue_model::native_value::LegacyValue;
use serde_json::Value;

use super::{Gui, Listener, WidgetId};

/// The event script of a click and of the other events without parameters.
pub const JS_CALLBACK: &str = "remi.sendCallback('%(emitter_identifier)s','%(event_name)s');";

/// An application owning its [`Gui`].
pub trait HasGui: Sized + 'static {
    fn gui(&mut self) -> &mut Gui<Self>;

    /// The body of an event method an application class defines, for a call through the websocket: `None`
    /// passes the event's string parameters on unchanged. The result is the method's return value as the event
    /// connector passes it on (empty: only the userdata).
    fn class_event(
        &mut self,
        _id: WidgetId,
        _signature: &'static super::classes::Signature,
        _bound: &super::classes::Bound,
    ) -> Option<Result<Vec<LegacyValue>, String>> {
        None
    }

    /// A method of the application itself, which the page script calls on the application identifier
    /// (`onpixelratio`, `onerror`): `None` when the application has no such method.
    fn app_method(&mut self, _id: WidgetId, _name: &str, _parameters: &[(&str, &str)]) -> Option<Result<(), String>> {
        None
    }
}

/// Convenience adapter for in-process event fixtures. The socket boundary uses
/// parse_parameters_checked and retains parse errors before invoking a method.
pub fn parse_parameters(text: &str) -> HashMap<String, String> {
    super::parameters::parse_parameters_checked(text).unwrap_or_default().into_iter().collect()
}

/// A websocket callback: the named event method of the addressed widget, called with keyword parameters. A name
/// that is no event method of the widget's class (nor, on the body, a method of the application) calls nothing;
/// an event method binds the keywords, runs its body (the application's [`HasGui::class_event`], else the
/// parameters are passed on), and then the connected listener.
pub fn dispatch_wire<'a, A: HasGui>(
    app: &mut A,
    id: WidgetId,
    event: &str,
    parameters: impl IntoIterator<Item = (&'a str, &'a str)> + Clone,
) -> Result<(), String> {
    let class = app.gui().node(id).py_type.clone();
    let Some(signature) = super::classes::event(&class, event) else {
        if class == "BODY" {
            let parameters: Vec<_> = parameters.into_iter().collect();
            if let Some(result) = app.app_method(id, event, &parameters) {
                return result;
            }
        }
        return Ok(());
    };
    let bound = super::classes::bind(signature, true, parameters)?;
    let args = match app.class_event(id, signature, &bound) {
        Some(result) => result?,
        None => bound.positional.iter().cloned().map(LegacyValue::Str).collect(),
    };
    emit_native(app, id, event, &args);
    Ok(())
}

/// In-process event fixtures: [`dispatch_wire`] with the parameters in key order.
pub fn dispatch<A: HasGui>(app: &mut A, id: WidgetId, event: &str, params: &HashMap<String, String>) {
    let _ = dispatch_checked(app, id, event, params);
}

pub fn dispatch_checked<A: HasGui>(
    app: &mut A,
    id: WidgetId,
    event: &str,
    params: &HashMap<String, String>,
) -> Result<(), String> {
    let mut ordered: Vec<(&str, &str)> = params.iter().map(|(key, value)| (key.as_str(), value.as_str())).collect();
    ordered.sort_unstable();
    dispatch_wire(app, id, event, ordered)
}

/// Calls the listener connected to `id`'s `event` with `args` (the event method already ran).
pub fn emit<A: HasGui>(app: &mut A, id: WidgetId, event: &str, args: &[Value]) {
    if let Some(listener) = app.gui().listener(id, event) {
        match listener {
            Listener::Json(callback) => callback(app, id, args),
            Listener::Native(callback) => {
                callback(app, id, &args.iter().cloned().map(Into::into).collect::<Vec<_>>());
            }
        }
    }
}

/// Calls the listener with stored values, without a JSON conversion changing NaN or infinity to None.
pub fn emit_native<A: HasGui>(app: &mut A, id: WidgetId, event: &str, args: &[LegacyValue]) {
    if let Some(listener) = app.gui().listener(id, event) {
        match listener {
            Listener::Native(callback) => callback(app, id, args),
            Listener::Json(callback) => {
                let json: Vec<_> = args
                    .iter()
                    .map(|value| openrigcue_config::import::de::from_legacy::<Value>(value).unwrap_or(Value::Null))
                    .collect();
                callback(app, id, &json);
            }
        }
    }
}

/// A listener from a closure.
pub fn cb<A, F>(f: F) -> Option<Listener<A>>
where
    F: Fn(&mut A, WidgetId, &[Value]) + Send + Sync + 'static,
{
    Some(Listener::Json(Arc::new(f)))
}

pub fn cb_native<A, F>(f: F) -> Option<Listener<A>>
where
    F: Fn(&mut A, WidgetId, &[LegacyValue]) + Send + Sync + 'static,
{
    Some(Listener::Native(Arc::new(f)))
}
