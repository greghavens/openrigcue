use super::{decode, LegacyValue};

fn assert_native_error(bytes: &[u8], message: &str) {
    assert_eq!(decode(bytes).unwrap_err().native_exception(), Some(("UnpicklingError", message.into())), "{bytes:?}");
}

#[test]
fn stack_operations_distinguish_empty_stack_from_mark_fence() {
    for bytes in [&b"."[..], b"0N.", b"2N.", b"q\0N.", b"r\0\0\0\0N.", b"\x94N.", b"\x85."] {
        assert_native_error(bytes, "unpickling stack underflow");
    }
    for bytes in [&b"N(."[..], b"(2l.", b"(q\0N.", b"(r\0\0\0\0N.", b"(\x94N.", b"(\x85."] {
        assert_native_error(bytes, "unexpected MARK found");
    }
}

#[test]
fn missing_container_mark_retains_native_error() {
    for bytes in [&b"1N."[..], b"Nl.", b"Nd.", b"Nt.", b"N\x91."] {
        assert_native_error(bytes, "could not find MARK");
    }
}

#[test]
fn pop_can_remove_a_mark_and_restore_the_enclosing_stack() {
    assert_eq!(decode(b"N(0."), Ok(LegacyValue::None));
    assert_eq!(decode(b"N(1."), Ok(LegacyValue::None));
    assert_eq!(decode(b"N((0t."), Ok(LegacyValue::Tuple(vec![])));
    assert_eq!(decode(b"(N2t."), Ok(LegacyValue::Tuple(vec![LegacyValue::None, LegacyValue::None])));
    assert_eq!(decode(b"N2\x86."), Ok(LegacyValue::Tuple(vec![LegacyValue::None, LegacyValue::None])));
}

#[test]
fn stored_stack_error_retains_native_identity_and_other_rows() {
    use crate::import::sqlitedict;

    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("profile.db");
    sqlitedict::set_item(&database, "unnamed", "good", &LegacyValue::Int(12)).unwrap();
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute("INSERT INTO unnamed(key,value) VALUES (?1,?2)", rusqlite::params!["bad", b"(2l.".as_slice()])
        .unwrap();
    let error = sqlitedict::read_item(&database, "unnamed", "bad").unwrap_err();
    assert_eq!(error.native_exception("select_one"), Some(("UnpicklingError", "unexpected MARK found".into())));
    assert_eq!(sqlitedict::read_item(&database, "unnamed", "good").unwrap(), Some(LegacyValue::Int(12)));
}

#[test]
fn stack_global_pops_both_operands_before_type_validation() {
    for bytes in [&b"N\x93."[..], b"\x8c\x01x\x93."] {
        assert_native_error(bytes, "unpickling stack underflow");
    }
    for bytes in [&b"NN\x93."[..], b"\x8c\x08builtinsN\x93.", b"N\x8c\x03int\x93."] {
        assert_native_error(bytes, "STACK_GLOBAL requires str");
    }
}

#[test]
fn empty_marked_mutations_are_noops_for_existing_targets() {
    for bytes in [&b"N(e."[..], b"N(u.", b"N(\x90."] {
        assert_eq!(decode(bytes), Ok(LegacyValue::None));
    }
    for bytes in [&b"(e."[..], b"(u.", b"(\x90."] {
        assert_native_error(bytes, "unpickling stack underflow");
    }
}

#[test]
fn marked_mutation_errors_keep_native_target_and_pair_checks() {
    for (bytes, class, message) in [
        (&b"NNa."[..], "AttributeError", "'NoneType' object has no attribute 'append'"),
        (&b"N(Ne."[..], "AttributeError", "'NoneType' object has no attribute 'append'"),
        (&b"N(N\x90."[..], "AttributeError", "'NoneType' object has no attribute 'add'"),
        (&b"}(Nu."[..], "UnpicklingError", "odd number of items for SETITEMS"),
        (&b"(Nd."[..], "UnpicklingError", "odd number of items for DICT"),
    ] {
        assert_eq!(decode(bytes).unwrap_err().native_exception(), Some((class, message.into())));
    }
}

