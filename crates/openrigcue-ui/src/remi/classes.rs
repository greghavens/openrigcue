//! The event methods of the widgets the browser sends events to: the page body's events and the interface's one
//! action receiver.
//!
//! The websocket handler resolves the named method of the addressed widget and calls it with the parsed
//! parameters as keyword arguments only. For an event method that call goes through the event's connector, which
//! forwards the keywords to the method. The binding errors below are: an unexpected keyword argument (with a
//! "Did you mean" variant), multiple values for an argument, and missing required arguments.

/// An event method's signature: its qualified name, its parameters, and whether it takes extra positional or keyword arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Signature {
    pub qualname: &'static str,
    pub params: &'static [&'static str],
    /// The number of leading parameters without a default value. Parameters at or beyond this index are
    /// optional; `bind` does not require them and marks them not-provided in [`Bound::provided`].
    pub required: usize,
    pub varargs: bool,
    pub varkw: bool,
}

const fn sig(qualname: &'static str, params: &'static [&'static str]) -> Signature {
    Signature { qualname, params, required: params.len(), varargs: false, varkw: false }
}

/// The event methods: the class that defines each, its name and its signature.
const EVENTS: &[(&str, &str, Signature)] = &[
    ("BODY", "onload", sig("BODY.onload", &[])),
    ("BODY", "ononline", sig("BODY.ononline", &[])),
    ("BODY", "onpagehide", sig("BODY.onpagehide", &[])),
    ("BODY", "onpageshow", sig("BODY.onpageshow", &["width", "height"])),
    ("BODY", "onresize", sig("BODY.onresize", &["width", "height"])),
    // Every action of the interface: its name, an argument and a value (both optional).
    (
        "OrcAction",
        "onact",
        Signature {
            qualname: "OrcAction.onact",
            params: &["act", "arg", "value"],
            required: 1,
            varargs: false,
            varkw: false,
        },
    ),
];

/// The event method `name` of a widget class, `None` when the class has no such event.
pub fn event(class: &str, name: &str) -> Option<&'static Signature> {
    EVENTS.iter().find(|(defined_by, event, _)| *defined_by == class && *event == name).map(|(_, _, s)| s)
}

/// Keyword arguments bound to an event method.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bound {
    /// The declared parameters' values, in declaration order. Omitted optional parameters hold an empty
    /// placeholder; [`Bound::provided`] says which were actually passed.
    pub positional: Vec<String>,
    /// Whether each declared parameter was passed (vs left at its default).
    pub provided: Vec<bool>,
    /// The extra keyword arguments of a method that takes them, in call order.
    pub kwargs: indexmap::IndexMap<String, String>,
}

/// Binds a keyword-only call to a method's signature.
/// `connector` puts the event connector in front: it takes every keyword except
/// one named `self`.
pub fn bind<'a>(
    signature: &Signature,
    connector: bool,
    parameters: impl IntoIterator<Item = (&'a str, &'a str)> + Clone,
) -> Result<Bound, String> {
    if connector && parameters.clone().into_iter().any(|(key, _)| key == "self") {
        return Err("TypeError: ClassEventConnector.__call__() got multiple values for argument 'self'".into());
    }
    let mut slots: Vec<Option<String>> = vec![None; signature.params.len()];
    let mut kwargs = indexmap::IndexMap::new();
    for (key, value) in parameters {
        if key == "self" {
            return Err(format!("TypeError: {}() got multiple values for argument 'self'", signature.qualname));
        }
        if let Some(index) = signature.params.iter().position(|name| *name == key) {
            slots[index] = Some(value.to_owned());
        } else if signature.varkw {
            kwargs.insert(key.to_owned(), value.to_owned());
        } else {
            return Err(unexpected_keyword(signature.qualname, signature.params, key));
        }
    }
    // Only the leading `required` parameters must be passed; the rest default.
    let missing: Vec<&str> = signature.params[..signature.required]
        .iter()
        .zip(&slots)
        .filter(|(_, slot)| slot.is_none())
        .map(|(name, _)| *name)
        .collect();
    if !missing.is_empty() {
        return Err(missing_positional(signature.qualname, &missing));
    }
    let provided = slots.iter().map(Option::is_some).collect();
    Ok(Bound { positional: slots.into_iter().map(Option::unwrap_or_default).collect(), provided, kwargs })
}

