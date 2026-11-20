//! Tests of the constructor and shared-object value kinds the reader represents.
use super::*;
use openrigcue_model::python_compare::{native_equal_checked, numeric_cmp};

fn reduce(name: &str, args: Vec<LegacyValue>) -> Result<LegacyValue, PickleError> {
    construct(AllowedGlobal::resolve("builtins", name).unwrap(), LegacyValue::Tuple(args))
}
#[test]
fn unbounded_binary_integer_and_surrogate_roundtrip() {
    for number in [BigInt::from(1) << 130_usize, -(BigInt::from(1) << 200_usize), BigInt::from(10).pow(4300)] {
        let value = LegacyValue::from_integer(number.clone());
        assert!(native_equal_checked(&decode(&encode(&value)).unwrap(), &value).unwrap());
        assert_eq!(reduce("int", vec![value.clone()]).unwrap().integer(), Some(number));
    }
    let text = PythonString::from_codepoints(vec![65, 0xd800, 0x10ffff]).unwrap();
    let mut dict = Dict::new();
    dict.insert_native(LegacyValue::Unicode(text.clone()), LegacyValue::Unicode(text.clone())).unwrap();
    let decoded = decode(&encode(&dict.into())).unwrap();
    assert_eq!(decoded.lookup_owned(&LegacyValue::Unicode(text.clone())).unwrap().python_string(), Some(text));
    assert!(crate::import::de::native_to_python_json(&decoded).unwrap().contains("\\ud800"));
}
#[test]
fn aliases_cycles_and_nan_membership_survive_real_sqlite() {
    let mut shared = LegacyValue::Object(NativeObject::new(LegacyValue::List(vec![LegacyValue::Int(1)])));
    let root = LegacyValue::List(vec![shared.clone(), shared.clone()]);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("profile.db");
    crate::import::sqlitedict::set_item(&path, "unnamed", "aliases", &root).unwrap();
    let decoded = crate::import::sqlitedict::read_item(&path, "unnamed", "aliases").unwrap().unwrap();
    let mut first = decoded.get_index(0).unwrap();
    let second = decoded.get_index(1).unwrap();
    assert!(first.same_object(&second));
    first.assign_index(0, LegacyValue::Int(8)).unwrap();
    assert_eq!(second.get_index(0).unwrap().integer(), Some(8.into()));
    if let LegacyValue::Object(object) = &mut shared {
        let own = object.clone();
        object.edit(|value| {
            if let LegacyValue::List(values) = value {
                values.push(LegacyValue::Object(own));
            }
        });
    }
    let decoded = decode(&encode(&shared)).unwrap();
    assert!(decoded.same_object(&decoded.get_index(1).unwrap()));
    assert_eq!(crate::import::de::native_to_python_json(&decoded).unwrap_err().message, "Circular reference detected");
    let nan = LegacyValue::from_float(f64::NAN);
    let mut keys = Dict::new();
    keys.insert_native(nan.clone(), 3.into()).unwrap();
    assert_eq!(LegacyValue::Dict(keys).lookup_owned(&nan).unwrap().integer(), Some(3.into()));
    assert!(!native_equal_checked(&nan, &nan).unwrap());
}
#[test]
fn a_recursive_tuple_uses_the_actual_memo_allocation() {
    let list = NativeObject::new(LegacyValue::List(Vec::new()));
    let tuple = LegacyValue::Object(NativeObject::new(LegacyValue::Tuple(vec![LegacyValue::Object(list.clone())])));
    list.edit(|value| {
        if let LegacyValue::List(values) = value {
            values.push(tuple.clone());
        }
    });
    let read = decode(&encode(&tuple)).unwrap();
    assert!(read.same_object(&read.get_index(0).unwrap().get_index(0).unwrap()));
}
#[test]
fn protocol5_frame_memo_and_small_container_opcodes_are_source_shaped() {
    assert_eq!(encode(&LegacyValue::Int(1)), b"\x80\x05K\x01.");
    assert_eq!(encode(&LegacyValue::Str("x".into())), b"\x80\x05\x95\x05\0\0\0\0\0\0\0\x8c\x01x\x94.");
    assert_eq!(encode(&LegacyValue::List(vec![1.into()])), b"\x80\x05\x95\x06\0\0\0\0\0\0\0]\x94K\x01a.");
    assert_eq!(
        encode(&LegacyValue::Tuple(vec![1.into(), 2.into()])),
        b"\x80\x05\x95\x07\0\0\0\0\0\0\0K\x01K\x02\x86\x94."
    );
    let array = LegacyValue::ByteArray(b"AB".to_vec());
    assert_eq!(encode(&array), b"\x80\x05\x95\x0d\0\0\0\0\0\0\0\x96\x02\0\0\0\0\0\0\0AB\x94.");
    assert_eq!(decode(&encode(&array)).unwrap(), array);
    let big = LegacyValue::Bytes(vec![42; 65536]);
    let bytes = encode(&big);
    assert_eq!(&bytes[..7], b"\x80\x05B\0\0\x01\0");
    assert_eq!(&bytes[bytes.len() - 2..], b"\x94.");
    assert_eq!(decode(&bytes).unwrap(), big);
}
#[test]
fn native_key_objects_keep_identity_when_reused_as_values() {
    // EMPTY_DICT/MEMOIZE; one interned key used both as key and value.
    let value = decode(b"\x80\x04}\x94\x8c\x03key\x94h\x01s.").unwrap();
    let encoded = encode(&value);
    assert!(encoded.windows(3).any(|v| v == b"h\x01s"));
    assert!(native_equal_checked(&decode(&encoded).unwrap(), &value).unwrap());
}
#[test]
fn bytearray_readonly_exports_are_live_and_prevent_resizing() {
    let mut data = b"\x80\x05(\x96\x02\0\0\0\0\0\0\0AB\x94\x98h\x00t.".to_vec();
    let result = decode(&data).unwrap();
    let view = result.get_index(0).unwrap();
    let mut buffer = result.get_index(1).unwrap();
    buffer.assign_index(0, 90.into()).unwrap();
    assert_eq!(view.buffer_bytes().unwrap(), b"ZB");
    assert!(buffer.extend_bytearray(&[65.into()]).unwrap_err().starts_with("BufferError:"));
    assert!(encode_checked(&result).unwrap_err().to_string().contains("cannot pickle memoryview"));
    data = b"\x80\x05\x96\x01\0\0\0\0\0\0\0A.".to_vec();
    assert_eq!(decode(&data).unwrap().buffer_bytes().unwrap(), b"A");
    for value in [LegacyValue::Bytes(b"12".to_vec()), LegacyValue::ByteArray(b"12".to_vec()), view] {
        let integer = reduce("int", vec![value.clone()]);
        if value.type_name() == "memoryview" {
            assert!(integer.is_err());
        } else {
            assert_eq!(integer.unwrap().integer(), Some(12.into()));
        }
    }
}
#[test]
fn scalar_keyword_datetime_and_output_digit_limits_keep_native_errors() {
    let huge = LegacyValue::from_integer(BigInt::from(10).pow(4300));
    assert_eq!(reduce("str", vec![huge]).unwrap_err().native().unwrap().class(), "ValueError");
    assert_eq!(reduce("int", vec![LegacyValue::Bytes(vec![255])]).unwrap_err().native().unwrap().class(), "ValueError");
    let mut kwargs = Dict::new();
    kwargs.insert("bas", 16.into());
    let failure =
        newobj::construct(AllowedGlobal::Int, LegacyValue::Tuple(vec!["ff".into()]), Some(&kwargs)).unwrap_err();
    assert_eq!(
        failure.native().unwrap().message().scalar_string().unwrap(),
        "int() got an unexpected keyword argument 'bas'. Did you mean 'base'?"
    );
    kwargs = Dict::new();
    for (k, v) in [("year", 2024), ("month", 2), ("day", 29), ("fold", 1)] {
        kwargs.insert(k, v.into());
    }
    let date = newobj::construct(AllowedGlobal::DateTime, LegacyValue::Tuple(Vec::new()), Some(&kwargs)).unwrap();
    assert!(matches!(date, LegacyValue::DateTime(DateTimeParts { fold: true, .. })));
    assert_eq!(
        numeric_cmp(
            &date,
            &LegacyValue::DateTime(DateTimeParts {
                fold: false,
                ..if let LegacyValue::DateTime(v) = date { v } else { unreachable!() }
            })
        )
        .unwrap(),
        Some(std::cmp::Ordering::Equal)
    );
    let invalid = newobj::construct(
        AllowedGlobal::Int,
        LegacyValue::Tuple(vec!["10".into(), LegacyValue::from_integer(BigInt::from(1) << 100)]),
        None,
    )
    .unwrap_err();
    assert_eq!(invalid.native().unwrap().class(), "ValueError");
}

