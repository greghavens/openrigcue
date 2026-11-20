use super::{decode, PickleError};
use crate::import::value::LegacyValue;

fn frame(size: u64, payload: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0x80, 4, 0x95];
    bytes.extend(size.to_le_bytes());
    bytes.extend(payload);
    bytes
}

#[test]
fn frame_announced_payload_is_checked_before_executing_stop() {
    assert_eq!(decode(&frame(2, b"N.")), Ok(LegacyValue::None));
    assert_eq!(decode(&frame(3, b"N.")), Err(PickleError::Truncated));
    assert_eq!(decode(&frame(0, b"N.")), Ok(LegacyValue::None));
    assert_eq!(decode(&frame(1, b"N.")), Ok(LegacyValue::None));
    assert_eq!(decode(&frame(0, b"")), Err(PickleError::Exhausted));
}

#[test]
fn frame_signed_size_errors_precede_truncated_payload() {
    for size in [1_u64 << 63, u64::MAX] {
        let error = decode(&frame(size, b"N.")).unwrap_err();
        assert_eq!(
            error.native_exception(),
            Some(("OverflowError", "FRAME length exceeds system's maximum of 9223372036854775807 bytes".into()))
        );
    }
    let error = decode(&frame(i64::MAX as u64, b"N.")).unwrap_err();
    assert_eq!(error.native_exception(), Some(("UnpicklingError", "read would overflow (invalid bytecode)".into())));
    assert_eq!(decode(&frame(i64::MAX as u64 - 8, b"N.")), Err(PickleError::Truncated));
}

#[test]
fn negative_long4_size_raises_before_reading_the_payload() {
    for length in [i32::MIN, -1] {
        let mut bytes = vec![0x80, 4, 0x8b];
        bytes.extend(length.to_le_bytes());
        let error = decode(&bytes).unwrap_err();
        assert_eq!(error.native_exception(), Some(("UnpicklingError", "LONG pickle has negative byte count".into())));
    }
}

#[test]
fn counted_binary_sizes_reject_negative_ssize_before_payload_read() {
    for (opcode, name) in [(0x8d, "BINUNICODE"), (0x8e, "BINBYTES")] {
        for length in [1_u64 << 63, u64::MAX] {
            let mut bytes = vec![0x80, 4, opcode];
            bytes.extend(length.to_le_bytes());
            let error = decode(&bytes).unwrap_err();
            assert_eq!(
                error.native_exception(),
                Some(("OverflowError", format!("{name} exceeds system's maximum size of 9223372036854775807 bytes")))
            );
        }
    }
}

#[test]
fn binary_python2_strings_retain_ascii_decode_errors() {
    for prefix in [&b"U\x03"[..], b"T\x03\0\0\0"] {
        let mut bytes = prefix.to_vec();
        bytes.extend(b"a\xffb.");
        assert_eq!(
            decode(&bytes).unwrap_err().native_exception(),
            Some((
                "UnicodeDecodeError",
                "'ascii' codec can't decode byte 0xff in position 1: ordinal not in range(128)".into()
            ))
        );
    }
}

#[test]
fn binary_unicode_retains_utf8_error_offsets_and_reason() {
    for (payload, detail) in [
        (&b"a\xffb"[..], "byte 0xff in position 1: invalid start byte"),
        (&b"a\xe2\x82"[..], "bytes in position 1-2: unexpected end of data"),
        (&b"a\xe2x"[..], "byte 0xe2 in position 1: invalid continuation byte"),
        (&b"a\xe2\x82x"[..], "bytes in position 1-2: invalid continuation byte"),
        (&b"\xc0\x80"[..], "byte 0xc0 in position 0: invalid start byte"),
        (&b"\xf4\x90\x80\x80"[..], "byte 0xf4 in position 0: invalid continuation byte"),
    ] {
        for size_bytes in [1, 4, 8] {
            let mut bytes = vec![match size_bytes {
                1 => 0x8c,
                4 => b'X',
                _ => 0x8d,
            }];
            bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes()[..size_bytes]);
            bytes.extend_from_slice(payload);
            bytes.push(b'.');
            assert_eq!(
                decode(&bytes).unwrap_err().native_exception(),
                Some(("UnicodeDecodeError", format!("'utf-8' codec can't decode {detail}")))
            );
        }
    }
    assert_eq!(decode(b"\x8c\x03\xed\xa0\x80.").unwrap().python_string().unwrap().codepoints(), &[0xd800]);
}

#[test]
fn frame_crossing_fixed_operands_replaces_the_file_buffer() {
    let mut bytes = frame(2, &[b'J', 255]);
    bytes.extend([7, 0, 0, 0, b'.']);
    assert_eq!(decode(&bytes), Ok(LegacyValue::Int(7)));

    let mut text = frame(3, &[0x8c, 3, b'x']);
    text.extend(b"abc.");
    assert_eq!(decode(&text), Ok(LegacyValue::Str("abc".into())));

    let mut line = frame(2, b"I9");
    line.extend(b"42\n.");
    assert_eq!(decode(&line), Ok(LegacyValue::Int(42)));
}

#[test]
fn frame_bytes_payload_combines_buffered_and_file_bytes() {
    let mut bytes = frame(3, &[b'C', 3, b'x']);
    bytes.extend(b"yz.");
    assert_eq!(decode(&bytes), Ok(LegacyValue::Bytes(b"xyz".to_vec())));

    let mut bytes = frame(3, &[b'C', 3, b'x']);
    bytes.extend(b"y");
    assert_eq!(decode(&bytes), Err(PickleError::Truncated));
}

#[test]
fn frame_nested_windows_are_prefetched_without_an_end_fence() {
    let mut nested = vec![0x95];
    nested.extend(2_u64.to_le_bytes());
    nested.extend(b"N.");
    assert_eq!(decode(&frame(nested.len() as u64, &nested)), Ok(LegacyValue::None));

    // The nested frame request is larger than the outer buffer remainder.
    // The stream has already advanced over that remainder, so it reads the suffix.
    let mut nested = vec![0x95];
    nested.extend(2_u64.to_le_bytes());
    nested.push(b'!');
    let mut bytes = frame(nested.len() as u64, &nested);
    bytes.extend(b"N.");
    assert_eq!(decode(&bytes), Ok(LegacyValue::None));
}

#[test]
fn frame_selected_database_row_keeps_error_identity_and_other_rows_readable() {
    use crate::import::sqlitedict;
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("profile.db");
    sqlitedict::set_item(&database, "unnamed", "good", &LegacyValue::Int(12)).unwrap();
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute("INSERT INTO unnamed(key,value) VALUES (?1,?2)", rusqlite::params!["bad", frame(3, b"N.")])
        .unwrap();
    let error = sqlitedict::read_item(&database, "unnamed", "bad").unwrap_err();
    assert_eq!(error.native_exception("select_one"), Some(("UnpicklingError", "pickle data was truncated".into())));
    assert_eq!(sqlitedict::read_item(&database, "unnamed", "good").unwrap(), Some(LegacyValue::Int(12)));
}