#[test]
fn external_pickle_references_retain_native_refusal_order() {
    let missing = "A load persistent id instruction was encountered, but no persistent_load function was specified.";
    for bytes in [&b"Pexternal\n."[..], b"P\n.", b"NQ."] {
        assert_native_error(bytes, missing);
    }
    assert_native_error(b"P\xff\n.", "persistent IDs in protocol 0 must be ASCII strings");
    assert_native_error(b"Q.", "unpickling stack underflow");
    assert_native_error(b"(Q.", "unexpected MARK found");
    assert_native_error(
        b"\x80\x05\x97.",
        "pickle stream refers to out-of-band data but no *buffers* argument was given",
    );
}

#[test]
fn global_lines_use_strict_utf8_and_require_names_before_lookup() {
    for bytes in [&b"c\nint\n."[..], b"cbuiltins\n\n."] {
        assert_eq!(
            decode(bytes).unwrap_err().native_exception(),
            Some(("UnpicklingError", "pickle data was truncated".into()))
        );
    }
    for bytes in [&b"c\xff\nint\n."[..], b"cbuiltins\n\xff\n."] {
        assert_eq!(
            decode(bytes).unwrap_err().native_exception(),
            Some((
                "UnicodeDecodeError",
                "'utf-8' codec can't decode byte 0xff in position 0: invalid start byte".into()
            ))
        );
    }
    assert_eq!(
        decode(b"c\xed\xa0\x80\nint\n.").unwrap_err().native_exception(),
        Some((
            "UnicodeDecodeError",
            "'utf-8' codec can't decode byte 0xed in position 0: invalid continuation byte".into()
        ))
    );
}

#[test]
fn reduce_pops_callable_before_checking_argument_tuple_and_callability() {
    for bytes in [&b"NR."[..], b"(NR."] {
        assert_native_error(
            bytes,
            if bytes[0] == b'(' { "unexpected MARK found" } else { "unpickling stack underflow" },
        );
    }
    for (bytes, message) in [
        (&b"NNR."[..], "argument list must be a tuple"),
        (&b"cbuiltins\nint\nNR."[..], "argument list must be a tuple"),
        (&b"N)R."[..], "'NoneType' object is not callable"),
        (&b"K\x01)R."[..], "'int' object is not callable"),
    ] {
        assert_eq!(decode(bytes).unwrap_err().native_exception(), Some(("TypeError", message.into())));
    }
}

#[test]
fn readonly_buffer_preserves_bytes_and_native_type_errors() {
    assert_eq!(decode(b"\x80\x05C\x03abc\x98."), Ok(LegacyValue::Bytes(b"abc".to_vec())));
    assert_native_error(b"\x98.", "unpickling stack underflow");
    assert_native_error(b"(\x98.", "unexpected MARK found");
    for (bytes, kind) in [(&b"N\x98."[..], "NoneType"), (b"]\x98.", "list"), (b"K\x01\x98.", "int")] {
        assert_eq!(
            decode(bytes).unwrap_err().native_exception(),
            Some(("TypeError", format!("memoryview: a bytes-like object is required, not '{kind}'")))
        );
    }
}

#[test]
fn setitem_opcodes_assign_existing_list_elements() {
    // SETITEM/SETITEMS assign by subscript, including into a list by index.
    assert_eq!(decode(b"](K\x01K\x02eK\x00K\x09s."), Ok(LegacyValue::List(vec![9.into(), 2.into()])));
    assert_eq!(
        decode(b"](K\x01K\x02e(J\xff\xff\xff\xffK\x09I00\nK\x08u."),
        Ok(LegacyValue::List(vec![8.into(), 9.into()]))
    );
    assert_eq!(decode(b"](K\x01K\x02e\x88K\x09s."), Ok(LegacyValue::List(vec![1.into(), 9.into()])));
}

#[test]
fn list_assignment_opcodes_retain_index_errors_and_pair_order() {
    for bytes in [&b"]K\x00K\x01s."[..], b"]K\x00aJ\xfe\xff\xff\xffK\x01s."] {
        assert_eq!(
            decode(bytes).unwrap_err().native_exception(),
            Some(("IndexError", "list assignment index out of range".into()))
        );
    }
    for bytes in [&b"]NK\x01s."[..], b"](NK\x01u."] {
        assert_eq!(
            decode(bytes).unwrap_err().native_exception(),
            Some(("TypeError", "list indices must be integers or slices, not NoneType".into()))
        );
    }
    // Odd pair validation precedes indexing, even with an invalid first key.
    assert_native_error(b"](NK\x01Nu.", "odd number of items for SETITEMS");
    assert_eq!(
        decode(b"](NK\x01K\x00K\x02u.").unwrap_err().native_exception(),
        Some(("TypeError", "list indices must be integers or slices, not NoneType".into()))
    );
}

