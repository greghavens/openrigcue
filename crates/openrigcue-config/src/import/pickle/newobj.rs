//! Keyword-argument handling of the admitted builtin constructors. No arbitrary callable is run.
use super::*;

fn error(class: &'static str, message: impl Into<String>) -> PickleError {
    PickleError::Constructor { class, message: message.into() }
}
pub(super) fn construct(
    callable: AllowedGlobal,
    args: LegacyValue,
    kwargs: Option<&Dict>,
) -> Result<LegacyValue, PickleError> {
    let LegacyValue::Tuple(args) = args else {
        return Err(error("TypeError", "argument list must be a tuple"));
    };
    let nonempty = kwargs.filter(|v| !v.is_empty());
    if callable == AllowedGlobal::Str {
        return super::codecs::construct_str(&args, kwargs);
    }
    if callable == AllowedGlobal::DateTime {
        return datetime(&args, kwargs);
    }
    if callable == AllowedGlobal::Int {
        let count = args.len() + nonempty.map_or(0, Dict::len);
        if count > 2 {
            return Err(error(
                "TypeError",
                format!(
                    "int() takes at most 2 {}arguments ({count} given)",
                    if args.is_empty() { "keyword " } else { "" }
                ),
            ));
        }
        if let Some(kwargs) = nonempty {
            if args.len() > 1 && kwargs.contains("base") {
                return Err(error("TypeError", "argument for int() given by name ('base') and position (2)"));
            }
            if kwargs.len() != usize::from(kwargs.contains("base")) {
                return Err(PickleError::Native(super::codecs::unexpected_keyword_for(kwargs, &["base"], "int")));
            }
            if let Some(base) = kwargs.get("base") {
                if args.is_empty() {
                    return Err(error("TypeError", "int() missing string argument"));
                }
                return super::based_integer_constructor(&[args[0].clone(), base.clone()]);
            }
        }
    } else if let Some(_kwargs) = nonempty {
        let name = match callable {
            AllowedGlobal::Tuple => "tuple",
            AllowedGlobal::Float => "float",
            AllowedGlobal::Bool => "bool",
            _ => unreachable!("mutable container tp_new is handled by caller"),
        };
        return Err(error("TypeError", format!("{name}() takes no keyword arguments")));
    }
    super::construct(callable, LegacyValue::Tuple(args))
}

pub(super) fn c_int(value: &LegacyValue) -> Result<i32, PickleError> {
    let integer = value.integer().ok_or_else(|| {
        error("TypeError", format!("'{}' object cannot be interpreted as an integer", value.qualified_type_name()))
    })?;
    // The argument is first narrowed to a signed 32-bit integer, so an
    // overflow is reported there, before the range check.
    integer.to_i32().ok_or_else(|| error("OverflowError", "Python int too large to convert to C long"))
}
pub(super) fn datetime(args: &[LegacyValue], kwargs: Option<&Dict>) -> Result<LegacyValue, PickleError> {
    if (1..=2).contains(&args.len()) {
        let first = args[0].resolved();
        let state = match &first {
            LegacyValue::Bytes(v) => v.len() == 10 && (1..=12).contains(&(v[2] & 127)),
            v => v.python_string().is_some_and(|v| v.len() == 10 && (1..=12).contains(&(v.codepoints()[2] & 127))),
        };
        // The datetime constructor checks the binary state before looking at keywords.
        if state {
            return super::datetime_from_state(args);
        }
    }
    let kwargs = kwargs.cloned().unwrap_or_default();
    if args.len() + kwargs.len() > 9 {
        return Err(error(
            "TypeError",
            format!(
                "function takes at most 9 {}arguments ({} given)",
                if args.is_empty() { "keyword " } else { "" },
                args.len() + kwargs.len()
            ),
        ));
    }
    if args.len() > 8 {
        return Err(error(
            "TypeError",
            format!("function takes at most 8 positional arguments ({} given)", args.len()),
        ));
    }
    let names = ["year", "month", "day", "hour", "minute", "second", "microsecond", "tzinfo", "fold"];
    let mut parsed = Vec::new();
    for (index, name) in names.iter().enumerate() {
        let positional = args.get(index);
        let keyword = kwargs.get(name);
        if positional.is_some() && keyword.is_some() {
            return Err(error(
                "TypeError",
                format!("argument for function given by name ('{name}') and position ({})", index + 1),
            ));
        }
        let value = positional.or(keyword);
        if value.is_none() && index < 3 {
            return Err(error("TypeError", format!("function missing required argument '{name}' (pos {})", index + 1)));
        }
        let value = value.cloned().unwrap_or_else(|| if index == 7 { LegacyValue::None } else { LegacyValue::Int(0) });
        if index == 7 {
            parsed.push(value);
        } else {
            parsed.push(LegacyValue::Int(i64::from(c_int(&value)?)));
        }
    }
    for (key, _) in kwargs.iter() {
        let text = key.to_value().python_string().ok_or_else(|| error("TypeError", "keywords must be strings"))?;
        if !text.scalar_string().is_some_and(|text| names.contains(&text.as_str())) {
            return Err(super::named_constructor_error(
                "TypeError",
                "'",
                &text,
                "' is an invalid keyword argument for this function",
            ));
        }
    }
    let tzinfo = parsed[7].clone();
    parsed[7] = LegacyValue::None;
    let fold = c_int(&parsed[8])?;
    let mut value = super::datetime_from_calendar(&parsed[..8])?;
    if !(0..=1).contains(&fold) {
        return Err(error("ValueError", "fold must be either 0 or 1"));
    }
    if !matches!(tzinfo.resolved(), LegacyValue::None) {
        return Err(error(
            "TypeError",
            format!("tzinfo argument must be None or of a tzinfo subclass, not type '{}'", tzinfo.type_name()),
        ));
    }
    if let LegacyValue::DateTime(parts) = &mut value {
        parts.fold = fold != 0;
    }
    Ok(value)
}
