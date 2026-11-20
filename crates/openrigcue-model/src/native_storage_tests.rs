use crate::native_value::*;
use crate::python_compare::{native_equal_checked, numeric_cmp};
#[test]
fn integer_float_comparison_does_not_round_the_integer_first() {
    let boundary: BigInt = BigInt::from(1) << 200;
    let float = LegacyValue::Float(2_f64.powi(200));
    assert_eq!(
        numeric_cmp(&LegacyValue::from_integer(boundary.clone() + 1), &float).unwrap(),
        Some(std::cmp::Ordering::Greater)
    );
    assert_eq!(numeric_cmp(&LegacyValue::from_integer(boundary), &float).unwrap(), Some(std::cmp::Ordering::Equal));
    // IEEE754 bits0x7e37e43c8800759c:53-bit significand times2^944.
    assert_eq!(LegacyValue::Float(1e300).py_integer().unwrap(), BigInt::from(0x17e43c8800759c_u64) << 944);
}
#[test]
fn copied_and_removed_children_remain_live_roots() {
    let mut root = LegacyValue::Object(NativeObject::new(LegacyValue::Dict(Dict::new())));
    root.assign_item("child", LegacyValue::List(vec![1.into()])).unwrap();
    let mut copy = root.copy_builtin().unwrap();
    let mut child = root.remove_path(&["child"]).unwrap();
    drop(root);
    child.assign_index(0, 7.into()).unwrap();
    assert_eq!(copy.item_owned("child").unwrap().get_index(0).unwrap().integer(), Some(7.into()));
    copy.assign_item("another", 2.into()).unwrap();
}
#[test]
fn recursive_comparisons_and_reprs_preserve_builtin_results() {
    fn cycle() -> LegacyValue {
        let object = NativeObject::new(LegacyValue::List(Vec::new()));
        let root = LegacyValue::Object(object.clone());
        object.edit(|value| {
            if let LegacyValue::List(values) = value {
                values.push(root.clone());
            }
        });
        root
    }
    let a = cycle();
    let b = cycle();
    assert!(native_equal_checked(&a, &a).unwrap());
    assert!(native_equal_checked(&a, &b).unwrap_err().starts_with("RecursionError:"));
    assert_eq!(crate::python_repr::repr_checked(&a).unwrap(), "[[...]]");
    let nan = LegacyValue::from_float(f64::NAN);
    assert!(!native_equal_checked(&nan, &nan).unwrap());
    assert!(native_equal_checked(&LegacyValue::List(vec![nan.clone()]), &LegacyValue::List(vec![nan])).unwrap());
}
#[test]
fn builtin_buffer_conversion_and_assignment_order_are_retained() {
    for value in [
        LegacyValue::Bytes(b" 12 ".to_vec()),
        LegacyValue::ByteArray(b" 12 ".to_vec()),
        LegacyValue::MemoryView(NativeBufferView::new(LegacyValue::ByteArray(b" 12 ".to_vec()), true)),
    ] {
        assert_eq!(value.py_integer().unwrap(), BigInt::from(12));
        assert_eq!(value.py_float_native().unwrap(), 12.0);
    }
    let mut buffer = LegacyValue::Object(NativeObject::new(LegacyValue::ByteArray(vec![1])));
    assert!(buffer.assign_index(0, buffer.clone()).unwrap_err().starts_with("TypeError:"));
    assert!(buffer.assign_key("bad".into(), LegacyValue::None).unwrap_err().contains("bytearray indices"));
    assert!(buffer.assign_key(9.into(), LegacyValue::None).unwrap_err().contains("cannot be interpreted"));
    assert!(LegacyValue::Bytes(vec![255]).py_integer().unwrap_err().starts_with("ValueError:"));
}
#[test]
fn surrogate_text_survives_native_repr_and_json_serialization() {
    let text = PythonString::from_codepoints(vec![0xd800, 65]).unwrap();
    assert_eq!(crate::python_repr::str_native(&LegacyValue::Unicode(text.clone())).unwrap(), text);
    assert_eq!(serde_json::to_string(&text).unwrap(), "\"\\ud800A\"");
    assert_eq!(crate::python_repr::repr(&LegacyValue::Unicode(text)), "'\\ud800A'");
}

#[test]
fn wrappers_are_transparent_to_structural_snapshots_but_keep_python_kinds() {
    let wrapped = LegacyValue::Object(NativeObject::new("same".into()));
    assert_eq!(wrapped, LegacyValue::Str("same".into()));
    assert_ne!(LegacyValue::Object(NativeObject::new(LegacyValue::Bool(true))), LegacyValue::Int(1));
    assert_eq!(LegacyValue::BigInt(1.into()), LegacyValue::Int(1));
    let nan: LegacyValue = f64::NAN.into();
    assert!(nan.same_object(&nan.clone()));
    let mut dict = Dict::new();
    dict.insert_native(LegacyValue::Float(f64::NAN), 1.into()).unwrap();
    let value = LegacyValue::Dict(dict);
    let keys = value.dict_keys_owned().unwrap();
    let key = keys[0].to_value();
    assert_eq!(value.lookup_owned(&key).unwrap(), LegacyValue::Int(1));
}

#[test]
fn live_dictionary_assignment_retains_the_actual_string_key() {
    let key = LegacyValue::Object(NativeObject::new("key".into()));
    let mut value = LegacyValue::Object(NativeObject::new(LegacyValue::Dict(Dict::new())));
    value.assign_key(key.clone(), key.clone()).unwrap();
    let stored = value.dict_keys_owned().unwrap()[0].to_value();
    assert!(stored.same_object(&key));
    assert!(stored.same_object(&value.item_owned("key").unwrap()));
    assert!(matches!(value.resolved(), LegacyValue::Dict(v) if v.str_keys() == ["key"]));
}