#[test]
fn stored_list_assignment_decodes_and_invalid_index_keeps_native_error() {
    use crate::import::sqlitedict;

    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("profile.db");
    sqlitedict::set_item(&database, "unnamed", "untouched", &LegacyValue::Int(12)).unwrap();
    let connection = rusqlite::Connection::open(&database).unwrap();
    for (key, bytes) in [("list", &b"](K\x01K\x02eK\x00K\x09s."[..]), ("invalid", &b"]K\x00K\x01s."[..])] {
        connection.execute("INSERT INTO unnamed(key,value) VALUES (?1,?2)", rusqlite::params![key, bytes]).unwrap();
    }
    assert_eq!(
        sqlitedict::read_item(&database, "unnamed", "list").unwrap(),
        Some(LegacyValue::List(vec![9.into(), 2.into()]))
    );
    let error = sqlitedict::read_item(&database, "unnamed", "invalid").unwrap_err();
    assert_eq!(error.native_exception("select_one"), Some(("IndexError", "list assignment index out of range".into())));
    assert_eq!(sqlitedict::read_item(&database, "unnamed", "untouched").unwrap(), Some(12.into()));
}

#[test]
fn legacy_instance_opcodes_construct_only_allowlisted_types() {
    for bytes in [&b"(ibuiltins\nlist\n."[..], b"(cbuiltins\nlist\no."] {
        assert_eq!(decode(bytes), Ok(LegacyValue::List(vec![])));
    }
    for bytes in [&b"(K\x2aibuiltins\nint\n."[..], b"(cbuiltins\nint\nK\x2ao."] {
        assert_eq!(decode(bytes), Ok(LegacyValue::Int(42)));
    }
    for bytes in [&b"((K\x01K\x02tibuiltins\nlist\n."[..], b"(cbuiltins\nlist\n(K\x01K\x02to."] {
        assert_eq!(decode(bytes), Ok(LegacyValue::List(vec![1.into(), 2.into()])));
    }
    for bytes in [&b"(ios\nsystem\n."[..], b"(cos\nsystem\no."] {
        assert_eq!(
            decode(bytes).unwrap_err().native_exception(),
            Some(("UnpicklingError", "forbidden global: os.system".into()))
        );
    }
}

#[test]
fn legacy_instance_opcodes_keep_mark_and_class_errors() {
    for bytes in [&b"ibuiltins\nlist\n."[..], b"o."] {
        assert_native_error(bytes, "could not find MARK");
    }
    assert_native_error(b"(o.", "unpickling stack underflow");
    assert_native_error(b"((o.", "unexpected MARK found");
    assert_eq!(
        decode(b"(No.").unwrap_err().native_exception(),
        Some(("TypeError", "'NoneType' object is not callable".into()))
    );
    // INST resolves ASCII names before validating constructor arguments.
    assert_eq!(
        decode(b"(i\xff\nlist\n.").unwrap_err().native_exception(),
        Some((
            "UnicodeDecodeError",
            "'ascii' codec can't decode byte 0xff in position 0: ordinal not in range(128)".into()
        ))
    );
    assert_eq!(
        decode(b"(ibuiltins\n\n.").unwrap_err().native_exception(),
        Some(("UnpicklingError", "pickle data was truncated".into()))
    );
}

#[test]
fn newobj_uses_new_without_running_container_initializers() {
    for (name, expected) in [
        ("list", LegacyValue::List(vec![])),
        ("dict", LegacyValue::Dict(super::Dict::new())),
        ("set", LegacyValue::Set(vec![])),
    ] {
        // Allocation of these three builtins ignores arguments; the object is
        // filled only by ordinary calls and REDUCE/INST/OBJ.
        for args in [&b")"[..], b"(K\x01K\x02t"] {
            let mut bytes = format!("cbuiltins\n{name}\n").into_bytes();
            bytes.extend(args);
            bytes.extend(b"\x81.");
            assert_eq!(decode(&bytes), Ok(expected.clone()));
        }
    }
    assert_eq!(decode(b"cbuiltins\nint\nK\x2a\x85\x81."), Ok(42.into()));
    assert_eq!(decode(b"cbuiltins\nstr\nK\x2a\x85\x81."), Ok("42".into()));
    assert_eq!(decode(b"cbuiltins\ntuple\n](K\x01K\x02e\x85\x81."), Ok(LegacyValue::Tuple(vec![1.into(), 2.into()])));
}