#[test]
fn admitted_class_data_retains_unbound_builtin_descriptor_errors() {
    let failure = decode(b"cbuiltins\nset\n(K\x01\x90.").unwrap_err();
    assert_eq!(
        failure.native_exception(),
        Some(("TypeError", "descriptor 'add' for 'set' objects doesn't apply to a 'int' object".into()))
    );
}

#[test]
fn build_slot_descriptor_errors_keep_builtin_and_calendar_type_names() {
    for (target, name, message) in [
        (LegacyValue::Int(1), "__getnewargs__", "'int' object attribute '__getnewargs__' is read-only"),
        (LegacyValue::Float(1.0), "__rdivmod__", "'float' object attribute '__rdivmod__' is read-only"),
        (LegacyValue::List(vec![]), "__iadd__", "'list' object attribute '__iadd__' is read-only"),
        (LegacyValue::None, "__bool__", "'NoneType' object attribute '__bool__' is read-only"),
        (
            LegacyValue::DateTime(DateTimeParts {
                year: 2024,
                month: 1,
                day: 1,
                hour: 0,
                minute: 0,
                second: 0,
                microsecond: 0,
                fold: false,
            }),
            "__replace__",
            "'datetime.datetime' object attribute '__replace__' is read-only",
        ),
    ] {
        let mut slots = Dict::new();
        slots.insert(name, LegacyValue::None);
        assert_eq!(
            build_slots(&Item::Value(target), &slots).unwrap_err().native_exception(),
            Some(("AttributeError", message.into()))
        );
    }
}
