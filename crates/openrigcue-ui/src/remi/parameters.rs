//! Parsing of a callback's parameter packet: Unicode-character slices, integer grammar,
//! insertion order, unconditional separator skip and errors before invocation.

pub type Parameters = indexmap::IndexMap<String, String>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParameterError {
    ValueError(String),
    /// The parse cannot advance past this point; the parameters read so far are kept.
    NonProgress {
        parameters: Parameters,
    },
}
impl std::fmt::Display for ParameterError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ValueError(message) => write!(formatter, "ValueError: {message}"),
            Self::NonProgress { .. } => formatter.write_str("parameter parsing cannot advance"),
        }
    }
}
impl std::error::Error for ParameterError {}

/// The signature of the application's plain method that takes the browser's pixel ratio.
pub const PIXEL_RATIO: super::classes::Signature = super::classes::Signature {
    qualname: "MyApp.onpixelratio",
    params: &["pixel_ratio"],
    required: 1,
    varargs: false,
    varkw: false,
};
/// The signature of the application's plain method that takes a page error: its message, source, line,
/// column and error text.
pub const ON_ERROR: super::classes::Signature = super::classes::Signature {
    qualname: "App.onerror",
    params: &["message", "source", "lineno", "colno", "error"],
    required: 5,
    varargs: false,
    varkw: false,
};

/// Keyword binding before a BODY event (through its event connector), the `onpixelratio`
/// method or `onerror` executes.
pub fn validate_app_parameters(event: &str, parameters: &Parameters) -> Result<(), String> {
    let items = parameters.iter().map(|(key, value)| (key.as_str(), value.as_str()));
    match event {
        "onpixelratio" => super::classes::bind(&PIXEL_RATIO, false, items).map(|_| ()),
        "onerror" => super::classes::bind(&ON_ERROR, false, items).map(|_| ()),
        _ => match super::classes::event("BODY", event) {
            Some(signature) => super::classes::bind(signature, true, items).map(|_| ()),
            None => Ok(()),
        },
    }
}

pub fn parse_parameters_checked(text: &str) -> Result<Parameters, ParameterError> {
    fn drop_characters(text: &str, count: usize) -> &str {
        &text[text.char_indices().nth(count).map_or(text.len(), |(index, _)| index)..]
    }
    let mut out = Parameters::new();
    let mut rest = text;
    while rest.chars().count() > 1 && rest.contains('|') {
        let prefix = rest.split('|').next().expect("nonempty rest");
        let normalized = openrigcue_model::python_numeric::integer_text(prefix).ok_or_else(|| {
            let repr: String = openrigcue_model::python_repr::repr_str(prefix).chars().take(200).collect();
            ParameterError::ValueError(format!("invalid literal for int() with base 10: {repr}"))
        })?;
        let digits = normalized.trim_start_matches(['+', '-']);
        if digits.len() > 4300 {
            return Err(ParameterError::ValueError(format!("Exceeds the limit (4300 digits) for integer string conversion: value has {} digits; use sys.set_int_max_str_digits() to increase the limit",digits.len())));
        }
        if normalized.starts_with('-') || digits.bytes().all(|byte| byte == b'0') {
            return Err(ParameterError::NonProgress { parameters: out });
        }
        let length = digits
            .bytes()
            .fold(0usize, |value, digit| value.saturating_mul(10).saturating_add(usize::from(digit - b'0')));
        rest = drop_characters(rest, prefix.chars().count() + 1);
        let name = rest
            .split('|')
            .next()
            .expect("split returns one field")
            .split('=')
            .next()
            .expect("split returns one field");
        let value = rest.chars().take(length).skip(name.chars().count() + 1).collect();
        out.insert(name.into(), value);
        rest = drop_characters(rest, length.saturating_add(1));
    }
    Ok(out)
}