/// The "got an unexpected keyword argument" error, with a suggestion from the parameter names
/// (`self` first).
pub fn unexpected_keyword(qualname: &str, params: &[&str], key: &str) -> String {
    let candidates: Vec<&str> = std::iter::once("self").chain(params.iter().copied()).collect();
    match suggestion(&candidates, key) {
        Some(suggested) => {
            format!("TypeError: {qualname}() got an unexpected keyword argument '{key}'. Did you mean '{suggested}'?")
        }
        None => format!("TypeError: {qualname}() got an unexpected keyword argument '{key}'"),
    }
}

/// The "missing required positional arguments" error text.
pub fn missing_positional(qualname: &str, missing: &[&str]) -> String {
    let quoted: Vec<String> = missing.iter().map(|name| format!("'{name}'")).collect();
    let names = match quoted.as_slice() {
        [one] => one.clone(),
        [first, second] => format!("{first} and {second}"),
        [rest @ .., before_last, last] => format!("{}, {before_last}, and {last}", rest.join(", ")),
        [] => String::new(),
    };
    format!(
        "TypeError: {qualname}() missing {} required positional argument{}: {names}",
        missing.len(),
        if missing.len() == 1 { "" } else { "s" }
    )
}

const MAX_CANDIDATE_ITEMS: usize = 750;
const MAX_STRING_SIZE: usize = 40;
const MOVE_COST: usize = 2;
const CASE_COST: usize = 1;

fn substitution_cost(a: u8, b: u8) -> usize {
    if (a & 31) != (b & 31) {
        return MOVE_COST;
    }
    if a == b {
        return 0;
    }
    // Only `A`-`Z` are folded.
    let fold = |c: u8| if c.is_ascii_uppercase() { c + (b'a' - b'A') } else { c };
    if fold(a) == fold(b) {
        CASE_COST
    } else {
        MOVE_COST
    }
}

/// The weighted edit distance over the UTF-8 bytes.
fn levenshtein<'a>(mut a: &'a [u8], mut b: &'a [u8], max_cost: usize) -> usize {
    if a == b {
        return 0;
    }
    while let (Some(x), Some(y)) = (a.first(), b.first()) {
        if x != y {
            break;
        }
        a = &a[1..];
        b = &b[1..];
    }
    while let (Some(x), Some(y)) = (a.last(), b.last()) {
        if x != y {
            break;
        }
        a = &a[..a.len() - 1];
        b = &b[..b.len() - 1];
    }
    if a.is_empty() || b.is_empty() {
        return (a.len() + b.len()) * MOVE_COST;
    }
    if a.len() > MAX_STRING_SIZE || b.len() > MAX_STRING_SIZE {
        return max_cost.saturating_add(1);
    }
    if b.len() < a.len() {
        std::mem::swap(&mut a, &mut b);
    }
    if (b.len() - a.len()) * MOVE_COST > max_cost {
        return max_cost.saturating_add(1);
    }
    let mut buffer: Vec<usize> = (1..=a.len()).map(|i| i * MOVE_COST).collect();
    let mut result = 0;
    for (b_index, code) in b.iter().enumerate() {
        let mut distance = b_index * MOVE_COST;
        result = distance;
        let mut minimum = usize::MAX;
        for (index, slot) in buffer.iter_mut().enumerate() {
            let substitute = distance + substitution_cost(*code, a[index]);
            distance = *slot;
            let insert_delete = result.min(distance) + MOVE_COST;
            result = insert_delete.min(substitute);
            *slot = result;
            minimum = minimum.min(result);
        }
        if minimum > max_cost {
            return max_cost.saturating_add(1);
        }
    }
    result
}

