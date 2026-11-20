use super::{decode, LegacyValue};

#[test]
fn text_int_preserves_native_base_detection_and_short_boolean_forms() {
    for (text, expected) in [
        ("012", LegacyValue::Int(10)),
        ("-012", LegacyValue::Int(-10)),
        ("0x2a", LegacyValue::Int(42)),
        ("0b101", LegacyValue::Int(5)),
        ("0o17", LegacyValue::Int(15)),
        ("1_000", LegacyValue::Int(1000)),
        ("2147483648", LegacyValue::Int(2147483648)),
        ("1 ", LegacyValue::Int(1)),
        ("+0", LegacyValue::Bool(false)),
        ("-0", LegacyValue::Bool(false)),
        (" 1", LegacyValue::Bool(true)),
        ("001", LegacyValue::Int(1)),
        ("42\0ignored", LegacyValue::Int(42)),
        ("\0", LegacyValue::Int(0)),
    ] {
        let data = format!("I{text}\n.");
        assert_eq!(decode(data.as_bytes()), Ok(expected), "{text:?}");
    }
}

#[test]
fn text_long_preserves_python_base_zero_instead_of_c_octal() {
    for (text, expected) in [
        ("0x2aL", 42),
        ("0x_2aL", 42),
        ("-0b101L", -5),
        ("0o17", 15),
        ("1_000L", 1000),
        ("000L", 0),
        (" -9223372036854775808L", i64::MIN),
        ("42\0ignoredL", 42),
    ] {
        let data = format!("L{text}\n.");
        assert_eq!(decode(data.as_bytes()), Ok(LegacyValue::Int(expected)), "{text:?}");
    }
    for text in ["012L", "09L", "3LL", "1__0L"] {
        assert!(decode(format!("L{text}\n.").as_bytes()).is_err(), "{text:?}");
    }
    for (text, base, shown) in [
        ("012L", 0, "'012'"),
        ("09L", 0, "'09'"),
        ("3LL", 10, "'3L'"),
        ("0xgL", 16, "'0xg'"),
        ("1__0", 10, "'1__0\\n'"),
    ] {
        let error = decode(format!("L{text}\n.").as_bytes()).unwrap_err();
        assert_eq!(
            error.native_exception(),
            Some(("ValueError", format!("invalid literal for int() with base {base}: {shown}")))
        );
    }
}

#[test]
fn text_integer_limits_apply_after_base_detection() {
    let decimal = "0".repeat(4301);
    let error = decode(format!("L{decimal}L\n.").as_bytes()).unwrap_err();
    assert_eq!(error.native_exception(), Some(("ValueError", "Exceeds the limit (4300 digits) for integer string conversion: value has 4301 digits; use sys.set_int_max_str_digits() to increase the limit".into())));
    assert_eq!(
        decode(format!("I{decimal}\n.").as_bytes()),
        Ok(LegacyValue::Int(0)),
        "strtol path has no decimal digit limit"
    );
    assert_eq!(
        decode(format!("L0x{}2aL\n.", "0".repeat(4301)).as_bytes()),
        Ok(LegacyValue::Int(42)),
        "power-of-two base has no decimal limit"
    );
    for text in ["9223372036854775808", "-9223372036854775809"] {
        assert_eq!(decode(format!("I{text}\n.").as_bytes()).unwrap().integer(), Some(text.parse().unwrap()));
    }
}

#[test]
fn selected_sqlite_row_decodes_text_int_without_touching_other_rows() {
    use crate::import::sqlitedict;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("profile.db");
    sqlitedict::set_item(&path, "unnamed", "good", &LegacyValue::Int(9)).unwrap();
    let connection = rusqlite::Connection::open(&path).unwrap();
    for (name, data) in [("octal", &b"I012\n."[..]), ("bad", &b"I09\n."[..])] {
        connection.execute("INSERT INTO unnamed VALUES (?1,?2)", rusqlite::params![name, data]).unwrap();
    }
    assert_eq!(sqlitedict::read_item(&path, "unnamed", "octal").unwrap(), Some(LegacyValue::Int(10)));
    assert_eq!(
        sqlitedict::read_item(&path, "unnamed", "bad").unwrap_err().native_exception("select_one"),
        Some(("ValueError", "could not convert string to int".into()))
    );
    assert_eq!(sqlitedict::read_item(&path, "unnamed", "good").unwrap(), Some(LegacyValue::Int(9)));
}

#[test]
fn text_float_keeps_prefix_errors_whitespace_and_overflow_boundaries() {
    for (text, class, message) in [
        (" 1.5", "ValueError", "could not convert string to float: ' 1.5\n'"),
        ("1.5 ", "ValueError", "could not convert string to float"),
        ("1_5", "ValueError", "could not convert string to float"),
        ("1e", "ValueError", "could not convert string to float"),
        ("1e999", "OverflowError", "value too large to convert to float: '1e999\n'"),
        ("1e999junk", "OverflowError", "value too large to convert to float: '1e999junk\n'"),
        ("invalid", "ValueError", "could not convert string to float: 'invalid\n'"),
    ] {
        let error = decode(format!("F{text}\n.").as_bytes()).unwrap_err();
        assert_eq!(error.native_exception(), Some((class, message.into())), "{text:?}");
    }
    for (text, expected) in [("-0.0", -0.0_f64), ("1.5\0ignored", 1.5), ("1e-999", 0.0), ("Infinity", f64::INFINITY)] {
        let LegacyValue::Float(value) = decode(format!("F{text}\n.").as_bytes()).unwrap() else { panic!("float") };
        assert_eq!(value.to_bits(), expected.to_bits(), "{text:?}");
    }
}

#[test]
fn invalid_text_int_keeps_original_exception() {
    for text in ["09", "invalid", "0002147483648", "1__0"] {
        let error = decode(format!("I{text}\n.").as_bytes()).unwrap_err();
        assert_eq!(
            error.native_exception(),
            Some(("ValueError", "could not convert string to int".into())),
            "{text:?}"
        );
    }
    assert_eq!(
        decode(b"I\n.").unwrap_err().native_exception(),
        Some(("UnpicklingError", "pickle data was truncated".into()))
    );
}

#[test]
fn long_text_errors_limit_the_rendered_repr_not_just_the_input() {
    let input = "\\".repeat(201);
    let expected = format!("'{}", "\\\\".repeat(100))[..200].to_owned();
    for bytes in [format!("L{input}\n."), format!("g{input}\n."), format!("Np{input}\n.")] {
        let error = decode(bytes.as_bytes()).unwrap_err();
        assert_eq!(
            error.native_exception(),
            Some(("ValueError", format!("invalid literal for int() with base 10: {expected}")))
        );
    }
}