#[test]
fn newobj_checks_stack_then_class_then_argument_tuple() {
    for bytes in [&b"\x81."[..], b"N\x81.", b"NN(\x81."] {
        assert_native_error(
            bytes,
            if bytes.contains(&b'(') { "unexpected MARK found" } else { "unpickling stack underflow" },
        );
    }
    assert_native_error(b"NN\x81.", "NEWOBJ class argument must be a type, not NoneType");
    assert_native_error(b"cbuiltins\nlist\nN\x81.", "NEWOBJ args argument must be a tuple, not NoneType");
}

#[test]
fn newobj_ex_constructs_allowlisted_types_with_empty_keywords() {
    for (name, expected) in [
        ("list", LegacyValue::List(vec![])),
        ("dict", LegacyValue::Dict(super::Dict::new())),
        ("set", LegacyValue::Set(vec![])),
        ("str", LegacyValue::Str(String::new())),
        ("int", LegacyValue::Int(0)),
        ("float", LegacyValue::Float(0.0)),
        ("bool", LegacyValue::Bool(false)),
        ("tuple", LegacyValue::Tuple(vec![])),
    ] {
        let mut bytes = format!("cbuiltins\n{name}\n)}}").into_bytes();
        bytes.extend(b"\x92.");
        assert_eq!(decode(&bytes), Ok(expected));
    }
    for name in ["list", "dict", "set"] {
        let mut bytes = format!("cbuiltins\n{name}\n").into_bytes();
        // Allocation ignores arbitrary positional and keyword arguments for these
        // empty containers. No initialization runs afterward.
        bytes.extend(b"K\x01\x85}\x8c\x01xK\x02s\x92.");
        let expected = match name {
            "list" => LegacyValue::List(vec![]),
            "set" => LegacyValue::Set(vec![]),
            _ => LegacyValue::Dict(super::Dict::new()),
        };
        assert_eq!(decode(&bytes), Ok(expected));
    }
}

#[test]
fn newobj_ex_validates_class_then_args_then_keywords() {
    for bytes in [&b"\x92."[..], b"N\x92.", b"NN\x92.", b"NN(\x92."] {
        assert_native_error(
            bytes,
            if bytes.contains(&b'(') { "unexpected MARK found" } else { "unpickling stack underflow" },
        );
    }
    assert_native_error(b"NNN\x92.", "NEWOBJ_EX class argument must be a type, not NoneType");
    assert_native_error(b"cbuiltins\nint\nNN\x92.", "NEWOBJ_EX args argument must be a tuple, not NoneType");
    assert_native_error(b"cbuiltins\nint\n)N\x92.", "NEWOBJ_EX kwargs argument must be a dict, not NoneType");
}

#[test]
fn build_preserves_empty_state_and_native_state_errors() {
    for bytes in [&b"]Nb."[..], b"]N}\x86b.", b"NNb.", b"K\x01Nb."] {
        let expected = if bytes[0] == b']' {
            LegacyValue::List(vec![])
        } else if bytes[0] == b'K' {
            1.into()
        } else {
            LegacyValue::None
        };
        assert_eq!(decode(bytes), Ok(expected));
    }
    for bytes in [&b"]K\x01b."[..], b"]K\x01N\x86b."] {
        assert_native_error(bytes, "state is not a dictionary");
    }
    assert_native_error(b"]NK\x01\x86b.", "slot state is not a dictionary");
    assert_eq!(
        decode(b"]}b.").unwrap_err().native_exception(),
        Some(("AttributeError", "'list' object has no attribute '__dict__'".into()))
    );
    for bytes in [&b"b."[..], b"Nb.", b"](Nb."] {
        assert_native_error(
            bytes,
            if bytes.contains(&b'(') { "unexpected MARK found" } else { "unpickling stack underflow" },
        );
    }
}