/// The closest candidate to `name`, if any is close enough to suggest.
pub fn suggestion<'a>(candidates: &[&'a str], name: &str) -> Option<&'a str> {
    if candidates.len() >= MAX_CANDIDATE_ITEMS {
        return None;
    }
    let mut best: Option<(&'a str, isize)> = None;
    for item in candidates {
        if *item == name {
            continue;
        }
        let max_distance = ((name.len() + item.len() + 3) * MOVE_COST / 6) as isize;
        let max_distance = match best {
            Some((_, distance)) => max_distance.min(distance - 1),
            None => max_distance,
        };
        if max_distance < 0 {
            continue;
        }
        let distance = levenshtein(name.as_bytes(), item.as_bytes(), max_distance as usize) as isize;
        if distance > max_distance {
            continue;
        }
        if best.is_none_or(|(_, current)| distance < current) {
            best = Some((item, distance));
        }
    }
    best.map(|(item, _)| item)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl IntoIterator<Item = (&'a str, &'a str)> + Clone + 'a {
        pairs.iter().copied()
    }

    #[test]
    fn events_are_found_by_class_and_name() {
        for (class, _, signature) in EVENTS {
            assert!(signature.qualname.starts_with(&format!("{class}.")), "{class}");
        }
        assert_eq!(event("BODY", "onresize").unwrap().qualname, "BODY.onresize");
        assert_eq!(event("OrcAction", "onact").unwrap().qualname, "OrcAction.onact");
        assert_eq!(event("OrcRegion", "onact"), None);
        assert_eq!(event("BODY", "onclick"), None);
    }

    #[test]
    fn keyword_binding_reports_missing_unexpected_and_duplicate_arguments() {
        let act = event("OrcAction", "onact").unwrap();
        assert_eq!(
            bind(act, true, items(&[])).unwrap_err(),
            "TypeError: OrcAction.onact() missing 1 required positional argument: 'act'"
        );
        let bound = bind(act, true, items(&[("value", "3"), ("act", "set")])).unwrap();
        assert_eq!(bound.positional, vec!["set".to_owned(), String::new(), "3".to_owned()]);
        assert_eq!(bound.provided, vec![true, false, true]);
        assert_eq!(
            bind(act, true, items(&[("act", "nav"), ("self", "s")])).unwrap_err(),
            "TypeError: ClassEventConnector.__call__() got multiple values for argument 'self'"
        );
        assert_eq!(
            bind(act, true, items(&[("act", "nav"), ("ag", "x")])).unwrap_err(),
            "TypeError: OrcAction.onact() got an unexpected keyword argument 'ag'. Did you mean 'arg'?"
        );
        let resize = event("BODY", "onresize").unwrap();
        assert_eq!(
            bind(resize, true, items(&[])).unwrap_err(),
            "TypeError: BODY.onresize() missing 2 required positional arguments: 'width' and 'height'"
        );
        assert_eq!(
            bind(resize, true, items(&[("widht", "1")])).unwrap_err(),
            "TypeError: BODY.onresize() got an unexpected keyword argument 'widht'. Did you mean 'width'?"
        );
        assert_eq!(
            bind(resize, true, items(&[("extra", "1")])).unwrap_err(),
            "TypeError: BODY.onresize() got an unexpected keyword argument 'extra'"
        );
        assert_eq!(
            bind(resize, true, items(&[("height", "1"), ("Width", "2")])).unwrap_err(),
            "TypeError: BODY.onresize() got an unexpected keyword argument 'Width'. Did you mean 'width'?"
        );
        let direct = Signature {
            qualname: "MyApp.onpixelratio",
            params: &["pixel_ratio"],
            required: 1,
            varargs: false,
            varkw: false,
        };
        assert_eq!(
            bind(&direct, false, items(&[("self", "1")])).unwrap_err(),
            "TypeError: MyApp.onpixelratio() got multiple values for argument 'self'"
        );
        assert_eq!(
            bind(&direct, false, items(&[("pixel", "1")])).unwrap_err(),
            "TypeError: MyApp.onpixelratio() got an unexpected keyword argument 'pixel'"
        );
        assert_eq!(
            bind(&direct, false, items(&[("pixelratio", "1")])).unwrap_err(),
            "TypeError: MyApp.onpixelratio() got an unexpected keyword argument 'pixelratio'. Did you mean 'pixel_ratio'?"
        );
    }

    #[test]
    fn suggestions_follow_the_distance_limits() {
        assert_eq!(suggestion(&["self", "value1"], "value"), Some("value1"));
        assert_eq!(suggestion(&["self", "width", "height"], "pixel_ratio"), None);
        assert_eq!(suggestion(&["self", "width", "height"], "emitter"), None);
        assert_eq!(suggestion(&["self", "width", "height"], "🚗"), None);
        assert_eq!(suggestion(&["self", "x", "y"], "z"), None);
        assert_eq!(suggestion(&["self", "x", "y"], "X"), Some("x"));
        assert_eq!(suggestion(&["self", "new_value"], "value"), None);
        assert_eq!(suggestion(&["self", "new_value"], "new_valu"), Some("new_value"));
        assert_eq!(suggestion(&["self", "emitter", "percent_value"], "percent"), None);
        assert_eq!(suggestion(&["self", "emitter", "percent_value"], "percentvalue"), Some("percent_value"));
    }
}
