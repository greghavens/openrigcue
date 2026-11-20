use super::{decode, LegacyValue};

#[test]
fn memoize_indexes_by_used_slot_count_after_sparse_explicit_put() {
    let bytes = b"\x80\x04K*q\x050K+\x940h\x01.";
    assert_eq!(decode(bytes), Ok(LegacyValue::Int(43)));
}

#[test]
fn memoize_overwrite_does_not_increment_the_used_slot_count() {
    let bytes = b"\x80\x04K*q\x010K+\x940K,\x940h\x01.";
    assert_eq!(decode(bytes), Ok(LegacyValue::Int(44)));
}

#[test]
fn missing_memo_slot_retains_native_exception() {
    for bytes in [&b"h\x07."[..], &b"j\x07\0\0\0."[..], &b"g7\n."[..]] {
        assert_eq!(
            decode(bytes).unwrap_err().native_exception(),
            Some(("UnpicklingError", "Memo value not found at index 7".into()))
        );
    }
}

#[test]
fn text_memo_indexes_use_ascii_decimal_python_grammar() {
    for index in ["+07", " 7 ", "0_7", "7\0ignored"] {
        assert_eq!(decode(format!("K*p{index}\n0g{index}\n.").as_bytes()), Ok(LegacyValue::Int(42)));
    }
    for (index, printed) in [("-1", "-1"), ("-9223372036854775808", "0")] {
        let error = decode(format!("g{index}\n.").as_bytes()).unwrap_err();
        assert_eq!(
            error.native_exception(),
            Some(("UnpicklingError", format!("Memo value not found at index {printed}")))
        );
        let error = decode(format!("Np{index}\n.").as_bytes()).unwrap_err();
        assert_eq!(error.native_exception(), Some(("ValueError", "negative PUT argument".into())));
    }
}

#[test]
fn missing_memo_index_uses_windows_c_long_formatting() {
    for (bytes, printed) in [(&b"j\xff\xff\xff\xff."[..], "-1"), (b"g4294967296\n.", "0")] {
        assert_eq!(
            decode(bytes).unwrap_err().native_exception(),
            Some(("UnpicklingError", format!("Memo value not found at index {printed}")))
        );
    }
}

#[test]
fn text_memo_index_errors_keep_native_identity_and_order() {
    for index in ["1__0", "0x7", "٧", "\u{a0}7", " "] {
        let input = format!("{index}\n");
        let message =
            format!("invalid literal for int() with base 10: {}", openrigcue_model::python_repr::repr_str(&input));
        for opcode in ["g", "Np"] {
            assert_eq!(
                decode(format!("{opcode}{index}\n.").as_bytes()).unwrap_err().native_exception(),
                Some(("ValueError", message.clone()))
            );
        }
    }
    for opcode in ["g", "Np"] {
        assert_eq!(
            decode(format!("{opcode}9223372036854775808\n.").as_bytes()).unwrap_err().native_exception(),
            Some(("OverflowError", "Python int too large to convert to C ssize_t".into()))
        );
    }
    assert_eq!(
        decode(b"pbad\n.").unwrap_err().native_exception(),
        Some(("UnpicklingError", "unpickling stack underflow".into()))
    );
    assert_eq!(
        decode(b"p\n.").unwrap_err().native_exception(),
        Some(("UnpicklingError", "pickle data was truncated".into()))
    );
}
